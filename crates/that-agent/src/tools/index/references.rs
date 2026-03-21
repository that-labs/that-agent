//! Cross-file reference extraction from tree-sitter ASTs.
//!
//! Walks the AST looking for identifier and import nodes, then resolves
//! them against known symbol names from the index to build reference edges.

use crate::tools::impls::code::parse::{self, Language};
use serde::Serialize;

/// A reference from one file to a symbol defined in another.
#[derive(Debug, Clone, Serialize)]
pub struct Reference {
    /// The symbol name being referenced.
    pub symbol_name: String,
    /// Line number where the reference occurs.
    pub line: usize,
    /// Kind of reference.
    pub kind: RefKind,
}

/// Classification of reference types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    Import,
    Call,
    TypeRef,
}

impl std::fmt::Display for RefKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefKind::Import => write!(f, "import"),
            RefKind::Call => write!(f, "call"),
            RefKind::TypeRef => write!(f, "type_ref"),
        }
    }
}

/// Extract references from a source file by walking its AST for identifiers
/// and import statements, then filtering against known symbol names.
pub fn extract_references(
    source: &str,
    language: Language,
    known_symbols: &[String],
) -> Vec<Reference> {
    let parsed = match parse::parse_source(source, language) {
        Ok(p) => p,
        Err(_) => return vec![],
    };

    let mut refs = Vec::new();
    let root = parsed.tree.root_node();

    // Use HashSet for O(1) lookups instead of linear Vec::contains
    let known_set: std::collections::HashSet<&str> =
        known_symbols.iter().map(|s| s.as_str()).collect();

    // Same-file references are included — they are legitimate and useful for agents.
    // PageRank already excludes self-loops via WHERE ref_f.id != sym_f.id in file_edges().

    // Iterative traversal to avoid stack overflow on deeply nested ASTs
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let kind = node.kind();

        // Check import statements
        if is_import_node(kind, language) {
            if let Some(name) = extract_import_name(node, source, language) {
                if known_set.contains(name.as_str()) {
                    refs.push(Reference {
                        symbol_name: name,
                        line: node.start_position().row + 1,
                        kind: RefKind::Import,
                    });
                }
            }
        }

        // Check identifiers that match known symbols
        if is_identifier_node(kind, language) {
            if let Ok(text) = node.utf8_text(source.as_bytes()) {
                if known_set.contains(text) {
                    let ref_kind = classify_identifier_context(node, language);
                    refs.push(Reference {
                        symbol_name: text.to_string(),
                        line: node.start_position().row + 1,
                        kind: ref_kind,
                    });
                }
            }
        }

        // Push children in reverse order so left-to-right traversal is preserved
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }

    refs
}

fn is_import_node(kind: &str, language: Language) -> bool {
    match language {
        Language::Rust => kind == "use_declaration",
        Language::TypeScript => kind == "import_statement",
        Language::Python => kind == "import_statement" || kind == "import_from_statement",
        Language::Go => kind == "import_spec",
        Language::Json => false,
    }
}

fn extract_import_name(
    node: tree_sitter::Node,
    source: &str,
    language: Language,
) -> Option<String> {
    match language {
        Language::Rust => {
            // For `use foo::Bar;`, extract "Bar" (the last segment)
            let text = node.utf8_text(source.as_bytes()).ok()?;
            let path = text.trim_start_matches("use ").trim_end_matches(';');
            path.rsplit("::").next().map(|s| s.trim().to_string())
        }
        Language::TypeScript => {
            // Look for import_clause > identifier
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "import_clause" {
                    let mut c2 = child.walk();
                    for grandchild in child.children(&mut c2) {
                        if grandchild.kind() == "identifier" {
                            return grandchild
                                .utf8_text(source.as_bytes())
                                .ok()
                                .map(|s| s.to_string());
                        }
                    }
                }
            }
            None
        }
        Language::Python => {
            // `import foo` or `from foo import bar`
            let text = node.utf8_text(source.as_bytes()).ok()?;
            if text.starts_with("from") {
                // `from X import Y` — extract Y
                text.split("import").nth(1).map(|s| s.trim().to_string())
            } else {
                text.strip_prefix("import ").map(|s| s.trim().to_string())
            }
        }
        Language::Go => {
            // import "path/name" — extract last path component
            let text = node.utf8_text(source.as_bytes()).ok()?;
            let path = text.trim_matches('"').trim();
            path.rsplit('/').next().map(|s| s.to_string())
        }
        Language::Json => None,
    }
}

fn is_identifier_node(kind: &str, language: Language) -> bool {
    match language {
        Language::Rust => kind == "identifier" || kind == "type_identifier",
        Language::TypeScript => kind == "identifier" || kind == "type_identifier",
        Language::Python => kind == "identifier",
        Language::Go => kind == "identifier" || kind == "type_identifier",
        Language::Json => false,
    }
}

fn classify_identifier_context(node: tree_sitter::Node, language: Language) -> RefKind {
    if let Some(parent) = node.parent() {
        let pk = parent.kind();
        match language {
            Language::Rust => {
                if pk == "call_expression" || pk == "macro_invocation" {
                    return RefKind::Call;
                }
                if pk == "type_identifier" || pk == "generic_type" || pk == "scoped_type_identifier"
                {
                    return RefKind::TypeRef;
                }
            }
            Language::TypeScript => {
                if pk == "call_expression" || pk == "new_expression" {
                    return RefKind::Call;
                }
                if pk == "type_annotation" || pk == "type_arguments" {
                    return RefKind::TypeRef;
                }
            }
            Language::Python => {
                if pk == "call" || pk == "argument_list" {
                    return RefKind::Call;
                }
            }
            Language::Go => {
                if pk == "call_expression" {
                    return RefKind::Call;
                }
                if pk == "type_identifier" || pk == "qualified_type" {
                    return RefKind::TypeRef;
                }
            }
            Language::Json => {}
        }
    }
    RefKind::TypeRef
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_rust_references() {
        let source = r#"
use std::io;

fn main() {
    let config = Config::new();
    process_data(&config);
}
"#;
        let known = vec!["Config".to_string(), "process_data".to_string()];
        let refs = extract_references(source, Language::Rust, &known);
        let names: Vec<&str> = refs.iter().map(|r| r.symbol_name.as_str()).collect();
        assert!(
            names.contains(&"Config"),
            "should find Config reference: {:?}",
            names
        );
        assert!(
            names.contains(&"process_data"),
            "should find process_data reference: {:?}",
            names
        );
    }

    #[test]
    fn test_extract_typescript_references() {
        let source = r#"
function main() {
    const service = new UserService();
    const result = processData("test");
}
"#;
        let known = vec!["UserService".to_string(), "processData".to_string()];
        let refs = extract_references(source, Language::TypeScript, &known);
        let names: Vec<&str> = refs.iter().map(|r| r.symbol_name.as_str()).collect();
        assert!(
            names.contains(&"UserService"),
            "should find UserService: {:?}",
            names
        );
        assert!(
            names.contains(&"processData"),
            "should find processData: {:?}",
            names
        );
    }

    #[test]
    fn test_same_file_references_included() {
        let source = r#"
fn Config() {}
fn main() {
    Config();
}
"#;
        let known = vec!["Config".to_string()];
        let refs = extract_references(source, Language::Rust, &known);
        // Same-file references are now included — they are useful for agents.
        // PageRank handles self-loop exclusion at the file-edge level.
        assert!(
            refs.iter().any(|r| r.symbol_name == "Config"),
            "should include same-file references: {:?}",
            refs
        );
    }

    #[test]
    fn test_extract_python_references() {
        let source = r#"
def main():
    processor = DataProcessor()
    result = process_items([1, 2, 3])
"#;
        let known = vec!["DataProcessor".to_string(), "process_items".to_string()];
        let refs = extract_references(source, Language::Python, &known);
        let names: Vec<&str> = refs.iter().map(|r| r.symbol_name.as_str()).collect();
        assert!(
            names.contains(&"DataProcessor"),
            "should find DataProcessor: {:?}",
            names
        );
    }

    #[test]
    fn test_empty_known_symbols() {
        let source = "fn main() { let x = Config::new(); }";
        let refs = extract_references(source, Language::Rust, &[]);
        assert!(refs.is_empty());
    }

    #[test]
    fn test_ref_kind_display() {
        assert_eq!(RefKind::Import.to_string(), "import");
        assert_eq!(RefKind::Call.to_string(), "call");
        assert_eq!(RefKind::TypeRef.to_string(), "type_ref");
    }
}
