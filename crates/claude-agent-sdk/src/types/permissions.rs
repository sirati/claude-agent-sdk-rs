//! Permission types for Claude Agent SDK

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Callback for tool usage permission
pub type CanUseToolCallback = Arc<
    dyn Fn(String, serde_json::Value, ToolPermissionContext) -> BoxFuture<'static, PermissionResult>
        + Send
        + Sync,
>;

/// Context provided to permission callbacks
#[derive(Debug, Clone, Default)]
pub struct ToolPermissionContext {
    /// Abort signal (future feature)
    pub signal: Option<()>,
    /// Permission suggestions from Claude
    pub suggestions: Vec<PermissionUpdate>,
    /// Unique identifier for this specific tool call within the assistant
    /// message. Always present when delivered to a `can_use_tool` callback.
    pub tool_use_id: Option<String>,
    /// If running within the context of a sub-agent, the sub-agent's ID
    pub agent_id: Option<String>,
    /// The file path that triggered the permission request, if applicable
    /// (e.g. when a Bash command tries to access a path outside allowed
    /// directories)
    pub blocked_path: Option<String>,
    /// Explains why this permission request was triggered. When a
    /// PreToolUse hook returns `permissionDecision: "ask"` with a
    /// `permissionDecisionReason`, that reason is forwarded here.
    pub decision_reason: Option<String>,
    /// Full permission prompt sentence (e.g. "Claude wants to read
    /// foo.txt"). Use this as the primary prompt text when present instead
    /// of reconstructing from tool name + input.
    pub title: Option<String>,
    /// Short noun phrase for the tool action (e.g. "Read file"), suitable
    /// for button labels or compact UI.
    pub display_name: Option<String>,
    /// Human-readable subtitle for the permission UI
    pub description: Option<String>,
}

/// Result of a permission check
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "behavior", rename_all = "lowercase")]
pub enum PermissionResult {
    /// Allow the tool use
    Allow(PermissionResultAllow),
    /// Deny the tool use
    Deny(PermissionResultDeny),
}

/// Permission result for allowing tool use
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionResultAllow {
    /// Updated tool input (if modified)
    #[serde(skip_serializing_if = "Option::is_none", rename = "updatedInput")]
    pub updated_input: Option<serde_json::Value>,
    /// Permission updates to apply
    #[serde(skip_serializing_if = "Option::is_none", rename = "updatedPermissions")]
    pub updated_permissions: Option<Vec<PermissionUpdate>>,
}

/// Permission result for denying tool use
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionResultDeny {
    /// Denial message
    pub message: String,
    /// Whether to interrupt execution
    pub interrupt: bool,
}

impl Default for PermissionResultDeny {
    fn default() -> Self {
        Self {
            message: "Tool use denied".to_string(),
            interrupt: false,
        }
    }
}

/// Permission update to apply
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionUpdate {
    /// Type of update
    #[serde(rename = "type")]
    pub type_: PermissionUpdateType,
    /// Permission rules (for rule updates)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<PermissionRuleValue>>,
    /// Permission behavior (for rule updates)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behavior: Option<PermissionBehavior>,
    /// Permission mode (for mode updates)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<super::config::PermissionMode>,
    /// Directories (for directory updates)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directories: Option<Vec<String>>,
    /// Destination for the update
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<PermissionUpdateDestination>,
}

/// Type of permission update
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PermissionUpdateType {
    /// Add permission rules
    AddRules,
    /// Replace permission rules
    ReplaceRules,
    /// Remove permission rules
    RemoveRules,
    /// Set permission mode
    SetMode,
    /// Add directories
    AddDirectories,
    /// Remove directories
    RemoveDirectories,
}

/// Permission rule value
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRuleValue {
    /// Tool name for this rule
    #[serde(rename = "toolName")]
    pub tool_name: String,
    /// Rule content (optional pattern/constraint)
    #[serde(skip_serializing_if = "Option::is_none", rename = "ruleContent")]
    pub rule_content: Option<String>,
}

/// Permission behavior
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PermissionBehavior {
    /// Allow the action
    Allow,
    /// Deny the action
    Deny,
    /// Ask for permission
    Ask,
}

/// Destination for permission updates
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PermissionUpdateDestination {
    /// User settings
    UserSettings,
    /// Project settings
    ProjectSettings,
    /// Local settings
    LocalSettings,
    /// Current session only
    #[serde(rename = "session")]
    Session,
}

/// Returns the tool that an `allowed_tools` entry allows outright, if any.
///
/// Mirrors the CLI's rule parser: an entry allows a whole tool when it has
/// no `(...)` specifier (`"Read"`), or when the specifier is empty or a
/// lone wildcard (`"Read()"`, `"Read(*)"`). A real specifier
/// (`"Bash(ls:*)"`) only allows matching invocations. Malformed entries
/// (unbalanced parens) match nothing.
fn whole_tool_allowed(entry: &str) -> Option<&str> {
    if entry.trim().is_empty() {
        return None;
    }
    match entry.find('(') {
        None => Some(entry),
        Some(0) => None,
        Some(open) if entry.ends_with(')') => {
            let inner = &entry[open + 1..entry.len() - 1];
            if inner.is_empty() || inner == "*" {
                Some(&entry[..open])
            } else {
                None
            }
        }
        Some(_) => None,
    }
}

/// Computes the `can_use_tool` shadowing advisory for the given options, if
/// any tool call would bypass the callback entirely.
///
/// `can_use_tool` is never invoked for a tool call that `permission_mode` or
/// `allowed_tools` already auto-approve before the callback would run. This
/// mirrors the Python/TypeScript SDKs' `CanUseToolShadowedWarning`; unlike
/// those SDKs (which use a warnings/process-warning mechanism), Rust callers
/// should log the returned message (e.g. via `tracing::warn!`) at the point
/// where `ClaudeAgentOptions` is validated or consumed, once `can_use_tool`
/// is known to be set. See the crate's config/client code for that call
/// site — this function only computes the message.
pub fn can_use_tool_shadowed_warning(
    permission_mode: Option<super::config::PermissionMode>,
    allowed_tools: &[String],
) -> Option<String> {
    if matches!(
        permission_mode,
        Some(super::config::PermissionMode::BypassPermissions)
    ) {
        return Some(
            "can_use_tool will not be invoked: permission_mode \
             'bypassPermissions' auto-approves every tool call (except \
             explicit deny rules) before the callback is consulted. To \
             gate every tool call, use a PreToolUse hook instead."
                .to_string(),
        );
    }

    let mut shadowed: Vec<&str> = Vec::new();
    for entry in allowed_tools {
        if let Some(tool) = whole_tool_allowed(entry) {
            if !shadowed.contains(&tool) {
                shadowed.push(tool);
            }
        }
    }

    if shadowed.is_empty() {
        return None;
    }

    Some(format!(
        "can_use_tool will not be invoked for: {}. An allowed_tools entry \
         that allows a whole tool auto-approves it before the callback is \
         consulted. To gate every tool call, use a PreToolUse hook; or \
         narrow the entry so calls fall through to can_use_tool. Allow \
         rules from settings files can also shadow the callback but are \
         not visible here.",
        shadowed.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_permission_behavior_serialization() {
        assert_eq!(
            serde_json::to_string(&PermissionBehavior::Allow).unwrap(),
            "\"allow\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionBehavior::Deny).unwrap(),
            "\"deny\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionBehavior::Ask).unwrap(),
            "\"ask\""
        );
    }

    #[test]
    fn test_permission_result_allow_serialization() {
        let result = PermissionResult::Allow(PermissionResultAllow {
            updated_input: Some(json!({"modified": true})),
            updated_permissions: None,
        });

        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["behavior"], "allow");
        assert_eq!(json["updatedInput"]["modified"], true);
    }

    #[test]
    fn test_permission_result_deny_serialization() {
        let result = PermissionResult::Deny(PermissionResultDeny {
            message: "Access denied".to_string(),
            interrupt: true,
        });

        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["behavior"], "deny");
        assert_eq!(json["message"], "Access denied");
        assert_eq!(json["interrupt"], true);
    }

    #[test]
    fn test_permission_update_type_serialization() {
        assert_eq!(
            serde_json::to_string(&PermissionUpdateType::AddRules).unwrap(),
            "\"addRules\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionUpdateType::SetMode).unwrap(),
            "\"setMode\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionUpdateType::RemoveDirectories).unwrap(),
            "\"removeDirectories\""
        );
    }

    #[test]
    fn test_permission_update_destination_serialization() {
        assert_eq!(
            serde_json::to_string(&PermissionUpdateDestination::UserSettings).unwrap(),
            "\"userSettings\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionUpdateDestination::ProjectSettings).unwrap(),
            "\"projectSettings\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionUpdateDestination::LocalSettings).unwrap(),
            "\"localSettings\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionUpdateDestination::Session).unwrap(),
            "\"session\""
        );
    }

    #[test]
    fn test_permission_update_with_rules() {
        let update = PermissionUpdate {
            type_: PermissionUpdateType::AddRules,
            rules: Some(vec![PermissionRuleValue {
                tool_name: "Bash".to_string(),
                rule_content: Some("allow echo".to_string()),
            }]),
            behavior: Some(PermissionBehavior::Allow),
            mode: None,
            directories: None,
            destination: Some(PermissionUpdateDestination::Session),
        };

        let json = serde_json::to_value(&update).unwrap();
        assert_eq!(json["type"], "addRules");
        assert_eq!(json["rules"][0]["toolName"], "Bash");
        assert_eq!(json["rules"][0]["ruleContent"], "allow echo");
        assert_eq!(json["behavior"], "allow");
        assert_eq!(json["destination"], "session");
    }

    #[test]
    fn test_permission_update_optional_fields_omitted() {
        let update = PermissionUpdate {
            type_: PermissionUpdateType::SetMode,
            rules: None,
            behavior: None,
            mode: Some(super::super::config::PermissionMode::AcceptEdits),
            directories: None,
            destination: None,
        };

        let json = serde_json::to_value(&update).unwrap();
        assert_eq!(json["type"], "setMode");
        assert!(json.get("rules").is_none());
        assert!(json.get("behavior").is_none());
        assert!(json.get("destination").is_none());
    }

    #[test]
    fn test_shadow_warning_bypass_permissions() {
        let message = can_use_tool_shadowed_warning(
            Some(super::super::config::PermissionMode::BypassPermissions),
            &[],
        );
        assert!(message.unwrap().contains("bypassPermissions"));
    }

    #[test]
    fn test_shadow_warning_whole_tool_allowed() {
        let message = can_use_tool_shadowed_warning(None, &["Read".to_string()]);
        assert!(message.unwrap().contains("Read"));
    }

    #[test]
    fn test_shadow_warning_wildcard_specifier_shadows() {
        let message = can_use_tool_shadowed_warning(None, &["Read(*)".to_string()]);
        assert!(message.unwrap().contains("Read"));
    }

    #[test]
    fn test_shadow_warning_narrow_specifier_does_not_shadow() {
        let message = can_use_tool_shadowed_warning(None, &["Bash(ls:*)".to_string()]);
        assert!(message.is_none());
    }

    #[test]
    fn test_shadow_warning_none_when_default_and_no_allowed_tools() {
        let message = can_use_tool_shadowed_warning(
            Some(super::super::config::PermissionMode::Default),
            &[],
        );
        assert!(message.is_none());
    }
}
