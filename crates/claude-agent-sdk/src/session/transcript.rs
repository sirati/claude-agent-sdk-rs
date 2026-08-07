//! Transcript JSONL parsing and conversation-chain reconstruction.
//!
//! Ported from `_TRANSCRIPT_ENTRY_TYPES`, `_parse_transcript_entries`,
//! `_filter_transcript_entries`, `_build_conversation_chain`,
//! `_is_visible_message`, and `_to_session_message` in upstream
//! `_internal/sessions.py`.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::info::{SessionMessage, SessionMessageType};

/// Transcript entry types that carry `uuid` + `parentUuid` chain links.
const TRANSCRIPT_ENTRY_TYPES: [&str; 5] = ["user", "assistant", "progress", "system", "attachment"];

/// A parsed JSONL transcript entry, kept as a loose JSON object — mirrors
/// the TS `TranscriptEntry` type (fields: `type`, `uuid`, `parentUuid`,
/// `sessionId`, `message`, `isSidechain`, `isMeta`, `isCompactSummary`,
/// `teamName`) without committing to a fixed Rust struct, since the CLI's
/// on-disk union is internal and evolving.
pub(crate) type TranscriptEntry = Value;

fn uuid_of(entry: &TranscriptEntry) -> &str {
    entry.get("uuid").and_then(Value::as_str).unwrap_or("")
}

fn parent_uuid_of(entry: &TranscriptEntry) -> Option<&str> {
    entry.get("parentUuid").and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn type_of(entry: &TranscriptEntry) -> Option<&str> {
    entry.get("type").and_then(Value::as_str)
}

/// JSON truthiness, matching Python's `if entry.get(key):` checks: `null`,
/// `false`, `0`, `""`, `[]`, and `{}` are falsy; everything else is truthy.
fn is_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Parses JSONL content into transcript entries.
///
/// Only keeps entries that have a `uuid` and are transcript message types
/// (user/assistant/progress/system/attachment). Skips corrupt lines.
pub(crate) fn parse_transcript_entries(content: &str) -> Vec<TranscriptEntry> {
    let mut entries = Vec::new();
    for raw_line in content.split('\n') {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        if entry.as_object().is_none() {
            continue;
        }
        if type_of(&entry).is_some_and(|t| TRANSCRIPT_ENTRY_TYPES.contains(&t))
            && entry.get("uuid").and_then(Value::as_str).is_some()
        {
            entries.push(entry);
        }
    }
    entries
}

/// Filters already-parsed store entries to transcript message types with a
/// `uuid`. Mirrors [`parse_transcript_entries`] for the pre-parsed path so
/// chain-building never sees metadata-only entries (custom-title, tag,
/// agent_metadata, etc.).
pub(crate) fn filter_transcript_entries(entries: &[Value]) -> Vec<TranscriptEntry> {
    entries
        .iter()
        .filter(|e| {
            type_of(e).is_some_and(|t| TRANSCRIPT_ENTRY_TYPES.contains(&t)) && e.get("uuid").and_then(Value::as_str).is_some()
        })
        .cloned()
        .collect()
}

/// Builds the conversation chain by finding the leaf and walking
/// `parentUuid`.
///
/// Returns messages in chronological order (root -> leaf).
///
/// `logicalParentUuid` (set on `compact_boundary` entries) is intentionally
/// NOT followed — matches VS Code IDE behavior: post-compaction, the
/// `isCompactSummary` message replaces earlier messages, so following
/// logical parents would duplicate content.
pub(crate) fn build_conversation_chain(entries: &[TranscriptEntry]) -> Vec<TranscriptEntry> {
    if entries.is_empty() {
        return Vec::new();
    }

    // Index by uuid -> position, for O(1) parent lookup and file-order
    // tie-breaking.
    let mut index_by_uuid: HashMap<&str, usize> = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        index_by_uuid.insert(uuid_of(entry), i);
    }

    // Terminal messages: nothing points at them via parentUuid.
    let mut parent_uuids: HashSet<&str> = HashSet::new();
    for entry in entries {
        if let Some(p) = parent_uuid_of(entry) {
            parent_uuids.insert(p);
        }
    }
    let terminals: Vec<usize> = (0..entries.len()).filter(|&i| !parent_uuids.contains(uuid_of(&entries[i]))).collect();

    // From each terminal, walk back to find the nearest user/assistant leaf.
    let mut leaves: Vec<usize> = Vec::new();
    for &terminal in &terminals {
        let mut cur = Some(terminal);
        let mut seen: HashSet<&str> = HashSet::new();
        while let Some(i) = cur {
            let uid = uuid_of(&entries[i]);
            if !seen.insert(uid) {
                break;
            }
            if matches!(type_of(&entries[i]), Some("user") | Some("assistant")) {
                leaves.push(i);
                break;
            }
            cur = parent_uuid_of(&entries[i]).and_then(|p| index_by_uuid.get(p).copied());
        }
    }

    if leaves.is_empty() {
        return Vec::new();
    }

    // Pick the leaf from the main chain (not sidechain/team/meta), preferring
    // the highest position in the entries array (most recent in file).
    let main_leaves: Vec<usize> = leaves
        .iter()
        .copied()
        .filter(|&i| {
            let e = &entries[i];
            !is_truthy(e.get("isSidechain")) && !is_truthy(e.get("teamName")) && !is_truthy(e.get("isMeta"))
        })
        .collect();

    let candidates = if main_leaves.is_empty() { &leaves } else { &main_leaves };
    let leaf = *candidates
        .iter()
        .max_by_key(|&&i| index_by_uuid.get(uuid_of(&entries[i])).copied().unwrap_or(0))
        .expect("candidates is non-empty");

    // Walk from leaf to root via parentUuid, then reverse.
    let mut chain: Vec<usize> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut cur = Some(leaf);
    while let Some(i) = cur {
        let uid = uuid_of(&entries[i]);
        if !seen.insert(uid) {
            break;
        }
        chain.push(i);
        cur = parent_uuid_of(&entries[i]).and_then(|p| index_by_uuid.get(p).copied());
    }
    chain.reverse();
    chain.into_iter().map(|i| entries[i].clone()).collect()
}

/// Builds the conversation chain for a subagent transcript.
///
/// Subagent transcripts are simpler than main sessions — no compaction, no
/// sidechains, no preserved segments. Finds the last user/assistant entry
/// and walks `parentUuid` links back to the root.
pub(crate) fn build_subagent_chain(entries: &[TranscriptEntry]) -> Vec<TranscriptEntry> {
    if entries.is_empty() {
        return Vec::new();
    }

    let mut by_uuid: HashMap<&str, usize> = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        by_uuid.insert(uuid_of(entry), i);
    }

    // Subagent transcripts are linear — the last user/assistant entry is the
    // leaf.
    let Some(leaf) = entries.iter().rposition(|e| matches!(type_of(e), Some("user") | Some("assistant"))) else {
        return Vec::new();
    };

    let mut chain: Vec<usize> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut cur = Some(leaf);
    while let Some(i) = cur {
        let uid = uuid_of(&entries[i]);
        if !seen.insert(uid) {
            break;
        }
        chain.push(i);
        cur = parent_uuid_of(&entries[i]).and_then(|p| by_uuid.get(p).copied());
    }
    chain.reverse();
    chain.into_iter().map(|i| entries[i].clone()).collect()
}

/// Returns `true` if the entry should be included in the returned messages.
pub(crate) fn is_visible_message(entry: &TranscriptEntry) -> bool {
    if !matches!(type_of(entry), Some("user") | Some("assistant")) {
        return false;
    }
    if is_truthy(entry.get("isMeta")) {
        return false;
    }
    if is_truthy(entry.get("isSidechain")) {
        return false;
    }
    // Note: isCompactSummary messages are intentionally included. They
    // contain the summarized content from compacted conversations and are
    // the only representation of that content post-compaction. This
    // matches VS Code IDE behavior (transcriptToSessionMessage does not
    // filter them).
    !is_truthy(entry.get("teamName"))
}

/// Converts a transcript entry into a [`SessionMessage`].
pub(crate) fn to_session_message(entry: &TranscriptEntry) -> SessionMessage {
    // Narrows to user/assistant — is_visible_message already guarantees
    // this for real callers.
    let type_ = if type_of(entry) == Some("user") { SessionMessageType::User } else { SessionMessageType::Assistant };
    SessionMessage {
        type_,
        uuid: entry.get("uuid").and_then(Value::as_str).unwrap_or("").to_string(),
        session_id: entry.get("sessionId").and_then(Value::as_str).unwrap_or("").to_string(),
        message: entry.get("message").cloned().unwrap_or(Value::Null),
        parent_tool_use_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_transcript_entries_filters_by_type_and_uuid() {
        let content = concat!(
            "{\"type\":\"user\",\"uuid\":\"a\"}\n",
            "{\"type\":\"tag\",\"uuid\":\"b\"}\n", // wrong type, dropped
            "{\"type\":\"assistant\"}\n",          // no uuid, dropped
            "not json\n",                          // corrupt, dropped
            "\n",                                  // blank, dropped
            "{\"type\":\"progress\",\"uuid\":\"c\"}\n",
        );
        let entries = parse_transcript_entries(content);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["uuid"], "a");
        assert_eq!(entries[1]["uuid"], "c");
    }

    #[test]
    fn filter_transcript_entries_matches_parse_semantics() {
        let raw = vec![json!({"type": "user", "uuid": "a"}), json!({"type": "custom-title", "uuid": "b"}), json!({"type": "system", "uuid": "c"})];
        let filtered = filter_transcript_entries(&raw);
        assert_eq!(filtered.len(), 2);
    }

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
    fn build_conversation_chain_walks_root_to_leaf() {
        let entries = vec![entry("1", None, "user"), entry("2", Some("1"), "assistant"), entry("3", Some("2"), "user")];
        let chain = build_conversation_chain(&entries);
        let uuids: Vec<&str> = chain.iter().map(|e| e["uuid"].as_str().unwrap()).collect();
        assert_eq!(uuids, vec!["1", "2", "3"]);
    }

    #[test]
    fn build_conversation_chain_empty_for_no_entries() {
        assert!(build_conversation_chain(&[]).is_empty());
    }

    #[test]
    fn build_conversation_chain_prefers_main_leaf_over_sidechain() {
        let mut sidechain = entry("2", Some("1"), "assistant");
        sidechain["isSidechain"] = json!(true);
        let entries = vec![entry("1", None, "user"), sidechain, entry("3", Some("1"), "user")];
        let chain = build_conversation_chain(&entries);
        let uuids: Vec<&str> = chain.iter().map(|e| e["uuid"].as_str().unwrap()).collect();
        assert_eq!(uuids, vec!["1", "3"]);
    }

    #[test]
    fn build_conversation_chain_breaks_cycles() {
        // 1 <-> 2 form a cycle; should not infinite-loop.
        let entries = vec![entry("1", Some("2"), "user"), entry("2", Some("1"), "assistant")];
        let chain = build_conversation_chain(&entries);
        assert!(chain.len() <= 2);
    }

    #[test]
    fn build_subagent_chain_walks_root_to_leaf() {
        let entries = vec![entry("1", None, "user"), entry("2", Some("1"), "assistant"), entry("3", Some("2"), "user")];
        let chain = build_subagent_chain(&entries);
        let uuids: Vec<&str> = chain.iter().map(|e| e["uuid"].as_str().unwrap()).collect();
        assert_eq!(uuids, vec!["1", "2", "3"]);
    }

    #[test]
    fn build_subagent_chain_empty_for_no_user_or_assistant() {
        let entries = vec![entry("1", None, "tag")];
        assert!(build_subagent_chain(&entries).is_empty());
    }

    #[test]
    fn build_subagent_chain_breaks_cycles() {
        let entries = vec![entry("1", Some("2"), "user"), entry("2", Some("1"), "assistant")];
        let chain = build_subagent_chain(&entries);
        assert!(chain.len() <= 2);
    }

    #[test]
    fn is_visible_message_filters_meta_sidechain_and_team() {
        assert!(is_visible_message(&entry("1", None, "user")));
        assert!(!is_visible_message(&entry("1", None, "tag")));

        let mut meta = entry("1", None, "user");
        meta["isMeta"] = json!(true);
        assert!(!is_visible_message(&meta));

        let mut side = entry("1", None, "assistant");
        side["isSidechain"] = json!(true);
        assert!(!is_visible_message(&side));

        let mut team = entry("1", None, "user");
        team["teamName"] = json!("dev-team");
        assert!(!is_visible_message(&team));
    }

    #[test]
    fn is_visible_message_includes_compact_summary() {
        let mut e = entry("1", None, "user");
        e["isCompactSummary"] = json!(true);
        assert!(is_visible_message(&e));
    }

    #[test]
    fn to_session_message_maps_fields() {
        let mut e = entry("u1", None, "user");
        e["sessionId"] = json!("s1");
        e["message"] = json!({"role": "user", "content": "hi"});
        let msg = to_session_message(&e);
        assert_eq!(msg.type_, SessionMessageType::User);
        assert_eq!(msg.uuid, "u1");
        assert_eq!(msg.session_id, "s1");
        assert_eq!(msg.message["content"], "hi");
        assert!(msg.parent_tool_use_id.is_none());
    }

    #[test]
    fn to_session_message_defaults_missing_message_to_null() {
        let e = entry("u1", None, "assistant");
        let msg = to_session_message(&e);
        assert_eq!(msg.type_, SessionMessageType::Assistant);
        assert_eq!(msg.message, Value::Null);
    }
}
