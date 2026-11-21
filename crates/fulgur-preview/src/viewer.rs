//! Interactive image viewport with cursor-anchored mouse-wheel zoom, drag pan,
//! horizontal & vertical viewport scrollbars, Blink Mode geometry lock,
//! live pixel ADU & Cursor Star Inspector HUD, and 1:1 VNG detail ROI streaming.

use crate::field_map::{MapKind, MapOverlay, draw_map, map_color_image};
use crate::jobs::{Job, JobPoll};
use crate::small_body_view::{
    SmallBodyOverlay, SmallBodyRequest, SmallBodyState, draw_small_bodies,
};
use eframe::egui::{self, Color32, Pos2, Rect, RichText, Sense, Stroke, TextureHandle, Vec2};
use fulgur_core::{Error, LayoutChoice, Result};
use fulgur_engine::field::format_length;
use fulgur_engine::sky::small_bodies::{SEARCH_RADIUS_ARCMIN, SmallBody, VMAG_LIMIT};
use fulgur_engine::sky::{
    PlateSolveReport,
    catalogue::{format_dec, format_ra},
};
use fulgur_engine::stars::{RADIAL_PROFILE_BINS, Star, StarQualityReport};
use fulgur_engine::stretch::{OverviewFrame, StretchParams, load_detail_roi_rgb8};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const DETAIL_PIXELS: usize = 16_777_216;
const DETAIL_EDGE: usize = 8192;
const DETAIL_MARGIN: f64 = 1.4;
const DETAIL_DELAY: Duration = Duration::from_millis(50);
const SCROLLBAR_SIZE: f32 = 12.0;

pub const MAIN_BG_COLOR: Color32 = Color32::from_rgb(16, 48, 48);

pub const OVERVIEW_TEXTURE: egui::TextureOptions =
    egui::TextureOptions::LINEAR.with_mipmap_mode(Some(egui::TextureFilter::Linear));

const DETAIL_TEXTURE: egui::TextureOptions = egui::TextureOptions {
    magnification: egui::TextureFilter::Nearest,
    ..OVERVIEW_TEXTURE
};

#[derive(Clone, PartialEq)]
struct DetailKey {
    path: PathBuf,
    hdu_index: usize,
    layout: LayoutChoice,
    size: [usize; 2],
    bounds: [usize; 4],
    params: StretchParams,
}

type DetailTask = Job<DetailKey, Result<egui::ColorImage>>;

/// Decode and stretch the 1:1 native-resolution crop described by `key`.
fn start_detail_task(key: DetailKey, norm_min: f32, norm_span: f32) -> DetailTask {
    let req = key.clone();
    Job::spawn(key, "fulgur-detail", 1, move |cancel, tx| {
        let [_, _, width, height] = req.bounds;
        let res = load_detail_roi_rgb8(
            &req.path,
            req.hdu_index,
            req.layout,
            req.size,
            req.bounds,
            norm_min,
            norm_span,
            req.params,
            cancel,
        )
        .map(|bytes| egui::ColorImage::from_rgb([width, height], &bytes));
        let _ = tx.send(res);
    })
}

/// Toggleable celestial overlay flags for the main viewport.
#[derive(Clone, Copy, Debug)]
pub struct SkyOverlayConfig {
    pub show_radec_grid: bool,
    pub show_compass: bool,
    pub show_dso_annotations: bool,
    pub show_star_annotations: bool,
}

impl SkyOverlayConfig {
    /// Turn every celestial overlay on (`true`) or off (`false`).
    pub fn set_all(&mut self, visible: bool) {
        self.show_radec_grid = visible;
        self.show_compass = visible;
        self.show_dso_annotations = visible;
        self.show_star_annotations = visible;
    }

    pub fn all_visible(&self) -> bool {
        self.show_radec_grid
            && self.show_compass
            && self.show_dso_annotations
            && self.show_star_annotations
    }

    pub fn none_visible(&self) -> bool {
        !(self.show_radec_grid
            || self.show_compass
            || self.show_dso_annotations
            || self.show_star_annotations)
    }
}

impl Default for SkyOverlayConfig {
    fn default() -> Self {
        Self {
            show_radec_grid: true,
            show_compass: true,
            show_dso_annotations: true,
            show_star_annotations: false,
        }
    }
}

/// Live cursor telemetry over the image canvas, including snapped star photometry and sky RA/Dec.
#[derive(Clone, Debug)]
pub struct CursorReadout {
    pub x: usize,
    pub y: usize,
    pub adu: [f32; 3],
    pub norm: [f32; 3],
    pub hovered_star: Option<Star>,
    pub sky_ra_dec: Option<(f64, f64)>,
}

pub struct ImageViewport {
    /// Mouse wheel browses frames instead of zooming (Ctrl+wheel / pinch still zoom).
    pub wheel_browse: bool,
    /// Frames to move by, requested by the wheel in browse mode (+1 next, -1 previous). The
    /// app consumes it after `show`.
    pub nav_request: i32,
    /// Asteroid / comet search requested from the right-click menu; the app consumes it.
    pub small_body_request: Option<SmallBodyRequest>,
    /// Image position under the cursor when the context menu was opened.
    context_target: Option<ContextTarget>,
    wheel_accum: f32,
    pub fit: bool,
    pub scale: f32,
    pub center: Pos2,
    initialized: bool,
    last_path: Option<PathBuf>,
    native_size: Option<[usize; 2]>,
    task: Option<DetailTask>,
    desired: Option<DetailKey>,
    changed: Option<Instant>,
    ready: Option<(DetailKey, TextureHandle)>,
    pub detail_error: Option<String>,
    pub cursor_readout: Option<CursorReadout>,
    /// Colour map of the field-curvature surface, keyed by the fitted coefficients.
    field_texture: Option<(u64, egui::TextureHandle)>,
}

impl Default for ImageViewport {
    fn default() -> Self {
        Self {
            wheel_browse: false,
            nav_request: 0,
            small_body_request: None,
            context_target: None,
            wheel_accum: 0.0,
            fit: true,
            scale: 1.0,
            center: Pos2::ZERO,
            initialized: false,
            last_path: None,
            native_size: None,
            task: None,
            desired: None,
            changed: None,
            ready: None,
            detail_error: None,
            cursor_readout: None,
            field_texture: None,
        }
    }
}

impl ImageViewport {
    pub fn set_fit(&mut self) {
        self.fit = true;
        if let Some([w, h]) = self.native_size {
            self.center = Pos2::new(w as f32 * 0.5, h as f32 * 0.5);
        }
    }

    pub fn set_zoom(&mut self, scale: f32) {
        self.fit = false;
        self.scale = scale.clamp(0.02, 32.0);
    }

    pub fn zoom_by(&mut self, factor: f32) {
        self.fit = false;
        self.scale = (self.scale * factor).clamp(0.02, 32.0);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        path: &PathBuf,
        hdu_index: usize,
        layout: LayoutChoice,
        overview_tex: &TextureHandle,
        frame: &OverviewFrame,
        params: StretchParams,
        lock_view: bool,
        star_report: Option<&StarQualityReport>,
        show_star_markers: bool,
        field_map: Option<MapKind>,
        arcsec_per_px: Option<f64>,
        plate_report: Option<&PlateSolveReport>,
        sky_overlays: SkyOverlayConfig,
        small_bodies: Option<&SmallBodyOverlay>,
    ) {
        let previous_scale = self.scale;
        let native = Vec2::new(frame.native_size[0] as f32, frame.native_size[1] as f32);

        let path_changed = self.last_path.as_ref() != Some(path);
        let size_changed = self.native_size != Some(frame.native_size);
        if size_changed || (path_changed && !lock_view) || !self.initialized {
            self.center = (native * 0.5).to_pos2();
            self.fit = true;
            self.initialized = true;
            self.native_size = Some(frame.native_size);
        }
        if path_changed {
            self.last_path = Some(path.clone());
        }

        let ppp = ui.ctx().pixels_per_point();
        let outer_size = ui.available_size().max(Vec2::splat(32.0));
        let (outer_rect, _) = ui.allocate_exact_size(outer_size, Sense::hover());

        // Partition outer_rect into main image canvas + horizontal scrollbar (bottom) + vertical scrollbar (right)
        let canvas_w = (outer_rect.width() - SCROLLBAR_SIZE).max(16.0);
        let canvas_h = (outer_rect.height() - SCROLLBAR_SIZE).max(16.0);
        let viewport = Rect::from_min_size(outer_rect.min, Vec2::new(canvas_w, canvas_h));
        let hbar_rect = Rect::from_min_size(
            egui::pos2(outer_rect.left(), viewport.bottom()),
            Vec2::new(canvas_w, SCROLLBAR_SIZE),
        );
        let vbar_rect = Rect::from_min_size(
            egui::pos2(viewport.right(), outer_rect.top()),
            Vec2::new(SCROLLBAR_SIZE, canvas_h),
        );
        let corner_rect = Rect::from_min_size(
            egui::pos2(viewport.right(), viewport.bottom()),
            Vec2::splat(SCROLLBAR_SIZE),
        );

        let response = ui.interact(
            viewport,
            ui.id().with("image_canvas_sense"),
            Sense::click_and_drag(),
        );
        let hbar_resp = ui.interact(
            hbar_rect,
            ui.id().with("image_hbar_sense"),
            Sense::click_and_drag(),
        );
        let vbar_resp = ui.interact(
            vbar_rect,
            ui.id().with("image_vbar_sense"),
            Sense::click_and_drag(),
        );

        let fit_scale = (viewport.width() / native.x).min(viewport.height() / native.y) * ppp;

        if self.fit {
            self.scale = fit_scale;
            self.center = (native * 0.5).to_pos2();
        }
        if response.double_clicked() {
            self.fit = true;
            self.scale = fit_scale;
            self.center = (native * 0.5).to_pos2();
        }

        // Mouse wheel zooms the image around the cursor (default), or browses frames when
        // `wheel_browse` is on; Ctrl+wheel and pinch always zoom.
        if response.hovered() {
            let (smooth, raw, pinch, pointer) = ui.input_mut(|input| {
                let smooth = input.smooth_scroll_delta.y;
                let raw = input.raw_scroll_delta.y;
                input.smooth_scroll_delta = Vec2::ZERO;
                input.raw_scroll_delta = Vec2::ZERO;
                (smooth, raw, input.zoom_delta(), input.pointer.hover_pos())
            });
            let scroll = if self.wheel_browse {
                self.nav_request += wheel_steps(&mut self.wheel_accum, raw);
                0.0
            } else {
                self.wheel_accum = 0.0;
                smooth
            };
            let factor = (scroll * 0.003).exp() * pinch;
            if (factor - 1.0).abs() > 0.00001 {
                let pointer = pointer.unwrap_or(viewport.center());
                let old = self.scale / ppp;
                let new_scale = (self.scale * factor).clamp(fit_scale.min(0.01), 32.0);
                self.center += (pointer - viewport.center()) * (1.0 / old - ppp / new_scale);
                self.scale = new_scale;
                self.fit = false;
            }
        }

        let points_per_pixel = (self.scale / ppp).max(1e-5);
        if response.dragged() {
            self.fit = false;
            self.center -= ui.input(|i| i.pointer.delta()) / points_per_pixel;
        }

        // Handle interactive Horizontal & Vertical Scrollbar dragging/clicking
        let half_x = viewport.width() / points_per_pixel * 0.5;
        let half_y = viewport.height() / points_per_pixel * 0.5;
        let can_scroll_x = native.x > 2.0 * half_x;
        let can_scroll_y = native.y > 2.0 * half_y;

        if can_scroll_x && (hbar_resp.dragged() || hbar_resp.clicked()) {
            if let Some(pointer) = ui.input(|i| i.pointer.interact_pos()) {
                let frac_x = (viewport.width() / (native.x * points_per_pixel)).clamp(0.04, 1.0);
                let thumb_w = (hbar_rect.width() * frac_x)
                    .max(24.0)
                    .min(hbar_rect.width());
                let usable = (hbar_rect.width() - thumb_w).max(1.0);
                let t = ((pointer.x - hbar_rect.left() - thumb_w * 0.5) / usable).clamp(0.0, 1.0);
                self.fit = false;
                self.center.x = half_x + t * (native.x - 2.0 * half_x);
            }
        }
        if can_scroll_y && (vbar_resp.dragged() || vbar_resp.clicked()) {
            if let Some(pointer) = ui.input(|i| i.pointer.interact_pos()) {
                let frac_y = (viewport.height() / (native.y * points_per_pixel)).clamp(0.04, 1.0);
                let thumb_h = (vbar_rect.height() * frac_y)
                    .max(24.0)
                    .min(vbar_rect.height());
                let usable = (vbar_rect.height() - thumb_h).max(1.0);
                let t = ((pointer.y - vbar_rect.top() - thumb_h * 0.5) / usable).clamp(0.0, 1.0);
                self.fit = false;
                self.center.y = half_y + t * (native.y - 2.0 * half_y);
            }
        }

        for axis in 0..2 {
            let half = viewport.size()[axis] / points_per_pixel * 0.5;
            self.center[axis] = if native[axis] > 2.0 * half {
                self.center[axis].clamp(half, native[axis] - half)
            } else {
                native[axis] * 0.5
            };
        }

        let hover_pos = response.hover_pos();
        let menu_response = response.clone();
        if response.dragged() {
            response.on_hover_cursor(egui::CursorIcon::Grabbing);
        } else {
            response.on_hover_cursor(egui::CursorIcon::Crosshair);
        }

        let image_origin = viewport.center() - self.center.to_vec2() * points_per_pixel;
        let image_rect = Rect::from_min_size(image_origin, native * points_per_pixel);

        // Update live cursor coordinate, physical ADU, normalized [0..1] readout, and hovered star lookup
        self.cursor_readout = hover_pos.and_then(|pos| {
            if !image_rect.contains(pos) {
                return None;
            }
            let rel = (pos - image_origin) / points_per_pixel;
            let ix = (rel.x.floor() as isize).clamp(0, frame.native_size[0] as isize - 1) as usize;
            let iy = (rel.y.floor() as isize).clamp(0, frame.native_size[1] as isize - 1) as usize;
            let adu = frame.sample_adu(ix, iy)?;
            let inv_span = 1.0 / frame.norm_span.max(1e-12);
            let norm = [
                ((adu[0] - frame.norm_min) * inv_span).clamp(0.0, 1.0),
                ((adu[1] - frame.norm_min) * inv_span).clamp(0.0, 1.0),
                ((adu[2] - frame.norm_min) * inv_span).clamp(0.0, 1.0),
            ];
            let snap_radius_px = (14.0 / f64::from(points_per_pixel)).clamp(6.0, 28.0);
            let hovered_star = star_report
                .and_then(|rep| {
                    rep.nearest_star(f64::from(rel.x), f64::from(rel.y), snap_radius_px)
                })
                .cloned();
            let sky_ra_dec =
                plate_report.map(|rep| rep.wcs.pixel_to_sky([f64::from(rel.x), f64::from(rel.y)]));
            Some(CursorReadout {
                x: ix,
                y: iy,
                adu,
                norm,
                hovered_star,
                sky_ra_dec,
            })
        });

        // Right-click menu: identify asteroids & comets around the clicked position.
        if menu_response.secondary_clicked() {
            self.context_target = self
                .cursor_readout
                .as_ref()
                .map(|c| (c.x, c.y, c.sky_ra_dec));
        }
        let target = self.context_target;
        let mut request = None;
        menu_response.context_menu(|ui| match target {
            Some((x, y, Some((ra, dec)))) => {
                if ui
                    .button(format!(
                        "Identify asteroids & comets here ({SEARCH_RADIUS_ARCMIN:.0}\u{2032}, V \u{2264} {VMAG_LIMIT:.0})"
                    ))
                    .on_hover_text(
                        "Online query (NASA/JPL, MPC orbits) for the observation time in the header.",
                    )
                    .clicked()
                {
                    request = Some(SmallBodyRequest {
                        x_px: x as f64 + 0.5,
                        y_px: y as f64 + 0.5,
                        ra_deg: ra,
                        dec_deg: dec,
                    });
                    ui.close_menu();
                }
            }
            Some((_, _, None)) => {
                ui.add_enabled(
                    false,
                    egui::Button::new("Identify asteroids & comets here"),
                )
                .on_disabled_hover_text("Plate-solve the frame first (S).");
            }
            None => {
                ui.label("Right-click on the image");
            }
        });
        if request.is_some() {
            self.small_body_request = request;
        }

        let painter = ui.painter().with_clip_rect(viewport);
        painter.rect_filled(viewport, 0.0, MAIN_BG_COLOR);
        painter.image(
            overview_tex.id(),
            image_rect,
            Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );

        // Compute visible native bounds for 1:1 VNG detail ROI streaming
        let visible = image_rect.intersect(viewport);
        let start = ((visible.min - image_origin) / points_per_pixel)
            .floor()
            .max(Vec2::ZERO);
        let end = ((visible.max - image_origin) / points_per_pixel)
            .ceil()
            .min(native);
        let bounds = [
            start.x as usize,
            start.y as usize,
            (end.x - start.x).max(0.0) as usize,
            (end.y - start.y).max(0.0) as usize,
        ];
        let texture_edge = ui.input(|input| input.max_texture_side).min(DETAIL_EDGE);
        let overview_is_native = [frame.width, frame.height] == frame.native_size;
        let detail_wanted = !overview_is_native
            && self.scale > (frame.width as f32 / native.x) * 1.05
            && bounds[2] > 0
            && bounds[3] > 0
            && bounds[2] <= texture_edge
            && bounds[3] <= texture_edge
            && bounds[2].saturating_mul(bounds[3]) <= DETAIL_PIXELS;

        let desired = if detail_wanted {
            let covers = |key: &&DetailKey| {
                &key.path == path
                    && key.hdu_index == hdu_index
                    && key.layout == layout
                    && key.size == frame.native_size
                    && key.params == params
                    && contains(key.bounds, bounds)
            };
            let known = self.ready.as_ref().map(|(key, _)| key).filter(covers);
            match known.or(self.desired.as_ref().filter(covers)) {
                Some(key) => Some(key.clone()),
                None => Some(DetailKey {
                    path: path.clone(),
                    hdu_index,
                    layout,
                    size: frame.native_size,
                    bounds: widened(bounds, frame.native_size, texture_edge),
                    params,
                }),
            }
        } else {
            None
        };

        if self.desired != desired {
            self.task = None;
            self.desired = desired;
            self.changed = Some(Instant::now());
            self.detail_error = None;
        }

        let completed = self.task.as_ref().and_then(|task| match task.poll() {
            JobPoll::Done(result) => Some(result),
            JobPoll::Pending => None,
            JobPoll::Stopped => Some(Err(Error::Invalid("Detail ROI worker stopped."))),
        });
        if let Some(result) = completed
            && let Some(task) = self.task.take()
        {
            match result {
                Ok(image) => {
                    self.ready = Some((
                        task.key.clone(),
                        ui.ctx()
                            .load_texture("native_detail_roi", image, DETAIL_TEXTURE),
                    ));
                }
                Err(err) => {
                    self.detail_error = Some(format!("Detail ROI: {err}"));
                }
            }
        }

        if let Some(key) = &self.desired {
            let ready = self.ready.as_ref().is_some_and(|(loaded, _)| loaded == key);
            if !ready && self.task.is_none() && self.detail_error.is_none() {
                if self.changed.is_some_and(|at| at.elapsed() >= DETAIL_DELAY) {
                    self.task = Some(start_detail_task(
                        key.clone(),
                        frame.norm_min,
                        frame.norm_span,
                    ));
                } else {
                    ui.ctx().request_repaint_after(DETAIL_DELAY);
                }
            }
        }

        let shown = self.ready.as_ref().filter(|(key, _)| {
            self.desired.is_some()
                && &key.path == path
                && key.hdu_index == hdu_index
                && key.size == frame.native_size
                && key.params == params
        });
        if let Some((key, texture)) = shown {
            let [x, y, w, h] = key.bounds;
            let rect = Rect::from_min_size(
                image_origin + egui::vec2(x as f32, y as f32) * points_per_pixel,
                egui::vec2(w as f32, h as f32) * points_per_pixel,
            );
            painter.image(
                texture.id(),
                rect,
                Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }

        // Optional overlay of detected star markers — radius scales proportionally with image zoom!
        if show_star_markers {
            if let Some(report) = star_report {
                let ref_fwhm = report.median_fwhm_px.unwrap_or(3.0);
                for star in &report.stars {
                    let pos = image_origin
                        + egui::vec2(star.x_px as f32, star.y_px as f32) * points_per_pixel;
                    if !viewport.contains(pos) {
                        continue;
                    }
                    let native_radius_px = (star.hfr_px as f32 * 3.8).max(5.5);
                    let radius = (native_radius_px * points_per_pixel).max(1.2);
                    let stroke_w = (1.1 * points_per_pixel.sqrt()).clamp(0.9, 2.2);
                    let ratio = star.moment_fwhm_px / ref_fwhm.max(0.5);
                    let col = if ratio <= 1.10 {
                        Color32::from_rgba_unmultiplied(80, 235, 155, 175)
                    } else if ratio <= 1.30 {
                        Color32::from_rgba_unmultiplied(255, 205, 85, 175)
                    } else {
                        Color32::from_rgba_unmultiplied(255, 110, 100, 180)
                    };
                    painter.circle_stroke(pos, radius, Stroke::new(stroke_w, col));
                }
            }
        }

        // Optional field map: FWHM (curvature / tilt / collimation) or eccentricity
        let overlay = match (field_map, star_report) {
            (Some(MapKind::Fwhm), Some(report)) => report
                .field
                .as_ref()
                .map(|f| MapOverlay::fwhm(f, arcsec_per_px)),
            (Some(MapKind::Eccentricity), Some(report)) => report
                .eccentricity_map
                .as_ref()
                .map(MapOverlay::eccentricity),
            _ => None,
        };
        if let Some(overlay) = overlay {
            let key = map_texture_key(field_map, &overlay.view);
            if self.field_texture.as_ref().is_none_or(|(k, _)| *k != key) {
                let tex = ui.ctx().load_texture(
                    "field_map",
                    map_color_image(overlay.view),
                    egui::TextureOptions::LINEAR,
                );
                self.field_texture = Some((key, tex));
            }
            if let Some((_, tex)) = &self.field_texture {
                draw_map(&painter, viewport, image_rect, &overlay, tex);
            }
        }

        // Asteroid / comet search: spinner while querying, then circle + identified objects
        if let Some(overlay) = small_bodies {
            let to_screen = |p: [f64; 2]| {
                image_origin + egui::vec2(p[0] as f32, p[1] as f32) * points_per_pixel
            };
            let motion = |body: &SmallBody, px: [f64; 2]| -> Option<Vec2> {
                let wcs = &plate_report?.wcs;
                let cos_dec = body.dec_deg.to_radians().cos().max(1e-6);
                let ra1 = body.ra_deg + body.ra_rate_arcsec_h / 3600.0 / cos_dec;
                let dec1 = body.dec_deg + body.dec_rate_arcsec_h / 3600.0;
                let p1 = wcs.sky_to_pixel(ra1.rem_euclid(360.0), dec1)?;
                Some(egui::vec2((p1[0] - px[0]) as f32, (p1[1] - px[1]) as f32) * points_per_pixel)
            };
            draw_small_bodies(
                &painter,
                viewport,
                overlay,
                points_per_pixel,
                to_screen,
                motion,
            );
            if matches!(overlay.state, SmallBodyState::Searching { .. }) {
                ui.ctx().request_repaint_after(Duration::from_millis(30));
            }
        }

        // Optional celestial WCS overlays: RA/Dec grid, DSO & Gaia star annotations, N/E compass rose
        if let Some(plate) = plate_report {
            if sky_overlays.show_radec_grid {
                draw_radec_grid(
                    &painter,
                    viewport,
                    image_origin,
                    points_per_pixel,
                    &plate.wcs,
                );
            }
            if sky_overlays.show_star_annotations || sky_overlays.show_dso_annotations {
                draw_sky_annotations(
                    &painter,
                    viewport,
                    image_origin,
                    points_per_pixel,
                    plate,
                    sky_overlays,
                );
            }
            if sky_overlays.show_compass {
                draw_compass_rose(&painter, viewport, &plate.wcs);
            }
        }

        // Hovered star reticle (scales with image zoom) & floating Cursor Star Inspector HUD
        if let Some(readout) = &self.cursor_readout {
            if let Some(star) = &readout.hovered_star {
                let star_screen = image_origin
                    + egui::vec2(star.x_px as f32, star.y_px as f32) * points_per_pixel;
                if viewport.contains(star_screen) {
                    let native_r_px = (star.hfr_px as f32 * 4.4).max(6.5);
                    let r = (native_r_px * points_per_pixel).max(2.2);
                    let gap = (2.0 * points_per_pixel).clamp(1.5, 6.0);
                    let tick = (5.0 * points_per_pixel).clamp(3.0, 14.0);
                    let reticle_col = Color32::from_rgb(255, 215, 90);
                    painter.circle_stroke(star_screen, r, Stroke::new(1.5_f32, reticle_col));
                    for (dx, dy) in [(-1.0_f32, 0.0_f32), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)] {
                        painter.line_segment(
                            [
                                star_screen + egui::vec2(dx * (r + gap), dy * (r + gap)),
                                star_screen
                                    + egui::vec2(dx * (r + gap + tick), dy * (r + gap + tick)),
                            ],
                            Stroke::new(1.4_f32, reticle_col),
                        );
                    }
                }

                draw_hovered_star_hud(&painter, viewport, star, arcsec_per_px);
            }
        }

        // Draw Horizontal & Vertical Scrollbars on outer_rect
        let bar_painter = ui.painter().with_clip_rect(outer_rect);
        draw_viewport_scrollbars(
            &bar_painter,
            hbar_rect,
            vbar_rect,
            corner_rect,
            &hbar_resp,
            &vbar_resp,
            viewport.size(),
            native,
            self.center,
            points_per_pixel,
        );

        if self.scale != previous_scale {
            ui.ctx().request_repaint();
        }
        if self.task.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(40));
        }
    }

    pub fn is_loading_detail(&self) -> bool {
        self.task.is_some()
    }
}

/// Renders the horizontal and vertical scrollbars for the main image viewport.
#[allow(clippy::too_many_arguments)]
fn draw_viewport_scrollbars(
    painter: &egui::Painter,
    hbar_rect: Rect,
    vbar_rect: Rect,
    corner_rect: Rect,
    hbar_resp: &egui::Response,
    vbar_resp: &egui::Response,
    view_size: Vec2,
    native: Vec2,
    center: Pos2,
    points_per_pixel: f32,
) {
    let track_bg = Color32::from_rgb(12, 36, 36);
    let track_border = Color32::from_rgb(28, 72, 72);
    painter.rect_filled(hbar_rect, 0.0, track_bg);
    painter.rect_filled(vbar_rect, 0.0, track_bg);
    painter.rect_filled(corner_rect, 0.0, track_bg);
    painter.line_segment(
        [hbar_rect.left_top(), hbar_rect.right_top()],
        Stroke::new(1.0_f32, track_border),
    );
    painter.line_segment(
        [vbar_rect.left_top(), vbar_rect.left_bottom()],
        Stroke::new(1.0_f32, track_border),
    );

    // Horizontal thumb
    let half_x = view_size.x / points_per_pixel * 0.5;
    let (h_start, h_len) = if native.x > 2.0 * half_x {
        let frac = (view_size.x / (native.x * points_per_pixel)).clamp(0.04, 1.0);
        let len = (hbar_rect.width() * frac).max(24.0).min(hbar_rect.width());
        let t = ((center.x - half_x) / (native.x - 2.0 * half_x)).clamp(0.0, 1.0);
        (hbar_rect.left() + t * (hbar_rect.width() - len), len)
    } else {
        (hbar_rect.left() + 2.0, (hbar_rect.width() - 4.0).max(8.0))
    };
    let h_thumb = Rect::from_min_max(
        egui::pos2(h_start, hbar_rect.top() + 2.0),
        egui::pos2(h_start + h_len, hbar_rect.bottom() - 2.0),
    );
    let h_col = if hbar_resp.dragged() {
        Color32::from_rgb(95, 185, 185)
    } else if hbar_resp.hovered() {
        Color32::from_rgb(70, 145, 145)
    } else {
        Color32::from_rgb(42, 98, 98)
    };
    painter.rect_filled(h_thumb, 4.0, h_col);

    // Vertical thumb
    let half_y = view_size.y / points_per_pixel * 0.5;
    let (v_start, v_len) = if native.y > 2.0 * half_y {
        let frac = (view_size.y / (native.y * points_per_pixel)).clamp(0.04, 1.0);
        let len = (vbar_rect.height() * frac)
            .max(24.0)
            .min(vbar_rect.height());
        let t = ((center.y - half_y) / (native.y - 2.0 * half_y)).clamp(0.0, 1.0);
        (vbar_rect.top() + t * (vbar_rect.height() - len), len)
    } else {
        (vbar_rect.top() + 2.0, (vbar_rect.height() - 4.0).max(8.0))
    };
    let v_thumb = Rect::from_min_max(
        egui::pos2(vbar_rect.left() + 2.0, v_start),
        egui::pos2(vbar_rect.right() - 2.0, v_start + v_len),
    );
    let v_col = if vbar_resp.dragged() {
        Color32::from_rgb(95, 185, 185)
    } else if vbar_resp.hovered() {
        Color32::from_rgb(70, 145, 145)
    } else {
        Color32::from_rgb(42, 98, 98)
    };
    painter.rect_filled(v_thumb, 4.0, v_col);
}

/// Draws the floating Star Inspector HUD card with radial flux profile when hovering a star.
fn draw_hovered_star_hud(
    painter: &egui::Painter,
    viewport: Rect,
    star: &Star,
    pixel_scale_arcsec: Option<f64>,
) {
    let hud_w = 248.0_f32;
    let hud_h = 136.0_f32;
    let margin = 10.0_f32;
    let hud_rect = Rect::from_min_size(
        egui::pos2(viewport.left() + margin, viewport.top() + margin),
        egui::vec2(hud_w, hud_h),
    );
    if !viewport.contains(hud_rect.max) {
        return;
    }

    painter.rect_filled(
        hud_rect,
        5.0,
        Color32::from_rgba_unmultiplied(14, 42, 42, 236),
    );
    painter.rect_stroke(
        hud_rect,
        5.0,
        Stroke::new(1.0_f32, Color32::from_rgb(65, 135, 135)),
        egui::StrokeKind::Middle,
    );

    let font_title = egui::FontId::proportional(12.0);
    let font_mono = egui::FontId::monospace(11.0);
    let x0 = hud_rect.left() + 8.0;
    let mut y = hud_rect.top() + 6.0;

    painter.text(
        egui::pos2(x0, y),
        egui::Align2::LEFT_TOP,
        format!("★ Star @ ({:.1}, {:.1})", star.x_px, star.y_px),
        font_title,
        Color32::from_rgb(255, 220, 110),
    );
    y += 16.0;

    let fwhm_str = format!(
        "FWHM: {}",
        format_length(star.moment_fwhm_px, pixel_scale_arcsec)
    );
    painter.text(
        egui::pos2(x0, y),
        egui::Align2::LEFT_TOP,
        fwhm_str,
        font_mono.clone(),
        Color32::from_rgb(130, 235, 175),
    );
    y += 14.0;

    painter.text(
        egui::pos2(x0, y),
        egui::Align2::LEFT_TOP,
        format!(
            "HFR:  {}   ·  Ecc: {:.2}",
            format_length(star.hfr_px, pixel_scale_arcsec),
            star.eccentricity
        ),
        font_mono.clone(),
        Color32::from_rgb(195, 225, 240),
    );
    y += 14.0;

    painter.text(
        egui::pos2(x0, y),
        egui::Align2::LEFT_TOP,
        format!("Peak: {:.0} ADU ·  SNR: {:.1}", star.peak_adu, star.snr),
        font_mono,
        Color32::from_rgb(175, 205, 210),
    );
    y += 16.0;

    // Mini Radial Profile Graph (r = 0 .. 6 px)
    let plot_rect = Rect::from_min_max(
        egui::pos2(x0, y),
        egui::pos2(hud_rect.right() - 8.0, hud_rect.bottom() - 6.0),
    );
    painter.rect_filled(plot_rect, 3.0, Color32::from_rgb(11, 32, 32));
    painter.rect_stroke(
        plot_rect,
        3.0,
        Stroke::new(1.0_f32, Color32::from_rgb(38, 82, 82)),
        egui::StrokeKind::Middle,
    );

    let inner = plot_rect.shrink(3.0);
    let mut pts = Vec::with_capacity(RADIAL_PROFILE_BINS);
    for (b, &val) in star.radial_profile.iter().enumerate() {
        let t = b as f32 / (RADIAL_PROFILE_BINS - 1) as f32;
        let px = inner.left() + t * inner.width();
        let py = inner.bottom() - val.clamp(0.0, 1.0) * inner.height();
        pts.push(egui::pos2(px, py));
    }
    painter.add(egui::Shape::line(
        pts,
        Stroke::new(1.5_f32, Color32::from_rgb(100, 215, 255)),
    ));

    // HFR vertical marker on the radial profile (r in 0..6 px)
    let hfr_t = (star.hfr_px as f32 / 6.0).clamp(0.0, 1.0);
    let hfr_x = inner.left() + hfr_t * inner.width();
    painter.line_segment(
        [
            egui::pos2(hfr_x, inner.top()),
            egui::pos2(hfr_x, inner.bottom()),
        ],
        Stroke::new(1.1_f32, Color32::from_rgb(255, 200, 90)),
    );
    let _ = RichText::new("");
}

fn contains(outer: [usize; 4], inner: [usize; 4]) -> bool {
    inner[0] >= outer[0]
        && inner[1] >= outer[1]
        && inner[0] + inner[2] <= outer[0] + outer[2]
        && inner[1] + inner[3] <= outer[1] + outer[3]
}

fn widened(bounds: [usize; 4], size: [usize; 2], edge: usize) -> [usize; 4] {
    let area = (bounds[2] * bounds[3]).max(1) as f64;
    let grow = (DETAIL_PIXELS as f64 / area)
        .sqrt()
        .clamp(1.0, DETAIL_MARGIN);
    let side = |start: usize, length: usize, limit: usize| {
        let wide = ((length as f64 * grow) as usize)
            .min(edge.max(length))
            .min(limit)
            .max(length);
        (
            start.saturating_sub((wide - length) / 2).min(limit - wide),
            wide,
        )
    };
    let (x, width) = side(bounds[0], bounds[2], size[0]);
    let (y, height) = side(bounds[1], bounds[3], size[1]);
    [x, y, width, height]
}

fn choose_grid_step_deg(span_deg: f64) -> f64 {
    let target = (span_deg / 4.5).max(1.0 / 3600.0);
    let steps = [
        1.0 / 120.0, // 0.5'
        1.0 / 60.0,  // 1'
        2.0 / 60.0,  // 2'
        5.0 / 60.0,  // 5'
        10.0 / 60.0, // 10'
        15.0 / 60.0, // 15'
        30.0 / 60.0, // 30'
        1.0,
        2.0,
        5.0,
        10.0,
        15.0,
        30.0,
    ];
    steps.into_iter().find(|&s| s >= target).unwrap_or(30.0)
}

/// Renders celestial Right Ascension and Declination grid curves and edge labels.
fn draw_radec_grid(
    painter: &egui::Painter,
    viewport: Rect,
    image_origin: Pos2,
    points_per_pixel: f32,
    wcs: &fulgur_engine::sky::solve::Wcs,
) {
    let w = wcs.width as f64;
    let h = wcs.height as f64;
    if w < 4.0 || h < 4.0 {
        return;
    }

    let (c_ra, c_dec) = wcs.centre_sky();
    let mut min_dec = c_dec;
    let mut max_dec = c_dec;
    let mut min_dra = 0.0_f64;
    let mut max_dra = 0.0_f64;

    for sy in 0..=6 {
        for sx in 0..=6 {
            let px = [w * sx as f64 / 6.0, h * sy as f64 / 6.0];
            let (ra, dec) = wcs.pixel_to_sky(px);
            min_dec = min_dec.min(dec);
            max_dec = max_dec.max(dec);
            let dra = (ra - c_ra + 540.0).rem_euclid(360.0) - 180.0;
            min_dra = min_dra.min(dra);
            max_dra = max_dra.max(dra);
        }
    }

    let dec_span = (max_dec - min_dec).max(1e-4);
    let dec_step = choose_grid_step_deg(dec_span);
    let cos_dec = c_dec.to_radians().cos().abs().clamp(0.05, 1.0);
    let ra_step = choose_grid_step_deg((max_dra - min_dra).abs() * cos_dec / 15.0) * 15.0;

    let grid_stroke = Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(95, 205, 240, 95));
    let label_bg = Color32::from_rgba_unmultiplied(12, 36, 36, 200);
    let dec_label_col = Color32::from_rgb(135, 225, 250);
    let ra_label_col = Color32::from_rgb(175, 235, 210);
    let font = egui::FontId::monospace(10.0);

    let to_screen = |ra: f64, dec: f64| -> Option<Pos2> {
        let px = wcs.sky_to_pixel(ra.rem_euclid(360.0), dec)?;
        if !wcs.inside(px, 64.0) {
            return None;
        }
        Some(image_origin + egui::vec2(px[0] as f32, px[1] as f32) * points_per_pixel)
    };

    // 1. Constant Declination curves
    let dec_start = ((min_dec - dec_step) / dec_step).floor() as i64;
    let dec_end = ((max_dec + dec_step) / dec_step).ceil() as i64;
    let ra_lo = c_ra + min_dra - dec_span * 0.25;
    let ra_hi = c_ra + max_dra + dec_span * 0.25;

    for k in (dec_start..=dec_end).take(32) {
        let dec = (k as f64 * dec_step).clamp(-89.95, 89.95);
        let mut pts = Vec::with_capacity(49);
        for s in 0..=48 {
            let ra = ra_lo + (ra_hi - ra_lo) * (s as f64 / 48.0);
            if let Some(pos) = to_screen(ra, dec) {
                pts.push(pos);
            }
        }
        if pts.len() >= 2 {
            painter.add(egui::Shape::line(pts.clone(), grid_stroke));
            if let Some(&label_pos) = pts.iter().find(|&&p| viewport.shrink(28.0).contains(p)) {
                let text = format_dec(dec);
                let galley = painter.layout_no_wrap(text, font.clone(), dec_label_col);
                let rect = Rect::from_min_size(
                    label_pos + egui::vec2(4.0, -galley.size().y - 2.0),
                    galley.size(),
                )
                .expand2(egui::vec2(3.0, 1.0));
                painter.rect_filled(rect, 2.0, label_bg);
                painter.galley(rect.min + egui::vec2(3.0, 1.0), galley, dec_label_col);
            }
        }
    }

    // 2. Constant Right Ascension curves
    let ra_min = c_ra + min_dra;
    let ra_max = c_ra + max_dra;
    let ra_start = ((ra_min - ra_step) / ra_step).floor() as i64;
    let ra_end = ((ra_max + ra_step) / ra_step).ceil() as i64;
    let dec_lo = (min_dec - dec_span * 0.2).clamp(-89.95, 89.95);
    let dec_hi = (max_dec + dec_span * 0.2).clamp(-89.95, 89.95);

    for k in (ra_start..=ra_end).take(32) {
        let ra = (k as f64 * ra_step).rem_euclid(360.0);
        let mut pts = Vec::with_capacity(49);
        for s in 0..=48 {
            let dec = dec_lo + (dec_hi - dec_lo) * (s as f64 / 48.0);
            if let Some(pos) = to_screen(ra, dec) {
                pts.push(pos);
            }
        }
        if pts.len() >= 2 {
            painter.add(egui::Shape::line(pts.clone(), grid_stroke));
            if let Some(&label_pos) = pts
                .iter()
                .rev()
                .find(|&&p| viewport.shrink(32.0).contains(p))
            {
                let text = format_ra(ra);
                let galley = painter.layout_no_wrap(text, font.clone(), ra_label_col);
                let rect = Rect::from_min_size(label_pos + egui::vec2(4.0, 2.0), galley.size())
                    .expand2(egui::vec2(3.0, 1.0));
                painter.rect_filled(rect, 2.0, label_bg);
                painter.galley(rect.min + egui::vec2(3.0, 1.0), galley, ra_label_col);
            }
        }
    }
}

/// Renders DSO target reticles/labels (`deep_sky.csv`) and Gaia DR3 star annotations.
fn draw_sky_annotations(
    painter: &egui::Painter,
    viewport: Rect,
    image_origin: Pos2,
    points_per_pixel: f32,
    plate: &PlateSolveReport,
    overlays: SkyOverlayConfig,
) {
    let font_dso = egui::FontId::proportional(11.5);
    let font_star = egui::FontId::monospace(9.5);
    let label_bg = Color32::from_rgba_unmultiplied(12, 36, 36, 215);

    if overlays.show_star_annotations {
        let star_col = Color32::from_rgba_unmultiplied(115, 220, 255, 195);
        for (idx, s) in plate.field_stars.iter().enumerate() {
            let pos = image_origin + egui::vec2(s.x, s.y) * points_per_pixel;
            if !viewport.contains(pos) {
                continue;
            }
            let r = (4.5 * points_per_pixel.sqrt()).clamp(2.5, 9.0);
            painter.circle_stroke(pos, r, Stroke::new(1.1_f32, star_col));
            if idx < 35 {
                let text = format!("G{:.1}", s.g_mag);
                painter.text(
                    pos + egui::vec2(r + 2.5, -r),
                    egui::Align2::LEFT_CENTER,
                    text,
                    font_star.clone(),
                    Color32::from_rgba_unmultiplied(165, 230, 255, 210),
                );
            }
        }
    }

    if overlays.show_dso_annotations {
        let dso_col = Color32::from_rgb(255, 210, 80);
        for dso in &plate.field_dsos {
            let pos = image_origin + egui::vec2(dso.x, dso.y) * points_per_pixel;
            if !viewport.expand(40.0).contains(pos) {
                continue;
            }
            let r = (14.0 * points_per_pixel.sqrt()).clamp(8.0, 28.0);
            painter.circle_stroke(pos, r, Stroke::new(1.5_f32, dso_col));
            let gap = 3.0_f32;
            let tick = 6.0_f32;
            for (dx, dy) in [(-1.0_f32, 0.0_f32), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)] {
                painter.line_segment(
                    [
                        pos + egui::vec2(dx * (r + gap), dy * (r + gap)),
                        pos + egui::vec2(dx * (r + gap + tick), dy * (r + gap + tick)),
                    ],
                    Stroke::new(1.4_f32, dso_col),
                );
            }

            let label = match &dso.common_name {
                Some(alias) if !alias.eq_ignore_ascii_case(&dso.name) => {
                    format!("{alias} ({})", dso.name)
                }
                _ => dso.name.clone(),
            };
            let galley = painter.layout_no_wrap(label, font_dso.clone(), dso_col);
            let rect = Rect::from_min_size(
                pos + egui::vec2(r + 6.0, -galley.size().y * 0.5),
                galley.size(),
            )
            .expand2(egui::vec2(4.0, 2.0));
            painter.rect_filled(rect, 3.0, label_bg);
            painter.rect_stroke(
                rect,
                3.0,
                Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 210, 80, 140)),
                egui::StrokeKind::Middle,
            );
            painter.galley(rect.min + egui::vec2(4.0, 2.0), galley, dso_col);
        }
    }
}

/// Renders the astronomical N / E orientation compass rose in the top-right corner of the viewport.
fn draw_compass_rose(
    painter: &egui::Painter,
    viewport: Rect,
    wcs: &fulgur_engine::sky::solve::Wcs,
) {
    let radius = 34.0_f32;
    let margin = 14.0_f32;
    let center = egui::pos2(
        viewport.right() - radius - margin,
        viewport.top() + radius + margin,
    );
    if !viewport.contains(center) {
        return;
    }

    painter.circle_filled(
        center,
        radius + 6.0,
        Color32::from_rgba_unmultiplied(12, 36, 36, 220),
    );
    painter.circle_stroke(
        center,
        radius + 6.0,
        Stroke::new(1.1_f32, Color32::from_rgb(55, 120, 120)),
    );

    let (c_ra, c_dec) = wcs.centre_sky();
    let Some(p0) = wcs.sky_to_pixel(c_ra, c_dec) else {
        return;
    };
    let step_deg = (wcs.scale() * 25.0 / 3600.0).clamp(1e-4, 0.2);
    let cos_dec = c_dec.to_radians().cos().abs().max(0.05);
    let Some(p_n) = wcs.sky_to_pixel(c_ra, (c_dec + step_deg).min(89.999)) else {
        return;
    };
    let Some(p_e) = wcs.sky_to_pixel((c_ra + step_deg / cos_dec).rem_euclid(360.0), c_dec) else {
        return;
    };

    let vn = egui::vec2((p_n[0] - p0[0]) as f32, (p_n[1] - p0[1]) as f32).normalized();
    let ve = egui::vec2((p_e[0] - p0[0]) as f32, (p_e[1] - p0[1]) as f32).normalized();

    let arm = radius - 8.0;
    let n_tip = center + vn * arm;
    let e_tip = center + ve * (arm * 0.85);

    let n_col = Color32::from_rgb(255, 115, 95);
    let e_col = Color32::from_rgb(95, 220, 255);

    // Draw East vector + arrowhead + "E"
    painter.line_segment([center, e_tip], Stroke::new(2.0_f32, e_col));
    let e_perp = egui::vec2(-ve.y, ve.x);
    painter.add(egui::Shape::convex_polygon(
        vec![
            e_tip,
            e_tip - ve * 6.5 + e_perp * 3.2,
            e_tip - ve * 6.5 - e_perp * 3.2,
        ],
        e_col,
        Stroke::NONE,
    ));
    painter.text(
        center + ve * (arm + 8.0),
        egui::Align2::CENTER_CENTER,
        "E",
        egui::FontId::proportional(11.5),
        e_col,
    );

    // Draw North vector + arrowhead + "N"
    painter.line_segment([center, n_tip], Stroke::new(2.2_f32, n_col));
    let n_perp = egui::vec2(-vn.y, vn.x);
    painter.add(egui::Shape::convex_polygon(
        vec![
            n_tip,
            n_tip - vn * 7.5 + n_perp * 3.6,
            n_tip - vn * 7.5 - n_perp * 3.6,
        ],
        n_col,
        Stroke::NONE,
    ));
    painter.text(
        center + vn * (arm + 8.5),
        egui::Align2::CENTER_CENTER,
        "N",
        egui::FontId::proportional(12.0),
        n_col,
    );
    painter.circle_filled(center, 2.5, Color32::from_rgb(230, 240, 240));
}

/// Pixel `x`, `y` and sky `(RA, Dec)` (when plate-solved) under the cursor at right-click.
type ContextTarget = (usize, usize, Option<(f64, f64)>);

/// Scroll distance (points) that counts as one wheel notch in browse mode.
const WHEEL_NOTCH: f32 = 40.0;

/// Accumulate raw wheel scroll and return whole frame steps: scrolling down (negative delta)
/// moves to the next frame, up to the previous one.
pub fn wheel_steps(accum: &mut f32, raw_delta_y: f32) -> i32 {
    *accum += raw_delta_y;
    let notches = (*accum / WHEEL_NOTCH).trunc();
    *accum -= notches * WHEEL_NOTCH;
    -(notches as i32)
}

/// Identity of the drawn map (kind + fitted surface + range), used to rebuild the colour map
/// texture only when it changes.
fn map_texture_key(kind: Option<MapKind>, view: &crate::field_map::MapView<'_>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    kind.hash(&mut h);
    for c in view.surface.coeffs {
        c.to_bits().hash(&mut h);
    }
    view.width.hash(&mut h);
    view.height.hash(&mut h);
    view.min.to_bits().hash(&mut h);
    view.max.to_bits().hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_notches_become_frame_steps() {
        let mut acc = 0.0;
        assert_eq!(wheel_steps(&mut acc, -50.0), 1, "scroll down = next frame");
        assert_eq!(
            wheel_steps(&mut acc, 50.0),
            -1,
            "scroll up = previous frame"
        );
        // Small touchpad deltas accumulate into one step.
        let mut acc = 0.0;
        let mut total = 0;
        for _ in 0..8 {
            total += wheel_steps(&mut acc, -6.0);
        }
        assert_eq!(total, 1);
        assert_eq!(wheel_steps(&mut 0.0, -130.0), 3);
    }

    #[test]
    fn show_all_and_hide_all_switch_every_overlay() {
        let mut overlays = SkyOverlayConfig::default();
        overlays.set_all(true);
        assert!(overlays.all_visible() && !overlays.none_visible());
        overlays.set_all(false);
        assert!(overlays.none_visible() && !overlays.all_visible());
        overlays.show_compass = true;
        assert!(!overlays.all_visible() && !overlays.none_visible());
    }
}
