//! Local (on-disk) session listing: `list_sessions()`.
//!
//! Ported from `_read_sessions_from_dir`, `_deduplicate_by_session_id`,
//! `_apply_sort_limit_offset`, `_list_all_sessions`, and `list_sessions` in
//! upstream `_internal/sessions.py`. The worktree-aware single-project scan
//! (`_list_sessions_for_project`) is large enough on its own to live in
//! [`super::listing_worktrees`].

use std::path::Path;

use indexmap::IndexMap;

use super::info::SDKSessionInfo;
use super::lite_info::parse_session_info_from_lite;
use super::local::{read_session_lite, validate_uuid};
use super::project_dir::get_projects_dir;

/// Reads session files from a single project directory.
///
/// Each file gets a stat + head/tail read. Filters out sidechain sessions
/// and metadata-only sessions (no title/summary/prompt).
pub(crate) async fn read_sessions_from_dir(project_dir: &Path, project_path: Option<&str>) -> Vec<SDKSessionInfo> {
    let mut entries = match tokio::fs::read_dir(project_dir).await {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut results = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(stem) = name.strip_suffix(".jsonl") else { continue };
        let Some(session_id) = validate_uuid(stem) else { continue };
        let session_id = session_id.to_string();

        let Some(lite) = read_session_lite(&entry.path()).await else { continue };
        if let Some(info) = parse_session_info_from_lite(&session_id, &lite, project_path) {
            results.push(info);
        }
    }
    results
}

/// Deduplicates by `session_id`, keeping the entry with the newest
/// `last_modified` (ties keep the first one seen).
///
/// Uses [`IndexMap`] rather than `std::collections::HashMap`: upstream's
/// Python `dict`-based dedup preserves each key's *first-insertion*
/// position even when its value is later overwritten, and that
/// insertion-order output feeds a stable sort in `apply_sort_limit_offset`
/// — so sessions tied on `last_modified` keep upstream's original scan
/// order. `HashMap`'s iteration order has no such guarantee (it is
/// SipHash-seeded per process), which would make tie order vary from run to
/// run and could shift which session lands on which page at a pagination
/// boundary.
pub(crate) fn deduplicate_by_session_id(sessions: Vec<SDKSessionInfo>) -> Vec<SDKSessionInfo> {
    let mut by_id: IndexMap<String, SDKSessionInfo> = IndexMap::new();
    for s in sessions {
        let replace = match by_id.get(&s.session_id) {
            None => true,
            Some(existing) => s.last_modified > existing.last_modified,
        };
        if replace {
            by_id.insert(s.session_id.clone(), s);
        }
    }
    by_id.into_values().collect()
}

/// Sorts sessions by `last_modified` descending and applies offset + limit.
///
/// `limit=Some(0)` means "no limit" (matches upstream's
/// `if limit is not None and limit > 0`); `offset` is `usize` so there is no
/// negative case to guard against, unlike upstream's `int`.
pub(crate) fn apply_sort_limit_offset(
    mut sessions: Vec<SDKSessionInfo>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SDKSessionInfo> {
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_modified));
    let mut sessions = if offset > 0 {
        let start = offset.min(sessions.len());
        sessions.split_off(start)
    } else {
        sessions
    };
    if let Some(l) = limit {
        if l > 0 {
            sessions.truncate(l);
        }
    }
    sessions
}

/// Lists sessions across all project directories.
pub(crate) async fn list_all_sessions(limit: Option<usize>, offset: usize) -> Vec<SDKSessionInfo> {
    let projects_dir = get_projects_dir();
    let mut entries = match tokio::fs::read_dir(&projects_dir).await {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut all_sessions = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_type().await.is_ok_and(|t| t.is_dir()) {
            all_sessions.extend(read_sessions_from_dir(&entry.path(), None).await);
        }
    }

    let deduped = deduplicate_by_session_id(all_sessions);
    apply_sort_limit_offset(deduped, limit, offset)
}

/// Lists sessions with metadata extracted from stat + head/tail reads.
///
/// When `directory` is provided (non-empty), returns sessions for that
/// project directory and its git worktrees. When omitted or empty, returns
/// sessions across all projects.
///
/// Use `limit` and `offset` for pagination; `limit=Some(0)` means
/// "unbounded", matching upstream.
///
/// See also: [`super::list_sessions_from_store`] for the
/// [`super::SessionStore`]-backed async variant.
pub async fn list_sessions(
    directory: Option<&str>,
    limit: Option<usize>,
    offset: usize,
    include_worktrees: bool,
) -> Vec<SDKSessionInfo> {
    match directory {
        Some(d) if !d.is_empty() => {
            super::listing_worktrees::list_sessions_for_project(d, limit, offset, include_worktrees).await
        }
        _ => list_all_sessions(limit, offset).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(session_id: &str, last_modified: i64) -> SDKSessionInfo {
        SDKSessionInfo {
            session_id: session_id.to_string(),
            summary: "s".to_string(),
            last_modified,
            file_size: None,
            custom_title: None,
            first_prompt: None,
            git_branch: None,
            cwd: None,
            tag: None,
            created_at: None,
        }
    }

    #[test]
    fn dedup_keeps_newest_last_modified() {
        let sessions = vec![info("a", 100), info("a", 200), info("a", 50)];
        let result = deduplicate_by_session_id(sessions);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].last_modified, 200);
    }

    #[test]
    fn dedup_ties_keep_first_seen() {
        let mut first = info("a", 100);
        first.summary = "first".to_string();
        let mut tie = info("a", 100);
        tie.summary = "second".to_string();
        let result = deduplicate_by_session_id(vec![first, tie]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].summary, "first");
    }

    /// Distinct session_ids tied on `last_modified` must come out of dedup
    /// in first-seen order (matching Python's `dict`-based dedup, whose
    /// insertion order feeds a stable downstream sort). A `HashMap`-backed
    /// implementation would not guarantee this — this test exists to catch
    /// a regression back to that.
    #[test]
    fn dedup_preserves_first_seen_order_across_distinct_ids_with_tied_mtime() {
        let sessions = vec![info("z", 100), info("m", 100), info("a", 100), info("k", 100)];
        let result = deduplicate_by_session_id(sessions);
        let ids: Vec<&str> = result.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, vec!["z", "m", "a", "k"]);
    }

    #[test]
    fn apply_sort_limit_offset_sorts_descending() {
        let sessions = vec![info("a", 100), info("b", 300), info("c", 200)];
        let result = apply_sort_limit_offset(sessions, None, 0);
        let ids: Vec<&str> = result.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, vec!["b", "c", "a"]);
    }

    #[test]
    fn apply_sort_limit_offset_applies_offset_then_limit() {
        let sessions = vec![info("a", 100), info("b", 300), info("c", 200), info("d", 400)];
        let result = apply_sort_limit_offset(sessions, Some(1), 1);
        let ids: Vec<&str> = result.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, vec!["b"]);
    }

    #[test]
    fn apply_sort_limit_offset_zero_limit_is_unbounded() {
        let sessions = vec![info("a", 100), info("b", 200)];
        let result = apply_sort_limit_offset(sessions, Some(0), 0);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn apply_sort_limit_offset_offset_past_end_is_empty() {
        let sessions = vec![info("a", 100)];
        assert!(apply_sort_limit_offset(sessions, None, 5).is_empty());
    }

    #[tokio::test]
    async fn read_sessions_from_dir_missing_dir_returns_empty() {
        let result = read_sessions_from_dir(Path::new("/definitely/does/not/exist"), None).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn read_sessions_from_dir_skips_non_uuid_and_non_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("not-a-uuid.jsonl"), "{\"type\":\"user\"}\n").await.unwrap();
        tokio::fs::write(dir.path().join("notes.txt"), "hello").await.unwrap();
        let result = read_sessions_from_dir(dir.path(), None).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn read_sessions_from_dir_reads_valid_session() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = "550e8400-e29b-41d4-a716-446655440000";
        let content = "{\"type\":\"user\",\"message\":{\"content\":\"hi there\"}}\n";
        tokio::fs::write(dir.path().join(format!("{session_id}.jsonl")), content).await.unwrap();
        let result = read_sessions_from_dir(dir.path(), Some("/proj")).await;
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].session_id, session_id);
        assert_eq!(result[0].first_prompt.as_deref(), Some("hi there"));
    }
}
