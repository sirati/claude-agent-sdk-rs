//! Full transcript reconstruction: `get_session_messages()`.
//!
//! Ported from `_read_session_file` / `_try_read_session_file` /
//! `get_session_messages` in upstream `_internal/sessions.py`. Locating the
//! file reuses [`super::local_session_file::find_session_file`] rather than
//! re-walking project directories + worktrees a second time — that helper's
//! "first candidate with size > 0" stat check has the same effect as
//! upstream's "first candidate with non-empty read" try-read loop.

use super::info::SessionMessage;
use super::local::validate_uuid;
use super::local_session_file::find_session_file;
use super::message_paging::entries_to_session_messages;
use super::transcript::parse_transcript_entries;

/// Finds and reads the session JSONL file's full contents.
async fn read_session_file(session_id: &str, directory: Option<&str>) -> Option<String> {
    let path = find_session_file(session_id, directory).await?;
    tokio::fs::read_to_string(&path).await.ok()
}

/// Reads a session's conversation messages from its JSONL transcript file.
///
/// Parses the full JSONL, builds the conversation chain via `parentUuid`
/// links, and returns user/assistant messages in chronological order.
///
/// `directory`: project directory to find the session in. If omitted,
/// searches all project directories under `~/.claude/projects/`.
///
/// Returns an empty list if the session is not found, `session_id` is not a
/// valid UUID, or the transcript contains no visible messages.
///
/// See also: [`super::get_session_messages_from_store`] for the
/// [`super::SessionStore`]-backed async variant.
pub async fn get_session_messages(
    session_id: &str,
    directory: Option<&str>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SessionMessage> {
    if validate_uuid(session_id).is_none() {
        return Vec::new();
    }

    let Some(content) = read_session_file(session_id, directory).await else {
        return Vec::new();
    };

    let entries = parse_transcript_entries(&content);
    entries_to_session_messages(&entries, limit, offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::local_session_file::env_lock;

    const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[tokio::test]
    async fn get_session_messages_rejects_invalid_uuid() {
        assert!(get_session_messages("not-a-uuid", None, None, 0).await.is_empty());
    }

    #[tokio::test]
    async fn get_session_messages_missing_session_returns_empty() {
        let _guard = env_lock().lock().await;
        let config_dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", config_dir.path()) };
        tokio::fs::create_dir_all(config_dir.path().join("projects")).await.unwrap();

        assert!(get_session_messages(SESSION_ID, None, None, 0).await.is_empty());
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }

    #[tokio::test]
    async fn get_session_messages_reads_and_chains() {
        let _guard = env_lock().lock().await;
        let config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", config_dir.path()) };

        let project_key = super::super::project_dir::project_key_for_directory(Some(project_dir.path().to_str().unwrap()));
        let sessions_dir = config_dir.path().join("projects").join(&project_key);
        tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
        let content = concat!(
            "{\"type\":\"user\",\"uuid\":\"u1\",\"sessionId\":\"s\",\"message\":{\"content\":\"hi\"}}\n",
            "{\"type\":\"assistant\",\"uuid\":\"u2\",\"parentUuid\":\"u1\",\"sessionId\":\"s\",\"message\":{\"content\":\"hey\"}}\n",
        );
        tokio::fs::write(sessions_dir.join(format!("{SESSION_ID}.jsonl")), content).await.unwrap();

        let messages =
            get_session_messages(SESSION_ID, Some(project_dir.path().to_str().unwrap()), None, 0).await;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].uuid, "u1");
        assert_eq!(messages[1].uuid, "u2");
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }
}
