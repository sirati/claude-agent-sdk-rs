//! Worktree-aware single-project session scan.
//!
//! Ported from `_list_sessions_for_project` in upstream
//! `_internal/sessions.py`. Split out from [`super::listing`] because the
//! multi-worktree branch — matching git worktree paths against sanitized
//! project directory names, with a prefix-match fallback for paths that
//! exceed the filesystem component length limit — is a distinct, sizable
//! concern from the plain single-directory / all-projects scans.

use std::collections::HashSet;
use std::path::PathBuf;

use super::info::SDKSessionInfo;
use super::listing::{apply_sort_limit_offset, deduplicate_by_session_id, read_sessions_from_dir};
use super::local::get_worktree_paths;
use super::project_dir::{canonicalize_path, find_project_dir, get_projects_dir, sanitize_path, MAX_SANITIZED_LENGTH};

/// Lists sessions for a specific project directory (and its worktrees).
pub(crate) async fn list_sessions_for_project(
    directory: &str,
    limit: Option<usize>,
    offset: usize,
    include_worktrees: bool,
) -> Vec<SDKSessionInfo> {
    let canonical_dir = canonicalize_path(directory);

    let worktree_paths = if include_worktrees { get_worktree_paths(&canonical_dir).await } else { Vec::new() };

    // No worktrees (or scanning disabled) — just scan the single project dir.
    if worktree_paths.len() <= 1 {
        return scan_single_project_dir(&canonical_dir, limit, offset).await;
    }

    let projects_dir = get_projects_dir();
    // Case-insensitive directory-name matching only applies on Windows,
    // matching upstream's `sys.platform == "win32"` check.
    let case_insensitive = cfg!(target_os = "windows");
    let adjust = |s: String| if case_insensitive { s.to_lowercase() } else { s };

    // Sort worktree paths by sanitized prefix length (longest first) so more
    // specific matches take priority over shorter ones.
    let mut indexed: Vec<(String, String)> =
        worktree_paths.iter().map(|wt| (wt.clone(), adjust(sanitize_path(wt)))).collect();
    indexed.sort_by_key(|(_, prefix)| std::cmp::Reverse(prefix.chars().count()));

    let mut all_dirents: Vec<PathBuf> = Vec::new();
    match tokio::fs::read_dir(&projects_dir).await {
        Ok(mut rd) => {
            while let Ok(Some(entry)) = rd.next_entry().await {
                if entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                    all_dirents.push(entry.path());
                }
            }
        }
        // Fall back to single project dir when the projects dir can't be
        // scanned at all.
        Err(_) => return scan_single_project_dir(&canonical_dir, limit, offset).await,
    }

    let mut all_sessions: Vec<SDKSessionInfo> = Vec::new();
    let mut seen_dirs: HashSet<String> = HashSet::new();

    // Always include the user's actual directory (handles subdirectories
    // like /repo/packages/my-app that won't match worktree root prefixes).
    if let Some(canonical_project_dir) = find_project_dir(&canonical_dir).await {
        let dir_base = adjust(canonical_project_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        seen_dirs.insert(dir_base);
        all_sessions.extend(read_sessions_from_dir(&canonical_project_dir, Some(&canonical_dir)).await);
    }

    for entry_path in all_dirents {
        let dir_name = adjust(entry_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        if seen_dirs.contains(&dir_name) {
            continue;
        }

        for (wt_path, prefix) in &indexed {
            // Only use startswith for truncated paths (>MAX_SANITIZED_LENGTH)
            // where a hash suffix follows. For short paths, require exact
            // match to avoid /root/project matching /root/project-foo.
            let is_match = dir_name == *prefix
                || (prefix.chars().count() >= MAX_SANITIZED_LENGTH && dir_name.starts_with(&format!("{prefix}-")));
            if is_match {
                seen_dirs.insert(dir_name.clone());
                all_sessions.extend(read_sessions_from_dir(&entry_path, Some(wt_path)).await);
                break;
            }
        }
    }

    let deduped = deduplicate_by_session_id(all_sessions);
    apply_sort_limit_offset(deduped, limit, offset)
}

async fn scan_single_project_dir(canonical_dir: &str, limit: Option<usize>, offset: usize) -> Vec<SDKSessionInfo> {
    let Some(project_dir) = find_project_dir(canonical_dir).await else {
        return apply_sort_limit_offset(Vec::new(), limit, offset);
    };
    let sessions = read_sessions_from_dir(&project_dir, Some(canonical_dir)).await;
    apply_sort_limit_offset(sessions, limit, offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::local_session_file::env_lock;

    #[tokio::test]
    async fn list_sessions_for_project_returns_empty_when_no_project_dir() {
        let _guard = env_lock().lock().await;
        let config_dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", config_dir.path()) };
        tokio::fs::create_dir_all(config_dir.path().join("projects")).await.unwrap();

        let result = list_sessions_for_project("/definitely/not/a/real/project/dir", None, 0, false).await;
        assert!(result.is_empty());
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }

    #[tokio::test]
    async fn list_sessions_for_project_single_dir_reads_sessions() {
        let _guard = env_lock().lock().await;
        let config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", config_dir.path()) };

        let project_key = super::super::project_dir::project_key_for_directory(Some(project_dir.path().to_str().unwrap()));
        let sessions_dir = config_dir.path().join("projects").join(&project_key);
        tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
        let session_id = "550e8400-e29b-41d4-a716-446655440000";
        tokio::fs::write(
            sessions_dir.join(format!("{session_id}.jsonl")),
            "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n",
        )
        .await
        .unwrap();

        let result = list_sessions_for_project(project_dir.path().to_str().unwrap(), None, 0, false).await;
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].session_id, session_id);
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }
}
