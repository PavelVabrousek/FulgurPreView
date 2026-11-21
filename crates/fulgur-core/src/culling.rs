//! Subframe culling into `<current_dir>/_Rejected/` with non-clobbering filenames and `Ctrl+Z` Undo.
//!
//! Every move is "no replace": it fails instead of overwriting an existing file. A plain
//! `std::fs::rename` silently replaces the target on Windows, so a race (or a name collision)
//! could have destroyed a previously rejected frame.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const REJECTED_DIR_NAME: &str = "_Rejected";

/// Maximum numbered variants tried for a colliding file name in `_Rejected`.
const MAX_NAME_ATTEMPTS: usize = 10_000;

/// Maximum number of undo steps kept in the on-disk journal.
const MAX_JOURNAL_ENTRIES: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CullingAction {
    pub original_path: PathBuf,
    pub rejected_path: PathBuf,
}

/// Undo history of culling moves. With a journal file, the history survives restarts of the
/// application (the journal lives in the app data folder, never in the image folders).
#[derive(Default, Debug)]
pub struct CullingHistory {
    stack: Vec<CullingAction>,
    journal: Option<PathBuf>,
}

impl CullingHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// History persisted to `journal`. Entries whose rejected file no longer exists (moved or
    /// deleted outside the app) are dropped on load. A missing or unreadable journal starts
    /// an empty history.
    pub fn with_journal(journal: PathBuf) -> Self {
        let stack = fs::read_to_string(&journal)
            .ok()
            .and_then(|text| serde_json::from_str::<Vec<CullingAction>>(&text).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|a| a.rejected_path.is_file())
            .collect();
        Self {
            stack,
            journal: Some(journal),
        }
    }

    /// Write the journal (best effort: a failed write never blocks culling itself).
    fn persist(&self) {
        let Some(path) = &self.journal else {
            return;
        };
        let start = self.stack.len().saturating_sub(MAX_JOURNAL_ENTRIES);
        let Ok(json) = serde_json::to_string_pretty(&self.stack[start..]) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, path);
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.stack.is_empty()
    }

    pub fn last_rejected_name(&self) -> Option<String> {
        self.stack
            .last()
            .and_then(|a| a.original_path.file_name())
            .map(|n| n.to_string_lossy().into_owned())
    }

    /// Move `path` into `<parent>/_Rejected/<filename>`, choosing an unused suffix if a file
    /// with the same name already exists in `_Rejected`. Never overwrites anything.
    pub fn reject_file(&mut self, path: &Path) -> Result<CullingAction> {
        if !path.is_file() {
            return Err(Error::Invalid("Cannot reject a file that does not exist."));
        }
        let parent = path
            .parent()
            .ok_or(Error::Invalid("File has no parent directory."))?;
        let file_name = path
            .file_name()
            .ok_or(Error::Invalid("File has no valid name."))?;
        let rejected_dir = parent.join(REJECTED_DIR_NAME);
        fs::create_dir_all(&rejected_dir)?;

        for attempt in 0..MAX_NAME_ATTEMPTS {
            let target = candidate_name(&rejected_dir, Path::new(file_name), attempt);
            match move_no_replace(path, &target) {
                Ok(()) => {
                    let action = CullingAction {
                        original_path: path.to_path_buf(),
                        rejected_path: target,
                    };
                    self.stack.push(action.clone());
                    self.persist();
                    return Ok(action);
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(Error::Io(err)),
            }
        }
        Err(Error::Invalid(
            "Too many files with this name in _Rejected; nothing was moved.",
        ))
    }

    /// Undo the most recent rejection (`Ctrl+Z`), moving the file from `_Rejected` back to its
    /// original path without overwriting any existing file.
    pub fn undo_last(&mut self) -> Result<Option<CullingAction>> {
        let Some(action) = self.stack.pop() else {
            return Ok(None);
        };
        if !action.rejected_path.is_file() {
            self.persist();
            return Err(Error::Invalid(
                "Rejected file is no longer in the _Rejected folder.",
            ));
        }
        match move_no_replace(&action.rejected_path, &action.original_path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                // Put back on stack so history is not lost
                self.stack.push(action);
                return Err(Error::Invalid(
                    "Cannot restore file because a file already exists at the original path.",
                ));
            }
            Err(err) => {
                self.stack.push(action);
                return Err(Error::Io(err));
            }
        }
        // Clean up `_Rejected` directory if it is now empty
        if let Some(rejected_dir) = action.rejected_path.parent()
            && fs::read_dir(rejected_dir)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false)
        {
            let _ = fs::remove_dir(rejected_dir);
        }
        self.persist();
        Ok(Some(action))
    }
}

/// `name.ext`, then `name_1.ext`, `name_2.ext`, ...
fn candidate_name(dir: &Path, file_name: &Path, attempt: usize) -> PathBuf {
    if attempt == 0 {
        return dir.join(file_name);
    }
    let stem = file_name
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "frame".to_owned());
    let ext = file_name
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    dir.join(format!("{stem}_{attempt}{ext}"))
}

/// Move a file within the same volume, failing with `AlreadyExists` instead of replacing
/// an existing target.
#[cfg(windows)]
pub fn move_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain([0]).collect() };
    let from_w = wide(from);
    let to_w = wide(to);
    // SAFETY: both buffers are valid, NUL-terminated UTF-16 strings that outlive the call.
    // Flags = 0: no MOVEFILE_REPLACE_EXISTING, so an existing target makes the call fail.
    let ok = unsafe { MoveFileExW(from_w.as_ptr(), to_w.as_ptr(), 0) };
    if ok != 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(code) if code == ERROR_ALREADY_EXISTS as i32 || code == ERROR_FILE_EXISTS as i32 => {
            Err(io::Error::new(io::ErrorKind::AlreadyExists, err))
        }
        _ => Err(err),
    }
}

/// Move a file within the same volume, failing with `AlreadyExists` instead of replacing
/// an existing target. A hard link is created atomically (fails if the target exists) and
/// the source name is then removed; filesystems without hard links fall back to a checked
/// rename.
#[cfg(not(windows))]
pub fn move_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    match fs::hard_link(from, to) {
        Ok(()) => fs::remove_file(from),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Err(err),
        Err(_) => {
            if to.exists() {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            fs::rename(from, to)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_and_undo_roundtrip_preserves_bytes_and_handles_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("Light_001.fit");
        fs::write(&file_a, b"original_frame_1").unwrap();

        let mut history = CullingHistory::new();
        let action1 = history.reject_file(&file_a).unwrap();
        assert!(!file_a.exists());
        assert!(action1.rejected_path.is_file());
        assert_eq!(
            fs::read(&action1.rejected_path).unwrap(),
            b"original_frame_1"
        );

        // Create a second file at the same path and reject it too (must not overwrite the first!)
        fs::write(&file_a, b"second_frame_1").unwrap();
        let action2 = history.reject_file(&file_a).unwrap();
        assert_ne!(action1.rejected_path, action2.rejected_path);
        assert_eq!(
            fs::read(&action1.rejected_path).unwrap(),
            b"original_frame_1"
        );
        assert_eq!(fs::read(&action2.rejected_path).unwrap(), b"second_frame_1");

        // Undo the second rejection
        let undone2 = history.undo_last().unwrap().unwrap();
        assert_eq!(undone2, action2);
        assert_eq!(fs::read(&file_a).unwrap(), b"second_frame_1");
    }

    #[test]
    fn undo_history_survives_restart_via_journal() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("appdata").join("culling_undo.json");
        let frame = dir.path().join("Light_003.fit");
        fs::write(&frame, b"frame").unwrap();
        {
            let mut history = CullingHistory::with_journal(journal.clone());
            history.reject_file(&frame).unwrap();
        }
        // New session
        let mut history = CullingHistory::with_journal(journal.clone());
        assert!(history.can_undo());
        let restored = history.undo_last().unwrap().unwrap();
        assert_eq!(restored.original_path, frame);
        assert_eq!(fs::read(&frame).unwrap(), b"frame");
        assert!(!CullingHistory::with_journal(journal).can_undo());
    }

    #[test]
    fn move_no_replace_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.fit");
        let b = dir.path().join("b.fit");
        fs::write(&a, b"A").unwrap();
        fs::write(&b, b"B").unwrap();
        let err = move_no_replace(&a, &b).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&a).unwrap(), b"A");
        assert_eq!(fs::read(&b).unwrap(), b"B");
        let c = dir.path().join("c.fit");
        move_no_replace(&a, &c).unwrap();
        assert!(!a.exists());
        assert_eq!(fs::read(&c).unwrap(), b"A");
    }

    #[test]
    fn undo_refuses_to_overwrite_new_file_at_original_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Light_002.fit");
        fs::write(&file, b"rejected").unwrap();
        let mut history = CullingHistory::new();
        let action = history.reject_file(&file).unwrap();
        fs::write(&file, b"new capture").unwrap();
        assert!(history.undo_last().is_err());
        assert_eq!(fs::read(&file).unwrap(), b"new capture");
        assert_eq!(fs::read(&action.rejected_path).unwrap(), b"rejected");
        assert!(
            history.can_undo(),
            "history must be kept after a refused undo"
        );
    }
}
