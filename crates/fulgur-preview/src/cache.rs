//! Bounded LRU frame cache, background neighbor prefetching (`i-2 .. i+2`),
//! asynchronous background star photometry (`StarQualityReport`), and
//! on-demand background astrometric plate solving (`PlateSolveReport`).

use crate::jobs::{Job, JobPoll};
use fulgur_catalog::config::CatalogConfig;
use fulgur_core::RawFrame;
use fulgur_core::fits::read_header_hdu;
use fulgur_core::raw::{RawStage, is_raw_image};
use fulgur_core::{LayoutChoice, Result};
use fulgur_engine::sky::{
    PlateSolveReport, SolveMethod, build_header_wcs_report, load_raw_for_solving, solve_raw_frame,
};
use fulgur_engine::stars::analyze_raw_frame;
use fulgur_engine::stars::{
    RAW_PREVIEW_ONLY_STARS_MESSAGE, StarQualityReport, analyze_frame_stars,
};
use fulgur_engine::stretch::{
    OverviewFrame, load_frame_overview_with_source, load_raw_stage1_overview,
    load_raw_stage2_overview_with_source,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Condvar, Mutex, OnceLock, PoisonError,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

/// Maximum memory budget for cached overview frames (1.2 GiB).
const MAX_CACHE_BYTES: usize = 1_200 * 1024 * 1024;
/// Maximum number of cached overview frames.
const MAX_CACHE_ENTRIES: usize = 24;
/// Memory budget for decoded full-resolution source planes kept for reuse by star analysis
/// and plate solving (768 MiB), so a frame is read and decoded from disk only once.
const MAX_SOURCE_BYTES: usize = 768 * 1024 * 1024;

/// Overview frame plus, when available, the decoded full-resolution single-plane source.
type LoadedFrame = (Arc<OverviewFrame>, Option<Arc<RawFrame>>);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub path: PathBuf,
    pub hdu_index: usize,
    pub layout: LayoutChoice,
}

/// Maximum number of full-frame decodes (overview, star analysis, plate solve) that may
/// hold image buffers at the same time. Further jobs wait without allocating anything.
const MAX_CONCURRENT_DECODES: usize = 3;

/// When the user navigates while the previous frame is still loading (holding an arrow
/// key), a new load waits this long first, so frames skipped over are never read at all.
const NAVIGATION_DEBOUNCE: Duration = Duration::from_millis(60);

/// Poll interval used while waiting for a decode slot or the navigation debounce.
const WAIT_STEP: Duration = Duration::from_millis(10);

static DECODES_RUNNING: Mutex<usize> = Mutex::new(0);
static DECODE_SLOT_FREED: Condvar = Condvar::new();
/// Highest number of decode slots ever held at once (telemetry and tests).
static DECODES_PEAK: AtomicUsize = AtomicUsize::new(0);

/// A held permission to decode one full frame. Released on drop, even on panic.
struct DecodeSlot;

impl DecodeSlot {
    /// Wait for a free slot. Returns `None` if `cancel` is set while waiting, so abandoned
    /// jobs exit before reading the file or allocating any buffer.
    fn acquire(cancel: &AtomicBool) -> Option<Self> {
        let mut running = DECODES_RUNNING
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            if *running < MAX_CONCURRENT_DECODES {
                *running += 1;
                DECODES_PEAK.fetch_max(*running, Ordering::Relaxed);
                return Some(Self);
            }
            running = DECODE_SLOT_FREED
                .wait_timeout(running, WAIT_STEP)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

impl Drop for DecodeSlot {
    fn drop(&mut self) {
        let mut running = DECODES_RUNNING
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *running = running.saturating_sub(1);
        DECODE_SLOT_FREED.notify_all();
    }
}

/// Highest number of concurrent full-frame decodes observed since start.
pub fn peak_concurrent_decodes() -> usize {
    DECODES_PEAK.load(Ordering::Relaxed)
}

/// Sleep for `duration` in small steps. Returns `false` if cancelled meanwhile.
fn wait_unless_cancelled(cancel: &AtomicBool, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(WAIT_STEP);
    }
    !cancel.load(Ordering::Relaxed)
}

/// Dedicated rayon pool for work nobody is waiting on (neighbour prefetch, star analysis,
/// plate solving). The UI thread renders stretches on the global pool; keeping background
/// work off it means a slider drag never queues behind a running star analysis.
fn background_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(1))
            .unwrap_or(1)
            .max(1);
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("fulgur-bg-{i}"))
            .build()
            .expect("building the background thread pool")
    })
}

/// Run `work` on the background pool when `background` is set, otherwise on the global
/// pool (used for the frame the user is waiting for).
fn run_in_pool<R: Send>(background: bool, work: impl FnOnce() -> R + Send) -> R {
    if background {
        background_pool().install(work)
    } else {
        work()
    }
}

/// Message stored when a background worker thread ended without reporting a result.
/// That only happens when it panicked; the crash log has the details.
const WORKER_STOPPED: &str = "Background worker stopped unexpectedly (see crash.log).";

type OverviewJob = Job<CacheKey, Result<LoadedFrame>>;
type StarJob = Job<CacheKey, Result<Arc<StarQualityReport>>>;
type PlateSolveResult = Result<(Arc<PlateSolveReport>, Option<Arc<fulgur_solve::Index>>)>;
type SolveJob = Job<CacheKey, PlateSolveResult>;

/// Load the overview of `key` (RAW: preview first, then linear data).
fn spawn_overview_job(
    key: CacheKey,
    two_stage_raw: bool,
    debounce: bool,
    background: bool,
) -> OverviewJob {
    let worker_key = key.clone();
    // Capacity 2: a RAW load sends the Stage-1 preview and then the Stage-2 frame.
    Job::spawn(key, "fulgur-load", 2, move |cancel, tx| {
        if debounce && !wait_unless_cancelled(cancel, NAVIGATION_DEBOUNCE) {
            return;
        }
        let Some(_slot) = DecodeSlot::acquire(cancel) else {
            return;
        };
        run_in_pool(background, || {
            if two_stage_raw && is_raw_image(&worker_key.path) {
                // Stage 1: Instant embedded preview (< 20 ms)
                if let Ok(stage1) =
                    load_raw_stage1_overview(&worker_key.path, worker_key.layout, cancel)
                    && tx.send(Ok((Arc::new(stage1), None))).is_err()
                {
                    return;
                }
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                // Stage 2: Background linear CFA decode + MTF stretch
                let res = load_raw_stage2_overview_with_source(
                    &worker_key.path,
                    worker_key.layout,
                    cancel,
                )
                .map(|(frame, source)| (Arc::new(frame), source));
                let _ = tx.send(res);
            } else {
                let res = load_frame_overview_with_source(
                    &worker_key.path,
                    worker_key.hdu_index,
                    worker_key.layout,
                    cancel,
                )
                .map(|(frame, source)| (Arc::new(frame), source));
                let _ = tx.send(res);
            }
        });
    })
}

/// Star photometry for `key`, reusing the decoded `source` plane when available.
fn spawn_star_job(key: CacheKey, source: Option<Arc<RawFrame>>) -> StarJob {
    let worker_key = key.clone();
    Job::spawn(key, "fulgur-stars", 1, move |cancel, tx| {
        let Some(_slot) = DecodeSlot::acquire(cancel) else {
            return;
        };
        let res = run_in_pool(true, || match &source {
            // Reuse the plane decoded for the overview: no second read / decode.
            Some(raw) => analyze_raw_frame(raw, cancel),
            None => analyze_frame_stars(
                &worker_key.path,
                worker_key.hdu_index,
                worker_key.layout,
                cancel,
            ),
        })
        .map(Arc::new);
        let _ = tx.send(res);
    })
}

/// Plate solve for `key`, reusing the decoded `source` plane when available.
fn spawn_solve_job(
    key: CacheKey,
    cfg: CatalogConfig,
    cached_index: Option<Arc<fulgur_solve::Index>>,
    source: Option<Arc<RawFrame>>,
) -> SolveJob {
    let worker_key = key.clone();
    Job::spawn(key, "fulgur-solve", 1, move |cancel, tx| {
        let Some(_slot) = DecodeSlot::acquire(cancel) else {
            return;
        };
        let res = run_in_pool(true, || -> PlateSolveResult {
            let raw = match source {
                Some(raw) => raw,
                None => Arc::new(load_raw_for_solving(
                    &worker_key.path,
                    worker_key.hdu_index,
                    worker_key.layout,
                    cancel,
                )?),
            };
            let (report, idx) =
                solve_raw_frame(&worker_key.path, &raw, &cfg, cached_index, cancel)?;
            Ok((Arc::new(report), idx))
        });
        let _ = tx.send(res);
    })
}

/// Outcome of the active overview job, as seen by the cache state machine.
enum ActiveOutcome {
    Loaded(LoadedFrame),
    Failed(String),
}

/// Bounded LRU cache of `OverviewFrame`s with prioritized active load + background neighbor prefetch
/// + asynchronous background star photometry (`StarQualityReport`) + on-demand plate solving (`PlateSolveReport`).
#[derive(Default)]
pub struct FrameCache {
    entries: HashMap<CacheKey, Arc<OverviewFrame>>,
    star_reports: HashMap<CacheKey, Arc<StarQualityReport>>,
    star_failed: HashMap<CacheKey, String>,
    /// RAW frames whose Stage-1 preview is cached but whose Stage-2 decode failed.
    /// Without this record the cache would restart the failed decode on every UI frame.
    stage2_failed: HashMap<CacheKey, String>,
    plate_reports: HashMap<CacheKey, Arc<PlateSolveReport>>,
    plate_errors: HashMap<CacheKey, String>,
    blind_index: Option<Arc<fulgur_solve::Index>>,
    /// Decoded full-resolution source planes (bounded by `MAX_SOURCE_BYTES`, LRU order).
    sources: HashMap<CacheKey, Arc<RawFrame>>,
    source_lru: VecDeque<CacheKey>,
    source_bytes: usize,
    lru_order: VecDeque<CacheKey>,
    used_bytes: usize,
    active_job: Option<OverviewJob>,
    prefetch_job: Option<OverviewJob>,
    star_job: Option<StarJob>,
    solve_job: Option<SolveJob>,
    prefetch_queue: VecDeque<CacheKey>,
    failed_keys: HashMap<CacheKey, String>,
    /// Neighbours whose background prefetch failed (e.g. not enough free RAM at that moment).
    /// They are not prefetched again, but opening them as the active frame still retries,
    /// so a transient memory shortage never sticks as a permanent error (ADR-023).
    prefetch_failed: HashSet<CacheKey>,
    /// Frames whose header WCS the user has chosen to show (pressed Solve once). Header WCS
    /// is parsed on open but stays hidden until then (ADR-024). Kept across eviction so a
    /// frame the user returns to keeps its overlays.
    header_wcs_shown: HashSet<CacheKey>,
}

/// What pressing Solve did for a frame (ADR-024).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SolveAction {
    /// The frame's WCS from its file header is now shown; nothing was computed.
    ShowedHeaderWcs,
    /// A real plate solve (local Gaia / blind index) was started.
    Started,
    /// The running solve for this frame was cancelled.
    Cancelled,
}

impl FrameCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn frame_byte_cost(frame: &OverviewFrame) -> usize {
        frame.pixels.len() * std::mem::size_of::<[f32; 3]>() + frame.cards.len() * 128 + 4096
    }

    pub fn get(&mut self, key: &CacheKey) -> Option<Arc<OverviewFrame>> {
        if let Some(frame) = self.entries.get(key).cloned() {
            self.touch_lru(key);
            Some(frame)
        } else {
            None
        }
    }

    /// Why star analysis is unavailable for this frame, if it failed or was skipped.
    pub fn get_star_error(&self, key: &CacheKey) -> Option<&str> {
        self.star_failed.get(key).map(String::as_str)
    }

    /// Why the RAW Stage-2 (linear sensor) decode failed, if it did.
    pub fn get_stage2_error(&self, key: &CacheKey) -> Option<&str> {
        self.stage2_failed.get(key).map(String::as_str)
    }

    pub fn get_star_report(&self, key: &CacheKey) -> Option<Arc<StarQualityReport>> {
        self.star_reports.get(key).cloned()
    }

    pub fn is_computing_stars(&self, key: &CacheKey) -> bool {
        self.star_job.as_ref().is_some_and(|j| &j.key == key)
    }

    /// The WCS to display for `key`: a computed solve, or the header WCS once the user has
    /// pressed Solve for that frame (ADR-024). `None` keeps all sky overlays hidden.
    pub fn get_plate_report(&self, key: &CacheKey) -> Option<Arc<PlateSolveReport>> {
        self.plate_reports
            .get(key)
            .filter(|r| r.method != SolveMethod::HeaderWcs || self.header_wcs_shown.contains(key))
            .cloned()
    }

    /// Plate scale (″/px) of the frame's header WCS even while it is hidden, for star metrics.
    pub fn header_wcs_scale(&self, key: &CacheKey) -> Option<f64> {
        self.plate_reports
            .get(key)
            .filter(|r| r.method == SolveMethod::HeaderWcs)
            .map(|r| r.wcs.scale())
            .filter(|s| s.is_finite() && *s > 0.0)
    }

    /// Whether the frame has a header WCS that pressing Solve would show without computing.
    pub fn has_hidden_header_wcs(&self, key: &CacheKey) -> bool {
        !self.header_wcs_shown.contains(key)
            && self
                .plate_reports
                .get(key)
                .is_some_and(|r| r.method == SolveMethod::HeaderWcs)
    }

    /// Solve button / `S` (ADR-024): cancel a running solve for this frame; otherwise show a
    /// still hidden header WCS instantly; otherwise run a real plate solve.
    pub fn solve_or_show_header(&mut self, key: CacheKey, cfg: CatalogConfig) -> SolveAction {
        if self.is_solving(&key) {
            self.cancel_plate_solve();
            return SolveAction::Cancelled;
        }
        if self.has_hidden_header_wcs(&key) {
            self.header_wcs_shown.insert(key);
            return SolveAction::ShowedHeaderWcs;
        }
        self.request_plate_solve(key, cfg);
        SolveAction::Started
    }

    pub fn get_plate_error(&self, key: &CacheKey) -> Option<&str> {
        self.plate_errors.get(key).map(String::as_str)
    }

    pub fn is_solving(&self, key: &CacheKey) -> bool {
        self.solve_job.as_ref().is_some_and(|j| &j.key == key)
    }

    pub fn request_plate_solve(&mut self, key: CacheKey, cfg: CatalogConfig) {
        self.plate_errors.remove(&key);
        let cached_index = self.blind_index.clone();
        let source = self.sources.get(&key).cloned();
        self.solve_job = Some(spawn_solve_job(key, cfg, cached_index, source));
    }

    /// Keep a decoded source plane for reuse, evicting the least recently stored ones when
    /// over budget. Planes larger than the whole budget are not kept.
    fn store_source(&mut self, key: CacheKey, source: Arc<RawFrame>) {
        let cost = source.pixels.len() * std::mem::size_of::<f32>();
        self.drop_source(&key);
        if cost > MAX_SOURCE_BYTES {
            return;
        }
        while self.source_bytes + cost > MAX_SOURCE_BYTES {
            let Some(oldest) = self.source_lru.pop_front() else {
                break;
            };
            if let Some(old) = self.sources.remove(&oldest) {
                self.source_bytes = self
                    .source_bytes
                    .saturating_sub(old.pixels.len() * std::mem::size_of::<f32>());
            }
        }
        self.source_bytes += cost;
        self.source_lru.push_back(key.clone());
        self.sources.insert(key, source);
    }

    fn drop_source(&mut self, key: &CacheKey) {
        if let Some(old) = self.sources.remove(key) {
            self.source_bytes = self
                .source_bytes
                .saturating_sub(old.pixels.len() * std::mem::size_of::<f32>());
            self.source_lru.retain(|k| k != key);
        }
    }

    /// Whether a decoded source plane is held for `key` (tests and diagnostics).
    pub fn has_source(&self, key: &CacheKey) -> bool {
        self.sources.contains_key(key)
    }

    pub fn cancel_plate_solve(&mut self) {
        self.solve_job = None;
    }

    /// Refresh the cached `OverviewFrame` header cards after an atomic WCS save to disk.
    pub fn reload_header_after_wcs_save(&mut self, key: &CacheKey) {
        let Some(existing) = self.entries.get(key).cloned() else {
            return;
        };
        if let Ok(mut hdr) = read_header_hdu(&key.path, key.hdu_index) {
            hdr.info.apply_layout(key.layout);
            let mut updated = (*existing).clone();
            updated.header_wcs = build_header_wcs_report(&hdr);
            updated.cards = hdr.header_cards(false);
            updated.info = hdr.info;
            self.entries.insert(key.clone(), Arc::new(updated));
        }
    }

    pub fn get_error(&self, key: &CacheKey) -> Option<&str> {
        self.failed_keys.get(key).map(String::as_str)
    }

    pub fn is_loading(&self, key: &CacheKey) -> bool {
        self.active_job.as_ref().is_some_and(|j| &j.key == key)
            || self.prefetch_job.as_ref().is_some_and(|j| &j.key == key)
    }

    pub fn remove_path(&mut self, path: &Path) {
        let keys: Vec<CacheKey> = self
            .entries
            .keys()
            .filter(|k| k.path == path)
            .cloned()
            .collect();
        for k in keys {
            if let Some(removed) = self.entries.remove(&k) {
                self.used_bytes = self
                    .used_bytes
                    .saturating_sub(Self::frame_byte_cost(&removed));
            }
            self.star_reports.remove(&k);
            self.star_failed.remove(&k);
            self.stage2_failed.remove(&k);
            self.plate_reports.remove(&k);
            self.plate_errors.remove(&k);
            self.drop_source(&k);
            self.lru_order.retain(|x| x != &k);
            self.failed_keys.remove(&k);
            self.prefetch_failed.remove(&k);
            self.header_wcs_shown.remove(&k);
        }
    }

    fn touch_lru(&mut self, key: &CacheKey) {
        self.lru_order.retain(|k| k != key);
        self.lru_order.push_back(key.clone());
    }

    pub fn insert(&mut self, key: CacheKey, frame: Arc<OverviewFrame>) {
        let cost = Self::frame_byte_cost(&frame);
        if let Some(old) = self.entries.remove(&key) {
            self.used_bytes = self.used_bytes.saturating_sub(Self::frame_byte_cost(&old));
            self.lru_order.retain(|k| k != &key);
        }
        while (self.used_bytes + cost > MAX_CACHE_BYTES || self.entries.len() >= MAX_CACHE_ENTRIES)
            && !self.lru_order.is_empty()
        {
            if let Some(oldest_key) = self.lru_order.pop_front() {
                if let Some(removed) = self.entries.remove(&oldest_key) {
                    self.used_bytes = self
                        .used_bytes
                        .saturating_sub(Self::frame_byte_cost(&removed));
                }
                self.star_reports.remove(&oldest_key);
                self.star_failed.remove(&oldest_key);
                self.stage2_failed.remove(&oldest_key);
                self.plate_reports.remove(&oldest_key);
                self.plate_errors.remove(&oldest_key);
                self.drop_source(&oldest_key);
            }
        }
        // Populate initial Header WCS report if available and no explicit solve exists yet
        if let Some(ref hdr_wcs) = frame.header_wcs {
            let has_explicit_solve = self
                .plate_reports
                .get(&key)
                .is_some_and(|r| r.method != SolveMethod::HeaderWcs);
            if !has_explicit_solve {
                self.plate_reports
                    .insert(key.clone(), Arc::new(hdr_wcs.clone()));
            }
        }
        self.used_bytes += cost;
        self.lru_order.push_back(key.clone());
        self.entries.insert(key, frame);
    }

    /// Update the desired active key and neighbor prefetch list.
    /// Call once per frame or whenever selection changes.
    pub fn request_active_and_neighbors(&mut self, active: CacheKey, neighbors: Vec<PathBuf>) {
        // If the prefetch job happens to be loading what is now the active key, promote it!
        if self
            .prefetch_job
            .as_ref()
            .is_some_and(|job| job.key == active)
        {
            self.active_job = self.prefetch_job.take();
        }

        // Start an active job if the frame is not cached, or is cached only as a RAW Stage-1
        // preview whose Stage-2 decode has not failed yet. Recorded failures are final until
        // the file is reopened, so a broken file can never be decoded in a loop.
        let needs_active_load = match self.entries.get(&active) {
            None => !self.failed_keys.contains_key(&active),
            Some(existing) => {
                existing.raw_stage == RawStage::Stage1Preview
                    && !self.stage2_failed.contains_key(&active)
            }
        };
        if needs_active_load {
            let already_active = self
                .active_job
                .as_ref()
                .is_some_and(|job| job.key == active);
            if !already_active {
                // A still-running load for another frame means the user is moving quickly
                // (key repeat): debounce so skipped frames are never read.
                let debounce = self.active_job.is_some();
                let is_stage1_cached = self.entries.contains_key(&active);
                self.active_job = Some(spawn_overview_job(
                    active.clone(),
                    !is_stage1_cached,
                    debounce,
                    false,
                ));
            }
        } else {
            self.active_job = None;
        }

        // Schedule asynchronous background star photometry once the active frame is ready
        let active_stage = self.entries.get(&active).map(|f| f.raw_stage);
        if active_stage == Some(RawStage::Stage2PreviewOnly) {
            // Only the embedded JPEG exists: star metrics would be misleading.
            self.star_failed
                .entry(active.clone())
                .or_insert_with(|| RAW_PREVIEW_ONLY_STARS_MESSAGE.to_owned());
        }
        let ready_for_stars = matches!(
            active_stage,
            Some(stage) if stage != RawStage::Stage1Preview && stage != RawStage::Stage2PreviewOnly
        );
        if ready_for_stars
            && !self.star_reports.contains_key(&active)
            && !self.star_failed.contains_key(&active)
        {
            let already_running = self.star_job.as_ref().is_some_and(|j| j.key == active);
            if !already_running {
                let source = self.sources.get(&active).cloned();
                self.star_job = Some(spawn_star_job(active.clone(), source));
            }
        } else if self.star_job.as_ref().is_some_and(|j| j.key != active) {
            // Cancel obsolete star job when user navigates to another frame
            self.star_job = None;
        }

        // Update prefetch queue for neighbors using hdu_index = 0 and the same layout
        self.prefetch_queue.clear();
        for path in neighbors {
            let nkey = CacheKey {
                path,
                hdu_index: 0,
                layout: active.layout,
            };
            if nkey != active
                && !self.entries.contains_key(&nkey)
                && !self.failed_keys.contains_key(&nkey)
                && !self.prefetch_failed.contains(&nkey)
            {
                self.prefetch_queue.push_back(nkey);
            }
        }
    }

    /// Apply the result of the active overview job.
    fn finish_active(&mut self, key: CacheKey, outcome: ActiveOutcome) {
        match outcome {
            ActiveOutcome::Loaded((frame, source)) => {
                if frame.raw_stage != RawStage::Stage1Preview {
                    self.active_job = None;
                }
                if let Some(source) = source {
                    self.store_source(key.clone(), source);
                }
                self.insert(key, frame);
            }
            ActiveOutcome::Failed(err) => {
                self.active_job = None;
                if self.entries.contains_key(&key) {
                    // Stage-1 preview stays visible; remember that Stage 2 failed.
                    self.stage2_failed.insert(key, err);
                } else {
                    self.failed_keys.insert(key, err);
                }
            }
        }
    }

    /// Poll background jobs for completion and dispatch next neighbor prefetch if idle.
    /// Returns `true` if a job completed (so the UI knows to refresh).
    ///
    /// Every job ends in exactly one recorded state (result, error, or cancelled by the
    /// cache itself). A worker that disappears without a result (panic) is recorded as an
    /// error, so the cache never restarts it on the next frame.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;

        if let Some(job) = &self.active_job {
            let outcome = match job.poll() {
                JobPoll::Done(Ok(frame)) => Some(ActiveOutcome::Loaded(frame)),
                JobPoll::Done(Err(err)) => Some(ActiveOutcome::Failed(err.to_string())),
                JobPoll::Pending => None,
                JobPoll::Stopped => Some(ActiveOutcome::Failed(WORKER_STOPPED.to_owned())),
            };
            if let Some(outcome) = outcome {
                let key = job.key.clone();
                self.finish_active(key, outcome);
                changed = true;
            }
        }

        if let Some(job) = &self.star_job {
            let outcome = match job.poll() {
                JobPoll::Done(result) => Some(result.map_err(|e| e.to_string())),
                JobPoll::Pending => None,
                JobPoll::Stopped => Some(Err(WORKER_STOPPED.to_owned())),
            };
            if let Some(outcome) = outcome {
                let key = job.key.clone();
                self.star_job = None;
                match outcome {
                    Ok(report) => {
                        self.star_reports.insert(key, report);
                    }
                    Err(err) => {
                        self.star_failed.insert(key, err);
                    }
                }
                changed = true;
            }
        }

        if let Some(job) = &self.solve_job {
            let outcome = match job.poll() {
                JobPoll::Done(result) => Some(result.map_err(|e| e.to_string())),
                JobPoll::Pending => None,
                JobPoll::Stopped => Some(Err(WORKER_STOPPED.to_owned())),
            };
            if let Some(outcome) = outcome {
                let key = job.key.clone();
                self.solve_job = None;
                match outcome {
                    Ok((report, maybe_idx)) => {
                        if let Some(idx) = maybe_idx {
                            self.blind_index = Some(idx);
                        }
                        self.plate_errors.remove(&key);
                        self.plate_reports.insert(key, report);
                    }
                    Err(err) => {
                        self.plate_errors.insert(key, err);
                    }
                }
                changed = true;
            }
        }

        if let Some(job) = &self.prefetch_job {
            let outcome = match job.poll() {
                JobPoll::Done(result) => Some(result.map_err(|e| e.to_string())),
                JobPoll::Pending => None,
                JobPoll::Stopped => Some(Err(WORKER_STOPPED.to_owned())),
            };
            if let Some(outcome) = outcome {
                let key = job.key.clone();
                self.prefetch_job = None;
                match outcome {
                    Ok((frame, source)) => {
                        if let Some(source) = source {
                            self.store_source(key.clone(), source);
                        }
                        self.insert(key, frame);
                        changed = true;
                    }
                    Err(_) => {
                        self.prefetch_failed.insert(key);
                    }
                }
            }
        }

        // Only run background prefetch when the active image has finished loading
        if self.active_job.is_none() && self.prefetch_job.is_none() && self.solve_job.is_none() {
            while let Some(next_key) = self.prefetch_queue.pop_front() {
                if !self.entries.contains_key(&next_key)
                    && !self.failed_keys.contains_key(&next_key)
                    && !self.prefetch_failed.contains(&next_key)
                {
                    self.prefetch_job = Some(spawn_overview_job(next_key, false, false, true));
                    break;
                }
            }
        }

        changed
    }

    pub fn cached_count(&self) -> usize {
        self.entries.len()
    }

    pub fn used_megabytes(&self) -> f64 {
        self.used_bytes as f64 / (1024.0 * 1024.0)
    }

    pub fn is_busy(&self) -> bool {
        self.active_job.is_some()
            || self.prefetch_job.is_some()
            || self.star_job.is_some()
            || self.solve_job.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fulgur_core::raw::build_synthetic_dng_raw;

    fn write_u16_fits(path: &Path, w: usize, h: usize, base: u16) {
        write_u16_fits_with(path, w, h, base, &[]);
    }

    fn write_u16_fits_with(path: &Path, w: usize, h: usize, base: u16, extra: &[(&str, &str)]) {
        let card = |k: &str, v: &str| {
            let text = if k == "END" {
                "END".to_owned()
            } else {
                format!("{k:<8}= {v:>20}")
            };
            let mut c = text.into_bytes();
            c.resize(80, b' ');
            c
        };
        let mut bytes = Vec::new();
        for (k, v) in [
            ("SIMPLE", "T".to_owned()),
            ("BITPIX", "16".to_owned()),
            ("NAXIS", "2".to_owned()),
            ("NAXIS1", w.to_string()),
            ("NAXIS2", h.to_string()),
            ("BZERO", "32768".to_owned()),
        ] {
            bytes.extend(card(k, &v));
        }
        for &(k, v) in extra {
            bytes.extend(card(k, v));
        }
        bytes.extend(card("END", ""));
        while bytes.len() % 2880 != 0 {
            bytes.push(b' ');
        }
        for i in 0..w * h {
            let adu = base.wrapping_add((i % 1009) as u16);
            bytes.extend_from_slice(&((adu as i32 - 32768) as i16).to_be_bytes());
        }
        while bytes.len() % 2880 != 0 {
            bytes.push(0);
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn key(path: &Path) -> CacheKey {
        CacheKey {
            path: path.to_path_buf(),
            hdu_index: 0,
            layout: LayoutChoice::Auto,
        }
    }

    /// ADR-024: a header WCS is parsed on open but stays hidden; the first Solve shows it
    /// without a solve job, the next Solve runs a real solve, other frames stay hidden, and
    /// star metrics still get the header's plate scale.
    #[test]
    fn header_wcs_is_hidden_until_solve_and_second_solve_recomputes() {
        let dir = tempfile::tempdir().unwrap();
        let solved = dir.path().join("a_solved.fits");
        let other = dir.path().join("b_other.fits");
        // 2.0″/px TAN WCS (CD in degrees per pixel).
        let wcs = [
            ("CTYPE1", "'RA---TAN'"),
            ("CTYPE2", "'DEC--TAN'"),
            ("CRVAL1", "83.82"),
            ("CRVAL2", "-5.39"),
            ("CRPIX1", "32.5"),
            ("CRPIX2", "24.5"),
            ("CD1_1", "-5.5555556E-4"),
            ("CD1_2", "0.0"),
            ("CD2_1", "0.0"),
            ("CD2_2", "5.5555556E-4"),
        ];
        write_u16_fits_with(&solved, 64, 48, 1000, &wcs);
        write_u16_fits_with(&other, 64, 48, 1000, &wcs);

        let mut cache = FrameCache::new();
        let wait = |cache: &mut FrameCache, key: &CacheKey| {
            let t0 = std::time::Instant::now();
            while (cache.is_busy() || cache.get(key).is_none())
                && t0.elapsed() < std::time::Duration::from_secs(10)
            {
                cache.poll();
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        let (a, b) = (key(&solved), key(&other));
        cache.request_active_and_neighbors(a.clone(), vec![other.clone()]);
        wait(&mut cache, &a);

        // Opening (and prefetching the neighbour) shows nothing and starts no solve.
        assert!(cache.get_plate_report(&a).is_none());
        assert!(cache.solve_job.is_none());
        assert!(cache.has_hidden_header_wcs(&a));
        let scale = cache.header_wcs_scale(&a).unwrap();
        assert!((scale - 2.0).abs() < 1e-3, "{scale}");
        let t0 = std::time::Instant::now();
        while cache.get_star_report(&a).is_none()
            && cache.get_star_error(&a).is_none()
            && t0.elapsed() < std::time::Duration::from_secs(10)
        {
            cache.request_active_and_neighbors(a.clone(), vec![other.clone()]);
            cache.poll();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            cache.solve_job.is_none(),
            "star analysis must not start a solve"
        );
        let stars = cache.get_star_report(&a).expect("star analysis ran");
        let star_scale = stars.pixel_scale_arcsec.unwrap();
        assert!((star_scale - 2.0).abs() < 1e-3, "{star_scale}");

        // First Solve: header WCS shown instantly, no solve job.
        // Empty paths: the solve job is only started and cancelled, never awaited.
        let cfg = CatalogConfig {
            gaia_folder: String::new(),
            gaia_cache_folder: String::new(),
            solve_index_folder: String::new(),
            source_note: String::new(),
        };
        assert_eq!(
            cache.solve_or_show_header(a.clone(), cfg.clone()),
            SolveAction::ShowedHeaderWcs
        );
        assert!(cache.solve_job.is_none());
        let shown = cache.get_plate_report(&a).unwrap();
        assert_eq!(shown.method, SolveMethod::HeaderWcs);

        // Navigating to the neighbour: still hidden there; back again: still shown.
        cache.request_active_and_neighbors(b.clone(), vec![solved.clone()]);
        wait(&mut cache, &b);
        assert!(cache.get_plate_report(&b).is_none());
        assert!(cache.solve_job.is_none());
        cache.request_active_and_neighbors(a.clone(), vec![other.clone()]);
        assert!(cache.get_plate_report(&a).is_some());

        // Second Solve on the same frame runs a real solve; pressing again cancels it.
        assert_eq!(
            cache.solve_or_show_header(a.clone(), cfg.clone()),
            SolveAction::Started
        );
        assert!(cache.is_solving(&a));
        assert_eq!(
            cache.solve_or_show_header(a.clone(), cfg),
            SolveAction::Cancelled
        );
        assert!(!cache.is_solving(&a));
    }

    /// ADR-023: a neighbour whose prefetch fails (here unreadable; in practice e.g. not
    /// enough free RAM at that moment) is not prefetched again and does not affect the
    /// active frame, but opening it later still runs a fresh load instead of replaying
    /// the prefetch error.
    #[test]
    fn failed_prefetch_is_retried_when_the_frame_becomes_active() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("a_good.fits");
        let bad = dir.path().join("b_bad.fits");
        write_u16_fits(&good, 64, 48, 1000);
        std::fs::write(&bad, b"SIMPLE  = not a real header").unwrap();

        let mut cache = FrameCache::new();
        let wait = |cache: &mut FrameCache| {
            let t0 = std::time::Instant::now();
            while cache.is_busy() && t0.elapsed() < std::time::Duration::from_secs(10) {
                cache.poll();
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        cache.request_active_and_neighbors(key(&good), vec![bad.clone()]);
        wait(&mut cache);
        assert!(cache.get(&key(&good)).is_some(), "active frame unaffected");
        assert!(cache.prefetch_failed.contains(&key(&bad)));
        assert!(
            cache.get_error(&key(&bad)).is_none(),
            "prefetch error is not final"
        );

        // Asking again does not restart the failed prefetch.
        cache.request_active_and_neighbors(key(&good), vec![bad.clone()]);
        assert!(cache.prefetch_queue.is_empty());

        // Opening it runs a real load, which reports its own error.
        cache.request_active_and_neighbors(key(&bad), vec![good.clone()]);
        wait(&mut cache);
        assert!(cache.get_error(&key(&bad)).is_some());
    }

    /// Finding 01: once a RAW Stage-2 decode has failed, the cached Stage-1 preview must
    /// not trigger a new decode on every UI frame.
    #[test]
    fn failed_stage2_is_recorded_and_not_restarted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame.dng");
        std::fs::write(&path, build_synthetic_dng_raw(128, 96)).unwrap();
        let cancel = AtomicBool::new(false);
        let stage1 = load_raw_stage1_overview(&path, LayoutChoice::Auto, &cancel).unwrap();
        assert_eq!(stage1.raw_stage, RawStage::Stage1Preview);

        let mut cache = FrameCache::new();
        let k = key(&path);
        cache.insert(k.clone(), Arc::new(stage1));
        cache.finish_active(
            k.clone(),
            ActiveOutcome::Failed("simulated Stage-2 failure".to_owned()),
        );

        for _ in 0..5 {
            cache.request_active_and_neighbors(k.clone(), Vec::new());
            cache.poll();
            assert!(!cache.is_loading(&k), "failed Stage 2 must not restart");
        }
        assert_eq!(
            cache.get_stage2_error(&k),
            Some("simulated Stage-2 failure")
        );
        assert!(cache.get(&k).is_some(), "Stage-1 preview stays visible");

        // Rejecting / reopening the file clears the failure record.
        cache.remove_path(&path);
        assert!(cache.get_stage2_error(&k).is_none());
    }

    /// Finding 03: holding an arrow key across many frames never runs more than
    /// `MAX_CONCURRENT_DECODES` full-frame decodes at once, and the frame the user stops
    /// on is loaded.
    #[test]
    fn rapid_navigation_bounds_concurrent_decodes() {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = (0..16)
            .map(|i| {
                let p = dir.path().join(format!("Light_{i:03}.fits"));
                write_u16_fits(&p, 1024, 768, 1000 + i as u16);
                p
            })
            .collect();

        let mut cache = FrameCache::new();
        for (i, p) in paths.iter().enumerate() {
            let neighbors: Vec<PathBuf> = paths
                .iter()
                .skip(i.saturating_sub(2))
                .take(5)
                .cloned()
                .collect();
            cache.request_active_and_neighbors(key(p), neighbors);
            cache.poll();
            // ~ keyboard auto-repeat rate (30 Hz)
            std::thread::sleep(Duration::from_millis(33));
        }

        let last = key(paths.last().unwrap());
        let deadline = Instant::now() + Duration::from_secs(20);
        while cache.get(&last).is_none() && Instant::now() < deadline {
            cache.request_active_and_neighbors(last.clone(), Vec::new());
            cache.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(cache.get(&last).is_some(), "final frame must load");
        assert!(
            peak_concurrent_decodes() <= MAX_CONCURRENT_DECODES,
            "peak {} > {}",
            peak_concurrent_decodes(),
            MAX_CONCURRENT_DECODES
        );
    }

    /// Finding 08: the active frame keeps its decoded plane, and star analysis completes
    /// using it.
    #[test]
    fn active_frame_keeps_decoded_source_for_star_analysis() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Light_src.fits");
        write_u16_fits(&path, 256, 192, 1200);
        let k = key(&path);
        let mut cache = FrameCache::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while cache.get_star_report(&k).is_none()
            && cache.get_star_error(&k).is_none()
            && Instant::now() < deadline
        {
            cache.request_active_and_neighbors(k.clone(), Vec::new());
            cache.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(cache.get(&k).is_some());
        assert!(cache.has_source(&k));
        assert!(
            cache.get_star_report(&k).is_some() || cache.get_star_error(&k).is_some(),
            "star analysis must finish"
        );
        cache.remove_path(&path);
        assert!(!cache.has_source(&k));
    }
}
