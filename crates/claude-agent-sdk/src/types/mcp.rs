//! MCP (Model Context Protocol) types for Claude Agent SDK

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::errors::Result;

/// MCP servers configuration
#[derive(Clone, Default)]
pub enum McpServers {
    /// No MCP servers
    #[default]
    Empty,
    /// Dictionary of server configurations
    Dict(HashMap<String, McpServerConfig>),
    /// Path to MCP servers configuration file
    Path(PathBuf),
}

/// MCP server configuration
#[derive(Clone)]
pub enum McpServerConfig {
    /// Stdio-based MCP server
    Stdio(McpStdioServerConfig),
    /// SSE-based MCP server
    Sse(McpSseServerConfig),
    /// HTTP-based MCP server
    Http(McpHttpServerConfig),
    /// SDK (in-process) MCP server
    Sdk(McpSdkServerConfig),
}

/// Stdio MCP server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpStdioServerConfig {
    /// Command to execute
    pub command: String,
    /// Command arguments
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// Environment variables
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
}

/// SSE MCP server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpSseServerConfig {
    /// Server URL
    pub url: String,
    /// HTTP headers
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
}

/// HTTP MCP server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpHttpServerConfig {
    /// Server URL
    pub url: String,
    /// HTTP headers
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
}

/// SDK (in-process) MCP server configuration
#[derive(Clone)]
pub struct McpSdkServerConfig {
    /// Server name
    pub name: String,
    /// Server instance
    pub instance: Arc<dyn SdkMcpServer>,
}

/// Trait for SDK MCP server implementations
#[async_trait]
pub trait SdkMcpServer: Send + Sync {
    /// Handle an MCP message
    async fn handle_message(&self, message: serde_json::Value) -> Result<serde_json::Value>;
}

/// Tool handler trait
pub trait ToolHandler: Send + Sync {
    /// Handle a tool invocation
    fn handle(&self, args: serde_json::Value) -> BoxFuture<'static, Result<ToolResult>>;
}

/// Tool result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Result content
    pub content: Vec<ToolResultContent>,
    /// Whether this is an error
    #[serde(default)]
    pub is_error: bool,
}

/// Tool result content types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolResultContent {
    /// Text content
    Text {
        /// Text content
        text: String,
    },
    /// Image content
    Image {
        /// Base64-encoded image data
        data: String,
        /// MIME type
        mime_type: String,
    },
}

/// SDK MCP tool definition
pub struct SdkMcpTool {
    /// Tool name
    pub name: String,
    /// Tool description
    pub description: String,
    /// JSON schema for tool input
    pub input_schema: serde_json::Value,
    /// Tool handler
    pub handler: Arc<dyn ToolHandler>,
}

/// Create an in-process MCP server
pub fn create_sdk_mcp_server(
    name: impl Into<String>,
    version: impl Into<String>,
    tools: Vec<SdkMcpTool>,
) -> McpSdkServerConfig {
    let server = DefaultSdkMcpServer {
        name: name.into(),
        version: version.into(),
        tools: tools.into_iter().map(|t| (t.name.clone(), t)).collect(),
    };

    McpSdkServerConfig {
        name: server.name.clone(),
        instance: Arc::new(server),
    }
}

/// Default implementation of SDK MCP server
struct DefaultSdkMcpServer {
    name: String,
    version: String,
    tools: HashMap<String, SdkMcpTool>,
}

#[async_trait]
impl SdkMcpServer for DefaultSdkMcpServer {
    async fn handle_message(&self, message: serde_json::Value) -> Result<serde_json::Value> {
        // Parse the MCP message
        let method = message["method"]
            .as_str()
            .ok_or_else(|| crate::errors::ClaudeError::Transport("Missing method".to_string()))?;

        match method {
            "initialize" => {
                // Return server info
                Ok(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": self.name,
                        "version": self.version
                    }
                }))
            },
            "tools/list" => {
                // Return list of tools
                let tools: Vec<_> = self
                    .tools
                    .values()
                    .map(|t| {
                        serde_json::json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": t.input_schema
                        })
                    })
                    .collect();

                Ok(serde_json::json!({
                    "tools": tools
                }))
            },
            "tools/call" => {
                // Execute a tool
                let params = &message["params"];
                let tool_name = params["name"].as_str().ok_or_else(|| {
                    crate::errors::ClaudeError::Transport("Missing tool name".to_string())
                })?;
                let arguments = params["arguments"].clone();

                let tool = self.tools.get(tool_name).ok_or_else(|| {
                    crate::errors::ClaudeError::Transport(format!("Tool not found: {}", tool_name))
                })?;

                let result = tool.handler.handle(arguments).await?;

                Ok(serde_json::json!({
                    "content": result.content,
                    "isError": result.is_error
                }))
            },
            _ => Err(crate::errors::ClaudeError::Transport(format!(
                "Unknown method: {}",
                method
            ))),
        }
    }
}

// --- MCP server status types (returned by `ClaudeClient::get_mcp_status`) ---
//
// These mirror the wire-format JSON emitted by the CLI (camelCase field
// names) rather than the in-process `McpServerConfig`/`McpSdkServerConfig`
// types above, which carry a non-serializable `instance` handle and have no
// `claudeai-proxy` variant.

/// SDK MCP server config as returned in status responses.
///
/// Unlike [`McpSdkServerConfig`], which holds the in-process `instance`,
/// this output-only type only carries serializable fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpSdkServerConfigStatus {
    /// Server name
    pub name: String,
}

/// Claude.ai proxy MCP server config.
///
/// Output-only type that appears in status responses for servers proxied
/// through Claude.ai.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpClaudeAIProxyServerConfig {
    /// Proxy URL
    pub url: String,
    /// Proxy identifier
    pub id: String,
}

/// Server configuration as reported in status responses.
///
/// Broader than [`McpServerConfig`]: it includes the output-only
/// `claudeai-proxy` and `sdk` (status) variants and is tagged by the wire
/// `type` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum McpServerStatusConfig {
    /// Stdio-based MCP server
    #[serde(rename = "stdio")]
    Stdio(McpStdioServerConfig),
    /// SSE-based MCP server
    #[serde(rename = "sse")]
    Sse(McpSseServerConfig),
    /// HTTP-based MCP server
    #[serde(rename = "http")]
    Http(McpHttpServerConfig),
    /// SDK (in-process) MCP server
    #[serde(rename = "sdk")]
    Sdk(McpSdkServerConfigStatus),
    /// Claude.ai proxy MCP server
    #[serde(rename = "claudeai-proxy")]
    ClaudeAiProxy(McpClaudeAIProxyServerConfig),
}

/// Tool annotations as returned in MCP server status.
///
/// Wire format uses camelCase field names (from CLI JSON output).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpToolAnnotations {
    /// Whether the tool only reads data
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "readOnly")]
    pub read_only: Option<bool>,
    /// Whether the tool may perform destructive updates
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "destructive")]
    pub destructive: Option<bool>,
    /// Whether the tool interacts with an open-ended external world
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "openWorld")]
    pub open_world: Option<bool>,
}

/// Information about a tool provided by an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    /// Tool name
    pub name: String,
    /// Tool description
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Tool annotations
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotations>,
}

/// Server info from the MCP initialize handshake (available when connected).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerInfo {
    /// Server name
    pub name: String,
    /// Server version
    pub version: String,
}

/// Connection status values for an MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpServerConnectionStatus {
    /// Connected and ready
    Connected,
    /// Connection failed
    Failed,
    /// Requires authentication before connecting
    NeedsAuth,
    /// Connection is in progress
    Pending,
    /// Server has been disabled
    Disabled,
}

/// Status information for an MCP server connection.
///
/// Returned by [`crate::ClaudeClient::get_mcp_status`] in the `mcpServers`
/// list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerStatus {
    /// Server name as configured
    pub name: String,
    /// Current connection status
    pub status: McpServerConnectionStatus,
    /// Server information from the MCP handshake (available when connected)
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "serverInfo")]
    pub server_info: Option<McpServerInfo>,
    /// Error message (available when status is `failed`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Server configuration (includes URL for HTTP/SSE servers)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<McpServerStatusConfig>,
    /// Configuration scope (e.g. project, user, local, claudeai, managed)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Tools provided by this server (available when connected)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<McpToolInfo>>,
}

/// Response from [`crate::ClaudeClient::get_mcp_status`].
///
/// Wraps the list of server statuses under the `mcpServers` key, matching
/// the wire-format response shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpStatusResponse {
    /// Status of every configured MCP server
    #[serde(rename = "mcpServers")]
    pub mcp_servers: Vec<McpServerStatus>,
}

/// A single context usage category (system prompt, tools, messages, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextUsageCategory {
    /// Category name
    pub name: String,
    /// Tokens used by this category
    pub tokens: u64,
    /// Display color for this category
    pub color: String,
    /// Whether this category's loading is deferred
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "isDeferred")]
    pub is_deferred: Option<bool>,
}

/// Response from [`crate::ClaudeClient::get_context_usage`].
///
/// Provides a breakdown of current context window usage by category,
/// matching the data shown by the `/context` command in the CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextUsageResponse {
    /// Token usage broken down by category (system prompt, tools, messages, etc.)
    pub categories: Vec<ContextUsageCategory>,
    /// Total tokens currently in the context window
    #[serde(rename = "totalTokens")]
    pub total_tokens: u64,
    /// Effective maximum tokens (may be reduced by autocompact buffer)
    #[serde(rename = "maxTokens")]
    pub max_tokens: u64,
    /// Raw model context window size
    #[serde(rename = "rawMaxTokens")]
    pub raw_max_tokens: u64,
    /// Percentage of context window used (0-100)
    pub percentage: f64,
    /// Model name the context usage is calculated for
    pub model: String,
    /// Whether autocompact is enabled for this session
    #[serde(rename = "isAutoCompactEnabled")]
    pub is_auto_compact_enabled: bool,
    /// CLAUDE.md and memory files loaded, with path, type, and token counts
    #[serde(rename = "memoryFiles")]
    pub memory_files: Vec<serde_json::Value>,
    /// MCP tools with name, serverName, tokens, and isLoaded status
    #[serde(rename = "mcpTools")]
    pub mcp_tools: Vec<serde_json::Value>,
    /// Agent definitions with agentType, source, and token counts
    pub agents: Vec<serde_json::Value>,
}

/// Macro to create a tool
#[macro_export]
macro_rules! tool {
    ($name:expr, $desc:expr, $schema:expr, $handler:expr) => {{
        struct Handler<F>(F);

        impl<F, Fut> $crate::types::mcp::ToolHandler for Handler<F>
        where
            F: Fn(serde_json::Value) -> Fut + Send + Sync,
            Fut: std::future::Future<Output = anyhow::Result<$crate::types::mcp::ToolResult>>
                + Send
                + 'static,
        {
            fn handle(
                &self,
                args: serde_json::Value,
            ) -> futures::future::BoxFuture<
                'static,
                $crate::errors::Result<$crate::types::mcp::ToolResult>,
            > {
                use futures::FutureExt;
                let f = &self.0;
                let fut = f(args);
                async move { fut.await.map_err(|e| e.into()) }.boxed()
            }
        }

        $crate::types::mcp::SdkMcpTool {
            name: $name.to_string(),
            description: $desc.to_string(),
            input_schema: $schema,
            handler: std::sync::Arc::new(Handler($handler)),
        }
    }};
}
