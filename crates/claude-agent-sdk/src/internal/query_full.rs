//! Full Query implementation with bidirectional control protocol

use futures::stream::StreamExt;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::errors::{ClaudeError, Result};
use crate::session::{SessionKey, TranscriptMirrorBatcher};
use crate::types::hooks::{HookCallback, HookContext, HookInput, HookMatcher};
use crate::types::mcp::McpSdkServerConfig;

use super::run_lifecycle::{RunEndedSignal, track_task_lifecycle};
use super::transport::Transport;

/// Build the wire JSON for a `mirror_error` system message (see
/// [`crate::types::messages::MirrorErrorMessage`]).
///
/// Shared between [`QueryFull::report_mirror_error`] and the
/// `on_error` callback client.rs wires up when attaching a
/// `TranscriptMirrorBatcher` (that callback runs before the `QueryFull` it
/// reports into is wrapped in its owning `Arc`, so it captures a plain
/// message-sender clone and calls this free function directly rather than
/// going through `report_mirror_error`).
pub(crate) fn build_mirror_error_message(key: Option<SessionKey>, error: String) -> serde_json::Value {
    let session_id = key.as_ref().map(|k| k.session_id.clone()).unwrap_or_default();
    let key_json = key.and_then(|k| serde_json::to_value(k).ok());
    json!({
        "type": "system",
        "subtype": "mirror_error",
        "error": error,
        "key": key_json,
        "uuid": uuid::Uuid::new_v4().to_string(),
        "session_id": session_id,
    })
}

/// Control request from SDK to CLI
#[allow(dead_code)]
#[derive(Debug, serde::Serialize)]
struct ControlRequest {
    #[serde(rename = "type")]
    type_: String,
    request_id: String,
    request: serde_json::Value,
}

/// Control response from CLI to SDK
#[derive(Debug, serde::Deserialize)]
struct ControlResponse {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    type_: String,
    response: ControlResponseData,
}

#[derive(Debug, serde::Deserialize)]
struct ControlResponseData {
    #[allow(dead_code)]
    subtype: String,
    request_id: String,
    #[serde(flatten)]
    data: serde_json::Value,
}

/// Control request from CLI to SDK
#[derive(Debug, serde::Deserialize)]
struct IncomingControlRequest {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    type_: String,
    request_id: String,
    request: serde_json::Value,
}

/// Full Query implementation with bidirectional control protocol
pub struct QueryFull {
    pub(crate) transport: Arc<Mutex<Box<dyn Transport>>>,
    hook_callbacks: Arc<Mutex<HashMap<String, HookCallback>>>,
    sdk_mcp_servers: Arc<Mutex<HashMap<String, McpSdkServerConfig>>>,
    next_callback_id: Arc<AtomicU64>,
    request_counter: Arc<AtomicU64>,
    pending_responses: Arc<Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>>,
    message_tx: mpsc::UnboundedSender<serde_json::Value>,
    pub(crate) message_rx: Arc<Mutex<mpsc::UnboundedReceiver<serde_json::Value>>>,
    // Direct access to stdin for writes (bypasses transport lock)
    pub(crate) stdin: Option<Arc<Mutex<Option<tokio::process::ChildStdin>>>>,
    // Store initialization result for get_server_info()
    initialization_result: Arc<Mutex<Option<serde_json::Value>>>,
    // SessionStore mirroring, attached via `set_transcript_mirror_batcher`
    // before `start()`. `None` when no `session_store` option is configured.
    transcript_mirror_batcher: Arc<Mutex<Option<Arc<TranscriptMirrorBatcher>>>>,
    // Task IDs of started-but-not-finished delegated agent tasks; see
    // `run_lifecycle` module docs (upstream issue #1088).
    inflight_tasks: Arc<Mutex<HashSet<String>>>,
    // Fires once a run-ending result (no tasks in flight) arrives, or the
    // read loop exits for any other reason.
    run_ended: Arc<RunEndedSignal>,
    // Handle to the background read task. Awaited (bounded) during close()
    // so the transport lock it holds for the whole read loop is released
    // before close() tries to close the transport itself.
    read_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl QueryFull {
    /// Create a new Query
    pub fn new(transport: Box<dyn Transport>) -> Self {
        let (message_tx, message_rx) = mpsc::unbounded_channel();

        Self {
            transport: Arc::new(Mutex::new(transport)),
            hook_callbacks: Arc::new(Mutex::new(HashMap::new())),
            sdk_mcp_servers: Arc::new(Mutex::new(HashMap::new())),
            next_callback_id: Arc::new(AtomicU64::new(0)),
            request_counter: Arc::new(AtomicU64::new(0)),
            pending_responses: Arc::new(Mutex::new(HashMap::new())),
            message_tx,
            message_rx: Arc::new(Mutex::new(message_rx)),
            stdin: None,
            initialization_result: Arc::new(Mutex::new(None)),
            transcript_mirror_batcher: Arc::new(Mutex::new(None)),
            inflight_tasks: Arc::new(Mutex::new(HashSet::new())),
            run_ended: Arc::new(RunEndedSignal::new()),
            read_task: Arc::new(Mutex::new(None)),
        }
    }

    /// Attach a batcher that receives `transcript_mirror` frames.
    ///
    /// Call before [`Self::start`]. When set, the read loop peels
    /// `transcript_mirror` frames off stdout (they are not yielded to
    /// consumers), enqueues them on the batcher, and flushes before
    /// forwarding each `result` message.
    pub async fn set_transcript_mirror_batcher(&self, batcher: Arc<TranscriptMirrorBatcher>) {
        *self.transcript_mirror_batcher.lock().await = Some(batcher);
    }

    /// A clone of the outgoing message sender, for callers that need to
    /// feed a synthetic message into the stream before they have a
    /// `QueryFull` handle to call [`Self::report_mirror_error`] on (see
    /// `build_mirror_error_message`'s doc comment).
    pub(crate) fn message_sender(&self) -> mpsc::UnboundedSender<serde_json::Value> {
        self.message_tx.clone()
    }

    /// Surface a `SessionStore::append` failure as a `mirror_error` system
    /// message fed back into the message stream.
    ///
    /// Called from the batcher's `on_error`; the dropped batch is not
    /// retried (at-most-once delivery), so this is the consumer's only
    /// signal. Non-blocking — the channel is unbounded, so this never
    /// stalls the caller.
    pub fn report_mirror_error(&self, key: Option<SessionKey>, error: String) {
        let _ = self.message_tx.send(build_mirror_error_message(key, error));
    }

    /// Set stdin for direct write access (called from client after transport is connected)
    pub fn set_stdin(&mut self, stdin: Arc<Mutex<Option<tokio::process::ChildStdin>>>) {
        self.stdin = Some(stdin);
    }

    /// Set SDK MCP servers
    pub async fn set_sdk_mcp_servers(&mut self, servers: HashMap<String, McpSdkServerConfig>) {
        *self.sdk_mcp_servers.lock().await = servers;
    }

    /// Initialize with hooks, custom agent definitions, skill selection, and
    /// the preset-system-prompt `exclude_dynamic_sections` flag.
    ///
    /// Agent definitions and skills are sent here (not as CLI argv) matching
    /// the TypeScript SDK and upstream Python's `_internal/query.py`
    /// `initialize()` — an earlier Rust implementation sent `agents` via a
    /// `--agents` CLI flag, which upstream deliberately moved away from.
    pub async fn initialize(
        &self,
        hooks: Option<HashMap<String, Vec<HookMatcher>>>,
        agents: Option<&HashMap<String, crate::types::config::AgentDefinition>>,
        skills: Option<&crate::types::config::SkillsSelector>,
        exclude_dynamic_sections: Option<bool>,
    ) -> Result<serde_json::Value> {
        // Build hooks configuration
        let mut hooks_config: HashMap<String, Vec<serde_json::Value>> = HashMap::new();

        if let Some(hooks_map) = hooks {
            for (event, matchers) in hooks_map {
                let mut event_matchers = Vec::new();

                for matcher in matchers {
                    let mut callback_ids = Vec::new();

                    for callback in matcher.hooks {
                        let callback_id = format!(
                            "hook_{}",
                            self.next_callback_id.fetch_add(1, Ordering::SeqCst)
                        );
                        self.hook_callbacks
                            .lock()
                            .await
                            .insert(callback_id.clone(), callback);
                        callback_ids.push(callback_id);
                    }

                    let mut matcher_json = json!({
                        "matcher": matcher.matcher,
                        "hookCallbackIds": callback_ids
                    });

                    // Add timeout if specified
                    if let Some(timeout) = matcher.timeout {
                        matcher_json["timeout"] = json!(timeout);
                    }

                    event_matchers.push(matcher_json);
                }

                hooks_config.insert(event, event_matchers);
            }
        }

        // Send initialize request
        let mut request = json!({
            "subtype": "initialize",
            "hooks": if hooks_config.is_empty() { json!(null) } else { json!(hooks_config) }
        });

        if let Some(agents) = agents
            && !agents.is_empty()
        {
            request["agents"] = json!(agents);
        }
        if let Some(eds) = exclude_dynamic_sections {
            request["excludeDynamicSections"] = json!(eds);
        }
        // "all" and omitted are equivalent at the wire level (no filter), so
        // only send the field for an explicit list, matching upstream.
        if let Some(crate::types::config::SkillsSelector::List(names)) = skills {
            request["skills"] = json!(names);
        }

        let response = self.send_control_request(request).await?;

        // Store initialization result for get_server_info()
        *self.initialization_result.lock().await = Some(response.clone());

        Ok(response)
    }

    /// Start reading messages in background
    pub async fn start(&self) -> Result<()> {
        let transport = Arc::clone(&self.transport);
        let hook_callbacks = Arc::clone(&self.hook_callbacks);
        let sdk_mcp_servers = Arc::clone(&self.sdk_mcp_servers);
        let pending_responses = Arc::clone(&self.pending_responses);
        let message_tx = self.message_tx.clone();
        let stdin = self.stdin.clone();
        let transcript_mirror_batcher = Arc::clone(&self.transcript_mirror_batcher);
        let inflight_tasks = Arc::clone(&self.inflight_tasks);
        let run_ended = Arc::clone(&self.run_ended);

        // Create a channel to signal when background task is ready
        let (ready_tx, ready_rx) = oneshot::channel();

        let handle = tokio::spawn(async move {
            let mut transport_guard = transport.lock().await;
            let mut stream = transport_guard.read_messages();

            // Signal that we're ready to receive messages
            let _ = ready_tx.send(());

            while let Some(result) = stream.next().await {
                match result {
                    Ok(message) => {
                        let msg_type = message.get("type").and_then(|v| v.as_str());

                        match msg_type {
                            Some("control_response") => {
                                // Handle control response
                                if let Ok(response) =
                                    serde_json::from_value::<ControlResponse>(message.clone())
                                {
                                    let mut pending = pending_responses.lock().await;
                                    if let Some(tx) = pending.remove(&response.response.request_id)
                                    {
                                        let _ = tx.send(response.response.data);
                                    }
                                }
                            },
                            Some("control_request") => {
                                // Handle incoming control request (e.g., hook callback, MCP message)
                                if let Ok(request) = serde_json::from_value::<IncomingControlRequest>(
                                    message.clone(),
                                ) {
                                    let stdin_clone = stdin.clone();
                                    let hook_callbacks_clone = Arc::clone(&hook_callbacks);
                                    let sdk_mcp_servers_clone = Arc::clone(&sdk_mcp_servers);

                                    tokio::spawn(async move {
                                        if let Err(e) = Self::handle_control_request_with_stdin(
                                            request,
                                            stdin_clone,
                                            hook_callbacks_clone,
                                            sdk_mcp_servers_clone,
                                        )
                                        .await
                                        {
                                            eprintln!("Error handling control request: {}", e);
                                        }
                                    });
                                }
                            },
                            Some("transcript_mirror") => {
                                // SessionStore write path: peel mirror frames
                                // off stdout and hand to the batcher; do NOT
                                // yield them to consumers.
                                let batcher = transcript_mirror_batcher.lock().await.clone();
                                if let Some(batcher) = batcher {
                                    let file_path = message
                                        .get("filePath")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string();
                                    let entries = message
                                        .get("entries")
                                        .cloned()
                                        .and_then(|v| serde_json::from_value(v).ok())
                                        .unwrap_or_default();
                                    batcher.enqueue(file_path, entries);
                                }
                            },
                            _ => {
                                // Track task-lifecycle frames so results can
                                // tell "one turn ended" apart from "the run
                                // is done" (see run_lifecycle module docs,
                                // upstream issue #1088).
                                if msg_type == Some("system") {
                                    let subtype =
                                        message.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
                                    let task_id = message.get("task_id").and_then(|v| v.as_str());
                                    let task_type = message.get("task_type").and_then(|v| v.as_str());
                                    let patch_status = message
                                        .get("patch")
                                        .and_then(|p| p.get("status"))
                                        .and_then(|v| v.as_str());
                                    let mut inflight = inflight_tasks.lock().await;
                                    track_task_lifecycle(
                                        &mut inflight,
                                        subtype,
                                        task_id,
                                        task_type,
                                        patch_status,
                                    );
                                }

                                if msg_type == Some("result") {
                                    // Flush pending transcript-mirror entries
                                    // before forwarding the result so
                                    // consumers observing it can rely on the
                                    // SessionStore being up to date for this
                                    // turn.
                                    let batcher = transcript_mirror_batcher.lock().await.clone();
                                    if let Some(batcher) = batcher {
                                        batcher.flush().await;
                                    }
                                    let no_tasks_in_flight = inflight_tasks.lock().await.is_empty();
                                    if no_tasks_in_flight {
                                        run_ended.fire().await;
                                    } else {
                                        tracing::debug!(
                                            "result received with tasks in flight; keeping stdin open"
                                        );
                                    }
                                }

                                // Regular message - send to stream
                                let _ = message_tx.send(message);
                            },
                        }
                    },
                    Err(_) => break,
                }
            }

            // Flush any remaining transcript-mirror entries before the read
            // loop ends so an early stdout EOF or transport error doesn't
            // drop entries batched this turn (belt-and-suspenders with the
            // same flush in `close()`, for the case where the process exits
            // before a caller calls `close()`).
            let batcher = transcript_mirror_batcher.lock().await.clone();
            if let Some(batcher) = batcher {
                batcher.flush().await;
            }
            // Unblock any waiter (e.g. `wait_for_result_and_end_input`) so it
            // doesn't stall forever on early exit.
            run_ended.fire().await;
        });

        *self.read_task.lock().await = Some(handle);

        // Wait for background task to be ready before returning
        ready_rx
            .await
            .map_err(|_| ClaudeError::Transport("Background task failed to start".to_string()))?;

        Ok(())
    }

    /// Close stdin so the CLI observes EOF.
    ///
    /// If [`Self::sdk_mcp_servers`] or hooks were configured, callers should
    /// prefer [`Self::wait_for_result_and_end_input`], which gates this on a
    /// run-ending result so hook/SDK-MCP control responses aren't cut off
    /// mid-turn.
    pub async fn end_input(&self) -> Result<()> {
        if let Some(ref stdin_arc) = self.stdin {
            let mut guard = stdin_arc.lock().await;
            if let Some(mut stdin) = guard.take() {
                let _ = stdin.shutdown().await;
            }
        }
        Ok(())
    }

    /// Wait for a run-ending result (if needed) then close stdin.
    ///
    /// If SDK MCP servers or hooks require bidirectional communication,
    /// keeps stdin open until a result arrives with no delegated agent
    /// tasks in flight — a result frame ends one turn, not necessarily the
    /// run: background tasks keep running past it and still need stdin for
    /// hook/SDK-MCP control responses (see `run_lifecycle` module docs,
    /// upstream issue #1088). No timeout is applied: the signal is
    /// guaranteed to fire, either from a qualifying result or from the read
    /// loop's exit path if the process ends early.
    pub async fn wait_for_result_and_end_input(&self) -> Result<()> {
        let needs_wait = {
            let has_mcp_servers = !self.sdk_mcp_servers.lock().await.is_empty();
            let has_hooks = !self.hook_callbacks.lock().await.is_empty();
            has_mcp_servers || has_hooks
        };
        if needs_wait {
            tracing::debug!("waiting for a run-ending result before closing stdin");
            self.run_ended.wait().await;
        }
        self.end_input().await
    }

    /// Close the query: final-flush any pending mirrored transcript
    /// entries, close stdin, then close the transport.
    ///
    /// The final mirror flush happens before transport teardown so the last
    /// turn's mirrored entries aren't lost. Closing stdin first lets the
    /// background read task (which holds the transport lock for the whole
    /// read loop) observe EOF and release that lock; this method then waits
    /// (bounded) for the read task before locking the transport itself,
    /// rather than transport.close() racing a lock still held by the read
    /// task.
    pub async fn close(&self) -> Result<()> {
        if let Some(batcher) = self.transcript_mirror_batcher.lock().await.clone() {
            batcher.close().await;
        }

        self.end_input().await?;

        if let Some(handle) = self.read_task.lock().await.take()
            && tokio::time::timeout(Duration::from_secs(5), handle).await.is_err()
        {
            tracing::debug!("read task did not exit within 5s of stdin closing");
        }

        let mut transport_guard = self.transport.lock().await;
        transport_guard.close().await
    }

    /// Handle incoming control request from CLI (new version using stdin directly)
    async fn handle_control_request_with_stdin(
        request: IncomingControlRequest,
        stdin: Option<Arc<Mutex<Option<tokio::process::ChildStdin>>>>,
        hook_callbacks: Arc<Mutex<HashMap<String, HookCallback>>>,
        sdk_mcp_servers: Arc<Mutex<HashMap<String, McpSdkServerConfig>>>,
    ) -> Result<()> {
        let request_id = request.request_id;
        let request_data = request.request;

        let subtype = request_data
            .get("subtype")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ClaudeError::ControlProtocol("Missing subtype".to_string()))?;

        let response_data: serde_json::Value = match subtype {
            "hook_callback" => {
                // Execute hook callback
                let callback_id = request_data
                    .get("callback_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ClaudeError::ControlProtocol("Missing callback_id".to_string())
                    })?;

                let callbacks = hook_callbacks.lock().await;
                let callback = callbacks.get(callback_id).ok_or_else(|| {
                    ClaudeError::ControlProtocol(format!(
                        "Hook callback not found: {}",
                        callback_id
                    ))
                })?;

                // Parse hook input
                let input_json = request_data.get("input").cloned().unwrap_or(json!({}));
                let hook_input: HookInput = serde_json::from_value(input_json).map_err(|e| {
                    ClaudeError::ControlProtocol(format!("Failed to parse hook input: {}", e))
                })?;

                let tool_use_id = request_data
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let context = HookContext::default();

                // Call the hook
                let hook_output = callback(hook_input, tool_use_id, context).await;

                // Convert to JSON
                serde_json::to_value(&hook_output).map_err(|e| {
                    ClaudeError::ControlProtocol(format!("Failed to serialize hook output: {}", e))
                })?
            },
            "mcp_message" => {
                // Handle SDK MCP message
                let server_name = request_data
                    .get("server_name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ClaudeError::ControlProtocol(
                            "Missing server_name for mcp_message".to_string(),
                        )
                    })?;

                let mcp_message = request_data.get("message").ok_or_else(|| {
                    ClaudeError::ControlProtocol("Missing message for mcp_message".to_string())
                })?;

                let mcp_response =
                    Self::handle_sdk_mcp_request(sdk_mcp_servers, server_name, mcp_message.clone())
                        .await?;

                json!({"mcp_response": mcp_response})
            },
            _ => {
                return Err(ClaudeError::ControlProtocol(format!(
                    "Unsupported control request subtype: {}",
                    subtype
                )));
            },
        };

        // Send success response
        let response = json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": response_data
            }
        });

        let response_str = serde_json::to_string(&response)
            .map_err(|e| ClaudeError::Transport(format!("Failed to serialize response: {}", e)))?;

        // Write directly to stdin (bypasses transport lock)
        if let Some(ref stdin_arc) = stdin {
            let mut stdin_guard = stdin_arc.lock().await;
            if let Some(ref mut stdin_stream) = *stdin_guard {
                use tokio::io::AsyncWriteExt;
                stdin_stream
                    .write_all(response_str.as_bytes())
                    .await
                    .map_err(|e| {
                        ClaudeError::Transport(format!("Failed to write control response: {}", e))
                    })?;
                stdin_stream.write_all(b"\n").await.map_err(|e| {
                    ClaudeError::Transport(format!("Failed to write newline: {}", e))
                })?;
                stdin_stream
                    .flush()
                    .await
                    .map_err(|e| ClaudeError::Transport(format!("Failed to flush: {}", e)))?;
            } else {
                return Err(ClaudeError::Transport("stdin not available".to_string()));
            }
        } else {
            return Err(ClaudeError::Transport("stdin not set".to_string()));
        }

        Ok(())
    }

    /// Send control request to CLI
    async fn send_control_request(&self, request: serde_json::Value) -> Result<serde_json::Value> {
        let request_id = format!(
            "req_{}_{}",
            self.request_counter.fetch_add(1, Ordering::SeqCst),
            uuid::Uuid::new_v4().simple()
        );

        // Create oneshot channel for response
        let (tx, rx) = oneshot::channel();
        self.pending_responses
            .lock()
            .await
            .insert(request_id.clone(), tx);

        // Build and send request
        let control_request = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": request
        });

        let request_str = serde_json::to_string(&control_request)
            .map_err(|e| ClaudeError::Transport(format!("Failed to serialize request: {}", e)))?;

        // Write directly to stdin (bypasses transport lock held by background reader)
        if let Some(ref stdin) = self.stdin {
            let mut stdin_guard = stdin.lock().await;
            if let Some(ref mut stdin_stream) = *stdin_guard {
                stdin_stream
                    .write_all(request_str.as_bytes())
                    .await
                    .map_err(|e| {
                        ClaudeError::Transport(format!("Failed to write control request: {}", e))
                    })?;
                stdin_stream.write_all(b"\n").await.map_err(|e| {
                    ClaudeError::Transport(format!("Failed to write newline: {}", e))
                })?;
                stdin_stream
                    .flush()
                    .await
                    .map_err(|e| ClaudeError::Transport(format!("Failed to flush: {}", e)))?;
            } else {
                return Err(ClaudeError::Transport("stdin not available".to_string()));
            }
        } else {
            return Err(ClaudeError::Transport("stdin not set".to_string()));
        }

        // Wait for response
        let response = rx.await.map_err(|_| {
            ClaudeError::ControlProtocol("Control request response channel closed".to_string())
        })?;

        Ok(response)
    }

    /// Receive messages
    #[allow(dead_code)]
    pub async fn receive_messages(&self) -> Vec<serde_json::Value> {
        let mut messages = Vec::new();
        let mut rx = self.message_rx.lock().await;

        while let Some(message) = rx.recv().await {
            messages.push(message);
        }

        messages
    }

    /// Send interrupt signal to Claude
    pub async fn interrupt(&self) -> Result<()> {
        let request = json!({
            "subtype": "interrupt"
        });

        self.send_control_request(request).await?;
        Ok(())
    }

    /// Change permission mode dynamically
    pub async fn set_permission_mode(
        &self,
        mode: crate::types::config::PermissionMode,
    ) -> Result<()> {
        let mode_str = match mode {
            crate::types::config::PermissionMode::Default => "default",
            crate::types::config::PermissionMode::AcceptEdits => "acceptEdits",
            crate::types::config::PermissionMode::Plan => "plan",
            crate::types::config::PermissionMode::BypassPermissions => "bypassPermissions",
            crate::types::config::PermissionMode::DontAsk => "dontAsk",
            crate::types::config::PermissionMode::Auto => "auto",
        };

        let request = json!({
            "subtype": "set_permission_mode",
            "mode": mode_str
        });

        self.send_control_request(request).await?;
        Ok(())
    }

    /// Change AI model dynamically
    pub async fn set_model(&self, model: Option<&str>) -> Result<()> {
        let request = json!({
            "subtype": "set_model",
            "model": model
        });

        self.send_control_request(request).await?;
        Ok(())
    }

    /// Rewind tracked files to their state at a specific user message.
    ///
    /// Requires:
    /// - `enable_file_checkpointing=true` to track file changes
    /// - `extra_args={"replay-user-messages": None}` to receive UserMessage
    ///   objects with `uuid` in the response stream
    ///
    /// # Arguments
    /// * `user_message_id` - UUID of the user message to rewind to. This should be
    ///   the `uuid` field from a `UserMessage` received during the conversation.
    pub async fn rewind_files(&self, user_message_id: &str) -> Result<()> {
        let request = json!({
            "subtype": "rewind_files",
            "user_message_id": user_message_id
        });

        self.send_control_request(request).await?;
        Ok(())
    }

    /// Reconnect a disconnected or failed MCP server.
    ///
    /// # Arguments
    /// * `server_name` - The name of the MCP server to reconnect
    pub async fn reconnect_mcp_server(&self, server_name: &str) -> Result<()> {
        let request = json!({
            "subtype": "mcp_reconnect",
            "serverName": server_name
        });

        self.send_control_request(request).await?;
        Ok(())
    }

    /// Enable or disable an MCP server.
    ///
    /// Disabling a server disconnects it and removes its tools from the
    /// available tool set. Enabling a server reconnects it and makes its
    /// tools available again.
    ///
    /// # Arguments
    /// * `server_name` - The name of the MCP server to toggle
    /// * `enabled` - Whether the server should be enabled
    pub async fn toggle_mcp_server(&self, server_name: &str, enabled: bool) -> Result<()> {
        let request = json!({
            "subtype": "mcp_toggle",
            "serverName": server_name,
            "enabled": enabled
        });

        self.send_control_request(request).await?;
        Ok(())
    }

    /// Stop a running task.
    ///
    /// After this resolves, a `task_notification` system message with status
    /// `stopped` will be emitted by the CLI in the message stream.
    ///
    /// # Arguments
    /// * `task_id` - The task ID from `task_notification` events
    pub async fn stop_task(&self, task_id: &str) -> Result<()> {
        let request = json!({
            "subtype": "stop_task",
            "task_id": task_id
        });

        self.send_control_request(request).await?;
        Ok(())
    }

    /// Get current MCP server connection status.
    ///
    /// Queries the Claude Code CLI for the live connection status of all
    /// configured MCP servers.
    pub async fn get_mcp_status(&self) -> Result<crate::types::mcp::McpStatusResponse> {
        let request = json!({
            "subtype": "mcp_status"
        });

        let response = self.send_control_request(request).await?;
        serde_json::from_value(response).map_err(|e| {
            ClaudeError::ControlProtocol(format!("Failed to parse mcp_status response: {}", e))
        })
    }

    /// Get a breakdown of current context window usage by category.
    ///
    /// Returns the same data shown by the `/context` command in the CLI.
    pub async fn get_context_usage(&self) -> Result<crate::types::mcp::ContextUsageResponse> {
        let request = json!({
            "subtype": "get_context_usage"
        });

        let response = self.send_control_request(request).await?;
        serde_json::from_value(response).map_err(|e| {
            ClaudeError::ControlProtocol(format!(
                "Failed to parse get_context_usage response: {}",
                e
            ))
        })
    }

    /// Get server initialization info
    ///
    /// Returns the initialization result that was obtained during connect().
    /// This includes information about available commands, output styles, and server capabilities.
    pub async fn get_initialization_result(&self) -> Option<serde_json::Value> {
        self.initialization_result.lock().await.clone()
    }

    /// Handle SDK MCP request by routing to the appropriate server
    async fn handle_sdk_mcp_request(
        sdk_mcp_servers: Arc<Mutex<HashMap<String, McpSdkServerConfig>>>,
        server_name: &str,
        message: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let servers = sdk_mcp_servers.lock().await;
        let server_config = servers.get(server_name).ok_or_else(|| {
            ClaudeError::ControlProtocol(format!("SDK MCP server not found: {}", server_name))
        })?;

        // Call the server's handle_message method
        server_config
            .instance
            .handle_message(message)
            .await
            .map_err(|e| ClaudeError::ControlProtocol(format!("MCP server error: {}", e)))
    }

    /// Get buffer metrics from the underlying transport
    ///
    /// Returns `None` if the transport doesn't support buffer metrics.
    /// For `SubprocessTransport`, this returns actual metrics about buffer usage.
    pub async fn get_buffer_metrics(&self) -> Option<crate::internal::transport::BufferMetricsSnapshot> {
        let transport = self.transport.lock().await;
        transport.get_buffer_metrics()
    }
}
