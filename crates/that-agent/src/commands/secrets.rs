use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};

use crate::cli::{self, SecretsCommands};

// ── Legacy bashrc constants (kept for migration) ────────────────────────────

const SECRETS_BLOCK_START: &str = "# >>> that-managed-secrets >>>";
const SECRETS_BLOCK_END: &str = "# <<< that-managed-secrets <<<";

// ── Validation ──────────────────────────────────────────────────────────────

fn validate_secret_key(key: &str) -> anyhow::Result<()> {
    if key.is_empty() {
        anyhow::bail!("Secret key cannot be empty");
    }
    let mut chars = key.chars();
    let first = chars.next().unwrap_or('_');
    if !(first.is_ascii_alphabetic() || first == '_') {
        anyhow::bail!("Invalid secret key '{key}': must start with [A-Za-z_]");
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        anyhow::bail!("Invalid secret key '{key}': use only [A-Za-z0-9_]");
    }
    Ok(())
}

// ── Path helpers ────────────────────────────────────────────────────────────

fn agent_home(agent_name: &str) -> anyhow::Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Failed to resolve home directory"))?;
    Ok(home.join(".that-agent").join("agents").join(agent_name))
}

pub fn agent_bashrc_path(agent_name: &str) -> anyhow::Result<PathBuf> {
    Ok(agent_home(agent_name)?.join(".bashrc"))
}

fn secrets_file_path(agent_home: &Path) -> PathBuf {
    agent_home.join(".secrets")
}

fn key_file_path(agent_home: &Path) -> PathBuf {
    agent_home.join(".secret_key")
}

// ── Key management ──────────────────────────────────────────────────────────

fn get_or_create_key(agent_home: &Path) -> anyhow::Result<[u8; 32]> {
    let path = key_file_path(agent_home);
    if path.exists() {
        let data = std::fs::read(&path)?;
        if data.len() != 32 {
            anyhow::bail!(
                "Corrupt secret key file (expected 32 bytes, got {})",
                data.len()
            );
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&data);
        return Ok(key);
    }
    let key: [u8; 32] = rand::random();
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, key)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(key)
}

// ── Encrypt / Decrypt ───────────────────────────────────────────────────────

fn encrypt_secret(key: &[u8; 32], plaintext: &str) -> String {
    let cipher = ChaCha20Poly1305::new(key.into());
    let nonce_bytes: [u8; 12] = rand::random();
    let nonce = Nonce::from(nonce_bytes);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .expect("encryption should not fail");
    let mut combined = nonce_bytes.to_vec();
    combined.extend_from_slice(&ciphertext);
    B64.encode(&combined)
}

fn decrypt_secret(key: &[u8; 32], encoded: &str) -> Option<String> {
    let combined = B64.decode(encoded).ok()?;
    if combined.len() < 12 {
        return None;
    }
    let (nonce_bytes, ciphertext) = combined.split_at(12);
    let cipher = ChaCha20Poly1305::new(key.into());
    let nonce = Nonce::from_slice(nonce_bytes);
    let plaintext = cipher.decrypt(nonce, ciphertext).ok()?;
    String::from_utf8(plaintext).ok()
}

// ── Encrypted secrets store (.secrets) ──────────────────────────────────────

fn load_encrypted_secrets(
    agent_home: &Path,
    key: &[u8; 32],
) -> anyhow::Result<BTreeMap<String, String>> {
    let path = secrets_file_path(agent_home);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let data = std::fs::read_to_string(&path)?;
    let encrypted: BTreeMap<String, String> = serde_json::from_str(&data)?;
    let mut secrets = BTreeMap::new();
    for (k, enc_val) in encrypted {
        if let Some(plain) = decrypt_secret(key, &enc_val) {
            secrets.insert(k, plain);
        }
    }
    Ok(secrets)
}

fn save_encrypted_secrets(
    agent_home: &Path,
    key: &[u8; 32],
    secrets: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let encrypted: BTreeMap<String, String> = secrets
        .iter()
        .map(|(k, v)| (k.clone(), encrypt_secret(key, v)))
        .collect();
    let json = serde_json::to_string_pretty(&encrypted)?;
    let path = secrets_file_path(agent_home);
    std::fs::write(&path, json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

// ── Legacy bashrc parsing (for migration + non-secret exports) ──────────────

fn unquote_bash_single(value: &str) -> String {
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        value[1..value.len() - 1].replace("'\"'\"'", "'")
    } else {
        value.to_string()
    }
}

fn parse_export_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("export ")?;
    let (key, raw_value) = rest.split_once('=')?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    Some((key.to_string(), unquote_bash_single(raw_value.trim())))
}

fn load_secrets_from_bashrc(content: &str) -> BTreeMap<String, String> {
    let mut in_block = false;
    let mut secrets = BTreeMap::new();
    for line in content.lines() {
        if line.trim() == SECRETS_BLOCK_START {
            in_block = true;
            continue;
        }
        if line.trim() == SECRETS_BLOCK_END {
            break;
        }
        if in_block {
            if let Some((k, v)) = parse_export_line(line) {
                secrets.insert(k, v);
            }
        }
    }
    secrets
}

/// Parse ALL export lines from bashrc content (non-secret exports for env injection).
pub fn load_exports_from_bashrc(content: &str) -> BTreeMap<String, String> {
    let mut exports = BTreeMap::new();
    for line in content.lines() {
        if let Some((k, v)) = parse_export_line(line) {
            exports.insert(k, v);
        }
    }
    exports
}

fn remove_secrets_block(content: &str) -> String {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == SECRETS_BLOCK_START {
            in_block = true;
            continue;
        }
        if trimmed == SECRETS_BLOCK_END {
            in_block = false;
            continue;
        }
        if !in_block {
            out.push(line);
        }
    }
    out.join("\n")
}

// ── Migration ───────────────────────────────────────────────────────────────

/// Migrate secrets from legacy .bashrc block to encrypted .secrets file.
/// Returns the migrated secrets (empty if nothing to migrate).
fn migrate_from_bashrc(
    agent_home: &Path,
    key: &[u8; 32],
) -> anyhow::Result<BTreeMap<String, String>> {
    let bashrc_path = agent_home.join(".bashrc");
    if !bashrc_path.exists() {
        return Ok(BTreeMap::new());
    }
    let content = std::fs::read_to_string(&bashrc_path)?;
    let legacy_secrets = load_secrets_from_bashrc(&content);
    if legacy_secrets.is_empty() {
        return Ok(BTreeMap::new());
    }
    // Save to encrypted store
    save_encrypted_secrets(agent_home, key, &legacy_secrets)?;
    // Remove secrets block from .bashrc
    let cleaned = remove_secrets_block(&content);
    std::fs::write(&bashrc_path, cleaned)?;
    Ok(legacy_secrets)
}

// ── Public load (used by inject_agent_profile_env) ──────────────────────────

/// Load decrypted secrets for an agent. Handles migration from legacy .bashrc format.
pub fn load_agent_secrets(agent_name: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let home = agent_home(agent_name)?;
    if !home.exists() {
        return Ok(BTreeMap::new());
    }
    let key = get_or_create_key(&home)?;
    let secrets_path = secrets_file_path(&home);
    if !secrets_path.exists() {
        // Try migrating from legacy bashrc
        let migrated = migrate_from_bashrc(&home, &key)?;
        if !migrated.is_empty() {
            return Ok(migrated);
        }
    }
    load_encrypted_secrets(&home, &key)
}

// ── CLI handler ─────────────────────────────────────────────────────────────

fn required_agent_name_or_exit(
    cli: &cli::Cli,
    ws: &crate::config::WorkspaceConfig,
    usage_hint: &str,
) -> String {
    match &cli.agent {
        Some(name) => name.clone(),
        None => {
            let agents = ws.list_agents().unwrap_or_default();
            eprintln!("Error: --agent is required. Specify which agent to use.\n");
            if agents.is_empty() {
                eprintln!(
                    "No agents found. Run 'that agent init <name> --api-key <KEY>' to create one."
                );
            } else {
                eprintln!("Available agents:");
                for name in &agents {
                    eprintln!("  {name}");
                }
                eprintln!("\nUsage: {usage_hint}");
            }
            std::process::exit(1);
        }
    }
}

pub fn handle_secrets_command(cli: &cli::Cli, command: &SecretsCommands) -> anyhow::Result<()> {
    let ws = crate::config::WorkspaceConfig::load(cli.workspace.as_deref())?;
    let agent_name =
        required_agent_name_or_exit(cli, &ws, "that --agent <name> secrets <add|delete> ...");
    let home = agent_home(&agent_name)?;
    std::fs::create_dir_all(&home)?;

    let key = get_or_create_key(&home)?;

    // Migrate from legacy bashrc if .secrets doesn't exist yet
    let secrets_path = secrets_file_path(&home);
    if !secrets_path.exists() {
        migrate_from_bashrc(&home, &key)?;
    }

    let mut secrets = load_encrypted_secrets(&home, &key)?;

    match command {
        SecretsCommands::Add {
            key: secret_key,
            value,
        } => {
            validate_secret_key(secret_key)?;
            secrets.insert(secret_key.clone(), value.clone());
            save_encrypted_secrets(&home, &key, &secrets)?;
            println!("Added secret '{}' for agent '{}'", secret_key, agent_name,);
        }
        SecretsCommands::Delete { key: secret_key } => {
            validate_secret_key(secret_key)?;
            let existed = secrets.remove(secret_key).is_some();
            save_encrypted_secrets(&home, &key, &secrets)?;
            if existed {
                println!("Deleted secret '{}' for agent '{}'", secret_key, agent_name,);
            } else {
                println!(
                    "Secret '{}' was not set for agent '{}'",
                    secret_key, agent_name,
                );
            }
        }
    }

    Ok(())
}
