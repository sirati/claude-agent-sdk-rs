//! Converts parsed transcript entries into paginated [`SessionMessage`] lists.
//!
//! Ported from `_entries_to_session_messages` and `_entries_to_subagent_messages`
//! in upstream `_internal/sessions.py`. Shared by the local-disk and
//! [`super::store::SessionStore`]-backed read paths, for both main-session
//! and subagent transcripts — chain-building differs (main sessions vs.
//! subagents), but the filter-then-paginate tail is identical.

use super::info::SessionMessage;
use super::transcript::{
    build_conversation_chain, build_subagent_chain, is_visible_message, to_session_message, TranscriptEntry,
};

/// Applies `limit`/`offset` the way upstream does: `limit=Some(0)` (like
/// Python's `limit=0`) is treated as "no limit", matching
/// `if limit is not None and limit > 0`. `offset` has no negative case in
/// Rust's `usize`, so `offset > 0` covers upstream's guard exactly.
fn paginate(mut messages: Vec<SessionMessage>, limit: Option<usize>, offset: usize) -> Vec<SessionMessage> {
    match limit {
        Some(l) if l > 0 => {
            let start = offset.min(messages.len());
            let end = start.saturating_add(l).min(messages.len());
            messages[start..end].to_vec()
        }
        _ if offset > 0 => {
            let start = offset.min(messages.len());
            messages.split_off(start)
        }
        _ => messages,
    }
}

/// Builds the main-session conversation chain from parsed entries and
/// applies paging.
pub(crate) fn entries_to_session_messages(
    entries: &[TranscriptEntry],
    limit: Option<usize>,
    offset: usize,
) -> Vec<SessionMessage> {
    let chain = build_conversation_chain(entries);
    let messages: Vec<SessionMessage> = chain.iter().filter(|e| is_visible_message(e)).map(to_session_message).collect();
    paginate(messages, limit, offset)
}

/// Builds the subagent chain from parsed entries and applies paging.
pub(crate) fn entries_to_subagent_messages(
    entries: &[TranscriptEntry],
    limit: Option<usize>,
    offset: usize,
) -> Vec<SessionMessage> {
    let chain = build_subagent_chain(entries);
    let messages: Vec<SessionMessage> = chain
        .iter()
        .filter(|e| matches!(e.get("type").and_then(|v| v.as_str()), Some("user") | Some("assistant")))
        .map(to_session_message)
        .collect();
    paginate(messages, limit, offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn entry(uuid: &str, parent: Option<&str>, ty: &str) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("uuid".to_string(), json!(uuid));
        obj.insert("type".to_string(), json!(ty));
        if let Some(p) = parent {
            obj.insert("parentUuid".to_string(), json!(p));
        }
        Value::Object(obj)
    }

    #[test]
    fn entries_to_session_messages_filters_and_orders() {
        let entries = vec![entry("1", None, "user"), entry("2", Some("1"), "assistant")];
        let messages = entries_to_session_messages(&entries, None, 0);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].uuid, "1");
        assert_eq!(messages[1].uuid, "2");
    }

    #[test]
    fn entries_to_session_messages_limit_zero_means_unlimited() {
        let entries = vec![entry("1", None, "user"), entry("2", Some("1"), "assistant")];
        let messages = entries_to_session_messages(&entries, Some(0), 0);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn entries_to_session_messages_applies_offset_and_limit() {
        let entries = vec![
            entry("1", None, "user"),
            entry("2", Some("1"), "assistant"),
            entry("3", Some("2"), "user"),
        ];
        let messages = entries_to_session_messages(&entries, Some(1), 1);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].uuid, "2");
    }

    #[test]
    fn entries_to_session_messages_offset_beyond_len_is_empty() {
        let entries = vec![entry("1", None, "user")];
        assert!(entries_to_session_messages(&entries, None, 5).is_empty());
    }

    #[test]
    fn entries_to_subagent_messages_filters_and_orders() {
        let entries = vec![entry("1", None, "user"), entry("2", Some("1"), "assistant")];
        let messages = entries_to_subagent_messages(&entries, None, 0);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].uuid, "1");
        assert_eq!(messages[1].uuid, "2");
    }

    #[test]
    fn entries_to_subagent_messages_applies_paging() {
        let entries = vec![
            entry("1", None, "user"),
            entry("2", Some("1"), "assistant"),
            entry("3", Some("2"), "user"),
        ];
        let messages = entries_to_subagent_messages(&entries, Some(2), 0);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].uuid, "1");
        assert_eq!(messages[1].uuid, "2");
    }
}
