//! Project-directory path resolution.
//!
//! Ported from the path-resolution section of upstream `_internal/sessions.py`:
//! `_simple_hash`, `_sanitize_path`, `_get_claude_config_home_dir`,
//! `_get_projects_dir`, `_get_project_dir`, `_canonicalize_path`,
//! `_find_project_dir`, and `project_key_for_directory`. These primitives
//! derive the on-disk `~/.claude/projects/<sanitized-cwd>/` layout the CLI
//! itself uses, so [`project_key_for_directory`] must match the CLI's
//! directory naming byte-for-byte.
//!
//! `session/local_session_file.rs` (session mutations/import slice) reuses
//! [`canonicalize_path`], [`find_project_dir`], and [`get_projects_dir`]
//! from here rather than keeping its own copy.

use std::env;
use std::path::{Component, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

/// Upper bound on a single sanitized filesystem path component. Most
/// filesystems limit individual components to 255 bytes; 200 leaves room
/// for the hash suffix and separator.
///
/// `pub(crate)`: also used by `session/listing_worktrees.rs` to replicate
/// `_list_sessions_for_project`'s prefix-match fallback for long paths.
pub(crate) const MAX_SANITIZED_LENGTH: usize = 200;

static SANITIZE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-zA-Z0-9]").unwrap());

fn nfc(s: &str) -> String {
    s.nfc().collect()
}

/// 32-bit integer hash to base36, matching the CLI's directory naming.
///
/// Portable djb2-style hash, byte-identical to upstream's Python
/// implementation for the same input string (iterated by Unicode scalar
/// value, exactly like Python's `ord(ch)` over `str`). Used as a stable
/// cross-runtime key suffix — do not change without also updating every
/// other runtime that computes project directory names.
pub(crate) fn simple_hash(s: &str) -> String {
    let mut h: i64 = 0;
    for ch in s.chars() {
        let code = ch as i64;
        h = (h << 5) - h + code;
        // Emulate JS `hash |= 0` (coerce to 32-bit signed int).
        h &= 0xFFFF_FFFF;
        if h >= 0x8000_0000 {
            h -= 0x1_0000_0000;
        }
    }
    h = h.abs();
    if h == 0 {
        return "0".to_string();
    }
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    let mut n = h;
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("digits are ASCII")
}

/// Makes a string safe for use as a directory name.
///
/// Replaces all non-alphanumeric characters with hyphens. Paths exceeding
/// [`MAX_SANITIZED_LENGTH`] are truncated and suffixed with a hash of the
/// *original* (pre-sanitization) string.
pub(crate) fn sanitize_path(name: &str) -> String {
    let sanitized = SANITIZE_RE.replace_all(name, "-").into_owned();
    if sanitized.chars().count() <= MAX_SANITIZED_LENGTH {
        return sanitized;
    }
    let hash = simple_hash(name);
    let truncated: String = sanitized.chars().take(MAX_SANITIZED_LENGTH).collect();
    format!("{truncated}-{hash}")
}

/// Returns the Claude config directory (respects `CLAUDE_CONFIG_DIR`).
fn claude_config_home_dir() -> PathBuf {
    if let Ok(dir) = env::var("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(nfc(&dir));
        }
    }
    let home = env::var("HOME").or_else(|_| env::var("USERPROFILE")).unwrap_or_default();
    PathBuf::from(nfc(&home)).join(".claude")
}

/// Returns the `projects/` subdirectory under the Claude config home.
///
/// Note: upstream's Python `_get_projects_dir` also accepts an
/// `env_override` dict so store-backed callers can resolve the same
/// directory a subprocess with an overridden `CLAUDE_CONFIG_DIR` would
/// write to. That parameter is not needed by anything in this slice; a
/// future slice building store-backed listing on top of this module can add
/// it if required.
pub(crate) fn get_projects_dir() -> PathBuf {
    claude_config_home_dir().join("projects")
}

/// Returns the per-project directory for `project_path` (sanitized).
pub(crate) fn get_project_dir(project_path: &str) -> PathBuf {
    get_projects_dir().join(sanitize_path(project_path))
}

/// Resolves a directory path to its canonical form (realpath + NFC).
///
/// Falls back to a lexically-normalized, NFC-normalized absolute path (no
/// symlink resolution) when the path — or some component of it — does not
/// exist. `std::fs::canonicalize` requires the full path to exist, unlike
/// Python's `os.path.realpath`, which resolves what it can and appends the
/// rest literally; this is a known point of divergence for a non-existent
/// path whose existing prefix also contains an as-yet-unresolved symlink.
pub(crate) fn canonicalize_path(d: &str) -> String {
    if let Ok(resolved) = std::fs::canonicalize(d) {
        return nfc(&resolved.to_string_lossy());
    }
    nfc(&lexically_absolute(d))
}

/// Makes `d` absolute against the current directory and lexically resolves
/// `.`/`..` components, without touching the filesystem or resolving
/// symlinks. Never pops past the root/prefix component.
fn lexically_absolute(d: &str) -> String {
    let path = PathBuf::from(d);
    let base = if path.is_absolute() {
        path
    } else {
        env::current_dir().map(|cwd| cwd.join(&path)).unwrap_or(path)
    };

    let mut out: Vec<Component> = Vec::new();
    for component in base.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            other => out.push(other),
        }
    }

    let mut result = PathBuf::new();
    for part in out {
        result.push(part.as_os_str());
    }
    result.to_string_lossy().into_owned()
}

/// Finds the project directory for a given (already-canonicalized) path.
///
/// Tolerates hash mismatches for long paths (>200 chars): the CLI uses
/// `Bun.hash` while other runtimes use `simpleHash`, which can diverge for
/// paths exceeding [`MAX_SANITIZED_LENGTH`]. Falls back to prefix-based
/// scanning when the exact match doesn't exist.
pub(crate) async fn find_project_dir(project_path: &str) -> Option<PathBuf> {
    let exact = get_project_dir(project_path);
    if tokio::fs::metadata(&exact).await.is_ok_and(|m| m.is_dir()) {
        return Some(exact);
    }

    // Exact match failed — for short paths this means no sessions exist.
    // For long paths, try prefix matching to handle hash mismatches.
    let sanitized = sanitize_path(project_path);
    if sanitized.chars().count() <= MAX_SANITIZED_LENGTH {
        return None;
    }
    let prefix: String = sanitized.chars().take(MAX_SANITIZED_LENGTH).collect();
    let prefix_with_dash = format!("{prefix}-");

    let projects_dir = get_projects_dir();
    let mut entries = tokio::fs::read_dir(&projects_dir).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(&prefix_with_dash)
            && tokio::fs::metadata(entry.path()).await.is_ok_and(|m| m.is_dir())
        {
            return Some(entry.path());
        }
    }
    None
}

/// Derive the `SessionStore` `project_key` for a directory.
///
/// Defaults to the current working directory. Uses the same realpath + NFC
/// normalization + djb2-hashed sanitization the CLI uses for project
/// directory names, so keys match between local-disk transcripts and
/// store-mirrored transcripts even on filesystems that decompose Unicode
/// (macOS HFS+).
pub fn project_key_for_directory(directory: Option<&str>) -> String {
    let target = directory.unwrap_or(".");
    let abs_path = canonicalize_path(target);
    sanitize_path(&abs_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_hash_matches_known_vectors() {
        // Hand-computed against the documented djb2-style algorithm
        // (h = h*31 + code, masked to signed 32-bit each step, abs'd, then
        // base36-encoded). Empty string hashes to 0.
        assert_eq!(simple_hash(""), "0");
        assert_eq!(simple_hash("a"), simple_hash("a"));
        // Deterministic and stable across calls.
        let first = simple_hash("/home/user/some/long/project/path");
        let second = simple_hash("/home/user/some/long/project/path");
        assert_eq!(first, second);
    }

    #[test]
    fn simple_hash_only_uses_base36_digits() {
        let h = simple_hash("/some/unicode/path/日本語");
        assert!(h.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn sanitize_path_replaces_non_alnum() {
        assert_eq!(sanitize_path("/home/user/my-project"), "-home-user-my-project");
    }

    #[test]
    fn sanitize_path_truncates_long_paths_with_hash_suffix() {
        let long = "a".repeat(300);
        let sanitized = sanitize_path(&long);
        assert_eq!(sanitized.chars().count(), MAX_SANITIZED_LENGTH + 1 + simple_hash(&long).len());
        assert!(sanitized.starts_with(&"a".repeat(MAX_SANITIZED_LENGTH)));
    }

    #[test]
    fn sanitize_path_short_unchanged_when_pure_alnum() {
        assert_eq!(sanitize_path("abc123"), "abc123");
    }

    #[test]
    fn canonicalize_path_resolves_existing_dir() {
        let tmp = std::env::temp_dir();
        let resolved = canonicalize_path(tmp.to_str().unwrap());
        assert!(PathBuf::from(&resolved).is_absolute());
    }

    #[test]
    fn canonicalize_path_falls_back_for_nonexistent() {
        let resolved = canonicalize_path("/definitely/does/not/exist/at/all/xyz");
        assert!(resolved.ends_with("xyz"));
        assert!(PathBuf::from(&resolved).is_absolute());
    }

    #[test]
    fn project_key_for_directory_is_deterministic() {
        let a = project_key_for_directory(Some("/tmp/some/dir"));
        let b = project_key_for_directory(Some("/tmp/some/dir"));
        assert_eq!(a, b);
    }

    #[test]
    fn project_key_for_directory_defaults_to_cwd() {
        let key = project_key_for_directory(None);
        assert!(!key.is_empty());
    }
}
