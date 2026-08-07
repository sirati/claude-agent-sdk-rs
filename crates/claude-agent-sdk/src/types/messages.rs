//! Message types for Claude Agent SDK

use serde::{Deserialize, Serialize};

/// Supported image MIME types for Claude API
const SUPPORTED_IMAGE_MIME_TYPES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];

/// Maximum base64 data size (15MB results in ~20MB decoded, within Claude's limits)
const MAX_BASE64_SIZE: usize = 15_728_640;

/// Error types for assistant messages
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AssistantMessageError {
    /// Authentication failed
    AuthenticationFailed,
    /// Billing error
    BillingError,
    /// Rate limit exceeded
    RateLimit,
    /// Invalid request
    InvalidRequest,
    /// Server error
    ServerError,
    /// Unknown error
    Unknown,
}

/// Main message enum containing all message types from CLI
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Message {
    /// Assistant message
    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),
    /// System message
    #[serde(rename = "system")]
    System(SystemMessage),
    /// Result message
    #[serde(rename = "result")]
    Result(ResultMessage),
    /// Stream event
    #[serde(rename = "stream_event")]
    StreamEvent(StreamEvent),
    /// User message (rarely used in stream output)
    #[serde(rename = "user")]
    User(UserMessage),
    /// Control cancel request (ignore this - it's internal control protocol)
    #[serde(rename = "control_cancel_request")]
    ControlCancelRequest(serde_json::Value),
    /// Rate limit status changed (emitted for claude.ai subscription users)
    #[serde(rename = "rate_limit_event")]
    RateLimitEvent(RateLimitEvent),
    /// Any message `type` this SDK version doesn't recognize yet.
    ///
    /// The CLI is forward-compatible: it may start emitting new message
    /// types before the SDK is updated to understand them. Rather than
    /// failing the whole stream (as older versions of this parser did,
    /// which crashed on every query once the CLI began emitting
    /// `rate_limit_event`), unrecognized types are captured here so
    /// callers can skip them. Mirrors the Python SDK's `parse_message`
    /// returning `None` for unknown types.
    #[serde(other)]
    Unknown,
}

/// User message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    /// Message text
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Message content blocks
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ContentBlock>>,
    /// UUID for file checkpointing (used with enable_file_checkpointing and rewind_files)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Parent tool use ID (if this is a tool result)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
    /// Additional fields
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

/// Message content can be text or blocks
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// Simple text content
    Text { text: String },
    /// Structured content blocks
    Blocks { content: Vec<ContentBlock> },
}

impl From<String> for MessageContent {
    fn from(text: String) -> Self {
        MessageContent::Text { text }
    }
}

impl From<&str> for MessageContent {
    fn from(text: &str) -> Self {
        MessageContent::Text {
            text: text.to_string(),
        }
    }
}

impl From<Vec<ContentBlock>> for MessageContent {
    fn from(blocks: Vec<ContentBlock>) -> Self {
        MessageContent::Blocks { content: blocks }
    }
}

/// Assistant message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// The actual message content (wrapped)
    pub message: AssistantMessageInner,
    /// Parent tool use ID (if applicable)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
    /// Session ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// UUID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
}

/// Inner assistant message content
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessageInner {
    /// Message content blocks
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    /// Model used
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Message ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Stop reason
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Usage statistics
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<serde_json::Value>,
    /// Error type (if any)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AssistantMessageError>,
}

/// System message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMessage {
    /// Message subtype
    pub subtype: String,
    /// Current working directory
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Session ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Available tools
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// MCP servers
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Vec<serde_json::Value>>,
    /// Model being used
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Permission mode
    #[serde(skip_serializing_if = "Option::is_none", rename = "permissionMode")]
    pub permission_mode: Option<String>,
    /// UUID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Additional data
    #[serde(flatten)]
    pub data: serde_json::Value,
}

impl SystemMessage {
    /// Classify this generic system message into a more specific subtype
    /// based on `subtype`, mirroring the CLI's `system`/`subtype` routing.
    ///
    /// Falls back to [`SystemMessageKind::Generic`] when the subtype is
    /// unrecognized, or when a subtype that requires fields this SDK
    /// considers mandatory is missing them — classification never fails
    /// the surrounding message parse, it just declines to specialize.
    pub fn classify(&self) -> SystemMessageKind {
        match self.subtype.as_str() {
            "task_started" => self
                .extract_task_started()
                .map(SystemMessageKind::TaskStarted)
                .unwrap_or(SystemMessageKind::Generic),
            "task_progress" => self
                .extract_task_progress()
                .map(SystemMessageKind::TaskProgress)
                .unwrap_or(SystemMessageKind::Generic),
            "task_notification" => self
                .extract_task_notification()
                .map(SystemMessageKind::TaskNotification)
                .unwrap_or(SystemMessageKind::Generic),
            "task_updated" => SystemMessageKind::TaskUpdated(self.extract_task_updated()),
            "mirror_error" => SystemMessageKind::MirrorError(self.extract_mirror_error()),
            "hook_started" | "hook_response" => {
                SystemMessageKind::HookEvent(self.extract_hook_event())
            }
            _ => SystemMessageKind::Generic,
        }
    }

    fn extract_task_started(&self) -> Option<TaskStartedMessage> {
        Some(TaskStartedMessage {
            task_id: self.data.get("task_id")?.as_str()?.to_string(),
            description: self.data.get("description")?.as_str()?.to_string(),
            uuid: self.uuid.clone()?,
            session_id: self.session_id.clone()?,
            tool_use_id: str_field(&self.data, "tool_use_id"),
            task_type: str_field(&self.data, "task_type"),
        })
    }

    fn extract_task_progress(&self) -> Option<TaskProgressMessage> {
        Some(TaskProgressMessage {
            task_id: self.data.get("task_id")?.as_str()?.to_string(),
            description: self.data.get("description")?.as_str()?.to_string(),
            usage: serde_json::from_value(self.data.get("usage")?.clone()).ok()?,
            uuid: self.uuid.clone()?,
            session_id: self.session_id.clone()?,
            tool_use_id: str_field(&self.data, "tool_use_id"),
            last_tool_name: str_field(&self.data, "last_tool_name"),
        })
    }

    fn extract_task_notification(&self) -> Option<TaskNotificationMessage> {
        Some(TaskNotificationMessage {
            task_id: self.data.get("task_id")?.as_str()?.to_string(),
            status: self.data.get("status")?.as_str()?.to_string(),
            output_file: self.data.get("output_file")?.as_str()?.to_string(),
            summary: self.data.get("summary")?.as_str()?.to_string(),
            uuid: self.uuid.clone()?,
            session_id: self.session_id.clone()?,
            tool_use_id: str_field(&self.data, "tool_use_id"),
            usage: self
                .data
                .get("usage")
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
        })
    }

    fn extract_task_updated(&self) -> TaskUpdatedMessage {
        // Parsed defensively: the patch may omit uuid/session_id and
        // parsing must never fail on a lifecycle event.
        let patch = self
            .data
            .get("patch")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let status = patch.get("status").and_then(|v| v.as_str()).map(String::from);
        TaskUpdatedMessage {
            task_id: str_field(&self.data, "task_id").unwrap_or_default(),
            patch,
            status,
            session_id: self.session_id.clone(),
            uuid: self.uuid.clone(),
        }
    }

    fn extract_mirror_error(&self) -> MirrorErrorMessage {
        MirrorErrorMessage {
            key: self.data.get("key").cloned(),
            error: str_field(&self.data, "error").unwrap_or_default(),
        }
    }

    fn extract_hook_event(&self) -> HookEventMessage {
        // Fallback chain matches the CLI's own inconsistent naming across
        // versions: hook_event, then hook_name, then hook_event_name.
        let hook_event_name = str_field(&self.data, "hook_event")
            .or_else(|| str_field(&self.data, "hook_name"))
            .or_else(|| str_field(&self.data, "hook_event_name"))
            .unwrap_or_default();
        HookEventMessage {
            hook_event_name,
            session_id: self.session_id.clone(),
            uuid: self.uuid.clone(),
        }
    }
}

/// Read a string field out of a `serde_json::Value` object, if present.
fn str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key).and_then(|v| v.as_str()).map(String::from)
}

/// Specific system-message subtype, obtained via [`SystemMessage::classify`].
///
/// The Python SDK models these as `SystemMessage` subclasses; Rust has no
/// inheritance, so each specific type carries only its own fields and is
/// reached by classifying the always-present [`SystemMessage`] rather than
/// replacing it in the [`Message`] enum. `Message::System` continues to
/// carry the raw `SystemMessage` regardless of subtype.
#[derive(Debug, Clone)]
pub enum SystemMessageKind {
    /// A background task started (`subtype: "task_started"`)
    TaskStarted(TaskStartedMessage),
    /// A background task reported progress (`subtype: "task_progress"`)
    TaskProgress(TaskProgressMessage),
    /// A background task completed, failed, or was stopped
    /// (`subtype: "task_notification"`)
    TaskNotification(TaskNotificationMessage),
    /// A background task's state changed (`subtype: "task_updated"`)
    TaskUpdated(TaskUpdatedMessage),
    /// A `SessionStore.append` call failed, non-fatally
    /// (`subtype: "mirror_error"`)
    MirrorError(MirrorErrorMessage),
    /// A hook lifecycle event (`subtype: "hook_started"` / `"hook_response"`)
    HookEvent(HookEventMessage),
    /// Any other subtype (e.g. "init"); use the base [`SystemMessage`] fields.
    Generic,
}

/// Usage statistics reported in `task_progress` and `task_notification`
/// messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskUsage {
    /// Total tokens consumed by the task so far
    pub total_tokens: u64,
    /// Number of tool calls made by the task so far
    pub tool_uses: u64,
    /// Task duration so far, in milliseconds
    pub duration_ms: u64,
}

/// System message emitted when a background task starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStartedMessage {
    /// ID of the started task
    pub task_id: String,
    /// Human-readable description of the task
    pub description: String,
    /// Unique ID of this event
    pub uuid: String,
    /// Session ID the task belongs to
    pub session_id: String,
    /// Tool use ID that spawned the task, if applicable
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// Task type, if reported
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_type: Option<String>,
}

/// System message emitted while a background task is in progress.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskProgressMessage {
    /// ID of the task
    pub task_id: String,
    /// Human-readable description of the task
    pub description: String,
    /// Usage statistics so far
    pub usage: TaskUsage,
    /// Unique ID of this event
    pub uuid: String,
    /// Session ID the task belongs to
    pub session_id: String,
    /// Tool use ID that spawned the task, if applicable
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// Name of the last tool the task invoked
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tool_name: Option<String>,
}

/// System message emitted when a background task completes, fails, or is
/// stopped.
///
/// Not every terminal task emits this message — some report completion
/// only via a [`TaskUpdatedMessage`] whose `patch.status` is terminal.
/// Consumers tracking active task IDs should clear them on a terminal
/// status from either message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNotificationMessage {
    /// ID of the task
    pub task_id: String,
    /// Terminal status ("completed", "failed", or "stopped")
    pub status: String,
    /// Path to the task's output file
    pub output_file: String,
    /// Short summary of the task's outcome
    pub summary: String,
    /// Unique ID of this event
    pub uuid: String,
    /// Session ID the task belongs to
    pub session_id: String,
    /// Tool use ID that spawned the task, if applicable
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// Usage statistics at completion, if reported
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TaskUsage>,
}

/// System message emitted when a background task's state changes.
///
/// `patch` carries the changed fields (e.g. `status`, `end_time`); when
/// `patch.status` is terminal ("completed", "failed", or "killed") the
/// task has finished. A background task's terminal state can arrive only
/// as a `TaskUpdatedMessage` with no accompanying
/// [`TaskNotificationMessage`] — for example a task stopped via TaskStop
/// reports `status: "killed"` here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskUpdatedMessage {
    /// ID of the task (empty string if the CLI omitted it)
    pub task_id: String,
    /// Raw patch of changed fields
    pub patch: serde_json::Value,
    /// Status extracted from `patch.status`, if present
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Session ID the task belongs to, if present
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Unique ID of this event, if present
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
}

/// System message emitted when a `SessionStore.append` call fails.
///
/// Non-fatal — the local-disk transcript is already durable, so the
/// session continues unaffected. The mirrored copy in the external store
/// will be missing the failed batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MirrorErrorMessage {
    /// Key identifying the session/subagent transcript that failed to mirror
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<serde_json::Value>,
    /// Error message
    pub error: String,
}

/// Hook event emitted by the CLI when `include_hook_events` is enabled.
///
/// These arrive on the wire as `{"type": "system", "subtype":
/// "hook_started" | "hook_response", "hook_event": "PreToolUse", ...}`
/// (or `hook_name` / `hook_event_name` on older CLI versions — see the
/// fallback chain in [`SystemMessage::classify`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEventMessage {
    /// Name of the hook event (e.g. "PreToolUse", "PostToolUse", "Stop")
    pub hook_event_name: String,
    /// Session ID the event belongs to, if present
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Unique ID of the event, if present
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
}

/// Rate limit status emitted by the CLI when rate limit state changes.
///
/// See <https://docs.claude.com/en/docs/claude-code/rate-limits>.
///
/// Deserialized by hand (see the `Deserialize` impl below) rather than via
/// derive: `raw` must capture the *entire* incoming object verbatim
/// (matching Python's `raw=info`), not just fields left over after the
/// named ones are matched — `#[serde(flatten)]` only gives the leftovers.
#[derive(Debug, Clone, Serialize)]
pub struct RateLimitInfo {
    /// Current rate limit status ("allowed", "allowed_warning", or
    /// "rejected"). `allowed_warning` means approaching the limit;
    /// `rejected` means the limit has been hit.
    pub status: String,
    /// Unix timestamp when the rate limit window resets
    #[serde(skip_serializing_if = "Option::is_none", rename = "resetsAt")]
    pub resets_at: Option<i64>,
    /// Which rate limit window applies (e.g. "five_hour", "seven_day")
    #[serde(skip_serializing_if = "Option::is_none", rename = "rateLimitType")]
    pub rate_limit_type: Option<String>,
    /// Fraction of the rate limit consumed (0.0 - 1.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utilization: Option<f64>,
    /// Status of overage/pay-as-you-go usage, if applicable
    #[serde(skip_serializing_if = "Option::is_none", rename = "overageStatus")]
    pub overage_status: Option<String>,
    /// Unix timestamp when the overage window resets
    #[serde(skip_serializing_if = "Option::is_none", rename = "overageResetsAt")]
    pub overage_resets_at: Option<i64>,
    /// Why overage is unavailable if status is rejected
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "overageDisabledReason"
    )]
    pub overage_disabled_reason: Option<String>,
    /// Full raw payload from the CLI, including any fields not modeled above
    pub raw: serde_json::Value,
}

impl<'de> serde::Deserialize<'de> for RateLimitInfo {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = serde_json::Value::deserialize(deserializer)?;
        let status = str_field(&raw, "status")
            .ok_or_else(|| serde::de::Error::missing_field("status"))?;
        Ok(RateLimitInfo {
            status,
            resets_at: raw.get("resetsAt").and_then(|v| v.as_i64()),
            rate_limit_type: str_field(&raw, "rateLimitType"),
            utilization: raw.get("utilization").and_then(|v| v.as_f64()),
            overage_status: str_field(&raw, "overageStatus"),
            overage_resets_at: raw.get("overageResetsAt").and_then(|v| v.as_i64()),
            overage_disabled_reason: str_field(&raw, "overageDisabledReason"),
            raw,
        })
    }
}

/// Rate limit event emitted when rate limit info changes.
///
/// The CLI emits this whenever the rate limit status transitions (e.g.
/// from "allowed" to "allowed_warning"). Use this to warn users before
/// they hit a hard limit, or to gracefully back off when
/// `rate_limit_info.status == "rejected"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitEvent {
    /// The rate limit status that triggered this event
    pub rate_limit_info: RateLimitInfo,
    /// Unique ID of this event
    pub uuid: String,
    /// Session ID this event belongs to
    pub session_id: String,
}

/// Result message indicating query completion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultMessage {
    /// Result subtype
    pub subtype: String,
    /// Duration in milliseconds
    pub duration_ms: u64,
    /// API duration in milliseconds
    pub duration_api_ms: u64,
    /// Whether this is an error result
    pub is_error: bool,
    /// Number of turns in conversation
    pub num_turns: u32,
    /// Session ID
    pub session_id: String,
    /// Why the agentic loop stopped (e.g. "end_turn", "max_turns")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Total cost in USD
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    /// Usage statistics
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<serde_json::Value>,
    /// Result text (if any)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Structured output (when output_format is specified)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<serde_json::Value>,
    /// Per-model token usage and cost breakdown
    #[serde(skip_serializing_if = "Option::is_none", rename = "modelUsage")]
    pub model_usage: Option<std::collections::HashMap<String, ModelUsage>>,
    /// Permission decisions that denied a tool call during this run
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_denials: Option<Vec<serde_json::Value>>,
    /// Tool use deferred by a PreToolUse hook returning "defer"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred_tool_use: Option<DeferredToolUse>,
    /// Non-fatal errors collected during the run
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<String>>,
    /// HTTP status code of the failing API call when `is_error` is true
    /// and `subtype` is "success"; `None` otherwise. Safe to log (no
    /// message content). Emitted by the CLI since v2.1.110.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_error_status: Option<i32>,
    /// Unique ID of this result event
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Why the query loop terminated (e.g. "completed", "max_turns",
    /// "aborted_streaming", "aborted_tools"). `None` when the CLI did not
    /// report a terminal reason (older CLI versions, or a result that
    /// bypassed the query loop such as a local slash command).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
}

/// Tool use that was deferred by a PreToolUse hook returning `"defer"`.
///
/// When a PreToolUse hook returns `permissionDecision: "defer"`, the run
/// stops and the result message carries the deferred tool call here so the
/// caller can inspect it and decide whether to resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeferredToolUse {
    /// Tool use ID
    pub id: String,
    /// Tool name
    pub name: String,
    /// Tool input parameters
    pub input: serde_json::Value,
}

/// Per-model token usage and cost breakdown.
///
/// Keys match the TypeScript SDK's `ModelUsage` shape (camelCase), since
/// the value is passed through verbatim from the CLI's `modelUsage` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    /// Input tokens consumed
    pub input_tokens: u64,
    /// Output tokens produced
    pub output_tokens: u64,
    /// Tokens read from the prompt cache
    pub cache_read_input_tokens: u64,
    /// Tokens written to the prompt cache
    pub cache_creation_input_tokens: u64,
    /// Number of web search requests made
    pub web_search_requests: u64,
    /// Cost in USD
    #[serde(rename = "costUSD")]
    pub cost_usd: f64,
    /// Context window size for this model
    pub context_window: u64,
    /// Maximum output tokens for this model
    pub max_output_tokens: u64,
    /// Canonical model id used for the pricing lookup (e.g.
    /// "claude-opus-4-7"). May differ from the raw model string this entry
    /// is keyed by (provider-specific ids, aliases).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_model: Option<String>,
    /// API provider that served this model ("firstParty", "bedrock",
    /// "vertex", "foundry", "anthropicAws", "anthropicGoogleCloud",
    /// "mantle", "gateway").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// Stream event message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEvent {
    /// Event UUID
    pub uuid: String,
    /// Session ID
    pub session_id: String,
    /// Event data
    pub event: serde_json::Value,
    /// Parent tool use ID (if applicable)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
}

/// Content block types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Text block
    Text(TextBlock),
    /// Thinking block (extended thinking)
    Thinking(ThinkingBlock),
    /// Tool use block
    ToolUse(ToolUseBlock),
    /// Tool result block
    ToolResult(ToolResultBlock),
    /// Image block
    Image(ImageBlock),
    /// Server-side tool use block (e.g. advisor, web_search, web_fetch)
    ServerToolUse(ServerToolUseBlock),
    /// Result of a server-side tool call
    #[serde(rename = "advisor_tool_result")]
    ServerToolResult(ServerToolResultBlock),
}

/// Text content block
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextBlock {
    /// Text content
    pub text: String,
}

/// Thinking block (extended thinking)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingBlock {
    /// Thinking content
    pub thinking: String,
    /// Signature
    pub signature: String,
}

/// Tool use block
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUseBlock {
    /// Tool use ID
    pub id: String,
    /// Tool name
    pub name: String,
    /// Tool input parameters
    pub input: serde_json::Value,
}

/// Tool result block
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultBlock {
    /// Tool use ID this result corresponds to
    pub tool_use_id: String,
    /// Result content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ToolResultContent>,
    /// Whether this is an error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// Tool result content
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    /// Text result
    Text(String),
    /// Structured blocks
    Blocks(Vec<serde_json::Value>),
}

/// Server-side tool use block (e.g. advisor, web_search, web_fetch)
///
/// These are tools the API executes server-side on the model's behalf, so
/// they appear in the message stream alongside regular `tool_use` blocks
/// but the caller never needs to return a result. `name` is a
/// discriminator — branch on it to know which server tool was invoked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerToolUseBlock {
    /// Tool use ID
    pub id: String,
    /// Server tool name (e.g. "advisor", "web_search", "web_fetch")
    pub name: String,
    /// Tool input parameters
    pub input: serde_json::Value,
}

/// Result block returned for a server-side tool call
///
/// Mirrors `ToolResultBlock`'s shape. `content` is the raw value from the
/// API, opaque to this layer — callers that care about a specific server
/// tool's result schema can inspect `content["type"]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerToolResultBlock {
    /// Tool use ID this result corresponds to
    pub tool_use_id: String,
    /// Result content (raw, tool-specific shape)
    pub content: serde_json::Value,
}

/// Image source for user prompts
///
/// Represents the source of image data that can be included in user messages.
/// Claude supports both base64-encoded images and URL references.
///
/// # Supported Formats
///
/// - JPEG (`image/jpeg`)
/// - PNG (`image/png`)
/// - GIF (`image/gif`)
/// - WebP (`image/webp`)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    /// Base64-encoded image data
    Base64 {
        /// MIME type (e.g., "image/png", "image/jpeg", "image/gif", "image/webp")
        media_type: String,
        /// Base64-encoded image data (without data URI prefix)
        data: String,
    },
    /// URL reference to an image
    Url {
        /// Publicly accessible image URL
        url: String,
    },
}

/// Image block for user prompts
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageBlock {
    /// Image source (base64 or URL)
    pub source: ImageSource,
}

/// Content block for user prompts (input)
///
/// Represents content that can be included in user messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserContentBlock {
    /// Text content
    Text {
        /// Text content string
        text: String,
    },
    /// Image content
    Image {
        /// Image source (base64 or URL)
        source: ImageSource,
    },
}

impl UserContentBlock {
    /// Create a text content block
    pub fn text(text: impl Into<String>) -> Self {
        UserContentBlock::Text { text: text.into() }
    }

    /// Create an image content block from base64 data
    ///
    /// # Arguments
    ///
    /// * `media_type` - MIME type of the image (e.g., "image/png", "image/jpeg")
    /// * `data` - Base64-encoded image data (without data URI prefix)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The MIME type is not supported (valid types: image/jpeg, image/png, image/gif, image/webp)
    /// - The base64 data exceeds the maximum size limit (15MB)
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use claude_agent_sdk::UserContentBlock;
    /// let block = UserContentBlock::image_base64("image/png", "iVBORw0KGgo=")?;
    /// # Ok::<(), claude_agent_sdk::ClaudeError>(())
    /// ```
    pub fn image_base64(
        media_type: impl Into<String>,
        data: impl Into<String>,
    ) -> crate::errors::Result<Self> {
        let media_type_str = media_type.into();
        let data_str = data.into();

        // Validate MIME type
        if !SUPPORTED_IMAGE_MIME_TYPES.contains(&media_type_str.as_str()) {
            return Err(crate::errors::ImageValidationError::new(format!(
                "Unsupported media type '{}'. Supported types: {:?}",
                media_type_str, SUPPORTED_IMAGE_MIME_TYPES
            ))
            .into());
        }

        // Validate base64 size
        if data_str.len() > MAX_BASE64_SIZE {
            return Err(crate::errors::ImageValidationError::new(format!(
                "Base64 data exceeds maximum size of {} bytes (got {} bytes)",
                MAX_BASE64_SIZE,
                data_str.len()
            ))
            .into());
        }

        Ok(UserContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: media_type_str,
                data: data_str,
            },
        })
    }

    /// Create an image content block from URL
    pub fn image_url(url: impl Into<String>) -> Self {
        UserContentBlock::Image {
            source: ImageSource::Url { url: url.into() },
        }
    }

    /// Validate a collection of content blocks
    ///
    /// Ensures the content is non-empty. This is used internally by query functions
    /// to provide consistent validation.
    ///
    /// # Errors
    ///
    /// Returns an error if the content blocks slice is empty.
    pub fn validate_content(blocks: &[UserContentBlock]) -> crate::Result<()> {
        if blocks.is_empty() {
            return Err(crate::errors::ClaudeError::InvalidConfig(
                "Content must include at least one block (text or image)".to_string(),
            ));
        }
        Ok(())
    }
}

impl From<String> for UserContentBlock {
    fn from(text: String) -> Self {
        UserContentBlock::Text { text }
    }
}

impl From<&str> for UserContentBlock {
    fn from(text: &str) -> Self {
        UserContentBlock::Text {
            text: text.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_content_block_text_serialization() {
        let block = ContentBlock::Text(TextBlock {
            text: "Hello".to_string(),
        });

        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "Hello");
    }

    #[test]
    fn test_content_block_tool_use_serialization() {
        let block = ContentBlock::ToolUse(ToolUseBlock {
            id: "tool_123".to_string(),
            name: "Bash".to_string(),
            input: json!({"command": "echo hello"}),
        });

        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "tool_use");
        assert_eq!(json["id"], "tool_123");
        assert_eq!(json["name"], "Bash");
        assert_eq!(json["input"]["command"], "echo hello");
    }

    #[test]
    fn test_message_assistant_deserialization() {
        let json_str = r#"{
            "type": "assistant",
            "message": {
                "content": [{"type": "text", "text": "Hello"}],
                "model": "claude-sonnet-4"
            },
            "session_id": "test-session"
        }"#;

        let msg: Message = serde_json::from_str(json_str).unwrap();
        match msg {
            Message::Assistant(assistant) => {
                assert_eq!(assistant.session_id, Some("test-session".to_string()));
                assert_eq!(assistant.message.model, Some("claude-sonnet-4".to_string()));
            },
            _ => panic!("Expected Assistant variant"),
        }
    }

    #[test]
    fn test_message_result_deserialization() {
        let json_str = r#"{
            "type": "result",
            "subtype": "query_complete",
            "duration_ms": 1500,
            "duration_api_ms": 1200,
            "is_error": false,
            "num_turns": 3,
            "session_id": "test-session",
            "total_cost_usd": 0.0042
        }"#;

        let msg: Message = serde_json::from_str(json_str).unwrap();
        match msg {
            Message::Result(result) => {
                assert_eq!(result.subtype, "query_complete");
                assert_eq!(result.duration_ms, 1500);
                assert_eq!(result.num_turns, 3);
                assert_eq!(result.total_cost_usd, Some(0.0042));
            },
            _ => panic!("Expected Result variant"),
        }
    }

    #[test]
    fn test_message_system_deserialization() {
        let json_str = r#"{
            "type": "system",
            "subtype": "session_start",
            "cwd": "/home/user",
            "session_id": "test-session",
            "tools": ["Bash", "Read", "Write"]
        }"#;

        let msg: Message = serde_json::from_str(json_str).unwrap();
        match msg {
            Message::System(system) => {
                assert_eq!(system.subtype, "session_start");
                assert_eq!(system.cwd, Some("/home/user".to_string()));
                assert_eq!(system.tools.as_ref().unwrap().len(), 3);
            },
            _ => panic!("Expected System variant"),
        }
    }

    #[test]
    fn test_tool_result_content_text() {
        let content = ToolResultContent::Text("Command output".to_string());
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json, "Command output");
    }

    #[test]
    fn test_tool_result_content_blocks() {
        let content = ToolResultContent::Blocks(vec![json!({"type": "text", "text": "Result"})]);
        let json = serde_json::to_value(&content).unwrap();
        assert!(json.is_array());
        assert_eq!(json[0]["type"], "text");
    }

    #[test]
    fn test_image_source_base64_serialization() {
        let source = ImageSource::Base64 {
            media_type: "image/png".to_string(),
            data: "iVBORw0KGgo=".to_string(),
        };

        let json = serde_json::to_value(&source).unwrap();
        assert_eq!(json["type"], "base64");
        assert_eq!(json["media_type"], "image/png");
        assert_eq!(json["data"], "iVBORw0KGgo=");
    }

    #[test]
    fn test_image_source_url_serialization() {
        let source = ImageSource::Url {
            url: "https://example.com/image.png".to_string(),
        };

        let json = serde_json::to_value(&source).unwrap();
        assert_eq!(json["type"], "url");
        assert_eq!(json["url"], "https://example.com/image.png");
    }

    #[test]
    fn test_image_source_base64_deserialization() {
        let json_str = r#"{
            "type": "base64",
            "media_type": "image/jpeg",
            "data": "base64data=="
        }"#;

        let source: ImageSource = serde_json::from_str(json_str).unwrap();
        match source {
            ImageSource::Base64 { media_type, data } => {
                assert_eq!(media_type, "image/jpeg");
                assert_eq!(data, "base64data==");
            },
            _ => panic!("Expected Base64 variant"),
        }
    }

    #[test]
    fn test_image_source_url_deserialization() {
        let json_str = r#"{
            "type": "url",
            "url": "https://example.com/test.gif"
        }"#;

        let source: ImageSource = serde_json::from_str(json_str).unwrap();
        match source {
            ImageSource::Url { url } => {
                assert_eq!(url, "https://example.com/test.gif");
            },
            _ => panic!("Expected Url variant"),
        }
    }

    #[test]
    fn test_user_content_block_text_serialization() {
        let block = UserContentBlock::text("Hello world");

        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "Hello world");
    }

    #[test]
    fn test_user_content_block_image_base64_serialization() {
        let block = UserContentBlock::image_base64("image/png", "iVBORw0KGgo=").unwrap();

        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "image");
        assert_eq!(json["source"]["type"], "base64");
        assert_eq!(json["source"]["media_type"], "image/png");
        assert_eq!(json["source"]["data"], "iVBORw0KGgo=");
    }

    #[test]
    fn test_user_content_block_image_url_serialization() {
        let block = UserContentBlock::image_url("https://example.com/image.webp");

        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "image");
        assert_eq!(json["source"]["type"], "url");
        assert_eq!(json["source"]["url"], "https://example.com/image.webp");
    }

    #[test]
    fn test_user_content_block_from_string() {
        let block: UserContentBlock = "Test message".into();

        match block {
            UserContentBlock::Text { text } => {
                assert_eq!(text, "Test message");
            },
            _ => panic!("Expected Text variant"),
        }
    }

    #[test]
    fn test_user_content_block_from_owned_string() {
        let block: UserContentBlock = String::from("Owned message").into();

        match block {
            UserContentBlock::Text { text } => {
                assert_eq!(text, "Owned message");
            },
            _ => panic!("Expected Text variant"),
        }
    }

    #[test]
    fn test_image_block_serialization() {
        let block = ImageBlock {
            source: ImageSource::Base64 {
                media_type: "image/gif".to_string(),
                data: "R0lGODlh".to_string(),
            },
        };

        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["source"]["type"], "base64");
        assert_eq!(json["source"]["media_type"], "image/gif");
        assert_eq!(json["source"]["data"], "R0lGODlh");
    }

    #[test]
    fn test_content_block_image_serialization() {
        let block = ContentBlock::Image(ImageBlock {
            source: ImageSource::Url {
                url: "https://example.com/photo.jpg".to_string(),
            },
        });

        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "image");
        assert_eq!(json["source"]["type"], "url");
        assert_eq!(json["source"]["url"], "https://example.com/photo.jpg");
    }

    #[test]
    fn test_content_block_image_deserialization() {
        let json_str = r#"{
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": "image/webp",
                "data": "UklGR"
            }
        }"#;

        let block: ContentBlock = serde_json::from_str(json_str).unwrap();
        match block {
            ContentBlock::Image(image) => match image.source {
                ImageSource::Base64 { media_type, data } => {
                    assert_eq!(media_type, "image/webp");
                    assert_eq!(data, "UklGR");
                },
                _ => panic!("Expected Base64 source"),
            },
            _ => panic!("Expected Image variant"),
        }
    }

    #[test]
    fn test_user_content_block_deserialization() {
        let json_str = r#"{
            "type": "text",
            "text": "Describe this image"
        }"#;

        let block: UserContentBlock = serde_json::from_str(json_str).unwrap();
        match block {
            UserContentBlock::Text { text } => {
                assert_eq!(text, "Describe this image");
            },
            _ => panic!("Expected Text variant"),
        }
    }

    #[test]
    fn test_user_content_block_image_deserialization() {
        let json_str = r#"{
            "type": "image",
            "source": {
                "type": "url",
                "url": "https://example.com/diagram.png"
            }
        }"#;

        let block: UserContentBlock = serde_json::from_str(json_str).unwrap();
        match block {
            UserContentBlock::Image { source } => match source {
                ImageSource::Url { url } => {
                    assert_eq!(url, "https://example.com/diagram.png");
                },
                _ => panic!("Expected Url source"),
            },
            _ => panic!("Expected Image variant"),
        }
    }

    #[test]
    fn test_image_base64_valid() {
        let block = UserContentBlock::image_base64("image/png", "iVBORw0KGgo=");
        assert!(block.is_ok());
    }

    #[test]
    fn test_image_base64_invalid_mime_type() {
        let block = UserContentBlock::image_base64("image/bmp", "data");
        assert!(block.is_err());
        let err = block.unwrap_err().to_string();
        assert!(err.contains("Unsupported media type"));
        assert!(err.contains("image/bmp"));
    }

    #[test]
    fn test_image_base64_exceeds_size_limit() {
        let large_data = "a".repeat(MAX_BASE64_SIZE + 1);
        let block = UserContentBlock::image_base64("image/png", large_data);
        assert!(block.is_err());
        let err = block.unwrap_err().to_string();
        assert!(err.contains("exceeds maximum size"));
    }

    #[test]
    fn test_message_unknown_type_is_forward_compatible() {
        let json_str = r#"{"type":"a_type_this_sdk_has_never_heard_of","foo":"bar"}"#;
        let msg: Message = serde_json::from_str(json_str).unwrap();
        assert!(matches!(msg, Message::Unknown));
    }

    #[test]
    fn test_message_rate_limit_event_deserialization() {
        let json_str = r#"{
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": "rejected",
                "overageStatus": "allowed",
                "overageDisabledReason": null
            },
            "uuid": "u1",
            "session_id": "s1"
        }"#;

        let msg: Message = serde_json::from_str(json_str).unwrap();
        match msg {
            Message::RateLimitEvent(event) => {
                assert_eq!(event.rate_limit_info.status, "rejected");
                assert_eq!(
                    event.rate_limit_info.overage_status,
                    Some("allowed".to_string())
                );
                assert_eq!(event.rate_limit_info.resets_at, None);
            }
            _ => panic!("Expected RateLimitEvent variant"),
        }
    }

    #[test]
    fn test_content_block_server_tool_use_deserialization() {
        let json_str = r#"{
            "type": "server_tool_use",
            "id": "srvtool_1",
            "name": "web_search",
            "input": {"query": "rust serde"}
        }"#;

        let block: ContentBlock = serde_json::from_str(json_str).unwrap();
        match block {
            ContentBlock::ServerToolUse(b) => {
                assert_eq!(b.id, "srvtool_1");
                assert_eq!(b.name, "web_search");
            }
            _ => panic!("Expected ServerToolUse variant"),
        }
    }

    #[test]
    fn test_content_block_server_tool_result_deserialization() {
        let json_str = r#"{
            "type": "advisor_tool_result",
            "tool_use_id": "srvtool_1",
            "content": {"type": "web_search_result", "results": []}
        }"#;

        let block: ContentBlock = serde_json::from_str(json_str).unwrap();
        match block {
            ContentBlock::ServerToolResult(b) => {
                assert_eq!(b.tool_use_id, "srvtool_1");
            }
            _ => panic!("Expected ServerToolResult variant"),
        }
    }

    fn system_message(subtype: &str, extra: serde_json::Value) -> SystemMessage {
        let mut data = extra;
        data["subtype"] = json!(subtype);
        serde_json::from_value(data).unwrap()
    }

    #[test]
    fn test_system_message_classify_task_started() {
        let sys = system_message(
            "task_started",
            json!({
                "task_id": "t1",
                "description": "do the thing",
                "uuid": "u1",
                "session_id": "s1",
                "task_type": "background"
            }),
        );

        match sys.classify() {
            SystemMessageKind::TaskStarted(t) => {
                assert_eq!(t.task_id, "t1");
                assert_eq!(t.description, "do the thing");
                assert_eq!(t.task_type, Some("background".to_string()));
            }
            other => panic!("Expected TaskStarted, got {:?}", other),
        }
    }

    #[test]
    fn test_system_message_classify_task_started_missing_required_falls_back() {
        // Missing `description` (required) must not panic or error the
        // surrounding parse — classification just declines to specialize.
        let sys = system_message(
            "task_started",
            json!({
                "task_id": "t1",
                "uuid": "u1",
                "session_id": "s1"
            }),
        );

        assert!(matches!(sys.classify(), SystemMessageKind::Generic));
    }

    #[test]
    fn test_system_message_classify_task_updated_is_defensive() {
        // No uuid/session_id/patch at all - must still classify without panicking.
        let sys = system_message("task_updated", json!({}));

        match sys.classify() {
            SystemMessageKind::TaskUpdated(t) => {
                assert_eq!(t.task_id, "");
                assert_eq!(t.status, None);
                assert_eq!(t.session_id, None);
            }
            other => panic!("Expected TaskUpdated, got {:?}", other),
        }
    }

    #[test]
    fn test_system_message_classify_task_updated_terminal_status() {
        let sys = system_message(
            "task_updated",
            json!({
                "task_id": "t1",
                "patch": {"status": "killed", "end_time": 123}
            }),
        );

        match sys.classify() {
            SystemMessageKind::TaskUpdated(t) => {
                assert_eq!(t.status, Some("killed".to_string()));
                assert_eq!(t.patch["end_time"], 123);
            }
            other => panic!("Expected TaskUpdated, got {:?}", other),
        }
    }

    #[test]
    fn test_system_message_classify_hook_event_fallback_chain() {
        // hook_event takes priority over hook_name/hook_event_name.
        let sys = system_message(
            "hook_started",
            json!({"hook_event": "PreToolUse", "hook_name": "ignored"}),
        );
        match sys.classify() {
            SystemMessageKind::HookEvent(h) => assert_eq!(h.hook_event_name, "PreToolUse"),
            other => panic!("Expected HookEvent, got {:?}", other),
        }

        // Falls back to hook_name when hook_event is absent.
        let sys2 = system_message("hook_response", json!({"hook_name": "PostToolUse"}));
        match sys2.classify() {
            SystemMessageKind::HookEvent(h) => assert_eq!(h.hook_event_name, "PostToolUse"),
            other => panic!("Expected HookEvent, got {:?}", other),
        }

        // Falls back to hook_event_name as the last resort.
        let sys3 = system_message(
            "hook_response",
            json!({"hook_event_name": "SessionStart"}),
        );
        match sys3.classify() {
            SystemMessageKind::HookEvent(h) => assert_eq!(h.hook_event_name, "SessionStart"),
            other => panic!("Expected HookEvent, got {:?}", other),
        }
    }

    #[test]
    fn test_system_message_classify_mirror_error() {
        let sys = system_message("mirror_error", json!({"error": "disk full"}));
        match sys.classify() {
            SystemMessageKind::MirrorError(m) => assert_eq!(m.error, "disk full"),
            other => panic!("Expected MirrorError, got {:?}", other),
        }
    }

    #[test]
    fn test_system_message_classify_generic_for_plain_subtype() {
        let sys = system_message("init", json!({"cwd": "/tmp"}));
        assert!(matches!(sys.classify(), SystemMessageKind::Generic));
    }

    #[test]
    fn test_result_message_new_fields_deserialization() {
        let json_str = r#"{
            "type": "result",
            "subtype": "success",
            "duration_ms": 100,
            "duration_api_ms": 80,
            "is_error": false,
            "num_turns": 1,
            "session_id": "s1",
            "stop_reason": "end_turn",
            "uuid": "u1",
            "terminal_reason": "completed",
            "errors": ["warn: something"],
            "api_error_status": 429,
            "modelUsage": {
                "claude-opus-4-7": {
                    "inputTokens": 10,
                    "outputTokens": 20,
                    "cacheReadInputTokens": 0,
                    "cacheCreationInputTokens": 0,
                    "webSearchRequests": 0,
                    "costUSD": 0.05,
                    "contextWindow": 200000,
                    "maxOutputTokens": 8192
                }
            },
            "deferred_tool_use": {"id": "tu1", "name": "Bash", "input": {}}
        }"#;

        let msg: Message = serde_json::from_str(json_str).unwrap();
        match msg {
            Message::Result(r) => {
                assert_eq!(r.stop_reason, Some("end_turn".to_string()));
                assert_eq!(r.api_error_status, Some(429));
                let usage = r.model_usage.unwrap();
                assert_eq!(usage["claude-opus-4-7"].cost_usd, 0.05);
                assert_eq!(r.deferred_tool_use.unwrap().name, "Bash");
            }
            _ => panic!("Expected Result variant"),
        }
    }
}
