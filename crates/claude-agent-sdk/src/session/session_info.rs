//! Single-session metadata lookup: `get_session_info()`.
//!
//! Ported from `get_session_info` in upstream `_internal/sessions.py`.

use super::info::SDKSessionInfo;
use super::lite_info::parse_session_info_from_lite;
use super::local::{get_worktree_paths, read_session_lite, validate_uuid};
use super::project_dir::{canonicalize_path, find_project_dir, get_projects_dir};

/// Reads metadata for a single session by ID.
///
/// Wraps a lite (stat + head/tail) read for one file — no directory scan.
/// Directory resolution matches [`super::get_session_messages`]: `directory`
/// is the project path; when omitted, all project directories are searched
/// for the session file.
///
/// Returns `None` if the session file is not found, is a sidechain session,
/// or has no extractable summary.
///
/// See also: [`super::get_session_info_from_store`] for the
/// [`super::SessionStore`]-backed async variant.
pub async fn get_session_info(session_id: &str, directory: Option<&str>) -> Option<SDKSessionInfo> {
    let uuid = validate_uuid(session_id)?;
    let file_name = format!("{uuid}.jsonl");

    if let Some(directory) = directory {
        let canonical = canonicalize_path(directory);
        if let Some(project_dir) = find_project_dir(&canonical).await {
            if let Some(lite) = read_session_lite(&project_dir.join(&file_name)).await {
                return parse_session_info_from_lite(uuid, &lite, Some(&canonical));
            }
        }

        // Worktree fallback — matches get_session_messages semantics.
        // Sessions may live under a different worktree root.
        for wt in get_worktree_paths(&canonical).await {
            if wt == canonical {
                continue;
            }
            if let Some(wt_project_dir) = find_project_dir(&wt).await {
                if let Some(lite) = read_session_lite(&wt_project_dir.join(&file_name)).await {
                    return parse_session_info_from_lite(uuid, &lite, Some(&wt));
                }
            }
        }
        return None;
    }

    // No directory — search all project directories for the session file.
    let projects_dir = get_projects_dir();
    let mut entries = tokio::fs::read_dir(&projects_dir).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Some(lite) = read_session_lite(&entry.path().join(&file_name)).await {
            return parse_session_info_from_lite(uuid, &lite, None);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::local_session_file::env_lock;

    const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[tokio::test]
    async fn get_session_info_rejects_invalid_uuid() {
        assert!(get_session_info("not-a-uuid", None).await.is_none());
    }

    #[tokio::test]
    async fn get_session_info_reads_from_project_dir() {
        let _guard = env_lock().lock().await;
        let config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", config_dir.path()) };

        let project_key = super::super::project_dir::project_key_for_directory(Some(project_dir.path().to_str().unwrap()));
        let sessions_dir = config_dir.path().join("projects").join(&project_key);
        tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
        tokio::fs::write(
            sessions_dir.join(format!("{SESSION_ID}.jsonl")),
            "{\"type\":\"user\",\"message\":{\"content\":\"hello\"}}\n",
        )
        .await
        .unwrap();

        let info = get_session_info(SESSION_ID, Some(project_dir.path().to_str().unwrap())).await.unwrap();
        assert_eq!(info.session_id, SESSION_ID);
        assert_eq!(info.first_prompt.as_deref(), Some("hello"));
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }

    #[tokio::test]
    async fn get_session_info_missing_session_returns_none() {
        let _guard = env_lock().lock().await;
        let config_dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", config_dir.path()) };
        tokio::fs::create_dir_all(config_dir.path().join("projects")).await.unwrap();

        assert!(get_session_info(SESSION_ID, None).await.is_none());
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }
}
