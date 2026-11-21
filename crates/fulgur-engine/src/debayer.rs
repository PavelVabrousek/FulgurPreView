//! Fast CFA demosaicing: 2x2 superpixel/MHC overview and full-detail MHC/VNG reconstruction.
//! Independently implemented MHC equations from Malvar, He & Cutler (2004).

use fulgur_core::{
    BayerPattern, Color, DemosaicMethod, Error, Mosaic, RawFrame, Result, RgbImage, check_cancelled,
};
use rayon::prelude::*;
use serde::Serialize;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
pub struct DebayerStats {
    pub demosaic_method: DemosaicMethod,
    pub cosmetic_seconds: f64,
    pub demosaic_seconds: f64,
    pub corrected_pixels: usize,
}

fn median8(values: &mut [f32; 8]) -> f32 {
    values.sort_unstable_by(f32::total_cmp);
    (values[3] + values[4]) * 0.5
}

#[inline]
fn neighbors(pixels: &[f32], width: usize, x: usize, y: usize) -> [f32; 8] {
    let above = (y - 2) * width + x;
    let center = y * width + x;
    let below = (y + 2) * width + x;
    [
        pixels[above - 2],
        pixels[above],
        pixels[above + 2],
        pixels[center - 2],
        pixels[center + 2],
        pixels[below - 2],
        pixels[below],
        pixels[below + 2],
    ]
}

#[inline]
fn local_noise(neighbors: [f32; 8], median: f32) -> f32 {
    let mut deviations = neighbors.map(|value| (value - median).abs());
    (1.4826 * median8(&mut deviations)).max(1.0)
}

fn neighborhood(pixels: &[f32], width: usize, x: usize, y: usize) -> (f32, f32) {
    let mut neighbors = neighbors(pixels, width, x, y);
    let median = median8(&mut neighbors);
    (median, local_noise(neighbors, median))
}

fn has_stellar_support(
    pixels: &[f32],
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    candidate_excess: f32,
) -> bool {
    let mut supported = 0;
    for (dx, dy) in [(-1_isize, 0_isize), (1, 0), (0, -1), (0, 1)] {
        let nx = (x as isize + dx) as usize;
        let ny = (y as isize + dy) as usize;
        if nx < 2 || ny < 2 || nx + 2 >= width || ny + 2 >= height {
            continue;
        }
        let (local, noise) = neighborhood(pixels, width, nx, ny);
        let threshold = (3.0 * noise).min(0.125 * candidate_excess);
        if pixels[ny * width + nx] > local + threshold {
            supported += 1;
            if supported == 2 {
                return true;
            }
        }
    }
    false
}

fn refined_noise(pixels: &[f32], width: usize, x: usize, y: usize) -> f32 {
    let mut neighbors = [0.0_f32; 24];
    let mut index = 0;
    for dy in [-4_isize, -2, 0, 2, 4] {
        for dx in [-4_isize, -2, 0, 2, 4] {
            if dx != 0 || dy != 0 {
                neighbors[index] =
                    pixels[(y as isize + dy) as usize * width + (x as isize + dx) as usize];
                index += 1;
            }
        }
    }
    neighbors.sort_unstable_by(f32::total_cmp);
    let median = 0.5 * (neighbors[11] + neighbors[12]);
    let mut deviations = neighbors.map(|value| (value - median).abs());
    deviations.sort_unstable_by(f32::total_cmp);
    (1.4826 * 0.5 * (deviations[11] + deviations[12])).max(1.0)
}

pub fn cosmetic_correct(frame: &RawFrame, cancel: &AtomicBool) -> Result<(Vec<f32>, usize)> {
    cosmetic_correct_into(frame, cancel, Vec::new())
}

pub(crate) fn cosmetic_correct_into(
    frame: &RawFrame,
    cancel: &AtomicBool,
    cleaned: Vec<f32>,
) -> Result<(Vec<f32>, usize)> {
    cosmetic_correct_spans(frame, cancel, cleaned, None)
}

fn cosmetic_correct_spans(
    frame: &RawFrame,
    cancel: &AtomicBool,
    mut cleaned: Vec<f32>,
    spans: Option<&[[usize; 2]]>,
) -> Result<(Vec<f32>, usize)> {
    let width = frame.header.info.width;
    let height = frame.header.info.height;
    if width.checked_mul(height) != Some(frame.pixels.len()) || width < 6 || height < 6 {
        return Err(Error::Invalid("Invalid CFA buffer dimensions."));
    }
    cleaned.clear();
    cleaned.extend_from_slice(&frame.pixels);
    let kernel = cosmetic_simd::Kernel::detect();
    let corrected = cleaned
        .par_chunks_mut(width)
        .enumerate()
        .map(|(y, row)| {
            check_cancelled(cancel)?;
            if y < 4 || y + 4 >= height || width <= 8 {
                return Ok(0);
            }
            let [from, to] = spans.map_or([0, width], |spans| spans[y]);
            let (from, to) = (from.max(4), to.min(width - 4));
            if from >= to {
                return Ok(0);
            }
            Ok(kernel.row(frame, y, row, from, to))
        })
        .collect::<Result<Vec<usize>>>()?
        .into_iter()
        .sum();
    Ok((cleaned, corrected))
}

#[path = "cosmetic_simd.rs"]
mod cosmetic_simd;

pub(crate) fn bayer_pattern(frame: &RawFrame) -> Result<BayerPattern> {
    match (frame.header.info.mosaic, frame.header.info.bayer_pattern) {
        (Mosaic::Bayer, Some(pattern)) => Ok(pattern),
        (Mosaic::Unknown, _) => Err(Error::Invalid(
            "Sensor layout is unknown; choose colour or mono.",
        )),
        _ => Err(Error::Invalid(
            "Demosaicing requires a colour (Bayer) frame.",
        )),
    }
}

#[inline]
pub(super) fn reflect(coordinate: isize, length: usize) -> usize {
    if coordinate < 0 {
        (-coordinate) as usize
    } else if coordinate >= length as isize {
        (2 * (length as isize - 1) - coordinate) as usize
    } else {
        coordinate as usize
    }
}

pub fn mhc_demosaic(frame: &RawFrame, cfa: &[f32], cancel: &AtomicBool) -> Result<RgbImage> {
    let width = frame.header.info.width;
    let height = frame.header.info.height;
    let pattern = bayer_pattern(frame)?;
    if width < 6 || height < 6 || width.checked_mul(height) != Some(cfa.len()) {
        return Err(Error::Invalid("Invalid CFA reconstruction dimensions."));
    }
    let mut pixels = vec![[0.0; 3]; cfa.len()];
    pixels
        .par_chunks_mut(width)
        .enumerate()
        .try_for_each(|(y, row)| -> Result<()> {
            check_cancelled(cancel)?;
            for (x, destination) in row.iter_mut().enumerate() {
                let sample = |dx: isize, dy: isize| {
                    cfa[reflect(y as isize + dy, height) * width + reflect(x as isize + dx, width)]
                };
                let center = sample(0, 0);
                let horizontal2 = sample(-2, 0) + sample(2, 0);
                let vertical2 = sample(0, -2) + sample(0, 2);
                let diagonals = sample(-1, -1) + sample(1, -1) + sample(-1, 1) + sample(1, 1);
                *destination = match pattern.color(x, y) {
                    Color::Red | Color::Blue => {
                        let green = (4.0 * center
                            + 2.0 * (sample(-1, 0) + sample(1, 0) + sample(0, -1) + sample(0, 1))
                            - horizontal2
                            - vertical2)
                            * 0.125;
                        let opposite = (6.0 * center + 2.0 * diagonals
                            - 1.5 * (horizontal2 + vertical2))
                            * 0.125;
                        if pattern.color(x, y) == Color::Red {
                            [center, green, opposite]
                        } else {
                            [opposite, green, center]
                        }
                    }
                    Color::Green => {
                        let horizontal = (5.0 * center + 4.0 * (sample(-1, 0) + sample(1, 0))
                            - horizontal2
                            - diagonals
                            + 0.5 * vertical2)
                            * 0.125;
                        let vertical = (5.0 * center + 4.0 * (sample(0, -1) + sample(0, 1))
                            - vertical2
                            - diagonals
                            + 0.5 * horizontal2)
                            * 0.125;
                        if pattern.color(x ^ 1, y) == Color::Red {
                            [horizontal, center, vertical]
                        } else {
                            [vertical, center, horizontal]
                        }
                    }
                };
            }
            Ok(())
        })?;
    Ok(RgbImage {
        width,
        height,
        demosaic_method: DemosaicMethod::Mhc,
        pixels,
    })
}

/// Fast full-frame overview builder for CFA or Mono frames.
///
/// - If the frame fits within `max_edge` and has `width >= 6 && height >= 6`,
///   uses MHC demosaicing (with hot-pixel cosmetic cleanup) at full 1:1 resolution.
/// - If the frame is larger than `max_edge`, performs direct 2x2 Bayer superpixel
///   area-averaging into an overview of `<= max_edge` on the long side, avoiding
///   a full-resolution RGB buffer allocation and completing in a few milliseconds.
pub fn fast_cfa_overview(
    frame: &RawFrame,
    max_edge: usize,
    cancel: &AtomicBool,
) -> Result<RgbImage> {
    let width = frame.header.info.width;
    let height = frame.header.info.height;
    if width == 0 || height == 0 || width.checked_mul(height) != Some(frame.pixels.len()) {
        return Err(Error::Invalid("Invalid frame dimensions for overview."));
    }
    check_cancelled(cancel)?;

    let is_bayer =
        frame.header.info.mosaic == Mosaic::Bayer && frame.header.info.bayer_pattern.is_some();

    if width <= max_edge && height <= max_edge {
        if is_bayer && width >= 6 && height >= 6 {
            let (cleaned, _) = cosmetic_correct(frame, cancel)?;
            return mhc_demosaic(frame, &cleaned, cancel);
        }
        if is_bayer {
            let pattern = frame.header.info.bayer_pattern.unwrap();
            let mut pixels = vec![[0.0_f32; 3]; width * height];
            for y in 0..height {
                let y0 = (y & !1).min(height.saturating_sub(2));
                for x in 0..width {
                    let x0 = (x & !1).min(width.saturating_sub(2));
                    pixels[y * width + x] =
                        sample_bayer_quad(&frame.pixels, width, x0, y0, pattern);
                }
            }
            return Ok(RgbImage {
                width,
                height,
                demosaic_method: DemosaicMethod::Mhc,
                pixels,
            });
        }
        let pixels = frame.pixels.iter().map(|&v| [v, v, v]).collect();
        return Ok(RgbImage {
            width,
            height,
            demosaic_method: DemosaicMethod::None,
            pixels,
        });
    }

    if is_bayer {
        let pattern = frame.header.info.bayer_pattern.unwrap();
        let quads_w = (width / 2).max(1);
        let quads_h = (height / 2).max(1);

        // If the 2x2 Bayer superpixel grid already fits within `max_edge` (e.g. 2160x3840 -> 1080x1920),
        // map each 2x2 Bayer quad 1:1 to an overview pixel so there are never empty/unfilled grid rows.
        if quads_w <= max_edge && quads_h <= max_edge {
            let mut pixels = vec![[0.0_f32; 3]; quads_w * quads_h];
            pixels
                .par_chunks_mut(quads_w)
                .enumerate()
                .try_for_each(|(qy, row)| -> Result<()> {
                    if qy & 63 == 0 {
                        check_cancelled(cancel)?;
                    }
                    let y = qy * 2;
                    for (qx, dst) in row.iter_mut().enumerate() {
                        *dst = sample_bayer_quad(&frame.pixels, width, qx * 2, y, pattern);
                    }
                    Ok(())
                })?;
            return Ok(RgbImage {
                width: quads_w,
                height: quads_h,
                demosaic_method: DemosaicMethod::Superpixel,
                pixels,
            });
        }

        // Otherwise downscale from the `(quads_w, quads_h)` superpixel grid using destination-driven
        // box averaging so every output pixel `(ox, oy)` is guaranteed >= 1 source quad for ANY camera size.
        let sp_factor = (quads_w.max(quads_h) as f64 / max_edge.max(1) as f64).max(1.0);
        let out_w = (quads_w as f64 / sp_factor)
            .round()
            .clamp(1.0, quads_w as f64) as usize;
        let out_h = (quads_h as f64 / sp_factor)
            .round()
            .clamp(1.0, quads_h as f64) as usize;
        let mut pixels = vec![[0.0_f32; 3]; out_w * out_h];
        pixels
            .par_chunks_mut(out_w)
            .enumerate()
            .try_for_each(|(oy, row)| -> Result<()> {
                if oy & 31 == 0 {
                    check_cancelled(cancel)?;
                }
                let qy0 = (oy * quads_h / out_h).min(quads_h - 1);
                let qy1 = ((oy + 1) * quads_h / out_h).max(qy0 + 1).min(quads_h);
                for (ox, dst) in row.iter_mut().enumerate() {
                    let qx0 = (ox * quads_w / out_w).min(quads_w - 1);
                    let qx1 = ((ox + 1) * quads_w / out_w).max(qx0 + 1).min(quads_w);
                    let mut sum = [0.0_f64; 3];
                    let mut count = 0_u32;
                    for qy in qy0..qy1 {
                        let y = qy * 2;
                        for qx in qx0..qx1 {
                            let rgb = sample_bayer_quad(&frame.pixels, width, qx * 2, y, pattern);
                            sum[0] += f64::from(rgb[0]);
                            sum[1] += f64::from(rgb[1]);
                            sum[2] += f64::from(rgb[2]);
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
                Ok(())
            })?;
        Ok(RgbImage {
            width: out_w,
            height: out_h,
            demosaic_method: DemosaicMethod::Superpixel,
            pixels,
        })
    } else {
        let factor = (width.max(height) as f64 / max_edge.max(1) as f64).max(1.0);
        let out_w = (width as f64 / factor).round().clamp(1.0, width as f64) as usize;
        let out_h = (height as f64 / factor).round().clamp(1.0, height as f64) as usize;
        let mut pixels = vec![[0.0_f32; 3]; out_w * out_h];
        pixels
            .par_chunks_mut(out_w)
            .enumerate()
            .try_for_each(|(oy, row)| -> Result<()> {
                if oy & 31 == 0 {
                    check_cancelled(cancel)?;
                }
                let y0 = (oy * height / out_h).min(height - 1);
                let y1 = ((oy + 1) * height / out_h).max(y0 + 1).min(height);
                for (ox, dst) in row.iter_mut().enumerate() {
                    let x0 = (ox * width / out_w).min(width - 1);
                    let x1 = ((ox + 1) * width / out_w).max(x0 + 1).min(width);
                    let mut sum = 0.0_f64;
                    let mut count = 0_u32;
                    for y in y0..y1 {
                        let row_slice = &frame.pixels[y * width + x0..y * width + x1];
                        for &v in row_slice {
                            sum += f64::from(v);
                            count += 1;
                        }
                    }
                    let g = (sum / f64::from(count.max(1))) as f32;
                    *dst = [g, g, g];
                }
                Ok(())
            })?;
        Ok(RgbImage {
            width: out_w,
            height: out_h,
            demosaic_method: DemosaicMethod::None,
            pixels,
        })
    }
}

#[inline]
fn sample_bayer_quad(
    pixels: &[f32],
    width: usize,
    x: usize,
    y: usize,
    pattern: BayerPattern,
) -> [f32; 3] {
    let p00 = pixels[y * width + x];
    let p10 = pixels[y * width + (x + 1).min(width - 1)];
    let p01 = pixels[(y + 1) * width + x];
    let p11 = pixels[(y + 1) * width + (x + 1).min(width - 1)];
    match pattern {
        BayerPattern::RGGB => [p00, 0.5 * (p10 + p01), p11],
        BayerPattern::BGGR => [p11, 0.5 * (p10 + p01), p00],
        BayerPattern::GRBG => [p10, 0.5 * (p00 + p11), p01],
        BayerPattern::GBRG => [p01, 0.5 * (p00 + p11), p10],
    }
}

pub fn debayer(
    frame: &RawFrame,
    cancel: &AtomicBool,
    method: DemosaicMethod,
) -> Result<(RgbImage, DebayerStats)> {
    debayer_reusing(frame, cancel, method, &mut DebayerBuffers::default())
}

#[derive(Default)]
pub struct DebayerBuffers {
    pub cleaned: Vec<f32>,
    pub rgb: Vec<[f32; 3]>,
}

pub fn debayer_reusing(
    frame: &RawFrame,
    cancel: &AtomicBool,
    method: DemosaicMethod,
    buffers: &mut DebayerBuffers,
) -> Result<(RgbImage, DebayerStats)> {
    if frame.header.info.mosaic == Mosaic::Rgb {
        return Err(Error::Invalid("This frame is already debayered."));
    }
    let mono = frame.header.info.mosaic != Mosaic::Bayer;
    let width = frame.header.info.width;
    let height = frame.header.info.height;
    let start = Instant::now();
    let corrected_pixels = if width >= 6 && height >= 6 {
        let (cleaned, corrected) =
            cosmetic_correct_spans(frame, cancel, std::mem::take(&mut buffers.cleaned), None)?;
        buffers.cleaned = cleaned;
        corrected
    } else {
        buffers.cleaned.clear();
        buffers.cleaned.extend_from_slice(&frame.pixels);
        0
    };
    let cleaned = &buffers.cleaned;
    let cosmetic_seconds = start.elapsed().as_secs_f64();
    let start = Instant::now();
    let method = if mono || width < 6 || height < 6 {
        DemosaicMethod::None
    } else {
        method
    };
    let rgb = match method {
        DemosaicMethod::Mhc => mhc_demosaic(frame, cleaned, cancel)?,
        DemosaicMethod::Vng => crate::vng::vng_demosaic(frame, cleaned, cancel)?,
        DemosaicMethod::Superpixel | DemosaicMethod::None | DemosaicMethod::External => {
            let mut pixels = std::mem::take(&mut buffers.rgb);
            pixels.clear();
            pixels.extend(cleaned.iter().map(|&v| [v; 3]));
            check_cancelled(cancel)?;
            RgbImage {
                width,
                height,
                demosaic_method: DemosaicMethod::None,
                pixels,
            }
        }
    };
    Ok((
        rgb,
        DebayerStats {
            demosaic_method: method,
            cosmetic_seconds,
            demosaic_seconds: start.elapsed().as_secs_f64(),
            corrected_pixels,
        },
    ))
}
