//! Bridging lite session reads to [`SDKSessionInfo`], and JSONL <-> lite
//! conversions for the store-backed read path.
//!
//! Ported from `_parse_session_info_from_lite`, `_entries_to_jsonl`,
//! `_jsonl_to_lite`, and `_mtime_from_jsonl_tail` in upstream
//! `_internal/sessions.py`.

use serde_json::Value;

use super::info::SDKSessionInfo;
use super::json_extract::{extract_first_prompt_from_head, extract_json_string_field, extract_last_json_string_field};
use super::local::{LiteSessionFile, LITE_READ_BUF_SIZE};

/// Parses [`SDKSessionInfo`] fields from a lite session read (head/tail/stat).
///
/// Returns `None` for sidechain sessions or metadata-only sessions with no
/// extractable summary. Shared by `list_sessions` and `get_session_info`
/// (a later slice built on top of this one).
pub(crate) fn parse_session_info_from_lite(
    session_id: &str,
    lite: &LiteSessionFile,
    project_path: Option<&str>,
) -> Option<SDKSessionInfo> {
    let LiteSessionFile { head, tail, mtime, size } = lite;

    // Check first line for sidechain sessions.
    let first_line = match head.find('\n') {
        Some(idx) => &head[..idx],
        None => head.as_str(),
    };
    if first_line.contains("\"isSidechain\":true") || first_line.contains("\"isSidechain\": true") {
        return None;
    }

    // User-set title (customTitle) wins over AI-generated title (aiTitle).
    // Head fallback covers short sessions where the title entry may not be
    // in tail.
    let custom_title = extract_last_json_string_field(tail, "customTitle")
        .or_else(|| extract_last_json_string_field(head, "customTitle"))
        .or_else(|| extract_last_json_string_field(tail, "aiTitle"))
        .or_else(|| extract_last_json_string_field(head, "aiTitle"));

    let first_prompt = {
        let prompt = extract_first_prompt_from_head(head);
        if prompt.is_empty() { None } else { Some(prompt) }
    };

    // lastPrompt tail entry shows what the user was most recently doing.
    let summary = custom_title
        .clone()
        .or_else(|| extract_last_json_string_field(tail, "lastPrompt"))
        .or_else(|| extract_last_json_string_field(tail, "summary"))
        .or_else(|| first_prompt.clone());

    // Skip metadata-only sessions (no title, no summary, no prompt).
    let summary = summary?;

    let git_branch = extract_last_json_string_field(tail, "gitBranch").or_else(|| extract_json_string_field(head, "gitBranch"));
    let session_cwd = extract_json_string_field(head, "cwd").or_else(|| project_path.map(str::to_string));

    // Scope tag extraction to `{"type":"tag"}` lines — a bare tail scan for
    // "tag" would match tool_use inputs (git tag, Docker tags, cloud
    // resource tags).
    let tag_line = tail.split('\n').rev().find(|ln| ln.starts_with("{\"type\":\"tag\""));
    let tag = tag_line.and_then(|ln| extract_last_json_string_field(ln, "tag"));

    // created_at from the first ISO timestamp found in the head (epoch ms).
    // More reliable than stat().birthtime, which is unsupported on some
    // filesystems. Scans the whole head rather than only the first line
    // because the first record may be a metadata-only entry (e.g.
    // permission-mode) with no timestamp field.
    let created_at = extract_json_string_field(head, "timestamp")
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(&ts).ok())
        .map(|dt| dt.timestamp_millis());

    Some(SDKSessionInfo {
        session_id: session_id.to_string(),
        summary,
        last_modified: *mtime,
        file_size: Some(*size),
        custom_title,
        first_prompt,
        git_branch,
        cwd: session_cwd,
        tag,
        created_at,
    })
}

/// Serializes store entries to a JSONL string (one compact JSON object per
/// line, `\n`-terminated).
///
/// The `SessionStore::load` contract permits adapters to reorder object
/// keys (e.g. Postgres JSONB), but [`parse_session_info_from_lite`] scans
/// for `{"type":"tag"` as a line prefix. Hoist `type` to the front so the
/// store path matches the byte shape the disk path produces.
pub(crate) fn entries_to_jsonl(entries: &[Value]) -> String {
    let mut out = String::new();
    for entry in entries {
        out.push_str(&hoist_type_first(entry));
        out.push('\n');
    }
    out
}

fn hoist_type_first(value: &Value) -> String {
    let Some(obj) = value.as_object().filter(|o| o.contains_key("type")) else {
        return serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
    };
    let mut out = String::from("{\"type\":");
    out.push_str(&serde_json::to_string(&obj["type"]).unwrap_or_else(|_| "null".to_string()));
    for (key, val) in obj {
        if key == "type" {
            continue;
        }
        out.push(',');
        out.push_str(&serde_json::to_string(key).unwrap_or_default());
        out.push(':');
        out.push_str(&serde_json::to_string(val).unwrap_or_else(|_| "null".to_string()));
    }
    out.push('}');
    out
}

/// Builds the head/tail/size lite shape from an in-memory JSONL string.
///
/// Matches [`super::local::read_session_lite`]'s byte semantics so the
/// store path exposes the same slice to [`parse_session_info_from_lite`] as
/// the disk path would for the same transcript.
pub(crate) fn jsonl_to_lite(jsonl: &str, mtime: i64) -> LiteSessionFile {
    let buf = jsonl.as_bytes();
    let size = buf.len();
    let head_end = size.min(LITE_READ_BUF_SIZE);
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let tail = if size > LITE_READ_BUF_SIZE {
        let tail_start = size - LITE_READ_BUF_SIZE;
        String::from_utf8_lossy(&buf[tail_start..]).into_owned()
    } else {
        head.clone()
    };
    LiteSessionFile { mtime, size: size as u64, head, tail }
}

/// Best-effort mtime: parses the last entry's `timestamp` field.
///
/// Falls back to the current wall-clock time when absent or unparseable.
pub(crate) fn mtime_from_jsonl_tail(jsonl: &str) -> i64 {
    let trimmed = jsonl.trim_end();
    let last_line = match trimmed.rfind('\n') {
        Some(idx) => &trimmed[idx + 1..],
        None => trimmed,
    };
    if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(last_line) {
        if let Some(Value::String(ts)) = obj.get("timestamp") {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
                return dt.timestamp_millis();
            }
        }
    }
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_session_info_skips_sidechain() {
        let lite = LiteSessionFile {
            mtime: 1_700_000_000_000,
            size: 10,
            head: "{\"isSidechain\":true,\"type\":\"user\"}\n".to_string(),
            tail: "{\"isSidechain\":true,\"type\":\"user\"}\n".to_string(),
        };
        assert!(parse_session_info_from_lite("id", &lite, None).is_none());
    }

    #[test]
    fn parse_session_info_skips_metadata_only() {
        let lite = LiteSessionFile { mtime: 0, size: 2, head: "{}\n".to_string(), tail: "{}\n".to_string() };
        assert!(parse_session_info_from_lite("id", &lite, None).is_none());
    }

    #[test]
    fn parse_session_info_prefers_custom_title() {
        let head = "{\"type\":\"user\",\"message\":{\"content\":\"first prompt\"}}\n".to_string();
        let tail = "{\"type\":\"custom-title\",\"customTitle\":\"My Title\"}\n".to_string();
        let lite = LiteSessionFile { mtime: 123, size: 9, head, tail };
        let info = parse_session_info_from_lite("sess-1", &lite, None).unwrap();
        assert_eq!(info.summary, "My Title");
        assert_eq!(info.custom_title.as_deref(), Some("My Title"));
        assert_eq!(info.first_prompt.as_deref(), Some("first prompt"));
    }

    #[test]
    fn parse_session_info_extracts_tag_scoped_to_tag_type() {
        let head = "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n".to_string();
        let tail = "{\"type\":\"tag\",\"tag\":\"important\"}\n".to_string();
        let lite = LiteSessionFile { mtime: 0, size: 1, head, tail };
        let info = parse_session_info_from_lite("sess-2", &lite, None).unwrap();
        assert_eq!(info.tag.as_deref(), Some("important"));
    }

    #[test]
    fn entries_to_jsonl_hoists_type_first() {
        let entries = vec![serde_json::json!({"sessionId": "s", "type": "tag", "tag": "x"})];
        let jsonl = entries_to_jsonl(&entries);
        assert!(jsonl.starts_with("{\"type\":\"tag\""));
        assert!(jsonl.ends_with('\n'));
        // Round-trips to the same logical object.
        let parsed: Value = serde_json::from_str(jsonl.trim_end()).unwrap();
        assert_eq!(parsed["tag"], "x");
        assert_eq!(parsed["sessionId"], "s");
    }

    #[test]
    fn entries_to_jsonl_passthrough_without_type() {
        let entries = vec![serde_json::json!({"foo": "bar"})];
        let jsonl = entries_to_jsonl(&entries);
        assert_eq!(jsonl.trim_end(), "{\"foo\":\"bar\"}");
    }

    #[test]
    fn jsonl_to_lite_small_matches_head_and_tail() {
        let jsonl = "{\"a\":1}\n";
        let lite = jsonl_to_lite(jsonl, 42);
        assert_eq!(lite.head, jsonl);
        assert_eq!(lite.tail, jsonl);
        assert_eq!(lite.mtime, 42);
    }

    #[test]
    fn jsonl_to_lite_large_splits_head_tail() {
        let mut jsonl = "a".repeat(LITE_READ_BUF_SIZE + 50);
        jsonl.push_str("TAIL");
        let lite = jsonl_to_lite(&jsonl, 0);
        assert!(lite.tail.ends_with("TAIL"));
        assert_ne!(lite.head, lite.tail);
    }

    #[test]
    fn mtime_from_jsonl_tail_parses_last_timestamp() {
        let jsonl = "{\"timestamp\":\"2024-01-01T00:00:00.000Z\"}\n{\"timestamp\":\"2024-06-01T00:00:00.000Z\"}\n";
        assert_eq!(mtime_from_jsonl_tail(jsonl), 1_717_200_000_000);
    }

    #[test]
    fn mtime_from_jsonl_tail_falls_back_when_unparseable() {
        let now_before = chrono::Utc::now().timestamp_millis();
        let mtime = mtime_from_jsonl_tail("not json");
        assert!(mtime >= now_before);
    }
}
