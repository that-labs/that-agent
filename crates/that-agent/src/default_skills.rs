use std::hash::{Hash, Hasher};
use std::path::Path;

fn current_install_stamp() -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    env!("CARGO_PKG_VERSION").hash(&mut hasher);
    for skill in DEFAULT_SKILLS {
        skill.rel_path.hash(&mut hasher);
        skill.content.hash(&mut hasher);
    }
    format!("{}:{:016x}", env!("CARGO_PKG_VERSION"), hasher.finish())
}

/// Check if the install marker matches the current bundled skill payload.
pub(crate) fn install_stamp_matches(marker: &Path) -> bool {
    std::fs::read_to_string(marker).ok().as_deref() == Some(current_install_stamp().as_str())
}

/// Write the current install stamp to a marker file.
pub(crate) fn stamp_install_state(marker: &Path) {
    let _ = std::fs::write(marker, current_install_stamp());
}

/// Default skills bundled with that-agent.
///
/// Each skill is embedded at compile time via `include_str!`. Skills with
/// `bootstrap: true` in their frontmatter are written to `~/.that-agent/skills/`
/// on every agent startup, ensuring the installed versions always match the
/// current binary. Files with a `rel_path` are written to that relative path
/// under the skills directory (supporting subdirectory layouts like references/).
struct DefaultSkill {
    /// Relative path under `~/.that-agent/agents/<name>/skills/` (e.g. `skill-name/SKILL.md`).
    rel_path: &'static str,
    content: &'static str,
}

const DEFAULT_SKILLS: &[DefaultSkill] = &[
    DefaultSkill {
        rel_path: "skill-creator/SKILL.md",
        content: include_str!("../skills/skill-creator/SKILL.md"),
    },
    DefaultSkill {
        rel_path: "channel-notify/SKILL.md",
        content: include_str!("../skills/channel-notify/SKILL.md"),
    },
    DefaultSkill {
        rel_path: "telegram-format/SKILL.md",
        content: include_str!("../skills/telegram-format/SKILL.md"),
    },
    DefaultSkill {
        rel_path: "channel-whitelist/SKILL.md",
        content: include_str!("../skills/channel-whitelist/SKILL.md"),
    },
    DefaultSkill {
        rel_path: "task-manager/SKILL.md",
        content: include_str!("../skills/task-manager/SKILL.md"),
    },
    DefaultSkill {
        rel_path: "that-plugins/SKILL.md",
        content: include_str!("../skills/that-plugins/SKILL.md"),
    },
    DefaultSkill {
        rel_path: "agent-orchestrator/SKILL.md",
        content: include_str!("../skills/agent-orchestrator/SKILL.md"),
    },
    DefaultSkill {
        rel_path: "git-workspace/SKILL.md",
        content: include_str!("../skills/git-workspace/SKILL.md"),
    },
    // that-tools skill + references (formerly installed via install_that_tools_skills_local)
    DefaultSkill {
        rel_path: "that-tools/SKILL.md",
        content: include_str!("tools/skills/SKILL.md"),
    },
    DefaultSkill {
        rel_path: "that-tools/references/code.md",
        content: include_str!("tools/skills/references/code.md"),
    },
    DefaultSkill {
        rel_path: "that-tools/references/fs.md",
        content: include_str!("tools/skills/references/fs.md"),
    },
    DefaultSkill {
        rel_path: "that-tools/references/search.md",
        content: include_str!("tools/skills/references/search.md"),
    },
    DefaultSkill {
        rel_path: "that-tools/references/memory.md",
        content: include_str!("tools/skills/references/memory.md"),
    },
    DefaultSkill {
        rel_path: "that-tools/references/exec.md",
        content: include_str!("tools/skills/references/exec.md"),
    },
    DefaultSkill {
        rel_path: "that-tools/references/human.md",
        content: include_str!("tools/skills/references/human.md"),
    },
    DefaultSkill {
        rel_path: "that-tools/references/index.md",
        content: include_str!("tools/skills/references/index.md"),
    },
];

/// Install all bundled default skills into the agent's skills directory.
///
/// Skills whose frontmatter contains `bootstrap: true` are gated on that flag.
/// Non-SKILL.md files (references, etc.) are always written.
/// A version marker (`.installed-version`) is checked first — if it matches
/// the current binary version, the install is skipped entirely.
pub fn install_default_skills(agent_name: &str) {
    let Some(skills_dir) = crate::skills::skills_dir_local(agent_name) else {
        tracing::warn!("Could not resolve home directory — skipping default skill install");
        return;
    };

    let marker = skills_dir.join(".installed-version");
    if install_stamp_matches(&marker) {
        return;
    }

    for skill in DEFAULT_SKILLS {
        // For SKILL.md files, check bootstrap flag; always write non-SKILL.md files (references)
        if skill.rel_path.ends_with("SKILL.md") && !has_bootstrap_flag(skill.content) {
            continue;
        }

        let dest = skills_dir.join(skill.rel_path);
        if let Some(parent) = dest.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(path = %skill.rel_path, error = %e, "Failed to create skill directory");
                continue;
            }
        }

        if let Err(e) = std::fs::write(&dest, skill.content) {
            tracing::warn!(path = %skill.rel_path, error = %e, "Failed to write default skill");
        } else {
            tracing::debug!(path = %skill.rel_path, "Default skill installed");
        }
    }

    // Write version marker after successful install.
    let _ = std::fs::create_dir_all(&skills_dir);
    stamp_install_state(&marker);
}

/// Return true if the SKILL.md frontmatter contains `bootstrap: true` under `metadata:`.
fn has_bootstrap_flag(content: &str) -> bool {
    crate::skills::parse_frontmatter(content)
        .map(|(_, _, meta)| meta.bootstrap)
        .unwrap_or(false)
}
