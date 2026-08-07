//! Subprocess transport implementation for Claude Code CLI

use async_trait::async_trait;
use futures::stream::Stream;
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tracing::warn;

use crate::errors::{
    ClaudeError, CliNotFoundError, ConnectionError, JsonDecodeError, ProcessError, Result,
};
use crate::types::config::{ClaudeAgentOptions, DynamicBufferConfig};
use crate::types::messages::UserContentBlock;
use crate::version::{
    ENTRYPOINT, MIN_CLI_VERSION, SDK_VERSION, SKIP_VERSION_CHECK_ENV, check_version,
};

use super::Transport;

use crate::internal::cli_installer::{CliInstaller, InstallProgress};

mod subprocess_windows_safety;
use subprocess_windows_safety::{reject_windows_batch_cli, reject_windows_cmd_metacharacters};

/// Thread-safe buffer usage metrics using atomic operations.
///
/// This struct tracks buffer statistics without requiring locks, using
/// atomic operations for thread safety.
#[derive(Debug, Default)]
pub struct AtomicBufferMetrics {
    /// Peak buffer size used during the session
    peak_size: AtomicUsize,
    /// Total number of messages processed
    message_count: AtomicUsize,
    /// Total bytes processed
    total_bytes: AtomicUsize,
    /// Number of buffer resizes
    resize_count: AtomicUsize,
}

impl AtomicBufferMetrics {
    /// Create new metrics with zero values
    pub fn new() -> Self {
        Self::default()
    }

    /// Update peak size if the new value is larger
    pub fn update_peak(&self, size: usize) {
        use std::sync::atomic::Ordering;
        let mut current = self.peak_size.load(Ordering::Relaxed);
        while size > current {
            match self.peak_size.compare_exchange_weak(
                current,
                size,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    /// Increment message count
    pub fn inc_message_count(&self) {
        self.message_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Add bytes to total
    pub fn add_bytes(&self, bytes: usize) {
        self.total_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Increment resize count
    pub fn inc_resize_count(&self) {
        self.resize_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Get snapshot of all metrics
    pub fn snapshot(&self) -> BufferMetricsSnapshot {
        BufferMetricsSnapshot {
            peak_size: self.peak_size.load(Ordering::Relaxed),
            message_count: self.message_count.load(Ordering::Relaxed),
            total_bytes: self.total_bytes.load(Ordering::Relaxed),
            resize_count: self.resize_count.load(Ordering::Relaxed),
        }
    }

    /// Reset all metrics to zero
    pub fn reset(&self) {
        self.peak_size.store(0, Ordering::Relaxed);
        self.message_count.store(0, Ordering::Relaxed);
        self.total_bytes.store(0, Ordering::Relaxed);
        self.resize_count.store(0, Ordering::Relaxed);
    }
}

/// Snapshot of buffer metrics at a point in time
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct BufferMetricsSnapshot {
    /// Peak buffer size used during the session
    pub peak_size: usize,
    /// Total number of messages processed
    pub message_count: usize,
    /// Total bytes processed
    pub total_bytes: usize,
    /// Number of buffer resizes
    pub resize_count: usize,
}

impl BufferMetricsSnapshot {
    /// Get average message size
    pub fn average_message_size(&self) -> usize {
        if self.message_count == 0 {
            0
        } else {
            self.total_bytes / self.message_count
        }
    }
}

/// Map a `ThinkingDisplay` to the CLI's `--thinking-display` value.
fn thinking_display_str(display: crate::types::config::ThinkingDisplay) -> &'static str {
    use crate::types::config::ThinkingDisplay;
    match display {
        ThinkingDisplay::Summarized => "summarized",
        ThinkingDisplay::Omitted => "omitted",
    }
}

/// Query prompt type
#[derive(Clone)]
pub enum QueryPrompt {
    /// Text prompt (one-shot mode)
    Text(String),
    /// Structured content blocks (supports images and text)
    Content(Vec<UserContentBlock>),
    /// Streaming mode (no initial prompt)
    Streaming,
}

impl From<String> for QueryPrompt {
    fn from(text: String) -> Self {
        QueryPrompt::Text(text)
    }
}

impl From<&str> for QueryPrompt {
    fn from(text: &str) -> Self {
        QueryPrompt::Text(text.to_string())
    }
}

impl From<Vec<UserContentBlock>> for QueryPrompt {
    fn from(blocks: Vec<UserContentBlock>) -> Self {
        QueryPrompt::Content(blocks)
    }
}

/// Subprocess transport for communicating with Claude Code CLI
///
/// # Lock Optimization
///
/// This struct uses direct `Option<T>` for stdin/stdout instead of `Arc<Mutex<Option<T>>>`
/// because:
/// 1. The transport is owned by a single `InternalClient`
/// 2. All Transport trait methods take `&mut self`, providing exclusive access
/// 3. This eliminates lock contention on the hot path (read_messages/write)
///
/// The performance improvement is significant for high-frequency query scenarios:
/// - No lock acquisition overhead (~50-100ns per operation saved)
/// - No cache line bouncing between cores
/// - Simpler code with the same safety guarantees
///
/// For bidirectional control protocol (QueryFull), use `take_stdin_arc()` after connect()
/// to get a shared reference to stdin for concurrent writes.
///
/// # Dynamic Buffer Management
///
/// Uses adaptive buffer sizing that:
/// - Starts with a configurable initial size (default 64KB)
/// - Grows dynamically based on actual message sizes
/// - Tracks metrics for monitoring and tuning
/// - Protects against memory exhaustion with hard limits
pub struct SubprocessTransport {
    cli_path: PathBuf,
    cwd: Option<PathBuf>,
    options: ClaudeAgentOptions,
    prompt: QueryPrompt,
    process: Option<Child>,
    /// Direct stdin access - owned for simple query mode
    stdin: Option<ChildStdin>,
    /// Shared stdin for bidirectional mode - set when take_stdin_arc() is called
    stdin_arc: Option<Arc<Mutex<Option<ChildStdin>>>>,
    /// Direct stdout access - no lock needed as we have exclusive mutable access
    stdout: Option<BufReader<ChildStdout>>,
    /// Dynamic buffer configuration
    buffer_config: DynamicBufferConfig,
    /// Buffer usage metrics (thread-safe atomic counters)
    buffer_metrics: AtomicBufferMetrics,
    ready: bool,
}

impl SubprocessTransport {
    /// Create a new subprocess transport
    pub fn new(prompt: QueryPrompt, options: ClaudeAgentOptions) -> Result<Self> {
        // Validate cwd early, before CLI lookup, for better error messages
        if let Some(ref cwd) = options.cwd {
            if !cwd.exists() {
                return Err(ClaudeError::InvalidConfig(format!(
                    "Working directory does not exist: {}. Please ensure the directory exists before connecting.",
                    cwd.display()
                )));
            }
            if !cwd.is_dir() {
                return Err(ClaudeError::InvalidConfig(format!(
                    "Working directory path is not a directory: {}",
                    cwd.display()
                )));
            }
        }

        let cli_path = if let Some(ref path) = options.cli_path {
            path.clone()
        } else {
            // 尝试查找 CLI，如果失败且启用自动安装，则尝试安装
            Self::find_cli_with_auto_install(&options)?
        };

        // Validate the resolved CLI before anything is spawned with it --
        // this guards the version probe in connect() as well as the main
        // spawn.
        reject_windows_batch_cli(&cli_path)?;

        let cwd = options.cwd.clone().or_else(|| std::env::current_dir().ok());

        // Resolve buffer configuration with backward compatibility
        let buffer_config = if let Some(ref config) = options.buffer_config {
            config.clone()
        } else if let Some(max_size) = options.max_buffer_size {
            // Backward compatibility: use max_buffer_size as max_message_size
            DynamicBufferConfig {
                max_message_size: max_size,
                ..DynamicBufferConfig::default()
            }
        } else {
            DynamicBufferConfig::default()
        };

        Ok(Self {
            cli_path,
            cwd,
            options,
            prompt,
            process: None,
            stdin: None,
            stdin_arc: None,
            stdout: None,
            buffer_config,
            buffer_metrics: AtomicBufferMetrics::new(),
            ready: false,
        })
    }

    /// Take stdin as Arc<Mutex> for bidirectional control protocol.
    ///
    /// This method transfers ownership of stdin to a shared reference that can be
    /// used for concurrent writes while the transport is reading messages.
    /// This is used by QueryFull for bidirectional communication.
    ///
    /// # Returns
    /// - `Some(Arc<Mutex<Option<ChildStdin>>>)` if stdin was available
    /// - `None` if stdin was already taken or not connected
    ///
    /// # Note
    /// After calling this method, direct stdin access via `write()` will fail
    /// because stdin ownership has been transferred to the shared reference.
    pub fn take_stdin_arc(&mut self) -> Option<Arc<Mutex<Option<ChildStdin>>>> {
        if let Some(stdin) = self.stdin.take() {
            let arc = Arc::new(Mutex::new(Some(stdin)));
            self.stdin_arc = Some(Arc::clone(&arc));
            Some(arc)
        } else {
            // Already taken, return the existing arc if available
            self.stdin_arc.clone()
        }
    }

    /// Get buffer usage metrics for monitoring and tuning.
    ///
    /// Returns metrics about buffer usage including peak size, message count,
    /// and resize operations. Only meaningful if `enable_metrics` is true in
    /// the buffer configuration.
    ///
    /// # Example
    /// ```ignore
    /// let metrics = transport.get_buffer_metrics();
    /// println!("Peak buffer size: {} bytes", metrics.peak_size);
    /// println!("Average message size: {} bytes", metrics.average_message_size());
    /// ```
    pub fn get_buffer_metrics(&self) -> BufferMetricsSnapshot {
        self.buffer_metrics.snapshot()
    }

    /// Reset buffer metrics.
    ///
    /// Useful when starting a new session to get fresh metrics.
    pub fn reset_buffer_metrics(&self) {
        self.buffer_metrics.reset();
    }

    /// Find the Claude CLI executable
    fn find_cli() -> Result<PathBuf> {
        // Strategy 1: Try executing 'claude' directly from PATH
        // This is the most reliable method as it respects the shell's PATH resolution
        if let Ok(output) = std::process::Command::new("claude")
            .arg("--version")
            .output()
            && output.status.success()
        {
            // 'claude' is in PATH and executable, return it as-is
            // The OS will resolve it when we spawn the process
            return Ok(PathBuf::from("claude"));
        }

        // Strategy 2: Use 'which' command to locate claude in PATH (Unix-like systems)
        #[cfg(not(target_os = "windows"))]
        if let Ok(output) = std::process::Command::new("which").arg("claude").output()
            && output.status.success()
        {
            let path_str = String::from_utf8_lossy(&output.stdout);
            let path = PathBuf::from(path_str.trim());
            // Verify the path exists and is executable
            if path.exists() && path.is_file() {
                return Ok(path);
            }
        }

        // Strategy 3: Use 'where' command on Windows
        #[cfg(target_os = "windows")]
        if let Ok(output) = std::process::Command::new("where").arg("claude").output() {
            if output.status.success() {
                let path_str = String::from_utf8_lossy(&output.stdout);
                // 'where' returns all matches, take the first one
                if let Some(first_line) = path_str.lines().next() {
                    let path = PathBuf::from(first_line.trim());
                    if path.exists() && path.is_file() {
                        return Ok(path);
                    }
                }
            }
        }

        // Strategy 4: Check common installation locations
        // Get home directory for path expansion
        let home_dir = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE")) // Windows fallback
            .ok()
            .map(PathBuf::from);

        // Common installation locations
        let mut common_paths: Vec<PathBuf> = vec![];

        // Unix-like paths
        #[cfg(not(target_os = "windows"))]
        {
            common_paths.extend(vec![
                PathBuf::from("/usr/local/bin/claude"),
                PathBuf::from("/opt/homebrew/bin/claude"),
                PathBuf::from("/usr/bin/claude"),
            ]);

            // Add home-relative paths if home directory is available
            if let Some(ref home) = home_dir {
                common_paths.push(home.join(".local/bin/claude"));
                common_paths.push(home.join("bin/claude"));
            }
        }

        // Windows paths
        #[cfg(target_os = "windows")]
        {
            if let Some(ref home) = home_dir {
                common_paths.extend(vec![
                    home.join("AppData\\Local\\Programs\\Claude\\claude.exe"),
                    home.join("AppData\\Roaming\\npm\\claude.cmd"),
                    home.join("AppData\\Roaming\\npm\\claude.exe"),
                ]);
            }
            common_paths.extend(vec![
                PathBuf::from("C:\\Program Files\\Claude\\claude.exe"),
                PathBuf::from("C:\\Program Files (x86)\\Claude\\claude.exe"),
            ]);
        }

        // Check each common path
        for path in common_paths {
            if path.exists() && path.is_file() {
                return Ok(path);
            }
        }

        // Strategy 5: Check if CLAUDE_CLI_PATH environment variable is set
        if let Ok(cli_path) = std::env::var("CLAUDE_CLI_PATH") {
            let path = PathBuf::from(cli_path);
            if path.exists() && path.is_file() {
                return Ok(path);
            }
        }

        Err(ClaudeError::CliNotFound(CliNotFoundError::new(
            "Claude Code CLI not found. Please ensure 'claude' is in your PATH or set CLAUDE_CLI_PATH environment variable.",
            None,
        )))
    }

    /// 查找 CLI，支持自动安装
    ///
    /// 首先尝试标准查找，如果失败且启用自动安装，则尝试自动安装
    fn find_cli_with_auto_install(options: &ClaudeAgentOptions) -> Result<PathBuf> {
        // 首先尝试标准查找
        match Self::find_cli() {
            Ok(path) => return Ok(path),
            Err(_) => {
                // CLI 未找到，检查是否启用自动安装
                let auto_install = options.auto_install_cli
                    || std::env::var("CLAUDE_AUTO_INSTALL_CLI")
                        .ok()
                        .and_then(|v| {
                            let v_lower = v.to_lowercase();
                            if v_lower == "true" || v_lower == "1" || v_lower == "yes" {
                                Some(true)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(false);

                if !auto_install {
                    // 未启用自动安装，返回原始错误
                    return Err(ClaudeError::CliNotFound(CliNotFoundError::new(
                        "Claude Code CLI not found. Please ensure 'claude' is in your PATH or set CLAUDE_CLI_PATH environment variable.",
                        None,
                    )));
                }

                // 启用自动安装
                tracing::info!("🔧 CLI not found, auto-install enabled - attempting installation...");
            }
        }

        // 使用 runtime executor 执行异步安装
        // 注意：我们在独立线程中运行，以避免在已有的 tokio runtime 中调用 block_on 导致 panic
        let installer_options = options.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new()
                .map_err(|e| ClaudeError::InternalError(format!("Failed to create runtime: {}", e)))?;

            let installer = CliInstaller::new(true);
            let installer = if let Some(ref callback) = installer_options.cli_install_callback {
                installer.with_progress_callback(callback.clone())
            } else {
                // 默认进度回调：记录日志
                let default_callback = std::sync::Arc::new(|event: InstallProgress| {
                    match event {
                        InstallProgress::Checking(msg) => {
                            tracing::info!("🔍 {}", msg);
                        }
                        InstallProgress::Downloading { current, total } => {
                            if let Some(total) = total {
                                let progress = (current as f64 / total as f64 * 100.0) as u32;
                                tracing::info!("⬇️  Downloading: {}% ({}/{})", progress, current, total);
                            } else {
                                tracing::info!("⬇️  Downloading: {} bytes", current);
                            }
                        }
                        InstallProgress::Installing(msg) => {
                            tracing::info!("🔧 {}", msg);
                        }
                        InstallProgress::Done(path) => {
                            tracing::info!("✅ Installation complete: {}", path.display());
                        }
                        InstallProgress::Failed(err) => {
                            tracing::error!("❌ {}", err);
                        }
                    }
                });
                installer.with_progress_callback(default_callback)
            };

            rt.block_on(installer.install_if_needed())
                .map_err(|e| ClaudeError::InternalError(format!("Auto-install failed: {}", e)))
        })
        .join()
        .map_err(|_| ClaudeError::InternalError("Auto-install thread panicked".to_string()))?
    }

    /// Build command arguments from options
    fn build_command(&self) -> Result<Vec<String>> {
        let mut args = vec![
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ];

        // For streaming mode or content mode, enable stream-json input
        if matches!(
            self.prompt,
            QueryPrompt::Streaming | QueryPrompt::Content(_)
        ) {
            args.push("--input-format".to_string());
            args.push("stream-json".to_string());
        }

        // Add system prompt
        // Note: Python SDK behavior (lines 91-102 of subprocess_cli.py):
        // - If None: skip
        // - If string: use --system-prompt
        // - If preset with append: use --append-system-prompt (NOT --system-prompt-preset)
        //   This relies on default Claude Code prompt and just appends to it
        if let Some(ref system_prompt) = self.options.system_prompt {
            match system_prompt {
                crate::types::config::SystemPrompt::Text(text) => {
                    args.push("--system-prompt".to_string());
                    args.push(text.clone());
                },
                crate::types::config::SystemPrompt::Preset(preset) => {
                    // Only add append if present (uses default Claude Code prompt)
                    if let Some(ref append) = preset.append {
                        args.push("--append-system-prompt".to_string());
                        args.push(append.clone());
                    }
                    // Note: preset.preset field is ignored - CLI uses default prompt
                },
                crate::types::config::SystemPrompt::File(file) => {
                    args.push("--system-prompt-file".to_string());
                    args.push(file.path.clone());
                },
            }
        }

        // Add tools configuration
        if let Some(ref tools) = self.options.tools {
            match tools {
                crate::types::config::Tools::List(tool_list) => {
                    if tool_list.is_empty() {
                        args.push("--tools".to_string());
                        args.push(String::new());
                    } else {
                        args.push("--tools".to_string());
                        args.push(tool_list.join(","));
                    }
                },
                crate::types::config::Tools::Preset(_) => {
                    // Preset object - 'claude_code' preset maps to 'default'
                    args.push("--tools".to_string());
                    args.push("default".to_string());
                },
            }
        }

        // Add permission mode
        if let Some(mode) = self.options.permission_mode {
            let mode_str = match mode {
                crate::types::config::PermissionMode::Default => "default",
                crate::types::config::PermissionMode::AcceptEdits => "acceptEdits",
                crate::types::config::PermissionMode::Plan => "plan",
                crate::types::config::PermissionMode::BypassPermissions => "bypassPermissions",
                crate::types::config::PermissionMode::DontAsk => "dontAsk",
                crate::types::config::PermissionMode::Auto => "auto",
            };
            args.push("--permission-mode".to_string());
            args.push(mode_str.to_string());
        }

        // Only use MCP servers passed via mcp_servers, ignoring other CLI-loaded
        // MCP configuration (project .mcp.json, user/global settings, plugins)
        if self.options.strict_mcp_config {
            args.push("--strict-mcp-config".to_string());
        }

        // Add allowed tools (Python SDK uses --allowedTools with comma-separated values)
        if !self.options.allowed_tools.is_empty() {
            args.push("--allowedTools".to_string());
            args.push(self.options.allowed_tools.join(","));
        }

        // Add disallowed tools (Python SDK uses --disallowedTools with comma-separated values)
        if !self.options.disallowed_tools.is_empty() {
            args.push("--disallowedTools".to_string());
            args.push(self.options.disallowed_tools.join(","));
        }

        // Add model
        if let Some(ref model) = self.options.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }

        // Add fallback model
        if let Some(ref fallback_model) = self.options.fallback_model {
            args.push("--fallback-model".to_string());
            args.push(fallback_model.clone());
        }

        // Add beta features
        if !self.options.betas.is_empty() {
            let betas: Vec<String> = self
                .options
                .betas
                .iter()
                .map(|b| match b {
                    crate::types::config::SdkBeta::Context1M => "context-1m-2025-08-07".to_string(),
                })
                .collect();
            args.push("--betas".to_string());
            args.push(betas.join(","));
        }

        // Add max budget USD
        if let Some(max_budget) = self.options.max_budget_usd {
            args.push("--max-budget-usd".to_string());
            args.push(max_budget.to_string());
        }

        // Add task budget
        if let Some(ref task_budget) = self.options.task_budget {
            args.push("--task-budget".to_string());
            args.push(task_budget.total.to_string());
        }

        // Resolve thinking config -> --thinking / --thinking-display /
        // --max-thinking-tokens. `thinking` takes precedence over the
        // deprecated `max_thinking_tokens`.
        if let Some(ref thinking) = self.options.thinking {
            use crate::types::config::ThinkingConfig;
            match thinking {
                ThinkingConfig::Adaptive { display } => {
                    args.push("--thinking".to_string());
                    args.push("adaptive".to_string());
                    if let Some(display) = display {
                        args.push("--thinking-display".to_string());
                        args.push(thinking_display_str(*display).to_string());
                    }
                },
                ThinkingConfig::Enabled { budget_tokens, display } => {
                    args.push("--max-thinking-tokens".to_string());
                    args.push(budget_tokens.to_string());
                    if let Some(display) = display {
                        args.push("--thinking-display".to_string());
                        args.push(thinking_display_str(*display).to_string());
                    }
                },
                ThinkingConfig::Disabled => {
                    args.push("--thinking".to_string());
                    args.push("disabled".to_string());
                },
            }
        } else if let Some(max_thinking) = self.options.max_thinking_tokens {
            args.push("--max-thinking-tokens".to_string());
            args.push(max_thinking.to_string());
        }

        // Add effort level
        if let Some(effort) = self.options.effort {
            use crate::types::config::EffortLevel;
            let effort_str = match effort {
                EffortLevel::Low => "low",
                EffortLevel::Medium => "medium",
                EffortLevel::High => "high",
                EffortLevel::XHigh => "xhigh",
                EffortLevel::Max => "max",
            };
            args.push("--effort".to_string());
            args.push(effort_str.to_string());
        }

        // Add permission prompt tool name
        if let Some(ref tool_name) = self.options.permission_prompt_tool_name {
            args.push("--permission-prompt-tool".to_string());
            args.push(tool_name.clone());
        }

        // Add output format (structured outputs / JSON schema)
        // Expected format: {"type": "json_schema", "schema": {...}}
        if let Some(ref output_format) = self.options.output_format
            && output_format.get("type") == Some(&serde_json::json!("json_schema"))
            && let Some(schema) = output_format.get("schema")
        {
            args.push("--json-schema".to_string());
            args.push(schema.to_string());
        }

        // Add max turns
        if let Some(max_turns) = self.options.max_turns {
            args.push("--max-turns".to_string());
            args.push(max_turns.to_string());
        }

        // Add resume session. Passed as --flag=value rather than two argv
        // tokens: the CLI declares --resume with an optional value, so in the
        // two-token form a dash-leading value would not bind to the flag and
        // could be parsed as a separate flag instead. The equals form always
        // binds the value to the flag.
        if let Some(ref resume) = self.options.resume {
            reject_windows_cmd_metacharacters("resume", resume)?;
            args.push(format!("--resume={resume}"));
        }

        // Add explicit session ID (same equals-form rationale as --resume)
        if let Some(ref session_id) = self.options.session_id {
            reject_windows_cmd_metacharacters("session_id", session_id)?;
            args.push(format!("--session-id={session_id}"));
        }

        // Add continue conversation
        if self.options.continue_conversation {
            args.push("--continue".to_string());
        }

        // Add settings (combined with sandbox if both are provided)
        let settings_value = self.build_settings_value();
        if let Some(ref settings) = settings_value {
            args.push("--settings".to_string());
            args.push(settings.clone());
        }

        // Add additional directories
        for dir in &self.options.add_dirs {
            args.push("--add-dir".to_string());
            args.push(dir.display().to_string());
        }

        // Tell the CLI about external (stdio/sse/http) MCP servers it needs
        // to spawn/connect to itself. SDK (in-process) servers are handled
        // entirely through the control protocol (see set_sdk_mcp_servers)
        // and are deliberately excluded here.
        if let Some(mcp_config) = self.build_mcp_config_value() {
            args.push("--mcp-config".to_string());
            args.push(mcp_config);
        }

        // Add include partial messages
        if self.options.include_partial_messages {
            args.push("--include-partial-messages".to_string());
        }

        // Add include hook events
        if self.options.include_hook_events {
            args.push("--include-hook-events".to_string());
        }

        // Add fork session
        if self.options.fork_session {
            args.push("--fork-session".to_string());
        }

        // Mirror session transcripts to an external store, if configured.
        // The store itself is consumed out-of-band by the SDK parent (not
        // passed on argv) — this flag just tells the CLI to emit the
        // `transcript_mirror` frames the SDK forwards to `session_store`.
        if self.options.session_store.is_some() {
            args.push("--session-mirror".to_string());
        }

        // Agent definitions are sent via the control-protocol `initialize`
        // request (see `QueryFull::initialize`), matching the TypeScript SDK
        // and upstream Python — no `--agents` CLI flag needed.

        // Add setting sources
        if let Some(ref sources) = self.options.setting_sources {
            let sources_str: Vec<&str> = sources
                .iter()
                .map(|s| match s {
                    crate::types::config::SettingSource::User => "user",
                    crate::types::config::SettingSource::Project => "project",
                    crate::types::config::SettingSource::Local => "local",
                })
                .collect();
            args.push("--setting-sources".to_string());
            args.push(sources_str.join(","));
        }

        // Add plugins
        for plugin in &self.options.plugins {
            if let Some(path) = plugin.path() {
                args.push("--plugin-dir".to_string());
                args.push(path.display().to_string());
            }
        }

        // Add extra args for future CLI flags. Passed as --flag=value rather
        // than two argv tokens when the value starts with '-': the CLI
        // declares some of these flags with an optional value, so in the
        // two-token form a dash-leading value would not bind to the flag
        // and could be parsed as a separate flag instead (the same
        // injection the --resume/--session-id equals-form above closes).
        for (key, value) in &self.options.extra_args {
            match value {
                None => args.push(format!("--{key}")),
                Some(v) if v.starts_with('-') => args.push(format!("--{key}={v}")),
                Some(v) => {
                    args.push(format!("--{key}"));
                    args.push(v.clone());
                },
            }
        }

        Ok(args)
    }

    /// Build the `--mcp-config` argv value from `options.mcp_servers`.
    ///
    /// SDK (in-process) servers are stripped out: the CLI never spawns or
    /// connects to them itself, so mirroring Python's `servers_for_cli`
    /// filtering, only stdio/sse/http configs are serialized. A raw
    /// string/path value is passed straight through, matching Python's
    /// non-dict branch. Returns `None` when there is nothing left to send.
    fn build_mcp_config_value(&self) -> Option<String> {
        use crate::types::mcp::{McpServerConfig, McpServers};

        match &self.options.mcp_servers {
            McpServers::Empty => None,
            McpServers::Path(path) => Some(path.display().to_string()),
            McpServers::Dict(servers) => {
                let servers_for_cli: serde_json::Map<String, serde_json::Value> = servers
                    .iter()
                    .filter_map(|(name, config)| {
                        let value = match config {
                            McpServerConfig::Stdio(cfg) => {
                                let mut v = serde_json::to_value(cfg).ok()?;
                                v["type"] = serde_json::json!("stdio");
                                v
                            },
                            McpServerConfig::Sse(cfg) => {
                                let mut v = serde_json::to_value(cfg).ok()?;
                                v["type"] = serde_json::json!("sse");
                                v
                            },
                            McpServerConfig::Http(cfg) => {
                                let mut v = serde_json::to_value(cfg).ok()?;
                                v["type"] = serde_json::json!("http");
                                v
                            },
                            // SDK servers are in-process and are handled via
                            // set_sdk_mcp_servers()/the control protocol, not
                            // the CLI subprocess.
                            McpServerConfig::Sdk(_) => return None,
                        };
                        Some((name.clone(), value))
                    })
                    .collect();

                if servers_for_cli.is_empty() {
                    None
                } else {
                    let payload = serde_json::json!({ "mcpServers": servers_for_cli });
                    Some(payload.to_string())
                }
            },
        }
    }

    /// Build settings value, merging sandbox settings if provided.
    ///
    /// Returns the settings value as either:
    /// - A JSON string (if sandbox is provided or settings is JSON)
    /// - A file path (if only settings path is provided without sandbox)
    /// - None if neither settings nor sandbox is provided
    fn build_settings_value(&self) -> Option<String> {
        let has_settings = self.options.settings.is_some();
        let has_sandbox = self.options.sandbox.is_some();

        if !has_settings && !has_sandbox {
            return None;
        }

        // If only settings path and no sandbox, pass through as-is
        if has_settings && !has_sandbox {
            return self.options.settings.clone();
        }

        // If we have sandbox settings, we need to merge into a JSON object
        let mut settings_obj = serde_json::Map::new();

        if let Some(settings_str) = &self.options.settings {
            let trimmed = settings_str.trim();
            // Check if settings is a JSON string or a file path
            if trimmed.starts_with('{') && trimmed.ends_with('}') {
                // Parse JSON string
                if let Ok(serde_json::Value::Object(obj)) =
                    serde_json::from_str::<serde_json::Value>(trimmed)
                {
                    settings_obj = obj;
                }
            } else {
                // It's a file path - try to read and parse
                if let Ok(content) = std::fs::read_to_string(trimmed)
                    && let Ok(serde_json::Value::Object(obj)) =
                        serde_json::from_str::<serde_json::Value>(&content)
                {
                    settings_obj = obj;
                }
            }
        }

        // Merge sandbox settings
        if let Some(sandbox) = &self.options.sandbox
            && let Ok(sandbox_value) = serde_json::to_value(sandbox)
        {
            settings_obj.insert("sandbox".to_string(), sandbox_value);
        }

        Some(serde_json::to_string(&serde_json::Value::Object(settings_obj)).unwrap_or_default())
    }

    /// Check Claude CLI version
    async fn check_claude_version(&self) -> Result<()> {
        // Skip if environment variable is set
        if std::env::var(SKIP_VERSION_CHECK_ENV).is_ok() {
            return Ok(());
        }

        let output = Command::new(&self.cli_path)
            .arg("--version")
            .output()
            .await
            .map_err(|e| {
                ClaudeError::Connection(ConnectionError::new(format!(
                    "Failed to get Claude version: {}",
                    e
                )))
            })?;

        let version_output = String::from_utf8_lossy(&output.stdout);
        let version = version_output
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().next())
            .unwrap_or("")
            .trim();

        if !check_version(version) {
            warn!(
                "Claude Code CLI ({}) version {} is below minimum required version {}. Some features may not work correctly.",
                self.cli_path.display(),
                version,
                MIN_CLI_VERSION
            );
        }

        Ok(())
    }

    /// Build environment variables
    fn build_env(&self) -> HashMap<String, String> {
        let mut env = self.options.env.clone();
        env.insert("CLAUDE_CODE_ENTRYPOINT".to_string(), ENTRYPOINT.to_string());
        env.insert(
            "CLAUDE_AGENT_SDK_VERSION".to_string(),
            SDK_VERSION.to_string(),
        );

        // Enable file checkpointing if requested
        if self.options.enable_file_checkpointing {
            env.insert(
                "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING".to_string(),
                "true".to_string(),
            );
        }

        env
    }
}

#[async_trait]
impl Transport for SubprocessTransport {
    async fn connect(&mut self) -> Result<()> {
        // Note: cwd validation is done in new() for early error detection

        // Check version
        self.check_claude_version().await?;

        // Build command
        let args = self.build_command()?;
        let env = self.build_env();

        // Build command
        let mut cmd = Command::new(&self.cli_path);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .envs(&env);

        if let Some(ref cwd) = self.cwd {
            cmd.current_dir(cwd);
        }

        // Spawn process
        let mut child = cmd.spawn().map_err(|e| {
            ClaudeError::Process(ProcessError::new(
                format!("Failed to spawn Claude CLI process: {}", e),
                None,
                None,
            ))
        })?;

        // Take stdin and stdout
        let stdin = child.stdin.take().ok_or_else(|| {
            ClaudeError::Connection(ConnectionError::new("Failed to get stdin".to_string()))
        })?;

        let stdout = child.stdout.take().ok_or_else(|| {
            ClaudeError::Connection(ConnectionError::new("Failed to get stdout".to_string()))
        })?;

        let stderr = child.stderr.take();

        // Spawn stderr handler if callback is provided
        if let (Some(stderr), Some(callback)) = (stderr, &self.options.stderr_callback) {
            let callback = Arc::clone(callback);
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                while let Ok(n) = reader.read_line(&mut line).await {
                    if n == 0 {
                        break;
                    }
                    callback(line.clone());
                    line.clear();
                }
            });
        }

        self.stdin = Some(stdin);
        self.stdout = Some(BufReader::new(stdout));
        self.process = Some(child);
        self.ready = true;

        // Send initial prompt based on type
        match &self.prompt {
            QueryPrompt::Text(text) => {
                let text_owned = text.clone();
                self.write(&text_owned).await?;
                self.end_input().await?;
            },
            QueryPrompt::Content(blocks) => {
                // Format as JSON user message for stream-json input format
                let user_message = serde_json::json!({
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": blocks
                    }
                });
                let content_json = serde_json::to_string(&user_message).map_err(|e| {
                    ClaudeError::Transport(format!("Failed to serialize content blocks: {}", e))
                })?;
                self.write(&content_json).await?;
                self.end_input().await?;
            },
            QueryPrompt::Streaming => {
                // Don't send initial prompt or close stdin - leave it open for streaming
            },
        }

        Ok(())
    }

    async fn write(&mut self, data: &str) -> Result<()> {
        // Try direct stdin first (simple mode)
        if let Some(ref mut stdin) = self.stdin {
            stdin
                .write_all(data.as_bytes())
                .await
                .map_err(|e| ClaudeError::Transport(format!("Failed to write to stdin: {}", e)))?;
            stdin
                .write_all(b"\n")
                .await
                .map_err(|e| ClaudeError::Transport(format!("Failed to write newline: {}", e)))?;
            stdin
                .flush()
                .await
                .map_err(|e| ClaudeError::Transport(format!("Failed to flush stdin: {}", e)))?;
            return Ok(());
        }

        // Fall back to shared stdin (bidirectional mode)
        if let Some(ref stdin_arc) = self.stdin_arc {
            let mut stdin_guard = stdin_arc.lock().await;
            if let Some(ref mut stdin) = *stdin_guard {
                stdin
                    .write_all(data.as_bytes())
                    .await
                    .map_err(|e| ClaudeError::Transport(format!("Failed to write to stdin: {}", e)))?;
                stdin
                    .write_all(b"\n")
                    .await
                    .map_err(|e| ClaudeError::Transport(format!("Failed to write newline: {}", e)))?;
                stdin
                    .flush()
                    .await
                    .map_err(|e| ClaudeError::Transport(format!("Failed to flush stdin: {}", e)))?;
                return Ok(());
            }
        }

        Err(ClaudeError::Transport("stdin not available".to_string()))
    }

    fn read_messages(
        &mut self,
    ) -> Pin<Box<dyn Stream<Item = Result<serde_json::Value>> + Send + '_>> {
        let max_message_size = self.buffer_config.max_message_size;
        let enable_metrics = self.buffer_config.enable_metrics;

        Box::pin(async_stream::stream! {
            if let Some(ref mut reader) = self.stdout {
                let mut line = String::with_capacity(self.buffer_config.initial_size);
                let mut current_capacity = self.buffer_config.initial_size;

                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => {
                            // EOF
                            break;
                        }
                        Ok(bytes_read) => {
                            // Per-message size check (not cumulative)
                            if bytes_read > max_message_size {
                                yield Err(ClaudeError::Transport(format!(
                                    "Message size {} bytes exceeded maximum of {} bytes",
                                    bytes_read, max_message_size
                                )));
                                break;
                            }

                            // Track metrics if enabled
                            if enable_metrics {
                                self.buffer_metrics.inc_message_count();
                                self.buffer_metrics.add_bytes(bytes_read);
                                self.buffer_metrics.update_peak(bytes_read);
                                // Check if we need to grow the buffer
                                if bytes_read > current_capacity {
                                    let new_capacity = (current_capacity as f64 * self.buffer_config.growth_factor) as usize;
                                    current_capacity = new_capacity.min(max_message_size);
                                    self.buffer_metrics.inc_resize_count();
                                }
                            }

                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }

                            match serde_json::from_str::<serde_json::Value>(trimmed) {
                                Ok(json) => {
                                    yield Ok(json);
                                }
                                Err(e) => {
                                    yield Err(ClaudeError::JsonDecode(JsonDecodeError::new(
                                        format!("Failed to parse JSON: {}", e),
                                        trimmed.to_string(),
                                    )));
                                }
                            }
                        }
                        Err(e) => {
                            yield Err(ClaudeError::Transport(format!("Failed to read line: {}", e)));
                            break;
                        }
                    }
                }
            }
        })
    }

    fn read_raw_messages(
        &mut self,
    ) -> Pin<Box<dyn Stream<Item = Result<String>> + Send + '_>> {
        let max_message_size = self.buffer_config.max_message_size;
        let enable_metrics = self.buffer_config.enable_metrics;

        Box::pin(async_stream::stream! {
            if let Some(ref mut reader) = self.stdout {
                let mut line = String::with_capacity(self.buffer_config.initial_size);
                let mut current_capacity = self.buffer_config.initial_size;

                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => {
                            // EOF
                            break;
                        }
                        Ok(bytes_read) => {
                            // Per-message size check (not cumulative)
                            if bytes_read > max_message_size {
                                yield Err(ClaudeError::Transport(format!(
                                    "Message size {} bytes exceeded maximum of {} bytes",
                                    bytes_read, max_message_size
                                )));
                                break;
                            }

                            // Track metrics if enabled
                            if enable_metrics {
                                self.buffer_metrics.inc_message_count();
                                self.buffer_metrics.add_bytes(bytes_read);
                                self.buffer_metrics.update_peak(bytes_read);
                                // Check if we need to grow the buffer
                                if bytes_read > current_capacity {
                                    let new_capacity = (current_capacity as f64 * self.buffer_config.growth_factor) as usize;
                                    current_capacity = new_capacity.min(max_message_size);
                                    self.buffer_metrics.inc_resize_count();
                                }
                            }

                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }

                            // Return the raw string - caller will parse
                            yield Ok(trimmed.to_string());
                        }
                        Err(e) => {
                            yield Err(ClaudeError::Transport(format!("Failed to read line: {}", e)));
                            break;
                        }
                    }
                }
            }
        })
    }

    async fn close(&mut self) -> Result<()> {
        // Close direct stdin (simple mode)
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.shutdown().await;
        }

        // Close shared stdin (bidirectional mode)
        if let Some(ref stdin_arc) = self.stdin_arc {
            let mut stdin_guard = stdin_arc.lock().await;
            if let Some(mut stdin) = stdin_guard.take() {
                let _ = stdin.shutdown().await;
            }
        }

        // Wait for process to exit
        if let Some(mut process) = self.process.take() {
            let status = process.wait().await.map_err(|e| {
                ClaudeError::Process(ProcessError::new(
                    format!("Failed to wait for process: {}", e),
                    None,
                    None,
                ))
            })?;

            if !status.success() {
                return Err(ClaudeError::Process(ProcessError::new(
                    "Claude CLI exited with non-zero status".to_string(),
                    status.code(),
                    None,
                )));
            }
        }

        self.ready = false;
        Ok(())
    }

    fn is_ready(&self) -> bool {
        self.ready
    }

    async fn end_input(&mut self) -> Result<()> {
        // Close direct stdin (simple mode)
        if let Some(mut stdin) = self.stdin.take() {
            stdin
                .shutdown()
                .await
                .map_err(|e| ClaudeError::Transport(format!("Failed to close stdin: {}", e)))?;
            return Ok(());
        }

        // Close shared stdin (bidirectional mode)
        if let Some(ref stdin_arc) = self.stdin_arc {
            let mut stdin_guard = stdin_arc.lock().await;
            if let Some(mut stdin) = stdin_guard.take() {
                stdin
                    .shutdown()
                    .await
                    .map_err(|e| ClaudeError::Transport(format!("Failed to close stdin: {}", e)))?;
            }
        }
        Ok(())
    }

    fn get_buffer_metrics(&self) -> Option<BufferMetricsSnapshot> {
        Some(self.buffer_metrics.snapshot())
    }
}

impl Drop for SubprocessTransport {
    fn drop(&mut self) {
        if let Some(mut process) = self.process.take() {
            let _ = process.start_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::config::{ClaudeAgentOptions, EffortLevel, TaskBudget, ThinkingConfig};
    use crate::types::mcp::{McpServerConfig, McpServers, McpSdkServerConfig, McpStdioServerConfig};
    use std::collections::HashMap;

    /// Avoids CLI lookup during construction: `cli_path` is set explicitly, so
    /// `SubprocessTransport::new()` never has to probe PATH.
    fn transport_with(options: ClaudeAgentOptions) -> SubprocessTransport {
        SubprocessTransport::new(QueryPrompt::Streaming, options).expect("transport")
    }

    #[test]
    fn build_command_includes_strict_mcp_config_session_id_and_hook_events() {
        let options = ClaudeAgentOptions::builder()
            .cli_path(PathBuf::from("claude"))
            .strict_mcp_config(true)
            .session_id("11111111-1111-1111-1111-111111111111")
            .include_hook_events(true)
            .build();
        let args = transport_with(options).build_command().expect("build_command");

        assert!(args.contains(&"--strict-mcp-config".to_string()));
        assert!(args.contains(&"--session-id=11111111-1111-1111-1111-111111111111".to_string()));
        assert!(args.contains(&"--include-hook-events".to_string()));
    }

    #[test]
    fn build_command_includes_effort_and_task_budget() {
        let options = ClaudeAgentOptions::builder()
            .cli_path(PathBuf::from("claude"))
            .effort(EffortLevel::XHigh)
            .task_budget(TaskBudget { total: 5000 })
            .build();
        let args = transport_with(options).build_command().expect("build_command");

        assert!(args.windows(2).any(|w| w == ["--effort", "xhigh"]));
        assert!(args.windows(2).any(|w| w == ["--task-budget", "5000"]));
    }

    #[test]
    fn build_command_thinking_takes_precedence_over_max_thinking_tokens() {
        let options = ClaudeAgentOptions::builder()
            .cli_path(PathBuf::from("claude"))
            .max_thinking_tokens(1000)
            .thinking(ThinkingConfig::Enabled {
                budget_tokens: 4096,
                display: None,
            })
            .build();
        let args = transport_with(options).build_command().expect("build_command");

        assert!(args.windows(2).any(|w| w == ["--max-thinking-tokens", "4096"]));
        assert!(!args.contains(&"1000".to_string()));
    }

    #[test]
    fn build_command_resume_uses_equals_form() {
        let options = ClaudeAgentOptions::builder()
            .cli_path(PathBuf::from("claude"))
            .resume("some-session")
            .build();
        let args = transport_with(options).build_command().expect("build_command");

        assert!(args.contains(&"--resume=some-session".to_string()));
        assert!(!args.contains(&"--resume".to_string()));
    }

    #[test]
    fn build_command_session_mirror_flag_follows_session_store() {
        let without_store = ClaudeAgentOptions::builder()
            .cli_path(PathBuf::from("claude"))
            .build();
        let args = transport_with(without_store).build_command().expect("build_command");
        assert!(!args.contains(&"--session-mirror".to_string()));

        let with_store = ClaudeAgentOptions::builder()
            .cli_path(PathBuf::from("claude"))
            .session_store(std::sync::Arc::new(crate::session::InMemorySessionStore::new())
                as std::sync::Arc<dyn crate::session::SessionStore>)
            .build();
        let args = transport_with(with_store).build_command().expect("build_command");
        assert!(args.contains(&"--session-mirror".to_string()));
    }

    /// SDK (in-process) servers must never reach the CLI's argv -- they are
    /// handled entirely through the control protocol. Only the external
    /// (stdio/sse/http) servers belong in `--mcp-config`.
    #[test]
    fn build_command_mcp_config_includes_stdio_and_excludes_sdk_servers() {
        let mut servers = HashMap::new();
        servers.insert(
            "stdio-server".to_string(),
            McpServerConfig::Stdio(McpStdioServerConfig {
                command: "some-command".to_string(),
                args: Some(vec!["--flag".to_string()]),
                env: None,
            }),
        );
        servers.insert(
            "sdk-server".to_string(),
            McpServerConfig::Sdk(McpSdkServerConfig {
                name: "sdk-server".to_string(),
                instance: std::sync::Arc::new(NoopSdkMcpServer),
            }),
        );

        let options = ClaudeAgentOptions::builder()
            .cli_path(PathBuf::from("claude"))
            .mcp_servers(McpServers::Dict(servers))
            .build();
        let args = transport_with(options).build_command().expect("build_command");

        let idx = args
            .iter()
            .position(|a| a == "--mcp-config")
            .expect("--mcp-config present");
        let value = &args[idx + 1];
        let parsed: serde_json::Value = serde_json::from_str(value).expect("valid json");
        let mcp_servers = parsed["mcpServers"].as_object().expect("mcpServers object");

        assert_eq!(mcp_servers.len(), 1);
        assert_eq!(mcp_servers["stdio-server"]["type"], "stdio");
        assert_eq!(mcp_servers["stdio-server"]["command"], "some-command");
        assert!(!mcp_servers.contains_key("sdk-server"));
    }

    /// No-op stub used only to construct an `McpServerConfig::Sdk` for the
    /// filtering test above.
    struct NoopSdkMcpServer;

    #[async_trait]
    impl crate::types::mcp::SdkMcpServer for NoopSdkMcpServer {
        async fn handle_message(&self, _message: serde_json::Value) -> Result<serde_json::Value> {
            Ok(serde_json::Value::Null)
        }
    }

    /// A dash-leading extra-arg value must bind to its flag via the equals
    /// form; the two-token form would let the CLI's arg parser see it as an
    /// independent flag instead of the value.
    #[test]
    fn build_command_extra_args_dash_value_uses_equals_form() {
        let mut extra_args = HashMap::new();
        extra_args.insert("future-flag".to_string(), Some("-injected".to_string()));
        extra_args.insert("bool-flag".to_string(), None);

        let options = ClaudeAgentOptions::builder()
            .cli_path(PathBuf::from("claude"))
            .extra_args(extra_args)
            .build();
        let args = transport_with(options).build_command().expect("build_command");

        assert!(args.contains(&"--future-flag=-injected".to_string()));
        assert!(!args.contains(&"--future-flag".to_string()));
        assert!(args.contains(&"--bool-flag".to_string()));
    }

    #[test]
    fn build_command_extra_args_plain_value_uses_two_tokens() {
        let mut extra_args = HashMap::new();
        extra_args.insert("future-flag".to_string(), Some("plain".to_string()));

        let options = ClaudeAgentOptions::builder()
            .cli_path(PathBuf::from("claude"))
            .extra_args(extra_args)
            .build();
        let args = transport_with(options).build_command().expect("build_command");

        let idx = args.iter().position(|a| a == "--future-flag").expect("flag present");
        assert_eq!(args[idx + 1], "plain");
    }
}
