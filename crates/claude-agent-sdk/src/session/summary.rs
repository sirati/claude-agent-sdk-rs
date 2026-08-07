//! Incremental session-summary derivation for [`super::store::SessionStore`]
//! adapters.
//!
//! [`fold_session_summary`] lets a store maintain a per-session
//! [`SessionSummaryEntry`] sidecar incrementally inside `append()` so
//! `list_session_summaries()` can fetch all metadata in one call instead of
//! N per-session `load()` calls. Every derived field is append-incremental
//! (set-once or last-wins) so adapters never need to re-read previously
//! appended entries.
//!
//! Ported from upstream's `_internal/session_summary.py`.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use super::info::SDKSessionInfo;
use super::types::{SessionKey, SessionStoreEntry, SessionSummaryEntry};

/// Matches `<command-name>...</command-name>` wrapper text emitted for
/// slash-command invocations.
static COMMAND_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<command-name>(.*?)</command-name>").unwrap());

/// Auto-generated / system message patterns to skip when hunting for the
/// first meaningful user prompt.
static SKIP_FIRST_PROMPT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:<local-command-stdout>|<session-start-hook>|<tick>|<goal>|\[Request interrupted by user[^\]]*\]|\s*<ide_opened_file>[\s\S]*</ide_opened_file>\s*$|\s*<ide_selection>[\s\S]*</ide_selection>\s*$)",
    )
    .unwrap()
});

/// Map of JSONL entry keys -> `SessionSummaryEntry` `data` keys for
/// last-wins string fields. Each appended entry overwrites the previous
/// value when present.
const LAST_WINS_FIELDS: &[(&str, &str)] = &[
    ("customTitle", "custom_title"),
    ("aiTitle", "ai_title"),
    ("lastPrompt", "last_prompt"),
    ("summary", "summary_hint"),
    ("gitBranch", "git_branch"),
];

/// Parse an ISO-8601 timestamp string to Unix epoch milliseconds.
fn iso_to_epoch_ms(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// Extract text strings from a `type == "user"` entry's message content.
fn entry_text_blocks(entry: &SessionStoreEntry) -> Vec<String> {
    let mut texts = Vec::new();
    let Some(message) = entry.extra.get("message").and_then(Value::as_object) else {
        return texts;
    };
    match message.get("content") {
        Some(Value::String(s)) => texts.push(s.clone()),
        Some(Value::Array(items)) => {
            for block in items {
                if let Some(obj) = block.as_object()
                    && obj.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(text) = obj.get("text").and_then(Value::as_str)
                {
                    texts.push(text.to_string());
                }
            }
        }
        _ => {}
    }
    texts
}

/// Whether a `type == "user"` entry's message content carries a
/// `tool_result` block (these are skipped when hunting for the first
/// prompt).
fn has_tool_result_content(entry: &SessionStoreEntry) -> bool {
    let Some(content) = entry
        .extra
        .get("message")
        .and_then(Value::as_object)
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    content.iter().any(|b| {
        b.as_object().and_then(|o| o.get("type")).and_then(Value::as_str) == Some("tool_result")
    })
}

/// Replicate `_extract_first_prompt_from_head` for a single parsed entry.
///
/// Mutates `data` in place: sets `first_prompt` + `first_prompt_locked` on a
/// real match, or stashes a `command_fallback` for slash-command messages.
/// Skips tool_result, isMeta, isCompactSummary, and auto-generated patterns.
fn fold_first_prompt(data: &mut Map<String, Value>, entry: &SessionStoreEntry) {
    if matches!(data.get("first_prompt_locked"), Some(Value::Bool(true))) {
        return;
    }
    if entry.type_ != "user" {
        return;
    }
    if matches!(entry.extra.get("isMeta"), Some(Value::Bool(true))) {
        return;
    }
    if matches!(entry.extra.get("isCompactSummary"), Some(Value::Bool(true))) {
        return;
    }
    if has_tool_result_content(entry) {
        return;
    }

    for raw in entry_text_blocks(entry) {
        let collapsed = raw.replace('\n', " ");
        let result = collapsed.trim();
        if result.is_empty() {
            continue;
        }

        if let Some(caps) = COMMAND_NAME_RE.captures(result) {
            let has_fallback = data
                .get("command_fallback")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty());
            if !has_fallback {
                let name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                data.insert("command_fallback".to_string(), json!(name));
            }
            continue;
        }

        if SKIP_FIRST_PROMPT_PATTERN.is_match(result) {
            continue;
        }

        let truncated = if result.chars().count() > 200 {
            let mut s: String = result.chars().take(200).collect();
            while s.ends_with(char::is_whitespace) {
                s.pop();
            }
            s.push('\u{2026}');
            s
        } else {
            result.to_string()
        };
        data.insert("first_prompt".to_string(), json!(truncated));
        data.insert("first_prompt_locked".to_string(), json!(true));
        return;
    }
}

/// Fold a batch of appended entries into the running summary for `key`.
///
/// Stores call this from inside `append()` to keep a [`SessionSummaryEntry`]
/// sidecar up to date without re-reading the transcript. `prev` is the
/// previous summary for the same key (or `None` for the first append).
///
/// Do not call this for keys with a `subpath` — subagent transcripts must
/// not contribute to the main session's summary. Guard with
/// `key.subpath.is_none()` before calling.
///
/// All derived state lives in the opaque `data` object; stores persist it
/// verbatim and do not interpret it.
///
/// `mtime` is NOT touched by the fold — it is the sidecar's storage write
/// time and must be stamped by the adapter after persisting (see
/// [`SessionSummaryEntry::mtime`]). For a new session (`prev` is `None`) the
/// fold returns `mtime: 0` as a placeholder; the adapter is expected to
/// overwrite it.
pub fn fold_session_summary(
    prev: Option<&SessionSummaryEntry>,
    key: &SessionKey,
    entries: &[SessionStoreEntry],
) -> SessionSummaryEntry {
    let mut summary = match prev {
        Some(p) => SessionSummaryEntry {
            session_id: p.session_id.clone(),
            mtime: p.mtime,
            data: p.data.clone(),
        },
        None => SessionSummaryEntry {
            session_id: key.session_id.clone(),
            mtime: 0,
            data: Value::Object(Map::new()),
        },
    };
    if !summary.data.is_object() {
        summary.data = Value::Object(Map::new());
    }
    // Safe: forced to Object above.
    let data = summary.data.as_object_mut().unwrap();

    for entry in entries {
        let ms = entry.timestamp().and_then(iso_to_epoch_ms);

        if !data.contains_key("is_sidechain") {
            let is_sidechain = matches!(entry.extra.get("isSidechain"), Some(Value::Bool(true)));
            data.insert("is_sidechain".to_string(), json!(is_sidechain));
        }
        if !data.contains_key("created_at")
            && let Some(ms) = ms
        {
            data.insert("created_at".to_string(), json!(ms));
        }
        if !data.contains_key("cwd")
            && let Some(cwd) = entry.extra.get("cwd").and_then(Value::as_str)
            && !cwd.is_empty()
        {
            data.insert("cwd".to_string(), json!(cwd));
        }

        fold_first_prompt(data, entry);

        for (src, dst) in LAST_WINS_FIELDS {
            if let Some(val) = entry.extra.get(*src).and_then(Value::as_str) {
                data.insert((*dst).to_string(), json!(val));
            }
        }

        if entry.type_ == "tag" {
            match entry.extra.get("tag").and_then(Value::as_str) {
                Some(tag) if !tag.is_empty() => {
                    data.insert("tag".to_string(), json!(tag));
                }
                _ => {
                    // Empty string or absent tag clears the tag.
                    data.remove("tag");
                }
            }
        }
    }

    summary
}

fn non_empty_string(data: &Map<String, Value>, key: &str) -> Option<String> {
    data.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Convert a [`SessionSummaryEntry`] to [`SDKSessionInfo`].
///
/// Returns `None` for sidechain sessions or sessions with no extractable
/// summary, matching `_parse_session_info_from_lite`'s filtering.
pub fn summary_entry_to_sdk_info(
    entry: &SessionSummaryEntry,
    project_path: Option<&str>,
) -> Option<SDKSessionInfo> {
    let data = entry.data.as_object()?;
    if matches!(data.get("is_sidechain"), Some(Value::Bool(true))) {
        return None;
    }

    let first_prompt_locked = matches!(data.get("first_prompt_locked"), Some(Value::Bool(true)));
    let first_prompt = if first_prompt_locked {
        non_empty_string(data, "first_prompt")
    } else {
        non_empty_string(data, "command_fallback")
    };

    let custom_title =
        non_empty_string(data, "custom_title").or_else(|| non_empty_string(data, "ai_title"));

    let summary = custom_title
        .clone()
        .or_else(|| non_empty_string(data, "last_prompt"))
        .or_else(|| non_empty_string(data, "summary_hint"))
        .or_else(|| first_prompt.clone())?;

    Some(SDKSessionInfo {
        session_id: entry.session_id.clone(),
        summary,
        last_modified: entry.mtime,
        // file_size is a JSONL byte count — meaningful only for the
        // local-disk path. Stores have no equivalent.
        file_size: None,
        custom_title,
        first_prompt,
        git_branch: non_empty_string(data, "git_branch"),
        cwd: non_empty_string(data, "cwd").or_else(|| project_path.map(str::to_string)),
        tag: non_empty_string(data, "tag"),
        created_at: data.get("created_at").and_then(Value::as_i64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(type_: &str, extra: Value) -> SessionStoreEntry {
        SessionStoreEntry {
            type_: type_.to_string(),
            extra: extra.as_object().cloned().unwrap_or_default(),
        }
    }

    fn key() -> SessionKey {
        SessionKey::new("proj", "sess-1")
    }

    #[test]
    fn first_append_starts_from_empty_summary() {
        let summary = fold_session_summary(None, &key(), &[]);
        assert_eq!(summary.session_id, "sess-1");
        assert_eq!(summary.mtime, 0);
        assert_eq!(summary.data, json!({}));
    }

    #[test]
    fn extracts_first_prompt_and_locks_it() {
        let e = entry(
            "user",
            json!({
                "message": {"role": "user", "content": "Hello there, Claude!"},
                "timestamp": "2024-01-01T00:00:00.000Z",
            }),
        );
        let summary = fold_session_summary(None, &key(), std::slice::from_ref(&e));
        assert_eq!(summary.data["first_prompt"], "Hello there, Claude!");
        assert_eq!(summary.data["first_prompt_locked"], true);
        assert_eq!(summary.data["created_at"], 1_704_067_200_000i64);

        // A second entry must not overwrite the locked first prompt.
        let e2 = entry(
            "user",
            json!({"message": {"role": "user", "content": "Second message"}}),
        );
        let summary2 = fold_session_summary(Some(&summary), &key(), &[e2]);
        assert_eq!(summary2.data["first_prompt"], "Hello there, Claude!");
    }

    #[test]
    fn skips_tool_result_and_meta_entries() {
        let tool_result = entry(
            "user",
            json!({
                "message": {"content": [{"type": "tool_result", "content": "ok"}]},
            }),
        );
        let meta = entry(
            "user",
            json!({"isMeta": true, "message": {"content": "meta text"}}),
        );
        let real = entry(
            "user",
            json!({"message": {"content": "real prompt"}}),
        );
        let summary = fold_session_summary(None, &key(), &[tool_result, meta, real]);
        assert_eq!(summary.data["first_prompt"], "real prompt");
    }

    #[test]
    fn command_fallback_used_when_no_real_prompt() {
        let e = entry(
            "user",
            json!({"message": {"content": "<command-name>review</command-name>"}}),
        );
        let summary = fold_session_summary(None, &key(), std::slice::from_ref(&e));
        assert!(summary.data.get("first_prompt").is_none());
        assert_eq!(summary.data["command_fallback"], "review");

        let info = summary_entry_to_sdk_info(&summary, None).unwrap();
        assert_eq!(info.first_prompt.as_deref(), Some("review"));
    }

    #[test]
    fn last_wins_fields_overwrite_across_entries() {
        let e1 = entry("user", json!({"customTitle": "First title"}));
        let e2 = entry("user", json!({"customTitle": "Second title"}));
        let summary = fold_session_summary(None, &key(), &[e1, e2]);
        assert_eq!(summary.data["custom_title"], "Second title");
    }

    #[test]
    fn tag_entry_sets_and_clears_tag() {
        let set_tag = entry("tag", json!({"tag": "important"}));
        let summary = fold_session_summary(None, &key(), std::slice::from_ref(&set_tag));
        assert_eq!(summary.data["tag"], "important");

        let clear_tag = entry("tag", json!({"tag": ""}));
        let summary2 = fold_session_summary(Some(&summary), &key(), &[clear_tag]);
        assert!(summary2.data.get("tag").is_none());
    }

    #[test]
    fn sidechain_summary_has_no_sdk_info() {
        let e = entry(
            "user",
            json!({"isSidechain": true, "message": {"content": "hi"}}),
        );
        let summary = fold_session_summary(None, &key(), std::slice::from_ref(&e));
        assert!(summary_entry_to_sdk_info(&summary, None).is_none());
    }

    #[test]
    fn empty_summary_has_no_sdk_info() {
        let summary = fold_session_summary(None, &key(), &[]);
        assert!(summary_entry_to_sdk_info(&summary, None).is_none());
    }

    #[test]
    fn sdk_info_falls_back_to_project_path_for_cwd() {
        let e = entry("user", json!({"message": {"content": "hi there"}}));
        let summary = fold_session_summary(None, &key(), std::slice::from_ref(&e));
        let info = summary_entry_to_sdk_info(&summary, Some("/home/user/proj")).unwrap();
        assert_eq!(info.cwd.as_deref(), Some("/home/user/proj"));
    }

    #[test]
    fn long_first_prompt_is_truncated() {
        let long = "a".repeat(250);
        let e = entry("user", json!({"message": {"content": long}}));
        let summary = fold_session_summary(None, &key(), std::slice::from_ref(&e));
        let fp = summary.data["first_prompt"].as_str().unwrap();
        assert_eq!(fp.chars().count(), 201); // 200 chars + ellipsis
        assert!(fp.ends_with('\u{2026}'));
    }
}
