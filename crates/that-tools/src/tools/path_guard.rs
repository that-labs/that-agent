//! Workspace-boundary path guard.
//!
//! Resolves paths canonically and rejects any that escape the workspace root.

use std::io;
use std::path::{Component, Path, PathBuf};

/// Resolves `target` canonically and verifies it stays within `workspace_root`.
/// Returns the canonical path on success.
/// Returns an error if the path escapes the workspace or contains traversal.
pub fn safe_path(workspace_root: &Path, target: &Path) -> io::Result<PathBuf> {
    let root = std::fs::canonicalize(workspace_root)?;
    let canonical = std::fs::canonicalize(target)?;
    if !canonical.starts_with(&root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "path '{}' escapes workspace root '{}'",
                target.display(),
                root.display()
            ),
        ));
    }
    Ok(canonical)
}

/// Lightweight fallback: rejects any path containing `..` components.
/// Use when the workspace root is unavailable or the target may not exist yet.
pub fn reject_traversal(target: &Path) -> io::Result<()> {
    if target.components().any(|c| c == Component::ParentDir) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("path '{}' contains disallowed '..' traversal", target.display()),
        ));
    }
    Ok(())
}

/// Returns the workspace root from `THAT_WORKSPACE_ROOT` env var, if set.
/// Returns `None` when not running inside a sandbox.
pub fn workspace_root() -> Option<PathBuf> {
    let root = std::env::var("THAT_WORKSPACE_ROOT").ok()?;
    let p = PathBuf::from(root);
    if p.is_dir() { Some(p) } else { None }
}

/// Convenience: validate target against the workspace boundary.
///
/// When `THAT_WORKSPACE_ROOT` is set (sandbox), canonicalizes and enforces
/// that the path stays inside the root. When unset (host), applies the
/// lightweight `reject_traversal` check only.
///
/// Returns the canonical path on success.
pub fn guard(target: &Path) -> io::Result<PathBuf> {
    if let Some(root) = workspace_root() {
        safe_path(&root, target)
    } else {
        reject_traversal(target)?;
        // On host (no sandbox root), just canonicalize without boundary check
        std::fs::canonicalize(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn allows_path_inside_workspace() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("hello.txt");
        std::fs::write(&file, "ok").unwrap();
        let result = safe_path(tmp.path(), &file);
        assert!(result.is_ok());
    }

    #[test]
    fn rejects_path_outside_workspace() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("secret.txt");
        std::fs::write(&file, "secret").unwrap();
        let result = safe_path(tmp.path(), &file);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn rejects_dotdot_traversal() {
        let p = Path::new("/workspace/../etc/passwd");
        let result = reject_traversal(p);
        assert!(result.is_err());
    }

    #[test]
    fn allows_clean_path() {
        let p = Path::new("/workspace/src/main.rs");
        assert!(reject_traversal(p).is_ok());
    }

    #[test]
    fn rejects_symlink_escape() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target_file = outside.path().join("secret.txt");
        std::fs::write(&target_file, "secret").unwrap();

        let link = tmp.path().join("escape");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target_file, &link).unwrap();
        #[cfg(not(unix))]
        return; // symlink test only meaningful on unix

        let result = safe_path(tmp.path(), &link);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }
}
