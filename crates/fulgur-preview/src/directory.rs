//! Instant directory playlist scanner, natural alphanumeric sorting, and neighbor selection.

use fulgur_core::fits::is_supported_image;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Directory sorting order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortOrder {
    #[default]
    NaturalName,
    ModifiedTime,
}

impl SortOrder {
    pub fn label(self) -> &'static str {
        match self {
            Self::NaturalName => "Name (A-Z)",
            Self::ModifiedTime => "Time (Old-New)",
        }
    }
}

/// Identity of one file on disk: changes when the file is rewritten or still growing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSig {
    pub path: PathBuf,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

/// List the supported image files of `dir` (not recursive; `_Rejected` is a subfolder and so
/// never listed). Uses the directory entry's own type and metadata, which on Windows come
/// from the directory listing itself without an extra file system call per file.
pub fn scan_directory(dir: &Path) -> Vec<FileSig> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read_dir
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .filter(|entry| is_supported_image(&entry.path()))
        .map(|entry| {
            let meta = entry.metadata().ok();
            FileSig {
                path: entry.path(),
                len: meta.as_ref().map_or(0, |m| m.len()),
                modified: meta.and_then(|m| m.modified().ok()),
            }
        })
        .collect()
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Compare two strings using natural alphanumeric ordering (`frame_2.fit` < `frame_10.fit`).
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut ia = a.chars().peekable();
    let mut ib = b.chars().peekable();

    loop {
        match (ia.peek(), ib.peek()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(&ca), Some(&cb)) => {
                if ca.is_ascii_digit() && cb.is_ascii_digit() {
                    let mut na = String::new();
                    while let Some(&c) = ia.peek() {
                        if c.is_ascii_digit() {
                            na.push(c);
                            ia.next();
                        } else {
                            break;
                        }
                    }
                    let mut nb = String::new();
                    while let Some(&c) = ib.peek() {
                        if c.is_ascii_digit() {
                            nb.push(c);
                            ib.next();
                        } else {
                            break;
                        }
                    }
                    let za = na.trim_start_matches('0');
                    let zb = nb.trim_start_matches('0');
                    let ord = za
                        .len()
                        .cmp(&zb.len())
                        .then_with(|| za.cmp(zb))
                        .then_with(|| na.len().cmp(&nb.len()));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                } else {
                    let la = ca.to_ascii_lowercase();
                    let lb = cb.to_ascii_lowercase();
                    if la != lb {
                        return la.cmp(&lb);
                    }
                    ia.next();
                    ib.next();
                }
            }
        }
    }
}

/// Active directory playlist of supported image files.
#[derive(Clone, Debug, Default)]
pub struct DirectoryPlaylist {
    pub directory: Option<PathBuf>,
    pub files: Vec<PathBuf>,
    pub current_index: usize,
    pub sort_order: SortOrder,
}

impl DirectoryPlaylist {
    /// Open either a specific file (scanning its parent directory and selecting it)
    /// or a directory (scanning it and selecting index 0).
    pub fn open(target: &Path, sort_order: SortOrder) -> Self {
        let (dir, selected_file) = if target.is_dir() {
            (target.to_path_buf(), None)
        } else {
            let parent = target
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            (parent, Some(target.to_path_buf()))
        };

        let mut playlist = Self {
            directory: Some(dir),
            files: Vec::new(),
            current_index: 0,
            sort_order,
        };
        playlist.rescan(selected_file.as_deref());
        playlist
    }

    /// Rescan the current directory and try to keep `preferred_file` selected.
    pub fn rescan(&mut self, preferred_file: Option<&Path>) {
        let keep = preferred_file
            .map(Path::to_path_buf)
            .or_else(|| self.current_path().map(Path::to_path_buf));

        let Some(dir) = &self.directory else {
            self.files.clear();
            self.current_index = 0;
            return;
        };
        let listing = scan_directory(dir);
        self.set_listing(listing, keep.as_deref());
    }

    /// Replace the file list with `listing` (sorted by the current order), keeping `keep`
    /// selected when it is still present.
    pub fn set_listing(&mut self, mut listing: Vec<FileSig>, keep: Option<&Path>) {
        match self.sort_order {
            SortOrder::NaturalName => {
                listing.sort_by(|a, b| natural_cmp(&file_name_of(&a.path), &file_name_of(&b.path)));
            }
            SortOrder::ModifiedTime => {
                listing.sort_by(|a, b| {
                    a.modified
                        .cmp(&b.modified)
                        .then_with(|| natural_cmp(&file_name_of(&a.path), &file_name_of(&b.path)))
                });
            }
        }

        self.files = listing.into_iter().map(|e| e.path).collect();
        if let Some(target) = keep {
            if let Some(idx) = self.files.iter().position(|p| p == target) {
                self.current_index = idx;
                return;
            }
            // Fallback: match by file_name
            if let Some(target_name) = target.file_name()
                && let Some(idx) = self
                    .files
                    .iter()
                    .position(|p| p.file_name() == Some(target_name))
            {
                self.current_index = idx;
                return;
            }
        }
        if self.current_index >= self.files.len() {
            self.current_index = self.files.len().saturating_sub(1);
        }
    }

    /// Apply a folder-watcher listing. Stable files are listed; a file that is still being
    /// written is only kept if it is already in the playlist (so it does not flicker away).
    /// Returns the number of files newly added.
    pub fn apply_watch_listing(&mut self, listing: &[crate::watch::WatchedFile]) -> usize {
        let before: std::collections::HashSet<&PathBuf> = self.files.iter().collect();
        let accepted: Vec<FileSig> = listing
            .iter()
            .filter(|f| f.stable || before.contains(&f.sig.path))
            .map(|f| f.sig.clone())
            .collect();
        let added = accepted
            .iter()
            .filter(|f| !before.contains(&f.path))
            .count();
        let keep = self.current_path().map(Path::to_path_buf);
        self.set_listing(accepted, keep.as_deref());
        added
    }

    /// `true` when the last file of the playlist is selected.
    pub fn is_at_last(&self) -> bool {
        !self.files.is_empty() && self.current_index + 1 == self.files.len()
    }

    pub fn set_sort_order(&mut self, order: SortOrder) {
        if self.sort_order != order {
            self.sort_order = order;
            let current = self.current_path().map(Path::to_path_buf);
            self.rescan(current.as_deref());
        }
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn current_path(&self) -> Option<&Path> {
        self.files.get(self.current_index).map(PathBuf::as_path)
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> bool {
        if self.current_index + 1 < self.files.len() {
            self.current_index += 1;
            true
        } else {
            false
        }
    }

    pub fn prev(&mut self) -> bool {
        if self.current_index > 0 {
            self.current_index -= 1;
            true
        } else {
            false
        }
    }

    pub fn first(&mut self) -> bool {
        if !self.files.is_empty() && self.current_index != 0 {
            self.current_index = 0;
            true
        } else {
            false
        }
    }

    pub fn last(&mut self) -> bool {
        if !self.files.is_empty() && self.current_index + 1 < self.files.len() {
            self.current_index = self.files.len() - 1;
            true
        } else {
            false
        }
    }

    /// Remove the currently selected file from the playlist (e.g. after moving to `_Rejected`).
    pub fn remove_current(&mut self) -> Option<PathBuf> {
        if self.files.is_empty() {
            return None;
        }
        let removed = self.files.remove(self.current_index);
        if self.current_index >= self.files.len() && !self.files.is_empty() {
            self.current_index = self.files.len() - 1;
        }
        Some(removed)
    }

    /// Return neighbor paths `[i+1, i-1, i+2, i-2, ...]` up to `radius` for background prefetching.
    pub fn neighbors_for_prefetch(&self, radius: usize) -> Vec<PathBuf> {
        let mut out = Vec::with_capacity(radius * 2);
        if self.files.is_empty() {
            return out;
        }
        for step in 1..=radius {
            if let Some(next_path) = self.files.get(self.current_index + step) {
                out.push(next_path.clone());
            }
            if self.current_index >= step {
                if let Some(prev_path) = self.files.get(self.current_index - step) {
                    out.push(prev_path.clone());
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_listing_adds_only_stable_files_and_keeps_selection() {
        use crate::watch::WatchedFile;
        let f = |name: &str, stable: bool| WatchedFile {
            sig: FileSig {
                path: PathBuf::from(name),
                len: 1,
                modified: None,
            },
            stable,
        };
        let mut playlist = DirectoryPlaylist {
            directory: None,
            files: vec![PathBuf::from("Light_1.fits"), PathBuf::from("Light_2.fits")],
            current_index: 1,
            sort_order: SortOrder::NaturalName,
        };
        let added = playlist.apply_watch_listing(&[
            f("Light_1.fits", true),
            f("Light_2.fits", false),
            f("Light_3.fits", false),
        ]);
        assert_eq!(added, 0, "half-written new frame is not added yet");
        assert_eq!(
            playlist.len(),
            2,
            "known file still being touched stays listed"
        );
        assert_eq!(playlist.current_path(), Some(Path::new("Light_2.fits")));

        let added = playlist.apply_watch_listing(&[
            f("Light_1.fits", true),
            f("Light_2.fits", true),
            f("Light_3.fits", true),
        ]);
        assert_eq!(added, 1);
        assert_eq!(playlist.current_path(), Some(Path::new("Light_2.fits")));
        assert!(!playlist.is_at_last());
    }

    #[test]
    fn test_natural_cmp() {
        let mut names = vec![
            "Light_10.fits",
            "Light_2.fits",
            "Light_1.fits",
            "Light_20.fits",
        ];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            vec![
                "Light_1.fits",
                "Light_2.fits",
                "Light_10.fits",
                "Light_20.fits"
            ]
        );
    }
}
