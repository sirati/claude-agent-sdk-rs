//! Core fork transform: remap UUIDs and produce forked transcript entries.
//!
//! Ported from `_build_fork_lines`, `_parse_fork_transcript`, and
//! `_derive_title_from_entries` in upstream
//! `_internal/session_mutations.py`. Shared by the disk-backed
//! [`super::fork::fork_session`] and the store-backed
//! `fork_session_via_store` (in [`super::mutations_store`]).

use std::collections::HashMap;

use serde_json::{Map, Value};
use uuid::Uuid;

use crate::errors::{ClaudeError, Result};

use super::iso_time::iso_now;

/// One transcript entry, kept as an opaque JSON object map — see
/// [`super::types::SessionStoreEntry`]'s docs for why entries are treated as
/// pass-through blobs rather than a fixed struct.
pub(super) type Entry = Map<String, Value>;

const TRANSCRIPT_TYPES: [&str; 5] = ["user", "assistant", "attachment", "system", "progress"];

/// Python-style truthiness for a JSON value (used for `isSidechain`
/// filtering, which upstream checks with a bare `if e.get(...)`).
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

fn classify_entry(entry: Entry, session_id: &str, transcript: &mut Vec<Entry>, content_replacements: &mut Vec<Value>) {
    let entry_type = entry.get("type").and_then(Value::as_str).map(str::to_string);
    match entry_type.as_deref() {
        Some(t) if TRANSCRIPT_TYPES.contains(&t) && entry.get("uuid").is_some_and(Value::is_string) => {
            transcript.push(entry);
        }
        Some("content-replacement")
            if entry.get("sessionId").and_then(Value::as_str) == Some(session_id)
                && entry.get("replacements").is_some_and(Value::is_array) =>
        {
            if let Some(Value::Array(items)) = entry.get("replacements") {
                content_replacements.extend(items.iter().cloned());
            }
        }
        _ => {}
    }
}

/// Partitions already-parsed entries into transcript messages + collected
/// `content-replacement` records. Shared by the disk path (after JSONL
/// decode, see [`parse_fork_transcript`]) and the store path (entries are
/// already objects from `SessionStore::load`).
pub(super) fn partition_transcript_entries(entries: Vec<Entry>, session_id: &str) -> (Vec<Entry>, Vec<Value>) {
    let mut transcript = Vec::new();
    let mut content_replacements = Vec::new();
    for entry in entries {
        classify_entry(entry, session_id, &mut transcript, &mut content_replacements);
    }
    (transcript, content_replacements)
}

/// Parses JSONL bytes into transcript entries + content-replacement records.
/// Only keeps entries that have a `uuid` and are transcript message types.
pub(super) fn parse_fork_transcript(content: &[u8], session_id: &str) -> (Vec<Entry>, Vec<Value>) {
    let text = String::from_utf8_lossy(content);
    let entries: Vec<Entry> = text
        .lines()
        .filter_map(|raw| {
            let line = raw.trim();
            if line.is_empty() {
                return None;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(Value::Object(obj)) => Some(obj),
                _ => None,
            }
        })
        .collect();
    partition_transcript_entries(entries, session_id)
}

/// Mirrors the disk path's head/tail title scan over already-parsed entry
/// objects. Precedence: last `customTitle` wins, then last `aiTitle`, then
/// the first user prompt (via [`super::json_extract::extract_first_prompt_from_head`]
/// over a re-serialized JSONL string so skip-patterns/truncation match the
/// disk path exactly).
pub(super) fn derive_title_from_entries(raw: &[Entry]) -> Option<String> {
    let mut custom: Option<String> = None;
    let mut ai: Option<String> = None;
    for e in raw {
        if let Some(ct) = e.get("customTitle").and_then(Value::as_str) {
            if !ct.is_empty() {
                custom = Some(ct.to_string());
            }
        }
        if let Some(at) = e.get("aiTitle").and_then(Value::as_str) {
            if !at.is_empty() {
                ai = Some(at.to_string());
            }
        }
    }
    if custom.is_some() {
        return custom;
    }
    if ai.is_some() {
        return ai;
    }
    let jsonl: String = raw
        .iter()
        .map(|e| serde_json::to_string(&Value::Object(e.clone())).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let prompt = super::json_extract::extract_first_prompt_from_head(&jsonl);
    if prompt.is_empty() {
        None
    } else {
        Some(prompt)
    }
}

/// Core fork transform — remap UUIDs and produce forked transcript entries.
///
/// Returns `(forked_session_id, entries)`. `derive_title` is invoked only
/// when no explicit `title` is given, so the disk path's byte scan and the
/// store path's entry scan only run when needed.
pub(super) fn build_fork_lines(
    transcript: Vec<Entry>,
    content_replacements: Vec<Value>,
    session_id: &str,
    up_to_message_id: Option<&str>,
    title: Option<&str>,
    derive_title: impl FnOnce() -> Option<String>,
) -> Result<(String, Vec<Entry>)> {
    // Filter out sidechains (subagent sessions with separate parentUuid
    // graphs). Keep isMeta entries — they're interleaved in the main chain.
    let mut transcript: Vec<Entry> = transcript.into_iter().filter(|e| !is_truthy(e.get("isSidechain"))).collect();

    if transcript.is_empty() {
        return Err(ClaudeError::InvalidInput(format!("Session {session_id} has no messages to fork")));
    }

    if let Some(up_to) = up_to_message_id {
        let cutoff = transcript.iter().position(|e| e.get("uuid").and_then(Value::as_str) == Some(up_to));
        let Some(cutoff) = cutoff else {
            return Err(ClaudeError::InvalidInput(format!(
                "Message {up_to} not found in session {session_id}"
            )));
        };
        transcript.truncate(cutoff + 1);
    }

    // Include progress entries in the mapping — needed for parentUuid chain walk.
    let mut uuid_mapping: HashMap<String, String> = HashMap::new();
    for entry in &transcript {
        if let Some(u) = entry.get("uuid").and_then(Value::as_str) {
            uuid_mapping.insert(u.to_string(), Uuid::new_v4().to_string());
        }
    }

    // Filter out progress messages from written output. They're UI-only
    // chain links; not needed in a fresh fork.
    let writable: Vec<&Entry> = transcript.iter().filter(|e| e.get("type").and_then(Value::as_str) != Some("progress")).collect();
    if writable.is_empty() {
        return Err(ClaudeError::InvalidInput(format!("Session {session_id} has no messages to fork")));
    }

    let by_uuid: HashMap<&str, &Entry> =
        transcript.iter().filter_map(|e| e.get("uuid").and_then(Value::as_str).map(|u| (u, e))).collect();

    let forked_session_id = Uuid::new_v4().to_string();
    let now = iso_now();
    let mut entries: Vec<Entry> = Vec::with_capacity(writable.len() + 2);

    let last_index = writable.len() - 1;
    for (i, original) in writable.iter().enumerate() {
        let original_uuid = original.get("uuid").and_then(Value::as_str).unwrap_or_default();
        let new_uuid = uuid_mapping.get(original_uuid).cloned().unwrap_or_default();

        // Resolve parentUuid, skipping progress ancestors.
        let mut new_parent_uuid: Option<String> = None;
        let mut parent_id = original.get("parentUuid").and_then(Value::as_str).map(str::to_string);
        while let Some(pid) = parent_id.clone() {
            if pid.is_empty() {
                break;
            }
            let Some(parent) = by_uuid.get(pid.as_str()) else { break };
            if parent.get("type").and_then(Value::as_str) != Some("progress") {
                new_parent_uuid = uuid_mapping.get(&pid).cloned();
                break;
            }
            parent_id = parent.get("parentUuid").and_then(Value::as_str).map(str::to_string);
        }

        // Only update timestamp on the last message (leaf detection on resume).
        let timestamp = if i == last_index {
            now.clone()
        } else {
            original.get("timestamp").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| now.clone())
        };

        // Remap logicalParentUuid (compact-boundary backpointer).
        let logical_parent = original.get("logicalParentUuid").and_then(Value::as_str);
        let new_logical_parent = match logical_parent {
            Some(lp) if !lp.is_empty() => uuid_mapping.get(lp).cloned().map(Value::String).unwrap_or(Value::Null),
            Some(lp) => Value::String(lp.to_string()),
            None => Value::Null,
        };

        let mut forked_from = Map::new();
        forked_from.insert("sessionId".to_string(), Value::String(session_id.to_string()));
        forked_from.insert("messageUuid".to_string(), Value::String(original_uuid.to_string()));

        let mut forked: Entry = (*original).clone();
        forked.insert("uuid".to_string(), Value::String(new_uuid));
        forked.insert("parentUuid".to_string(), new_parent_uuid.map(Value::String).unwrap_or(Value::Null));
        forked.insert("logicalParentUuid".to_string(), new_logical_parent);
        forked.insert("sessionId".to_string(), Value::String(forked_session_id.clone()));
        forked.insert("timestamp".to_string(), Value::String(timestamp));
        forked.insert("isSidechain".to_string(), Value::Bool(false));
        forked.insert("forkedFrom".to_string(), Value::Object(forked_from));
        // Remove fields that would leak state from the source session.
        for key in ["teamName", "agentName", "slug", "sourceToolAssistantUUID"] {
            forked.remove(key);
        }

        entries.push(forked);
    }

    // Append content-replacement entry (if any) with the fork's sessionId.
    if !content_replacements.is_empty() {
        let mut cr = Map::new();
        cr.insert("type".to_string(), Value::String("content-replacement".to_string()));
        cr.insert("sessionId".to_string(), Value::String(forked_session_id.clone()));
        cr.insert("replacements".to_string(), Value::Array(content_replacements));
        cr.insert("uuid".to_string(), Value::String(Uuid::new_v4().to_string()));
        cr.insert("timestamp".to_string(), Value::String(now.clone()));
        entries.push(cr);
    }

    // Derive title: explicit > original customTitle > original aiTitle > first
    // prompt. Suffix with " (fork)" for derived titles. list_sessions reads the
    // LAST custom-title from the tail, so this entry is what surfaces.
    let fork_title = title.map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).unwrap_or_else(|| {
        format!("{} (fork)", derive_title().unwrap_or_else(|| "Forked session".to_string()))
    });

    let mut title_entry = Map::new();
    title_entry.insert("type".to_string(), Value::String("custom-title".to_string()));
    title_entry.insert("sessionId".to_string(), Value::String(forked_session_id.clone()));
    title_entry.insert("customTitle".to_string(), Value::String(fork_title));
    title_entry.insert("uuid".to_string(), Value::String(Uuid::new_v4().to_string()));
    title_entry.insert("timestamp".to_string(), Value::String(now));
    entries.push(title_entry);

    Ok((forked_session_id, entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(json: serde_json::Value) -> Entry {
        json.as_object().unwrap().clone()
    }

    #[test]
    fn build_fork_lines_remaps_uuids_and_parent_chain() {
        let transcript = vec![
            entry(serde_json::json!({"type": "user", "uuid": "u1", "parentUuid": null})),
            entry(serde_json::json!({"type": "assistant", "uuid": "u2", "parentUuid": "u1"})),
        ];
        let (forked_id, entries) =
            build_fork_lines(transcript, vec![], "src", None, Some("My Fork"), || None).unwrap();

        // 2 messages + 1 custom-title entry.
        assert_eq!(entries.len(), 3);
        let msg1 = &entries[0];
        let msg2 = &entries[1];
        assert_ne!(msg1["uuid"].as_str().unwrap(), "u1");
        assert_eq!(msg2["parentUuid"].as_str().unwrap(), msg1["uuid"].as_str().unwrap());
        assert_eq!(msg1["sessionId"].as_str().unwrap(), forked_id);
        assert_eq!(msg1["forkedFrom"]["sessionId"].as_str().unwrap(), "src");
        assert_eq!(msg1["forkedFrom"]["messageUuid"].as_str().unwrap(), "u1");
        assert_eq!(entries[2]["customTitle"].as_str().unwrap(), "My Fork");
    }

    #[test]
    fn build_fork_lines_skips_progress_ancestors_in_parent_chain() {
        let transcript = vec![
            entry(serde_json::json!({"type": "user", "uuid": "u1", "parentUuid": null})),
            entry(serde_json::json!({"type": "progress", "uuid": "p1", "parentUuid": "u1"})),
            entry(serde_json::json!({"type": "assistant", "uuid": "u2", "parentUuid": "p1"})),
        ];
        let (_id, entries) = build_fork_lines(transcript, vec![], "src", None, Some("t"), || None).unwrap();
        // progress entry dropped from output; only user+assistant+title remain.
        assert_eq!(entries.len(), 3);
        let msg1_new_uuid = entries[0]["uuid"].as_str().unwrap();
        let msg2 = &entries[1];
        assert_eq!(msg2["parentUuid"].as_str().unwrap(), msg1_new_uuid);
    }

    #[test]
    fn build_fork_lines_filters_sidechains() {
        let transcript = vec![
            entry(serde_json::json!({"type": "user", "uuid": "u1", "isSidechain": true})),
            entry(serde_json::json!({"type": "user", "uuid": "u2"})),
        ];
        let (_id, entries) = build_fork_lines(transcript, vec![], "src", None, Some("t"), || None).unwrap();
        assert_eq!(entries.len(), 2); // u2 + title, u1 dropped
        assert_eq!(entries[0]["forkedFrom"]["messageUuid"].as_str().unwrap(), "u2");
    }

    #[test]
    fn build_fork_lines_errors_when_all_sidechain() {
        let transcript = vec![entry(serde_json::json!({"type": "user", "uuid": "u1", "isSidechain": true}))];
        let err = build_fork_lines(transcript, vec![], "src", None, None, || None).unwrap_err();
        assert!(err.to_string().contains("no messages to fork"));
    }

    #[test]
    fn build_fork_lines_up_to_message_id_truncates() {
        let transcript = vec![
            entry(serde_json::json!({"type": "user", "uuid": "u1"})),
            entry(serde_json::json!({"type": "assistant", "uuid": "u2"})),
            entry(serde_json::json!({"type": "user", "uuid": "u3"})),
        ];
        let (_id, entries) = build_fork_lines(transcript, vec![], "src", Some("u2"), Some("t"), || None).unwrap();
        // u1 + u2 + title; u3 excluded.
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn build_fork_lines_up_to_message_id_not_found_errors() {
        let transcript = vec![entry(serde_json::json!({"type": "user", "uuid": "u1"}))];
        let err = build_fork_lines(transcript, vec![], "src", Some("missing"), None, || None).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn build_fork_lines_derives_title_when_none_given() {
        let transcript = vec![entry(serde_json::json!({"type": "user", "uuid": "u1"}))];
        let (_id, entries) =
            build_fork_lines(transcript, vec![], "src", None, None, || Some("Original".to_string())).unwrap();
        let title_entry = entries.last().unwrap();
        assert_eq!(title_entry["customTitle"].as_str().unwrap(), "Original (fork)");
    }

    #[test]
    fn build_fork_lines_appends_content_replacement_entry() {
        let transcript = vec![entry(serde_json::json!({"type": "user", "uuid": "u1"}))];
        let replacements = vec![serde_json::json!({"from": "a", "to": "b"})];
        let (forked_id, entries) =
            build_fork_lines(transcript, replacements, "src", None, Some("t"), || None).unwrap();
        // message + content-replacement + title
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1]["type"].as_str().unwrap(), "content-replacement");
        assert_eq!(entries[1]["sessionId"].as_str().unwrap(), forked_id);
    }

    #[test]
    fn partition_transcript_entries_separates_content_replacements() {
        let entries = vec![
            entry(serde_json::json!({"type": "user", "uuid": "u1"})),
            entry(serde_json::json!({
                "type": "content-replacement",
                "sessionId": "src",
                "replacements": [{"a": 1}],
            })),
        ];
        let (transcript, replacements) = partition_transcript_entries(entries, "src");
        assert_eq!(transcript.len(), 1);
        assert_eq!(replacements.len(), 1);
    }

    #[test]
    fn parse_fork_transcript_skips_malformed_lines() {
        let content = b"not json\n{\"type\":\"user\",\"uuid\":\"u1\"}\n";
        let (transcript, _) = parse_fork_transcript(content, "src");
        assert_eq!(transcript.len(), 1);
    }

    #[test]
    fn derive_title_from_entries_prefers_custom_title() {
        let raw = vec![
            entry(serde_json::json!({"type": "user", "aiTitle": "AI Title"})),
            entry(serde_json::json!({"type": "custom-title", "customTitle": "Custom"})),
        ];
        assert_eq!(derive_title_from_entries(&raw), Some("Custom".to_string()));
    }

    #[test]
    fn is_truthy_matches_python_semantics() {
        assert!(!is_truthy(None));
        assert!(!is_truthy(Some(&Value::Bool(false))));
        assert!(!is_truthy(Some(&Value::String(String::new()))));
        assert!(is_truthy(Some(&Value::Bool(true))));
        assert!(is_truthy(Some(&Value::String("x".to_string()))));
    }
}
