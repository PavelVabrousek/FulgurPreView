//! Main application state: navigation, culling, keyboard shortcuts, folder watching, frame
//! cache requests and the per-frame `update` loop.
//!
//! The UI panels live in their own modules, each an `impl FulgurPreViewApp` block:
//! `toolbar`, `histogram_panel`, `star_panel` (incl. units and field maps),
//! `astrometry_panel`, `metadata_panel`, `status_bar`; window chrome (theme, welcome screen,
//! title-bar buttons, borderless resize) is in `chrome`.

mod astrometry_panel;
mod chrome;
mod histogram_panel;
mod metadata_panel;
mod small_bodies;
mod star_panel;
mod status_bar;
mod toolbar;

use chrome::*;

use crate::cache::{CacheKey, FrameCache, SolveAction};
use crate::directory::{DirectoryPlaylist, FileSig, SortOrder};
use crate::field_map::MapKind;
use crate::viewer::{ImageViewport, MAIN_BG_COLOR, OVERVIEW_TEXTURE, SkyOverlayConfig};
use crate::watch::{FolderWatcher, WATCH_INTERVAL};
use eframe::egui::{self, Color32, Key, RichText, Stroke, TextureHandle};
use fulgur_catalog::config::CatalogConfig;
use fulgur_catalog::sky::{format_dec, format_ra};
use fulgur_core::culling::CullingHistory;
use fulgur_core::fits::{is_astro_input, is_gps_keyword, save_wcs_to_file};
use fulgur_core::raw::RawStage;
use fulgur_core::{BayerPattern, LayoutChoice, Mosaic};
use fulgur_engine::field::format_length;
use fulgur_engine::histogram::{HISTOGRAM_BINS, evaluate_mtf_curve};
use fulgur_engine::sky::{PlateSolveReport, SolveMethod};
use fulgur_engine::stars::StarQualityReport;
use fulgur_engine::stretch::{ChannelView, OverviewFrame, StretchMode, StretchParams};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TiltMetric {
    Fwhm,
    Hfr,
    Eccentricity,
}

#[derive(Clone, PartialEq)]
struct UploadedTextureKey {
    path: PathBuf,
    hdu_index: usize,
    layout: LayoutChoice,
    raw_stage: RawStage,
    params: StretchParams,
}

pub struct FulgurPreViewApp {
    pub playlist: DirectoryPlaylist,
    pub cache: FrameCache,
    pub viewport: ImageViewport,
    pub culling: CullingHistory,
    pub catalog_config: CatalogConfig,
    pub hdu_index: usize,
    pub layout_choice: LayoutChoice,
    pub stretch_mode: StretchMode,
    pub manual_stretch: StretchParams,
    /// Manual sliders set all three channels to the same values (otherwise each channel keeps
    /// its own black / midtone / white offset, e.g. from an unlinked auto stretch).
    manual_link_channels: bool,
    pub user_overrode_stretch: bool,
    pub last_auto_format_stretch: Option<bool>,
    pub invert: bool,
    pub channel_view: ChannelView,
    pub lock_view: bool,
    pub show_histogram: bool,
    pub hist_zoom_to_signal: bool,
    pub hist_range_min: f32,
    pub hist_range_max: f32,
    pub hist_log_y: bool,
    pub hist_show_mtf: bool,
    pub hist_show_channels: [bool; 4],
    last_hist_frame_id: Option<(PathBuf, usize, LayoutChoice, RawStage)>,
    pub show_star_quality: bool,
    pub show_star_markers: bool,
    pub tilt_metric: TiltMetric,
    pub show_astrometry: bool,
    pub show_catalog_settings: bool,
    pub sky_overlays: SkyOverlayConfig,
    pub show_metadata: bool,
    pub show_manual_sliders: bool,
    pub metadata_filter: String,
    pub show_gps_metadata: bool,
    pub fullscreen: bool,
    pub status_banner: Option<(String, Instant)>,
    pub last_switch_latency_ms: Option<f64>,
    /// Background watcher of the open folder (new / rewritten / deleted frames).
    watcher: Option<FolderWatcher>,
    /// Last stable on-disk identity of each file, to detect frames rewritten on disk.
    known_sigs: HashMap<PathBuf, FileSig>,
    /// Jump to newly arrived frames while viewing the last frame of the folder.
    follow_new_frames: bool,
    /// Asteroid / comet searches per frame (right-click menu).
    small_body_searches: HashMap<CacheKey, small_bodies::SmallBodySearch>,
    /// Field map over the image: FWHM (key C) or eccentricity (key E).
    field_map: Option<MapKind>,
    /// Show FWHM / HFR and other lengths in arcseconds (when the scale is known).
    lengths_in_arcsec: bool,
    /// Arcseconds per pixel of the active frame: plate solution first, then header.
    active_arcsec_per_px: Option<f64>,
    /// Why the RAW linear (Stage-2) decode failed for the active frame, if it did.
    active_stage2_error: Option<String>,
    /// Why star analysis is unavailable for the active frame, if it is.
    active_star_error: Option<String>,
    switch_started_at: Option<Instant>,
    uploaded_texture: Option<(UploadedTextureKey, TextureHandle)>,
}

impl FulgurPreViewApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_target: Option<PathBuf>) -> Self {
        configure_dark_astro_theme(&cc.egui_ctx);
        let mut app = Self {
            playlist: DirectoryPlaylist::default(),
            cache: FrameCache::new(),
            viewport: ImageViewport::default(),
            culling: CullingHistory::with_journal(
                crate::crash::app_data_dir().join("culling_undo.json"),
            ),
            catalog_config: CatalogConfig::discover(),
            hdu_index: 0,
            layout_choice: LayoutChoice::Auto,
            stretch_mode: StretchMode::AutoUnlinked,
            manual_stretch: StretchParams::identity(),
            manual_link_channels: false,
            user_overrode_stretch: false,
            last_auto_format_stretch: None,
            invert: false,
            channel_view: ChannelView::Rgb,
            lock_view: true,
            show_histogram: true,
            hist_zoom_to_signal: true,
            hist_range_min: 0.0,
            hist_range_max: 1.0,
            hist_log_y: true,
            hist_show_mtf: true,
            hist_show_channels: [true, true, true, false],
            last_hist_frame_id: None,
            show_star_quality: true,
            show_star_markers: false,
            tilt_metric: TiltMetric::Fwhm,
            show_astrometry: true,
            show_catalog_settings: false,
            sky_overlays: SkyOverlayConfig::default(),
            show_metadata: true,
            show_manual_sliders: false,
            metadata_filter: String::new(),
            show_gps_metadata: false,
            fullscreen: false,
            status_banner: None,
            last_switch_latency_ms: None,
            watcher: None,
            known_sigs: HashMap::new(),
            follow_new_frames: false,
            field_map: None,
            small_body_searches: HashMap::new(),
            lengths_in_arcsec: false,
            active_arcsec_per_px: None,
            active_stage2_error: None,
            active_star_error: None,
            switch_started_at: None,
            uploaded_texture: None,
        };
        if let Some(target) = initial_target {
            app.open_path(&target);
        }
        app
    }

    pub fn open_path(&mut self, target: &Path) {
        self.switch_started_at = Some(Instant::now());
        self.hdu_index = 0;
        self.user_overrode_stretch = false;
        self.playlist = DirectoryPlaylist::open(target, self.playlist.sort_order);
        self.known_sigs.clear();
        self.watcher = self
            .playlist
            .directory
            .clone()
            .map(|dir| FolderWatcher::start(dir, WATCH_INTERVAL));
        self.sync_active_request();
    }

    fn sync_active_request(&mut self) {
        if let Some(path) = self.playlist.current_path().map(Path::to_path_buf) {
            let active_key = CacheKey {
                path,
                hdu_index: self.hdu_index,
                layout: self.layout_choice,
            };
            let neighbors = self.playlist.neighbors_for_prefetch(2);
            self.cache
                .request_active_and_neighbors(active_key, neighbors);
        }
    }

    /// Apply the latest folder-watcher listing: add newly arrived frames, drop deleted ones,
    /// and forget cached data (including recorded load failures) of frames rewritten on disk.
    fn poll_folder_watcher(&mut self) {
        let Some(listing) = self.watcher.as_ref().and_then(FolderWatcher::try_latest) else {
            return;
        };
        let mut next_known: HashMap<PathBuf, FileSig> = HashMap::new();
        for file in &listing {
            if file.stable {
                next_known.insert(file.sig.path.clone(), file.sig.clone());
            } else if let Some(old) = self.known_sigs.get(&file.sig.path) {
                // Still being written: keep the old identity until it settles.
                next_known.insert(file.sig.path.clone(), old.clone());
            }
        }
        let mut rewritten = false;
        for (path, old) in &self.known_sigs {
            if next_known.get(path) != Some(old) {
                self.cache.remove_path(path);
                rewritten |= next_known.contains_key(path);
            }
        }
        self.known_sigs = next_known;

        let was_at_last = self.playlist.is_at_last();
        let before = self.playlist.current_path().map(Path::to_path_buf);
        let added = self.playlist.apply_watch_listing(&listing);
        if added > 0 && self.follow_new_frames && was_at_last {
            self.playlist.last();
        }
        if added > 0 {
            self.status_banner = Some((
                format!(
                    "{added} new frame{} in folder",
                    if added == 1 { "" } else { "s" }
                ),
                Instant::now(),
            ));
        }
        let moved = self.playlist.current_path().map(Path::to_path_buf) != before;
        if moved {
            self.navigate_to(true);
        } else if rewritten || added > 0 {
            self.sync_active_request();
        }
    }

    fn navigate_to(&mut self, moved: bool) {
        if moved {
            self.switch_started_at = Some(Instant::now());
            self.hdu_index = 0;
            if !self.lock_view {
                self.user_overrode_stretch = false;
            }
            self.sync_active_request();
        }
    }

    /// Tooltip for the Solve buttons, describing what pressing it will do (ADR-024).
    fn solve_button_hint(&self, is_solving: bool) -> &'static str {
        let key = self.current_cache_key();
        if is_solving {
            "Cancel the running plate solve (S)"
        } else if key
            .as_ref()
            .is_some_and(|k| self.cache.has_hidden_header_wcs(k))
        {
            "Show the WCS stored in the file header: grid, compass, objects and sky readout, no solve needed (S)"
        } else if key
            .as_ref()
            .and_then(|k| self.cache.get_plate_report(k))
            .is_some()
        {
            "Re-solve: run local Gaia DR3 / blind index plate solving again on this frame (S)"
        } else {
            "Run local Gaia DR3 / blind index plate solving on the current frame (S)"
        }
    }

    fn trigger_plate_solve(&mut self) {
        let Some(key) = self.current_cache_key() else {
            return;
        };
        self.show_astrometry = true;
        match self
            .cache
            .solve_or_show_header(key, self.catalog_config.clone())
        {
            SolveAction::Cancelled => {
                self.status_banner = Some(("Plate solve cancelled.".to_owned(), Instant::now()));
            }
            SolveAction::ShowedHeaderWcs => {
                self.status_banner = Some((
                    "Showing the WCS stored in the file header (no solve). Press S again to re-solve."
                        .to_owned(),
                    Instant::now(),
                ));
            }
            SolveAction::Started => {}
        }
    }

    fn save_wcs_to_current_file(&mut self, report: &PlateSolveReport) {
        let Some(key) = self.current_cache_key() else {
            return;
        };
        if !is_astro_input(&key.path) {
            self.status_banner = Some((
                "WCS header save is supported for FITS and XISF files.".to_owned(),
                Instant::now(),
            ));
            return;
        }
        let Some(cards) = report.to_fits_cards() else {
            self.status_banner = Some((
                "WCS solution does not have a valid linear CD matrix.".to_owned(),
                Instant::now(),
            ));
            return;
        };
        match save_wcs_to_file(&key.path, key.hdu_index, &cards) {
            Ok(()) => {
                self.cache.reload_header_after_wcs_save(&key);
                // Our own rewrite must not look like an external change to the watcher.
                if let Ok(meta) = std::fs::metadata(&key.path) {
                    self.known_sigs.insert(
                        key.path.clone(),
                        FileSig {
                            path: key.path.clone(),
                            len: meta.len(),
                            modified: meta.modified().ok(),
                        },
                    );
                }
                let name = key
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.status_banner = Some((
                    format!("Saved WCS to '{name}' header (atomic .tmp rename)"),
                    Instant::now(),
                ));
            }
            Err(err) => {
                self.status_banner = Some((format!("WCS save failed: {err}"), Instant::now()));
            }
        }
    }

    fn reject_current_file(&mut self) {
        let Some(current) = self.playlist.current_path().map(Path::to_path_buf) else {
            return;
        };
        match self.culling.reject_file(&current) {
            Ok(entry) => {
                let name = current
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.cache.remove_path(&current);
                self.playlist.remove_current();
                self.hdu_index = 0;
                self.switch_started_at = Some(Instant::now());
                self.sync_active_request();
                self.status_banner = Some((
                    format!(
                        "Moved '{name}' to _Rejected/ ({})",
                        entry.rejected_path.display()
                    ),
                    Instant::now(),
                ));
            }
            Err(err) => {
                self.status_banner = Some((format!("Reject failed: {err}"), Instant::now()));
            }
        }
    }

    fn undo_last_reject(&mut self) {
        match self.culling.undo_last() {
            Ok(Some(action)) => {
                let restored = action.original_path;
                let name = restored
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                // The journal can restore a frame from another folder (previous session).
                if restored.parent() == self.playlist.directory.as_deref() {
                    self.playlist.rescan(Some(&restored));
                } else {
                    self.open_path(&restored);
                }
                self.hdu_index = 0;
                self.switch_started_at = Some(Instant::now());
                self.sync_active_request();
                self.status_banner =
                    Some((format!("Restored '{name}' from _Rejected/"), Instant::now()));
            }
            Ok(None) => {
                self.status_banner = Some(("Nothing to undo.".to_owned(), Instant::now()));
            }
            Err(err) => {
                self.status_banner = Some((format!("Undo failed: {err}"), Instant::now()));
            }
        }
    }

    fn handle_keyboard_shortcuts(&mut self, ctx: &egui::Context) {
        // Do not intercept typing when the metadata search box has keyboard focus
        if ctx.wants_keyboard_input() {
            return;
        }

        let mut nav_changed = false;
        let mut solve_requested = false;
        ctx.input_mut(|input| {
            if input.consume_key(egui::Modifiers::COMMAND, Key::O) {
                if let Some(file) = rfd::FileDialog::new()
                    .add_filter(
                        "Astro & Standard Images",
                        &[
                            "fit", "fits", "fts", "xisf", "tif", "tiff", "jpg", "jpeg", "png",
                            "webp", "bmp", "cr2", "cr3", "nef", "arw", "dng", "orf", "pef", "raf",
                            "rw2",
                        ],
                    )
                    .pick_file()
                {
                    self.open_path(&file);
                }
            }
            if input.consume_key(egui::Modifiers::COMMAND, Key::Z) {
                self.undo_last_reject();
            }
            if input.consume_key(egui::Modifiers::NONE, Key::ArrowRight)
                || input.consume_key(egui::Modifiers::NONE, Key::Space)
            {
                nav_changed |= self.playlist.next();
            }
            if input.consume_key(egui::Modifiers::NONE, Key::ArrowLeft)
                || input.consume_key(egui::Modifiers::NONE, Key::Backspace)
            {
                nav_changed |= self.playlist.prev();
            }
            if input.consume_key(egui::Modifiers::NONE, Key::Home) {
                nav_changed |= self.playlist.first();
            }
            if input.consume_key(egui::Modifiers::NONE, Key::End) {
                nav_changed |= self.playlist.last();
            }
            if input.consume_key(egui::Modifiers::NONE, Key::X)
                || input.consume_key(egui::Modifiers::NONE, Key::Delete)
            {
                self.reject_current_file();
            }
            if input.consume_key(egui::Modifiers::NONE, Key::F) {
                self.viewport.set_fit();
            }
            if input.consume_key(egui::Modifiers::NONE, Key::Num1) {
                self.viewport.set_zoom(1.0);
            }
            if input.consume_key(egui::Modifiers::NONE, Key::Num2) {
                self.viewport.set_zoom(2.0);
            }
            if input.consume_key(egui::Modifiers::NONE, Key::Num4) {
                self.viewport.set_zoom(4.0);
            }
            if input.consume_key(egui::Modifiers::NONE, Key::Plus)
                || input.consume_key(egui::Modifiers::NONE, Key::Equals)
            {
                self.viewport.zoom_by(1.25);
            }
            if input.consume_key(egui::Modifiers::NONE, Key::Minus) {
                self.viewport.zoom_by(0.8);
            }
            if input.consume_key(egui::Modifiers::NONE, Key::S) {
                solve_requested = true;
            }
            if input.consume_key(egui::Modifiers::NONE, Key::O) {
                self.show_astrometry = !self.show_astrometry;
            }
            if input.consume_key(egui::Modifiers::NONE, Key::T) {
                self.user_overrode_stretch = true;
                self.stretch_mode = match self.stretch_mode {
                    StretchMode::Linear => StretchMode::AutoUnlinked,
                    StretchMode::AutoUnlinked => StretchMode::AutoLinked,
                    StretchMode::AutoLinked => StretchMode::Linear,
                    StretchMode::Manual => StretchMode::AutoUnlinked,
                };
            }
            if input.consume_key(egui::Modifiers::NONE, Key::I) {
                self.invert = !self.invert;
            }
            if input.consume_key(egui::Modifiers::NONE, Key::B) {
                self.lock_view = !self.lock_view;
            }
            if input.consume_key(egui::Modifiers::NONE, Key::H) {
                self.show_histogram = !self.show_histogram;
            }
            if input.consume_key(egui::Modifiers::NONE, Key::W) {
                self.viewport.wheel_browse = !self.viewport.wheel_browse;
                self.status_banner = Some((
                    if self.viewport.wheel_browse {
                        "Mouse wheel: browse frames (Ctrl+wheel zooms)".to_owned()
                    } else {
                        "Mouse wheel: zoom".to_owned()
                    },
                    Instant::now(),
                ));
            }
            if input.consume_key(egui::Modifiers::NONE, Key::Q) {
                self.show_star_quality = !self.show_star_quality;
            }
            for (key, kind) in [(Key::C, MapKind::Fwhm), (Key::E, MapKind::Eccentricity)] {
                if input.consume_key(egui::Modifiers::NONE, key) {
                    self.field_map = if self.field_map == Some(kind) {
                        None
                    } else {
                        self.show_star_quality = true;
                        Some(kind)
                    };
                }
            }
            if input.consume_key(egui::Modifiers::NONE, Key::M) {
                self.show_metadata = !self.show_metadata;
            }
            if input.consume_key(egui::Modifiers::NONE, Key::F11) {
                self.fullscreen = !self.fullscreen;
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
            }
            if input.consume_key(egui::Modifiers::NONE, Key::Escape) && self.fullscreen {
                self.fullscreen = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            }
        });

        if solve_requested {
            self.trigger_plate_solve();
        }
        if nav_changed {
            self.navigate_to(true);
        }
    }

    fn handle_drag_and_drop(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if let Some(first) = dropped.first() {
            self.open_path(first);
        }
    }

    fn current_cache_key(&self) -> Option<CacheKey> {
        self.playlist.current_path().map(|p| CacheKey {
            path: p.to_path_buf(),
            hdu_index: self.hdu_index,
            layout: self.layout_choice,
        })
    }
}

impl eframe::App for FulgurPreViewApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        handle_borderless_window_resize(ctx, self.fullscreen);
        self.handle_drag_and_drop(ctx);
        self.handle_keyboard_shortcuts(ctx);

        self.poll_folder_watcher();
        if self.poll_small_body_searches() {
            ctx.request_repaint();
        }
        if self.watcher.is_some() {
            // Wake up periodically to pick up frames arriving in the folder.
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        if self.cache.poll() {
            ctx.request_repaint();
        }
        if self.cache.is_busy() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        let current_key = self.current_cache_key();
        let active_frame = current_key.as_ref().and_then(|k| self.cache.get(k));
        if let Some(k) = &current_key {
            let neighbors = self.playlist.neighbors_for_prefetch(2);
            self.cache
                .request_active_and_neighbors(k.clone(), neighbors);
        }
        let active_star_report = current_key
            .as_ref()
            .and_then(|k| self.cache.get_star_report(k));
        let is_computing_stars = current_key
            .as_ref()
            .is_some_and(|k| self.cache.is_computing_stars(k));
        let active_plate_report = current_key
            .as_ref()
            .and_then(|k| self.cache.get_plate_report(k));
        let is_solving = current_key
            .as_ref()
            .is_some_and(|k| self.cache.is_solving(k));
        let solve_error = current_key
            .as_ref()
            .and_then(|k| self.cache.get_plate_error(k))
            .map(str::to_owned);
        self.active_stage2_error = current_key
            .as_ref()
            .and_then(|k| self.cache.get_stage2_error(k))
            .map(str::to_owned);
        self.active_arcsec_per_px = active_plate_report
            .as_ref()
            .map(|p| p.wcs.scale())
            .filter(|s| s.is_finite() && *s > 0.0)
            // ADR-024: a hidden header WCS still gives star metrics their ″/px scale.
            .or_else(|| {
                current_key
                    .as_ref()
                    .and_then(|k| self.cache.header_wcs_scale(k))
            })
            .or_else(|| {
                current_key
                    .as_ref()
                    .and_then(|k| self.cache.get_star_report(k))
                    .and_then(|r| r.pixel_scale_arcsec)
            });
        self.active_star_error = current_key
            .as_ref()
            .and_then(|k| self.cache.get_star_error(k))
            .map(str::to_owned);

        // Apply format-aware default stretch and sync histogram signal bounds when frame or RAW stage changes
        if let Some(frame) = &active_frame {
            if let Some(started) = self.switch_started_at.take() {
                self.last_switch_latency_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
            }
            if !self.user_overrode_stretch
                && self.last_auto_format_stretch != Some(frame.default_stretch)
            {
                self.stretch_mode = if frame.default_stretch {
                    StretchMode::AutoUnlinked
                } else {
                    StretchMode::Linear
                };
                self.last_auto_format_stretch = Some(frame.default_stretch);
                self.hist_zoom_to_signal = frame.default_stretch;
            }
            if let Some(k) = &current_key {
                let frame_id = (k.path.clone(), k.hdu_index, k.layout, frame.raw_stage);
                if self.last_hist_frame_id.as_ref() != Some(&frame_id) {
                    if self.hist_zoom_to_signal {
                        self.hist_range_min = frame.histogram.signal_range[0];
                        self.hist_range_max = frame.histogram.signal_range[1];
                    } else {
                        self.hist_range_min = 0.0;
                        self.hist_range_max = 1.0;
                    }
                    self.last_hist_frame_id = Some(frame_id);
                }
            }
        }

        egui::TopBottomPanel::top("top_toolbar").show(ctx, |ui| {
            self.show_top_toolbar(ui, active_frame.as_ref());
        });

        egui::TopBottomPanel::bottom("bottom_status_bar").show(ctx, |ui| {
            self.show_status_bar(ui, active_frame.as_ref(), active_star_report.as_ref());
        });

        if self.show_astrometry
            || self.show_histogram
            || self.show_star_quality
            || self.show_metadata
        {
            if let Some(frame) = &active_frame {
                let frame_clone = Arc::clone(frame);
                let active_params = frame_clone.resolve_stretch(
                    self.stretch_mode,
                    &self.manual_stretch,
                    self.invert,
                    self.channel_view,
                );
                egui::SidePanel::right("metadata_inspector_panel")
                    .resizable(true)
                    .default_width(285.0)
                    .width_range(145.0..=560.0)
                    .show(ctx, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("right_inspector_scroll")
                            .scroll_bar_visibility(
                                egui::scroll_area::ScrollBarVisibility::AlwaysVisible,
                            )
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if self.show_astrometry {
                                    self.show_astrometry_panel(
                                        ui,
                                        active_plate_report.as_ref(),
                                        is_solving,
                                        solve_error.as_deref(),
                                    );
                                    if self.show_histogram
                                        || self.show_star_quality
                                        || self.show_metadata
                                    {
                                        ui.separator();
                                    }
                                }
                                if self.show_histogram {
                                    self.show_histogram_panel(ui, &frame_clone, active_params);
                                    self.show_signal_levels(
                                        ui,
                                        &frame_clone,
                                        active_star_report.as_deref(),
                                    );
                                    if self.show_star_quality || self.show_metadata {
                                        ui.separator();
                                    }
                                }
                                if self.show_star_quality {
                                    self.show_star_quality_panel(
                                        ui,
                                        active_star_report.as_ref(),
                                        is_computing_stars,
                                    );
                                    if self.show_metadata {
                                        ui.separator();
                                    }
                                }
                                if self.show_metadata {
                                    self.show_metadata_panel(ui, &frame_clone);
                                }
                            });
                    });
            }
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(MAIN_BG_COLOR))
            .show(ctx, |ui| {
                let Some(key) = current_key else {
                    show_empty_welcome_state(ui);
                    return;
                };

                if let Some(frame) = active_frame {
                    let resolved_params = frame.resolve_stretch(
                        self.stretch_mode,
                        &self.manual_stretch,
                        self.invert,
                        self.channel_view,
                    );
                    let tex_key = UploadedTextureKey {
                        path: key.path.clone(),
                        hdu_index: key.hdu_index,
                        layout: key.layout,
                        raw_stage: frame.raw_stage,
                        params: resolved_params,
                    };
                    let needs_upload = self
                        .uploaded_texture
                        .as_ref()
                        .is_none_or(|(k, _)| k != &tex_key);
                    if needs_upload {
                        let rgb8 = frame.render_rgb8(resolved_params);
                        let color_image =
                            egui::ColorImage::from_rgb([frame.width, frame.height], &rgb8);
                        let tex = ui.ctx().load_texture(
                            "overview_texture",
                            color_image,
                            OVERVIEW_TEXTURE,
                        );
                        self.uploaded_texture = Some((tex_key, tex));
                    }
                    let (_, tex_handle) = self.uploaded_texture.as_ref().unwrap();
                    let small_body_overlay =
                        self.small_body_overlay(&key, active_plate_report.as_deref());
                    self.viewport.show(
                        ui,
                        &key.path,
                        key.hdu_index,
                        key.layout,
                        tex_handle,
                        &frame,
                        resolved_params,
                        self.lock_view,
                        active_star_report.as_deref(),
                        self.show_star_markers,
                        self.field_map,
                        self.display_arcsec_per_px(),
                        active_plate_report.as_deref(),
                        self.sky_overlays,
                        small_body_overlay.as_ref(),
                    );
                    if let Some(request) = self.viewport.small_body_request.take() {
                        self.start_small_body_search(
                            key.clone(),
                            request,
                            &frame,
                            active_plate_report.as_deref(),
                        );
                    }
                    // Mouse wheel in browse mode: one frame per notch.
                    let steps = std::mem::take(&mut self.viewport.nav_request);
                    if steps != 0 {
                        let mut moved = false;
                        for _ in 0..steps.unsigned_abs() {
                            moved |= if steps > 0 {
                                self.playlist.next()
                            } else {
                                self.playlist.prev()
                            };
                        }
                        self.navigate_to(moved);
                    }
                } else if let Some(err) = self.cache.get_error(&key) {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "Could not load '{}':\n{err}",
                                key.path.display()
                            ))
                            .color(Color32::from_rgb(255, 120, 120))
                            .size(16.0),
                        );
                    });
                } else if self.cache.is_loading(&key) {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            RichText::new(format!("Loading {}...", key.path.display()))
                                .color(Color32::from_rgb(160, 200, 255))
                                .size(16.0),
                        );
                    });
                }
            });
    }
}
