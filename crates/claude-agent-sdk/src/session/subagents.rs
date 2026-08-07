//! Subagent transcript listing/reading: `list_subagents()` /
//! `get_subagent_messages()`.
//!
//! Ported from `_resolve_session_file_path` (reused via
//! [`super::local_session_file::find_session_file`], not re-ported —
//! identical "first candidate with size > 0" semantics), `_resolve_subagents_dir`,
//! `_collect_agent_files`, `_build_subagent_chain` (in
//! [`super::transcript::build_subagent_chain`]), `list_subagents`, and
//! `get_subagent_messages` in upstream `_internal/sessions.py`.

use std::path::PathBuf;

use futures::future::BoxFuture;

use super::info::SessionMessage;
use super::local::validate_uuid;
use super::local_session_file::find_session_file;
use super::message_paging::entries_to_subagent_messages;
use super::transcript::parse_transcript_entries;

/// Resolves the subagents directory for a given session.
///
/// The session file lives at `<projectDir>/<sessionId>.jsonl` and the
/// subagents directory at `<projectDir>/<sessionId>/subagents/`. Returns
/// `None` if the session cannot be found.
async fn resolve_subagents_dir(session_id: &str, directory: Option<&str>) -> Option<PathBuf> {
    let resolved = find_session_file(session_id, directory).await?;
    // Strip the .jsonl suffix to derive the session directory.
    let session_dir = resolved.with_extension("");
    Some(session_dir.join("subagents"))
}

/// Recursively collects `agent-*.jsonl` files from a directory tree.
///
/// Subagent transcripts may live directly in `subagents/` or in nested
/// subdirectories such as `subagents/workflows/<runId>/`. Each directory
/// level is visited in filename-sorted order (matches upstream), so results
/// are deterministic across filesystems with unordered directory listings.
fn collect_agent_files(base_dir: PathBuf) -> BoxFuture<'static, Vec<(String, PathBuf)>> {
    Box::pin(async move {
        let mut results = Vec::new();
        let Ok(mut read_dir) = tokio::fs::read_dir(&base_dir).await else {
            return results;
        };

        let mut names = Vec::new();
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            names.push(entry.file_name());
        }
        names.sort();

        for name in names {
            let path = base_dir.join(&name);
            let Ok(metadata) = tokio::fs::metadata(&path).await else { continue };
            let name = name.to_string_lossy();

            if metadata.is_file() {
                if let Some(agent_id) = name.strip_prefix("agent-").and_then(|rest| rest.strip_suffix(".jsonl")) {
                    results.push((agent_id.to_string(), path));
                }
            } else if metadata.is_dir() {
                results.extend(collect_agent_files(path).await);
            }
        }
        results
    })
}

/// Lists subagent IDs for a given session by scanning the subagents
/// directory.
///
/// Subagent transcripts are stored at
/// `~/.claude/projects/<project>/<sessionId>/subagents/agent-<agentId>.jsonl`
/// (and may be nested in subdirectories such as `workflows/<runId>/`).
///
/// `directory`: project directory to find the session in. If omitted,
/// searches all project directories under `~/.claude/projects/`.
///
/// Returns an empty list if the session is not found, `session_id` is not a
/// valid UUID, or the session has no subagents.
///
/// See also: [`super::list_subagents_from_store`] for the
/// [`super::SessionStore`]-backed async variant.
pub async fn list_subagents(session_id: &str, directory: Option<&str>) -> Vec<String> {
    if validate_uuid(session_id).is_none() {
        return Vec::new();
    }
    let Some(subagents_dir) = resolve_subagents_dir(session_id, directory).await else {
        return Vec::new();
    };
    collect_agent_files(subagents_dir).await.into_iter().map(|(agent_id, _)| agent_id).collect()
}

/// Reads a subagent's conversation messages from its JSONL transcript file.
///
/// Parses the subagent transcript, builds the conversation chain via
/// `parentUuid` links, and returns user/assistant messages in chronological
/// order. Returns an empty list if the session or subagent is not found,
/// `session_id` is not a valid UUID, or the transcript contains no
/// user/assistant messages.
///
/// See also: [`super::get_subagent_messages_from_store`] for the
/// [`super::SessionStore`]-backed async variant.
pub async fn get_subagent_messages(
    session_id: &str,
    agent_id: &str,
    directory: Option<&str>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SessionMessage> {
    if validate_uuid(session_id).is_none() || agent_id.is_empty() {
        return Vec::new();
    }
    let Some(subagents_dir) = resolve_subagents_dir(session_id, directory).await else {
        return Vec::new();
    };

    // The agent file may be directly in subagents/ or in a nested
    // subdirectory — scan to find it.
    let Some((_, path)) =
        collect_agent_files(subagents_dir).await.into_iter().find(|(found_id, _)| found_id == agent_id)
    else {
        return Vec::new();
    };

    let Ok(content) = tokio::fs::read_to_string(&path).await else {
        return Vec::new();
    };
    if content.is_empty() {
        return Vec::new();
    }

    let entries = parse_transcript_entries(&content);
    entries_to_subagent_messages(&entries, limit, offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::local_session_file::env_lock;

    /// Test-only wrapper taking `&Path` (the production helper takes an
    /// owned `PathBuf` since it recurses via `BoxFuture<'static, _>`).
    async fn collect_agent_files_pub_for_test(base_dir: &std::path::Path) -> Vec<(String, PathBuf)> {
        collect_agent_files(base_dir.to_path_buf()).await
    }

    const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[tokio::test]
    async fn list_subagents_rejects_invalid_uuid() {
        assert!(list_subagents("not-a-uuid", None).await.is_empty());
    }

    #[tokio::test]
    async fn collect_agent_files_finds_nested_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("agent-b.jsonl"), "{}").await.unwrap();
        let nested = dir.path().join("workflows/run1");
        tokio::fs::create_dir_all(&nested).await.unwrap();
        tokio::fs::write(nested.join("agent-a.jsonl"), "{}").await.unwrap();
        tokio::fs::write(dir.path().join("not-an-agent.jsonl"), "{}").await.unwrap();

        let files = collect_agent_files_pub_for_test(dir.path()).await;
        let ids: Vec<&str> = files.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a"]);
    }

    #[tokio::test]
    async fn list_subagents_and_get_subagent_messages_end_to_end() {
        let _guard = env_lock().lock().await;
        let config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", config_dir.path()) };

        let project_key = super::super::project_dir::project_key_for_directory(Some(project_dir.path().to_str().unwrap()));
        let sessions_dir = config_dir.path().join("projects").join(&project_key);
        tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
        tokio::fs::write(sessions_dir.join(format!("{SESSION_ID}.jsonl")), "{\"type\":\"user\"}\n").await.unwrap();

        let subagents_dir = sessions_dir.join(SESSION_ID).join("subagents");
        tokio::fs::create_dir_all(&subagents_dir).await.unwrap();
        let content = concat!(
            "{\"type\":\"user\",\"uuid\":\"u1\",\"sessionId\":\"s\",\"message\":{\"content\":\"hi\"}}\n",
            "{\"type\":\"assistant\",\"uuid\":\"u2\",\"parentUuid\":\"u1\",\"sessionId\":\"s\",\"message\":{\"content\":\"hey\"}}\n",
        );
        tokio::fs::write(subagents_dir.join("agent-abc.jsonl"), content).await.unwrap();

        let directory = Some(project_dir.path().to_str().unwrap());
        let ids = list_subagents(SESSION_ID, directory).await;
        assert_eq!(ids, vec!["abc".to_string()]);

        let messages = get_subagent_messages(SESSION_ID, "abc", directory, None, 0).await;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].uuid, "u1");
        assert_eq!(messages[1].uuid, "u2");

        assert!(get_subagent_messages(SESSION_ID, "does-not-exist", directory, None, 0).await.is_empty());
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }
}
