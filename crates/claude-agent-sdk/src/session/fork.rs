//! Fork a local (on-disk) session into a new branch with fresh UUIDs.
//!
//! Ported from `fork_session` in upstream `_internal/session_mutations.py`.
//! The store-backed counterpart, `fork_session_via_store`, lives in
//! [`super::mutations_store`] — both share the transform in
//! [`super::fork_transform`].

use serde_json::Value;

use crate::errors::{ClaudeError, Result};

use super::fork_transform::{build_fork_lines, parse_fork_transcript};
use super::json_extract::{extract_first_prompt_from_head, extract_last_json_string_field};
use super::local::{validate_uuid, LITE_READ_BUF_SIZE};
use super::local_session_file::find_session_file_with_dir;

/// Result of a fork operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkSessionResult {
    /// UUID of the new forked session.
    pub session_id: String,
}

/// Fork a session into a new branch with fresh UUIDs.
///
/// Copies transcript messages from the source session into a new session
/// file, remapping every message UUID and preserving the `parentUuid`
/// chain. Supports `up_to_message_id` for branching from a specific point
/// in the conversation.
///
/// Forked sessions start without undo history (file-history snapshots are
/// not copied).
///
/// # Errors
/// Returns [`ClaudeError::InvalidInput`] if `session_id` or
/// `up_to_message_id` is not a valid UUID, or if the session has no
/// messages to fork (or `up_to_message_id` isn't found in the transcript).
/// Returns [`ClaudeError::NotFound`] if the source session file cannot be
/// found.
///
/// See also: `fork_session_via_store` (in [`super::mutations_store`]) for
/// the [`super::SessionStore`]-backed variant.
pub async fn fork_session(
    session_id: &str,
    directory: Option<&str>,
    up_to_message_id: Option<&str>,
    title: Option<&str>,
) -> Result<ForkSessionResult> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeError::InvalidInput(format!("Invalid session_id: {session_id}")));
    }
    if let Some(up_to) = up_to_message_id {
        if validate_uuid(up_to).is_none() {
            return Err(ClaudeError::InvalidInput(format!("Invalid up_to_message_id: {up_to}")));
        }
    }

    let Some((file_path, project_dir)) = find_session_file_with_dir(session_id, directory).await else {
        let suffix = directory.map(|d| format!(" in project directory for {d}")).unwrap_or_default();
        return Err(ClaudeError::NotFound(format!("Session {session_id} not found{suffix}")));
    };

    let content = tokio::fs::read(&file_path).await?;
    if content.is_empty() {
        return Err(ClaudeError::InvalidInput(format!("Session {session_id} has no messages to fork")));
    }

    let (transcript, content_replacements) = parse_fork_transcript(&content, session_id);

    let derive_title = || {
        let buf_len = content.len();
        let head = String::from_utf8_lossy(&content[..buf_len.min(LITE_READ_BUF_SIZE)]);
        let tail_start = buf_len.saturating_sub(LITE_READ_BUF_SIZE);
        let tail = String::from_utf8_lossy(&content[tail_start..]);
        extract_last_json_string_field(&tail, "customTitle")
            .or_else(|| extract_last_json_string_field(&head, "customTitle"))
            .or_else(|| extract_last_json_string_field(&tail, "aiTitle"))
            .or_else(|| extract_last_json_string_field(&head, "aiTitle"))
            .or_else(|| {
                let prompt = extract_first_prompt_from_head(&head);
                if prompt.is_empty() {
                    None
                } else {
                    Some(prompt)
                }
            })
    };

    let (forked_session_id, entries) =
        build_fork_lines(transcript, content_replacements, session_id, up_to_message_id, title, derive_title)?;

    let lines: Vec<String> =
        entries.iter().map(|e| serde_json::to_string(&Value::Object(e.clone())).unwrap_or_default()).collect();
    let mut body = lines.join("\n");
    body.push('\n');

    let fork_path = project_dir.join(format!("{forked_session_id}.jsonl"));
    let mut open_options = tokio::fs::OpenOptions::new();
    open_options.write(true).create_new(true);
    #[cfg(unix)]
    open_options.mode(0o600);
    let mut file = open_options.open(&fork_path).await?;
    tokio::io::AsyncWriteExt::write_all(&mut file, body.as_bytes()).await?;

    Ok(ForkSessionResult { session_id: forked_session_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::local_session_file::env_lock;

    const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

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
    async fn fork_session_writes_new_file_with_remapped_uuids() {
        let _guard = env_lock().lock().await;
        let config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let transcript = format!(
            "{{\"type\":\"user\",\"uuid\":\"u1\",\"sessionId\":\"{SESSION_ID}\"}}\n{{\"type\":\"assistant\",\"uuid\":\"u2\",\"parentUuid\":\"u1\",\"sessionId\":\"{SESSION_ID}\"}}\n"
        );
        setup_session(config_dir.path(), project_dir.path(), SESSION_ID, &transcript).await;

        let result =
            fork_session(SESSION_ID, Some(project_dir.path().to_str().unwrap()), None, Some("Forked!")).await.unwrap();
        assert_ne!(result.session_id, SESSION_ID);

        let project_key = super::super::project_dir::project_key_for_directory(Some(project_dir.path().to_str().unwrap()));
        let fork_path = config_dir.path().join("projects").join(project_key).join(format!("{}.jsonl", result.session_id));
        let content = tokio::fs::read_to_string(&fork_path).await.unwrap();
        assert!(content.contains("\"customTitle\":\"Forked!\""));
        assert!(!content.contains("\"uuid\":\"u1\""));
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }

    #[tokio::test]
    async fn fork_session_not_found_errors() {
        let _guard = env_lock().lock().await;
        let config_dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", config_dir.path()) };
        tokio::fs::create_dir_all(config_dir.path().join("projects")).await.unwrap();

        let err = fork_session(SESSION_ID, None, None, None).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }

    #[tokio::test]
    async fn fork_session_rejects_invalid_session_id() {
        let err = fork_session("bad-id", None, None, None).await.unwrap_err();
        assert!(err.to_string().contains("Invalid session_id"));
    }
}
