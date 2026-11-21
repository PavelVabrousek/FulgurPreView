//! One cancellable background job: a named worker thread, a result channel and a cancel flag.
//!
//! Every background task of the viewer (overview load, star analysis, plate solve, 1:1 detail
//! crop) uses this single type, so they all follow the same rules:
//! - Dropping the job sets its cancel flag; the worker stops at its next check and the thread
//!   is detached (never joined on the UI thread).
//! - [`Job::poll`] reports exactly one of: still running, a result, or "stopped" when the
//!   worker ended without sending anything (a panic, recorded in `crash.log`). Callers must
//!   record "stopped" as a failure so the job is not restarted in a loop.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

/// State of a job as seen from the UI thread.
pub enum JobPoll<T> {
    /// The worker has not reported yet.
    Pending,
    /// The worker sent a result.
    Done(T),
    /// The worker ended without a result (it panicked).
    Stopped,
}

pub struct Job<K, T> {
    pub key: K,
    rx: mpsc::Receiver<T>,
    cancel: Arc<AtomicBool>,
}

impl<K, T: Send + 'static> Job<K, T> {
    /// Start `work` on a new thread called `name`. `work` receives the cancel flag and a sender
    /// with room for `capacity` unread results (RAW loads send a preview and then a final
    /// frame, so they use 2).
    pub fn spawn(
        key: K,
        name: &str,
        capacity: usize,
        work: impl FnOnce(&AtomicBool, &mpsc::SyncSender<T>) + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel(capacity);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let spawned = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || work(&worker_cancel, &tx));
        // If the OS refuses a new thread, the sender is dropped with the closure and the job
        // reports `Stopped`, which callers record as a failure.
        let _ = spawned;
        Self { key, rx, cancel }
    }

    /// Non-blocking check for the job's next result.
    pub fn poll(&self) -> JobPoll<T> {
        match self.rx.try_recv() {
            Ok(value) => JobPoll::Done(value),
            Err(mpsc::TryRecvError::Empty) => JobPoll::Pending,
            Err(mpsc::TryRecvError::Disconnected) => JobPoll::Stopped,
        }
    }
}

impl<K, T> Drop for Job<K, T> {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait<T: Send + 'static>(job: &Job<u8, T>) -> JobPoll<T> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match job.poll() {
                JobPoll::Pending if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                other => return other,
            }
        }
    }

    #[test]
    fn reports_result_and_stopped_worker() {
        let ok = Job::spawn(1_u8, "test-ok", 1, |_, tx| {
            let _ = tx.send(42_u32);
        });
        assert!(matches!(wait(&ok), JobPoll::Done(42)));

        let silent = Job::<u8, u32>::spawn(2, "test-silent", 1, |_, _| {});
        assert!(matches!(wait(&silent), JobPoll::Stopped));
    }

    #[test]
    fn dropping_the_job_cancels_the_worker() {
        let (seen_tx, seen_rx) = mpsc::channel();
        let job = Job::<u8, ()>::spawn(3, "test-cancel", 1, move |cancel, _| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !cancel.load(Ordering::Relaxed) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            let _ = seen_tx.send(cancel.load(Ordering::Relaxed));
        });
        drop(job);
        assert_eq!(seen_rx.recv_timeout(Duration::from_secs(5)), Ok(true));
    }
}
