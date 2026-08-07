//! Windows BatBadBut (CVE-2024-27980-class) protections for the CLI spawn
//! path.
//!
//! Ported from Python's `_is_windows_batch_cli` / `_reject_windows_batch_cli`
//! and `_reject_windows_cmd_metacharacters`. Windows has no shebang
//! mechanism: CreateProcess spawns `.bat`/`.cmd` scripts by silently
//! rewriting into a `cmd.exe /c` invocation, and cmd.exe re-parses the whole
//! command line at execution time. There is no reliable escaping for
//! cmd.exe metacharacters, so the fix is (a) refuse to execute a batch
//! script as the CLI at all, and (b) reject untrusted values that would
//! reach argv on the resume/session-id/extra-arg flags if they contain
//! cmd.exe metacharacters.
//!
//! Detection is pure string/path logic (mirroring Python, which computes it
//! the same way) so it is unit-tested on every platform. Only the `reject_*`
//! wrappers gate actual enforcement on `cfg!(windows)`.

use std::path::Path;

use crate::errors::{ClaudeError, ConnectionError, Result};

/// cmd.exe metacharacters, plus the quote character cmd.exe uses to toggle
/// its quoting state, and "!", which expands like "%" under delayed
/// expansion. Mirrors Python's `_CMD_EXE_METACHARACTERS`.
const CMD_EXE_METACHARACTERS: &str = "&|<>^%!\"";

/// Whether `cli_path` names a `.bat`/`.cmd` batch script anywhere in its
/// components.
///
/// Classifies every path component (not just the final one), and within a
/// component every ':'-separated segment (NTFS stream specs, drive
/// prefixes), stripping trailing dots/spaces the way Windows normalizes
/// paths at resolution time. Refusing whenever any component carries a
/// batch extension closes the whole class of normalization tricks: no
/// legitimate `claude.exe` lives beneath a directory spelled like a batch
/// file.
pub(super) fn is_windows_batch_cli(cli_path: &str) -> bool {
    cli_path
        .replace('\\', "/")
        .split('/')
        .flat_map(|component| component.split(':'))
        .any(|segment| {
            let trimmed = segment.trim_end_matches(['.', ' ']).to_lowercase();
            trimmed.ends_with(".bat") || trimmed.ends_with(".cmd")
        })
}

/// Refuse to execute a `.bat`/`.cmd` script as the CLI on Windows.
///
/// No-op off Windows, and when `cli_path` is not a batch script. Call this
/// before spawning `cli_path` for any purpose (version probe, main spawn).
pub(super) fn reject_windows_batch_cli(cli_path: &Path) -> Result<()> {
    if !cfg!(windows) {
        return Ok(());
    }
    let path_str = cli_path.to_string_lossy();
    if !is_windows_batch_cli(&path_str) {
        return Ok(());
    }
    Err(ClaudeError::Connection(ConnectionError::new(format!(
        "Refusing to execute batch script {path_str:?}: Windows runs .bat/.cmd \
         files via cmd.exe, which can execute commands injected through CLI \
         arguments, and no reliable escaping for cmd.exe exists. Use a native \
         claude executable instead: install Claude Code natively \
         (irm https://claude.ai/install.ps1 | iex), or point cli_path at a \
         claude.exe."
    ))))
}

/// The cmd.exe metacharacters (and bare CR/LF) present in `value`, sorted
/// and deduplicated. Empty when `value` is safe.
pub(super) fn find_cmd_exe_metacharacters(value: &str) -> Vec<char> {
    let mut bad: Vec<char> = value
        .chars()
        .filter(|c| CMD_EXE_METACHARACTERS.contains(*c) || *c == '\r' || *c == '\n')
        .collect();
    bad.sort_unstable();
    bad.dedup();
    bad
}

/// Reject `value` if it contains cmd.exe metacharacters, enforced on
/// Windows only.
///
/// Defense in depth: with batch-script spawning refused, these characters
/// are inert for native executables, but resume/session-id/extra-arg
/// values commonly come from external input, so they are rejected outright
/// (not silently stripped) in case a cmd.exe hop is ever reintroduced
/// between the SDK and the CLI. No format is imposed beyond this (resume
/// values may be arbitrary session titles, not only UUIDs).
pub(super) fn reject_windows_cmd_metacharacters(option_name: &str, value: &str) -> Result<()> {
    if !cfg!(windows) {
        return Ok(());
    }
    let bad = find_cmd_exe_metacharacters(value);
    if bad.is_empty() {
        return Ok(());
    }
    Err(ClaudeError::InvalidInput(format!(
        "{option_name} value {value:?} contains characters that are unsafe to \
         pass on a Windows command line: {bad:?}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_bat_and_cmd_extensions() {
        assert!(is_windows_batch_cli("claude.cmd"));
        assert!(is_windows_batch_cli("C:/tools/claude.bat"));
        assert!(is_windows_batch_cli(r"C:\tools\claude.CMD"));
        assert!(is_windows_batch_cli("claude.cmd. "));
    }

    #[test]
    fn detects_batch_extension_in_any_component() {
        assert!(is_windows_batch_cli("claude.cmd/sub/claude.exe"));
        assert!(is_windows_batch_cli(r"C:\claude.cmd\..\claude"));
    }

    #[test]
    fn detects_stream_spec_and_drive_prefix() {
        assert!(is_windows_batch_cli("claude:evil.cmd"));
        assert!(is_windows_batch_cli("claude.cmd:stream"));
        assert!(is_windows_batch_cli("C:claude.cmd"));
    }

    #[test]
    fn allows_native_executables_and_posix_paths() {
        assert!(!is_windows_batch_cli("claude.exe"));
        assert!(!is_windows_batch_cli("/usr/local/bin/claude"));
        assert!(!is_windows_batch_cli("claude"));
    }

    #[test]
    fn finds_cmd_exe_metacharacters() {
        for value in [
            "x&calc", "x|whoami", "x<in", "x>out", "x^y", "x%PATH%y", "x!VAR!y", "x\"y", "x\ny",
            "x\ry",
        ] {
            assert!(
                !find_cmd_exe_metacharacters(value).is_empty(),
                "expected a metacharacter to be found in {value:?}"
            );
        }
    }

    #[test]
    fn ordinary_values_have_no_metacharacters() {
        assert!(find_cmd_exe_metacharacters("My project - daily notes (v2) #3").is_empty());
    }
}
