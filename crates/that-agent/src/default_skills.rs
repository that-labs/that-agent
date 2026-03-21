use std::hash::{Hash, Hasher};
use std::path::Path;

fn current_install_stamp() -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    env!("CARGO_PKG_VERSION").hash(&mut hasher);
    for skill in DEFAULT_SKILLS {
        if has_bootstrap_flag(skill.content) {
            skill.name.hash(&mut hasher);
            skill.content.hash(&mut hasher);
        }
    }
    format!("{}:{:016x}", env!("CARGO_PKG_VERSION"), hasher.finish())
}

/// Check if a version marker file matches the current binary version.
pub(crate) fn version_matches(marker: &Path) -> bool {
    std::fs::read_to_string(marker).ok().as_deref() == Some(env!("CARGO_PKG_VERSION"))
}

/// Write the current binary version to a marker file.
pub(crate) fn stamp_version(marker: &Path) {
    let _ = std::fs::write(marker, env!("CARGO_PKG_VERSION"));
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
/// current binary.
struct DefaultSkill {
    /// Directory name under `~/.that-agent/skills/`
    name: &'static str,
    content: &'static str,
}

const DEFAULT_SKILLS: &[DefaultSkill] = &[
    DefaultSkill {
        name: "skill-creator",
        content: include_str!("../skills/skill-creator/SKILL.md"),
    },
    DefaultSkill {
        name: "channel-notify",
        content: include_str!("../skills/channel-notify/SKILL.md"),
    },
    DefaultSkill {
        name: "telegram-format",
        content: include_str!("../skills/telegram-format/SKILL.md"),
    },
    DefaultSkill {
        name: "channel-whitelist",
        content: include_str!("../skills/channel-whitelist/SKILL.md"),
    },
    DefaultSkill {
        name: "task-manager",
        content: include_str!("../skills/task-manager/SKILL.md"),
    },
    DefaultSkill {
        name: "that-plugins",
        content: include_str!("../skills/that-plugins/SKILL.md"),
    },
    DefaultSkill {
        name: "agent-orchestrator",
        content: include_str!("../skills/agent-orchestrator/SKILL.md"),
    },
    DefaultSkill {
        name: "git-workspace",
        content: include_str!("../skills/git-workspace/SKILL.md"),
    },
];

/// Install all bundled default skills into the agent's skills directory.
///
/// Only skills whose frontmatter contains `bootstrap: true` are written.
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
        if !has_bootstrap_flag(skill.content) {
            continue;
        }

        let skill_dir = skills_dir.join(skill.name);
        if let Err(e) = std::fs::create_dir_all(&skill_dir) {
            tracing::warn!(skill = skill.name, error = %e, "Failed to create skill directory");
            continue;
        }

        let dest = skill_dir.join("SKILL.md");
        if let Err(e) = std::fs::write(&dest, skill.content) {
            tracing::warn!(skill = skill.name, error = %e, "Failed to write default skill");
        } else {
            tracing::debug!(skill = skill.name, "Default skill installed");
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
