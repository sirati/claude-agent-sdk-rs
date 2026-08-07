//! Copies auth material into a materialized `CLAUDE_CONFIG_DIR` so the
//! resumed subprocess can authenticate.
//!
//! Security-sensitive: ported from upstream `_internal/session_resume.py`'s
//! `_copy_auth_files`, `_write_redacted_credentials`, `_copy_if_present`,
//! and `_read_keychain_credentials`. Read the docstrings on
//! [`write_redacted_credentials`] and [`read_keychain_credentials`] before
//! changing either — the refresh-token redaction exists specifically to
//! avoid stranding the parent's stored credentials, and the Keychain read
//! must stay best-effort / non-macOS-safe.
//!
//! Simplification vs. upstream: `.claude.json` / `.credentials.json` reads
//! and copies are fully best-effort here (any I/O error, not just "file
//! missing", is swallowed). Upstream only suppresses `FileNotFoundError`
//! and lets other errors (e.g. permission denied) propagate and abort
//! materialization. This file-copy step is a convenience (missing auth
//! files just mean the resumed subprocess falls back to API-key auth or
//! fails its own auth check later) rather than a correctness requirement,
//! so failing open here was judged a reasonable simplification — flag for
//! review if strict parity turns out to matter.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use super::resume_io::write_secure_file;

/// Default macOS Keychain service name for OAuth credentials when
/// `CLAUDE_CONFIG_DIR` is unset.
const KEYCHAIN_SERVICE_NAME: &str = "Claude Code-credentials";
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Copy `.credentials.json` (`claudeAiOauth.refreshToken` redacted) and
/// `.claude.json` into `tmp_base`.
///
/// Source resolution mirrors the CLI:
/// - `.credentials.json` lives under the config dir (default `~/.claude/`).
/// - `.claude.json` lives at `$CLAUDE_CONFIG_DIR/.claude.json` when set,
///   else `~/.claude.json` (NOT `~/.claude/.claude.json`).
pub(super) async fn copy_auth_files(tmp_base: &Path, opt_env: &HashMap<String, String>) {
    let opt_env = opt_env.clone();
    let env_lookup = move |k: &str| opt_env.get(k).cloned().or_else(|| std::env::var(k).ok());
    copy_auth_files_with_deps(tmp_base, &home_dir(), env_lookup, read_keychain_credentials).await;
}

/// Same as [`copy_auth_files`] but with the home directory, env lookup, and
/// Keychain reader all injected — lets tests exercise every branch (custom
/// `CLAUDE_CONFIG_DIR`, API-key/OAuth-token short-circuit, Keychain
/// fallback) without touching real process env or `$HOME`.
async fn copy_auth_files_with_deps<L, F, Fut>(
    tmp_base: &Path,
    home: &Path,
    env_lookup: L,
    read_keychain: F,
) where
    L: Fn(&str) -> Option<String>,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Option<String>>,
{
    let caller_config_dir = env_lookup("CLAUDE_CONFIG_DIR");
    let source_config_dir = match &caller_config_dir {
        Some(d) => PathBuf::from(d),
        None => home.join(".claude"),
    };

    let mut creds_json = tokio::fs::read_to_string(source_config_dir.join(".credentials.json"))
        .await
        .ok();

    // macOS default setup keeps OAuth tokens in the Keychain, not a file.
    // Redirecting CLAUDE_CONFIG_DIR changes the Keychain service-name
    // suffix, so the subprocess's lookup misses and falls back to
    // plainTextStorage at ${tmp_base}/.credentials.json. Populate that file
    // from the parent's Keychain so the resumed subprocess can auth.
    // Skipped when env-based auth or a custom config dir is already in play.
    let has_api_key = env_lookup("ANTHROPIC_API_KEY").is_some();
    let has_oauth_token = env_lookup("CLAUDE_CODE_OAUTH_TOKEN").is_some();

    if caller_config_dir.is_none()
        && !has_api_key
        && !has_oauth_token
        && let Some(keychain) = read_keychain().await
    {
        creds_json = Some(keychain);
    }

    write_redacted_credentials(creds_json.as_deref(), &tmp_base.join(".credentials.json")).await;

    let claude_json_src = match &caller_config_dir {
        Some(d) => PathBuf::from(d).join(".claude.json"),
        None => home.join(".claude.json"),
    };
    copy_if_present(&claude_json_src, &tmp_base.join(".claude.json")).await;
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// Write `creds_json` to `dst` with `claudeAiOauth.refreshToken` removed.
///
/// The resumed subprocess runs under a redirected `CLAUDE_CONFIG_DIR`; if
/// it refreshes, the single-use refresh token would be consumed
/// server-side and the new tokens written to a location the parent never
/// reads back — leaving the parent's stored creds revoked. With no
/// `refreshToken`, the subprocess's refresh check short-circuits. No-op
/// when `creds_json` is `None`.
async fn write_redacted_credentials(creds_json: Option<&str>, dst: &Path) {
    let Some(raw) = creds_json else { return };
    let out = redact_refresh_token(raw);
    let _ = write_secure_file(dst, out.as_bytes()).await;
}

/// Remove `claudeAiOauth.refreshToken` from `raw` if both parse and the key
/// are present; on any parse failure, or when there was nothing to redact,
/// return `raw` byte-for-byte unchanged (an unparseable file is written
/// through as-is — the subprocess will fail to parse it too — and a file
/// that never had a refresh token isn't rewritten just to prove a point).
fn redact_refresh_token(raw: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return raw.to_string();
    };
    let removed = value
        .get_mut("claudeAiOauth")
        .and_then(|v| v.as_object_mut())
        .map(|oauth| oauth.remove("refreshToken").is_some())
        .unwrap_or(false);
    if removed {
        serde_json::to_string(&value).unwrap_or_else(|_| raw.to_string())
    } else {
        raw.to_string()
    }
}

/// Copy `src` to `dst` if it exists; any error (including "not found") is
/// swallowed. See module docs for how this differs from upstream.
async fn copy_if_present(src: &Path, dst: &Path) {
    if let Ok(bytes) = tokio::fs::read(src).await {
        let _ = tokio::fs::write(dst, bytes).await;
    }
}

/// Read OAuth credentials JSON from the macOS Keychain (default service
/// name). Best-effort: returns `None` on any error, timeout (5s), or on
/// non-macOS platforms.
async fn read_keychain_credentials() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "claude-code-user".to_string());
    let mut cmd = Command::new("security");
    cmd.args(["find-generic-password", "-a", &user, "-w", "-s", KEYCHAIN_SERVICE_NAME]);

    let output = match tokio::time::timeout(KEYCHAIN_TIMEOUT, cmd.output()).await {
        Ok(Ok(output)) => output,
        _ => return None,
    };
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn no_keychain() -> Option<String> {
        None
    }

    fn empty_env(_: &str) -> Option<String> {
        None
    }

    #[tokio::test]
    async fn redacts_refresh_token_but_keeps_other_fields() {
        let raw = json!({
            "claudeAiOauth": {"accessToken": "at", "refreshToken": "SECRET"}
        })
        .to_string();
        let out = redact_refresh_token(&raw);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["claudeAiOauth"]["accessToken"], "at");
        assert!(parsed["claudeAiOauth"].get("refreshToken").is_none());
    }

    #[test]
    fn leaves_creds_without_refresh_token_byte_identical() {
        let raw = r#"{"claudeAiOauth":{"accessToken":"at"}}"#;
        assert_eq!(redact_refresh_token(raw), raw);
    }

    #[test]
    fn leaves_unparseable_json_untouched() {
        let raw = "not json";
        assert_eq!(redact_refresh_token(raw), raw);
    }

    #[test]
    fn leaves_non_object_oauth_field_untouched() {
        let raw = r#"{"claudeAiOauth":"weird-string-value"}"#;
        assert_eq!(redact_refresh_token(raw), raw);
    }

    #[tokio::test]
    async fn copy_auth_files_redacts_from_disk_and_copies_claude_json() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".claude");
        tokio::fs::create_dir_all(&config).await.unwrap();
        tokio::fs::write(
            config.join(".credentials.json"),
            json!({"claudeAiOauth": {"accessToken": "at", "refreshToken": "SECRET"}})
                .to_string(),
        )
        .await
        .unwrap();
        tokio::fs::write(home.path().join(".claude.json"), r#"{"theme":"dark"}"#)
            .await
            .unwrap();

        let tmp_base = tempfile::tempdir().unwrap();
        copy_auth_files_with_deps(tmp_base.path(), home.path(), empty_env, no_keychain).await;

        let creds: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(tmp_base.path().join(".credentials.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(creds["claudeAiOauth"]["accessToken"], "at");
        assert!(creds["claudeAiOauth"].get("refreshToken").is_none());

        let claude_json = tokio::fs::read_to_string(tmp_base.path().join(".claude.json"))
            .await
            .unwrap();
        assert_eq!(claude_json, r#"{"theme":"dark"}"#);
    }

    #[tokio::test]
    async fn caller_config_dir_env_takes_precedence_over_home() {
        let custom = tempfile::tempdir().unwrap();
        tokio::fs::write(
            custom.path().join(".credentials.json"),
            json!({"claudeAiOauth": {"accessToken": "fromenv"}}).to_string(),
        )
        .await
        .unwrap();
        let unused_home = tempfile::tempdir().unwrap();

        let custom_path = custom.path().display().to_string();
        let env_lookup = move |k: &str| {
            (k == "CLAUDE_CONFIG_DIR").then(|| custom_path.clone())
        };

        let tmp_base = tempfile::tempdir().unwrap();
        copy_auth_files_with_deps(tmp_base.path(), unused_home.path(), env_lookup, no_keychain)
            .await;

        let creds: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(tmp_base.path().join(".credentials.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(creds["claudeAiOauth"]["accessToken"], "fromenv");
    }

    #[tokio::test]
    async fn keychain_fallback_supplies_and_redacts_credentials() {
        async fn fake_keychain() -> Option<String> {
            Some(
                json!({"claudeAiOauth": {"accessToken": "kc", "refreshToken": "SECRET"}})
                    .to_string(),
            )
        }

        let home = tempfile::tempdir().unwrap();
        let tmp_base = tempfile::tempdir().unwrap();
        copy_auth_files_with_deps(tmp_base.path(), home.path(), empty_env, fake_keychain).await;

        let creds: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(tmp_base.path().join(".credentials.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(creds["claudeAiOauth"]["accessToken"], "kc");
        assert!(creds["claudeAiOauth"].get("refreshToken").is_none());
    }

    #[tokio::test]
    async fn keychain_skipped_when_api_key_env_present() {
        async fn panicking_keychain() -> Option<String> {
            panic!("keychain must not be consulted when ANTHROPIC_API_KEY is set");
        }
        let env_lookup = |k: &str| (k == "ANTHROPIC_API_KEY").then(|| "sk-test".to_string());
        let home = tempfile::tempdir().unwrap();
        let tmp_base = tempfile::tempdir().unwrap();
        copy_auth_files_with_deps(tmp_base.path(), home.path(), env_lookup, panicking_keychain)
            .await;
        // No credentials source at all -> no file written.
        assert!(!tmp_base.path().join(".credentials.json").exists());
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn read_keychain_credentials_is_none_off_macos() {
        assert_eq!(read_keychain_credentials().await, None);
    }
}
