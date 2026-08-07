# Porting upstream Python SDK changes into the Rust fork

## Baseline

- Python upstream repo: `/home/sirati/.claude/jobs/00d0dd10/tmp/py`
- Rust fork repo: `/home/sirati/.claude/jobs/00d0dd10/tmp/fork` (this repo), branch `port-upstream-2026-08`
- Determined baseline (the Python commit this fork's Rust source actually corresponds
  to): **`d36db81` ("docs: update changelog for v0.1.39", 2026-02-20)**, i.e. the last
  Python commit *before* `146e3d6` ("fix: handle unknown message types gracefully
  instead of crashing (#598)"). The fork's message parser was fatal-on-unknown-type,
  i.e. it precedes `146e3d6` exactly, which landed 2026-02-20, one day before the
  fork's last commit date (2026-02-21) — the sharpest dividing line available.
- Tip: `6c754df1c44c6429df4d5277337e94af4ddf0499` (v0.2.132, 2026-08-07)
- Range: `d36db81..6c754df`, 332 commits touching `src/`, +8233/-341 lines across
  22 Python files.

Diff any file with:
```
cd /home/sirati/.claude/jobs/00d0dd10/tmp/py && git diff d36db81 6c754df -- <path>
```

## Nix dev shell

`flake.nix` / `devel.nix` at the fork repo root (mirrors the learngraph repo's flake
pattern: rust-overlay nightly toolchain + cargo-nextest/rust-analyzer, plus
python3.12/pip/virtualenv/maturin — pinned to 3.12 since it was also used for an
out-of-scope PyO3 exploration on a sibling branch; not required for this branch's
build but harmless to keep). Committed on this branch. Use:
```
cd /home/sirati/.claude/jobs/00d0dd10/tmp/fork
nix develop --command bash -c '<cargo command>'
```

## Status: all tiers complete

- [x] **Slice 1: message parser forward-compatibility + new message/content types** — DONE
- [x] **Slice 2: `ClaudeAgentOptions` fields + CLI argv construction** — DONE
- [x] **Slice 3: hooks & permissions** — DONE
- [x] **Slice 4: MCP status/control + control-protocol client methods** — DONE
- [x] **Slice 5: session store / resume / mutation subsystem** — DONE (see below;
      initially deferred, then completed after explicit direction that "documented as
      skipped" is not an acceptable end state for this task)
- [x] **Residual gaps found on re-enumeration** — DONE (see below)

Detailed per-slice notes:

### Slice 1 — message parser + types
`Message` gained a `#[serde(other)]` catch-all `Unknown` variant instead of failing
to deserialize on an unrecognized `type` (the confirmed `rate_limit_event` crash);
callers filter it out. Added `RateLimitEvent`/`RateLimitInfo`,
`ServerToolUseBlock`/`ServerToolResultBlock`, `TaskStartedMessage`/`TaskProgressMessage`/
`TaskNotificationMessage`/`TaskUpdatedMessage`, `MirrorErrorMessage`, `HookEventMessage`,
`DeferredToolUse`, `ModelUsage`, `TaskUsage`. Verified live against the real `claude`
CLI (`examples/99_smoke_rate_limit_event.rs`) — a real `rate_limit_event` line no
longer crashes `query()`. Known, precisely-named follow-up: `types/messages.rs` grew
to ~1495 lines, over this project's file-size guidance — splitting it out was judged
unsafe to do mid-parallel-port and is left as a follow-up refactor, not a missing
behavior. `Message` also carries a non-blocking clippy `large size difference between
variants` warning from `ResultMessage`'s growth (boxing it would ripple through many
match sites crate-wide; deferred, not a functional issue).

### Slice 2 — options + CLI argv
`types/config.rs` split into `types/config/`. Added `tools`, `SystemPromptFile`,
`strict_mcp_config`, `session_id`, `fallback_model`, add-dirs, `plugins`, sandbox
settings, `thinking` union type, `effort: "xhigh"`, `output_format`, `task_budget`,
`PermissionMode::DontAsk`/`Auto`, `skills` (`SkillsSelector::List`/`All`), and the
corresponding CLI flags.

### Slice 3 — hooks + permissions
Added `HookEvent::PostToolUseFailure`/`Notification`/`SubagentStart`/
`PermissionRequest` (fork only had 6 of 10 events), their hook-input/output structs,
`_SubagentContextMixin` fields (`agent_id`, `agent_type`), `ToolPermissionContext` new
fields, and `can_use_tool_shadowed_warning()`, wired into `ClaudeClient::connect()`
including the `skills == "all"` implicit-`"Skill"`-entry special case (matches
upstream's `_warn_if_can_use_tool_shadowed` exactly — fixed after initially being
ported as a partial match).

### Slice 4 — MCP status + control protocol
Added `reconnect_mcp_server`, `toggle_mcp_server`, `stop_task`, `get_mcp_status`
(typed), `get_context_usage` to `ClaudeClient` (none of these existed before, not even
untyped). Added `McpToolAnnotations`, `McpServerInfo`, `McpServerStatus`,
`McpStatusResponse`, `McpToolInfo`, `McpServerConnectionStatus`,
`McpServerStatusConfig`, `McpSdkServerConfigStatus`, `McpClaudeAIProxyServerConfig`,
`ContextUsageCategory`, `ContextUsageResponse`.

### Slice 5 — session-store subsystem (fully ported)
New `crates/claude-agent-sdk/src/session/` module (~30 files, single-concern each),
porting all ~4000 lines of upstream's new session-persistence layer:
- `store.rs`/`types.rs`/`info.rs`: the `SessionStore` trait (`append`/`load` required,
  5 optional methods via `supports_*()` capability probes replacing Python's duck-typed
  `_store_implements`), `SessionKey`, `SessionStoreEntry` (opaque passthrough via
  `#[serde(flatten)]`), `SessionStoreListEntry`, `SessionSummaryEntry`, `SDKSessionInfo`,
  `SessionMessage`.
- `in_memory.rs`: `InMemorySessionStore` reference implementation.
- `validation.rs`: `validate_session_store_options`.
- `conformance/`: reusable behavioral-contract test suite any `SessionStore` impl can
  run against (test-only), run against `InMemorySessionStore`.
- `summary.rs`: `fold_session_summary`, `summary_entry_to_sdk_info`.
- `transcript_mirror.rs`: `TranscriptMirrorBatcher` as a single-actor task (channel,
  not a mutex) with the same 500-entry/1MiB/eager-flush thresholds and 3-attempt
  retry-with-backoff (no retry on timeout) as upstream.
- `local.rs`, `project_dir.rs`, `json_extract.rs`, `lite_info.rs`, `iso_time.rs`,
  `unicode_sanitize.rs`, `local_session_file.rs`, `transcript.rs`: filesystem-layout
  primitives (project-key hashing, path sanitization/canonicalization, lightweight
  JSON field extraction, transcript entry parsing/conversation-chain building).
  `project_key_for_directory`'s hash algorithm verified byte-for-byte identical to
  Python's (cross-checked against a live Python run, not just read-through).
- `mutations.rs`/`mutations_store.rs`/`fork.rs`/`fork_transform.rs`/
  `import_to_store.rs`: rename/tag/delete/fork (local + store-backed),
  import-to-store.
- `resume.rs`/`resume_credentials.rs`/`resume_io.rs`/`resume_subkeys.rs`:
  resume-from-store materialization, including the macOS-Keychain credential-copying
  step (`security find-generic-password`, shelled out via `tokio::process::Command`,
  5s timeout, best-effort — this was confirmed fully portable, not a genuine
  Rust-inexpressible case, since it's just a subprocess call) and the
  path-traversal guard (`is_safe_subpath`).
- `listing.rs`, `listing_worktrees.rs`, `session_info.rs`, `messages.rs`,
  `subagents.rs`, `message_paging.rs`, `listing_store.rs`,
  `listing_store_fast_path.rs`, `subagents_store.rs`: the local + store-backed
  session-listing/query API (`list_sessions`, `get_session_info`,
  `get_session_messages`, `list_subagents`, `get_subagent_messages`, and their
  `*_from_store` variants), including the store-backed fast-path staleness check
  (compare a cached summary's `mtime` against `list_sessions`' reported `mtime`).
- `ClaudeAgentOptions.session_store`/`session_store_flush`/`load_timeout_ms` wired,
  plus the `--session-mirror` CLI flag.
- All public functions/types re-exported at the crate root (`lib.rs`), matching
  upstream's public `__init__.py` export list, including `TERMINAL_TASK_STATUSES`
  (promoted from `pub(crate)` to `pub` and re-exported for parity) and
  `fold_session_summary`.

**Bugs found and fixed during mandatory skeptical re-verification** (each slice was
independently re-checked by a fresh agent against the Python source, not just
trusted):
- `resume_subkeys.rs`'s drive-letter detection in the path-traversal guard required
  an ASCII-alphabetic first character; Python's `ntpath.splitdrive` only checks the
  second character is `:` (so `"1:foo"`/`"@:foo"` were wrongly accepted). Fixed —
  not independently exploitable given the resolve-and-prefix backstop, but a real
  spec deviation in security-sensitive code.
- `transcript_mirror.rs`'s single-actor loop awaited its error-reporting callback
  inline, reintroducing head-of-line blocking that upstream's Python explicitly
  avoids (`_drain()` calls `on_error` after releasing its lock) — a slow
  `on_error` callback could delay unrelated concurrent flushes. Fixed by dispatching
  `on_error` via a detached task; added a regression test that empirically confirms
  the fix.
- `listing.rs`'s `deduplicate_by_session_id` used `std::collections::HashMap`, whose
  iteration order is unspecified/randomized per process. Python's dict-based dedup
  preserves first-insertion order for tied entries, and that order feeds a stable
  sort — so ties could land on different pagination pages across runs in Rust.
  Fixed by switching to `indexmap::IndexMap` (already a workspace dependency), which
  preserves insertion order on overwrite exactly like Python's `dict`.
- Two narrow, deliberately-left-as-documented gaps: `_canonicalize_path`'s behavior
  differs from Python's `os.path.realpath` for a nonexistent path whose existing
  prefix contains a symlink (Rust's `canonicalize` requires full path existence);
  `iso_now()` always emits millisecond precision vs. Python's variable precision
  (omits fractional seconds when zero) — cosmetic, informational field only.

### Residual gaps found on re-enumeration (outside the session cluster)
A second, more careful enumeration pass (prompted by explicit direction not to
round up to "done") found four real gaps unrelated to the session-store work:
1. **`--mcp-config` was never sent to the CLI at all** — external (stdio/sse/http)
   MCP servers configured via `ClaudeAgentOptions.mcp_servers` were silently dropped;
   only in-process SDK servers worked. Fixed.
2. **`extra_args` argv-injection vulnerability**: a dash-leading value passed as a
   separate argv token is not bound to its flag by the CLI's parser and gets parsed
   as an independent flag instead — the exact vulnerability class upstream's
   `--resume`/`--session-id` equals-form fix addresses, but the fork's generic
   `extra_args` path still used the vulnerable two-token form. Fixed (equals-form
   when the value starts with `-`, matching upstream exactly).
3. **No Windows BatBadBut (CVE-2024-27980-class) protection**: upstream refuses to
   execute a `.bat`/`.cmd` CLI shim and rejects cmd.exe metacharacters in
   `resume`/`session_id` values; the fork had neither. Fixed (platform-independent
   detection logic, `cfg!(windows)`-gated enforcement).
4. **`SystemPromptPreset.exclude_dynamic_sections`** missing entirely. Fixed, and
   wired through to where it actually needs to reach the CLI: the control-protocol
   `initialize` request (`excludeDynamicSections`), not CLI argv.
5. **Found while wiring #4**: `agents` were being sent via a stale `--agents` CLI
   flag — this was upstream's *pre-fix* behavior (their commit `8a7c0a7`, "send agent
   definitions via initialize request matching TypeScript SDK", predates this fork's
   own baseline, meaning this bug predates the 6-month drift window entirely and was
   never correctly ported in the first place). Also found `skills` had no
   `ClaudeAgentOptions` field at all. Fixed: `agents`/`skills`/
   `exclude_dynamic_sections` are now all sent via `QueryFull::initialize()`'s
   control-protocol request, matching current upstream/TypeScript-SDK behavior, and
   the incorrect `--agents` CLI flag was removed.

## Deliberately skipped (not applicable to Rust, or genuinely inexpressible)

- Packaging/version bump commits (`chore: release vX.Y.Z`, `chore: bump bundled CLI
  version`) — no Rust equivalent artifact.
- Python typing-only changes, `.github` workflow changes, docs-only commits,
  Python-specific async/`_task_compat.py` shims (anyio TaskGroup polyfills) — Rust's
  async model (tokio) doesn't need them; there is no Rust equivalent of "polyfill
  structured concurrency across asyncio/trio" because Rust only has one async runtime
  in play here.
- `RateLimitInfo.status`/`rate_limit_type` and similar CLI-sourced string fields
  (task statuses, server-tool names) are kept as plain `String` rather than closed
  Rust enums — a deliberate choice made in Slice 1 specifically to preserve the
  forward-compatibility this whole port is about (a closed enum would reintroduce
  the same "unknown variant is fatal" failure mode for a new CLI string value).
  Python's corresponding `Literal[...]` type aliases (`ServerToolName`,
  `RateLimitStatus`, `RateLimitType`, `TaskNotificationStatus`, `TaskUpdatedStatus`)
  are compile-time-only Python type hints with no runtime behavior of their own, so
  they have no Rust runtime counterpart to port — the *data* they type is fully
  ported (as `String`), just not re-closed into an enum.
- `testing/session_store_conformance.py` ported as Rust `#[cfg(test)]` conformance
  tests (`session/conformance/`) rather than a 1:1 file, since Rust has no equivalent
  of shipping test helpers as part of the public non-test API surface.

## Build/test/clippy status

`cargo build --workspace`: clean. `cargo test --workspace`: 668 lib + 15 integration +
143 doctests, all passing (repeated multiple times, including under
`--test-threads=16`, to rule out flakiness — none observed). `cargo clippy
--workspace --all-targets`: 0 errors; only pre-existing-style warnings in example
files plus the one documented `large_enum_variant`-style warning on `Message` (Slice
1, non-blocking). Live smoke test (`examples/99_smoke_rate_limit_event.rs`) re-run
against the real `claude` CLI after all session-cluster and residual-gap work landed:
8 messages received including a real `RateLimitEvent`, `query()` completes
successfully.
