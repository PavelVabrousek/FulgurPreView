//! Real-time hot-pixel-robust star photometry (FWHM, HFR, Eccentricity),
//! live Cursor Star Inspector lookup, and 3x3 Sensor Tilt & Corner Map.
//! Adapted from `FulgurStack` (`fulgur-pipeline::stars`) for exact metric parity.

use fulgur_core::fits::{
    read_header_hdu, read_raw_from_header_cancellable, read_rgb_roi, saturation_level,
};
use fulgur_core::{
    BayerPattern, Color, Error, LayoutChoice, Mosaic, RawFrame, Result, check_cancelled,
};
use serde::Serialize;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

/// Number of stars used for global frame median FWHM / HFR / Eccentricity
/// (matches `FulgurStack` `CATALOG_STARS = 250` for exact parity).
pub const CATALOG_STARS: usize = 250;

/// Reported instead of star metrics when a camera RAW only yields its embedded JPEG.
/// Shown instead of star metrics for 8-bit processed exports (ADR-022).
pub const RENDERED_IMAGE_STARS_MESSAGE: &str = "Star metrics need linear data (FITS, XISF, camera RAW or 16/32-bit TIFF). This 8-bit image is a processed export: stretching, downscaling and clipped stars would make FWHM / HFR / tilt misleading.";
pub const RAW_PREVIEW_ONLY_STARS_MESSAGE: &str = "Star metrics unavailable: this RAW format's sensor data is not decoded yet (only the embedded JPEG preview), and FWHM measured on a JPEG would be misleading.";

/// Total stars measured per frame to populate the 3x3 Sensor Tilt Map and Cursor Inspector.
/// Because candidates are visited in round-robin bucket rank order, the first `CATALOG_STARS`
/// stars are 100% identical to `FulgurStack`'s `CATALOG_STARS` list.
pub const MAX_INSPECT_STARS: usize = 450;

/// Number of radial bins in `Star::radial_profile` spanning `r = 0.0 .. 6.0 px`.
pub const RADIAL_PROFILE_BINS: usize = 16;

pub(crate) fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mid = values.len() / 2;
    let even = mid * 2 == values.len();
    let (lower, center, _) = values.select_nth_unstable_by(mid, f64::total_cmp);
    if even {
        (lower.iter().copied().fold(f64::NEG_INFINITY, f64::max) + *center) * 0.5
    } else {
        *center
    }
}

fn median_opt(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut vec: Vec<f64> = values.filter(|v| v.is_finite()).collect();
    (!vec.is_empty()).then(|| median(&mut vec))
}

fn location_noise(values: &mut Vec<f64>) -> (f64, f64) {
    let mut center = median(values);
    let mut residuals: Vec<_> = values.iter().map(|v| (v - center).abs()).collect();
    let mut noise = 1.4826 * median(&mut residuals);
    if noise > 0.0 {
        values.retain(|v| (v - center).abs() <= 3.0 * noise);
        center = median(values);
        residuals.clear();
        residuals.extend(values.iter().map(|v| (v - center).abs()));
        noise = 1.4826 * median(&mut residuals);
    }
    (center, noise)
}

struct Sky {
    cells: Vec<(f64, f64)>,
    width: usize,
    height: usize,
    background: f64,
    noise: f64,
}

impl Sky {
    fn new(grid: &[f32], width: usize, height: usize) -> Self {
        Self::sampled(width, height, |x, y| grid[y * width + x])
    }

    fn sampled(width: usize, height: usize, value: impl Fn(usize, usize) -> f32) -> Self {
        let mut cells = Vec::with_capacity(256);
        for cy in 0..16 {
            for cx in 0..16 {
                let (x0, x1) = (cx * width / 16, (cx + 1) * width / 16);
                let (y0, y1) = (cy * height / 16, (cy + 1) * height / 16);
                let (nx, ny) = ((x1 - x0).clamp(1, 16), (y1 - y0).clamp(1, 16));
                let mut samples = Vec::with_capacity(nx * ny);
                for sy in 0..ny {
                    for sx in 0..nx {
                        let x = (x0 + (2 * sx + 1) * (x1 - x0) / (2 * nx)).min(width - 1);
                        let y = (y0 + (2 * sy + 1) * (y1 - y0) / (2 * ny)).min(height - 1);
                        samples.push(f64::from(value(x, y)));
                    }
                }
                cells.push(location_noise(&mut samples));
            }
        }
        let background = median(&mut cells.iter().map(|v| v.0).collect::<Vec<_>>());
        let noise = median(&mut cells.iter().map(|v| v.1).collect::<Vec<_>>());
        Self {
            cells,
            width,
            height,
            background,
            noise,
        }
    }

    fn at(&self, x: usize, y: usize) -> (f64, f64) {
        self.cells
            [(y * 16 / self.height.max(1)).min(15) * 16 + (x * 16 / self.width.max(1)).min(15)]
    }
}

/// Sky level of each colour channel (R, G, B) of a CFA frame.
pub fn channel_sky(
    pixels: &[f32],
    width: usize,
    height: usize,
    pattern: BayerPattern,
    green: f64,
) -> [f64; 3] {
    let (gw, gh) = (width / 2, height / 2);
    if gw == 0 || gh == 0 {
        return [green, green, green];
    }
    let level = |color: Color| {
        let (px, py) = [(0, 0), (1, 0), (0, 1), (1, 1)]
            .into_iter()
            .find(|&(x, y)| pattern.color(x, y) == color)
            .unwrap_or((0, 0));
        Sky::sampled(gw, gh, |x, y| pixels[(2 * y + py) * width + 2 * x + px]).background
    };
    [level(Color::Red), green, level(Color::Blue)]
}

#[derive(Clone, Debug, Serialize)]
pub struct Star {
    pub x_px: f64,
    pub y_px: f64,
    pub hfr_px: f64,
    pub moment_fwhm_px: f64,
    pub eccentricity: f64,
    /// Direction of the star's long axis: degrees in `[0, 180)`, 0 = horizontal, 90 = vertical,
    /// counter-clockwise as seen on screen (from the second moments).
    pub elongation_angle_deg: f64,
    pub flux_adu: f64,
    pub peak_adu: f64,
    pub background_adu: f64,
    pub snr: f64,
    /// Normalized `[0.0 .. 1.0]` radial brightness profile across `r = 0.0 .. 6.0 px`.
    pub radial_profile: [f32; RADIAL_PROFILE_BINS],
}

/// One cell of the 3x3 Sensor Tilt & Corner Map.
#[derive(Clone, Debug, Serialize)]
pub struct TiltCell {
    pub code: &'static str,
    pub name: &'static str,
    pub star_count: usize,
    pub median_fwhm_px: Option<f64>,
    pub median_hfr_px: Option<f64>,
    pub median_eccentricity: Option<f64>,
    /// Percentage deviation of this cell's FWHM from the center (or global) reference.
    pub delta_fwhm_pct: Option<f64>,
    /// Percentage deviation of this cell's HFR from the center (or global) reference.
    pub delta_hfr_pct: Option<f64>,
}

/// 3x3 Sensor Tilt & Corner Map across the 9 sensor zones.
#[derive(Clone, Debug, Serialize)]
pub struct SensorTiltMap {
    pub cells: [TiltCell; 9],
    /// Reference FWHM (Center cell if populated, otherwise global median).
    pub ref_fwhm_px: Option<f64>,
    /// Reference HFR (Center cell if populated, otherwise global median).
    pub ref_hfr_px: Option<f64>,
    /// Mean corner FWHM vs center FWHM in percent (positive = corners softer / field curvature).
    pub corner_to_center_fwhm_pct: Option<f64>,
    /// Max minus min corner FWHM in pixels (measures sensor tilt across corners).
    pub max_corner_tilt_px: Option<f64>,
    /// Max minus min corner FWHM as a percentage of reference FWHM.
    pub max_corner_tilt_pct: Option<f64>,
}

impl SensorTiltMap {
    pub fn from_stars(
        stars: &[Star],
        width: usize,
        height: usize,
        global_fwhm: Option<f64>,
        global_hfr: Option<f64>,
    ) -> Self {
        const CODES: [&str; 9] = ["TL", "T", "TR", "L", "C", "R", "BL", "B", "BR"];
        const NAMES: [&str; 9] = [
            "Top-Left",
            "Top",
            "Top-Right",
            "Left",
            "Center",
            "Right",
            "Bottom-Left",
            "Bottom",
            "Bottom-Right",
        ];

        let mut cell_stars: [Vec<&Star>; 9] = std::array::from_fn(|_| Vec::new());
        let w = width.max(1) as f64;
        let h = height.max(1) as f64;
        for star in stars {
            let cx = ((star.x_px / w) * 3.0).floor().clamp(0.0, 2.0) as usize;
            let cy = ((star.y_px / h) * 3.0).floor().clamp(0.0, 2.0) as usize;
            cell_stars[cy * 3 + cx].push(star);
        }

        let mut cells: [TiltCell; 9] = std::array::from_fn(|idx| {
            let list = &cell_stars[idx];
            TiltCell {
                code: CODES[idx],
                name: NAMES[idx],
                star_count: list.len(),
                median_fwhm_px: median_opt(list.iter().map(|s| s.moment_fwhm_px)),
                median_hfr_px: median_opt(list.iter().map(|s| s.hfr_px)),
                median_eccentricity: median_opt(list.iter().map(|s| s.eccentricity)),
                delta_fwhm_pct: None,
                delta_hfr_pct: None,
            }
        });

        let ref_fwhm_px = cells[4].median_fwhm_px.or(global_fwhm);
        let ref_hfr_px = cells[4].median_hfr_px.or(global_hfr);

        for cell in &mut cells {
            if let (Some(v), Some(r)) = (cell.median_fwhm_px, ref_fwhm_px)
                && r > 1e-6
            {
                cell.delta_fwhm_pct = Some((v - r) / r * 100.0);
            }
            if let (Some(v), Some(r)) = (cell.median_hfr_px, ref_hfr_px)
                && r > 1e-6
            {
                cell.delta_hfr_pct = Some((v - r) / r * 100.0);
            }
        }

        let corner_fwhms: Vec<f64> = [0, 2, 6, 8]
            .into_iter()
            .filter_map(|i| cells[i].median_fwhm_px)
            .collect();

        let corner_to_center_fwhm_pct = if !corner_fwhms.is_empty() {
            if let Some(center) = ref_fwhm_px.filter(|&c| c > 1e-6) {
                let mean_corner = corner_fwhms.iter().sum::<f64>() / corner_fwhms.len() as f64;
                Some((mean_corner - center) / center * 100.0)
            } else {
                None
            }
        } else {
            None
        };

        let (max_corner_tilt_px, max_corner_tilt_pct) = if corner_fwhms.len() >= 2 {
            let min_c = corner_fwhms.iter().copied().fold(f64::INFINITY, f64::min);
            let max_c = corner_fwhms
                .iter()
                .copied()
                .fold(f64::NEG_INFINITY, f64::max);
            let diff = (max_c - min_c).max(0.0);
            let pct = ref_fwhm_px.filter(|&r| r > 1e-6).map(|r| diff / r * 100.0);
            (Some(diff), pct)
        } else {
            (None, None)
        };

        Self {
            cells,
            ref_fwhm_px,
            ref_hfr_px,
            corner_to_center_fwhm_pct,
            max_corner_tilt_px,
            max_corner_tilt_pct,
        }
    }
}

/// Complete single-frame star photometry and sensor tilt report for Milestone 3.
#[derive(Clone, Debug, Serialize)]
pub struct StarQualityReport {
    pub width: usize,
    pub height: usize,
    pub plane_label: &'static str,
    pub background_adu: f64,
    pub channel_background_adu: Option<[f64; 3]>,
    pub noise_adu: f64,
    pub detected_peaks: usize,
    pub star_count: usize,
    pub measured_star_count: usize,
    pub suppressed_hot_pixels: usize,
    pub refused_defect_detections: usize,
    pub median_fwhm_px: Option<f64>,
    pub median_fwhm_arcsec: Option<f64>,
    pub median_hfr_px: Option<f64>,
    pub median_eccentricity: Option<f64>,
    pub pixel_scale_arcsec: Option<f64>,
    pub stars: Vec<Star>,
    pub tilt_map: SensorTiltMap,
    /// Field curvature / tilt / collimation from a FWHM surface fit (`None` with too few
    /// stars).
    pub field: Option<crate::field::FieldAnalysis>,
    /// Star elongation surface across the field (`None` with too few stars).
    pub eccentricity_map: Option<crate::field::EccentricityMap>,
    /// Median and noise per colour channel at full resolution, in ADU.
    pub levels: Vec<crate::levels::ChannelLevel>,
    pub elapsed_ms: f64,
    /// Why star metrics were not measured for this frame (shown instead of them).
    pub metrics_unavailable: Option<&'static str>,
}

impl StarQualityReport {
    /// A report carrying only per-channel levels, with star metrics withheld for `reason`.
    pub fn metrics_unavailable(
        width: usize,
        height: usize,
        levels: Vec<crate::levels::ChannelLevel>,
        reason: &'static str,
    ) -> Self {
        Self {
            width,
            height,
            plane_label: "Not measured",
            background_adu: 0.0,
            channel_background_adu: None,
            noise_adu: 0.0,
            detected_peaks: 0,
            star_count: 0,
            measured_star_count: 0,
            suppressed_hot_pixels: 0,
            refused_defect_detections: 0,
            median_fwhm_px: None,
            median_fwhm_arcsec: None,
            median_hfr_px: None,
            median_eccentricity: None,
            pixel_scale_arcsec: None,
            stars: Vec::new(),
            tilt_map: SensorTiltMap::from_stars(&[], width, height, None, None),
            field: None,
            eccentricity_map: None,
            levels,
            elapsed_ms: 0.0,
            metrics_unavailable: Some(reason),
        }
    }

    /// Find the nearest measured star within `max_radius_px` native sensor pixels of `(x_px, y_px)`.
    pub fn nearest_star(&self, x_px: f64, y_px: f64, max_radius_px: f64) -> Option<&Star> {
        let max_r2 = max_radius_px * max_radius_px;
        self.stars
            .iter()
            .filter_map(|star| {
                let d2 = (star.x_px - x_px).powi(2) + (star.y_px - y_px).powi(2);
                (d2 <= max_r2).then_some((d2, star))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, s)| s)
    }
}

struct Green<'a> {
    raw: &'a [f32],
    width: usize,
    height: usize,
    pattern: Option<BayerPattern>,
    plans: [Vec<(isize, f64)>; 16],
}

static PHASE_WEIGHTS: std::sync::LazyLock<[[f64; 6]; 4]> = std::sync::LazyLock::new(|| {
    std::array::from_fn(|phase| {
        let mut weights: [f64; 6] = std::array::from_fn(|i| {
            let x = phase as f64 * 0.25 - (i as f64 - 2.0);
            if x.abs() < 1e-12 {
                1.0
            } else if x.abs() >= 3.0 {
                0.0
            } else {
                let p = std::f64::consts::PI * x;
                (p.sin() / p) * ((p / 3.0).sin() / (p / 3.0))
            }
        });
        let sum: f64 = weights.iter().sum();
        for value in &mut weights {
            *value /= sum;
        }
        weights
    })
});

impl<'a> Green<'a> {
    fn mono(raw: &'a [f32], width: usize, height: usize) -> Self {
        Self {
            raw,
            width,
            height,
            pattern: None,
            plans: std::array::from_fn(|_| Vec::new()),
        }
    }

    fn new(raw: &'a [f32], width: usize, height: usize, pattern: BayerPattern) -> Self {
        let phase = if pattern.color(0, 0) == Color::Green {
            0
        } else {
            1
        };
        let plans = std::array::from_fn(|n| {
            let (x, y) = ((n % 4) as f64 * 0.5, (n / 4) as f64 * 0.5);
            let (u, v) = ((x + y - phase as f64) * 0.5, (x - y - phase as f64) * 0.5);
            let (iu, iv) = (u.floor() as isize, v.floor() as isize);
            let wx = &PHASE_WEIGHTS[((u - iu as f64) * 4.0).round() as usize];
            let wy = &PHASE_WEIGHTS[((v - iv as f64) * 4.0).round() as usize];
            let mut taps = Vec::new();
            for (j, &w_y) in wy.iter().enumerate() {
                for (i, &w_x) in wx.iter().enumerate() {
                    let weight = w_x * w_y;
                    if weight.abs() < 1e-12 {
                        continue;
                    }
                    let (a, b) = (iu + i as isize - 2, iv + j as isize - 2);
                    let (dx, dy) = (a + b + phase, a - b);
                    taps.push((dy * width as isize + dx, weight));
                }
            }
            taps
        });
        Self {
            raw,
            width,
            height,
            pattern: Some(pattern),
            plans,
        }
    }

    fn interpolate(&self, x: f64, y: f64) -> f64 {
        if self.pattern.is_none() {
            return self.interpolate_mono(x, y);
        }
        let (ix, iy) = ((x * 2.0) as usize, (y * 2.0) as usize);
        let base = (iy / 4 * 2 * self.width + ix / 4 * 2) as isize;
        self.plans[(iy % 4) * 4 + ix % 4]
            .iter()
            .map(|&(offset, weight)| weight * f64::from(self.raw[(base + offset) as usize]))
            .sum()
    }

    fn interpolate_mono(&self, x: f64, y: f64) -> f64 {
        let (ix, iy) = (x.floor() as isize, y.floor() as isize);
        let wx = &PHASE_WEIGHTS[((x - ix as f64) * 4.0).round() as usize];
        let wy = &PHASE_WEIGHTS[((y - iy as f64) * 4.0).round() as usize];
        let mut sum = 0.0;
        for (j, &w_y) in wy.iter().enumerate() {
            let row = (iy + j as isize - 2) as usize * self.width;
            for (i, &w_x) in wx.iter().enumerate() {
                sum += w_x * w_y * f64::from(self.raw[row + (ix + i as isize - 2) as usize]);
            }
        }
        sum
    }

    fn measure(&self, x: f64, y: f64, background: f64, noise: f64) -> Option<Star> {
        let (minimum_support, aperture_samples) = if self.pattern.is_some() {
            (9, 56.0_f64)
        } else {
            (18, 113.0_f64)
        };
        if x < 14.0 || y < 14.0 || x + 14.0 >= self.width as f64 || y + 14.0 >= self.height as f64 {
            return None;
        }
        let (mut cx, mut cy) = (x, y);
        let mut peak = 0.0_f64;
        let mut support = 0;
        for _ in 0..3 {
            let (mut flux, mut sx, mut sy) = (0.0, 0.0, 0.0);
            for yy in (cy as usize - 6)..=(cy as usize + 6) {
                for xx in (cx as usize - 6)..=(cx as usize + 6) {
                    if (xx as f64 - cx).powi(2) + (yy as f64 - cy).powi(2) > 36.0 {
                        continue;
                    }
                    let value = f64::from(self.raw[yy * self.width + xx]);
                    if (xx as f64 - cx).abs() <= 2.0
                        && (yy as f64 - cy).abs() <= 2.0
                        && value > 62000.0
                    {
                        return None;
                    }
                    if self
                        .pattern
                        .is_some_and(|pattern| pattern.color(xx, yy) != Color::Green)
                    {
                        continue;
                    }
                    peak = peak.max(value);
                    if value > background + 3.0 * noise {
                        support += 1;
                    }
                    let f = (value - background).max(0.0);
                    flux += f;
                    sx += f * xx as f64;
                    sy += f * yy as f64;
                }
            }
            if flux <= 0.0 {
                return None;
            }
            cx = sx / flux;
            cy = sy / flux;
            if (cx - x).abs() > 3.0 || (cy - y).abs() > 3.0 {
                return None;
            }
        }
        if support < minimum_support || peak < background + 10.0 * noise {
            return None;
        }
        if cx < 14.0
            || cy < 14.0
            || cx + 14.0 >= self.width as f64
            || cy + 14.0 >= self.height as f64
        {
            return None;
        }

        let mut radial = vec![0.0; 640];
        let mut prof_sum = [0.0_f64; RADIAL_PROFILE_BINS];
        let mut prof_cnt = [0u32; RADIAL_PROFILE_BINS];
        let (mut flux, mut xx, mut yy, mut xy) = (0.0, 0.0, 0.0, 0.0);
        for j in -12..=12 {
            for i in -12..=12 {
                let px = (cx * 2.0).floor() * 0.5 + i as f64 * 0.5;
                let py = (cy * 2.0).floor() * 0.5 + j as f64 * 0.5;
                let (dx, dy) = (px - cx, py - cy);
                let r2 = dx * dx + dy * dy;
                if r2 > 36.0 {
                    continue;
                }
                let sample_excess = (self.interpolate(px, py) - background).max(0.0);
                let r_center = r2.sqrt();
                let pbin = ((r_center / 6.0) * RADIAL_PROFILE_BINS as f64) as usize;
                if pbin < RADIAL_PROFILE_BINS {
                    prof_sum[pbin] += sample_excess;
                    prof_cnt[pbin] += 1;
                }

                let f = sample_excess * 0.25;
                flux += f;
                xx += f * dx * dx;
                yy += f * dy * dy;
                xy += f * dx * dy;
                for oy in [-0.125, 0.125] {
                    for ox in [-0.125, 0.125] {
                        let radius = ((dx + ox).powi(2) + (dy + oy).powi(2)).sqrt();
                        radial[(radius * 100.0) as usize] += f / 4.0;
                    }
                }
            }
        }
        let snr = flux / (noise.max(1e-12) * aperture_samples.sqrt() * 2.0);
        if flux <= 0.0 || snr < 15.0 {
            return None;
        }
        let trace = (xx + yy) / flux;
        let delta = (((xx - yy) / flux).powi(2) + 4.0 * (xy / flux).powi(2)).sqrt();
        let (major, minor) = ((trace + delta) * 0.5, (trace - delta) * 0.5);
        if minor <= 0.1 || major <= 0.0 {
            return None;
        }
        let (mut cumulative, mut hfr) = (0.0, 0.0);
        for (bin, f) in radial.into_iter().enumerate() {
            if cumulative + f >= flux * 0.5 && f > 0.0 {
                hfr = (bin as f64 + (flux * 0.5 - cumulative) / f) / 100.0;
                break;
            }
            cumulative += f;
        }
        if !(0.5..5.0).contains(&hfr) {
            return None;
        }

        let mut radial_profile = [0.0_f32; RADIAL_PROFILE_BINS];
        let mut max_prof = 0.0_f64;
        for b in 0..RADIAL_PROFILE_BINS {
            if prof_cnt[b] > 0 {
                let avg = prof_sum[b] / f64::from(prof_cnt[b]);
                max_prof = max_prof.max(avg);
                radial_profile[b] = avg as f32;
            }
        }
        if max_prof > 1e-9 {
            for v in &mut radial_profile {
                *v = (*v / max_prof as f32).clamp(0.0, 1.0);
            }
        }

        Some(Star {
            x_px: cx,
            y_px: cy,
            hfr_px: hfr,
            moment_fwhm_px: 2.354820045 * (trace * 0.5).sqrt(),
            eccentricity: (1.0 - minor / major).max(0.0).sqrt(),
            // Sensor y points down; negate so the angle is counter-clockwise on screen.
            elongation_angle_deg: (-0.5 * (2.0 * xy).atan2(xx - yy))
                .to_degrees()
                .rem_euclid(180.0),
            flux_adu: flux,
            peak_adu: peak,
            background_adu: background,
            snr,
            radial_profile,
        })
    }
}

const MIN_NEIGHBOUR_SHARE: f64 = 0.1;
const SATURATED_STAR_ADU: f32 = 62000.0;

pub fn saturated_limit(saturation: Option<f32>) -> f32 {
    saturation.map_or(SATURATED_STAR_ADU, |level| {
        level * (SATURATED_STAR_ADU / 65535.0)
    })
}

fn plane_neighbours(pattern: Option<BayerPattern>) -> [(isize, isize); 4] {
    if pattern.is_some() {
        [(-1, -1), (1, -1), (-1, 1), (1, 1)]
    } else {
        [(0, -1), (-1, 0), (1, 0), (0, 1)]
    }
}

fn natural_profile(
    raw: &[f32],
    width: usize,
    pattern: Option<BayerPattern>,
    x: usize,
    y: usize,
) -> bool {
    let on_plane = |xx: usize, yy: usize| pattern.is_none_or(|p| p.color(xx, yy) == Color::Green);
    let mut best = (2 * x, 2 * y);
    let mut peak = f32::NEG_INFINITY;
    for yy in 2 * y - 1..=2 * y + 2 {
        for xx in 2 * x - 1..=2 * x + 2 {
            if on_plane(xx, yy) && raw[yy * width + xx] > peak {
                peak = raw[yy * width + xx];
                best = (xx, yy);
            }
        }
    }
    let (px, py) = best;
    let mut ring = Vec::new();
    for yy in py - 10..=py + 10 {
        for xx in px - 10..=px + 10 {
            let r2 = (xx.abs_diff(px).pow(2) + yy.abs_diff(py).pow(2)) as f64;
            if (49.0..=100.0).contains(&r2) && on_plane(xx, yy) {
                ring.push(f64::from(raw[yy * width + xx]));
            }
        }
    }
    let background = median(&mut ring);
    let mut near = plane_neighbours(pattern).map(|(dx, dy)| {
        let (xx, yy) = (px as isize + dx, py as isize + dy);
        f64::from(raw[yy as usize * width + xx as usize]) - background
    });
    near.sort_unstable_by(f64::total_cmp);
    let excess = f64::from(peak) - background;
    excess > 0.0 && (near[1] + near[2]) * 0.5 >= MIN_NEIGHBOUR_SHARE * excess
}

fn cell_kind(pattern: Option<BayerPattern>) -> (bool, bool) {
    (
        pattern.is_none(),
        pattern.is_some_and(|p| p.color(0, 0) == Color::Green),
    )
}

#[inline]
fn grid_cell(pixels: &[f32], width: usize, kind: (bool, bool), x: usize, y: usize) -> f32 {
    let i = 2 * y * width + 2 * x;
    if kind.0 {
        (pixels[i] + pixels[i + 1] + pixels[i + width] + pixels[i + width + 1]) * 0.25
    } else if kind.1 {
        (pixels[i] + pixels[i + width + 1]) * 0.5
    } else {
        (pixels[i + 1] + pixels[i + width]) * 0.5
    }
}

fn detection_grid(
    pixels: &[f32],
    width: usize,
    pattern: Option<BayerPattern>,
    cancel: &AtomicBool,
) -> Result<Vec<f32>> {
    let (gw, gh) = (width / 2, pixels.len() / width / 2);
    let kind = cell_kind(pattern);
    let mut grid = vec![0.0; gw * gh];
    for y in 0..gh {
        check_cancelled(cancel)?;
        for x in 0..gw {
            grid[y * gw + x] = grid_cell(pixels, width, kind, x, y);
        }
    }
    Ok(grid)
}

fn suppress(
    pixels: &[f32],
    width: usize,
    pattern: Option<BayerPattern>,
    fixes: &[(usize, f32)],
    grid: &mut [f32],
    copy: &mut Vec<f32>,
) {
    copy.clear();
    copy.extend_from_slice(pixels);
    for &(i, value) in fixes {
        copy[i] = value;
    }
    let (gw, gh) = (width / 2, pixels.len() / width / 2);
    let kind = cell_kind(pattern);
    for &(i, _) in fixes {
        let (x, y) = (i % width / 2, i / width / 2);
        if x < gw && y < gh {
            grid[y * gw + x] = grid_cell(copy, width, kind, x, y);
        }
    }
}

fn defects(
    pixels: &[f32],
    width: usize,
    height: usize,
    pattern: Option<BayerPattern>,
    sky: &Sky,
    pixel_noise: f64,
    cancel: &AtomicBool,
) -> Result<Vec<(usize, f32)>> {
    let offsets = plane_neighbours(pattern).map(|(dx, dy)| dy * width as isize + dx);
    let limit = 10.0 * pixel_noise;
    let mut fixes = Vec::new();
    for y in 1..height - 1 {
        check_cancelled(cancel)?;
        let row_sky = y / 2;
        let (first, step) = match pattern {
            Some(p) if p.color(1, y) == Color::Green => (1, 2),
            Some(_) => (2, 2),
            None => (1, 1),
        };
        for x in (first..width - 1).step_by(step) {
            let i = y * width + x;
            let floor = pixels[i] - limit as f32;
            if offsets
                .iter()
                .filter(|&&o| pixels[(i as isize + o) as usize] < floor)
                .count()
                < 2
            {
                continue;
            }
            let value = f64::from(pixels[i]);
            let mut near = offsets.map(|o| f64::from(pixels[(i as isize + o) as usize]));
            near.sort_unstable_by(f64::total_cmp);
            let middle = (near[1] + near[2]) * 0.5;
            if value - middle <= limit {
                continue;
            }
            let background = sky
                .at((x / 2).min(sky.width - 1), row_sky.min(sky.height - 1))
                .0;
            if middle - background < MIN_NEIGHBOUR_SHARE * (value - background) {
                fixes.push((i, middle as f32));
            }
        }
    }
    Ok(fixes)
}

/// Run hot-pixel-robust star photometry on a raw single-channel plane (CFA with `pattern` or Mono `None`).
#[allow(clippy::too_many_arguments)]
pub fn analyze_plane_stars(
    frame_pixels: &[f32],
    width: usize,
    height: usize,
    pattern: Option<BayerPattern>,
    saturation: Option<f32>,
    pixel_scale_arcsec: Option<f64>,
    cancel: &AtomicBool,
) -> Result<StarQualityReport> {
    let t0 = Instant::now();
    if width < 32 || height < 32 || frame_pixels.len() < width * height {
        return Err(Error::Invalid(
            "Frame dimensions are too small for star photometry (minimum 32x32).",
        ));
    }
    let saturated_above = saturated_limit(saturation);
    let (gw, gh) = (width / 2, height / 2);
    let mut grid = detection_grid(frame_pixels, width, pattern, cancel)?;
    let sky = Sky::new(&grid, gw, gh);
    let pixel_noise = if pattern.is_some() {
        sky.noise * std::f64::consts::SQRT_2
    } else {
        sky.noise * 2.0
    };

    let fixes = defects(
        frame_pixels,
        width,
        height,
        pattern,
        &sky,
        pixel_noise,
        cancel,
    )?;
    let suppressed_hot_pixels = fixes.len();
    let mut copy = Vec::new();
    let pixels: &[f32] = if fixes.is_empty() {
        frame_pixels
    } else {
        suppress(frame_pixels, width, pattern, &fixes, &mut grid, &mut copy);
        &copy
    };

    let peak_limits: Vec<f64> = sky
        .cells
        .iter()
        .map(|&(bg, noise)| (bg + 6.0 * noise).max(sky.background + 6.0 * sky.noise))
        .collect();
    let column_cells: Vec<usize> = (0..gw).map(|x| (x * 16 / gw).min(15)).collect();
    let mut buckets: [Vec<(f32, usize, usize)>; 16] = std::array::from_fn(|_| Vec::new());
    let mut detected_peaks = 0;
    let mut detected_stars = 0;
    let mut refused_defect_detections = 0;

    for y in 7..gh.saturating_sub(7) {
        check_cancelled(cancel)?;
        let row_cells = (y * 16 / gh).min(15) * 16;
        for x in 7..gw.saturating_sub(7) {
            let v = grid[y * gw + x];
            if f64::from(v) <= peak_limits[row_cells + column_cells[x]] {
                continue;
            }
            let (bg, noise) = sky.at(x, y);
            let mut is_peak = true;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let neighbor =
                        grid[(y as isize + dy) as usize * gw + (x as isize + dx) as usize];
                    if neighbor > v || (neighbor == v && (dy < 0 || (dy == 0 && dx < 0))) {
                        is_peak = false;
                    }
                }
            }
            if !is_peak {
                continue;
            }
            detected_peaks += 1;
            let mut supported = 0;
            for yy in y - 1..=y + 1 {
                for xx in x - 1..=x + 1 {
                    supported += usize::from(f64::from(grid[yy * gw + xx]) > bg + 3.0 * noise);
                }
            }
            if supported < 3 {
                continue;
            }
            let mut saturated = false;
            for yy in 2 * y - 1..=2 * y + 2 {
                for xx in 2 * x - 1..=2 * x + 2 {
                    saturated |= pixels[yy * width + xx] > saturated_above;
                }
            }
            if saturated {
                continue;
            }
            if !natural_profile(pixels, width, pattern, x, y) {
                refused_defect_detections += 1;
                continue;
            }
            detected_stars += 1;
            let bucket = &mut buckets[(y * 4 / gh).min(3) * 4 + (x * 4 / gw).min(3)];
            bucket.push((v, x, y));
        }
    }

    let per_cell = MAX_INSPECT_STARS.div_ceil(16).max(64);
    for bucket in &mut buckets {
        bucket.sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then(a.2.cmp(&b.2)).then(a.1.cmp(&b.1)));
        bucket.truncate(per_cell);
    }

    let plane = match pattern {
        Some(pattern) => Green::new(pixels, width, height, pattern),
        None => Green::mono(pixels, width, height),
    };
    let measure = |x: usize, y: usize| {
        let (cx, cy) = (2.0 * x as f64 + 0.5, 2.0 * y as f64 + 0.5);
        let mut ring = Vec::new();
        for yy in (cy as usize - 10)..=(cy as usize + 10) {
            for xx in (cx as usize - 10)..=(cx as usize + 10) {
                let r2 = (xx as f64 - cx).powi(2) + (yy as f64 - cy).powi(2);
                if (49.0..=100.0).contains(&r2)
                    && pattern.is_none_or(|p| p.color(xx, yy) == Color::Green)
                {
                    ring.push(f64::from(pixels[yy * width + xx]));
                }
            }
        }
        let local = median(&mut ring);
        plane.measure(cx, cy, local, pixel_noise)
    };

    let mut stars: Vec<Star> = Vec::new();
    for rank in 0..per_cell {
        check_cancelled(cancel)?;
        for bucket in &buckets {
            let Some(&(_, x, y)) = bucket.get(rank) else {
                continue;
            };
            let (cx, cy) = (2.0 * x as f64 + 0.5, 2.0 * y as f64 + 0.5);
            if stars
                .iter()
                .any(|s| (s.x_px - cx).powi(2) + (s.y_px - cy).powi(2) < 144.0)
            {
                continue;
            }
            if let Some(star) = measure(x, y) {
                stars.push(star);
                if stars.len() == MAX_INSPECT_STARS {
                    break;
                }
            }
        }
        if stars.len() == MAX_INSPECT_STARS {
            break;
        }
    }

    // Global medians use the first CATALOG_STARS (250) stars to match FulgurStack identically.
    let catalog_slice = &stars[..stars.len().min(CATALOG_STARS)];
    let median_hfr_px = median_opt(catalog_slice.iter().map(|s| s.hfr_px));
    let median_fwhm_px = median_opt(catalog_slice.iter().map(|s| s.moment_fwhm_px));
    let median_eccentricity = median_opt(catalog_slice.iter().map(|s| s.eccentricity));
    let median_fwhm_arcsec = match (median_fwhm_px, pixel_scale_arcsec) {
        (Some(fwhm), Some(scale)) => Some(fwhm * scale),
        _ => None,
    };

    let channel_background_adu =
        pattern.map(|pat| channel_sky(frame_pixels, width, height, pat, sky.background));

    let tilt_map = SensorTiltMap::from_stars(&stars, width, height, median_fwhm_px, median_hfr_px);
    let field = crate::field::FieldAnalysis::from_stars(&stars, width, height, pixel_scale_arcsec);
    let eccentricity_map = crate::field::EccentricityMap::from_stars(&stars, width, height);

    Ok(StarQualityReport {
        width,
        height,
        plane_label: if pattern.is_some() {
            "Green CFA"
        } else {
            "Monochrome / Green"
        },
        background_adu: sky.background,
        channel_background_adu,
        noise_adu: sky.noise,
        detected_peaks,
        star_count: detected_stars,
        measured_star_count: catalog_slice.len(),
        suppressed_hot_pixels,
        refused_defect_detections,
        median_fwhm_px,
        median_fwhm_arcsec,
        median_hfr_px,
        median_eccentricity,
        pixel_scale_arcsec,
        stars,
        tilt_map,
        field,
        eccentricity_map,
        levels: Vec::new(),
        elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
        metrics_unavailable: None,
    })
}

/// Analyze a `RawFrame` directly (used by unit tests and CFA/Mono readers).
pub fn analyze_raw_frame(frame: &RawFrame, cancel: &AtomicBool) -> Result<StarQualityReport> {
    let width = frame.header.info.width;
    let height = frame.header.info.height;
    let pattern = match frame.header.info.mosaic {
        Mosaic::Bayer => frame.header.info.bayer_pattern,
        Mosaic::Mono | Mosaic::Rgb | Mosaic::Unknown => None,
    };
    let saturation = frame
        .header
        .info
        .saturation_adu
        .or_else(|| saturation_level(&frame.pixels, frame.header.info.bitpix));
    let scale = frame.header.pixel_scale_arcsec();
    let mut report = analyze_plane_stars(
        &frame.pixels,
        width,
        height,
        pattern,
        saturation,
        scale,
        cancel,
    )?;
    report.levels = crate::levels::plane_levels(frame);
    Ok(report)
}

/// Load and analyze any supported image file (`FITS`, `XISF`, `DSLR RAW`, or `TIFF/PNG/JPEG`)
/// for star FWHM/HFR/eccentricity and 3x3 sensor tilt.
pub fn analyze_frame_stars(
    path: &Path,
    hdu_index: usize,
    layout: LayoutChoice,
    cancel: &AtomicBool,
) -> Result<StarQualityReport> {
    check_cancelled(cancel)?;
    if fulgur_core::fits::is_astro_input(path) {
        let mut header = read_header_hdu(path, hdu_index)?;
        header.info.apply_layout(layout);
        let scale = header.pixel_scale_arcsec();
        if header.info.mosaic == Mosaic::Rgb {
            let w = header.info.width;
            let h = header.info.height;
            let rgb = read_rgb_roi(path, &header, [0, 0, w, h], cancel)?;
            let green: Vec<f32> = rgb.pixels.iter().map(|p| p[1]).collect();
            let sat = saturation_level(&green, header.info.bitpix);
            let mut report = analyze_plane_stars(&green, w, h, None, sat, scale, cancel)?;
            report.levels = crate::levels::rgb_levels(&rgb.pixels, w, h);
            return Ok(report);
        }
        let mut raw = read_raw_from_header_cancellable(path, header, cancel)?;
        raw.header.info.saturation_adu = saturation_level(&raw.pixels, raw.header.info.bitpix);
        return analyze_raw_frame(&raw, cancel);
    }

    if fulgur_core::raw::is_raw_image(path) {
        let stage2 = fulgur_core::raw::read_raw_stage2_linear(path, layout, cancel)?;
        return match stage2 {
            fulgur_core::raw::RawStage2Output::Cfa(mut raw) => {
                raw.header.info.saturation_adu =
                    saturation_level(&raw.pixels, raw.header.info.bitpix);
                analyze_raw_frame(&raw, cancel)
            }
            // Only the camera's embedded JPEG could be decoded. It is sharpened, white-balanced
            // and compressed, so FWHM / HFR / tilt measured on it would be misleading.
            fulgur_core::raw::RawStage2Output::LinearRgb(_) => {
                Err(Error::Invalid(RAW_PREVIEW_ONLY_STARS_MESSAGE))
            }
        };
    }

    let std_frame = fulgur_core::standard::read_standard(path, cancel)?;
    let max_v = std_frame
        .rgb
        .pixels
        .iter()
        .step_by(64)
        .fold(0.0_f32, |m, p| m.max(p[1]));
    let scale_adu = if max_v <= 2.0 { 65535.0 } else { 1.0 };
    let (w, h) = (std_frame.rgb.width, std_frame.rgb.height);
    let levels = if scale_adu == 1.0 {
        crate::levels::rgb_levels(&std_frame.rgb.pixels, w, h)
    } else {
        let scaled: Vec<[f32; 3]> = std_frame
            .rgb
            .pixels
            .iter()
            .map(|p| p.map(|v| v * scale_adu))
            .collect();
        crate::levels::rgb_levels(&scaled, w, h)
    };
    // ADR-022: 8-bit processed exports (JPEG, PNG, WebP, BMP, 8-bit TIFF) are stretched,
    // usually downscaled and clip bright stars, so photometry on them is misleading.
    // Skip it and keep only the per-channel levels.
    if std_frame.info.bitpix == 8 {
        return Ok(StarQualityReport::metrics_unavailable(
            w,
            h,
            levels,
            RENDERED_IMAGE_STARS_MESSAGE,
        ));
    }
    let green: Vec<f32> = std_frame
        .rgb
        .pixels
        .iter()
        .map(|p| p[1] * scale_adu)
        .collect();
    let mut report = analyze_plane_stars(&green, w, h, None, None, None, cancel)?;
    report.levels = levels;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Elongated stars at a known on-screen angle: the measured long-axis direction must
    /// match (counter-clockwise on screen, sensor y pointing down).
    #[test]
    fn elongation_angle_matches_star_orientation_on_screen() {
        let cancel = AtomicBool::new(false);
        let (w, h) = (640, 480);
        for screen_angle in [0.0_f64, 30.0, 90.0, 135.0] {
            let mut pixels = vec![1000.0_f32; w * h];
            for (i, p) in pixels.iter_mut().enumerate() {
                *p += ((i * 7919) % 97) as f32 / 97.0 * 6.0 - 3.0;
            }
            let t = screen_angle.to_radians();
            // Long axis in sensor coordinates (y down): (cos t, -sin t).
            let (ux, uy) = (t.cos(), -t.sin());
            let (s_major, s_minor) = (2.6, 1.4);
            for gy in 0..4 {
                for gx in 0..5 {
                    let cx = 80.0 + gx as f64 * 120.0 + 0.3;
                    let cy = 70.0 + gy as f64 * 110.0 + 0.2;
                    for yy in (cy as usize - 14)..=(cy as usize + 14) {
                        for xx in (cx as usize - 14)..=(cx as usize + 14) {
                            let (dx, dy) = (xx as f64 - cx, yy as f64 - cy);
                            let a = (dx * ux + dy * uy) / s_major;
                            let b = (-dx * uy + dy * ux) / s_minor;
                            pixels[yy * w + xx] += (9000.0 * (-0.5 * (a * a + b * b)).exp()) as f32;
                        }
                    }
                }
            }
            let report = analyze_plane_stars(&pixels, w, h, None, None, None, &cancel).unwrap();
            assert!(report.stars.len() >= 15, "angle {screen_angle}");
            for star in &report.stars {
                let diff = (star.elongation_angle_deg - screen_angle).rem_euclid(180.0);
                let diff = diff.min(180.0 - diff);
                assert!(
                    diff < 3.0,
                    "expected {screen_angle}, measured {}",
                    star.elongation_angle_deg
                );
                assert!(star.eccentricity > 0.6);
            }
        }
    }

    #[test]
    fn test_star_photometry_and_3x3_tilt_map_on_synthetic_field() {
        let cancel = AtomicBool::new(false);
        let (w, h) = (640, 480);
        let mut pixels = vec![1000.0_f32; w * h];
        // Add mild deterministic noise (-3..+3 ADU) and a 6x5 grid of Gaussian stars
        // across all 9 cells of the 3x3 sensor tilt map.
        for y in 0..h {
            for x in 0..w {
                let noise = ((x * 7919 + y * 104_729) % 97) as f32 / 97.0 * 6.0 - 3.0;
                pixels[y * w + x] += noise;
            }
        }
        for gy in 0..5 {
            for gx in 0..6 {
                let cx = 64.0 + gx as f64 * 100.0 + 0.3;
                let cy = 56.0 + gy as f64 * 90.0 + 0.2;
                // Slightly larger sigma in corner 3x3 cells to test 3x3 tilt/curvature measurement
                let in_left_or_right = cx < w as f64 / 3.0 || cx > w as f64 * 2.0 / 3.0;
                let in_top_or_bottom = cy < h as f64 / 3.0 || cy > h as f64 * 2.0 / 3.0;
                let is_corner = in_left_or_right && in_top_or_bottom;
                let sigma = if is_corner { 1.85 } else { 1.55 };
                for yy in (cy as usize - 12)..=(cy as usize + 12) {
                    for xx in (cx as usize - 12)..=(cx as usize + 12) {
                        let dx = (xx as f64 - cx) / sigma;
                        let dy = (yy as f64 - cy) / sigma;
                        pixels[yy * w + xx] += (9000.0 * (-0.5 * (dx * dx + dy * dy)).exp()) as f32;
                    }
                }
            }
        }
        // Inject 5 single-pixel hot pixels (50,000 ADU) to verify hot-pixel suppression
        for k in 0..5 {
            pixels[(80 + k * 40) * w + (90 + k * 50)] = 50_000.0;
        }

        let report = analyze_plane_stars(&pixels, w, h, None, None, Some(2.39), &cancel).unwrap();
        assert_eq!(report.star_count, 30);
        assert_eq!(report.measured_star_count, 30);
        assert!(report.suppressed_hot_pixels >= 5);
        let fwhm = report.median_fwhm_px.unwrap();
        let hfr = report.median_hfr_px.unwrap();
        let center_fwhm = report.tilt_map.cells[4].median_fwhm_px.unwrap();
        assert!(
            (center_fwhm - 2.355 * 1.55).abs() < 0.35,
            "Center FWHM={center_fwhm}"
        );
        assert!((3.4..4.6).contains(&fwhm), "FWHM={fwhm}");
        assert!((1.6..2.5).contains(&hfr), "HFR={hfr}");
        assert!(report.median_fwhm_arcsec.unwrap() > fwhm * 2.0);
        assert!(report.median_eccentricity.unwrap() < 0.25);

        // Verify all 9 zones of the 3x3 Tilt Map are populated and corners show positive delta
        for cell in &report.tilt_map.cells {
            assert!(cell.star_count >= 1, "Cell {} had 0 stars", cell.code);
            assert!(cell.median_fwhm_px.is_some());
        }
        assert!(report.tilt_map.corner_to_center_fwhm_pct.unwrap() > 8.0);

        // Verify live cursor star lookup
        let found = report
            .nearest_star(64.3, 56.2, 8.0)
            .expect("Should snap to star");
        assert!((found.x_px - 64.3).abs() < 0.5);
        assert!(found.radial_profile[0] > found.radial_profile[RADIAL_PROFILE_BINS - 1]);
    }
}
