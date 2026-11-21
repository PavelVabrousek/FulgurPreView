//! Background folder watcher: notices frames added, removed or rewritten while the viewer is
//! open (for example new subs arriving from ASIAIR, NINA or a Seestar during a session).
//!
//! The watcher rescans the folder on its own thread at a fixed interval, so the UI never
//! blocks on disk or network I/O. Polling is used on purpose: it works the same on local
//! disks, USB drives and network shares, where change notifications are unreliable.
//!
//! A file whose size or modification time changed since the previous scan is reported as
//! not yet stable: a camera may still be writing it. The viewer only adds a new file to the
//! playlist once it is stable, so it never tries to decode a half-written frame.

use crate::directory::{FileSig, scan_directory};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Default interval between two folder scans.
pub const WATCH_INTERVAL: Duration = Duration::from_millis(1500);

/// One file seen by the watcher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchedFile {
    pub sig: FileSig,
    /// `true` once size and modification time were unchanged across two scans.
    pub stable: bool,
}

/// Handle to a running watcher thread. Dropping it stops the thread.
pub struct FolderWatcher {
    pub directory: PathBuf,
    rx: mpsc::Receiver<Vec<WatchedFile>>,
    stop: Arc<AtomicBool>,
}

impl FolderWatcher {
    pub fn start(directory: PathBuf, interval: Duration) -> Self {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let dir = directory.clone();
        std::thread::Builder::new()
            .name("fulgur-folder-watch".to_owned())
            .spawn(move || watch_loop(&dir, interval, &worker_stop, &tx))
            .expect("spawning the folder watcher thread");
        Self {
            directory,
            rx,
            stop,
        }
    }

    /// Latest listing produced since the previous call, if the folder changed.
    pub fn try_latest(&self) -> Option<Vec<WatchedFile>> {
        let mut latest = None;
        while let Ok(listing) = self.rx.try_recv() {
            latest = Some(listing);
        }
        latest
    }
}

impl Drop for FolderWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn watch_loop(
    dir: &std::path::Path,
    interval: Duration,
    stop: &AtomicBool,
    tx: &mpsc::Sender<Vec<WatchedFile>>,
) {
    let mut previous: Option<HashMap<PathBuf, FileSig>> = None;
    let mut last_sent: Option<Vec<WatchedFile>> = None;
    while !stop.load(Ordering::Relaxed) {
        let scan = scan_directory(dir);
        let listing = classify(&scan, previous.as_ref());
        if last_sent.as_ref() != Some(&listing) {
            if tx.send(listing.clone()).is_err() {
                return;
            }
            last_sent = Some(listing);
        }
        previous = Some(scan.into_iter().map(|s| (s.path.clone(), s)).collect());

        let deadline = Instant::now() + interval;
        while Instant::now() < deadline {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Mark each scanned file stable when it looked identical in the previous scan. On the first
/// scan every file is treated as stable (it was already there when the folder was opened).
pub fn classify(
    scan: &[FileSig],
    previous: Option<&HashMap<PathBuf, FileSig>>,
) -> Vec<WatchedFile> {
    scan.iter()
        .map(|sig| WatchedFile {
            stable: previous.is_none_or(|prev| prev.get(&sig.path) == Some(sig)),
            sig: sig.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(name: &str, len: u64) -> FileSig {
        FileSig {
            path: PathBuf::from(name),
            len,
            modified: None,
        }
    }

    #[test]
    fn classify_marks_growing_and_new_files_unstable() {
        let first = vec![sig("a.fit", 10)];
        let all_stable = classify(&first, None);
        assert!(all_stable.iter().all(|f| f.stable));

        let prev: HashMap<PathBuf, FileSig> =
            first.iter().map(|s| (s.path.clone(), s.clone())).collect();
        let second = vec![sig("a.fit", 10), sig("b.fit", 5)];
        let out = classify(&second, Some(&prev));
        assert!(out[0].stable, "unchanged file is stable");
        assert!(!out[1].stable, "new file waits one more scan");

        let prev: HashMap<PathBuf, FileSig> =
            second.iter().map(|s| (s.path.clone(), s.clone())).collect();
        let third = vec![sig("a.fit", 10), sig("b.fit", 9)];
        let out = classify(&third, Some(&prev));
        assert!(!out[1].stable, "still growing");
    }

    #[test]
    fn watcher_reports_new_file_once_written() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Light_001.fits"), b"x").unwrap();
        let watcher = FolderWatcher::start(dir.path().to_path_buf(), Duration::from_millis(60));
        let wait_for = |pred: &dyn Fn(&[WatchedFile]) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Some(listing) = watcher.try_latest()
                    && pred(&listing)
                {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        };
        assert!(wait_for(&|l| l.len() == 1 && l[0].stable));
        std::fs::write(dir.path().join("Light_002.fits"), b"yy").unwrap();
        assert!(wait_for(&|l| l.len() == 2 && l.iter().all(|f| f.stable)));
    }
}
