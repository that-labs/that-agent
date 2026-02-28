//! Code grep — keyword search with .gitignore awareness and token-budget output.
//!
//! Unlike raw grep, `anvil code grep` returns deduplicated, ranked results
//! with structural context. Results are grouped by file, with surrounding
//! context lines and symbol annotations.

use crate::output::{self, BudgetedOutput};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::{WalkBuilder, WalkState};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum GrepError {
    #[error("invalid pattern: {0}")]
    InvalidPattern(String),
    #[error("path not found: {0}")]
    NotFound(String),
}

/// A single grep match with context (legacy flat format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepMatch {
    pub file: String,
    pub line_number: usize,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_before: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_after: Vec<String>,
}

/// A group of matches within a single file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepFileGroup {
    pub file: String,
    pub matches: Vec<GrepLineMatch>,
}

/// A single line match within a file group (no file field — it's on the parent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepLineMatch {
    pub line_number: usize,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_before: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_after: Vec<String>,
}

/// Result of a grep operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepResult {
    /// Schema version for downstream format detection.
    pub schema_version: u8,
    pub pattern: String,
    /// Grouped results by file (preferred format).
    pub file_matches: Vec<GrepFileGroup>,
    /// Number of matches returned (capped by limit).
    pub returned_matches: usize,
    /// True total matches across ALL files searched.
    pub total_matches: usize,
    /// Number of files with at least one match.
    pub matched_files: usize,
    /// Number of files where content was read and checked.
    pub files_searched: usize,
    /// Number of files skipped (binary, unreadable, glob-filtered).
    pub files_skipped: usize,
    /// Whether returned_matches was capped by the limit.
    pub limit_reached: bool,
}

/// Runtime controls for grep execution performance.
#[derive(Debug, Clone, Copy, Default)]
pub struct GrepRuntimeOptions {
    /// Number of walker worker threads. `None` uses an adaptive default.
    pub workers: Option<usize>,
    /// File size threshold for mmap-backed reads.
    pub mmap_min_bytes: Option<usize>,
}

const DEFAULT_MMAP_MIN_BYTES: usize = 256 * 1024;

/// Search for a pattern across files in a directory.
///
/// Respects .gitignore, provides context lines, deduplicates results,
/// and enforces token budget on output.
///
/// When `use_regex` is true, the pattern is compiled as a regular expression.
/// Invalid regex patterns return `GrepError::InvalidPattern`.
#[cfg(test)]
pub fn code_grep(
    root: &Path,
    pattern: &str,
    context_lines: Option<usize>,
    max_tokens: Option<usize>,
    max_results: Option<usize>,
    case_insensitive: bool,
    use_regex: bool,
) -> Result<BudgetedOutput, GrepError> {
    code_grep_filtered(
        root,
        pattern,
        context_lines,
        max_tokens,
        max_results,
        case_insensitive,
        use_regex,
        &[],
        &[],
    )
}

/// Search for a pattern across files with optional include/exclude glob filters.
///
/// `include` globs restrict matches to files whose relative path matches any pattern.
/// `exclude` globs reject files whose relative path matches any pattern.
/// Exclude takes precedence over include.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub fn code_grep_filtered(
    root: &Path,
    pattern: &str,
    context_lines: Option<usize>,
    max_tokens: Option<usize>,
    max_results: Option<usize>,
    case_insensitive: bool,
    use_regex: bool,
    include: &[String],
    exclude: &[String],
) -> Result<BudgetedOutput, GrepError> {
    code_grep_filtered_with_options(
        root,
        pattern,
        context_lines,
        max_tokens,
        max_results,
        case_insensitive,
        use_regex,
        include,
        exclude,
        GrepRuntimeOptions::default(),
    )
}

/// Search for a pattern across files with explicit runtime controls.
#[allow(clippy::too_many_arguments)]
pub fn code_grep_filtered_with_options(
    root: &Path,
    pattern: &str,
    context_lines: Option<usize>,
    max_tokens: Option<usize>,
    max_results: Option<usize>,
    case_insensitive: bool,
    use_regex: bool,
    include: &[String],
    exclude: &[String],
    runtime: GrepRuntimeOptions,
) -> Result<BudgetedOutput, GrepError> {
    if !root.exists() {
        return Err(GrepError::NotFound(root.to_string_lossy().to_string()));
    }

    let include_set = Arc::new(build_glob_set(include)?);
    let exclude_set = Arc::new(build_glob_set(exclude)?);
    let matcher = Arc::new(build_match_engine(pattern, case_insensitive, use_regex)?);

    let ctx = context_lines.unwrap_or(2);
    let limit = max_results.unwrap_or(50);
    let workers = resolved_workers(runtime.workers);
    let mmap_min_bytes = runtime.mmap_min_bytes.unwrap_or(DEFAULT_MMAP_MIN_BYTES);
    let root_buf = root.to_path_buf();

    let files_searched = Arc::new(AtomicUsize::new(0));
    let files_skipped = Arc::new(AtomicUsize::new(0));
    let file_results: Arc<Mutex<Vec<FileScanResult>>> = Arc::new(Mutex::new(Vec::new()));

    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .sort_by_file_name(|a, b| a.cmp(b))
        .threads(workers)
        .build_parallel();

    walker.run(|| {
        let include_set = Arc::clone(&include_set);
        let exclude_set = Arc::clone(&exclude_set);
        let matcher = Arc::clone(&matcher);
        let root = root_buf.clone();
        let files_searched = Arc::clone(&files_searched);
        let files_skipped = Arc::clone(&files_skipped);
        let file_results = Arc::clone(&file_results);

        Box::new(move |entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    files_skipped.fetch_add(1, Ordering::Relaxed);
                    return WalkState::Continue;
                }
            };

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return WalkState::Continue;
            }

            let path = entry.path().to_path_buf();
            if is_likely_binary(&path) {
                files_skipped.fetch_add(1, Ordering::Relaxed);
                return WalkState::Continue;
            }

            let relative_for_glob = relative_for_display(&path, &root);
            if let Some(ref gs) = *exclude_set {
                if gs.is_match(&relative_for_glob) {
                    files_skipped.fetch_add(1, Ordering::Relaxed);
                    return WalkState::Continue;
                }
            }
            if let Some(ref gs) = *include_set {
                if !gs.is_match(&relative_for_glob) {
                    files_skipped.fetch_add(1, Ordering::Relaxed);
                    return WalkState::Continue;
                }
            }

            let content = match read_text_fast(&path, mmap_min_bytes) {
                Ok(content) => content,
                Err(_) => {
                    files_skipped.fetch_add(1, Ordering::Relaxed);
                    return WalkState::Continue;
                }
            };

            files_searched.fetch_add(1, Ordering::Relaxed);
            let scan = scan_file_for_matches(&relative_for_glob, &content, ctx, limit, &matcher);
            if scan.total_matches > 0 {
                if let Ok(mut lock) = file_results.lock() {
                    lock.push(scan);
                }
            }

            WalkState::Continue
        })
    });

    let mut per_file = file_results.lock().map(|v| v.clone()).unwrap_or_default();
    per_file.sort_by(|a, b| a.file.cmp(&b.file));

    let total_matches: usize = per_file.iter().map(|f| f.total_matches).sum();
    let matched_files = per_file.len();

    // Deterministic top-N merge by sorted file order and line order.
    let mut matches = Vec::new();
    for file in &per_file {
        if matches.len() >= limit {
            break;
        }
        let needed = limit - matches.len();
        for m in file.details.iter().take(needed) {
            matches.push(GrepMatch {
                file: file.file.clone(),
                line_number: m.line_number,
                content: m.content.clone(),
                context_before: m.context_before.clone(),
                context_after: m.context_after.clone(),
            });
        }
    }

    let file_matches = build_file_groups(&matches);

    let returned = matches.len();
    let result = GrepResult {
        schema_version: 2,
        pattern: pattern.to_string(),
        file_matches,
        returned_matches: returned,
        total_matches,
        matched_files,
        files_searched: files_searched.load(Ordering::Relaxed),
        files_skipped: files_skipped.load(Ordering::Relaxed),
        limit_reached: returned < total_matches,
    };

    Ok(output::emit_json(&result, max_tokens))
}

/// Build grouped file results from a sorted flat match list, with context dedup.
fn build_file_groups(matches: &[GrepMatch]) -> Vec<GrepFileGroup> {
    let mut groups: Vec<GrepFileGroup> = Vec::new();
    for m in matches {
        if groups.last().is_some_and(|g| g.file == m.file) {
            groups
                .last_mut()
                .expect("checked is_some")
                .matches
                .push(GrepLineMatch {
                    line_number: m.line_number,
                    content: m.content.clone(),
                    context_before: m.context_before.clone(),
                    context_after: m.context_after.clone(),
                });
        } else {
            groups.push(GrepFileGroup {
                file: m.file.clone(),
                matches: vec![GrepLineMatch {
                    line_number: m.line_number,
                    content: m.content.clone(),
                    context_before: m.context_before.clone(),
                    context_after: m.context_after.clone(),
                }],
            });
        }
    }
    for group in &mut groups {
        dedup_context_in_group(&mut group.matches);
    }
    groups
}

/// Deduplicate overlapping context lines between adjacent matches in a file group.
fn dedup_context_in_group(matches: &mut [GrepLineMatch]) {
    let mut covered_through: usize = 0;
    for m in matches.iter_mut() {
        let ctx_start = m.line_number.saturating_sub(m.context_before.len());
        if ctx_start < covered_through {
            let overlap = covered_through - ctx_start;
            let drain_count = overlap.min(m.context_before.len());
            m.context_before = m.context_before.split_off(drain_count);
        }
        covered_through = m.line_number + m.context_after.len();
    }
}

fn build_match_engine(
    pattern: &str,
    case_insensitive: bool,
    use_regex: bool,
) -> Result<MatchEngine, GrepError> {
    if use_regex {
        let re = regex::RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|e| GrepError::InvalidPattern(e.to_string()))?;
        return Ok(MatchEngine::Regex(re));
    }

    if case_insensitive {
        let escaped = regex::escape(pattern);
        let re = regex::RegexBuilder::new(&escaped)
            .case_insensitive(true)
            .build()
            .map_err(|e| GrepError::InvalidPattern(e.to_string()))?;
        return Ok(MatchEngine::CaseInsensitiveLiteral(re));
    }

    Ok(MatchEngine::Literal(pattern.to_string()))
}

/// Build a `GlobSet` from a slice of pattern strings.
/// Returns `None` for empty input, `Err` for invalid patterns.
fn build_glob_set(patterns: &[String]) -> Result<Option<GlobSet>, GrepError> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        let glob = Glob::new(p).map_err(|e| GrepError::InvalidPattern(e.to_string()))?;
        builder.add(glob);
    }
    let set = builder
        .build()
        .map_err(|e| GrepError::InvalidPattern(e.to_string()))?;
    Ok(Some(set))
}

#[derive(Debug, Clone)]
struct FileScanResult {
    file: String,
    total_matches: usize,
    details: Vec<GrepLineMatch>,
}

#[derive(Debug)]
enum MatchEngine {
    Regex(regex::Regex),
    CaseInsensitiveLiteral(regex::Regex),
    Literal(String),
}

fn resolved_workers(configured: Option<usize>) -> usize {
    match configured {
        Some(0) => 1,
        Some(n) => n.max(1),
        None => std::thread::available_parallelism()
            .map(|n| n.get().min(8))
            .unwrap_or(4),
    }
}

fn relative_for_display(path: &Path, root: &Path) -> String {
    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    if rel.is_empty() {
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    } else {
        rel
    }
}

fn scan_file_for_matches(
    relative_path: &str,
    content: &str,
    context_lines: usize,
    limit: usize,
    matcher: &MatchEngine,
) -> FileScanResult {
    let lines: Vec<&str> = content.lines().collect();
    let mut total_matches = 0usize;
    let mut details = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        if !matches_line(line, matcher) {
            continue;
        }

        total_matches += 1;
        if details.len() >= limit {
            continue;
        }

        let context_before: Vec<String> = lines[i.saturating_sub(context_lines)..i]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let context_after: Vec<String> = lines[(i + 1)
            ..i.saturating_add(1)
                .saturating_add(context_lines)
                .min(lines.len())]
            .iter()
            .map(|s| s.to_string())
            .collect();

        details.push(GrepLineMatch {
            line_number: i + 1,
            content: line.to_string(),
            context_before,
            context_after,
        });
    }

    FileScanResult {
        file: relative_path.to_string(),
        total_matches,
        details,
    }
}

fn matches_line(line: &str, matcher: &MatchEngine) -> bool {
    match matcher {
        MatchEngine::Regex(re) | MatchEngine::CaseInsensitiveLiteral(re) => re.is_match(line),
        MatchEngine::Literal(pattern) => line.contains(pattern),
    }
}

fn is_probably_binary_bytes(sample: &[u8]) -> bool {
    if sample.contains(&0) {
        return true;
    }
    if sample.is_empty() {
        return false;
    }

    let control_bytes = sample
        .iter()
        .filter(|&&b| b < 0x09 || (b > 0x0D && b < 0x20))
        .count();
    control_bytes * 100 / sample.len() > 30
}

fn read_text_fast(path: &Path, mmap_min_bytes: usize) -> std::io::Result<String> {
    let metadata = std::fs::metadata(path)?;
    if metadata.len() as usize >= mmap_min_bytes {
        let file = std::fs::File::open(path)?;
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file)? };
        let sample_len = mmap.len().min(8192);
        if is_probably_binary_bytes(&mmap[..sample_len]) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "binary content",
            ));
        }
        let text = std::str::from_utf8(&mmap)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "non-utf8 file"))?;
        return Ok(text.to_string());
    }
    let bytes = std::fs::read(path)?;
    if is_probably_binary_bytes(&bytes[..bytes.len().min(8192)]) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "binary content",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "non-utf8 file"))
}

/// Heuristic to detect binary files by extension.
fn is_likely_binary(path: &Path) -> bool {
    let binary_extensions = [
        "png", "jpg", "jpeg", "gif", "bmp", "ico", "svg", "woff", "woff2", "ttf", "eot", "otf",
        "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "exe", "dll", "so", "dylib", "a", "o", "obj",
        "pdf", "doc", "docx", "xls", "xlsx", "mp3", "mp4", "avi", "mov", "flv", "wmv", "wasm",
        "class", "pyc", "pyo", "db", "sqlite", "sqlite3",
    ];

    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| binary_extensions.contains(&ext))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_grep_dir() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("main.rs"),
            "fn main() {\n    println!(\"hello world\");\n    // TODO: fix this\n}\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("lib.rs"),
            "pub fn helper() {\n    // TODO: refactor\n    let x = 42;\n}\n",
        )
        .unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src").join("utils.rs"),
            "// Utility functions\nfn util_one() {}\nfn util_two() {}\n",
        )
        .unwrap();
        tmp
    }

    #[test]
    fn test_grep_basic() {
        let tmp = setup_grep_dir();
        let result = code_grep(tmp.path(), "TODO", None, None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 2);
        assert!(parsed
            .file_matches
            .iter()
            .all(|g| g.matches.iter().all(|m| m.content.contains("TODO"))));
    }

    #[test]
    fn test_grep_case_insensitive() {
        let tmp = setup_grep_dir();
        let result = code_grep(tmp.path(), "todo", None, None, None, true, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 2);
    }

    #[test]
    fn test_grep_case_sensitive() {
        let tmp = setup_grep_dir();
        let result = code_grep(tmp.path(), "todo", None, None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 0);
    }

    #[test]
    fn test_grep_with_context() {
        let tmp = setup_grep_dir();
        let result = code_grep(tmp.path(), "TODO", Some(1), None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        // Each match should have context lines
        for g in &parsed.file_matches {
            for m in &g.matches {
                assert!(
                    !m.context_before.is_empty() || !m.context_after.is_empty(),
                    "match should have context"
                );
            }
        }
    }

    #[test]
    fn test_grep_respects_gitignore() {
        let tmp = setup_grep_dir();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        fs::write(tmp.path().join(".gitignore"), "*.log\n").unwrap();
        fs::write(tmp.path().join("debug.log"), "TODO: check this log\n").unwrap();

        let result = code_grep(tmp.path(), "TODO", None, None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        // Should not find the match in the .log file
        assert!(parsed.file_matches.iter().all(|g| !g.file.contains(".log")));
    }

    #[test]
    fn test_grep_max_results() {
        let tmp = TempDir::new().unwrap();
        let content: String = (0..100)
            .map(|i| format!("line {} with pattern\n", i))
            .collect();
        fs::write(tmp.path().join("big.txt"), content).unwrap();

        let result = code_grep(tmp.path(), "pattern", None, None, Some(5), false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.returned_matches, 5);
        assert_eq!(parsed.total_matches, 100);
        assert!(parsed.limit_reached);
    }

    #[test]
    fn test_grep_not_found() {
        let result = code_grep(
            Path::new("/nonexistent"),
            "test",
            None,
            None,
            None,
            false,
            false,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_grep_no_matches() {
        let tmp = setup_grep_dir();
        let result = code_grep(tmp.path(), "ZZZZNOTFOUND", None, None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 0);
    }

    #[test]
    fn test_grep_skips_binary_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("image.png"), "TODO in binary").unwrap();
        fs::write(tmp.path().join("code.rs"), "// TODO: fix this").unwrap();

        let result = code_grep(tmp.path(), "TODO", None, None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 1);
        assert!(parsed.file_matches[0].file.contains("code.rs"));
    }

    #[test]
    fn test_grep_skips_extensionless_binary_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("blob"), vec![0, 159, 146, 150, 0, 88]).unwrap();
        fs::write(tmp.path().join("code.rs"), "// TODO: fix this").unwrap();

        let result = code_grep(tmp.path(), "TODO", None, None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 1);
        assert_eq!(parsed.files_searched, 1);
        assert!(parsed.files_skipped >= 1);
        assert!(parsed.file_matches[0].file.contains("code.rs"));
    }

    #[test]
    fn test_grep_token_budget() {
        let tmp = TempDir::new().unwrap();
        let content: String = (0..200)
            .map(|i| format!("fn func_{}() {{ /* match_pattern */ }}\n", i))
            .collect();
        fs::write(tmp.path().join("big.rs"), content).unwrap();

        let result = code_grep(
            tmp.path(),
            "match_pattern",
            None,
            Some(50),
            None,
            false,
            false,
        )
        .unwrap();
        assert!(result.tokens <= 60); // some tolerance
    }

    #[test]
    fn test_grep_regex_mode() {
        let tmp = setup_grep_dir();
        let result = code_grep(tmp.path(), r"fn \w+\(\)", None, None, None, false, true).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert!(
            parsed.total_matches >= 2,
            "regex should match function signatures"
        );
    }

    #[test]
    fn test_grep_regex_case_insensitive() {
        let tmp = setup_grep_dir();
        let result = code_grep(tmp.path(), r"todo", None, None, None, true, true).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 2);
    }

    #[test]
    fn test_grep_regex_invalid_pattern() {
        let tmp = setup_grep_dir();
        let result = code_grep(tmp.path(), r"[invalid", None, None, None, false, true);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, GrepError::InvalidPattern(_)));
    }

    #[test]
    fn test_grep_context_dedup_adjacent() {
        let tmp = TempDir::new().unwrap();
        // Lines 1-5: matches on line 2 and 3 with context=2
        fs::write(
            tmp.path().join("file.rs"),
            "line1\nmatch_a\nmatch_b\nline4\nline5\n",
        )
        .unwrap();

        let result = code_grep(tmp.path(), "match_", Some(2), None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.file_matches.len(), 1);
        let group = &parsed.file_matches[0];
        assert_eq!(group.matches.len(), 2);
        // First match should have full context_before
        let first = &group.matches[0];
        assert_eq!(first.line_number, 2);
        // Second match (line 3) should have trimmed context_before since line 1-2 are covered
        let second = &group.matches[1];
        assert_eq!(second.line_number, 3);
        // Context before for second match should be empty or shorter (line1 and match_a covered)
        assert!(
            second.context_before.len() < 2,
            "context_before should be deduped, got {:?}",
            second.context_before
        );
    }

    #[test]
    fn test_grep_context_dedup_chain() {
        let tmp = TempDir::new().unwrap();
        // Matches on lines 3, 5, 7 with context=2 — cascading dedup
        let content = "line1\nline2\nmatch3\nline4\nmatch5\nline6\nmatch7\nline8\nline9\n";
        fs::write(tmp.path().join("file.rs"), content).unwrap();

        let result = code_grep(tmp.path(), "match", Some(2), None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.file_matches.len(), 1);
        let group = &parsed.file_matches[0];
        assert_eq!(group.matches.len(), 3);
        // First match keeps full context
        assert!(!group.matches[0].context_before.is_empty());
        // Second and third matches should have reduced context_before
        let total_ctx_before: usize = group.matches.iter().map(|m| m.context_before.len()).sum();
        // Without dedup, total would be 2+2+2=6. With dedup it should be less.
        assert!(
            total_ctx_before < 6,
            "cascading dedup should reduce total context_before, got {}",
            total_ctx_before
        );
    }

    #[test]
    fn test_grep_context_dedup_no_overlap() {
        let tmp = TempDir::new().unwrap();
        // Matches on lines 2 and 8, far apart — no dedup needed
        let content = "line1\nmatch_a\nline3\nline4\nline5\nline6\nline7\nmatch_b\nline9\nline10\n";
        fs::write(tmp.path().join("file.rs"), content).unwrap();

        let result = code_grep(tmp.path(), "match_", Some(2), None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.file_matches.len(), 1);
        let group = &parsed.file_matches[0];
        assert_eq!(group.matches.len(), 2);
        // Both matches should keep their full context_before
        assert_eq!(group.matches[0].context_before.len(), 1); // only line1 before line 2
        assert_eq!(group.matches[1].context_before.len(), 2); // line6, line7 before line 8
    }

    #[test]
    fn test_grep_include_glob() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("code.rs"), "fn hello() {}\n").unwrap();
        fs::write(tmp.path().join("code.py"), "def hello():\n    pass\n").unwrap();
        fs::write(tmp.path().join("data.txt"), "hello world\n").unwrap();

        let result = code_grep_filtered(
            tmp.path(),
            "hello",
            None,
            None,
            None,
            false,
            false,
            &["*.rs".to_string()],
            &[],
        )
        .unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 1);
        assert!(parsed.file_matches[0].file.ends_with(".rs"));
    }

    #[test]
    fn test_grep_exclude_glob() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src").join("main.rs"), "fn hello() {}\n").unwrap();
        fs::create_dir(tmp.path().join("tests")).unwrap();
        fs::write(
            tmp.path().join("tests").join("test_main.rs"),
            "fn hello_test() {}\n",
        )
        .unwrap();

        let result = code_grep_filtered(
            tmp.path(),
            "hello",
            None,
            None,
            None,
            false,
            false,
            &[],
            &["*test*".to_string()],
        )
        .unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 1);
        assert!(parsed.file_matches[0].file.contains("main.rs"));
        assert!(!parsed.file_matches[0].file.contains("test"));
    }

    #[test]
    fn test_grep_include_nested_glob() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src").join("lib.rs"), "fn nested() {}\n").unwrap();
        fs::write(tmp.path().join("top.rs"), "fn top() {}\n").unwrap();

        let result = code_grep_filtered(
            tmp.path(),
            "fn",
            None,
            None,
            None,
            false,
            false,
            &["src/**/*.rs".to_string()],
            &[],
        )
        .unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 1);
        assert!(parsed.file_matches[0].file.contains("src"));
    }

    #[test]
    fn test_grep_exclude_wins_over_include() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test_code.rs"), "fn test_fn() {}\n").unwrap();
        fs::write(tmp.path().join("main.rs"), "fn main() {}\n").unwrap();

        // Include all .rs files but exclude anything with "test"
        let result = code_grep_filtered(
            tmp.path(),
            "fn",
            None,
            None,
            None,
            false,
            false,
            &["*.rs".to_string()],
            &["*test*".to_string()],
        )
        .unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 1);
        assert!(parsed.file_matches[0].file.contains("main.rs"));
    }

    #[test]
    fn test_grep_include_single_file_root() {
        // When root is a single file, --include should still work
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("main.rs");
        fs::write(&file, "fn main() {}\nfn helper() {}\n").unwrap();

        // Include *.rs should match when grepping a single file
        let result = code_grep_filtered(
            &file,
            "fn",
            None,
            None,
            None,
            false,
            false,
            &["*.rs".to_string()],
            &[],
        )
        .unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(
            parsed.total_matches, 2,
            "include *.rs should match a single .rs file"
        );
        assert_eq!(parsed.files_skipped, 0);

        // Include *.py should NOT match a .rs file
        let result = code_grep_filtered(
            &file,
            "fn",
            None,
            None,
            None,
            false,
            false,
            &["*.py".to_string()],
            &[],
        )
        .unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(
            parsed.total_matches, 0,
            "include *.py should not match a .rs file"
        );
    }

    #[test]
    fn test_grep_invalid_glob() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("file.rs"), "hello\n").unwrap();

        let result = code_grep_filtered(
            tmp.path(),
            "hello",
            None,
            None,
            None,
            false,
            false,
            &["[invalid".to_string()],
            &[],
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, GrepError::InvalidPattern(_)));
    }

    #[test]
    fn test_grep_total_matches_counts_all() {
        let tmp = TempDir::new().unwrap();
        let content: String = (0..50).map(|i| format!("line {} match_me\n", i)).collect();
        fs::write(tmp.path().join("data.txt"), content).unwrap();

        let result = code_grep(tmp.path(), "match_me", None, None, Some(10), false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.returned_matches, 10);
        assert_eq!(parsed.total_matches, 50);
        assert!(parsed.limit_reached);
        assert_eq!(parsed.matched_files, 1);
    }

    #[test]
    fn test_grep_limit_reached_flag() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "match\nmatch\nmatch\n").unwrap();

        // With limit larger than matches — flag should be false
        let result = code_grep(tmp.path(), "match", None, None, Some(10), false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert!(!parsed.limit_reached);
        assert_eq!(parsed.returned_matches, parsed.total_matches);

        // With limit smaller than matches — flag should be true
        let result = code_grep(tmp.path(), "match", None, None, Some(1), false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.limit_reached);
        assert_eq!(parsed.returned_matches, 1);
        assert_eq!(parsed.total_matches, 3);
    }

    #[test]
    fn test_grep_returned_vs_total() {
        let tmp = TempDir::new().unwrap();
        // Create multiple files each with matches
        for i in 0..5 {
            let content: String = (0..10)
                .map(|j| format!("line {} needle {}\n", i, j))
                .collect();
            fs::write(tmp.path().join(format!("file_{}.txt", i)), content).unwrap();
        }

        let result = code_grep(tmp.path(), "needle", None, None, Some(7), false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.returned_matches, 7);
        assert_eq!(parsed.total_matches, 50);
        assert_eq!(parsed.matched_files, 5);
        assert_eq!(parsed.files_searched, 5);
        assert!(parsed.limit_reached);
    }

    #[test]
    fn test_grep_files_skipped_counts() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("code.rs"), "fn hello() {}\n").unwrap();
        fs::write(tmp.path().join("image.png"), "fake binary content").unwrap();

        let result = code_grep(tmp.path(), "hello", None, None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.files_searched, 1);
        assert!(
            parsed.files_skipped >= 1,
            "should count binary files as skipped"
        );
    }

    #[test]
    fn test_grep_deterministic_order() {
        let tmp = setup_grep_dir();
        // Run grep twice with same parameters
        let result1 = code_grep(tmp.path(), "fn", None, None, None, false, false).unwrap();
        let parsed1: GrepResult = serde_json::from_str(&result1.content).unwrap();
        let result2 = code_grep(tmp.path(), "fn", None, None, None, false, false).unwrap();
        let parsed2: GrepResult = serde_json::from_str(&result2.content).unwrap();
        // Results should be identical
        assert_eq!(parsed1.file_matches.len(), parsed2.file_matches.len());
        for (ga, gb) in parsed1.file_matches.iter().zip(parsed2.file_matches.iter()) {
            assert_eq!(ga.file, gb.file);
            assert_eq!(ga.matches.len(), gb.matches.len());
            for (a, b) in ga.matches.iter().zip(gb.matches.iter()) {
                assert_eq!(a.line_number, b.line_number);
                assert_eq!(a.content, b.content);
            }
        }
        // Verify groups are sorted by file
        for w in parsed1.file_matches.windows(2) {
            assert!(
                w[0].file <= w[1].file,
                "file groups should be sorted by file name"
            );
        }
    }

    #[test]
    fn test_grep_single_file_has_filename() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("only.rs"), "fn test() {}\n").unwrap();
        // Search the file itself (root == file's parent, so relative path is just the filename)
        let result = code_grep(tmp.path(), "fn", None, None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.file_matches.len(), 1);
        assert!(
            !parsed.file_matches[0].file.is_empty(),
            "file field should not be empty"
        );
        assert!(
            parsed.file_matches[0].file.contains("only.rs"),
            "file field should contain the filename"
        );
    }

    #[test]
    fn test_is_likely_binary() {
        assert!(is_likely_binary(Path::new("image.png")));
        assert!(is_likely_binary(Path::new("archive.zip")));
        assert!(is_likely_binary(Path::new("lib.so")));
        assert!(!is_likely_binary(Path::new("main.rs")));
        assert!(!is_likely_binary(Path::new("config.toml")));
    }

    #[test]
    fn test_grep_empty_directory() {
        let tmp = TempDir::new().unwrap();
        // No files at all
        let result = code_grep(tmp.path(), "anything", None, None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 0);
        assert_eq!(parsed.returned_matches, 0);
        assert_eq!(parsed.matched_files, 0);
        assert_eq!(parsed.files_searched, 0);
        assert!(!parsed.limit_reached);
    }

    #[test]
    fn test_grep_zero_context_lines() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("test.rs"),
            "line1\nmatch_me\nline3\nmatch_me\nline5\n",
        )
        .unwrap();
        let result = code_grep(tmp.path(), "match_me", Some(0), None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 2);
        // With zero context, context_before and context_after should be empty
        for fm in &parsed.file_matches {
            for m in &fm.matches {
                assert!(
                    m.context_before.is_empty(),
                    "context_before should be empty with context=0"
                );
                assert!(
                    m.context_after.is_empty(),
                    "context_after should be empty with context=0"
                );
            }
        }
    }

    #[test]
    fn test_grep_multiple_include_patterns() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("main.rs"), "fn hello() {}\n").unwrap();
        fs::write(tmp.path().join("lib.py"), "def hello(): pass\n").unwrap();
        fs::write(tmp.path().join("app.ts"), "function hello() {}\n").unwrap();
        fs::write(tmp.path().join("data.txt"), "hello world\n").unwrap();

        // Include both *.rs and *.py — should match both but not .ts or .txt
        let result = code_grep_filtered(
            tmp.path(),
            "hello",
            None,
            None,
            None,
            false,
            false,
            &["*.rs".to_string(), "*.py".to_string()],
            &[],
        )
        .unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.matched_files, 2);
        let files: Vec<&str> = parsed
            .file_matches
            .iter()
            .map(|f| f.file.as_str())
            .collect();
        assert!(files.contains(&"main.rs"));
        assert!(files.contains(&"lib.py"));
    }

    #[test]
    fn test_grep_unicode_pattern() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("unicode.txt"),
            "Hello 世界\nBonjour monde\nHello 世界 again\n",
        )
        .unwrap();
        let result = code_grep(tmp.path(), "世界", None, None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 2);
    }

    #[test]
    fn test_grep_literal_regex_metacharacters() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("code.rs"),
            "let arr = vec![1, 2, 3];\nlet x = arr[0];\n",
        )
        .unwrap();
        // Literal search for "[1, 2, 3]" — should NOT be interpreted as regex
        let result = code_grep(
            tmp.path(),
            "[1, 2, 3]",
            None,
            None,
            None,
            false,
            false, // regex = false
        )
        .unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 1);
        assert!(parsed.file_matches[0].matches[0]
            .content
            .contains("[1, 2, 3]"));
    }

    #[test]
    fn test_grep_single_file_no_matches() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("empty.rs"), "fn main() {}\n").unwrap();
        let file_path = tmp.path().join("empty.rs");
        let result = code_grep(&file_path, "NONEXISTENT", None, None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 0);
        assert_eq!(parsed.matched_files, 0);
        assert!(parsed.file_matches.is_empty());
    }

    #[test]
    fn test_grep_file_matches_and_flat_matches_consistent() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        fs::write(tmp.path().join("b.rs"), "fn three() {}\n").unwrap();
        let result = code_grep(tmp.path(), "fn ", None, None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();

        // returned_matches should equal sum of matches across all file groups
        let grouped_count: usize = parsed.file_matches.iter().map(|g| g.matches.len()).sum();
        assert_eq!(parsed.returned_matches, grouped_count);
    }

    #[test]
    fn test_grep_schema_version() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.rs"), "fn hello() {}\n").unwrap();
        let result = code_grep(tmp.path(), "fn", None, None, None, false, false).unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.schema_version, 2);
    }

    #[test]
    fn test_grep_include_exclude_combined() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("main.rs"), "fn hello() {}\n").unwrap();
        fs::write(tmp.path().join("main_test.rs"), "fn hello() {}\n").unwrap();
        fs::write(tmp.path().join("lib.rs"), "fn hello() {}\n").unwrap();
        fs::write(tmp.path().join("lib_test.rs"), "fn hello() {}\n").unwrap();

        // Include *.rs but exclude *test*
        let result = code_grep_filtered(
            tmp.path(),
            "fn hello",
            None,
            None,
            None,
            false,
            false,
            &["*.rs".to_string()],
            &["*test*".to_string()],
        )
        .unwrap();
        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.matched_files, 2);
        let files: Vec<&str> = parsed
            .file_matches
            .iter()
            .map(|f| f.file.as_str())
            .collect();
        assert!(files.contains(&"main.rs"));
        assert!(files.contains(&"lib.rs"));
        assert!(!files.iter().any(|f| f.contains("test")));
    }

    #[test]
    fn test_grep_with_runtime_options() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("main.rs"), "pub fn a() {}\npub fn b() {}\n").unwrap();

        let result = code_grep_filtered_with_options(
            tmp.path(),
            "pub fn",
            None,
            None,
            Some(1),
            false,
            false,
            &[],
            &[],
            GrepRuntimeOptions {
                workers: Some(0),        // coerced to 1
                mmap_min_bytes: Some(1), // force mmap path for tiny files
            },
        )
        .unwrap();

        let parsed: GrepResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 2);
        assert_eq!(parsed.returned_matches, 1);
        assert!(parsed.limit_reached);
    }
}
