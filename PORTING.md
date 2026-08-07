# Porting upstream Python SDK changes into the Rust fork

## Baseline

- Python upstream repo: `/home/sirati/.claude/jobs/00d0dd10/tmp/py`
- Rust fork repo: `/home/sirati/.claude/jobs/00d0dd10/tmp/fork` (this repo), branch `port-upstream-2026-08`
- Determined baseline (the Python commit this fork's Rust source actually corresponds
  to): **`d36db81` ("docs: update changelog for v0.1.39", 2026-02-20)**, i.e. the last
  Python commit *before* `146e3d6` ("fix: handle unknown message types gracefully
  instead of crashing (#598)"). Evidence:
  - The fork lacks `AssistantMessage.error` population fix (#506, Feb 3) in spirit but
    does have an `error` field, so it is roughly Feb-era.
  - The fork has no `McpToolAnnotations` (#551) and no expanded hook events beyond
    `PreToolUse/PostToolUse/UserPromptSubmit/Stop/SubagentStop/PreCompact` (#545) at
    the type level even though python had them by Feb 3 — meaning fork predates or is
    contemporaneous with early Feb Python, OR (more likely) the fork author only ported
    a subset. Decisive evidence: the fork's message parser is fatal-on-unknown-type,
    i.e. it precedes `146e3d6` exactly, which landed 2026-02-20, one day before the
    fork's last commit date (2026-02-21). This is the single sharpest dividing line
    available, so `146e3d6^` = `d36db81` is used as baseline.
- Tip: `6c754df1c44c6429df4d5277337e94af4ddf0499` (v0.2.132, 2026-08-07)
- Range: `d36db81..6c754df`, 332 commits touching `src/`, +8233/-341 lines across
  22 Python files.

Diff any file with:
```
cd /home/sirati/.claude/jobs/00d0dd10/tmp/py && git diff d36db81 6c754df -- <path>
```

## Nix dev shell

`flake.nix` / `devel.nix` added at the fork repo root (mirrors the pattern used in
the learngraph repo: rust-overlay latest nightly + cargo-nextest + rust-analyzer).
Not part of the port itself — kept as an untracked convenience unless the user wants
it committed. Use:
```
cd /home/sirati/.claude/jobs/00d0dd10/tmp/fork
git add -N flake.nix devel.nix   # only needed because flakes require tracked files
nix develop --command bash -c '<cargo command>'
```

## Tier 1 — correctness-critical, in scope for this pass

- [ ] **Slice 1: message parser forward-compatibility + new message/content types**
  - Python: `src/claude_agent_sdk/_internal/message_parser.py`, `types.py` (message /
    content block classes only)
  - Rust: `crates/claude-agent-sdk/src/internal/message_parser.rs`,
    `crates/claude-agent-sdk/src/types/messages.rs`
  - Must fix: unknown `type` must not be a fatal deserialize error (the confirmed
    `rate_limit_event` crash). Must add: `RateLimitEvent`/`RateLimitInfo`,
    `ServerToolUseBlock`/`ServerToolResultBlock` content blocks, `TaskStartedMessage`/
    `TaskProgressMessage`/`TaskNotificationMessage`/`TaskUpdatedMessage`,
    `MirrorErrorMessage`, `HookEventMessage`, `DeferredToolUse`, `ModelUsage`,
    new `AssistantMessage` fields (`usage`, `id`, `stop_reason`, `session_id`, `uuid`),
    new `ResultMessage` fields (`stop_reason`, `modelUsage`, `permission_denials`,
    `deferred_tool_use`, `errors`, `api_error_status`, `uuid`, `terminal_reason`).

- [ ] **Slice 2: `ClaudeAgentOptions` fields + CLI argv construction**
  - Python: `types.py` (`ClaudeAgentOptions` class only),
    `_internal/transport/subprocess_cli.py`
  - Rust: `crates/claude-agent-sdk/src/types/config.rs`,
    `crates/claude-agent-sdk/src/internal/transport/subprocess.rs`
  - New fields/flags identified: `tools` (base tool preset), `SystemPromptFile`
    variant, `strict_mcp_config` (`--strict-mcp-config`), `session_id`,
    `fallback_model`, `add_dirs`, `settings` path, `plugins`, sandbox settings,
    `thinking` union (`adaptive`/`enabled`/`disabled`, `--thinking`,
    `--thinking-display`), `effort` gains `"xhigh"`, `output_format`,
    `--include-hook-events`, `--max-thinking-tokens`, `--system-prompt-file`,
    `--task-budget` (+ `TaskBudget` type), permission mode gains `"dontAsk"`.
  - Explicitly excluded from this slice: `session_store`, `session_store_flush`,
    `load_timeout_ms` — belongs to Slice 5 (session-store subsystem).

- [ ] **Slice 3: hooks & permissions**
  - Python: `types.py` (`*HookInput`, `HookEvent`, permission-related classes),
    `client.py` (`CanUseToolShadowedWarning` / shadow-warning helpers)
  - Rust: `crates/claude-agent-sdk/src/types/hooks.rs`,
    `crates/claude-agent-sdk/src/types/permissions.rs`
  - New: `_SubagentContextMixin` fields added to `PreToolUseHookInput`,
    `PostToolUseHookInput`, `PostToolUseFailureHookInput`,
    `PermissionRequestHookInput`; any new `HookEvent` variants beyond the 6 the fork
    has; `HookEventMessage` plumbing (routed via Slice 1's parser, but the hook-event
    subtype constants/types belong here); permission mode `"dontAsk"`.

- [ ] **Slice 4: MCP status/control + control-protocol client methods**
  - Python: `client.py` (`reconnect_mcp_server`, `toggle_mcp_server`, `stop_task`,
    `get_mcp_status`, `get_context_usage`), `types.py` (`McpToolAnnotations`,
    `McpServerInfo`, `McpServerStatus`, `McpStatusResponse`, `ContextUsageCategory`,
    `ContextUsageResponse`, `SDKControlMcpReconnectRequest`,
    `SDKControlMcpToggleRequest`, `SDKControlStopTaskRequest`),
    `_internal/query.py` (control-protocol request/response plumbing)
  - Rust: `crates/claude-agent-sdk/src/client.rs`,
    `crates/claude-agent-sdk/src/internal/client.rs`,
    `crates/claude-agent-sdk/src/internal/query_full.rs`,
    `crates/claude-agent-sdk/src/types/mcp.rs`
  - The fork currently has **no** `get_mcp_status` at all (not even the old
    untyped version) — this is wholly new surface, not just a signature change.

## Tier 2 — large new subsystem, tracked separately

- [ ] **Slice 5: session store / resume / mutation subsystem**
  - Python (all new since baseline): `_internal/session_import.py`,
    `_internal/session_mutations.py`, `_internal/session_resume.py`,
    `_internal/session_store.py`, `_internal/session_store_validation.py`,
    `_internal/session_summary.py`, `_internal/sessions.py` (1925 lines, entirely
    new), `_internal/transcript_mirror_batcher.py`, `testing/session_store_conformance.py`
  - This is a ~4000-line new pluggable session-persistence layer (`SessionStore`
    protocol, transcript mirroring, fork/rename/tag/delete session operations,
    resume-from-store materialization, session summaries). It did not exist at the
    fork's baseline at all.
  - Decision: attempt a solid-effort port of the core `SessionStore` trait + types +
    the `ClaudeAgentOptions.session_store`/`session_store_flush`/`load_timeout_ms`
    wiring, but the full mutation/resume/replay engine is a project-sized feature in
    its own right. Track actual completion honestly below rather than claiming 100%
    if it isn't.

## Deliberately skipped (not applicable to Rust)

- Packaging/version bump commits (`chore: release vX.Y.Z`, `chore: bump bundled CLI
  version`) — no Rust equivalent artifact.
- Python typing-only changes, `.github` workflow changes, docs-only commits,
  Python-specific async/`_task_compat.py` shims (TaskGroup polyfills) — Rust's
  async model (tokio) doesn't need them.
- `testing/` conformance-test-only Python helpers beyond what's needed to validate
  the Rust `SessionStore` trait shape (ported as Rust tests instead of 1:1 files).

## Status log

(Updated as slices land — see commit history on this branch.)
