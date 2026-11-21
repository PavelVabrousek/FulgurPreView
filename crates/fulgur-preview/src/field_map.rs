//! Field map overlays: a colour map of a fitted surface over the image, with labelled
//! contours and a summary box.
//!
//! Two maps are available:
//! - **FWHM** (field curvature / tilt / collimation): also marks the sharpest point, the frame
//!   centre and a tilt arrow.
//! - **Eccentricity** (star elongation across the field).

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Shape, Stroke, Vec2};
use fulgur_engine::field::{EccentricityMap, FieldAnalysis, FwhmSurface, format_length};

/// Long side of the colour-map texture (the surface is smooth, so this is plenty).
const MAP_TEXELS: usize = 160;
/// Opacity of the colour map over the image.
const MAP_ALPHA: u8 = 150;
/// Grid used for the contour lines.
const CONTOUR_GRID: usize = 48;
/// Number of contour levels between min and max.
const CONTOUR_LEVELS: usize = 4;
/// Room below the summary box for the one-line colour legend.
const LEGEND_SPACE: f32 = 22.0;
/// Keep contour labels this far (points) from the edge of the visible image.
const LABEL_MARGIN: f32 = 36.0;

/// Which field map is drawn over the image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MapKind {
    /// FWHM: field curvature, tilt and collimation (key C).
    Fwhm,
    /// Star eccentricity / elongation (key E).
    Eccentricity,
}

impl MapKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Fwhm => "FWHM",
            Self::Eccentricity => "Eccentricity",
        }
    }
}

/// A fitted surface with the range used for colours and contours.
#[derive(Clone, Copy)]
pub struct MapView<'a> {
    pub surface: &'a FwhmSurface,
    pub width: usize,
    pub height: usize,
    pub min: f64,
    pub max: f64,
}

impl<'a> MapView<'a> {
    pub fn fwhm(field: &'a FieldAnalysis) -> Self {
        Self {
            surface: &field.surface,
            width: field.width,
            height: field.height,
            min: field.min_fwhm_px,
            max: field.max_fwhm_px,
        }
    }

    pub fn eccentricity(map: &'a EccentricityMap) -> Self {
        Self {
            surface: &map.surface,
            width: map.width,
            height: map.height,
            min: map.min,
            max: map.max,
        }
    }

    fn normalized(&self, value: f64) -> f32 {
        ((value - self.min) / (self.max - self.min).max(1e-9)) as f32
    }
}

/// Colour for a normalized value `t` (0 = best, 1 = worst): deep blue -> teal -> green ->
/// salmon.
pub fn map_color(t: f32) -> Color32 {
    const STOPS: [(f32, [f32; 3]); 4] = [
        (0.0, [28.0, 36.0, 130.0]),
        (0.4, [54.0, 140.0, 160.0]),
        (0.7, [120.0, 196.0, 140.0]),
        (1.0, [240.0, 140.0, 120.0]),
    ];
    let t = t.clamp(0.0, 1.0);
    let k = STOPS
        .windows(2)
        .position(|w| t <= w[1].0)
        .unwrap_or(STOPS.len() - 2);
    let (t0, c0) = STOPS[k];
    let (t1, c1) = STOPS[k + 1];
    let f = ((t - t0) / (t1 - t0).max(1e-6)).clamp(0.0, 1.0);
    let mix = |i: usize| (c0[i] + (c1[i] - c0[i]) * f).round() as u8;
    Color32::from_rgb(mix(0), mix(1), mix(2))
}

/// Colour-map image of the fitted surface, same aspect ratio as the frame.
pub fn map_color_image(view: MapView<'_>) -> egui::ColorImage {
    let (w, h) = (view.width.max(1) as f64, view.height.max(1) as f64);
    let (tw, th) = if w >= h {
        (
            MAP_TEXELS,
            ((MAP_TEXELS as f64 * h / w).round() as usize).max(2),
        )
    } else {
        (
            ((MAP_TEXELS as f64 * w / h).round() as usize).max(2),
            MAP_TEXELS,
        )
    };
    let mut pixels = Vec::with_capacity(tw * th);
    for j in 0..th {
        let y = (j as f64 + 0.5) * h / th as f64;
        for i in 0..tw {
            let x = (i as f64 + 0.5) * w / tw as f64;
            let c = map_color(view.normalized(view.surface.eval_px(x, y)));
            pixels.push(Color32::from_rgba_unmultiplied(
                c.r(),
                c.g(),
                c.b(),
                MAP_ALPHA,
            ));
        }
    }
    egui::ColorImage {
        size: [tw, th],
        pixels,
    }
}

/// Contour segments (in native sensor pixels) of the surface at `level`, by marching squares.
pub fn contour_segments(view: MapView<'_>, level: f64) -> Vec<[[f64; 2]; 2]> {
    let (w, h) = (view.width as f64, view.height as f64);
    let nx = CONTOUR_GRID;
    let ny = ((CONTOUR_GRID as f64 * h / w.max(1.0)).round() as usize).max(2);
    let px = |i: usize| i as f64 * w / nx as f64;
    let py = |j: usize| j as f64 * h / ny as f64;
    let val = |i: usize, j: usize| view.surface.eval_px(px(i), py(j)) - level;
    let mut out = Vec::new();
    for j in 0..ny {
        for i in 0..nx {
            let corners = [
                ([px(i), py(j)], val(i, j)),
                ([px(i + 1), py(j)], val(i + 1, j)),
                ([px(i + 1), py(j + 1)], val(i + 1, j + 1)),
                ([px(i), py(j + 1)], val(i, j + 1)),
            ];
            let mut crossings = Vec::with_capacity(4);
            for e in 0..4 {
                let (pa, va) = corners[e];
                let (pb, vb) = corners[(e + 1) % 4];
                if (va < 0.0) != (vb < 0.0) {
                    let t = va / (va - vb);
                    crossings.push([pa[0] + t * (pb[0] - pa[0]), pa[1] + t * (pb[1] - pa[1])]);
                }
            }
            for pair in crossings.chunks_exact(2) {
                out.push([pair[0], pair[1]]);
            }
        }
    }
    out
}

/// Values of the contour lines: evenly spaced between min and max.
pub fn contour_levels(view: MapView<'_>) -> Vec<f64> {
    (1..=CONTOUR_LEVELS)
        .map(|k| view.min + (view.max - view.min) * k as f64 / (CONTOUR_LEVELS + 1) as f64)
        .collect()
}

/// Where to put a contour's value label: the midpoint of the contour segment that lies
/// deepest inside `area` (so the label is on screen and away from the edges). `None` if no
/// part of the contour is inside `area`.
pub fn label_anchor(segments: &[[Pos2; 2]], area: Rect) -> Option<Pos2> {
    if !area.is_positive() {
        return None;
    }
    segments
        .iter()
        .map(|seg| seg[0].lerp(seg[1], 0.5))
        .filter(|p| area.contains(*p))
        .map(|p| {
            let depth = (p.x - area.min.x)
                .min(area.max.x - p.x)
                .min(p.y - area.min.y)
                .min(area.max.y - p.y);
            (depth, p)
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, p)| p)
}

/// The map to draw, with everything it needs for labels and the summary box.
pub struct MapOverlay<'a> {
    pub view: MapView<'a>,
    /// Contour label text for a surface value.
    pub label: Box<dyn Fn(f64) -> String + 'a>,
    pub summary: String,
    pub legend: &'static str,
    /// FWHM map only: sharpest point, centre ring and tilt arrow.
    pub fwhm_markers: Option<&'a FieldAnalysis>,
    /// Eccentricity map only: elongation direction whiskers per grid cell.
    pub whiskers: Option<&'a EccentricityMap>,
}

impl<'a> MapOverlay<'a> {
    /// FWHM map; lengths in arcseconds when `arcsec_per_px` is given, otherwise pixels.
    pub fn fwhm(field: &'a FieldAnalysis, arcsec_per_px: Option<f64>) -> Self {
        Self {
            view: MapView::fwhm(field),
            label: Box::new(move |v| format_length(v, arcsec_per_px)),
            summary: field.summary_text_in(arcsec_per_px),
            legend: "FWHM map: blue = sharp, red = soft",
            fwhm_markers: Some(field),
            whiskers: None,
        }
    }

    pub fn eccentricity(map: &'a EccentricityMap) -> Self {
        Self {
            view: MapView::eccentricity(map),
            label: Box::new(|v| format!("e {v:.2}")),
            summary: map.summary_text(),
            legend: "Eccentricity map: blue = round, red = elongated; lines = elongation direction",
            fwhm_markers: None,
            whiskers: Some(map),
        }
    }
}

/// Draw the overlay. `texture` is the colour map built by [`map_color_image`].
pub fn draw_map(
    painter: &egui::Painter,
    viewport: Rect,
    image_rect: Rect,
    overlay: &MapOverlay<'_>,
    texture: &egui::TextureHandle,
) {
    let view = overlay.view;
    painter.image(
        texture.id(),
        image_rect,
        Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );
    let scale = Vec2::new(
        image_rect.width() / view.width.max(1) as f32,
        image_rect.height() / view.height.max(1) as f32,
    );
    let to_screen = |p: [f64; 2]| image_rect.min + Vec2::new(p[0] as f32, p[1] as f32) * scale;

    // Contours, each labelled with its value
    let contour = Stroke::new(1.0_f32, Color32::from_white_alpha(110));
    let label_area = image_rect.intersect(viewport).shrink(LABEL_MARGIN);
    for level in contour_levels(view) {
        let segments: Vec<[Pos2; 2]> = contour_segments(view, level)
            .into_iter()
            .map(|seg| [to_screen(seg[0]), to_screen(seg[1])])
            .collect();
        for seg in &segments {
            painter.line_segment(*seg, contour);
        }
        if let Some(anchor) = label_anchor(&segments, label_area) {
            let galley = painter.layout_no_wrap(
                (overlay.label)(level),
                FontId::monospace(11.0),
                Color32::WHITE,
            );
            let bg = Rect::from_center_size(anchor, galley.size() + Vec2::new(8.0, 2.0));
            painter.rect_filled(bg, 3.0, Color32::from_black_alpha(160));
            painter.galley(bg.min + Vec2::new(4.0, 1.0), galley, Color32::WHITE);
        }
    }

    if let Some(field) = overlay.fwhm_markers {
        draw_fwhm_markers(painter, image_rect, field, &to_screen);
    }
    if let Some(map) = overlay.whiskers {
        draw_whiskers(painter, viewport, image_rect, map, &to_screen);
    }

    // Summary box in the bottom-left corner of the visible viewport: the hovered-star panel
    // uses the top-left corner and the N/E compass the top-right one.
    let font = FontId::monospace(12.0);
    let galley = painter.layout_no_wrap(overlay.summary.clone(), font, Color32::WHITE);
    let box_size = galley.size() + Vec2::new(16.0, 12.0);
    let origin = Pos2::new(
        viewport.min.x + 10.0,
        viewport.max.y - box_size.y - LEGEND_SPACE - 10.0,
    );
    let bg = Rect::from_min_size(origin, box_size);
    painter.rect_filled(bg, 4.0, Color32::from_black_alpha(170));
    painter.galley(origin + Vec2::new(8.0, 6.0), galley, Color32::WHITE);
    painter.text(
        Pos2::new(bg.min.x + 8.0, bg.max.y + 6.0),
        Align2::LEFT_TOP,
        overlay.legend,
        FontId::proportional(11.0),
        Color32::from_white_alpha(200),
    );
}

/// Screen segment of one whisker: centred on the cell, along the cell's mean long axis
/// (`angle_deg` counter-clockwise on screen), `length` points long.
pub fn whisker_segment(centre: Pos2, angle_deg: f64, length: f32) -> [Pos2; 2] {
    let a = (angle_deg as f32).to_radians();
    let half = Vec2::new(a.cos(), -a.sin()) * (length * 0.5);
    [centre - half, centre + half]
}

/// Elongation whiskers: one line per grid cell; the longest line spans most of a cell, the
/// others scale with their mean elongation.
fn draw_whiskers(
    painter: &egui::Painter,
    viewport: Rect,
    image_rect: Rect,
    map: &EccentricityMap,
    to_screen: &impl Fn([f64; 2]) -> Pos2,
) {
    let longest = map
        .whiskers
        .iter()
        .map(|w| w.elongation)
        .fold(0.0_f64, f64::max);
    if longest <= 1e-9 {
        return;
    }
    let cell = image_rect.width() / map.whisker_cols.max(1) as f32;
    let stroke = Stroke::new(2.0_f32, Color32::from_rgb(255, 236, 160));
    for w in &map.whiskers {
        let centre = to_screen([w.x_px, w.y_px]);
        if !viewport.contains(centre) {
            continue;
        }
        let length = cell * 0.85 * (w.elongation / longest) as f32;
        painter.line_segment(whisker_segment(centre, w.angle_deg, length), stroke);
    }
}

/// Frame centre ring, sharpest-point crosshair and tilt arrow of the FWHM map.
fn draw_fwhm_markers(
    painter: &egui::Painter,
    image_rect: Rect,
    field: &FieldAnalysis,
    to_screen: &impl Fn([f64; 2]) -> Pos2,
) {
    let centre = to_screen(field.surface.center_px);
    let marker = Stroke::new(1.5_f32, Color32::from_white_alpha(220));
    painter.circle_stroke(centre, 6.0, marker);
    let sharp = to_screen(field.sharpest_px);
    let arm = (image_rect.width().min(image_rect.height()) * 0.18).clamp(20.0, 160.0);
    painter.line_segment(
        [sharp - Vec2::new(arm, 0.0), sharp + Vec2::new(arm, 0.0)],
        marker,
    );
    painter.line_segment(
        [sharp - Vec2::new(0.0, arm), sharp + Vec2::new(0.0, arm)],
        marker,
    );

    // Tilt arrow from the centre towards the softer side
    if field.total_tilt_pct >= 1.0 {
        let angle = (field.tilt_angle_deg as f32).to_radians();
        let dir = Vec2::new(angle.cos(), -angle.sin());
        let tip = centre + dir * arm * 0.9;
        let arrow = Stroke::new(2.0_f32, Color32::from_rgb(255, 210, 120));
        painter.line_segment([centre, tip], arrow);
        let back = -dir * 10.0;
        let side = Vec2::new(-dir.y, dir.x) * 5.0;
        painter.add(Shape::convex_polygon(
            vec![tip, tip + back + side, tip + back - side],
            Color32::from_rgb(255, 210, 120),
            Stroke::NONE,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fulgur_engine::stars::{RADIAL_PROFILE_BINS, Star};

    fn bowl_stars() -> Vec<Star> {
        let mut stars = Vec::new();
        for j in 0..10 {
            for i in 0..14 {
                let x = (i as f64 + 0.5) * 2800.0 / 14.0;
                let y = (j as f64 + 0.5) * 2000.0 / 10.0;
                let f = 2.0 + ((x - 1400.0).powi(2) + (y - 1000.0).powi(2)) / 3.0e6;
                stars.push(Star {
                    x_px: x,
                    y_px: y,
                    hfr_px: f / 2.0,
                    moment_fwhm_px: f,
                    eccentricity: 0.1 + 0.4 * (x / 2800.0),
                    elongation_angle_deg: 0.0,
                    flux_adu: 1.0,
                    peak_adu: 1.0,
                    background_adu: 0.0,
                    snr: 10.0,
                    radial_profile: [0.0; RADIAL_PROFILE_BINS],
                });
            }
        }
        stars
    }

    fn bowl() -> FieldAnalysis {
        FieldAnalysis::from_stars(&bowl_stars(), 2800, 2000, None).unwrap()
    }

    #[test]
    fn color_map_has_frame_aspect_and_sharp_centre() {
        let field = bowl();
        let img = map_color_image(MapView::fwhm(&field));
        assert_eq!(img.size[0], MAP_TEXELS);
        assert_eq!(img.size[1], 114); // 160 * 2000 / 2800
        let centre = img.pixels[(img.size[1] / 2) * img.size[0] + img.size[0] / 2];
        let corner = img.pixels[0];
        // Sharp centre is blue-dominant, soft corner is red-dominant.
        assert!(centre.b() > centre.r());
        assert!(corner.r() > corner.b());
    }

    #[test]
    fn eccentricity_map_is_red_on_the_elongated_side() {
        let map = EccentricityMap::from_stars(&bowl_stars(), 2800, 2000).unwrap();
        let img = map_color_image(MapView::eccentricity(&map));
        let row = img.size[1] / 2;
        let left = img.pixels[row * img.size[0] + 2];
        let right = img.pixels[row * img.size[0] + img.size[0] - 3];
        assert!(left.b() > left.r(), "round side is blue");
        assert!(right.r() > right.b(), "elongated side is red");
        let overlay = MapOverlay::eccentricity(&map);
        assert_eq!((overlay.label)(0.456), "e 0.46");
        assert!(overlay.fwhm_markers.is_none());
        assert!(overlay.whiskers.is_some());
    }

    #[test]
    fn whisker_points_along_the_screen_angle() {
        let c = Pos2::new(100.0, 100.0);
        let [a, b] = whisker_segment(c, 0.0, 20.0);
        assert!((a.x - 90.0).abs() < 1e-4 && (b.x - 110.0).abs() < 1e-4 && a.y == b.y);
        // 90 degrees is vertical on screen.
        let [a, b] = whisker_segment(c, 90.0, 20.0);
        assert!((a.x - b.x).abs() < 1e-4 && (a.y - b.y).abs() > 19.9);
        // 45 degrees: up-right on screen (y decreases as x increases).
        let [a, b] = whisker_segment(c, 45.0, 20.0);
        assert!(b.x > a.x && b.y < a.y);
    }

    #[test]
    fn fwhm_labels_follow_the_unit_choice() {
        let field = bowl();
        assert_eq!((MapOverlay::fwhm(&field, None).label)(2.5), "2.50 px");
        assert_eq!(
            (MapOverlay::fwhm(&field, Some(2.0)).label)(2.5),
            "5.00\u{2033}"
        );
        assert!(
            MapOverlay::fwhm(&field, Some(2.0))
                .summary
                .contains("Min FWHM:     4.00\u{2033}")
        );
    }

    #[test]
    fn every_contour_gets_a_label_on_the_line_inside_the_view() {
        let field = bowl();
        let view = MapView::fwhm(&field);
        // Frame drawn 1:10 at the origin, fully visible.
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(280.0, 200.0)).shrink(LABEL_MARGIN);
        let levels = contour_levels(view);
        assert_eq!(levels.len(), CONTOUR_LEVELS);
        let mut labelled = 0;
        for level in levels {
            let segs: Vec<[Pos2; 2]> = contour_segments(view, level)
                .into_iter()
                .map(|s| {
                    [
                        Pos2::new(s[0][0] as f32 / 10.0, s[0][1] as f32 / 10.0),
                        Pos2::new(s[1][0] as f32 / 10.0, s[1][1] as f32 / 10.0),
                    ]
                })
                .collect();
            if let Some(p) = label_anchor(&segs, area) {
                labelled += 1;
                assert!(area.contains(p));
                // The anchor lies on the contour: the surface there equals the level.
                let f = field.surface.eval_px(p.x as f64 * 10.0, p.y as f64 * 10.0);
                assert!((f - level).abs() < 0.02 * level, "{f} vs {level}");
            }
        }
        assert!(labelled >= 2, "inner contours must be labelled");
        assert!(label_anchor(&[], area).is_none());
    }

    #[test]
    fn contours_of_a_bowl_surround_the_centre() {
        let field = bowl();
        let level = (field.min_fwhm_px + field.max_fwhm_px) / 2.0;
        let segs = contour_segments(MapView::fwhm(&field), level);
        assert!(segs.len() > 20);
        for seg in segs {
            for p in seg {
                let d = (p[0] - 1400.0).hypot(p[1] - 1000.0);
                assert!(d > 200.0, "contour too close to the sharp centre: {d}");
            }
        }
    }
}
