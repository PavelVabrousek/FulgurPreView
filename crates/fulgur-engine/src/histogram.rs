//! Linear Signal Histogram engine with automatic "Zoom to Signal" peak framing,
//! robust per-channel statistics (Median, MAD Sigma, Clipping %), and dynamic
//! 256-bin re-binning across arbitrary `[Min, Max]` slider ranges.

use serde::{Deserialize, Serialize};

/// Number of horizontal bins rendered in the histogram plot.
pub const HISTOGRAM_BINS: usize = 256;

/// Maximum samples used when computing robust median/MAD statistics.
const HISTOGRAM_STAT_SAMPLES: usize = 120_000;

/// Robust linear statistics for one channel (`Red`, `Green`, `Blue`, or `Luminance`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChannelStats {
    pub min_norm: f32,
    pub max_norm: f32,
    pub median_norm: f32,
    pub sigma_norm: f32,
    pub min_adu: f32,
    pub max_adu: f32,
    pub median_adu: f32,
    pub sigma_adu: f32,
    pub clipped_low_pct: f32,
    pub clipped_high_pct: f32,
}

/// Precomputed linear histogram and signal-zoom metadata for an `OverviewFrame`.
#[derive(Clone, Debug)]
pub struct FrameHistogram {
    /// Per-channel statistics: `[0] = Red`, `[1] = Green`, `[2] = Blue`, `[3] = Luminance`.
    pub stats: [ChannelStats; 4],
    /// Automatic "Zoom to Signal" X-axis bounds `[min_norm, max_norm]` in `[0.0, 1.0]`.
    pub signal_range: [f32; 2],
    /// 256-bin counts over the full `[0.0, 1.0]` normalized range (`[R, G, B, L]`).
    pub full_bins: [[u32; HISTOGRAM_BINS]; 4],
    /// 256-bin counts over `signal_range` (`[R, G, B, L]`) for high-resolution peak display.
    pub signal_bins: [[u32; HISTOGRAM_BINS]; 4],
}

impl Default for FrameHistogram {
    fn default() -> Self {
        Self {
            stats: [ChannelStats::default(); 4],
            signal_range: [0.0, 1.0],
            full_bins: [[0; HISTOGRAM_BINS]; 4],
            signal_bins: [[0; HISTOGRAM_BINS]; 4],
        }
    }
}

impl FrameHistogram {
    /// Compute the linear histogram, per-channel statistics, and automatic "Zoom to Signal"
    /// window from an overview frame's physical ADU pixels.
    pub fn compute(pixels: &[[f32; 3]], norm_min: f32, norm_span: f32) -> Self {
        if pixels.is_empty() {
            return Self::default();
        }
        let span = norm_span.max(1e-12);
        let inv_span = 1.0 / span;
        let step = (pixels.len() / HISTOGRAM_STAT_SAMPLES).max(1);
        let cap = pixels.len() / step + 1;

        let mut ch_samples: [Vec<f32>; 4] = [
            Vec::with_capacity(cap),
            Vec::with_capacity(cap),
            Vec::with_capacity(cap),
            Vec::with_capacity(cap),
        ];

        for p in pixels.iter().step_by(step) {
            if !p[0].is_finite() || !p[1].is_finite() || !p[2].is_finite() {
                continue;
            }
            let r = ((p[0] - norm_min) * inv_span).clamp(0.0, 1.0);
            let g = ((p[1] - norm_min) * inv_span).clamp(0.0, 1.0);
            let b = ((p[2] - norm_min) * inv_span).clamp(0.0, 1.0);
            let l = (0.2126 * r + 0.7152 * g + 0.0722 * b).clamp(0.0, 1.0);
            ch_samples[0].push(r);
            ch_samples[1].push(g);
            ch_samples[2].push(b);
            ch_samples[3].push(l);
        }

        let mut stats = [ChannelStats::default(); 4];
        for ch in 0..4 {
            let vals = &mut ch_samples[ch];
            if vals.is_empty() {
                continue;
            }
            let total = vals.len() as f32;
            let low_count = vals.iter().filter(|&&v| v <= 1e-6).count() as f32;
            let high_count = vals.iter().filter(|&&v| v >= 1.0 - 1e-6).count() as f32;

            vals.sort_unstable_by(f32::total_cmp);
            let min_norm = vals[0];
            let max_norm = *vals.last().unwrap_or(&1.0);
            let median_norm = vals[vals.len() / 2];

            let mut devs: Vec<f32> = vals.iter().map(|&v| (v - median_norm).abs()).collect();
            devs.sort_unstable_by(f32::total_cmp);
            let sigma_norm = devs[devs.len() / 2] * 1.4826;

            stats[ch] = ChannelStats {
                min_norm,
                max_norm,
                median_norm,
                sigma_norm,
                min_adu: norm_min + min_norm * span,
                max_adu: norm_min + max_norm * span,
                median_adu: norm_min + median_norm * span,
                sigma_adu: sigma_norm * span,
                clipped_low_pct: (low_count / total) * 100.0,
                clipped_high_pct: (high_count / total) * 100.0,
            };
        }

        // Compute automatic "Zoom to Signal" bounds around the sky-background and nebulosity peak:
        // [min_channel_median - 6*sigma .. max_channel_median + 24*sigma]
        let min_med = stats[0]
            .median_norm
            .min(stats[1].median_norm)
            .min(stats[2].median_norm);
        let max_med = stats[0]
            .median_norm
            .max(stats[1].median_norm)
            .max(stats[2].median_norm);
        let max_sig = stats[0]
            .sigma_norm
            .max(stats[1].sigma_norm)
            .max(stats[2].sigma_norm)
            .max(1e-5);
        let global_min = stats[0]
            .min_norm
            .min(stats[1].min_norm)
            .min(stats[2].min_norm);

        let sig_min = (min_med - 6.0 * max_sig)
            .max(global_min * 0.90)
            .clamp(0.0, 0.96);
        let sig_max = (max_med + 24.0 * max_sig).clamp(sig_min + 0.0005, 1.0);
        let signal_range = [sig_min, sig_max];

        let full_bins = compute_bins_in_range(pixels, norm_min, span, 0.0, 1.0);
        let signal_bins =
            compute_bins_in_range(pixels, norm_min, span, signal_range[0], signal_range[1]);

        Self {
            stats,
            signal_range,
            full_bins,
            signal_bins,
        }
    }
}

/// Bin `pixels` into `HISTOGRAM_BINS` (256) buckets across the normalized X-axis window `[range_min, range_max]`.
/// Values outside `[range_min, range_max]` are excluded from the zoomed plot so the active signal region fills the curve cleanly.
pub fn compute_bins_in_range(
    pixels: &[[f32; 3]],
    norm_min: f32,
    norm_span: f32,
    range_min: f32,
    range_max: f32,
) -> [[u32; HISTOGRAM_BINS]; 4] {
    let mut bins = [[0_u32; HISTOGRAM_BINS]; 4];
    if pixels.is_empty() {
        return bins;
    }
    let inv_span = 1.0 / norm_span.max(1e-12);
    let r_min = range_min.clamp(0.0, 1.0);
    let r_span = (range_max.clamp(0.0, 1.0) - r_min).max(1e-7);
    let scale = (HISTOGRAM_BINS as f32 - 1.0) / r_span;

    // Sample up to ~250k pixels so re-binning during live slider dragging takes < 0.2 ms
    let step = (pixels.len() / 250_000).max(1);
    for p in pixels.iter().step_by(step) {
        if !p[0].is_finite() || !p[1].is_finite() || !p[2].is_finite() {
            continue;
        }
        let r = ((p[0] - norm_min) * inv_span).clamp(0.0, 1.0);
        let g = ((p[1] - norm_min) * inv_span).clamp(0.0, 1.0);
        let b = ((p[2] - norm_min) * inv_span).clamp(0.0, 1.0);
        let l = (0.2126 * r + 0.7152 * g + 0.0722 * b).clamp(0.0, 1.0);
        let vals = [r, g, b, l];
        for ch in 0..4 {
            let v = vals[ch];
            if v >= r_min && v <= r_min + r_span {
                let idx = ((v - r_min) * scale).round() as usize;
                bins[ch][idx.min(HISTOGRAM_BINS - 1)] += 1;
            }
        }
    }
    bins
}

/// Evaluate the MTF curve `y = MTF(x)` for a normalized input `x` in `[0, 1]` on channel `ch`.
#[inline]
pub fn evaluate_mtf_curve(x_norm: f32, black: f32, mid: f32, white: f32, invert: bool) -> f32 {
    let w = white.max(black + 1e-6);
    let m = mid.clamp(1e-6, 1.0 - 1e-6);
    let x = ((x_norm - black) / (w - black)).clamp(0.0, 1.0);
    let denom = (2.0 * m - 1.0) * x - m;
    let mut y = if denom.abs() > 1e-12 {
        ((m - 1.0) * x / denom).clamp(0.0, 1.0)
    } else {
        x
    };
    if invert {
        y = 1.0 - y;
    }
    y
}
