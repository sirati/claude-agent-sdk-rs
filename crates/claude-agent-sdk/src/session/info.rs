//! Session-listing read-model types.
//!
//! Distinct from [`super::types`] (the store's wire format): these are
//! returned by higher-level session-listing/reading APIs (a later slice)
//! built on top of a [`super::store::SessionStore`].

use serde::{Deserialize, Serialize};

/// Session metadata returned by `list_sessions()`.
///
/// Contains only data extractable from stat + head/tail reads — no full
/// JSONL parsing required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SDKSessionInfo {
    /// Unique session identifier (UUID).
    pub session_id: String,
    /// Display title for the session — custom title, auto-generated
    /// summary, or first prompt.
    pub summary: String,
    /// Last modified time in milliseconds since epoch.
    pub last_modified: i64,
    /// Session file size in bytes. Only populated for local JSONL storage;
    /// may be `None` for remote storage backends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
    /// Session title — user-set custom title or AI-generated title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_title: Option<String>,
    /// First meaningful user prompt in the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_prompt: Option<String>,
    /// Git branch at the end of the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// Working directory for the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// User-set session tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Creation time in milliseconds since epoch, extracted from the first
    /// entry's ISO timestamp field. More reliable than a filesystem
    /// birthtime, which is unsupported on some filesystems.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
}

/// Discriminant for [`SessionMessage`], mirroring the SDK wire protocol's
/// user/assistant message types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionMessageType {
    /// A user-authored message.
    User,
    /// An assistant-authored message.
    Assistant,
}

/// A user or assistant message from a session transcript.
///
/// Returned by `get_session_messages()` for reading historical session
/// data. Fields match the SDK wire protocol types (`SDKUserMessage` /
/// `SDKAssistantMessage`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMessage {
    /// Message type — user or assistant.
    #[serde(rename = "type")]
    pub type_: SessionMessageType,
    /// Unique message identifier.
    pub uuid: String,
    /// ID of the session this message belongs to.
    pub session_id: String,
    /// Raw Anthropic API message (role, content, etc.).
    pub message: serde_json::Value,
    /// Always `None` for top-level conversation messages (tool-use
    /// sidechain messages are filtered out before reaching this type).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_message_type_serde() {
        assert_eq!(
            serde_json::to_string(&SessionMessageType::User).unwrap(),
            "\"user\""
        );
        assert_eq!(
            serde_json::to_string(&SessionMessageType::Assistant).unwrap(),
            "\"assistant\""
        );
    }

    #[test]
    fn sdk_session_info_optional_fields_omitted() {
        let info = SDKSessionInfo {
            session_id: "s".to_string(),
            summary: "hi".to_string(),
            last_modified: 1_700_000_000_000,
            file_size: None,
            custom_title: None,
            first_prompt: None,
            git_branch: None,
            cwd: None,
            tag: None,
            created_at: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert!(json.get("file_size").is_none());
        assert!(json.get("custom_title").is_none());
    }
}
