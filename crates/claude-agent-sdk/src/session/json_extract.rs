//! Cheap, no-full-parse JSON string field extraction.
//!
//! Ported from `_unescape_json_string`, `_extract_json_string_field`,
//! `_extract_last_json_string_field`, and `_extract_first_prompt_from_head`
//! in upstream `_internal/sessions.py`. These scan raw JSONL text for
//! `"key":"value"` shapes without a full parse — a performance optimization
//! so peeking at one field of a large transcript file doesn't require
//! parsing every line.
//!
//! Byte-offset scanning is safe here even though these operate on
//! (possibly multi-byte UTF-8) `&str`: the only bytes that terminate a scan
//! (`"` and `\`) are ASCII, and ASCII byte values never occur inside a
//! multi-byte UTF-8 encoding of a non-ASCII codepoint, so scanning raw
//! bytes for them can never land mid-character.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

static SKIP_FIRST_PROMPT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:<local-command-stdout>|<session-start-hook>|<tick>|<goal>|\[Request interrupted by user[^\]]*\]|\s*<ide_opened_file>[\s\S]*</ide_opened_file>\s*$|\s*<ide_selection>[\s\S]*</ide_selection>\s*$)",
    )
    .unwrap()
});

static COMMAND_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<command-name>(.*?)</command-name>").unwrap());

/// Unescape a JSON string value extracted as raw text (i.e. the bytes
/// between the surrounding quotes, not yet interpreted).
pub(crate) fn unescape_json_string(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    let quoted = format!("\"{raw}\"");
    serde_json::from_str::<String>(&quoted).unwrap_or_else(|_| raw.to_string())
}

/// Scans forward from byte offset `start` for the first unescaped `"`,
/// treating `\` as escaping the following byte. Returns the byte offset of
/// the closing quote, or `None` if the text ends first.
fn scan_to_unescaped_quote(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Extracts a simple JSON string field value without full parsing.
///
/// Looks for `"key":"` or `"key": "` (in that pattern-priority order — the
/// first pattern that occurs anywhere in `text` wins, not necessarily the
/// earliest byte offset overall). Returns the first match, or `None`.
pub(crate) fn extract_json_string_field(text: &str, key: &str) -> Option<String> {
    let patterns = [format!("\"{key}\":\""), format!("\"{key}\": \"")];
    for pattern in &patterns {
        let Some(idx) = text.find(pattern.as_str()) else { continue };
        let value_start = idx + pattern.len();
        if let Some(end) = scan_to_unescaped_quote(text, value_start) {
            return Some(unescape_json_string(&text[value_start..end]));
        }
    }
    None
}

/// Like [`extract_json_string_field`] but returns the LAST occurrence,
/// scanning all matches of both patterns (spaced-colon pattern's matches
/// are considered after all of the tight-colon pattern's matches, matching
/// upstream's two-pass structure).
pub(crate) fn extract_last_json_string_field(text: &str, key: &str) -> Option<String> {
    let patterns = [format!("\"{key}\":\""), format!("\"{key}\": \"")];
    let mut last_value: Option<String> = None;
    for pattern in &patterns {
        let mut search_from = 0usize;
        while let Some(rel_idx) = text.get(search_from..).and_then(|t| t.find(pattern.as_str())) {
            let idx = search_from + rel_idx;
            let value_start = idx + pattern.len();
            match scan_to_unescaped_quote(text, value_start) {
                Some(end) => {
                    last_value = Some(unescape_json_string(&text[value_start..end]));
                    search_from = end + 1;
                }
                None => break,
            }
        }
    }
    last_value
}

/// Extracts the first meaningful user prompt from a JSONL head chunk.
///
/// Skips `tool_result` messages, `isMeta`, `isCompactSummary`, and
/// auto-generated patterns (IDE context blocks, interruption notices,
/// slash-command invocations). Truncates to 200 chars. Falls back to the
/// first slash-command name if no plain-text prompt is found.
pub(crate) fn extract_first_prompt_from_head(head: &str) -> String {
    let mut command_fallback = String::new();

    for line in head.split('\n') {
        if !line.contains("\"type\":\"user\"") && !line.contains("\"type\": \"user\"") {
            continue;
        }
        if line.contains("\"tool_result\"") {
            continue;
        }
        if line.contains("\"isMeta\":true") || line.contains("\"isMeta\": true") {
            continue;
        }
        if line.contains("\"isCompactSummary\":true") || line.contains("\"isCompactSummary\": true") {
            continue;
        }

        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        let Some(obj) = entry.as_object() else { continue };
        if obj.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(message) = obj.get("message").and_then(Value::as_object) else { continue };

        for raw in texts_from_message(message) {
            let replaced = raw.replace('\n', " ");
            let result = replaced.trim();
            if result.is_empty() {
                continue;
            }

            if let Some(caps) = COMMAND_NAME_RE.captures(result) {
                if command_fallback.is_empty() {
                    command_fallback = caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
                }
                continue;
            }

            if SKIP_FIRST_PROMPT_PATTERN.is_match(result) {
                continue;
            }

            if result.chars().count() > 200 {
                let truncated: String = result.chars().take(200).collect();
                return format!("{}\u{2026}", truncated.trim_end());
            }
            return result.to_string();
        }
    }

    command_fallback
}

fn texts_from_message(message: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut texts = Vec::new();
    match message.get("content") {
        Some(Value::String(s)) => texts.push(s.clone()),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                let Some(b) = block.as_object() else { continue };
                if b.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                if let Some(Value::String(t)) = b.get("text") {
                    texts.push(t.clone());
                }
            }
        }
        _ => {}
    }
    texts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_json_string_passthrough_without_backslash() {
        assert_eq!(unescape_json_string("plain text"), "plain text");
    }

    #[test]
    fn unescape_json_string_handles_escapes() {
        assert_eq!(unescape_json_string("a\\nb"), "a\nb");
        assert_eq!(unescape_json_string("quote:\\\""), "quote:\"");
    }

    #[test]
    fn extract_json_string_field_tight_colon() {
        let text = r#"{"cwd":"/home/user"}"#;
        assert_eq!(extract_json_string_field(text, "cwd"), Some("/home/user".to_string()));
    }

    #[test]
    fn extract_json_string_field_spaced_colon() {
        let text = r#"{"cwd": "/home/user"}"#;
        assert_eq!(extract_json_string_field(text, "cwd"), Some("/home/user".to_string()));
    }

    #[test]
    fn extract_json_string_field_missing() {
        assert_eq!(extract_json_string_field("{}", "cwd"), None);
    }

    #[test]
    fn extract_json_string_field_prefers_tight_pattern_even_if_later() {
        // Spaced pattern occurs first in the text, but the tight-colon
        // pattern is checked first regardless of byte offset.
        let text = r#"{"other": "x", "cwd":"/tight"}"#;
        assert_eq!(extract_json_string_field(text, "cwd"), Some("/tight".to_string()));
    }

    #[test]
    fn extract_last_json_string_field_prefers_last_occurrence() {
        let text = "{\"customTitle\":\"first\"}\n{\"customTitle\":\"second\"}";
        assert_eq!(extract_last_json_string_field(text, "customTitle"), Some("second".to_string()));
    }

    #[test]
    fn extract_last_json_string_field_absent() {
        assert_eq!(extract_last_json_string_field("{}", "customTitle"), None);
    }

    #[test]
    fn extract_last_json_string_field_handles_escaped_quotes() {
        let text = r#"{"tag":"a\"b"}"#;
        assert_eq!(extract_last_json_string_field(text, "tag"), Some("a\"b".to_string()));
    }

    #[test]
    fn extract_first_prompt_from_head_skips_meta_and_tool_result() {
        let head = concat!(
            "{\"type\":\"user\",\"isMeta\":true,\"message\":{\"content\":\"skip me\"}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":\"real prompt\"}}\n",
        );
        assert_eq!(extract_first_prompt_from_head(head), "real prompt");
    }

    #[test]
    fn extract_first_prompt_from_head_truncates_long_prompts() {
        let long = "a".repeat(250);
        let head = format!("{{\"type\":\"user\",\"message\":{{\"content\":\"{long}\"}}}}\n");
        let result = extract_first_prompt_from_head(&head);
        assert_eq!(result.chars().count(), 201);
        assert!(result.ends_with('\u{2026}'));
    }

    #[test]
    fn extract_first_prompt_from_head_falls_back_to_command_name() {
        let head = "{\"type\":\"user\",\"message\":{\"content\":\"<command-name>foo</command-name>\"}}\n";
        assert_eq!(extract_first_prompt_from_head(head), "foo");
    }

    #[test]
    fn extract_first_prompt_from_head_empty_when_nothing_found() {
        assert_eq!(extract_first_prompt_from_head(""), "");
        assert_eq!(extract_first_prompt_from_head("{\"type\":\"assistant\"}\n"), "");
    }

    #[test]
    fn extract_first_prompt_from_head_content_blocks() {
        let head = "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hi there\"}]}}\n";
        assert_eq!(extract_first_prompt_from_head(head), "hi there");
    }

    #[test]
    fn extract_first_prompt_from_head_skips_ide_opened_file() {
        let head = "{\"type\":\"user\",\"message\":{\"content\":\"<ide_opened_file>x</ide_opened_file>\"}}\n";
        assert_eq!(extract_first_prompt_from_head(head), "");
    }
}
