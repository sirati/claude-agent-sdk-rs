//! Locating and appending to a session's on-disk JSONL file.
//!
//! Ported from `_find_session_file` / `_find_session_file_with_dir` /
//! `_append_to_session` / `_try_append` in upstream
//! `_internal/session_mutations.py`, plus `_resolve_session_file_path` from
//! `_internal/sessions.py` (the two upstream helpers are near-duplicates of
//! each other; this file merges them into one implementation used by both
//! [`super::mutations`]/[`super::fork`] and [`super::import_to_store`]).
//!
//! Path-resolution primitives (`canonicalize_path`, `find_project_dir`,
//! `get_projects_dir`, `get_worktree_paths`) are shared with the
//! session-listing slice via [`super::project_dir`] / [`super::local`].

use std::path::{Path, PathBuf};

use crate::errors::{ClaudeError, Result};

use super::local::get_worktree_paths;
use super::project_dir::{canonicalize_path, find_project_dir, get_projects_dir};

/// Finds a session file and its containing project directory.
///
/// Directory resolution: when `directory` is given, looks in that project's
/// directory and its git worktrees; otherwise searches every project
/// directory. Returns `(file_path, project_dir)` for the first non-empty
/// match.
pub(super) async fn find_session_file_with_dir(
    session_id: &str,
    directory: Option<&str>,
) -> Option<(PathBuf, PathBuf)> {
    let file_name = format!("{session_id}.jsonl");

    async fn try_dir(project_dir: &Path, file_name: &str) -> Option<(PathBuf, PathBuf)> {
        let path = project_dir.join(file_name);
        let meta = tokio::fs::metadata(&path).await.ok()?;
        if meta.len() > 0 {
            Some((path, project_dir.to_path_buf()))
        } else {
            None
        }
    }

    if let Some(directory) = directory {
        let canonical = canonicalize_path(directory);
        if let Some(project_dir) = find_project_dir(&canonical).await {
            if let Some(found) = try_dir(&project_dir, &file_name).await {
                return Some(found);
            }
        }

        for wt in get_worktree_paths(&canonical).await {
            if wt == canonical {
                continue;
            }
            if let Some(wt_project_dir) = find_project_dir(&wt).await {
                if let Some(found) = try_dir(&wt_project_dir, &file_name).await {
                    return Some(found);
                }
            }
        }
        return None;
    }

    let projects_dir = get_projects_dir();
    let mut entries = tokio::fs::read_dir(&projects_dir).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Some(found) = try_dir(&entry.path(), &file_name).await {
            return Some(found);
        }
    }
    None
}

/// Finds the path to a session's JSONL file (drops the project dir).
pub(super) async fn find_session_file(session_id: &str, directory: Option<&str>) -> Option<PathBuf> {
    find_session_file_with_dir(session_id, directory).await.map(|(path, _)| path)
}

/// Try appending `data` to `path`.
///
/// Opens with write+append, no create, so the open fails with "not found"
/// if the file does not exist — no separate existence check (avoids
/// TOCTOU). Returns `Ok(true)` on successful write, `Ok(false)` if the file
/// does not exist or is 0 bytes (0-byte `.jsonl` means "session not here,
/// keep searching" — matches upstream's guard). Other I/O errors propagate.
async fn try_append(path: &Path, data: &str) -> Result<bool> {
    use tokio::io::AsyncWriteExt;

    let file = match tokio::fs::OpenOptions::new().write(true).append(true).open(path).await {
        Ok(f) => f,
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                || e.kind() == std::io::ErrorKind::NotADirectory =>
        {
            return Ok(false);
        }
        Err(e) => return Err(e.into()),
    };

    let size = file.metadata().await?.len();
    if size == 0 {
        return Ok(false);
    }
    let mut file = file;
    file.write_all(data.as_bytes()).await?;
    Ok(true)
}

/// Appends `data` to an existing session file, searching candidate project
/// directories (and their git worktrees) the same way [`find_session_file_with_dir`]
/// does.
pub(super) async fn append_to_session(session_id: &str, data: &str, directory: Option<&str>) -> Result<()> {
    let file_name = format!("{session_id}.jsonl");

    if let Some(directory) = directory {
        let canonical = canonicalize_path(directory);

        if let Some(project_dir) = find_project_dir(&canonical).await {
            if try_append(&project_dir.join(&file_name), data).await? {
                return Ok(());
            }
        }

        for wt in get_worktree_paths(&canonical).await {
            if wt == canonical {
                continue;
            }
            if let Some(wt_project_dir) = find_project_dir(&wt).await {
                if try_append(&wt_project_dir.join(&file_name), data).await? {
                    return Ok(());
                }
            }
        }

        return Err(ClaudeError::NotFound(format!(
            "Session {session_id} not found in project directory for {directory}"
        )));
    }

    let projects_dir = get_projects_dir();
    let mut entries = tokio::fs::read_dir(&projects_dir)
        .await
        .map_err(|_| ClaudeError::NotFound(format!("Session {session_id} not found (no projects directory)")))?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if try_append(&entry.path().join(&file_name), data).await? {
            return Ok(());
        }
    }
    Err(ClaudeError::NotFound(format!(
        "Session {session_id} not found in any project directory"
    )))
}

/// Shared lock for tests (in this file, [`super::fork`], [`super::mutations`],
/// [`super::import_to_store`]) that point `CLAUDE_CONFIG_DIR` at a temp dir.
/// The env var is process-global, so every test that mutates it must
/// serialize through this *one* lock — per-file locks don't prevent two
/// different files' tests from racing on the same variable.
///
/// `tokio::sync::Mutex` rather than `std::sync::Mutex`: the guard is held
/// across `.await` points (every filesystem/mutation call in the guarded
/// section), and a std mutex held across an await point can stall the
/// async runtime — this crate's convention is to reach for an async-aware
/// primitive instead. It also isn't poisoned by a panicking test, so one
/// failing test can't wedge every other test that shares this lock.
#[cfg(test)]
pub(super) fn env_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}
