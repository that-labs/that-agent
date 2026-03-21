//! Tree-sitter based source code parsing and symbol extraction.
//!
//! Every source file is parsed into an AST on access. Symbol definitions
//! (functions, structs, classes, traits, interfaces) are extracted for
//! indexing and context-aware reading.

use serde::Serialize;
use std::path::Path;
use thiserror::Error;
use tree_sitter::{Parser, Tree};

#[derive(Error, Debug)]
pub enum ParseError {
    #[error("unsupported language for file: {0}")]
    UnsupportedLanguage(String),
    #[error("parse failed for file: {0}")]
    ParseFailed(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Supported programming languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Rust,
    TypeScript,
    Python,
    Go,
    Json,
}

impl Language {
    /// Detect language from file extension.
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?;
        match ext {
            "rs" => Some(Language::Rust),
            "ts" | "tsx" => Some(Language::TypeScript),
            "js" | "jsx" => Some(Language::TypeScript), // TS parser handles JS
            "py" => Some(Language::Python),
            "go" => Some(Language::Go),
            "json" => Some(Language::Json),
            _ => None,
        }
    }

    /// Get the tree-sitter language for this language.
    pub fn tree_sitter_language(&self) -> tree_sitter::Language {
        match self {
            Language::Rust => tree_sitter_rust::LANGUAGE.into(),
            Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Language::Python => tree_sitter_python::LANGUAGE.into(),
            Language::Go => tree_sitter_go::LANGUAGE.into(),
            Language::Json => tree_sitter_json::LANGUAGE.into(),
        }
    }
}

/// A symbol extracted from source code.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub line_start: usize,
    pub line_end: usize,
    /// Byte offsets in the source.
    pub byte_start: usize,
    pub byte_end: usize,
}

/// Kind of code symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    Function,
    Struct,
    Enum,
    Trait,
    Impl,
    Class,
    Interface,
    Method,
    Module,
    Constant,
    Variable,
    Type,
}

/// A parsed source file with its AST and extracted symbols.
/// Fields `source` and `tree` are retained for Phase 2 incremental indexing.
#[allow(dead_code)]
pub struct ParsedFile {
    pub language: Language,
    pub source: String,
    pub tree: Tree,
    pub symbols: Vec<Symbol>,
}

/// Parse a source file and extract symbols.
pub fn parse_file(path: &Path) -> Result<ParsedFile, ParseError> {
    let language = Language::from_path(path)
        .ok_or_else(|| ParseError::UnsupportedLanguage(path.to_string_lossy().to_string()))?;

    let source = std::fs::read_to_string(path)?;
    parse_source(&source, language)
}

/// Parse source code string with the given language.
pub fn parse_source(source: &str, language: Language) -> Result<ParsedFile, ParseError> {
    let mut parser = Parser::new();
    parser
        .set_language(&language.tree_sitter_language())
        .map_err(|_| ParseError::ParseFailed("failed to set language".to_string()))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| ParseError::ParseFailed("tree-sitter parse returned None".to_string()))?;

    let symbols = extract_symbols(&tree, source, language);

    Ok(ParsedFile {
        language,
        source: source.to_string(),
        tree,
        symbols,
    })
}

/// Extract symbols from a parsed AST using iterative traversal.
fn extract_symbols(tree: &Tree, source: &str, language: Language) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        if let Some((name, kind)) = classify_node(node, source, language) {
            symbols.push(Symbol {
                name,
                kind,
                line_start: node.start_position().row + 1,
                line_end: node.end_position().row + 1,
                byte_start: node.start_byte(),
                byte_end: node.end_byte(),
            });
        }

        // Push children in reverse order so left-to-right traversal is preserved
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }

    symbols
}

/// Classify a tree-sitter node as a known symbol kind.
fn classify_node(
    node: tree_sitter::Node,
    source: &str,
    language: Language,
) -> Option<(String, SymbolKind)> {
    let kind = node.kind();

    match language {
        Language::Rust => classify_rust_node(node, source, kind),
        Language::TypeScript => classify_typescript_node(node, source, kind),
        Language::Python => classify_python_node(node, source, kind),
        Language::Go => classify_go_node(node, source, kind),
        Language::Json => None, // JSON has no symbols
    }
}

fn classify_rust_node(
    node: tree_sitter::Node,
    source: &str,
    kind: &str,
) -> Option<(String, SymbolKind)> {
    match kind {
        "function_item" => {
            let name = find_child_text(node, "name", source)?;
            Some((name, SymbolKind::Function))
        }
        "struct_item" => {
            let name = find_child_text(node, "name", source)?;
            Some((name, SymbolKind::Struct))
        }
        "enum_item" => {
            let name = find_child_text(node, "name", source)?;
            Some((name, SymbolKind::Enum))
        }
        "trait_item" => {
            let name = find_child_text(node, "name", source)?;
            Some((name, SymbolKind::Trait))
        }
        "impl_item" => {
            // For impl blocks, try to get the type name
            let name = find_child_by_kind(node, "type_identifier")
                .and_then(|n| Some(n.utf8_text(source.as_bytes()).ok()?.to_string()))
                .unwrap_or_else(|| "impl".to_string());
            Some((name, SymbolKind::Impl))
        }
        "const_item" => {
            let name = find_child_text(node, "name", source)?;
            Some((name, SymbolKind::Constant))
        }
        "type_item" => {
            let name = find_child_text(node, "name", source)?;
            Some((name, SymbolKind::Type))
        }
        "mod_item" => {
            let name = find_child_text(node, "name", source)?;
            Some((name, SymbolKind::Module))
        }
        _ => None,
    }
}

fn classify_typescript_node(
    node: tree_sitter::Node,
    source: &str,
    kind: &str,
) -> Option<(String, SymbolKind)> {
    match kind {
        "function_declaration" => {
            let name = find_child_text(node, "name", source)?;
            Some((name, SymbolKind::Function))
        }
        "class_declaration" => {
            let name = find_child_text(node, "name", source)?;
            Some((name, SymbolKind::Class))
        }
        "interface_declaration" => {
            let name = find_child_text(node, "name", source)?;
            Some((name, SymbolKind::Interface))
        }
        "method_definition" | "public_field_definition" => {
            let name = find_child_text(node, "name", source)?;
            Some((name, SymbolKind::Method))
        }
        "enum_declaration" => {
            let name = find_child_text(node, "name", source)?;
            Some((name, SymbolKind::Enum))
        }
        "type_alias_declaration" => {
            let name = find_child_text(node, "name", source)?;
            Some((name, SymbolKind::Type))
        }
        "lexical_declaration" | "variable_declaration" => {
            // Capture top-level or export-level const/let declarations
            let is_top_level = node
                .parent()
                .is_some_and(|p| p.kind() == "program" || p.kind() == "export_statement");
            if is_top_level {
                let name = find_child_by_kind(node, "variable_declarator")
                    .and_then(|vd| find_child_text(vd, "name", source));
                name.map(|n| (n, SymbolKind::Variable))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn classify_python_node(
    node: tree_sitter::Node,
    source: &str,
    kind: &str,
) -> Option<(String, SymbolKind)> {
    match kind {
        "function_definition" => {
            let name = find_child_text(node, "name", source)?;
            // Distinguish methods (inside class body) from free functions
            let is_method = node.parent().is_some_and(|p| {
                p.kind() == "block" && p.parent().is_some_and(|gp| gp.kind() == "class_definition")
            });
            if is_method {
                Some((name, SymbolKind::Method))
            } else {
                Some((name, SymbolKind::Function))
            }
        }
        "class_definition" => {
            let name = find_child_text(node, "name", source)?;
            Some((name, SymbolKind::Class))
        }
        _ => None,
    }
}

fn classify_go_node(
    node: tree_sitter::Node,
    source: &str,
    kind: &str,
) -> Option<(String, SymbolKind)> {
    match kind {
        "function_declaration" => {
            let name = find_child_text(node, "name", source)?;
            Some((name, SymbolKind::Function))
        }
        "method_declaration" => {
            let name = find_child_text(node, "name", source)?;
            Some((name, SymbolKind::Method))
        }
        "type_spec" => {
            // Handle type_spec directly — called from recursive walk
            let name = find_child_text(node, "name", source)?;
            let has_struct = find_child_by_kind(node, "struct_type").is_some();
            let has_interface = find_child_by_kind(node, "interface_type").is_some();
            let sym_kind = if has_struct {
                SymbolKind::Struct
            } else if has_interface {
                SymbolKind::Interface
            } else {
                SymbolKind::Type
            };
            Some((name, sym_kind))
        }
        _ => None,
    }
}

/// Find a named child node's text content.
fn find_child_text(node: tree_sitter::Node, field_name: &str, source: &str) -> Option<String> {
    let child = node.child_by_field_name(field_name)?;
    child
        .utf8_text(source.as_bytes())
        .ok()
        .map(|s| s.to_string())
}

/// Find a child node by its kind (node type name).
fn find_child_by_kind<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    let result = node.children(&mut cursor).find(|c| c.kind() == kind);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_language_detection_rust() {
        assert_eq!(
            Language::from_path(Path::new("main.rs")),
            Some(Language::Rust)
        );
    }

    #[test]
    fn test_language_detection_typescript() {
        assert_eq!(
            Language::from_path(Path::new("app.ts")),
            Some(Language::TypeScript)
        );
        assert_eq!(
            Language::from_path(Path::new("app.tsx")),
            Some(Language::TypeScript)
        );
    }

    #[test]
    fn test_language_detection_python() {
        assert_eq!(
            Language::from_path(Path::new("main.py")),
            Some(Language::Python)
        );
    }

    #[test]
    fn test_language_detection_go() {
        assert_eq!(
            Language::from_path(Path::new("main.go")),
            Some(Language::Go)
        );
    }

    #[test]
    fn test_language_detection_unknown() {
        assert_eq!(Language::from_path(Path::new("file.xyz")), None);
    }

    #[test]
    fn test_parse_rust_source() {
        let source = r#"
fn main() {
    println!("hello");
}

struct Config {
    name: String,
    value: i32,
}

enum Status {
    Active,
    Inactive,
}

trait Processor {
    fn process(&self);
}

impl Config {
    fn new() -> Self {
        Config { name: String::new(), value: 0 }
    }
}

const MAX_SIZE: usize = 100;

type Result<T> = std::result::Result<T, Error>;

mod utils;
"#;

        let parsed = parse_source(source, Language::Rust).unwrap();
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"main"));
        assert!(names.contains(&"Config"));
        assert!(names.contains(&"Status"));
        assert!(names.contains(&"Processor"));
        assert!(names.contains(&"MAX_SIZE"));
        assert!(names.contains(&"utils"));

        // Check symbol kinds
        let main_sym = parsed.symbols.iter().find(|s| s.name == "main").unwrap();
        assert_eq!(main_sym.kind, SymbolKind::Function);

        let config_sym = parsed
            .symbols
            .iter()
            .find(|s| s.name == "Config" && s.kind == SymbolKind::Struct)
            .unwrap();
        assert_eq!(config_sym.kind, SymbolKind::Struct);

        let status_sym = parsed.symbols.iter().find(|s| s.name == "Status").unwrap();
        assert_eq!(status_sym.kind, SymbolKind::Enum);
    }

    #[test]
    fn test_parse_typescript_source() {
        let source = r#"
function greet(name: string): string {
    return `Hello, ${name}!`;
}

class UserService {
    getUser(id: string) {
        return { id };
    }
}

interface ApiResponse {
    data: unknown;
    status: number;
}

enum Role {
    Admin,
    User,
}

type UserId = string;
"#;

        let parsed = parse_source(source, Language::TypeScript).unwrap();
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"greet"));
        assert!(names.contains(&"UserService"));
        assert!(names.contains(&"ApiResponse"));
        assert!(names.contains(&"Role"));
        assert!(names.contains(&"UserId"));
    }

    #[test]
    fn test_parse_python_source() {
        let source = r#"
def process_data(items):
    return [x * 2 for x in items]

class DataProcessor:
    def __init__(self):
        self.data = []

    def run(self):
        pass
"#;

        let parsed = parse_source(source, Language::Python).unwrap();
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"process_data"));
        assert!(names.contains(&"DataProcessor"));
        assert!(names.contains(&"__init__"));
        assert!(names.contains(&"run"));

        // Free function vs method distinction
        let process_sym = parsed
            .symbols
            .iter()
            .find(|s| s.name == "process_data")
            .unwrap();
        assert_eq!(process_sym.kind, SymbolKind::Function);

        let init_sym = parsed
            .symbols
            .iter()
            .find(|s| s.name == "__init__")
            .unwrap();
        assert_eq!(init_sym.kind, SymbolKind::Method);

        let run_sym = parsed.symbols.iter().find(|s| s.name == "run").unwrap();
        assert_eq!(run_sym.kind, SymbolKind::Method);
    }

    #[test]
    fn test_parse_go_source() {
        let source = r#"package main

func main() {
    fmt.Println("hello")
}

type Config struct {
    Name string
}

func (c *Config) Validate() error {
    return nil
}
"#;

        let parsed = parse_source(source, Language::Go).unwrap();
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"main"));
        assert!(names.contains(&"Config"));
        assert!(names.contains(&"Validate"));
    }

    #[test]
    fn test_parse_go_multiple_types() {
        let source = r#"package main

type (
    Request struct {
        URL string
    }
    Response struct {
        Body string
    }
    Handler interface {
        Handle(r Request) Response
    }
)
"#;

        let parsed = parse_source(source, Language::Go).unwrap();
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(
            names.contains(&"Request"),
            "should find Request: {:?}",
            names
        );
        assert!(
            names.contains(&"Response"),
            "should find Response: {:?}",
            names
        );
        assert!(
            names.contains(&"Handler"),
            "should find Handler: {:?}",
            names
        );

        let request_sym = parsed.symbols.iter().find(|s| s.name == "Request").unwrap();
        assert_eq!(request_sym.kind, SymbolKind::Struct);

        let handler_sym = parsed.symbols.iter().find(|s| s.name == "Handler").unwrap();
        assert_eq!(handler_sym.kind, SymbolKind::Interface);
    }

    #[test]
    fn test_symbol_line_numbers() {
        let source = "fn first() {}\nfn second() {}\nfn third() {}\n";
        let parsed = parse_source(source, Language::Rust).unwrap();

        assert_eq!(parsed.symbols.len(), 3);
        assert_eq!(parsed.symbols[0].line_start, 1);
        assert_eq!(parsed.symbols[1].line_start, 2);
        assert_eq!(parsed.symbols[2].line_start, 3);
    }

    #[test]
    fn test_parse_empty_source() {
        let parsed = parse_source("", Language::Rust).unwrap();
        assert!(parsed.symbols.is_empty());
    }

    #[test]
    fn test_parse_file_from_disk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file_path = tmp.path().join("test.rs");
        std::fs::write(&file_path, "fn hello() {}\n").unwrap();

        let parsed = parse_file(&file_path).unwrap();
        assert_eq!(parsed.symbols.len(), 1);
        assert_eq!(parsed.symbols[0].name, "hello");
    }

    #[test]
    fn test_parse_unsupported_extension() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file_path = tmp.path().join("test.xyz");
        std::fs::write(&file_path, "some content").unwrap();

        let result = parse_file(&file_path);
        assert!(result.is_err());
    }
}
