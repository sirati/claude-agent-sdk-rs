//! Local (on-disk) session mutations: rename, tag, delete.
//!
//! Ported from `rename_session` / `tag_session` / `delete_session` in
//! upstream `_internal/session_mutations.py`. `fork_session` is the fourth
//! local mutation but lives in [`super::fork`] (it's the largest piece —
//! kept separate per this crate's file-size convention).
//!
//! `list_sessions` reads the LAST `custom-title`/`tag` entry from a
//! session's JSONL tail, so repeated renames/tags are safe — the most
//! recent append wins.

use crate::errors::{ClaudeError, Result};

use super::local::validate_uuid;
use super::local_session_file::{append_to_session, find_session_file};
use super::unicode_sanitize::sanitize_unicode;

/// Rename a session by appending a `custom-title` entry.
///
/// # Errors
/// [`ClaudeError::InvalidInput`] if `session_id` is not a valid UUID, or if
/// `title` is empty/whitespace-only. [`ClaudeError::NotFound`] if the
/// session file cannot be found.
///
/// See also: `rename_session_via_store` (in [`super::mutations_store`]) for
/// the [`super::SessionStore`]-backed async variant.
pub async fn rename_session(session_id: &str, title: &str, directory: Option<&str>) -> Result<()> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeError::InvalidInput(format!("Invalid session_id: {session_id}")));
    }
    // Matches CLI guard — empty/whitespace titles are rejected rather than
    // overloaded as "clear title".
    let stripped = title.trim();
    if stripped.is_empty() {
        return Err(ClaudeError::InvalidInput("title must be non-empty".to_string()));
    }

    let entry = serde_json::json!({
        "type": "custom-title",
        "customTitle": stripped,
        "sessionId": session_id,
    });
    let data = format!("{}\n", serde_json::to_string(&entry).unwrap_or_default());

    append_to_session(session_id, &data, directory).await
}

/// Tag a session. Pass `None` to clear the tag.
///
/// Tags are Unicode-sanitized before storing (removes zero-width chars,
/// directional marks, private-use characters, etc.) for CLI filter
/// compatibility.
///
/// # Errors
/// [`ClaudeError::InvalidInput`] if `session_id` is not a valid UUID, or if
/// `tag` is empty/whitespace-only after sanitization.
/// [`ClaudeError::NotFound`] if the session file cannot be found.
///
/// See also: `tag_session_via_store` (in [`super::mutations_store`]) for the
/// [`super::SessionStore`]-backed async variant.
pub async fn tag_session(session_id: &str, tag: Option<&str>, directory: Option<&str>) -> Result<()> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeError::InvalidInput(format!("Invalid session_id: {session_id}")));
    }
    let sanitized_tag = match tag {
        Some(tag) => {
            let sanitized = sanitize_unicode(tag);
            let sanitized = sanitized.trim();
            if sanitized.is_empty() {
                return Err(ClaudeError::InvalidInput("tag must be non-empty (use None to clear)".to_string()));
            }
            sanitized.to_string()
        }
        None => String::new(),
    };

    let entry = serde_json::json!({
        "type": "tag",
        "tag": sanitized_tag,
        "sessionId": session_id,
    });
    let data = format!("{}\n", serde_json::to_string(&entry).unwrap_or_default());

    append_to_session(session_id, &data, directory).await
}

/// Delete a session by removing its JSONL file and subagent transcripts.
///
/// This is a hard delete — the `{session_id}.jsonl` file is removed
/// permanently, along with the sibling `{session_id}/` subdirectory that
/// holds subagent transcripts (if it exists). Callers needing soft-delete
/// semantics can use `tag_session(id, Some("__hidden"))` and filter on
/// listing instead.
///
/// # Errors
/// [`ClaudeError::InvalidInput`] if `session_id` is not a valid UUID.
/// [`ClaudeError::NotFound`] if the session file cannot be found.
///
/// See also: `delete_session_via_store` (in [`super::mutations_store`]) for
/// the [`super::SessionStore`]-backed async variant.
pub async fn delete_session(session_id: &str, directory: Option<&str>) -> Result<()> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeError::InvalidInput(format!("Invalid session_id: {session_id}")));
    }

    let Some(path) = find_session_file(session_id, directory).await else {
        return Err(ClaudeError::NotFound(format!("Session {session_id} not found")));
    };

    match tokio::fs::remove_file(&path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ClaudeError::NotFound(format!("Session {session_id} not found")));
        }
        Err(e) => return Err(e.into()),
    }

    // Subagent transcripts live in a sibling {session_id}/ dir; often absent.
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::remove_dir_all(parent.join(session_id)).await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::local_session_file::env_lock;

    const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    // These tests point CLAUDE_CONFIG_DIR at a temp dir; serialize through
    // the shared `env_lock` (in `local_session_file`) since the env var is
    // process-global and `fork`/`import_to_store`'s tests mutate it too.
    async fn setup_session(
        config_dir: &std::path::Path,
        project_dir: &std::path::Path,
        session_id: &str,
        content: &str,
    ) -> std::path::PathBuf {
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", config_dir) };
        let project_key = super::super::project_dir::project_key_for_directory(Some(project_dir.to_str().unwrap()));
        let sessions_dir = config_dir.join("projects").join(project_key);
        tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
        let file = sessions_dir.join(format!("{session_id}.jsonl"));
        tokio::fs::write(&file, content).await.unwrap();
        file
    }

    #[tokio::test]
    async fn rename_session_appends_custom_title_entry() {
        let _guard = env_lock().lock().await;
        let config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let file = setup_session(config_dir.path(), project_dir.path(), SESSION_ID, "{\"type\":\"user\"}\n").await;

        rename_session(SESSION_ID, "New Title", Some(project_dir.path().to_str().unwrap())).await.unwrap();

        let content = tokio::fs::read_to_string(&file).await.unwrap();
        assert!(content.contains("\"type\":\"custom-title\""));
        assert!(content.contains("\"customTitle\":\"New Title\""));
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }

    #[tokio::test]
    async fn rename_session_rejects_blank_title() {
        let err = rename_session(SESSION_ID, "   ", None).await.unwrap_err();
        assert!(err.to_string().contains("non-empty"));
    }

    #[tokio::test]
    async fn rename_session_rejects_invalid_uuid() {
        let err = rename_session("not-a-uuid", "title", None).await.unwrap_err();
        assert!(err.to_string().contains("Invalid session_id"));
    }

    #[tokio::test]
    async fn tag_session_sanitizes_and_appends() {
        let _guard = env_lock().lock().await;
        let config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let file = setup_session(config_dir.path(), project_dir.path(), SESSION_ID, "{\"type\":\"user\"}\n").await;

        tag_session(SESSION_ID, Some("exp\u{200b}eriment"), Some(project_dir.path().to_str().unwrap()))
            .await
            .unwrap();

        let content = tokio::fs::read_to_string(&file).await.unwrap();
        assert!(content.contains("\"tag\":\"experiment\""));
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }

    #[tokio::test]
    async fn tag_session_none_clears_with_empty_string() {
        let _guard = env_lock().lock().await;
        let config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let file = setup_session(config_dir.path(), project_dir.path(), SESSION_ID, "{\"type\":\"user\"}\n").await;

        tag_session(SESSION_ID, None, Some(project_dir.path().to_str().unwrap())).await.unwrap();

        let content = tokio::fs::read_to_string(&file).await.unwrap();
        assert!(content.contains("\"tag\":\"\""));
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }

    #[tokio::test]
    async fn delete_session_removes_file_and_subagent_dir() {
        let _guard = env_lock().lock().await;
        let config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let file = setup_session(config_dir.path(), project_dir.path(), SESSION_ID, "{\"type\":\"user\"}\n").await;
        let subagent_dir = file.parent().unwrap().join(SESSION_ID);
        tokio::fs::create_dir_all(&subagent_dir).await.unwrap();

        delete_session(SESSION_ID, Some(project_dir.path().to_str().unwrap())).await.unwrap();

        assert!(tokio::fs::metadata(&file).await.is_err());
        assert!(tokio::fs::metadata(&subagent_dir).await.is_err());
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }

    #[tokio::test]
    async fn delete_session_not_found_errors() {
        let _guard = env_lock().lock().await;
        let config_dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", config_dir.path()) };
        tokio::fs::create_dir_all(config_dir.path().join("projects")).await.unwrap();

        let err = delete_session(SESSION_ID, None).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }
}
