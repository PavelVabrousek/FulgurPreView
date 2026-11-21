//! Field curvature, sensor tilt and collimation analysis from per-star FWHM.
//!
//! The FWHM of every measured star is fitted with a smooth quadratic surface over the frame,
//!
//! ```text
//! FWHM(u, v) = c0 + c1*u + c2*v + c3*u^2 + c4*v^2 + c5*u*v
//! ```
//!
//! where `u`, `v` are pixel offsets from the frame centre divided by the half-diagonal (so
//! the corners sit at radius 1). The fit is robust: stars whose residual exceeds 2.5 sigma
//! (MAD-based) are dropped and the fit is repeated.
//!
//! From the surface:
//! - **Min / Max FWHM:** the best and worst fitted FWHM inside the frame.
//! - **Curvature:** mean fitted FWHM at the four corners relative to the sharpest point,
//!   in percent. Flat fields are near 0 %, strong field curvature gives large values.
//! - **Tilt X / Y:** fitted FWHM difference between the right and left edge (X) and between
//!   the bottom and top edge (Y), from the linear terms. Positive X = right side softer,
//!   positive Y = bottom side softer.
//! - **Total tilt:** the edge-to-edge FWHM difference along the steepest direction relative
//!   to the sharpest FWHM (percent), and the direction towards the softer side (degrees,
//!   0 = right, 90 = up on screen).
//! - **Collimation:** distance of the sharpest point of the surface from the frame centre.
//!   With good collimation and no tilt the sharpest point sits in the centre.
//!
//! These definitions follow the usual meaning of the terms in focus-analysis tools; exact
//! numbers differ between tools because each defines them slightly differently.

use crate::stars::Star;
use serde::Serialize;

/// Minimum number of stars for a meaningful surface fit.
pub const MIN_FIELD_STARS: usize = 20;
/// Sigma-clipping threshold for outlier stars (in MAD-sigma units).
const CLIP_SIGMA: f64 = 2.5;
/// Fit / clip iterations.
const CLIP_ITERATIONS: usize = 3;
/// Grid used to find the fitted minimum / maximum inside the frame.
const SEARCH_GRID: usize = 64;

/// Quadratic FWHM surface in normalized frame coordinates.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct FwhmSurface {
    /// `c0 + c1*u + c2*v + c3*u^2 + c4*v^2 + c5*u*v`
    pub coeffs: [f64; 6],
    pub center_px: [f64; 2],
    /// Pixels per normalized unit (half-diagonal).
    pub scale_px: f64,
}

impl FwhmSurface {
    /// Fitted FWHM (px) at sensor pixel `(x, y)`.
    pub fn eval_px(&self, x: f64, y: f64) -> f64 {
        let u = (x - self.center_px[0]) / self.scale_px;
        let v = (y - self.center_px[1]) / self.scale_px;
        self.eval_uv(u, v)
    }

    fn eval_uv(&self, u: f64, v: f64) -> f64 {
        let c = &self.coeffs;
        c[0] + c[1] * u + c[2] * v + c[3] * u * u + c[4] * v * v + c[5] * u * v
    }
}

/// Field curvature / tilt / collimation summary for one frame.
#[derive(Clone, Debug, Serialize)]
pub struct FieldAnalysis {
    pub surface: FwhmSurface,
    pub width: usize,
    pub height: usize,
    pub stars_used: usize,
    pub min_fwhm_px: f64,
    pub max_fwhm_px: f64,
    /// Corners vs sharpest point, percent.
    pub curvature_pct: f64,
    /// Right edge minus left edge fitted FWHM (px). Positive: right side softer.
    pub tilt_x_px: f64,
    /// Bottom edge minus top edge fitted FWHM (px). Positive: bottom side softer.
    pub tilt_y_px: f64,
    /// Steepest edge-to-edge FWHM difference relative to the sharpest FWHM, percent.
    pub total_tilt_pct: f64,
    /// Direction towards the softer side: degrees, 0 = right, 90 = up on screen.
    pub tilt_angle_deg: f64,
    /// Sharpest point of the fitted surface (sensor px), clamped to the frame.
    pub sharpest_px: [f64; 2],
    /// `false` when the surface has no interior minimum (saddle or dome-shaped field); the
    /// sharpest point is then the best grid point and collimation is less meaningful.
    pub sharpest_is_vertex: bool,
    /// Distance of the sharpest point from the frame centre (px).
    pub collimation_offset_px: f64,
    /// The same distance in percent of the half-diagonal.
    pub collimation_offset_pct: f64,
    /// Arcseconds per pixel, when known (for display in ″).
    pub pixel_scale_arcsec: Option<f64>,
}

impl FieldAnalysis {
    /// Fit the field from measured stars. `None` when fewer than [`MIN_FIELD_STARS`] stars
    /// have a valid FWHM.
    pub fn from_stars(
        stars: &[Star],
        width: usize,
        height: usize,
        pixel_scale_arcsec: Option<f64>,
    ) -> Option<Self> {
        let (surface, stars_used) =
            fit_surface(stars, width, height, |s| s.moment_fwhm_px, |v| v > 0.0)?;
        let coeffs = surface.coeffs;
        let center = surface.center_px;
        let scale = surface.scale_px;
        let half_u = center[0] / scale;
        let half_v = center[1] / scale;

        // Min / max over the frame on a grid.
        let mut min_f = f64::INFINITY;
        let mut max_f = f64::NEG_INFINITY;
        let mut min_uv = (0.0, 0.0);
        for j in 0..=SEARCH_GRID {
            let v = -half_v + 2.0 * half_v * j as f64 / SEARCH_GRID as f64;
            for i in 0..=SEARCH_GRID {
                let u = -half_u + 2.0 * half_u * i as f64 / SEARCH_GRID as f64;
                let f = eval(&coeffs, u, v);
                if f < min_f {
                    min_f = f;
                    min_uv = (u, v);
                }
                max_f = max_f.max(f);
            }
        }

        // Interior minimum of the paraboloid, when it has one.
        let (a, b, d) = (2.0 * coeffs[3], coeffs[5], 2.0 * coeffs[4]);
        let det = a * d - b * b;
        let mut sharpest_is_vertex = false;
        let mut sharp_uv = min_uv;
        if det > 1e-12 && a > 0.0 {
            let u0 = (-coeffs[1] * d + coeffs[2] * b) / det;
            let v0 = (coeffs[1] * b - coeffs[2] * a) / det;
            if u0.abs() <= half_u && v0.abs() <= half_v {
                sharp_uv = (u0, v0);
                sharpest_is_vertex = true;
                min_f = min_f.min(eval(&coeffs, u0, v0));
            }
        }
        let min_f = min_f.max(1e-6);

        let corners = [
            (-half_u, -half_v),
            (half_u, -half_v),
            (-half_u, half_v),
            (half_u, half_v),
        ];
        let corner_mean = corners
            .iter()
            .map(|&(u, v)| eval(&coeffs, u, v))
            .sum::<f64>()
            / 4.0;
        let curvature_pct = (corner_mean - min_f) / min_f * 100.0;

        // Linear (tilt) terms across the full frame width / height.
        let tilt_x_px = coeffs[1] * 2.0 * half_u;
        let tilt_y_px = coeffs[2] * 2.0 * half_v;
        let total = tilt_x_px.hypot(tilt_y_px);
        let total_tilt_pct = total / min_f * 100.0;
        // Screen "up" is -y in sensor coordinates.
        let tilt_angle_deg = if total > 1e-12 {
            (-tilt_y_px).atan2(tilt_x_px).to_degrees().rem_euclid(360.0)
        } else {
            0.0
        };

        let sharpest_px = [
            center[0] + sharp_uv.0 * scale,
            center[1] + sharp_uv.1 * scale,
        ];
        let collimation_offset_px = (sharpest_px[0] - center[0]).hypot(sharpest_px[1] - center[1]);

        Some(Self {
            surface,
            width,
            height,
            stars_used,
            min_fwhm_px: min_f,
            max_fwhm_px: max_f.max(min_f),
            curvature_pct,
            tilt_x_px,
            tilt_y_px,
            total_tilt_pct,
            tilt_angle_deg,
            sharpest_px,
            sharpest_is_vertex,
            collimation_offset_px,
            collimation_offset_pct: collimation_offset_px / scale * 100.0,
            pixel_scale_arcsec,
        })
    }

    /// Format a FWHM value in arcseconds when the scale is known, otherwise in pixels.
    pub fn format_fwhm(&self, px: f64) -> String {
        format_length(px, self.pixel_scale_arcsec)
    }

    /// Plain-text summary (for the clipboard / logs), in arcseconds when the frame's scale is
    /// known.
    pub fn summary_text(&self) -> String {
        self.summary_text_in(self.pixel_scale_arcsec)
    }

    /// Plain-text summary with lengths in arcseconds when `arcsec_per_px` is given, otherwise
    /// in pixels (the viewer's px / ″ toggle).
    pub fn summary_text_in(&self, arcsec_per_px: Option<f64>) -> String {
        let len = |px: f64| format_length(px, arcsec_per_px);
        let signed = |px: f64| {
            let text = len(px.abs());
            if px < 0.0 {
                format!("-{text}")
            } else {
                format!("+{text}")
            }
        };
        format!(
            "Min FWHM:     {}\nMax FWHM:     {}\nCurvature:    {:.1}%\nTilt in X:    {}\nTilt in Y:    {}\nTotal Tilt:   {:.0}% @{:.0}\u{b0}\nCollimation:  {:.0} px ({:.1}% of half-diagonal){}\nStars Used:   {}\n",
            len(self.min_fwhm_px),
            len(self.max_fwhm_px),
            self.curvature_pct,
            signed(self.tilt_x_px),
            signed(self.tilt_y_px),
            self.total_tilt_pct,
            self.tilt_angle_deg,
            self.collimation_offset_px,
            self.collimation_offset_pct,
            if self.sharpest_is_vertex {
                ""
            } else {
                " [no clear sharpest point]"
            },
            self.stars_used,
        )
    }
}

/// Format a length in arcseconds when `arcsec_per_px` is given, otherwise in pixels.
pub fn format_length(px: f64, arcsec_per_px: Option<f64>) -> String {
    match arcsec_per_px {
        Some(s) => format!("{:.2}\u{2033}", px * s),
        None => format!("{px:.2} px"),
    }
}

/// Robust quadratic fit of `metric(star)` over the frame. Stars whose metric is not finite or
/// fails `valid` are skipped; outliers beyond `CLIP_SIGMA` are clipped iteratively. Returns the
/// surface and the number of stars kept, or `None` with fewer than [`MIN_FIELD_STARS`].
fn fit_surface(
    stars: &[Star],
    width: usize,
    height: usize,
    metric: impl Fn(&Star) -> f64,
    valid: impl Fn(f64) -> bool,
) -> Option<(FwhmSurface, usize)> {
    let center = [width as f64 / 2.0, height as f64 / 2.0];
    let scale = (center[0].hypot(center[1])).max(1.0);
    let mut points: Vec<(f64, f64, f64)> = stars
        .iter()
        .map(|s| (s, metric(s)))
        .filter(|&(_, m)| m.is_finite() && valid(m))
        .map(|(s, m)| {
            (
                (s.x_px - center[0]) / scale,
                (s.y_px - center[1]) / scale,
                m,
            )
        })
        .collect();
    if points.len() < MIN_FIELD_STARS {
        return None;
    }

    let mut coeffs = fit_quadratic(&points)?;
    for _ in 0..CLIP_ITERATIONS {
        let residuals: Vec<f64> = points
            .iter()
            .map(|&(u, v, f)| f - eval(&coeffs, u, v))
            .collect();
        let sigma = 1.4826 * median_abs(&residuals);
        if sigma <= 1e-9 {
            break;
        }
        let kept: Vec<(f64, f64, f64)> = points
            .iter()
            .zip(&residuals)
            .filter(|(_, r)| r.abs() <= CLIP_SIGMA * sigma)
            .map(|(p, _)| *p)
            .collect();
        if kept.len() == points.len() || kept.len() < MIN_FIELD_STARS {
            break;
        }
        points = kept;
        coeffs = fit_quadratic(&points)?;
    }
    Some((
        FwhmSurface {
            coeffs,
            center_px: center,
            scale_px: scale,
        },
        points.len(),
    ))
}

/// Fitted minimum and maximum of `surface` inside a `width` x `height` frame (grid search).
fn surface_range(surface: &FwhmSurface, width: usize, height: usize) -> (f64, f64) {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for j in 0..=SEARCH_GRID {
        let y = height as f64 * j as f64 / SEARCH_GRID as f64;
        for i in 0..=SEARCH_GRID {
            let x = width as f64 * i as f64 / SEARCH_GRID as f64;
            let v = surface.eval_px(x, y);
            lo = lo.min(v);
            hi = hi.max(v);
        }
    }
    (lo, hi)
}

/// Star elongation across the field: a smooth surface fitted to per-star eccentricity
/// (0 = round, 1 = a line). Elongated corners point to coma / tilt / spacing issues,
/// elongation everywhere to tracking or wind.
#[derive(Clone, Debug, Serialize)]
pub struct EccentricityMap {
    pub surface: FwhmSurface,
    pub width: usize,
    pub height: usize,
    pub stars_used: usize,
    pub min: f64,
    pub max: f64,
    /// Fitted value in the frame centre.
    pub centre: f64,
    /// Fitted values in the corners: top-left, top-right, bottom-left, bottom-right.
    pub corners: [f64; 4],
    /// Elongation direction per grid cell (whisker plot).
    pub whiskers: Vec<WhiskerCell>,
    /// Number of whisker grid columns (cells across the frame width).
    pub whisker_cols: usize,
    /// Eccentricity-weighted mean direction of elongation over the frame, degrees `[0, 180)`.
    pub mean_angle_deg: f64,
    /// How consistently stars point the same way: 1 = all parallel (typical of tracking,
    /// guiding or wind), near 0 = no common direction (radial patterns from optics, or round
    /// stars).
    pub direction_coherence: f64,
}

/// Mean elongation of the stars in one grid cell.
#[derive(Clone, Debug, Serialize)]
pub struct WhiskerCell {
    pub x_px: f64,
    pub y_px: f64,
    /// Mean long-axis direction, degrees `[0, 180)`, counter-clockwise on screen.
    pub angle_deg: f64,
    /// Length of the mean elongation vector (0..1): mean eccentricity reduced by how much the
    /// stars in the cell disagree on direction.
    pub elongation: f64,
    pub stars: usize,
}

/// Grid used for the whisker plot (columns on the long side of the frame).
const WHISKER_GRID: usize = 12;

/// Average directions as doubled-angle vectors (an axis at 10° and one at 190° are the same).
/// Returns `(angle_deg, length)` of the eccentricity-weighted mean.
fn mean_axis<'a>(stars: impl Iterator<Item = &'a Star>) -> Option<(f64, f64, usize)> {
    let (mut c, mut s, mut n) = (0.0, 0.0, 0usize);
    for star in stars {
        if !(star.eccentricity.is_finite() && star.elongation_angle_deg.is_finite()) {
            continue;
        }
        let a = (2.0 * star.elongation_angle_deg).to_radians();
        c += star.eccentricity * a.cos();
        s += star.eccentricity * a.sin();
        n += 1;
    }
    if n == 0 {
        return None;
    }
    let (c, s) = (c / n as f64, s / n as f64);
    let angle = (0.5 * s.atan2(c)).to_degrees().rem_euclid(180.0);
    Some((angle, c.hypot(s), n))
}

/// Whisker grid size `(columns, rows)` for a frame.
fn whisker_dims(width: usize, height: usize) -> (usize, usize) {
    let (w, h) = (width.max(1) as f64, height.max(1) as f64);
    if w >= h {
        (
            WHISKER_GRID,
            ((WHISKER_GRID as f64 * h / w).round() as usize).max(1),
        )
    } else {
        (
            ((WHISKER_GRID as f64 * w / h).round() as usize).max(1),
            WHISKER_GRID,
        )
    }
}

fn whisker_grid(stars: &[Star], width: usize, height: usize) -> Vec<WhiskerCell> {
    let (w, h) = (width.max(1) as f64, height.max(1) as f64);
    let (cols, rows) = whisker_dims(width, height);
    let mut buckets: Vec<Vec<&Star>> = vec![Vec::new(); cols * rows];
    for star in stars {
        let cx = ((star.x_px / w) * cols as f64)
            .floor()
            .clamp(0.0, (cols - 1) as f64) as usize;
        let cy = ((star.y_px / h) * rows as f64)
            .floor()
            .clamp(0.0, (rows - 1) as f64) as usize;
        buckets[cy * cols + cx].push(star);
    }
    buckets
        .iter()
        .enumerate()
        .filter(|(_, b)| b.len() >= 2)
        .filter_map(|(i, b)| {
            let (angle_deg, elongation, n) = mean_axis(b.iter().copied())?;
            Some(WhiskerCell {
                x_px: ((i % cols) as f64 + 0.5) * w / cols as f64,
                y_px: ((i / cols) as f64 + 0.5) * h / rows as f64,
                angle_deg,
                elongation,
                stars: n,
            })
        })
        .collect()
}

impl EccentricityMap {
    pub const CORNER_NAMES: [&'static str; 4] =
        ["top-left", "top-right", "bottom-left", "bottom-right"];

    pub fn from_stars(stars: &[Star], width: usize, height: usize) -> Option<Self> {
        let (surface, stars_used) = fit_surface(
            stars,
            width,
            height,
            |s| s.eccentricity,
            |v| (0.0..=1.0).contains(&v),
        )?;
        let (lo, hi) = surface_range(&surface, width, height);
        let (w, h) = (width as f64, height as f64);
        let corners = [
            surface.eval_px(0.0, 0.0),
            surface.eval_px(w, 0.0),
            surface.eval_px(0.0, h),
            surface.eval_px(w, h),
        ];
        let clamp = |v: f64| v.clamp(0.0, 1.0);
        let (mean_angle_deg, mean_len, _) = mean_axis(stars.iter()).unwrap_or((0.0, 0.0, 0));
        let mean_ecc = stars
            .iter()
            .map(|s| s.eccentricity)
            .filter(|e| e.is_finite())
            .sum::<f64>()
            / stars.len().max(1) as f64;
        let direction_coherence = if mean_ecc > 1e-9 {
            (mean_len / mean_ecc).clamp(0.0, 1.0)
        } else {
            0.0
        };
        Some(Self {
            whiskers: whisker_grid(stars, width, height),
            whisker_cols: whisker_dims(width, height).0,
            mean_angle_deg,
            direction_coherence,
            centre: clamp(surface.eval_px(w / 2.0, h / 2.0)),
            corners: corners.map(clamp),
            min: clamp(lo),
            max: clamp(hi).max(clamp(lo)),
            surface,
            width,
            height,
            stars_used,
        })
    }

    /// Mean of the four corner values.
    pub fn corner_mean(&self) -> f64 {
        self.corners.iter().sum::<f64>() / 4.0
    }

    /// Index into [`Self::CORNER_NAMES`] of the most elongated corner.
    pub fn worst_corner(&self) -> usize {
        (0..4)
            .max_by(|&a, &b| self.corners[a].total_cmp(&self.corners[b]))
            .unwrap_or(0)
    }

    /// Plain-language reading of the direction coherence.
    pub fn direction_hint(&self) -> &'static str {
        if self.direction_coherence >= 0.6 {
            "common direction: tracking / guiding / wind?"
        } else if self.direction_coherence >= 0.3 {
            "partly common direction"
        } else {
            "no common direction"
        }
    }

    /// Plain-text summary (for the map box and the clipboard).
    pub fn summary_text(&self) -> String {
        let worst = self.worst_corner();
        format!(
            "Min Ecc:      {:.2}\nMax Ecc:      {:.2}\nCentre Ecc:   {:.2}\nCorners Ecc:  {:.2} ({:+.2} vs centre)\nWorst Corner: {} ({:.2})\nDirection:    {:.0}\u{b0} (coherence {:.2}: {})\nStars Used:   {}\n",
            self.min,
            self.max,
            self.centre,
            self.corner_mean(),
            self.corner_mean() - self.centre,
            Self::CORNER_NAMES[worst],
            self.corners[worst],
            self.mean_angle_deg,
            self.direction_coherence,
            self.direction_hint(),
            self.stars_used,
        )
    }
}

fn eval(c: &[f64; 6], u: f64, v: f64) -> f64 {
    c[0] + c[1] * u + c[2] * v + c[3] * u * u + c[4] * v * v + c[5] * u * v
}

/// Least-squares fit of the 6-term quadratic via the normal equations.
fn fit_quadratic(points: &[(f64, f64, f64)]) -> Option<[f64; 6]> {
    let mut ata = [[0.0_f64; 6]; 6];
    let mut atb = [0.0_f64; 6];
    for &(u, v, f) in points {
        let row = [1.0, u, v, u * u, v * v, u * v];
        for i in 0..6 {
            atb[i] += row[i] * f;
            for j in 0..6 {
                ata[i][j] += row[i] * row[j];
            }
        }
    }
    solve6(ata, atb)
}

/// Gaussian elimination with partial pivoting; `None` if the system is singular (for example
/// all stars on a line).
#[allow(clippy::needless_range_loop)]
fn solve6(mut a: [[f64; 6]; 6], mut b: [f64; 6]) -> Option<[f64; 6]> {
    for col in 0..6 {
        let pivot = (col..6).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        if a[pivot][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        for row in col + 1..6 {
            let factor = a[row][col] / a[col][col];
            for k in col..6 {
                a[row][k] -= factor * a[col][k];
            }
            b[row] -= factor * b[col];
        }
    }
    let mut x = [0.0; 6];
    for row in (0..6).rev() {
        let mut sum = b[row];
        for k in row + 1..6 {
            sum -= a[row][k] * x[k];
        }
        x[row] = sum / a[row][row];
    }
    x.iter().all(|v| v.is_finite()).then_some(x)
}

fn median_abs(values: &[f64]) -> f64 {
    let mut abs: Vec<f64> = values.iter().map(|v| v.abs()).collect();
    if abs.is_empty() {
        return 0.0;
    }
    let mid = abs.len() / 2;
    abs.select_nth_unstable_by(mid, f64::total_cmp);
    abs[mid]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stars::RADIAL_PROFILE_BINS;

    fn star(x: f64, y: f64, fwhm: f64) -> Star {
        Star {
            x_px: x,
            y_px: y,
            moment_fwhm_px: fwhm,
            hfr_px: fwhm / 2.0,
            eccentricity: 0.1,
            elongation_angle_deg: 0.0,
            flux_adu: 1000.0,
            peak_adu: 500.0,
            background_adu: 100.0,
            snr: 50.0,
            radial_profile: [0.0; RADIAL_PROFILE_BINS],
        }
    }

    /// Stars on a grid whose FWHM follows `f(x, y)`.
    fn field(w: usize, h: usize, f: impl Fn(f64, f64) -> f64) -> Vec<Star> {
        let mut out = Vec::new();
        for j in 0..12 {
            for i in 0..16 {
                let x = (i as f64 + 0.5) * w as f64 / 16.0;
                let y = (j as f64 + 0.5) * h as f64 / 12.0;
                out.push(star(x, y, f(x, y)));
            }
        }
        out
    }

    #[test]
    fn flat_field_has_no_curvature_tilt_or_offset() {
        let stars = field(4000, 3000, |_, _| 3.0);
        let a = FieldAnalysis::from_stars(&stars, 4000, 3000, None).unwrap();
        assert!(a.curvature_pct.abs() < 1e-6);
        assert!(a.total_tilt_pct.abs() < 1e-6);
        assert!((a.min_fwhm_px - 3.0).abs() < 1e-9);
        assert_eq!(a.stars_used, 192);
    }

    #[test]
    fn centred_bowl_is_curvature_with_centred_sharpest_point() {
        let (w, h) = (4000.0, 3000.0);
        let r_max2 = (w / 2.0) * (w / 2.0) + (h / 2.0) * (h / 2.0);
        // 2.0 px in the centre, 3.0 px in the corners.
        let stars = field(4000, 3000, |x, y| {
            2.0 + ((x - w / 2.0).powi(2) + (y - h / 2.0).powi(2)) / r_max2
        });
        let a = FieldAnalysis::from_stars(&stars, 4000, 3000, Some(1.5)).unwrap();
        assert!(a.sharpest_is_vertex);
        assert!(a.collimation_offset_px < 1.0, "{}", a.collimation_offset_px);
        assert!((a.curvature_pct - 50.0).abs() < 0.5, "{}", a.curvature_pct);
        assert!(a.total_tilt_pct < 0.1);
        assert!((a.min_fwhm_px - 2.0).abs() < 1e-6);
        assert!(a.summary_text().contains("Curvature:    50.0%"));
    }

    #[test]
    fn linear_tilt_reports_side_and_direction() {
        // Right side softer by 0.6 px across the frame, no curvature.
        let stars = field(4000, 3000, |x, _| 2.0 + 0.6 * x / 4000.0);
        let a = FieldAnalysis::from_stars(&stars, 4000, 3000, None).unwrap();
        assert!((a.tilt_x_px - 0.6).abs() < 1e-6, "{}", a.tilt_x_px);
        assert!(a.tilt_y_px.abs() < 1e-6);
        assert!(a.tilt_angle_deg < 1e-3 || a.tilt_angle_deg > 359.999);
        // Sharpest edge (left) FWHM is 2.0 -> 30 %
        assert!(
            (a.total_tilt_pct - 30.0).abs() < 0.1,
            "{}",
            a.total_tilt_pct
        );

        // Top softer: angle 90 (up on screen), tilt Y negative (bottom sharper).
        let stars = field(4000, 3000, |_, y| 2.0 + 0.6 * (3000.0 - y) / 3000.0);
        let a = FieldAnalysis::from_stars(&stars, 4000, 3000, None).unwrap();
        assert!((a.tilt_y_px + 0.6).abs() < 1e-6, "{}", a.tilt_y_px);
        assert!(
            (a.tilt_angle_deg - 90.0).abs() < 1e-3,
            "{}",
            a.tilt_angle_deg
        );
    }

    #[test]
    fn decentred_bowl_reports_collimation_offset_and_ignores_outliers() {
        // Sharpest point 400 px right and 300 px down of centre (500 px away).
        let mut stars = field(4000, 3000, |x, y| {
            2.0 + ((x - 2400.0).powi(2) + (y - 1800.0).powi(2)) / 6.25e6
        });
        // A few wildly wrong measurements (double stars, satellites).
        for k in 0..6 {
            stars.push(star(300.0 + 500.0 * k as f64, 1500.0, 12.0));
        }
        let a = FieldAnalysis::from_stars(&stars, 4000, 3000, None).unwrap();
        assert!(a.sharpest_is_vertex);
        assert!(
            (a.collimation_offset_px - 500.0).abs() < 5.0,
            "{}",
            a.collimation_offset_px
        );
        assert!((a.sharpest_px[0] - 2400.0).abs() < 5.0);
        assert!((a.sharpest_px[1] - 1800.0).abs() < 5.0);
        assert!(a.stars_used <= 192, "outliers must be clipped");
    }

    #[test]
    fn eccentricity_map_finds_elongated_corner() {
        // Round in the centre, increasingly elongated towards the bottom-right corner.
        let mut stars = field(4000, 3000, |_, _| 3.0);
        for s in &mut stars {
            let d =
                ((s.x_px - 2000.0).max(0.0) / 2000.0 + (s.y_px - 1500.0).max(0.0) / 1500.0) / 2.0;
            s.eccentricity = 0.2 + 0.5 * d * d;
        }
        let m = EccentricityMap::from_stars(&stars, 4000, 3000).unwrap();
        assert_eq!(
            EccentricityMap::CORNER_NAMES[m.worst_corner()],
            "bottom-right"
        );
        assert!(m.centre < 0.3, "{}", m.centre);
        assert!(m.max > 0.45 && m.max <= 1.0, "{}", m.max);
        assert!(m.min >= 0.0 && m.min < m.max);
        assert!(m.summary_text().contains("Worst Corner: bottom-right"));
    }

    #[test]
    fn whiskers_follow_star_direction_and_coherence() {
        // Every star elongated at 30 degrees: one common direction (trailing).
        let mut stars = field(4000, 3000, |_, _| 3.0);
        for s in &mut stars {
            s.eccentricity = 0.6;
            s.elongation_angle_deg = 30.0;
        }
        let m = EccentricityMap::from_stars(&stars, 4000, 3000).unwrap();
        assert!(
            (m.mean_angle_deg - 30.0).abs() < 1e-6,
            "{}",
            m.mean_angle_deg
        );
        assert!(m.direction_coherence > 0.99);
        assert!(m.direction_hint().contains("tracking"));
        assert!(!m.whiskers.is_empty());
        for w in &m.whiskers {
            assert!((w.angle_deg - 30.0).abs() < 1e-6);
            assert!((w.elongation - 0.6).abs() < 1e-9);
        }

        // Radial pattern (coma-like): no common direction, whiskers point at the centre.
        for s in &mut stars {
            let a = (-(s.y_px - 1500.0)).atan2(s.x_px - 2000.0).to_degrees();
            s.elongation_angle_deg = a.rem_euclid(180.0);
        }
        let m = EccentricityMap::from_stars(&stars, 4000, 3000).unwrap();
        assert!(m.direction_coherence < 0.3, "{}", m.direction_coherence);
        let corner = m
            .whiskers
            .iter()
            .min_by(|a, b| (a.x_px + a.y_px).total_cmp(&(b.x_px + b.y_px)))
            .unwrap();
        // Top-left cell: radial direction is about 180 - 37 = 143 degrees on screen.
        let expected = (-(corner.y_px - 1500.0))
            .atan2(corner.x_px - 2000.0)
            .to_degrees()
            .rem_euclid(180.0);
        assert!(
            (corner.angle_deg - expected).abs() < 6.0,
            "{} vs {expected}",
            corner.angle_deg
        );
    }

    #[test]
    fn summary_switches_between_pixels_and_arcseconds() {
        let stars = field(4000, 3000, |x, _| 2.0 + 0.6 * x / 4000.0);
        let a = FieldAnalysis::from_stars(&stars, 4000, 3000, Some(2.0)).unwrap();
        let px = a.summary_text_in(None);
        let arcsec = a.summary_text_in(Some(2.0));
        assert!(px.contains("Min FWHM:     2.00 px"), "{px}");
        assert!(px.contains("Tilt in X:    +0.60 px"), "{px}");
        assert!(arcsec.contains("Min FWHM:     4.00\u{2033}"), "{arcsec}");
        assert!(arcsec.contains("Tilt in X:    +1.20\u{2033}"), "{arcsec}");
        assert_eq!(format_length(1.5, None), "1.50 px");
    }

    #[test]
    fn too_few_stars_gives_no_analysis() {
        let stars: Vec<Star> = (0..10).map(|i| star(i as f64 * 10.0, 5.0, 2.0)).collect();
        assert!(FieldAnalysis::from_stars(&stars, 100, 100, None).is_none());
    }
}
