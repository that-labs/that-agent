use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use tracing::{debug, warn};

thread_local! {
    static BIN_CACHE: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
}

/// Metadata declared in a skill's YAML frontmatter under the `metadata:` key.
#[derive(Debug, Clone, Default)]
pub struct SkillMetadata {
    /// Marks skills bundled with the binary that should be auto-installed on startup.
    pub bootstrap: bool,
    /// Binary names required for this skill to function — all must be on PATH.
    pub binaries: Vec<String>,
    /// At least one of these binary names must exist on PATH for the skill to be eligible.
    pub any_bins: Vec<String>,
    /// Environment variable specs required by this skill.
    /// Format per entry: `${VAR_NAME}` or `ALIAS: ${VAR_NAME}`.
    pub envvars: Vec<String>,
    /// Allowed OS names (e.g. `["darwin", "linux"]`); empty means any OS is accepted.
    pub os: Vec<String>,
    /// If true, inject the full skill body into the agent preamble without requiring read_skill.
    pub always: bool,
    /// Semver string (informational only, no validation performed).
    pub version: Option<String>,
}

/// A discovered skill — metadata from YAML frontmatter plus the full body content.
#[derive(Debug, Clone)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    /// Absolute path to the SKILL.md file (for display and TUI skill view).
    #[allow(dead_code)]
    pub path: String,
    pub metadata: SkillMetadata,
    /// Pre-stripped body for `always: true` skills — avoids a file re-read in preamble building.
    pub body: Option<String>,
}

/// Return the skills directory path inside the sandbox container for the given agent.
pub fn skills_dir_sandbox(agent_name: &str) -> String {
    format!("/home/agent/.that-agent/agents/{}/skills", agent_name)
}

/// Return the local skills directory: `~/.that-agent/agents/<name>/skills/`.
pub fn skills_dir_local(agent_name: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".that-agent")
            .join("agents")
            .join(agent_name)
            .join("skills")
    })
}

/// Create an agent-scoped skill scaffold at
/// `~/.that-agent/agents/<agent>/skills/<skill>/SKILL.md`.
pub fn create_skill_scaffold_local(
    agent_name: &str,
    skill_name: &str,
    force: bool,
) -> Result<PathBuf> {
    let skill_dir_name = crate::sandbox::scope::normalize_skill_dir_name(skill_name);
    if skill_dir_name.is_empty() {
        anyhow::bail!("Skill name must contain at least one alphanumeric character");
    }
    let skills_root = crate::sandbox::scope::ensure_scope_path(
        agent_name,
        &crate::sandbox::scope::ScopeTarget::AgentSkills,
    )?;
    let skill_dir = skills_root.join(&skill_dir_name);
    std::fs::create_dir_all(&skill_dir)?;
    let skill_path = skill_dir.join("SKILL.md");
    if skill_path.exists() && !force {
        anyhow::bail!(
            "Skill '{}' already exists at {} (use --force to overwrite)",
            skill_dir_name,
            skill_path.display()
        );
    }
    std::fs::write(
        &skill_path,
        format!(
            "---\nname: {skill_dir_name}\ndescription: {skill_dir_name} skill\nmetadata:\n  bootstrap: false\n  always: false\n---\n\nAdd skill instructions here.\n"
        ),
    )?;
    Ok(skill_path)
}

/// Discover skills on the host filesystem by scanning the skills directory.
///
/// Skills that fail eligibility checks (missing binaries, wrong OS, unset env vars) are
/// silently skipped with a debug log entry.
pub fn discover_skills_local(dir: &Path) -> Vec<SkillMeta> {
    let mut skills = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return skills,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let skill_file = path.join("SKILL.md");
        if !skill_file.exists() {
            continue;
        }

        let content = match std::fs::read_to_string(&skill_file) {
            Ok(c) => c,
            Err(e) => {
                warn!(path = %skill_file.display(), error = %e, "Failed to read skill file");
                continue;
            }
        };

        if let Some((name, description, metadata)) = parse_frontmatter(&content) {
            match check_eligibility(&metadata) {
                Err(reason) => {
                    debug!(skill = %name, reason = %reason, "Skill ineligible — skipping");
                }
                Ok(()) => {
                    let body = metadata
                        .always
                        .then(|| strip_frontmatter(&content).to_string());
                    skills.push(SkillMeta {
                        name,
                        description,
                        path: skill_file.display().to_string(),
                        metadata,
                        body,
                    });
                }
            }
        } else {
            debug!(path = %skill_file.display(), "Skill file missing valid frontmatter, skipping");
        }
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Check whether a skill is eligible to run in the current environment.
///
/// Returns `Ok(())` if all requirements are satisfied, or `Err(reason)` with a
/// human-readable explanation of the first unmet requirement.
///
/// Checks are applied in order:
/// 1. `os` — current OS must be in the list (if non-empty)
/// 2. `binaries` — all must be executable on PATH
/// 3. `any_bins` — at least one must be executable on PATH
/// 4. `envvars` — every referenced variable must be set in the environment
pub fn check_eligibility(metadata: &SkillMetadata) -> Result<(), String> {
    // 1. OS check
    if !metadata.os.is_empty() {
        let current = current_os_name();
        if !metadata.os.iter().any(|o| o == current) {
            return Err(format!(
                "not supported on this OS ({}); requires one of: {}",
                current,
                metadata.os.join(", ")
            ));
        }
    }

    // 2. binaries — all must exist
    let missing_bins: Vec<&str> = metadata
        .binaries
        .iter()
        .filter(|b| !binary_exists(b))
        .map(|b| b.as_str())
        .collect();
    if !missing_bins.is_empty() {
        return Err(format!("missing binaries: {}", missing_bins.join(", ")));
    }

    // 3. any_bins — at least one must exist
    if !metadata.any_bins.is_empty() {
        let any_found = metadata.any_bins.iter().any(|b| binary_exists(b));
        if !any_found {
            return Err(format!(
                "none of the required binaries found: {}",
                metadata.any_bins.join(", ")
            ));
        }
    }

    // 4. envvars — each referenced variable must be set
    let missing_vars: Vec<String> = metadata
        .envvars
        .iter()
        .map(|spec| extract_env_var_name(spec))
        .filter(|var| std::env::var(var).is_err())
        .collect();
    if !missing_vars.is_empty() {
        return Err(format!(
            "missing environment variables: {}",
            missing_vars.join(", ")
        ));
    }

    Ok(())
}

/// Returns `true` if `name` is an executable file in any directory listed in `$PATH`.
/// Results are cached per-thread for the lifetime of the process.
fn binary_exists(name: &str) -> bool {
    BIN_CACHE.with(|cache| {
        if let Some(&result) = cache.borrow().get(name) {
            return result;
        }
        let found = binary_exists_uncached(name);
        cache.borrow_mut().insert(name.to_string(), found);
        found
    })
}

#[cfg(unix)]
fn binary_exists_uncached(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p).any(|dir| {
                let f = dir.join(name);
                f.is_file()
                    && f.metadata()
                        .map(|m| m.permissions().mode() & 0o111 != 0)
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[cfg(windows)]
fn binary_exists_uncached(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p).any(|dir| {
                ["", ".exe", ".bat", ".cmd"]
                    .iter()
                    .any(|ext| dir.join(format!("{name}{ext}")).is_file())
            })
        })
        .unwrap_or(false)
}

#[cfg(not(any(unix, windows)))]
fn binary_exists_uncached(_name: &str) -> bool {
    false
}

/// Extract the environment variable name from a spec string.
///
/// Handles both plain `${VAR}` and aliased `ALIAS: ${VAR}` forms.
/// Falls back to returning the spec as-is if no `${...}` pattern is found.
fn extract_env_var_name(spec: &str) -> String {
    let spec = spec.trim();
    if let Some(start) = spec.find("${") {
        let after = &spec[start + 2..];
        if let Some(end) = after.find('}') {
            return after[..end].to_string();
        }
    }
    spec.to_string()
}

/// Return the canonical OS name used in skill `os:` lists.
fn current_os_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "win32"
    } else {
        "unknown"
    }
}

/// Active list being accumulated during frontmatter parsing.
#[derive(Debug, PartialEq)]
enum ActiveList {
    None,
    Binaries,
    AnyBins,
    Os,
    Envvars,
}

/// Parse YAML frontmatter from a SKILL.md file.
///
/// Expected format (2-space indentation under `metadata:`):
///
/// ```text
/// ---
/// name: skill-name
/// description: A short description
/// metadata:
///   bootstrap: true
///   always: false
///   version: 1.0.0
///   os:
///     - darwin
///     - linux
///   binaries:
///     - that-tools
///     - node
///   any_bins:
///     - bun
///     - node
///   envvars:
///     - ${API_KEY}
///     - ALIAS: ${OTHER_VAR}
/// ---
/// ```
///
/// Returns `(name, description, metadata)` or `None` if required fields are missing.
pub fn parse_frontmatter(content: &str) -> Option<(String, String, SkillMetadata)> {
    let content = content.trim_start();

    if !content.starts_with("---") {
        return None;
    }

    // Find the closing `---`
    let after_open = &content[3..];
    let close_idx = after_open.find("\n---")?;
    let frontmatter = &after_open[..close_idx];

    let mut name = None;
    let mut description = None;
    let mut metadata = SkillMetadata::default();

    // Track which block we're currently inside based on indentation.
    let mut in_metadata = false;
    let mut active_list = ActiveList::None;

    for line in frontmatter.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let indent = line.len() - line.trim_start_matches(' ').len();
        let trimmed = line.trim();

        if indent == 0 {
            // Root-level key — reset all sub-block state.
            in_metadata = false;
            active_list = ActiveList::None;

            if let Some(v) = trimmed.strip_prefix("name:") {
                name = Some(v.trim().to_string());
            } else if let Some(v) = trimmed.strip_prefix("description:") {
                description = Some(v.trim().to_string());
            } else if trimmed == "metadata:" {
                in_metadata = true;
            }
        } else if in_metadata && indent == 2 {
            // Metadata sub-key — reset active list then check the key.
            active_list = ActiveList::None;

            if trimmed == "binaries:" {
                active_list = ActiveList::Binaries;
            } else if trimmed == "any_bins:" {
                active_list = ActiveList::AnyBins;
            } else if trimmed == "os:" {
                active_list = ActiveList::Os;
            } else if trimmed == "envvars:" {
                active_list = ActiveList::Envvars;
            } else if let Some(v) = trimmed.strip_prefix("bootstrap:") {
                metadata.bootstrap = v.trim() == "true";
            } else if let Some(v) = trimmed.strip_prefix("always:") {
                metadata.always = v.trim() == "true";
            } else if let Some(v) = trimmed.strip_prefix("version:") {
                let ver = v.trim().to_string();
                if !ver.is_empty() {
                    metadata.version = Some(ver);
                }
            }
        } else if active_list != ActiveList::None && indent >= 4 {
            if let Some(item) = trimmed.strip_prefix('-') {
                let val = item.trim().to_string();
                if !val.is_empty() {
                    match active_list {
                        ActiveList::Binaries => metadata.binaries.push(val),
                        ActiveList::AnyBins => metadata.any_bins.push(val),
                        ActiveList::Os => metadata.os.push(val),
                        ActiveList::Envvars => metadata.envvars.push(val),
                        ActiveList::None => {}
                    }
                }
            }
        }
    }

    match (name, description) {
        (Some(n), Some(d)) if !n.is_empty() && !d.is_empty() => Some((n, d, metadata)),
        _ => None,
    }
}

/// Strip YAML frontmatter from skill content, returning only the body.
///
/// Finds the closing `---` of the frontmatter block and returns everything after it,
/// with leading newlines trimmed. Returns the original content unchanged if no valid
/// frontmatter is detected.
fn strip_frontmatter(content: &str) -> &str {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return content;
    }
    let after = &trimmed[3..];
    after
        .find("\n---")
        .map(|i| after[i + 4..].trim_start_matches('\n'))
        .unwrap_or(content)
}

/// Format the skill catalog section for the agent's preamble.
///
/// Skills marked `always: true` have their full body injected inline as named sections.
/// Remaining skills are listed in the "Available Skills" catalog for progressive loading
/// via `read_skill(name)`. Always-skills are excluded from the catalog list since their
/// content is already present in the preamble.
pub fn format_skill_preamble(skills: &[SkillMeta], skills_path: &str) -> String {
    let mut out = String::new();

    out.push_str("## Skills\n\n");
    out.push_str(&format!(
        "Your skills directory: `{skills_path}`  \n\
         New or updated skill files are hot-reloaded automatically — no restart needed.  \n\
         Skill naming must be deterministic and kebab-case. If the user does not provide a name, \
         derive it from the core capability phrase and keep role nouns stable \
         (e.g. `JSON formatter` -> `json-formatter`, `task manager` -> `task-manager`).  \n\
         Do not substitute role nouns with alternates like `formatting` when `formatter` is implied.\n\n\
         ### Installing skills\n\n\
         When the user provides a repository URL or download link for a skill, \
         **clone or download it** into the skills directory — never manually recreate the content with `fs_write`. \
         Use `shell_exec` to run the appropriate command (e.g. clone the repository directly into the skills path). \
         Only use `fs_write` for skills you are authoring from scratch.\n\n\
         ### Reading skills\n\n\
         **Always use `read_skill(name)` to read skill content** — never use `fs_cat` or other file-reading tools \
         on skill files. `read_skill` returns the skill body along with available reference files for progressive \
         disclosure, which raw file reads cannot provide.\n\n"
    ));

    if skills.is_empty() {
        out.push_str("No skills installed yet.\n\n");
        return out;
    }

    // Partition into always-injected and catalog skills.
    let always_skills: Vec<&SkillMeta> = skills.iter().filter(|s| s.metadata.always).collect();
    let catalog_skills: Vec<&SkillMeta> = skills.iter().filter(|s| !s.metadata.always).collect();

    // Inject always-skills inline as named sections.
    for skill in &always_skills {
        out.push_str(&format!("### {}\n", skill.name));
        let body = skill.body.as_deref().unwrap_or_default();
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("\n---\n\n");
    }

    // Catalog skills with progressive-disclosure instructions.
    if !catalog_skills.is_empty() {
        out.push_str(
            "Use `list_skills()` to discover all available skills at any time. \
             **Before starting a task, scan this list and `read_skill(name)` any skill \
             whose description matches what you are about to do.** \
             If an installed skill appears relevant to the problem domain, framework, or implementation \
             style the user is asking for, read it before choosing libraries, architecture, or workflows. \
             The tool returns the skill body and lists any reference files available \
             for deeper progressive loading. \
             Call `read_skill` only once per skill per conversation — \
             the content is already in your context after the first load. \
             Only reload a skill if your context was compacted and the content is no longer visible.\n\n",
        );
        out.push_str("### Available Skills\n\n");
        for skill in &catalog_skills {
            out.push_str(&format!("- **{}**: {}\n", skill.name, skill.description));
        }
        out.push('\n');
    }

    out
}

/// Delete a skill directory (local mode only).
///
/// Removes the entire `<name>/` folder under the given skills directory.
/// Returns `Ok(())` on success or an error if the directory doesn't exist or can't be removed.
pub fn delete_skill_local(dir: &Path, name: &str) -> Result<(), String> {
    let skill_dir = dir.join(name);
    if !skill_dir.is_dir() {
        return Err(format!("Skill '{}' not found", name));
    }
    std::fs::remove_dir_all(&skill_dir)
        .map_err(|e| format!("Failed to delete skill '{}': {}", name, e))
}

/// Read the full content of a skill file (local mode).
pub fn read_skill_local(dir: &Path, name: &str) -> Option<String> {
    let skill_file = dir.join(name).join("SKILL.md");
    std::fs::read_to_string(skill_file).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Frontmatter parsing ───────────────────────────────────────────────

    #[test]
    fn test_parse_frontmatter_valid() {
        let content = "---\nname: greet\ndescription: A greeting skill\n---\n\nHello world!";
        let result = parse_frontmatter(content);
        assert!(result.is_some());
        let (name, desc, meta) = result.unwrap();
        assert_eq!(name, "greet");
        assert_eq!(desc, "A greeting skill");
        assert!(!meta.bootstrap);
        assert!(meta.binaries.is_empty());
        assert!(meta.envvars.is_empty());
        assert!(meta.any_bins.is_empty());
        assert!(meta.os.is_empty());
        assert!(!meta.always);
        assert!(meta.version.is_none());
    }

    #[test]
    fn test_parse_frontmatter_no_opening() {
        let content = "name: greet\ndescription: A greeting skill\n---\n";
        assert!(parse_frontmatter(content).is_none());
    }

    #[test]
    fn test_parse_frontmatter_missing_fields() {
        let content = "---\nname: greet\n---\n";
        assert!(parse_frontmatter(content).is_none());
    }

    #[test]
    fn test_parse_frontmatter_empty_values() {
        let content = "---\nname:\ndescription:\n---\n";
        assert!(parse_frontmatter(content).is_none());
    }

    #[test]
    fn test_parse_frontmatter_with_whitespace() {
        let content = "\n  ---\nname:  my-skill \ndescription:  Does things \n---\n";
        let result = parse_frontmatter(content);
        assert!(result.is_some());
        let (name, desc, _) = result.unwrap();
        assert_eq!(name, "my-skill");
        assert_eq!(desc, "Does things");
    }

    #[test]
    fn test_parse_frontmatter_with_metadata() {
        let content = indoc(
            "---
name: deploy
description: Handles deployment
metadata:
  bootstrap: true
  always: true
  version: 2.1.0
  os:
    - darwin
    - linux
  binaries:
    - that-tools
    - node
  any_bins:
    - bun
    - yarn
  envvars:
    - ${API_KEY}
    - ALIAS: ${OTHER_VAR}
---
",
        );
        let (name, desc, meta) = parse_frontmatter(&content).unwrap();
        assert_eq!(name, "deploy");
        assert_eq!(desc, "Handles deployment");
        assert!(meta.bootstrap);
        assert!(meta.always);
        assert_eq!(meta.version.as_deref(), Some("2.1.0"));
        assert_eq!(meta.os, vec!["darwin", "linux"]);
        assert_eq!(meta.binaries, vec!["that-tools", "node"]);
        assert_eq!(meta.any_bins, vec!["bun", "yarn"]);
        assert_eq!(meta.envvars, vec!["${API_KEY}", "ALIAS: ${OTHER_VAR}"]);
    }

    #[test]
    fn test_parse_frontmatter_bootstrap_at_root_is_ignored() {
        // bootstrap: true at root level (old format) should NOT set the flag
        let content = "---\nname: greet\ndescription: A greeting skill\nbootstrap: true\n---\n";
        let (_, _, meta) = parse_frontmatter(content).unwrap();
        assert!(!meta.bootstrap);
    }

    // ── strip_frontmatter ─────────────────────────────────────────────────

    #[test]
    fn test_strip_frontmatter_basic() {
        let content = "---\nname: foo\ndescription: bar\n---\n\nBody content here.";
        let body = strip_frontmatter(content);
        assert_eq!(body, "Body content here.");
    }

    #[test]
    fn test_strip_frontmatter_no_frontmatter() {
        let content = "Just a plain body.";
        let body = strip_frontmatter(content);
        assert_eq!(body, "Just a plain body.");
    }

    #[test]
    fn test_strip_frontmatter_leading_newlines_trimmed() {
        let content = "---\nname: foo\ndescription: bar\n---\n\n\nBody after gaps.";
        let body = strip_frontmatter(content);
        assert_eq!(body, "Body after gaps.");
    }

    // ── check_eligibility ─────────────────────────────────────────────────

    #[test]
    fn test_eligibility_passes() {
        let meta = SkillMetadata::default();
        assert!(check_eligibility(&meta).is_ok());
    }

    #[test]
    fn test_eligibility_missing_binary() {
        let meta = SkillMetadata {
            binaries: vec!["__definitely_not_a_real_binary_xyz__".into()],
            ..Default::default()
        };
        let result = check_eligibility(&meta);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing binaries"));
    }

    #[test]
    fn test_eligibility_missing_any_bin() {
        let meta = SkillMetadata {
            any_bins: vec!["__not_real_bin_a__".into(), "__not_real_bin_b__".into()],
            ..Default::default()
        };
        let result = check_eligibility(&meta);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("none of the required binaries"));
    }

    #[test]
    fn test_eligibility_os_mismatch() {
        let meta = SkillMetadata {
            os: vec!["not-this-os".into()],
            ..Default::default()
        };
        let result = check_eligibility(&meta);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not supported on this OS"));
    }

    #[test]
    fn test_eligibility_missing_envvar() {
        let meta = SkillMetadata {
            envvars: vec!["${__THAT_AGENT_TEST_VAR_DEFINITELY_UNSET__}".into()],
            ..Default::default()
        };
        let result = check_eligibility(&meta);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing environment variables"));
    }

    #[test]
    fn test_eligibility_os_current_accepted() {
        // The current OS name should be accepted when included in the list.
        let current = current_os_name();
        let meta = SkillMetadata {
            os: vec![current.to_string()],
            ..Default::default()
        };
        assert!(check_eligibility(&meta).is_ok());
    }

    // ── format_skill_preamble ─────────────────────────────────────────────

    #[test]
    fn test_format_skill_preamble_empty() {
        let out = format_skill_preamble(&[], "/some/path");
        assert!(out.contains("No skills installed"));
    }

    #[test]
    fn test_format_skill_preamble_with_skills() {
        let skills = vec![SkillMeta {
            name: "greet".into(),
            description: "Says hello".into(),
            path: "/skills/greet/SKILL.md".into(),
            metadata: SkillMetadata::default(),
            body: None,
        }];
        let out = format_skill_preamble(&skills, "/skills");
        assert!(out.contains("**greet**"));
        assert!(out.contains("Says hello"));
        assert!(out.contains("Available Skills"));
    }

    #[test]
    fn test_format_preamble_always_skill_injected() {
        // Create a real temp file so strip_frontmatter + fs::read_to_string work.
        let dir = std::env::temp_dir().join("that_agent_test_always_skill");
        let skill_dir = dir.join("my-always-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_file = skill_dir.join("SKILL.md");
        std::fs::write(
            &skill_file,
            "---\nname: my-always-skill\ndescription: Always injected\nmetadata:\n  always: true\n---\n\nThis body should appear.\n",
        )
        .unwrap();

        let skills = vec![SkillMeta {
            name: "my-always-skill".into(),
            description: "Always injected".into(),
            path: skill_file.display().to_string(),
            metadata: SkillMetadata {
                always: true,
                ..Default::default()
            },
            body: Some("This body should appear.\n".into()),
        }];

        let out = format_skill_preamble(&skills, dir.to_str().unwrap());

        // Body must appear inline.
        assert!(out.contains("This body should appear."));
        // Skill must NOT be listed in the catalog.
        assert!(!out.contains("Available Skills"));
        assert!(!out.contains("read_skill"));
        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_format_preamble_always_skill_excluded_from_catalog() {
        // When both an always-skill and a catalog skill are present, only the catalog
        // skill appears in "Available Skills".
        let dir = std::env::temp_dir().join("that_agent_test_mixed_skills");
        let always_dir = dir.join("inline-skill");
        std::fs::create_dir_all(&always_dir).unwrap();
        let always_file = always_dir.join("SKILL.md");
        std::fs::write(
            &always_file,
            "---\nname: inline-skill\ndescription: Always present\nmetadata:\n  always: true\n---\n\nInline body.\n",
        )
        .unwrap();

        let skills = vec![
            SkillMeta {
                name: "inline-skill".into(),
                description: "Always present".into(),
                path: always_file.display().to_string(),
                metadata: SkillMetadata {
                    always: true,
                    ..Default::default()
                },
                body: Some("Inline body.\n".into()),
            },
            SkillMeta {
                name: "catalog-skill".into(),
                description: "On demand".into(),
                path: "/nonexistent/catalog-skill/SKILL.md".into(),
                metadata: SkillMetadata::default(),
                body: None,
            },
        ];

        let out = format_skill_preamble(&skills, dir.to_str().unwrap());

        assert!(out.contains("Inline body."));
        assert!(out.contains("**catalog-skill**"));
        assert!(!out.contains("**inline-skill**"));
        assert!(out.contains("Available Skills"));
        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Minimal indoc helper — strips the leading newline from a raw string literal.
    fn indoc(s: &str) -> String {
        s.trim_start_matches('\n').to_string()
    }
}
