//! Non-destructive MTF Auto-Stretch (Linked & Unlinked RGB), manual stretch,
//! overview generation, and bounded 1:1 native-resolution ROI rendering.

use crate::debayer::{debayer, fast_cfa_overview};
use crate::histogram::{FrameHistogram, HISTOGRAM_BINS, compute_bins_in_range};
use fulgur_core::RawFrame;
use fulgur_core::fits::saturation_level;
use fulgur_core::fits::{
    FrameInfo, is_astro_input, read_header_hdu, read_raw_from_header_cancellable, read_rgb_roi,
    read_roi,
};
use fulgur_core::raw::{
    RawStage, RawStage2Output, is_raw_image, read_raw_stage1_preview, read_raw_stage2_linear,
};
use fulgur_core::standard::{is_standard_image, read_standard};
use fulgur_core::{
    BayerPattern, DemosaicMethod, Error, HeaderCard, LayoutChoice, Mosaic, Result, RgbImage,
    check_cancelled,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

/// Maximum long edge of the full-frame overview texture.
pub const PREVIEW_EDGE: usize = 2048;

/// Number of samples used to estimate robust median & MAD for MTF stretch.
const STRETCH_SAMPLES: usize = 100_000;

/// Active display stretch mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StretchMode {
    /// Display raw normalized values without MTF curve (default for JPEG/PNG/sRGB TIFF).
    Linear,
    /// Shared black point and midtone across R, G, B (preserves calibrated star/nebula colors).
    AutoLinked,
    /// Independent black point and midtone per R, G, B channel (neutralizes strong sky/Bayer cast).
    AutoUnlinked,
    /// User-adjusted black point, midtone balance, and white point sliders.
    Manual,
}

/// Which channel(s) to display in the viewport.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelView {
    #[default]
    Rgb,
    Red,
    Green,
    Blue,
    Luminance,
}

impl ChannelView {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rgb => "RGB",
            Self::Red => "Red (R)",
            Self::Green => "Green (G)",
            Self::Blue => "Blue (B)",
            Self::Luminance => "Luminance (L)",
        }
    }
}

/// Parameters of the MTF (Midtones Transfer Function) display transform in normalized `[0, 1]` space.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct StretchParams {
    pub black: [f32; 3],
    pub mid: [f32; 3],
    pub white: [f32; 3],
    pub invert: bool,
    pub channel: ChannelView,
}

impl Default for StretchParams {
    fn default() -> Self {
        Self {
            black: [0.0; 3],
            mid: [0.5; 3],
            white: [1.0; 3],
            invert: false,
            channel: ChannelView::Rgb,
        }
    }
}

impl StretchParams {
    pub fn identity() -> Self {
        Self::default()
    }
}

/// Decoded overview frame ready for instant GPU upload, stretch toggling, and metadata inspection.
#[derive(Clone, Debug)]
pub struct OverviewFrame {
    pub width: usize,
    pub height: usize,
    pub native_size: [usize; 2],
    /// Overview pixels stored in physical source ADU / units.
    pub pixels: Vec<[f32; 3]>,
    /// Normalization minimum in physical ADU.
    pub norm_min: f32,
    /// Normalization span (`max - min` or sensor full well `65535.0` / `255.0` / `1.0`).
    pub norm_span: f32,
    /// Precomputed Linked RGB MTF parameters (shared across channels).
    pub linked_stretch: StretchParams,
    /// Precomputed Unlinked RGB MTF parameters (independent per channel).
    pub unlinked_stretch: StretchParams,
    /// Whether MTF Auto-Stretch should be enabled by default for this file format.
    pub default_stretch: bool,
    /// Parsed frame metadata summary.
    pub info: FrameInfo,
    /// Extracted header cards (FITS, XISF, or EXIF).
    pub cards: Vec<HeaderCard>,
    /// Available HDU names (for Multi-Extension FITS or multi-image XISF).
    pub hdu_names: Vec<String>,
    /// Active HDU index.
    pub hdu_index: usize,
    /// Short format badge (`"FITS"`, `"XISF"`, `"TIFF"`, `"PNG"`, `"JPEG"`, `"CR2"`, `"NEF"`, etc.).
    pub format_label: &'static str,
    /// Two-stage RAW decoding status (`NotRaw`, `Stage1Preview`, or `Stage2Linear`).
    pub raw_stage: RawStage,
    /// Precomputed linear signal histogram and "Zoom to Signal" bounds.
    pub histogram: FrameHistogram,
    /// Existing WCS parsed directly from FITS/XISF header on load (if present).
    pub header_wcs: Option<crate::sky::PlateSolveReport>,
    /// Decode + overview build latency in milliseconds.
    pub decode_ms: f64,
}

impl OverviewFrame {
    /// Return 256 histogram bins across `[min_norm, max_norm]`, reusing precomputed bins when matching
    /// `[0.0, 1.0]` or `signal_range`, and re-binning dynamically when custom sliders are dragged.
    pub fn bins_for_range(&self, min_norm: f32, max_norm: f32) -> [[u32; HISTOGRAM_BINS]; 4] {
        if (min_norm - 0.0).abs() < 1e-5 && (max_norm - 1.0).abs() < 1e-5 {
            return self.histogram.full_bins;
        }
        let [s0, s1] = self.histogram.signal_range;
        if (min_norm - s0).abs() < 1e-5 && (max_norm - s1).abs() < 1e-5 {
            return self.histogram.signal_bins;
        }
        compute_bins_in_range(
            &self.pixels,
            self.norm_min,
            self.norm_span,
            min_norm,
            max_norm,
        )
    }

    /// Returns the active `StretchParams` for the chosen `StretchMode`, manual overrides,
    /// invert flag, and channel selector.
    pub fn resolve_stretch(
        &self,
        mode: StretchMode,
        manual: &StretchParams,
        invert: bool,
        channel: ChannelView,
    ) -> StretchParams {
        let mut base = match mode {
            StretchMode::Linear => StretchParams::identity(),
            StretchMode::AutoLinked => self.linked_stretch,
            StretchMode::AutoUnlinked => self.unlinked_stretch,
            StretchMode::Manual => *manual,
        };
        base.invert = invert;
        base.channel = channel;
        base
    }

    /// Sample physical ADU `[R, G, B]` at native image coordinates `(native_x, native_y)`.
    pub fn sample_adu(&self, native_x: usize, native_y: usize) -> Option<[f32; 3]> {
        let [nw, nh] = self.native_size;
        if native_x >= nw || native_y >= nh || self.width == 0 || self.height == 0 {
            return None;
        }
        let ox = (native_x * self.width / nw).min(self.width - 1);
        let oy = (native_y * self.height / nh).min(self.height - 1);
        self.pixels.get(oy * self.width + ox).copied()
    }

    /// Render the overview into an 8-bit RGB buffer (`width * height * 3` bytes) for GPU upload.
    pub fn render_rgb8(&self, params: StretchParams) -> Vec<u8> {
        let mut out = vec![0_u8; self.pixels.len() * 3];
        let norm_min = self.norm_min;
        let inv_span = 1.0 / self.norm_span.max(1e-12);
        out.par_chunks_exact_mut(self.width.max(1) * 3)
            .enumerate()
            .for_each(|(y, row_bytes)| {
                let row_pixels = &self.pixels[y * self.width..(y + 1) * self.width];
                for (dst, &adu) in row_bytes.chunks_exact_mut(3).zip(row_pixels.iter()) {
                    let norm = [
                        ((adu[0] - norm_min) * inv_span).clamp(0.0, 1.0),
                        ((adu[1] - norm_min) * inv_span).clamp(0.0, 1.0),
                        ((adu[2] - norm_min) * inv_span).clamp(0.0, 1.0),
                    ];
                    let rgb8 = apply_stretch_pixel(norm, params);
                    dst.copy_from_slice(&rgb8);
                }
            });
        out
    }
}

/// Apply MTF stretch, channel selection, and optional inversion to a normalized `[0, 1]` pixel.
#[inline]
pub fn apply_stretch_pixel(norm_rgb: [f32; 3], params: StretchParams) -> [u8; 3] {
    let mut stretched = [0.0_f32; 3];
    for ch in 0..3 {
        let black = params.black[ch];
        let white = params.white[ch].max(black + 1e-6);
        let mid = params.mid[ch].clamp(1e-6, 1.0 - 1e-6);
        let x = ((norm_rgb[ch] - black) / (white - black)).clamp(0.0, 1.0);
        let denom = (2.0 * mid - 1.0) * x - mid;
        let mut y = if denom.abs() > 1e-12 {
            ((mid - 1.0) * x / denom).clamp(0.0, 1.0)
        } else {
            x
        };
        if params.invert {
            y = 1.0 - y;
        }
        stretched[ch] = y;
    }

    let final_rgb = match params.channel {
        ChannelView::Rgb => stretched,
        ChannelView::Red => [stretched[0]; 3],
        ChannelView::Green => [stretched[1]; 3],
        ChannelView::Blue => [stretched[2]; 3],
        ChannelView::Luminance => {
            let luma = 0.2126 * stretched[0] + 0.7152 * stretched[1] + 0.0722 * stretched[2];
            [luma; 3]
        }
    };

    [
        (final_rgb[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (final_rgb[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (final_rgb[2].clamp(0.0, 1.0) * 255.0).round() as u8,
    ]
}

/// Compute Unlinked (per-channel) and Linked (shared) MTF stretch parameters from normalized `[0, 1]` samples.
pub fn compute_auto_stretch(samples: &[[f32; 3]]) -> (StretchParams, StretchParams) {
    if samples.is_empty() {
        return (StretchParams::identity(), StretchParams::identity());
    }
    let target = 0.18_f32;
    let mut unlinked = StretchParams::identity();

    let mut ch_medians = [0.0_f32; 3];
    let mut ch_sigmas = [0.0_f32; 3];
    let mut sparse_channels = 0;

    for ch in 0..3 {
        let mut values: Vec<f32> = samples
            .iter()
            .map(|p| p[ch])
            .filter(|v| v.is_finite())
            .collect();
        if values.is_empty() {
            continue;
        }
        values.sort_unstable_by(f32::total_cmp);
        let median = values[values.len() / 2];
        let mut deviations: Vec<f32> = values.iter().map(|&v| (v - median).abs()).collect();
        deviations.sort_unstable_by(f32::total_cmp);
        let sigma = deviations[deviations.len() / 2] * 1.4826;
        ch_medians[ch] = median;
        ch_sigmas[ch] = sigma;

        // Sparse data (rejection / coverage-gap maps, masks): most samples share the minimum,
        // so median and sigma are zero and the MTF midtone collapses towards 0, turning every
        // non-zero pixel white. Stretch the non-zero part instead.
        if let Some((black, mid, white)) = sparse_stretch(&values, median, sigma) {
            unlinked.black[ch] = black;
            unlinked.mid[ch] = mid;
            unlinked.white[ch] = white;
            sparse_channels += 1;
            continue;
        }

        let black = (median - 2.8 * sigma).clamp(0.0, 0.99);
        let x = ((median - black) / (1.0 - black)).clamp(1e-6, 1.0 - 1e-6);
        let mid = (x * (target - 1.0)) / (2.0 * target * x - target - x);
        unlinked.black[ch] = black;
        unlinked.mid[ch] = mid.clamp(1e-6, 1.0 - 1e-6);
        unlinked.white[ch] = 1.0;
    }

    let linked_median = (ch_medians[0] + ch_medians[1] + ch_medians[2]) / 3.0;
    let linked_sigma = (ch_sigmas[0] + ch_sigmas[1] + ch_sigmas[2]) / 3.0;
    let linked_black = (linked_median - 2.8 * linked_sigma).clamp(0.0, 0.99);
    let lx = ((linked_median - linked_black) / (1.0 - linked_black)).clamp(1e-6, 1.0 - 1e-6);
    let linked_mid =
        ((lx * (target - 1.0)) / (2.0 * target * lx - target - lx)).clamp(1e-6, 1.0 - 1e-6);

    let linked = if sparse_channels > 0 {
        // Same sparse treatment for the linked variant: average the per-channel points.
        let mean = |v: [f32; 3]| [(v[0] + v[1] + v[2]) / 3.0; 3];
        StretchParams {
            black: mean(unlinked.black),
            mid: mean(unlinked.mid),
            white: mean(unlinked.white),
            invert: false,
            channel: ChannelView::Rgb,
        }
    } else {
        StretchParams {
            black: [linked_black; 3],
            mid: [linked_mid; 3],
            white: [1.0; 3],
            invert: false,
            channel: ChannelView::Rgb,
        }
    };

    (linked, unlinked)
}

/// Stretch for sparse data, where at least half the samples sit at the minimum (`median`
/// equals the minimum and the MAD sigma is zero): black at that base level, white at the
/// 99.5th percentile of the values above it, and the midtone chosen so the typical non-zero
/// value lands mid-grey. `values` must be sorted. `None` for ordinary images.
fn sparse_stretch(values: &[f32], median: f32, sigma: f32) -> Option<(f32, f32, f32)> {
    let base = *values.first()?;
    if sigma > 1e-9 || median > base {
        return None;
    }
    let above = &values[values.partition_point(|&v| v <= base)..];
    if above.is_empty() {
        return None; // constant image: identity stretch is fine
    }
    let black = base.clamp(0.0, 0.99);
    let white = above[((above.len() - 1) as f64 * 0.995) as usize]
        .max(black + 1e-6)
        .min(1.0);
    let typical = above[above.len() / 2];
    let x = ((typical - black) / (white - black)).clamp(1e-4, 1.0 - 1e-4);
    // MTF(m, x) = 0.5 exactly when m = x.
    Some((black, x, white))
}

fn determine_normalization(pixels: &[[f32; 3]], bitpix: i32) -> (f32, f32) {
    let mut min_v = f32::INFINITY;
    let mut max_v = f32::NEG_INFINITY;
    let step = (pixels.len() / STRETCH_SAMPLES).max(1);
    for p in pixels.iter().step_by(step) {
        for &c in p {
            if c.is_finite() {
                min_v = min_v.min(c);
                max_v = max_v.max(c);
            }
        }
    }
    if !min_v.is_finite() || !max_v.is_finite() {
        return (0.0, 65535.0);
    }
    match bitpix {
        8 | 16 if min_v >= 0.0 && max_v <= 65535.0 => (0.0, 65535.0),
        _ => {
            if min_v >= 0.0 && max_v <= 1.05 {
                (0.0, 1.0)
            } else if min_v >= 0.0 && max_v <= 65535.0 && max_v > 1.05 {
                (0.0, 65535.0)
            } else {
                let span = (max_v - min_v).max(1e-6);
                (min_v, span)
            }
        }
    }
}

fn downscale_rgb_image(image: &RgbImage, max_edge: usize) -> (usize, usize, Vec<[f32; 3]>) {
    if image.width <= max_edge && image.height <= max_edge {
        return (image.width, image.height, image.pixels.clone());
    }
    let factor = (image.width.max(image.height) as f64 / max_edge.max(1) as f64).max(1.0);
    let width = (image.width as f64 / factor)
        .round()
        .clamp(1.0, image.width as f64) as usize;
    let height = (image.height as f64 / factor)
        .round()
        .clamp(1.0, image.height as f64) as usize;
    let mut pixels = vec![[0.0_f32; 3]; width * height];
    pixels
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(oy, row)| {
            let y0 = (oy * image.height / height).min(image.height - 1);
            let y1 = ((oy + 1) * image.height / height)
                .max(y0 + 1)
                .min(image.height);
            for (ox, dst) in row.iter_mut().enumerate() {
                let x0 = (ox * image.width / width).min(image.width - 1);
                let x1 = ((ox + 1) * image.width / width)
                    .max(x0 + 1)
                    .min(image.width);
                let mut sum = [0.0_f64; 3];
                let mut count = 0_u32;
                for y in y0..y1 {
                    for p in &image.pixels[y * image.width + x0..y * image.width + x1] {
                        for c in 0..3 {
                            if p[c].is_finite() {
                                sum[c] += f64::from(p[c]);
                            }
                        }
                        count += 1;
                    }
                }
                let inv = 1.0 / f64::from(count.max(1));
                *dst = [
                    (sum[0] * inv) as f32,
                    (sum[1] * inv) as f32,
                    (sum[2] * inv) as f32,
                ];
            }
        });
    (width, height, pixels)
}

pub fn format_label_for_path(path: &Path) -> &'static str {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return "IMAGE";
    };
    match ext.to_ascii_lowercase().as_str() {
        "fit" | "fits" | "fts" => "FITS",
        "xisf" | "xish" => "XISF",
        "tif" | "tiff" => "TIFF",
        "jpg" | "jpeg" => "JPEG",
        "png" => "PNG",
        "webp" => "WebP",
        "bmp" => "BMP",
        "cr2" => "CR2",
        "cr3" => "CR3",
        "nef" => "NEF",
        "arw" => "ARW",
        "dng" => "DNG",
        "orf" => "ORF",
        "rw2" => "RW2",
        "pef" => "PEF",
        "raf" => "RAF",
        _ => "IMAGE",
    }
}

#[allow(clippy::too_many_arguments)]
fn build_overview_from_rgb(
    rgb: &RgbImage,
    info: FrameInfo,
    cards: Vec<HeaderCard>,
    hdu_names: Vec<String>,
    hdu_index: usize,
    default_stretch: bool,
    format_label: &'static str,
    raw_stage: RawStage,
    decode_ms: f64,
) -> OverviewFrame {
    let (norm_min, norm_span) = determine_normalization(&rgb.pixels, info.bitpix);
    let inv_span = 1.0 / norm_span.max(1e-12);
    let step = (rgb.pixels.len() / STRETCH_SAMPLES).max(1) | 1;
    let samples: Vec<[f32; 3]> = rgb
        .pixels
        .iter()
        .step_by(step)
        .map(|p| {
            [
                ((p[0] - norm_min) * inv_span).clamp(0.0, 1.0),
                ((p[1] - norm_min) * inv_span).clamp(0.0, 1.0),
                ((p[2] - norm_min) * inv_span).clamp(0.0, 1.0),
            ]
        })
        .collect();
    let (linked_stretch, unlinked_stretch) = compute_auto_stretch(&samples);
    let (width, height, pixels) = downscale_rgb_image(rgb, PREVIEW_EDGE);
    let histogram = FrameHistogram::compute(&pixels, norm_min, norm_span);
    OverviewFrame {
        width,
        height,
        native_size: [rgb.width, rgb.height],
        pixels,
        norm_min,
        norm_span,
        linked_stretch,
        unlinked_stretch,
        default_stretch,
        info,
        cards,
        hdu_names,
        hdu_index,
        format_label,
        raw_stage,
        histogram,
        header_wcs: None,
        decode_ms,
    }
}

fn build_overview_from_raw_cfa(
    raw: &fulgur_core::RawFrame,
    format_label: &'static str,
    raw_stage: RawStage,
    decode_ms: f64,
    cancel: &AtomicBool,
) -> Result<OverviewFrame> {
    let overview_rgb = fast_cfa_overview(raw, PREVIEW_EDGE, cancel)?;
    let native_size = [raw.header.info.width, raw.header.info.height];
    let bitpix = raw.header.info.bitpix;
    let (norm_min, norm_span) = determine_normalization(&overview_rgb.pixels, bitpix);
    let inv_span = 1.0 / norm_span.max(1e-12);

    let sample_rgb = sample_raw_quads_for_stretch(raw, norm_min, inv_span);
    let (linked_stretch, unlinked_stretch) = compute_auto_stretch(&sample_rgb);
    let histogram = FrameHistogram::compute(&overview_rgb.pixels, norm_min, norm_span);
    let header_wcs = crate::sky::build_header_wcs_report(&raw.header);
    let cards = raw.header.header_cards(false);
    Ok(OverviewFrame {
        width: overview_rgb.width,
        height: overview_rgb.height,
        native_size,
        pixels: overview_rgb.pixels,
        norm_min,
        norm_span,
        linked_stretch,
        unlinked_stretch,
        default_stretch: true,
        info: raw.header.info.clone(),
        cards,
        hdu_names: raw.header.hdu_names.clone(),
        hdu_index: raw.header.hdu_index,
        format_label,
        raw_stage,
        histogram,
        header_wcs,
        decode_ms,
    })
}

/// Stage 1 RAW Loader: Instantly extracts the embedded preview JPEG (`< 20 ms`) and camera EXIF/TIFF metadata.
pub fn load_raw_stage1_overview(
    path: &Path,
    layout: LayoutChoice,
    cancel: &AtomicBool,
) -> Result<OverviewFrame> {
    let start = Instant::now();
    let std_frame = read_raw_stage1_preview(path, layout, cancel)?;
    let decode_ms = start.elapsed().as_secs_f64() * 1000.0;
    Ok(build_overview_from_rgb(
        &std_frame.rgb,
        std_frame.info,
        std_frame.cards,
        vec!["#0 Embedded Preview".to_owned()],
        0,
        std_frame.default_stretch,
        format_label_for_path(path),
        RawStage::Stage1Preview,
        decode_ms,
    ))
}

/// Stage 2 RAW Loader: Decodes the linear sensor CFA plane in the background and builds the linear astro overview.
pub fn load_raw_stage2_overview(
    path: &Path,
    layout: LayoutChoice,
    cancel: &AtomicBool,
) -> Result<OverviewFrame> {
    load_raw_stage2_overview_with_source(path, layout, cancel).map(|(frame, _)| frame)
}

/// Like [`load_raw_stage2_overview`], but also returns the decoded linear CFA plane (when the
/// sensor data was decoded) so star analysis and plate solving can reuse it.
pub fn load_raw_stage2_overview_with_source(
    path: &Path,
    layout: LayoutChoice,
    cancel: &AtomicBool,
) -> Result<(OverviewFrame, Option<Arc<RawFrame>>)> {
    let start = Instant::now();
    let stage2 = read_raw_stage2_linear(path, layout, cancel)?;
    let decode_ms = start.elapsed().as_secs_f64() * 1000.0;
    let format_label = format_label_for_path(path);
    match stage2 {
        RawStage2Output::Cfa(raw) => {
            let raw = prepare_source(raw);
            let frame = build_overview_from_raw_cfa(
                &raw,
                format_label,
                RawStage::Stage2Linear,
                decode_ms,
                cancel,
            )?;
            Ok((frame, Some(raw)))
        }
        RawStage2Output::LinearRgb(std_frame) => Ok((
            build_overview_from_rgb(
                &std_frame.rgb,
                std_frame.info,
                std_frame.cards,
                vec!["#0 Embedded Preview (sensor data not decoded)".to_owned()],
                0,
                true,
                format_label,
                RawStage::Stage2PreviewOnly,
                decode_ms,
            ),
            None,
        )),
    }
}

/// Finish a decoded single-plane frame for sharing: record its saturation level (needed by
/// star analysis) and wrap it for reuse by the overview, star and solve stages.
fn prepare_source(mut raw: RawFrame) -> Arc<RawFrame> {
    raw.header.info.saturation_adu = saturation_level(&raw.pixels, raw.header.info.bitpix);
    Arc::new(raw)
}

/// Load a fast `<= 2048 px` overview and full metadata header from any supported file.
pub fn load_frame_overview(
    path: &Path,
    hdu_index: usize,
    layout: LayoutChoice,
    cancel: &AtomicBool,
) -> Result<OverviewFrame> {
    load_frame_overview_with_source(path, hdu_index, layout, cancel).map(|(frame, _)| frame)
}

/// Like [`load_frame_overview`], but also returns the decoded single-plane (mono / Bayer CFA)
/// source frame, so star analysis and plate solving reuse it instead of reading and decoding
/// the file again. RGB and standard images return `None` (they keep the path-based path).
pub fn load_frame_overview_with_source(
    path: &Path,
    hdu_index: usize,
    layout: LayoutChoice,
    cancel: &AtomicBool,
) -> Result<(OverviewFrame, Option<Arc<RawFrame>>)> {
    let start = Instant::now();
    check_cancelled(cancel)?;
    let format_label = format_label_for_path(path);

    if is_astro_input(path) {
        let mut header = read_header_hdu(path, hdu_index)?;
        header.info.apply_layout(layout);
        if header.info.mosaic == Mosaic::Rgb {
            let (w, h) = (header.info.width, header.info.height);
            let rgb = read_rgb_roi(path, &header, [0, 0, w, h], cancel)?;
            let header_wcs = crate::sky::build_header_wcs_report(&header);
            let cards = header.header_cards(false);
            let decode_ms = start.elapsed().as_secs_f64() * 1000.0;
            let mut ov = build_overview_from_rgb(
                &rgb,
                header.info,
                cards,
                header.hdu_names,
                header.hdu_index,
                true,
                format_label,
                RawStage::NotRaw,
                decode_ms,
            );
            ov.header_wcs = header_wcs;
            return Ok((ov, None));
        }

        let raw = read_raw_from_header_cancellable(path, header, cancel)?;
        check_cancelled(cancel)?;
        let decode_ms = start.elapsed().as_secs_f64() * 1000.0;
        let raw = prepare_source(raw);
        let frame =
            build_overview_from_raw_cfa(&raw, format_label, RawStage::NotRaw, decode_ms, cancel)?;
        return Ok((frame, Some(raw)));
    }

    if is_raw_image(path) {
        return load_raw_stage2_overview_with_source(path, layout, cancel);
    }

    if is_standard_image(path) {
        let std_frame = read_standard(path, cancel)?;
        let decode_ms = start.elapsed().as_secs_f64() * 1000.0;
        return Ok((
            build_overview_from_rgb(
                &std_frame.rgb,
                std_frame.info,
                std_frame.cards,
                vec!["#0 Primary".to_owned()],
                0,
                std_frame.default_stretch,
                format_label,
                RawStage::NotRaw,
                decode_ms,
            ),
            None,
        ));
    }

    Err(Error::Invalid("Unsupported image file extension."))
}

fn sample_raw_quads_for_stretch(
    raw: &fulgur_core::RawFrame,
    norm_min: f32,
    inv_span: f32,
) -> Vec<[f32; 3]> {
    let width = raw.header.info.width;
    let height = raw.header.info.height;
    if width < 2 || height < 2 {
        return raw
            .pixels
            .iter()
            .map(|&v| [((v - norm_min) * inv_span).clamp(0.0, 1.0); 3])
            .collect();
    }
    let quads_w = width / 2;
    let quads_h = height / 2;
    let total_quads = (quads_w * quads_h).max(1);
    let step = (total_quads / STRETCH_SAMPLES).max(1) | 1;
    let mut samples = Vec::with_capacity(total_quads / step + 1);
    let pattern = raw.header.info.bayer_pattern;

    let mut q_idx = 0;
    for qy in 0..quads_h {
        let y = qy * 2;
        for qx in 0..quads_w {
            if q_idx % step == 0 {
                let x = qx * 2;
                let p00 = raw.pixels[y * width + x];
                let p10 = raw.pixels[y * width + x + 1];
                let p01 = raw.pixels[(y + 1) * width + x];
                let p11 = raw.pixels[(y + 1) * width + x + 1];
                let rgb = match (raw.header.info.mosaic, pattern) {
                    (Mosaic::Bayer, Some(BayerPattern::RGGB)) => [p00, 0.5 * (p10 + p01), p11],
                    (Mosaic::Bayer, Some(BayerPattern::BGGR)) => [p11, 0.5 * (p10 + p01), p00],
                    (Mosaic::Bayer, Some(BayerPattern::GRBG)) => [p10, 0.5 * (p00 + p11), p01],
                    (Mosaic::Bayer, Some(BayerPattern::GBRG)) => [p01, 0.5 * (p00 + p11), p10],
                    _ => {
                        let m = 0.25 * (p00 + p10 + p01 + p11);
                        [m, m, m]
                    }
                };
                samples.push([
                    ((rgb[0] - norm_min) * inv_span).clamp(0.0, 1.0),
                    ((rgb[1] - norm_min) * inv_span).clamp(0.0, 1.0),
                    ((rgb[2] - norm_min) * inv_span).clamp(0.0, 1.0),
                ]);
            }
            q_idx += 1;
        }
    }
    samples
}

/// Load a native 1:1 resolution ROI `[x, y, width, height]` with AVX2 VNG demosaicing
/// (for FITS/XISF Bayer CFA frames) and render it with the given stretch parameters.
#[allow(clippy::too_many_arguments)]
pub fn load_detail_roi_rgb8(
    path: &Path,
    hdu_index: usize,
    layout: LayoutChoice,
    expected_size: [usize; 2],
    bounds: [usize; 4],
    norm_min: f32,
    norm_span: f32,
    params: StretchParams,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    let [x, y, width, height] = bounds;
    if width == 0
        || height == 0
        || x.checked_add(width)
            .is_none_or(|end| end > expected_size[0])
        || y.checked_add(height)
            .is_none_or(|end| end > expected_size[1])
    {
        return Err(Error::Invalid("Invalid detail ROI bounds."));
    }
    let inv_span = 1.0 / norm_span.max(1e-12);

    if is_astro_input(path) {
        let mut header = read_header_hdu(path, hdu_index)?;
        header.info.apply_layout(layout);
        if [header.info.width, header.info.height] != expected_size {
            return Err(Error::Invalid("Source astro image dimensions changed."));
        }
        let left = x.saturating_sub(8) & !1;
        let top = y.saturating_sub(8) & !1;
        let right = (x + width + 8).min(expected_size[0]);
        let bottom = (y + height + 8).min(expected_size[1]);
        let roi = [left, top, right - left, bottom - top];
        let rgb = if header.info.mosaic == Mosaic::Rgb {
            read_rgb_roi(path, &header, roi, cancel)?
        } else {
            let raw = read_roi(path, &header, roi, cancel)?;
            debayer(&raw, cancel, DemosaicMethod::Vng)?.0
        };
        let mut bytes = Vec::with_capacity(width * height * 3);
        for row in 0..height {
            check_cancelled(cancel)?;
            for col in 0..width {
                let adu = rgb.pixels[(y + row - top) * rgb.width + (x + col - left)];
                let norm = [
                    ((adu[0] - norm_min) * inv_span).clamp(0.0, 1.0),
                    ((adu[1] - norm_min) * inv_span).clamp(0.0, 1.0),
                    ((adu[2] - norm_min) * inv_span).clamp(0.0, 1.0),
                ];
                bytes.extend_from_slice(&apply_stretch_pixel(norm, params));
            }
        }
        return Ok(bytes);
    }

    if is_raw_image(path) {
        let stage2 = read_raw_stage2_linear(path, layout, cancel)?;
        match stage2 {
            RawStage2Output::Cfa(full_raw) => {
                if [full_raw.header.info.width, full_raw.header.info.height] != expected_size {
                    return Err(Error::Invalid("Source RAW dimensions changed."));
                }
                let left = x.saturating_sub(8) & !1;
                let top = y.saturating_sub(8) & !1;
                let right = (x + width + 8).min(expected_size[0]);
                let bottom = (y + height + 8).min(expected_size[1]);
                let rw = right - left;
                let rh = bottom - top;
                let mut roi_pixels = Vec::with_capacity(rw * rh);
                for ry in top..bottom {
                    let row_start = ry * expected_size[0] + left;
                    roi_pixels.extend_from_slice(&full_raw.pixels[row_start..row_start + rw]);
                }
                let mut roi_header = full_raw.header;
                roi_header.info.width = rw;
                roi_header.info.height = rh;
                let roi_raw = fulgur_core::RawFrame {
                    header: roi_header,
                    pixels: roi_pixels,
                };
                let rgb = debayer(&roi_raw, cancel, DemosaicMethod::Vng)?.0;
                let mut bytes = Vec::with_capacity(width * height * 3);
                for row in 0..height {
                    check_cancelled(cancel)?;
                    for col in 0..width {
                        let adu = rgb.pixels[(y + row - top) * rgb.width + (x + col - left)];
                        let norm = [
                            ((adu[0] - norm_min) * inv_span).clamp(0.0, 1.0),
                            ((adu[1] - norm_min) * inv_span).clamp(0.0, 1.0),
                            ((adu[2] - norm_min) * inv_span).clamp(0.0, 1.0),
                        ];
                        bytes.extend_from_slice(&apply_stretch_pixel(norm, params));
                    }
                }
                return Ok(bytes);
            }
            RawStage2Output::LinearRgb(std_img) => {
                let rgb = std_img.rgb;
                if [rgb.width, rgb.height] != expected_size {
                    return Err(Error::Invalid("Source RAW dimensions changed."));
                }
                let mut bytes = Vec::with_capacity(width * height * 3);
                for row in y..y + height {
                    check_cancelled(cancel)?;
                    for col in x..x + width {
                        let adu = rgb.pixels[row * rgb.width + col];
                        let norm = [
                            ((adu[0] - norm_min) * inv_span).clamp(0.0, 1.0),
                            ((adu[1] - norm_min) * inv_span).clamp(0.0, 1.0),
                            ((adu[2] - norm_min) * inv_span).clamp(0.0, 1.0),
                        ];
                        bytes.extend_from_slice(&apply_stretch_pixel(norm, params));
                    }
                }
                return Ok(bytes);
            }
        }
    }

    if is_standard_image(path) {
        let std_img = read_standard(path, cancel)?;
        let rgb = std_img.rgb;
        if [rgb.width, rgb.height] != expected_size {
            return Err(Error::Invalid("Source image dimensions changed."));
        }
        let mut bytes = Vec::with_capacity(width * height * 3);
        for row in y..y + height {
            check_cancelled(cancel)?;
            for col in x..x + width {
                let adu = rgb.pixels[row * rgb.width + col];
                let norm = [
                    ((adu[0] - norm_min) * inv_span).clamp(0.0, 1.0),
                    ((adu[1] - norm_min) * inv_span).clamp(0.0, 1.0),
                    ((adu[2] - norm_min) * inv_span).clamp(0.0, 1.0),
                ];
                bytes.extend_from_slice(&apply_stretch_pixel(norm, params));
            }
        }
        return Ok(bytes);
    }

    Err(Error::Invalid("Unsupported image file extension."))
}

#[cfg(test)]
mod sparse_tests {
    use super::*;

    #[test]
    fn sparse_map_keeps_zero_black_and_typical_value_mid_grey() {
        // 87 % zeros, the rest mostly one level (like a high-rejection map).
        let mut samples = vec![[0.0_f32; 3]; 8700];
        samples.extend(std::iter::repeat_n([0.00215_f32; 3], 1100));
        samples.extend((0..200).map(|i| [0.003 + i as f32 * 1e-4; 3]));
        let (linked, unlinked) = compute_auto_stretch(&samples);
        for p in [linked, unlinked] {
            assert_eq!(p.black[0], 0.0);
            assert!(p.mid[0] > 0.01, "midtone must not collapse: {}", p.mid[0]);
            assert!(p.white[0] > 0.00215 && p.white[0] < 0.03, "{}", p.white[0]);
            assert_eq!(
                apply_stretch_pixel([0.0; 3], p),
                [0, 0, 0],
                "zero stays black"
            );
            let typical = apply_stretch_pixel([0.00215; 3], p)[0];
            assert!(
                (100..=160).contains(&typical),
                "typical value mid-grey: {typical}"
            );
            // Averaged overview pixels (one rejected pixel in nine) stay dark grey, not white.
            let averaged = apply_stretch_pixel([0.00215 / 9.0; 3], p)[0];
            assert!(averaged < 100, "{averaged}");
        }
    }

    #[test]
    fn ordinary_images_are_unchanged() {
        let samples: Vec<[f32; 3]> = (0..1000)
            .map(|i| [0.1 + (i % 37) as f32 * 1e-4; 3])
            .collect();
        let (_, unlinked) = compute_auto_stretch(&samples);
        assert!(unlinked.black[0] > 0.09 && unlinked.white[0] == 1.0);
    }
}
