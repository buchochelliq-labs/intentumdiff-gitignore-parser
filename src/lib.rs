use intentdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

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
    let metadata = intentdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
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

/// Classify a single physical line. Blank lines return `None` so they contribute NO node
/// — adding or removing spacing between patterns is invisible to the diff, which is the
/// whole point of parsing over generic-text token churn.
fn classify_line(raw: &str) -> Option<(&'static str, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // A leading '#' is a comment unless escaped (`\#` is a literal pattern).
    if trimmed.starts_with('#') {
        return Some(("comment", trimmed.to_string()));
    }
    if let Some(rest) = trimmed.strip_prefix('!') {
        // Negation — kept as its own kind so un-negating a rule reads as a real change,
        // not a delete+add of an unrelated pattern.
        return Some(("negated_pattern", format!("!{}", rest.trim())));
    }
    Some(("pattern", trimmed.to_string()))
}

fn parse_gitignore(source: &str) -> String {
    let mut children: Vec<SemanticNode> = Vec::new();
    let mut total_lines = 0u32;
    for (index, raw) in source.lines().enumerate() {
        total_lines = index as u32;
        let Some((node_type, label)) = classify_line(raw) else {
            continue;
        };
        let id = format!("0.{}", children.len());
        let line = index as u32;
        children.push(
            SemanticNodeBuilder::new(
                &id,
                node_type,
                &label,
                line,
                0,
                line,
                label.len() as u32,
                stable_hash(node_type, &label, &[]),
            )
            .build(),
        );
    }

    let root = SemanticNodeBuilder::new(
        "0",
        ROOT_NODE_TYPE,
        LANGUAGE_ID,
        0,
        0,
        total_lines,
        0,
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
        intentdiff_plugin_sdk::testing::assert_valid_json(&parsed, LANGUAGE_ID);
        intentdiff_plugin_sdk::testing::assert_root_node_type(&parsed, ROOT_NODE_TYPE, LANGUAGE_ID);
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
}
