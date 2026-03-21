//! Architecture summary — module structure, public API, and dependency mapping.
//!
//! Combines tree, symbol extraction, and import scanning to produce
//! a structured overview of a codebase.

use crate::tools::impls::code::parse::{self, Language, SymbolKind};
use crate::tools::output::{self, BudgetedOutput};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SummaryError {
    #[error("path not found: {0}")]
    NotFound(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Top-level architecture summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchSummary {
    pub modules: Vec<ModuleSummary>,
    pub dependencies: Vec<ModuleDep>,
}

/// Summary of a single module/directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleSummary {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub public_symbols: Vec<String>,
    pub file_count: usize,
}

/// A dependency relationship between modules.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleDep {
    pub from_module: String,
    pub to_module: String,
    pub kind: String,
}

#[derive(Debug)]
struct FileContribution {
    relative: String,
    module: String,
    symbols: Vec<String>,
    description: Option<String>,
    deps: Vec<ModuleDep>,
}

/// Generate an architecture summary for a directory.
pub fn code_summary(
    root: &Path,
    max_tokens: Option<usize>,
) -> Result<BudgetedOutput, SummaryError> {
    if !root.exists() {
        return Err(SummaryError::NotFound(root.to_string_lossy().to_string()));
    }

    // 1. Walk files and group by top-level directory (module)
    let mut module_files: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let inv = crate::tools::impls::code::inventory::collect_inventory(root, None)?;
    let mut all_symbols: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut all_deps: Vec<ModuleDep> = Vec::new();
    let mut module_descriptions: BTreeMap<String, String> = BTreeMap::new();
    let mut parse_targets: Vec<(PathBuf, String, String)> = Vec::new();

    for entry in inv.entries.iter().filter(|e| !e.is_dir) {
        let path = entry.abs_path.as_path();
        let relative = entry.relative_path.clone();

        if relative.is_empty() {
            continue;
        }

        // Determine module: first path component or "." for root files
        let module = module_from_relative(&relative);

        module_files
            .entry(module.clone())
            .or_default()
            .push(relative.clone());

        // Collect parseable files for parallel parsing.
        if Language::from_path(path).is_some() {
            parse_targets.push((path.to_path_buf(), relative, module));
        }
    }

    // Parse files in parallel, then merge in deterministic relative-path order.
    let mut contributions: Vec<FileContribution> = parse_targets
        .into_par_iter()
        .filter_map(|(path, relative, module)| {
            let parsed = parse::parse_file(&path).ok()?;
            let source_lines: Vec<&str> = parsed.source.lines().collect();
            let symbols: Vec<String> = parsed
                .symbols
                .iter()
                .filter_map(|sym| {
                    match sym.kind {
                        SymbolKind::Function
                        | SymbolKind::Struct
                        | SymbolKind::Enum
                        | SymbolKind::Trait
                        | SymbolKind::Class
                        | SymbolKind::Interface
                        | SymbolKind::Type => {}
                        _ => return None,
                    }
                    if sym.name.starts_with("test_") {
                        return None;
                    }
                    if !is_public_symbol(&source_lines, sym, &parsed.language) {
                        return None;
                    }
                    Some(sym.name.clone())
                })
                .collect();

            let description = extract_module_doc(&parsed.source, &parsed.language);
            let deps = extract_imports(&parsed.source, &relative);

            Some(FileContribution {
                relative,
                module,
                symbols,
                description,
                deps,
            })
        })
        .collect();
    contributions.sort_by(|a, b| a.relative.cmp(&b.relative));

    for c in contributions {
        if !c.symbols.is_empty() {
            all_symbols
                .entry(c.module.clone())
                .or_default()
                .extend(c.symbols);
        }
        if let Some(desc) = c.description {
            module_descriptions.entry(c.module.clone()).or_insert(desc);
        }
        all_deps.extend(c.deps);
    }

    // 2. Build module summaries
    let modules: Vec<ModuleSummary> = module_files
        .iter()
        .map(|(module, files)| {
            let symbols = all_symbols.get(module).cloned().unwrap_or_default();
            ModuleSummary {
                path: module.clone(),
                description: module_descriptions.get(module).cloned(),
                public_symbols: symbols,
                file_count: files.len(),
            }
        })
        .collect();

    // 3. Deduplicate dependencies
    let mut seen = std::collections::HashSet::new();
    let dependencies: Vec<ModuleDep> = all_deps
        .into_iter()
        .filter(|d| seen.insert((d.from_module.clone(), d.to_module.clone(), d.kind.clone())))
        .collect();

    let summary = ArchSummary {
        modules,
        dependencies,
    };

    Ok(output::emit_json(&summary, max_tokens))
}

fn module_from_relative(relative: &str) -> String {
    relative
        .split('/')
        .next()
        .map(|first| {
            if relative.contains('/') {
                first.to_string()
            } else {
                ".".to_string()
            }
        })
        .unwrap_or_else(|| ".".to_string())
}

/// Check if a symbol is public by inspecting the source line where it's defined.
fn is_public_symbol(source_lines: &[&str], sym: &parse::Symbol, language: &Language) -> bool {
    // line_start is 1-indexed
    let line_idx = sym.line_start.saturating_sub(1);
    let line = match source_lines.get(line_idx) {
        Some(l) => l.trim(),
        None => return false,
    };

    match language {
        // Rust: public if the definition line starts with `pub`
        Language::Rust => line.starts_with("pub ") || line.starts_with("pub("),
        // Python: public if name doesn't start with underscore
        Language::Python => !sym.name.starts_with('_'),
        // TypeScript/JS: exported if line contains `export`
        Language::TypeScript => line.contains("export "),
        // Go: public if name starts with uppercase
        Language::Go => sym.name.starts_with(|c: char| c.is_uppercase()),
        // Other: include by default
        _ => true,
    }
}

/// Extract module-level doc comments from source.
fn extract_module_doc(source: &str, language: &Language) -> Option<String> {
    let mut doc_lines = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        match language {
            Language::Rust => {
                if let Some(content) = trimmed.strip_prefix("//!") {
                    doc_lines.push(content.trim().to_string());
                } else if !trimmed.is_empty() && !trimmed.starts_with("//") {
                    break;
                }
            }
            Language::Python => {
                // Simple: grab first docstring-like comment block
                if trimmed.starts_with("\"\"\"") || trimmed.starts_with("'''") {
                    let quote = &trimmed[..3];
                    if trimmed.len() > 6 && trimmed.ends_with(quote) {
                        doc_lines.push(trimmed[3..trimmed.len() - 3].to_string());
                    } else {
                        doc_lines.push(trimmed[3..].to_string());
                    }
                    break;
                } else if trimmed.starts_with('#') {
                    let content = trimmed.strip_prefix('#').unwrap_or("").trim();
                    doc_lines.push(content.to_string());
                } else if !trimmed.is_empty() {
                    break;
                }
            }
            _ => break, // Other languages: skip for now
        }
    }

    if doc_lines.is_empty() {
        None
    } else {
        Some(doc_lines.join(" ").trim().to_string())
    }
}

/// Check if a string looks like a valid module/identifier name.
/// Must start with a letter or underscore, contain only alphanumeric/underscore/hyphen chars.
fn is_valid_module_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let first = name.chars().next().unwrap();
    if !first.is_alphabetic() && first != '_' {
        return false;
    }
    name.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

/// Scan source for import/use/require statements and derive module dependencies.
fn extract_imports(source: &str, file_path: &str) -> Vec<ModuleDep> {
    let from_module = file_path
        .split('/')
        .next()
        .map(|first| {
            if file_path.contains('/') {
                first.to_string()
            } else {
                ".".to_string()
            }
        })
        .unwrap_or_else(|| ".".to_string());

    let mut deps = Vec::new();

    for line in source.lines() {
        let trimmed = line.trim();

        // Skip comments
        if trimmed.starts_with("//") || trimmed.starts_with('#') && !trimmed.starts_with("#[") {
            continue;
        }

        // Rust: `use crate::tools::module::...`
        if let Some(rest) = trimmed.strip_prefix("use ") {
            if let Some(target) = rest.strip_prefix("crate::tools::") {
                let to_module = target.split("::").next().unwrap_or("").to_string();
                if is_valid_module_name(&to_module) && to_module != from_module {
                    deps.push(ModuleDep {
                        from_module: from_module.clone(),
                        to_module,
                        kind: "use".to_string(),
                    });
                }
            }
            continue;
        }

        // Python: `from module import ...` (check before bare `import`)
        if let Some(rest) = trimmed.strip_prefix("from ") {
            let to_module = rest.split([' ', '.']).next().unwrap_or("").to_string();
            if is_valid_module_name(&to_module) && to_module != from_module {
                deps.push(ModuleDep {
                    from_module: from_module.clone(),
                    to_module,
                    kind: "import".to_string(),
                });
            }
            continue;
        }

        // Python: `import module`
        if trimmed.starts_with("import ") && !trimmed.contains(" from ") {
            let rest = &trimmed[7..];
            let to_module = rest.split([' ', ',', '.']).next().unwrap_or("").to_string();
            if is_valid_module_name(&to_module) && to_module != from_module {
                deps.push(ModuleDep {
                    from_module: from_module.clone(),
                    to_module,
                    kind: "import".to_string(),
                });
            }
            continue;
        }

        // JS/TS: `import ... from '...'`
        if trimmed.starts_with("import ") && trimmed.contains(" from ") {
            if let Some(from_pos) = trimmed.rfind(" from ") {
                let after = &trimmed[from_pos + 6..];
                if let Some(module) = extract_quoted_string(after) {
                    let to_module = module
                        .trim_start_matches("./")
                        .split('/')
                        .next()
                        .unwrap_or("")
                        .to_string();
                    if is_valid_module_name(&to_module)
                        && to_module != from_module
                        && !to_module.starts_with('.')
                    {
                        deps.push(ModuleDep {
                            from_module: from_module.clone(),
                            to_module,
                            kind: "import".to_string(),
                        });
                    }
                }
            }
            continue;
        }

        // JS/TS: `require('...')`
        if trimmed.contains("require(") {
            if let Some(start) = trimmed.find("require(") {
                let after = &trimmed[start + 8..];
                if let Some(module) = extract_quoted_string(after) {
                    let to_module = module
                        .trim_start_matches("./")
                        .split('/')
                        .next()
                        .unwrap_or("")
                        .to_string();
                    if is_valid_module_name(&to_module)
                        && to_module != from_module
                        && !to_module.starts_with('.')
                    {
                        deps.push(ModuleDep {
                            from_module: from_module.clone(),
                            to_module,
                            kind: "require".to_string(),
                        });
                    }
                }
            }
        }
    }

    deps
}

/// Extract a quoted string from text (single or double quotes).
fn extract_quoted_string(s: &str) -> Option<String> {
    let trimmed = s.trim();
    let quote = trimmed.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let rest = &trimmed[1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_summary_basic() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src").join("main.rs"),
            "//! Main entry point.\nfn main() {}\npub struct Config {}\npub fn run() {}\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("src").join("lib.rs"),
            "pub fn helper() {}\n",
        )
        .unwrap();
        fs::write(tmp.path().join("README.md"), "# Project\n").unwrap();

        let result = code_summary(tmp.path(), None).unwrap();
        let parsed: ArchSummary = serde_json::from_str(&result.content).unwrap();
        assert!(!parsed.modules.is_empty());
        // Should have a "src" module with symbols
        let src_module = parsed.modules.iter().find(|m| m.path == "src");
        assert!(src_module.is_some(), "should have src module");
        let src = src_module.unwrap();
        assert!(src.file_count >= 2);
        // fn main() is not pub, so should NOT appear
        assert!(!src.public_symbols.contains(&"main".to_string()));
        assert!(src.public_symbols.contains(&"Config".to_string()));
        assert!(src.public_symbols.contains(&"run".to_string()));
        assert!(src.public_symbols.contains(&"helper".to_string()));
    }

    #[test]
    fn test_summary_with_imports() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src").join("main.rs"),
            "use crate::tools::config::load;\nfn main() {}\n",
        )
        .unwrap();

        let result = code_summary(tmp.path(), None).unwrap();
        let parsed: ArchSummary = serde_json::from_str(&result.content).unwrap();
        // Should detect a dependency from src -> config
        let dep = parsed
            .dependencies
            .iter()
            .find(|d| d.from_module == "src" && d.to_module == "config");
        assert!(
            dep.is_some(),
            "should detect use crate::tools::config dependency"
        );
    }

    #[test]
    fn test_summary_module_doc() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src").join("lib.rs"),
            "//! Token budget engine for that-tools.\n\npub fn emit() {}\n",
        )
        .unwrap();

        let result = code_summary(tmp.path(), None).unwrap();
        let parsed: ArchSummary = serde_json::from_str(&result.content).unwrap();
        let src_module = parsed.modules.iter().find(|m| m.path == "src").unwrap();
        assert!(
            src_module.description.is_some(),
            "should extract module doc"
        );
        assert!(src_module
            .description
            .as_ref()
            .unwrap()
            .contains("Token budget"));
    }

    #[test]
    fn test_summary_not_found() {
        let result = code_summary(Path::new("/nonexistent"), None);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_quoted_string() {
        assert_eq!(
            extract_quoted_string("'module'"),
            Some("module".to_string())
        );
        assert_eq!(
            extract_quoted_string("\"module\""),
            Some("module".to_string())
        );
        assert_eq!(extract_quoted_string("noquotes"), None);
    }

    #[test]
    fn test_summary_empty_directory() {
        let tmp = TempDir::new().unwrap();
        // No files at all
        let result = code_summary(tmp.path(), None).unwrap();
        let parsed: ArchSummary = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.modules.is_empty());
        assert!(parsed.dependencies.is_empty());
    }

    #[test]
    fn test_summary_python_imports() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("app")).unwrap();
        fs::write(
            tmp.path().join("app").join("main.py"),
            "from flask import Flask\nimport requests\ndef run(): pass\n",
        )
        .unwrap();

        let result = code_summary(tmp.path(), None).unwrap();
        let parsed: ArchSummary = serde_json::from_str(&result.content).unwrap();

        // Should detect dependencies from "app" module
        let flask_dep = parsed
            .dependencies
            .iter()
            .find(|d| d.from_module == "app" && d.to_module == "flask");
        assert!(flask_dep.is_some(), "should detect 'from flask' import");

        let requests_dep = parsed
            .dependencies
            .iter()
            .find(|d| d.from_module == "app" && d.to_module == "requests");
        assert!(requests_dep.is_some(), "should detect 'import requests'");
    }

    #[test]
    fn test_summary_javascript_imports() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src").join("app.ts"),
            "import { Router } from 'express';\nconst db = require('mongoose');\nexport function handler() {}\n",
        )
        .unwrap();

        let result = code_summary(tmp.path(), None).unwrap();
        let parsed: ArchSummary = serde_json::from_str(&result.content).unwrap();

        let express_dep = parsed
            .dependencies
            .iter()
            .find(|d| d.from_module == "src" && d.to_module == "express");
        assert!(express_dep.is_some(), "should detect 'import from express'");

        let mongoose_dep = parsed
            .dependencies
            .iter()
            .find(|d| d.from_module == "src" && d.to_module == "mongoose");
        assert!(mongoose_dep.is_some(), "should detect require('mongoose')");
    }

    #[test]
    fn test_summary_go_visibility() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("pkg")).unwrap();
        fs::write(
            tmp.path().join("pkg").join("handler.go"),
            "package handler\n\nfunc HandleRequest() {}\nfunc privateHelper() {}\n",
        )
        .unwrap();

        let result = code_summary(tmp.path(), None).unwrap();
        let parsed: ArchSummary = serde_json::from_str(&result.content).unwrap();
        let pkg = parsed.modules.iter().find(|m| m.path == "pkg").unwrap();
        // Go: uppercase = public, lowercase = private
        assert!(
            pkg.public_symbols.contains(&"HandleRequest".to_string()),
            "uppercase Go function should be public"
        );
        assert!(
            !pkg.public_symbols.contains(&"privateHelper".to_string()),
            "lowercase Go function should not be public"
        );
    }

    #[test]
    fn test_summary_typescript_export_visibility() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("lib")).unwrap();
        fs::write(
            tmp.path().join("lib").join("api.ts"),
            "export function createApp() {}\nfunction internalHelper() {}\nexport class Server {}\n",
        )
        .unwrap();

        let result = code_summary(tmp.path(), None).unwrap();
        let parsed: ArchSummary = serde_json::from_str(&result.content).unwrap();
        let lib = parsed.modules.iter().find(|m| m.path == "lib").unwrap();
        assert!(
            lib.public_symbols.contains(&"createApp".to_string()),
            "exported TS function should be public"
        );
        assert!(
            lib.public_symbols.contains(&"Server".to_string()),
            "exported TS class should be public"
        );
        assert!(
            !lib.public_symbols.contains(&"internalHelper".to_string()),
            "non-exported TS function should not be public"
        );
    }

    #[test]
    fn test_summary_filters_test_functions() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src").join("lib.rs"),
            "pub fn real_function() {}\npub fn test_something() {}\npub fn test_another() {}\n",
        )
        .unwrap();

        let result = code_summary(tmp.path(), None).unwrap();
        let parsed: ArchSummary = serde_json::from_str(&result.content).unwrap();
        let src = parsed.modules.iter().find(|m| m.path == "src").unwrap();
        assert!(src.public_symbols.contains(&"real_function".to_string()));
        assert!(
            !src.public_symbols.contains(&"test_something".to_string()),
            "test_ prefixed functions should be filtered out"
        );
        assert!(
            !src.public_symbols.contains(&"test_another".to_string()),
            "test_ prefixed functions should be filtered out"
        );
    }

    #[test]
    fn test_summary_no_invalid_module_names() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        // Source with edge-case imports that could produce invalid names
        fs::write(
            tmp.path().join("src").join("main.rs"),
            "use crate::tools::config::load;\nuse std::collections::HashMap;\n// use crate::tools::bad;\n",
        )
        .unwrap();

        let result = code_summary(tmp.path(), None).unwrap();
        let parsed: ArchSummary = serde_json::from_str(&result.content).unwrap();
        for dep in &parsed.dependencies {
            assert!(
                is_valid_module_name(&dep.to_module),
                "invalid to_module: '{}'",
                dep.to_module
            );
            assert!(
                is_valid_module_name(&dep.from_module),
                "invalid from_module: '{}'",
                dep.from_module
            );
        }
    }

    #[test]
    fn test_summary_no_description_when_no_doc_comment() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        // File with no doc comments at top
        fs::write(
            tmp.path().join("src").join("lib.rs"),
            "pub fn no_docs() {}\n",
        )
        .unwrap();

        let result = code_summary(tmp.path(), None).unwrap();
        let parsed: ArchSummary = serde_json::from_str(&result.content).unwrap();
        let src = parsed.modules.iter().find(|m| m.path == "src").unwrap();
        assert!(
            src.description.is_none(),
            "should have no description when no doc comments exist"
        );
    }

    #[test]
    fn test_summary_deeply_nested_modules() {
        let tmp = TempDir::new().unwrap();
        // Deeply nested structure: src/tools/code/parse.rs
        fs::create_dir_all(tmp.path().join("src").join("tools").join("code")).unwrap();
        fs::write(
            tmp.path()
                .join("src")
                .join("tools")
                .join("code")
                .join("parse.rs"),
            "pub fn parse_file() {}\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("src").join("main.rs"),
            "pub fn main_func() {}\n",
        )
        .unwrap();

        let result = code_summary(tmp.path(), None).unwrap();
        let parsed: ArchSummary = serde_json::from_str(&result.content).unwrap();
        // Everything under src/ should be in the "src" module (first path component)
        let src = parsed.modules.iter().find(|m| m.path == "src").unwrap();
        assert!(
            src.file_count >= 2,
            "deeply nested files should all be grouped under top-level module"
        );
    }

    #[test]
    fn test_summary_multi_language_project() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("backend")).unwrap();
        fs::create_dir(tmp.path().join("frontend")).unwrap();
        fs::write(
            tmp.path().join("backend").join("server.rs"),
            "pub struct Server {}\npub fn start() {}\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("frontend").join("app.ts"),
            "export function render() {}\nexport class App {}\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("backend").join("utils.py"),
            "def helper(): pass\ndef _private(): pass\n",
        )
        .unwrap();

        let result = code_summary(tmp.path(), None).unwrap();
        let parsed: ArchSummary = serde_json::from_str(&result.content).unwrap();

        let backend = parsed.modules.iter().find(|m| m.path == "backend").unwrap();
        assert!(backend.public_symbols.contains(&"Server".to_string()));
        assert!(backend.public_symbols.contains(&"start".to_string()));
        assert!(backend.public_symbols.contains(&"helper".to_string()));
        // Python private functions (underscore prefix) should be excluded
        assert!(!backend.public_symbols.contains(&"_private".to_string()));

        let frontend = parsed
            .modules
            .iter()
            .find(|m| m.path == "frontend")
            .unwrap();
        assert!(frontend.public_symbols.contains(&"render".to_string()));
        assert!(frontend.public_symbols.contains(&"App".to_string()));
    }

    #[test]
    fn test_summary_deduplicates_dependencies() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        // Two files in same module both import config
        fs::write(
            tmp.path().join("src").join("a.rs"),
            "use crate::tools::config::A;\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("src").join("b.rs"),
            "use crate::tools::config::B;\n",
        )
        .unwrap();

        let result = code_summary(tmp.path(), None).unwrap();
        let parsed: ArchSummary = serde_json::from_str(&result.content).unwrap();
        let config_deps: Vec<_> = parsed
            .dependencies
            .iter()
            .filter(|d| d.from_module == "src" && d.to_module == "config")
            .collect();
        assert_eq!(
            config_deps.len(),
            1,
            "duplicate dependencies should be deduplicated"
        );
    }

    #[test]
    fn test_summary_root_files_grouped_as_dot() {
        let tmp = TempDir::new().unwrap();
        // Files at root level (not in any directory)
        fs::write(tmp.path().join("main.rs"), "pub fn main() {}\n").unwrap();
        fs::write(tmp.path().join("lib.rs"), "pub fn lib_fn() {}\n").unwrap();

        let result = code_summary(tmp.path(), None).unwrap();
        let parsed: ArchSummary = serde_json::from_str(&result.content).unwrap();
        let root = parsed.modules.iter().find(|m| m.path == ".");
        assert!(
            root.is_some(),
            "root-level files should be grouped under '.'"
        );
        assert!(root.unwrap().file_count >= 2);
    }

    #[test]
    fn test_summary_python_docstring_description() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("pkg")).unwrap();
        fs::write(
            tmp.path().join("pkg").join("main.py"),
            "\"\"\"Main application module.\"\"\"\ndef run(): pass\n",
        )
        .unwrap();

        let result = code_summary(tmp.path(), None).unwrap();
        let parsed: ArchSummary = serde_json::from_str(&result.content).unwrap();
        let pkg = parsed.modules.iter().find(|m| m.path == "pkg").unwrap();
        assert!(
            pkg.description.is_some(),
            "should extract Python docstring as description"
        );
        assert!(pkg
            .description
            .as_ref()
            .unwrap()
            .contains("Main application"));
    }

    #[test]
    fn test_is_valid_module_name_edge_cases() {
        assert!(!is_valid_module_name(""));
        assert!(!is_valid_module_name("="));
        assert!(!is_valid_module_name("123abc"));
        assert!(!is_valid_module_name(".hidden"));
        assert!(!is_valid_module_name("mod/path"));
        assert!(is_valid_module_name("config"));
        assert!(is_valid_module_name("_private"));
        assert!(is_valid_module_name("my-module"));
        assert!(is_valid_module_name("mod123"));
    }

    #[test]
    fn test_summary_comment_lines_not_parsed_as_imports() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src").join("lib.rs"),
            "// use crate::tools::commented_out::Module;\n/// use crate::tools::doc_comment::Thing;\npub fn real() {}\n",
        )
        .unwrap();

        let result = code_summary(tmp.path(), None).unwrap();
        let parsed: ArchSummary = serde_json::from_str(&result.content).unwrap();
        // Should NOT detect commented-out imports
        assert!(
            !parsed
                .dependencies
                .iter()
                .any(|d| d.to_module == "commented_out"),
            "commented-out imports should not be detected"
        );
    }

    #[test]
    fn test_summary_token_budget() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src").join("lib.rs"),
            "pub fn a() {}\npub fn b() {}\npub fn c() {}\n",
        )
        .unwrap();

        // Very small budget
        let result = code_summary(tmp.path(), Some(20)).unwrap();
        // Should still produce valid JSON
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&result.content);
        assert!(
            parsed.is_ok(),
            "output should be valid JSON even at small budget"
        );
    }
}
