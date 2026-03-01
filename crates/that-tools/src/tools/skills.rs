//! Skills system — embedded documentation for tool categories.
//!
//! Layout (compiled-in via `include_str!`):
//!   skills/SKILL.md            — main that-tools skill (overview + quick-ref)
//!   skills/references/*.md     — per-tool detail, loaded on demand
//!   skills/install.md          — setup/install guide (separate skill)
//!
//! On disk after `that-tools skills install`:
//!   `<dest>/that-tools/SKILL.md`
//!   `<dest>/that-tools/references/code.md`  (and fs, search, memory, exec, human, index)
//!   `<dest>/that-tools-install/SKILL.md`

use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    pub name: String,
    pub content: String,
}

// Main entry point — always loaded when the "that-tools" skill triggers
const SKILL_MAIN: &str = include_str!("../../skills/SKILL.md");

// Reference files — loaded on demand by the agent
const REF_CODE: &str = include_str!("../../skills/references/code.md");
const REF_FS: &str = include_str!("../../skills/references/fs.md");
const REF_SEARCH: &str = include_str!("../../skills/references/search.md");
const REF_MEMORY: &str = include_str!("../../skills/references/memory.md");
const REF_HUMAN: &str = include_str!("../../skills/references/human.md");
const REF_INDEX: &str = include_str!("../../skills/references/index.md");
const REF_EXEC: &str = include_str!("../../skills/references/exec.md");

// NOTE: install.md is intentionally NOT compiled in.
// It is operator/human documentation only — exposing it as a skill confuses agents.
// See skills/install.md for setup instructions.

/// All readable skills: name → content.
/// "that-tools" is the main skill; others are reference files readable by name.
const ALL_READABLE: &[(&str, &str)] = &[
    ("that-tools", SKILL_MAIN),
    ("code", REF_CODE),
    ("fs", REF_FS),
    ("search", REF_SEARCH),
    ("memory", REF_MEMORY),
    ("exec", REF_EXEC),
    ("human", REF_HUMAN),
    ("index", REF_INDEX),
];

/// Relative install paths within the destination directory.
/// Mirrors the on-disk layout: `that-tools/SKILL.md`, `that-tools/references/*.md`.
fn install_rel_path(name: &str) -> &'static str {
    match name {
        "that-tools" => "that-tools/SKILL.md",
        "code" => "that-tools/references/code.md",
        "fs" => "that-tools/references/fs.md",
        "search" => "that-tools/references/search.md",
        "memory" => "that-tools/references/memory.md",
        "exec" => "that-tools/references/exec.md",
        "human" => "that-tools/references/human.md",
        "index" => "that-tools/references/index.md",
        _ => unreachable!("all names are covered above"),
    }
}

/// List all skills (main + references + install) with their one-line descriptions.
pub fn list() -> Vec<Skill> {
    ALL_READABLE
        .iter()
        .map(|(name, content)| Skill {
            name: name.to_string(),
            content: skill_description(content),
        })
        .collect()
}

/// Read a skill's full content by name.
/// "that-tools" returns the main SKILL.md; other names return their reference file.
pub fn read(name: &str) -> Option<Skill> {
    ALL_READABLE
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(name, content)| Skill {
            name: name.to_string(),
            content: content.to_string(),
        })
}

/// Result of installing one skill file.
#[derive(Debug, Serialize)]
pub struct InstalledSkill {
    pub name: String,
    pub path: String,
    pub skipped: bool, // true when file exists and --force was not set
}

/// Install skills into the standard agent skills directory layout.
///
/// Creates:
///   `<dest>/that-tools/SKILL.md`
///   `<dest>/that-tools/references/code.md`  (and fs, search, memory, exec, human, index)
///   `<dest>/that-tools-install/SKILL.md`
///
/// `skill_name` — install only this skill; `None` installs all.
/// `dest`       — destination root; defaults to `~/.claude/skills/`.
/// `force`      — overwrite existing files.
pub fn install(
    skill_name: Option<&str>,
    dest: Option<&Path>,
    force: bool,
) -> Result<Vec<InstalledSkill>, Box<dyn std::error::Error>> {
    let base = match dest {
        Some(p) => p.to_path_buf(),
        None => default_skills_dir()?,
    };

    let targets: Vec<(&str, &str)> = match skill_name {
        Some(name) => {
            let content = ALL_READABLE
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, c)| *c)
                .ok_or_else(|| {
                    format!(
                        "unknown skill '{}'. Available: {}",
                        name,
                        ALL_READABLE
                            .iter()
                            .map(|(n, _)| *n)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
            vec![(name, content)]
        }
        None => ALL_READABLE.to_vec(),
    };

    let mut results = Vec::new();
    for (name, content) in targets {
        let rel = install_rel_path(name);
        let dest_file = base.join(rel);

        if dest_file.exists() && !force {
            results.push(InstalledSkill {
                name: name.to_string(),
                path: dest_file.to_string_lossy().into_owned(),
                skipped: true,
            });
            continue;
        }

        if let Some(parent) = dest_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest_file, content)?;

        results.push(InstalledSkill {
            name: name.to_string(),
            path: dest_file.to_string_lossy().into_owned(),
            skipped: false,
        });
    }

    Ok(results)
}

/// Resolve the default skills installation directory.
///
/// 1. `~/.claude/skills/`  (Claude Code convention)
/// 2. `~/.config/skills/`  (generic fallback)
fn default_skills_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let home = dirs::home_dir().ok_or("cannot determine home directory")?;
    let claude_skills = home.join(".claude").join("skills");
    if claude_skills.exists() || claude_skills.parent().map(|p| p.exists()).unwrap_or(false) {
        return Ok(claude_skills);
    }
    Ok(home.join(".config").join("skills"))
}

/// Extract the description from YAML frontmatter.
///
/// Looks for `description: <value>` inside the opening `---` block.
/// Falls back to the first non-empty, non-heading line if no frontmatter is found.
fn skill_description(text: &str) -> String {
    let trimmed = text.trim_start();
    if let Some(after_open) = trimmed.strip_prefix("---") {
        if let Some(end) = after_open.find("\n---") {
            let frontmatter = &after_open[..end];
            for line in frontmatter.lines() {
                if let Some(rest) = line.strip_prefix("description:") {
                    return rest.trim().to_string();
                }
            }
        }
    }
    text.lines()
        .find(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#') && !t.starts_with('-')
        })
        .unwrap_or("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_returns_all_skills() {
        let skills = list();
        assert_eq!(skills.len(), 8); // that-tools + 7 references (install is operator-only, not a skill)
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"that-tools"));
        assert!(names.contains(&"code"));
        assert!(names.contains(&"fs"));
        assert!(names.contains(&"search"));
        assert!(names.contains(&"memory"));
        assert!(names.contains(&"human"));
        assert!(names.contains(&"index"));
        assert!(names.contains(&"exec"));
        assert!(
            !names.contains(&"install"),
            "install must not be exposed as a skill"
        );
    }

    #[test]
    fn test_read_main_skill() {
        let skill = read("that-tools").unwrap();
        assert_eq!(skill.name, "that-tools");
        // Main skill must contain the quick reference table and gotchas
        assert!(skill.content.contains("that code grep"));
        assert!(skill.content.contains("that fs ls"));
        assert!(skill.content.contains("that skills read"));
    }

    #[test]
    fn test_read_existing_reference() {
        let skill = read("code").unwrap();
        assert_eq!(skill.name, "code");
        assert!(skill.content.contains("that code read"));
        assert!(skill.content.contains("that code grep"));
    }

    #[test]
    fn test_read_nonexistent_skill() {
        assert!(read("nonexistent").is_none());
    }

    #[test]
    fn test_skills_have_nonempty_descriptions() {
        for skill in list() {
            assert!(
                !skill.content.is_empty(),
                "skill '{}' has empty description",
                skill.name
            );
        }
    }

    #[test]
    fn test_skill_descriptions_extracted_from_frontmatter() {
        for skill in list() {
            assert!(
                !skill.content.starts_with("---"),
                "skill '{}' description looks like a YAML separator",
                skill.name
            );
            assert!(
                skill.content.len() > 20,
                "skill '{}' description is suspiciously short: {:?}",
                skill.name,
                skill.content
            );
        }
    }

    #[test]
    fn test_all_skills_teach_cli() {
        let names = [
            "that-tools",
            "code",
            "fs",
            "search",
            "memory",
            "exec",
            "human",
            "index",
        ];
        for name in names {
            let skill = read(name).unwrap();
            assert!(
                skill.content.contains("that "),
                "skill '{}' does not mention 'that' CLI",
                name
            );
        }
    }

    #[test]
    fn test_install_skill_not_readable_by_agents() {
        // install.md is operator docs — agents must not be able to read it
        assert!(
            read("install").is_none(),
            "install must not be readable as a skill"
        );
    }

    #[test]
    fn test_search_skill_documents_free_engines() {
        let skill = read("search").unwrap();
        for engine in &["duckduckgo", "bing", "yahoo", "mojeek"] {
            assert!(
                skill.content.contains(engine),
                "search skill missing engine '{}'",
                engine
            );
        }
    }

    #[test]
    fn test_search_skill_documents_fetch_modes() {
        let skill = read("search").unwrap();
        for mode in &["scrape", "inspect", "markdown", "text"] {
            assert!(
                skill.content.contains(mode),
                "search skill missing fetch mode '{}'",
                mode
            );
        }
    }

    #[test]
    fn test_code_skill_covers_core_operations() {
        let skill = read("code").unwrap();
        for op in &[
            "read", "grep", "tree", "symbols", "edit", "ast-grep", "index", "summary",
        ] {
            assert!(
                skill.content.to_lowercase().contains(op),
                "code skill missing operation '{}'",
                op
            );
        }
    }

    #[test]
    fn test_memory_skill_covers_core_operations() {
        let skill = read("memory").unwrap();
        for op in &[
            "add", "recall", "search", "prune", "stats", "export", "import",
        ] {
            assert!(
                skill.content.to_lowercase().contains(op),
                "memory skill missing operation '{}'",
                op
            );
        }
    }

    #[test]
    fn test_human_skill_covers_core_operations() {
        let skill = read("human").unwrap();
        for op in &["ask", "approve", "confirm", "pending"] {
            assert!(
                skill.content.to_lowercase().contains(op),
                "human skill missing operation '{}'",
                op
            );
        }
    }

    #[test]
    fn test_main_skill_has_all_gotchas() {
        let skill = read("that-tools").unwrap();
        // The main skill must pre-empt every observed agent mistake
        assert!(
            skill.content.contains("grep"),
            "main skill must mention grep"
        );
        assert!(skill.content.contains("ls"), "main skill must mention ls");
        assert!(skill.content.contains("cat"), "main skill must mention cat");
        assert!(
            skill.content.contains("--path"),
            "main skill must warn about --path flag"
        );
        assert!(
            skill.content.contains("pattern"),
            "main skill must show pattern-first order"
        );
    }

    #[test]
    fn test_skill_description_extraction() {
        let text = "---\nname: test\ndescription: This is the test description.\n---\n# Body";
        assert_eq!(skill_description(text), "This is the test description.");
    }

    #[test]
    fn test_skill_description_fallback_without_frontmatter() {
        let text = "# Heading\n\nFirst real paragraph here.";
        assert_eq!(skill_description(text), "First real paragraph here.");
    }

    // --- install tests ---

    #[test]
    fn test_install_creates_nested_structure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let installed = install(None, Some(tmp.path()), false).unwrap();
        assert_eq!(installed.len(), 8); // that-tools + 7 references, no install skill

        // Main skill
        assert!(tmp.path().join("that-tools/SKILL.md").exists());
        // References
        for name in &["code", "fs", "search", "memory", "exec", "human", "index"] {
            let p = tmp
                .path()
                .join(format!("that-tools/references/{}.md", name));
            assert!(p.exists(), "missing reference: {}", p.display());
        }
        // Install skill must NOT be created — it's operator docs, not an agent skill
        assert!(!tmp.path().join("that-tools-install/SKILL.md").exists());
    }

    #[test]
    fn test_install_single_reference() {
        let tmp = tempfile::TempDir::new().unwrap();
        let installed = install(Some("code"), Some(tmp.path()), false).unwrap();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].name, "code");
        assert!(!installed[0].skipped);
        assert!(tmp.path().join("that-tools/references/code.md").exists());
    }

    #[test]
    fn test_install_main_skill() {
        let tmp = tempfile::TempDir::new().unwrap();
        let installed = install(Some("that-tools"), Some(tmp.path()), false).unwrap();
        assert_eq!(installed.len(), 1);
        assert!(tmp.path().join("that-tools/SKILL.md").exists());
    }

    #[test]
    fn test_install_skips_existing_without_force() {
        let tmp = tempfile::TempDir::new().unwrap();
        install(Some("fs"), Some(tmp.path()), false).unwrap();
        let second = install(Some("fs"), Some(tmp.path()), false).unwrap();
        assert!(second[0].skipped);
    }

    #[test]
    fn test_install_force_overwrites() {
        let tmp = tempfile::TempDir::new().unwrap();
        install(Some("fs"), Some(tmp.path()), false).unwrap();
        let skill_file = tmp.path().join("that-tools/references/fs.md");
        std::fs::write(&skill_file, "garbage").unwrap();
        let result = install(Some("fs"), Some(tmp.path()), true).unwrap();
        assert!(!result[0].skipped);
        let content = std::fs::read_to_string(&skill_file).unwrap();
        assert_ne!(content, "garbage");
        assert!(content.contains("that fs ls"));
    }

    #[test]
    fn test_install_unknown_skill_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let result = install(Some("nonexistent"), Some(tmp.path()), false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nonexistent"));
    }

    #[test]
    fn test_install_rel_paths_cover_all_skills() {
        // Every skill in ALL_READABLE must have a corresponding install path
        for (name, _) in ALL_READABLE {
            // This will panic (unreachable!) if a name is missing from install_rel_path
            let _ = install_rel_path(name);
        }
    }
}
