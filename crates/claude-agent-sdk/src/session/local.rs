//! Session-file lite reads and git worktree discovery.
//!
//! Ported from `_validate_uuid`, `_LiteSessionFile`, `_read_session_lite`,
//! and `_get_worktree_paths` in upstream `_internal/sessions.py`.
//!
//! "Lite" reads are stat + head/tail byte reads — never a full JSONL parse
//! — so callers scanning a whole project directory for `list_sessions()`
//! don't pay the cost of parsing every line of every transcript.

use std::path::Path;
use std::sync::LazyLock;
use std::time::UNIX_EPOCH;

use regex::Regex;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use unicode_normalization::UnicodeNormalization;

/// Size of the head/tail buffer for lite metadata reads.
pub(crate) const LITE_READ_BUF_SIZE: usize = 65536;

static UUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").unwrap()
});

/// Returns `maybe_uuid` unchanged if it is a valid UUID string, else
/// `None`.
///
/// Matches upstream exactly: the match is case-insensitive, but the
/// returned string is the original input as-is (NOT lowercased), even
/// though it's easy to misread the Python docstring as implying
/// normalization.
pub(crate) fn validate_uuid(maybe_uuid: &str) -> Option<&str> {
    if UUID_RE.is_match(maybe_uuid) {
        Some(maybe_uuid)
    } else {
        None
    }
}

/// Result of reading a session file's head, tail, mtime and size.
#[derive(Debug, Clone)]
pub(crate) struct LiteSessionFile {
    /// Last-modified time in Unix epoch milliseconds.
    pub mtime: i64,
    /// File size in bytes.
    pub size: u64,
    /// First [`LITE_READ_BUF_SIZE`] bytes, UTF-8 decoded (lossy).
    pub head: String,
    /// Last [`LITE_READ_BUF_SIZE`] bytes, UTF-8 decoded (lossy). Equal to
    /// `head` when the whole file fits in one buffer.
    pub tail: String,
}

/// Opens a session file, stats it, and reads its head + tail.
///
/// Returns `None` on any I/O error or if the file is empty — mirrors
/// upstream, which treats both as "nothing to read" rather than surfacing
/// an error to callers scanning a whole project directory.
pub(crate) async fn read_session_lite(file_path: &Path) -> Option<LiteSessionFile> {
    let mut file = tokio::fs::File::open(file_path).await.ok()?;
    let metadata = file.metadata().await.ok()?;
    let size = metadata.len();
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let head_bytes = read_up_to(&mut file, LITE_READ_BUF_SIZE).await.ok()?;
    if head_bytes.is_empty() {
        return None;
    }
    let head = String::from_utf8_lossy(&head_bytes).into_owned();

    let tail_offset = size.saturating_sub(LITE_READ_BUF_SIZE as u64);
    let tail = if tail_offset == 0 {
        head.clone()
    } else {
        file.seek(std::io::SeekFrom::Start(tail_offset)).await.ok()?;
        let tail_bytes = read_up_to(&mut file, LITE_READ_BUF_SIZE).await.ok()?;
        String::from_utf8_lossy(&tail_bytes).into_owned()
    };

    Some(LiteSessionFile { mtime, size, head, tail })
}

/// Reads up to `n` bytes from `file`, stopping early at EOF.
async fn read_up_to(file: &mut tokio::fs::File, n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    let mut total = 0usize;
    loop {
        let read = file.read(&mut buf[total..]).await?;
        if read == 0 {
            break;
        }
        total += read;
        if total == n {
            break;
        }
    }
    buf.truncate(total);
    Ok(buf)
}

/// Returns absolute worktree paths for the git repo containing `cwd`.
///
/// Returns an empty list if `git` is unavailable, times out (5s, matching
/// upstream), or `cwd` is not inside a repository.
pub(crate) async fn get_worktree_paths(cwd: &str) -> Vec<String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(cwd)
            .output(),
    )
    .await;

    let Ok(Ok(output)) = output else {
        return Vec::new();
    };
    if !output.status.success() || output.stdout.is_empty() {
        return Vec::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .split('\n')
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(|p| p.nfc().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_uuid_accepts_and_preserves_case() {
        assert_eq!(
            validate_uuid("550e8400-e29b-41d4-a716-446655440000"),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(
            validate_uuid("550E8400-E29B-41D4-A716-446655440000"),
            Some("550E8400-E29B-41D4-A716-446655440000")
        );
    }

    #[test]
    fn validate_uuid_rejects_invalid() {
        assert_eq!(validate_uuid("not-a-uuid"), None);
        assert_eq!(validate_uuid("550e8400-e29b-41d4-a716-44665544000"), None);
    }

    #[tokio::test]
    async fn read_session_lite_returns_none_for_missing_file() {
        let result = read_session_lite(Path::new("/definitely/does/not/exist.jsonl")).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn read_session_lite_reads_small_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        tokio::fs::write(&path, b"{\"a\":1}\n").await.unwrap();
        let lite = read_session_lite(&path).await.unwrap();
        assert_eq!(lite.head, "{\"a\":1}\n");
        assert_eq!(lite.tail, lite.head);
        assert_eq!(lite.size, 8);
    }

    #[tokio::test]
    async fn read_session_lite_splits_head_and_tail_for_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut content = vec![b'a'; LITE_READ_BUF_SIZE + 100];
        content[..5].copy_from_slice(b"HEAD!");
        let tail_start = content.len() - 5;
        content[tail_start..].copy_from_slice(b"TAIL!");
        tokio::fs::write(&path, &content).await.unwrap();

        let lite = read_session_lite(&path).await.unwrap();
        assert!(lite.head.starts_with("HEAD!"));
        assert!(lite.tail.ends_with("TAIL!"));
        assert_eq!(lite.size, (LITE_READ_BUF_SIZE + 100) as u64);
    }

    #[tokio::test]
    async fn read_session_lite_none_for_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        tokio::fs::write(&path, b"").await.unwrap();
        assert!(read_session_lite(&path).await.is_none());
    }

    #[tokio::test]
    async fn get_worktree_paths_empty_outside_repo() {
        let dir = tempfile::tempdir().unwrap();
        let paths = get_worktree_paths(dir.path().to_str().unwrap()).await;
        assert!(paths.is_empty());
    }
}
