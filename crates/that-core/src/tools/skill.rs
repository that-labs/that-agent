use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Host-side skill reader.
///
/// Reads files from one or more local skill roots.
/// This is the agent's primary progressive disclosure mechanism — it calls this
/// tool when a skill becomes relevant to the current task, loading only what it needs.
///
/// Supports:
/// - Reading SKILL.md (default) to get the skill's instructions
/// - Reading any reference file within the skill directory
/// - Listing available files in a skill directory
#[derive(Clone)]
pub struct ReadSkillTool {
    skill_roots: Vec<PathBuf>,
    skill_index: Arc<HashMap<String, PathBuf>>,
}

#[derive(Debug, Deserialize)]
pub struct ReadSkillArgs {
    /// Skill name as it appears in the skills catalog (e.g. "that-tools", "that-tools-code").
    pub name: String,
    /// File to read within the skill directory. Defaults to "SKILL.md".
    /// Use this to load reference files listed in SKILL.md (e.g. "references/grep.md").
    pub file: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ReadSkillOutput {
    /// Content of the requested file.
    pub content: String,
    /// Other files available in this skill directory for progressive reference loading.
    pub available_files: Vec<String>,
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ReadSkillError(String);

impl ReadSkillTool {
    pub fn new(skills_dir: PathBuf) -> Self {
        Self::new_with_roots(vec![skills_dir])
    }

    pub fn new_with_roots(skill_roots: Vec<PathBuf>) -> Self {
        let mut unique_roots = Vec::new();
        for root in skill_roots {
            if !unique_roots.iter().any(|r| r == &root) {
                unique_roots.push(root);
            }
        }
        let skill_index = Arc::new(build_skill_index(&unique_roots));
        Self {
            skill_roots: unique_roots,
            skill_index,
        }
    }
}

impl ReadSkillTool {
    fn call_inner(&self, args: ReadSkillArgs) -> Result<ReadSkillOutput, ReadSkillError> {
        let skill_dir = resolve_skill_dir(&self.skill_roots, &self.skill_index, &args.name)
            .ok_or_else(|| {
                ReadSkillError(format!(
                    "Skill '{}' not found in configured skill roots",
                    args.name
                ))
            })?;

        if !skill_dir.is_dir() {
            return Err(ReadSkillError(format!(
                "Skill '{}' not found in skills directory",
                args.name
            )));
        }

        let file = args.file.as_deref().unwrap_or("SKILL.md");
        let rel_file = sanitize_relative_path(file).map_err(ReadSkillError)?;
        let file_path = skill_dir.join(rel_file);

        let content = std::fs::read_to_string(&file_path).map_err(|e| {
            ReadSkillError(format!(
                "Failed to read '{file}' for skill '{}': {e}",
                args.name
            ))
        })?;

        // List all files in the skill directory for progressive disclosure
        let available_files = list_skill_files(&skill_dir);

        Ok(ReadSkillOutput {
            content,
            available_files,
        })
    }
}

/// Dispatch a `read_skill` tool call from the agent loop.
///
/// Parses the JSON args, executes the skill read, and returns a JSON string result.
pub async fn dispatch_read_skill(
    args_json: &str,
    skill_roots: &[PathBuf],
) -> Result<serde_json::Value, crate::tools::typed::ToolError> {
    let args: ReadSkillArgs = serde_json::from_str(args_json)
        .map_err(|e| crate::tools::typed::ToolError(format!("invalid args: {e}")))?;
    let tool = ReadSkillTool::new_with_roots(skill_roots.to_vec());
    tool.call_inner(args)
        .map(|out| serde_json::to_value(out).unwrap_or_default())
        .map_err(|e| crate::tools::typed::ToolError(e.0))
}

/// Recursively list all files in a skill directory, relative to that directory.
fn list_skill_files(dir: &std::path::Path) -> Vec<String> {
    let mut files = Vec::new();
    collect_files(dir, dir, &mut files);
    files.sort();
    files
}

fn collect_files(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.display().to_string());
            }
        } else if path.is_dir() {
            collect_files(root, &path, out);
        }
    }
}

fn build_skill_index(skill_roots: &[PathBuf]) -> HashMap<String, PathBuf> {
    let mut index = HashMap::new();
    for root in skill_roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
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

            if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                index
                    .entry(dir_name.to_string())
                    .or_insert_with(|| path.clone());
            }

            if let Ok(content) = std::fs::read_to_string(&skill_file) {
                if let Some((name, _, _)) = crate::skills::parse_frontmatter(&content) {
                    index.entry(name).or_insert(path.clone());
                }
            }
        }
    }
    index
}

fn resolve_skill_dir(
    skill_roots: &[PathBuf],
    skill_index: &HashMap<String, PathBuf>,
    name: &str,
) -> Option<PathBuf> {
    if let Some(path) = skill_index.get(name) {
        return Some(path.clone());
    }
    for root in skill_roots {
        let candidate = root.join(name);
        if candidate.join("SKILL.md").is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Dispatch a `list_skills` tool call — returns all discovered skills with name and description.
pub async fn dispatch_list_skills(
    skill_roots: &[PathBuf],
) -> Result<serde_json::Value, crate::tools::typed::ToolError> {
    let tool = ReadSkillTool::new_with_roots(skill_roots.to_vec());
    let mut skills = Vec::new();

    for (_key, dir) in tool.skill_index.iter() {
        let skill_file = dir.join("SKILL.md");
        let Ok(content) = std::fs::read_to_string(&skill_file) else {
            continue;
        };
        let (name, description, _meta) = match crate::skills::parse_frontmatter(&content) {
            Some(parsed) => parsed,
            None => continue,
        };
        let references = list_skill_files(dir)
            .into_iter()
            .filter(|f| f != "SKILL.md")
            .collect::<Vec<_>>();
        skills.push(serde_json::json!({
            "name": name,
            "description": description,
            "references": references,
        }));
    }

    skills.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });
    Ok(serde_json::json!({ "skills": skills }))
}

fn sanitize_relative_path(path: &str) -> Result<&Path, String> {
    let rel = Path::new(path);
    if rel.is_absolute() {
        return Err("Skill file path must be relative".to_string());
    }
    if rel.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err("Skill file path must not contain parent traversal".to_string());
    }
    Ok(rel)
}
