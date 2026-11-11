use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const LANGUAGE_ID: &str = "gitignore";
const ROOT_NODE_TYPE: &str = "gitignore_file";
const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

// The ignore-family: files that share .gitignore pattern syntax. Detection is by
// filename (these are extensionless dotfiles), so a suffix match on the basename covers
// both the bare form (`.gitignore`) and prefixed forms (`foo.gitignore`).
const IGNORE_SUFFIXES: &[&str] = &[
    ".gitignore",
    ".dockerignore",
    ".npmignore",
    ".eslintignore",
    ".prettierignore",
    ".helmignore",
    ".gcloudignore",
    ".cursorignore",
    ".vscodeignore",
];

const DEFAULT_OLD: &str = "# Build output\n/target\nnode_modules/\n\n*.log\n";
const DEFAULT_NEW: &str = "# Build output\n/target\nnode_modules/\n\n*.log\n.env\n";

struct GitignoreParser;

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}

fn basename(path: &str) -> &str {
    path.rsplit(|ch| ch == '/' || ch == '\\')
        .next()
        .unwrap_or(path)
}

fn detect_language_impl(filename: &str, _content: &str) -> String {
    let name = basename(filename).to_lowercase();
    if IGNORE_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
        LANGUAGE_ID.to_string()
    } else {
        String::new()
    }
}

/// Git's `trim_trailing_spaces` (dir.c): drop trailing ASCII spaces unless the run is
/// escaped by a backslash. Only U+0020 is trimmed — tabs and other whitespace (including
/// Unicode separators such as U+2028) are significant pattern bytes, exactly as in git.
/// Leading spaces are never trimmed: ` foo` and `foo` name different paths.
fn git_trim_trailing_spaces(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut last_space: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b' ' => {
                if last_space.is_none() {
                    last_space = Some(i);
                }
            }
            b'\\' => {
                // The escaped byte (often a space) is kept verbatim. A lone trailing
                // backslash ends the scan with nothing trimmed, as in git.
                i += 1;
                if i >= bytes.len() {
                    return line;
                }
                last_space = None;
            }
            _ => last_space = None,
        }
        i += 1;
    }
    match last_space {
        // Spaces and backslashes are ASCII, so the cut is always on a char boundary.
        Some(cut) => &line[..cut],
        None => line,
    }
}

/// Classify one physical line (line terminator already removed) the way git's
/// `add_patterns_from_buffer` does. Returns the node kind and the exact retained source
/// text, which is always a prefix of `raw` so spans map to the original bytes.
///
/// - A line whose first byte is `#` is a comment (` #x` and `\#x` are patterns).
/// - Trailing unescaped spaces are dropped; leading spaces are kept.
/// - A line that is empty after that trimming matches nothing and yields NO node, so
///   adding or removing spacing between patterns is invisible to the diff.
fn classify_line(raw: &str) -> Option<(&'static str, &str)> {
    if raw.starts_with('#') {
        // Comments are review content; trailing layout whitespace is not.
        let text = raw.trim_end_matches([' ', '\t']);
        return Some(("comment", text));
    }
    let pattern = git_trim_trailing_spaces(raw);
    if pattern.is_empty() {
        return None;
    }
    // Negation — kept as its own kind so un-negating a rule reads as a real change, not a
    // delete+add of an unrelated pattern. The text after `!` is kept verbatim (`! foo`
    // negates the path " foo").
    if pattern.starts_with('!') {
        return Some(("negated_pattern", pattern));
    }
    Some(("pattern", pattern))
}

/// Split `source` into raw LF-separated rows. A single `\r` before each `\n` (and at the
/// very end of the buffer, which git terminates with an implicit `\n`) is a line
/// terminator, not content. Columns are UTF-8 byte offsets into the raw row, so for CRLF
/// input the `\r` byte simply lies past every node's end column.
fn raw_rows(source: &str) -> impl Iterator<Item = &str> {
    source
        .split('\n')
        .map(|row| row.strip_suffix('\r').unwrap_or(row))
}

fn parse_gitignore(source: &str) -> String {
    let mut children: Vec<SemanticNode> = Vec::new();
    for (index, row) in raw_rows(source).enumerate() {
        let line = index as u32;
        // Git skips a UTF-8 BOM at the start of the buffer.
        let (start_col, content) = match (index, row.strip_prefix('\u{feff}')) {
            (0, Some(rest)) => ('\u{feff}'.len_utf8(), rest),
            _ => (0, row),
        };
        let Some((node_type, label)) = classify_line(content) else {
            continue;
        };
        let id = format!("0.{}", children.len());
        children.push(
            SemanticNodeBuilder::new(
                &id,
                node_type,
                label,
                line,
                start_col as u32,
                line,
                (start_col + label.len()) as u32,
                stable_hash(node_type, label, &[]),
            )
            .build(),
        );
    }

    // The root spans the whole source: it ends after the last byte, on the row following
    // the final LF when the file ends with one.
    let end_row = source.matches('\n').count() as u32;
    let end_col = source.rsplit('\n').next().unwrap_or("").len() as u32;
    let root = SemanticNodeBuilder::new(
        "0",
        ROOT_NODE_TYPE,
        LANGUAGE_ID,
        0,
        0,
        end_row,
        end_col,
        stable_hash(ROOT_NODE_TYPE, LANGUAGE_ID, &children),
    )
    .children(children)
    .build();

    match serde_json::to_string(&root) {
        Ok(serialized) => serialized,
        Err(err) => format!(r#"{{"error":"Serialisation error: {}"}}"#, err),
    }
}

fn stable_hash(node_type: &str, label: &str, children: &[SemanticNode]) -> String {
    let mut value = format!("{node_type}:{label}");
    for child in children {
        value.push('|');
        value.push_str(&child.structural_hash);
    }
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

impl Guest for GitignoreParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }

    fn grammar_id() -> String {
        LANGUAGE_ID.to_string()
    }

    fn detect_language(filename: String, content: String) -> String {
        detect_language_impl(&filename, &content)
    }

    fn preprocess_source(source: String) -> String {
        source
    }

    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: DEFAULT_OLD.to_string(),
            new: DEFAULT_NEW.to_string(),
        }
    }

    fn process(input: String, _language: String, _filename: String) -> String {
        parse_gitignore(&input)
    }

    fn trivia_node_types() -> Vec<String> {
        // Comments are kept as first-class nodes (a comment edit is a real change), so
        // there is no trivia to strip.
        vec![]
    }

    fn language_ids() -> Vec<String> {
        vec![LANGUAGE_ID.to_string()]
    }

    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }

    fn priority() -> i32 {
        // Above the generic fallback (-10): a filename-matched ignore file is always a
        // better fit than the content-based generic parser.
        5
    }
}

export!(GitignoreParser);

#[cfg(test)]
mod tests {
    use super::*;

    fn labels_by_type(node: &SemanticNode, node_type: &str, out: &mut Vec<String>) {
        if node.node_type == node_type {
            out.push(node.label.clone());
        }
        for child in &node.children {
            labels_by_type(child, node_type, out);
        }
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert_eq!(GitignoreParser::get_parser_mode(), ParserMode::FullParse);
    }

    #[test]
    fn detects_ignore_family_by_filename() {
        assert_eq!(detect_language_impl(".gitignore", ""), LANGUAGE_ID);
        assert_eq!(detect_language_impl("sub/dir/.gitignore", ""), LANGUAGE_ID);
        assert_eq!(detect_language_impl(".dockerignore", ""), LANGUAGE_ID);
        assert_eq!(detect_language_impl("app.gitignore", ""), LANGUAGE_ID);
        assert_eq!(detect_language_impl("main.rs", ""), "");
    }

    #[test]
    fn process_returns_valid_json_with_pattern_and_comment_nodes() {
        let parsed = parse_gitignore(DEFAULT_NEW);
        intentumdiff_plugin_sdk::testing::assert_valid_json(&parsed, LANGUAGE_ID);
        intentumdiff_plugin_sdk::testing::assert_root_node_type(
            &parsed,
            ROOT_NODE_TYPE,
            LANGUAGE_ID,
        );
        let root: SemanticNode = serde_json::from_str(&parsed).unwrap();
        let mut comments = Vec::new();
        let mut patterns = Vec::new();
        labels_by_type(&root, "comment", &mut comments);
        labels_by_type(&root, "pattern", &mut patterns);
        assert!(comments.contains(&"# Build output".to_string()));
        assert!(patterns.contains(&"/target".to_string()));
        assert!(patterns.contains(&".env".to_string()));
    }

    #[test]
    fn blank_lines_produce_no_nodes_so_spacing_is_invisible() {
        // The core reason this parser exists: adding a blank line between patterns must
        // not surface as a change (generic-text token churn did — issue #43).
        let spaced = parse_gitignore("/a\n\n\n/b\n");
        let tight = parse_gitignore("/a\n/b\n");
        let spaced_root: SemanticNode = serde_json::from_str(&spaced).unwrap();
        let tight_root: SemanticNode = serde_json::from_str(&tight).unwrap();
        assert_eq!(spaced_root.children.len(), 2);
        assert_eq!(spaced_root.structural_hash, tight_root.structural_hash);
    }

    #[test]
    fn adding_a_pattern_changes_the_root_hash() {
        let old: SemanticNode = serde_json::from_str(&parse_gitignore(DEFAULT_OLD)).unwrap();
        let new: SemanticNode = serde_json::from_str(&parse_gitignore(DEFAULT_NEW)).unwrap();
        assert_ne!(old.structural_hash, new.structural_hash);
        assert_eq!(new.children.len(), old.children.len() + 1);
    }

    #[test]
    fn negation_is_its_own_kind() {
        let parsed = parse_gitignore("build/\n!build/keep.txt\n");
        let root: SemanticNode = serde_json::from_str(&parsed).unwrap();
        let mut negations = Vec::new();
        labels_by_type(&root, "negated_pattern", &mut negations);
        assert_eq!(negations, vec!["!build/keep.txt".to_string()]);
    }

    fn parse(source: &str) -> SemanticNode {
        let parsed = parse_gitignore(source);
        intentumdiff_plugin_sdk::testing::assert_valid_json(&parsed, LANGUAGE_ID);
        serde_json::from_str(&parsed).unwrap()
    }

    /// (type, label, start_row, start_col, end_row, end_col) for each child.
    fn facts(root: &SemanticNode) -> Vec<(String, String, u32, u32, u32, u32)> {
        root.children
            .iter()
            .map(|c| {
                (
                    c.node_type.clone(),
                    c.label.clone(),
                    c.position.start_line,
                    c.position.start_col,
                    c.position.end_line,
                    c.position.end_col,
                )
            })
            .collect()
    }

    fn fact(
        t: &str,
        l: &str,
        r0: u32,
        c0: u32,
        r1: u32,
        c1: u32,
    ) -> (String, String, u32, u32, u32, u32) {
        (t.to_string(), l.to_string(), r0, c0, r1, c1)
    }

    #[test]
    fn leading_space_is_significant_issue_4() {
        // `foo` and ` foo` match different paths in git (assessment
        // data_config.gitignore.known_leading_space and issue #4).
        let spaced = parse(" foo\n");
        let plain = parse("foo\n");
        assert_eq!(facts(&spaced), vec![fact("pattern", " foo", 0, 0, 0, 4)]);
        assert_eq!(facts(&plain), vec![fact("pattern", "foo", 0, 0, 0, 3)]);
        assert_ne!(spaced.structural_hash, plain.structural_hash);
        // Same on a later line among other rules.
        let multi_old = parse("*.log\n foo\n!keep.log\n");
        let multi_new = parse("*.log\nfoo\n!keep.log\n");
        assert_eq!(multi_old.children[1].label, " foo");
        assert_ne!(multi_old.structural_hash, multi_new.structural_hash);
    }

    #[test]
    fn escaped_trailing_space_is_kept_issue_4() {
        // Exact pairs from issue #4 and data_config.gitignore.known_escaped_trailing_space.
        let escaped = parse("foo\\ \n");
        assert_eq!(facts(&escaped), vec![fact("pattern", "foo\\ ", 0, 0, 0, 5)]);
        let bare_backslash = parse("foo\\\n");
        assert_eq!(
            facts(&bare_backslash),
            vec![fact("pattern", "foo\\", 0, 0, 0, 4)]
        );
        assert_ne!(escaped.structural_hash, bare_backslash.structural_hash);
        assert_ne!(escaped.structural_hash, parse("foo\n").structural_hash);
        // Only the unescaped tail run is trimmed: `foo\  ` keeps one escaped space.
        assert_eq!(parse("foo\\   \n").children[0].label, "foo\\ ");
        // `a b` keeps its interior space.
        assert_eq!(parse("a b  \n").children[0].label, "a b");
    }

    #[test]
    fn unescaped_trailing_spaces_are_layout_only() {
        let trailing = parse("foo   \n");
        let plain = parse("foo\n");
        assert_eq!(facts(&trailing), vec![fact("pattern", "foo", 0, 0, 0, 3)]);
        assert_eq!(trailing.structural_hash, plain.structural_hash);
        // A spaces-only line is empty after trimming and matches nothing.
        assert_eq!(
            parse("a\n   \nb\n").structural_hash,
            parse("a\nb\n").structural_hash
        );
    }

    #[test]
    fn tabs_and_unicode_whitespace_are_pattern_bytes() {
        // git trims only U+0020; a trailing tab is part of the pattern.
        assert_eq!(parse("foo\t\n").children[0].label, "foo\t");
        assert_ne!(
            parse("foo\t\n").structural_hash,
            parse("foo\n").structural_hash
        );
    }

    #[test]
    fn comment_only_at_column_zero_and_negation_verbatim() {
        assert_eq!(
            facts(&parse("# c\n")),
            vec![fact("comment", "# c", 0, 0, 0, 3)]
        );
        assert_eq!(
            facts(&parse(" #c\n")),
            vec![fact("pattern", " #c", 0, 0, 0, 3)]
        );
        assert_eq!(
            facts(&parse("\\#c\n")),
            vec![fact("pattern", "\\#c", 0, 0, 0, 3)]
        );
        // `! foo` re-includes the path " foo", distinct from `!foo`.
        let spaced = parse("! foo\n");
        assert_eq!(
            facts(&spaced),
            vec![fact("negated_pattern", "! foo", 0, 0, 0, 5)]
        );
        assert_ne!(spaced.structural_hash, parse("!foo\n").structural_hash);
    }

    #[test]
    fn positions_are_exact_utf8_byte_extents_issue_5() {
        // Exact input from issue #5: U+2028 (3 bytes) + U+1F9A3 (4) + U+2028 (3) + '"'.
        let source = "\u{2028}\u{1F9A3}\u{2028}\"";
        let root = parse(source);
        assert_eq!(facts(&root), vec![fact("pattern", source, 0, 0, 0, 11)]);
        assert_eq!(source.len(), 11);
        assert_eq!((root.position.end_line, root.position.end_col), (0, 11));

        // Multibyte indentation keeps its bytes and offsets
        // (data_config.gitignore.unicode_prefix_span).
        let root = parse("# λ\n  μ.log\n");
        assert_eq!(
            facts(&root),
            vec![
                fact("comment", "# λ", 0, 0, 0, 4),
                fact("pattern", "  μ.log", 1, 0, 1, 8)
            ]
        );
        assert_eq!((root.position.end_line, root.position.end_col), (2, 0));
    }

    #[test]
    fn root_span_covers_source_and_ids_are_unique() {
        let empty = parse("");
        assert!(empty.children.is_empty());
        assert_eq!((empty.position.end_line, empty.position.end_col), (0, 0));

        let no_final_lf = parse("a\n\nbc");
        assert_eq!(
            facts(&no_final_lf),
            vec![
                fact("pattern", "a", 0, 0, 0, 1),
                fact("pattern", "bc", 2, 0, 2, 2)
            ]
        );
        assert_eq!(
            (no_final_lf.position.end_line, no_final_lf.position.end_col),
            (2, 2)
        );

        let ids: Vec<&str> = no_final_lf.children.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["0.0", "0.1"]);
        assert_eq!(parse_gitignore("a\nb\n"), parse_gitignore("a\nb\n"));
    }

    #[test]
    fn crlf_terminator_is_not_content() {
        // Columns are UTF-8 bytes in the raw row; the CR before LF (or at EOF) is a line
        // terminator and lies beyond every node's end column.
        let crlf = parse(" foo \r\nbar\r");
        assert_eq!(
            facts(&crlf),
            vec![
                fact("pattern", " foo", 0, 0, 0, 4),
                fact("pattern", "bar", 1, 0, 1, 3)
            ]
        );
        assert_eq!(
            crlf.children[0].structural_hash,
            parse(" foo\n").children[0].structural_hash
        );
    }

    #[test]
    fn utf8_bom_is_skipped_like_git() {
        let root = parse("\u{feff}# c\nfoo\n");
        assert_eq!(
            facts(&root),
            vec![
                fact("comment", "# c", 0, 3, 0, 6),
                fact("pattern", "foo", 1, 0, 1, 3)
            ]
        );
    }
}
