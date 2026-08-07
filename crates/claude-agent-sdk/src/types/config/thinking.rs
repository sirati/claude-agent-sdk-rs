//! Extended-thinking configuration, effort level, and task budget types.

use serde::{Deserialize, Serialize};
use typed_builder::TypedBuilder;

/// Controls whether thinking text is returned summarized or omitted.
///
/// Opus 4.7+ defaults to `Omitted` (signature-only); pass `Summarized` to
/// receive text.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingDisplay {
    /// Return a summarized version of the thinking text
    Summarized,
    /// Omit thinking text (signature-only)
    Omitted,
}

/// Controls Claude's thinking/reasoning behavior.
///
/// When set, takes precedence over the deprecated `max_thinking_tokens`.
/// See <https://docs.anthropic.com/en/docs/build-with-claude/adaptive-thinking>.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ThinkingConfig {
    /// Claude decides when and how much to think (Opus 4.6+)
    Adaptive {
        /// Whether thinking text is summarized or omitted
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
    /// Fixed thinking token budget (older models)
    Enabled {
        /// Maximum tokens Claude may spend thinking
        budget_tokens: u32,
        /// Whether thinking text is summarized or omitted
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
    /// No extended thinking
    Disabled,
}

/// Controls how much effort Claude puts into its response.
///
/// Works with adaptive thinking to guide thinking depth. See
/// <https://docs.anthropic.com/en/docs/build-with-claude/effort>.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    /// Minimal thinking, fastest responses
    Low,
    /// Moderate thinking
    Medium,
    /// Deep reasoning (default)
    High,
    /// Extended reasoning depth (Opus 4.7 only; falls back to `High` on
    /// other models)
    XHigh,
    /// Maximum effort
    Max,
}

/// API-side task budget in tokens.
///
/// When set, the model is made aware of its remaining token budget so it can
/// pace tool use and wrap up before the limit. Sent as
/// `output_config.task_budget` with the `task-budgets-2026-03-13` beta
/// header.
#[derive(Debug, Clone, Serialize, Deserialize, TypedBuilder)]
#[builder(doc)]
pub struct TaskBudget {
    /// Total token budget for the task
    pub total: u32,
}
