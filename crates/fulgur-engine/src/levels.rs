//! Signal levels per colour channel: median (sky background for typical deep-sky frames) and
//! noise, measured on the full-resolution data in ADU.
//!
//! Noise is estimated from differences between neighbouring same-colour pixels:
//! `sigma = 1.4826 * MAD(x[i] - x[i+step]) / sqrt(2)`. Differencing removes gradients,
//! vignetting and extended nebulosity, so the value tracks the pixel-to-pixel (shot + read)
//! noise rather than the image content. MAD makes it robust to stars and hot pixels.

use fulgur_core::{BayerPattern, Color, Mosaic, RawFrame};
use serde::Serialize;

/// Maximum samples per channel used for the statistics (spread evenly over the frame).
const MAX_SAMPLES: usize = 500_000;

/// Median and noise of one channel, in ADU.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChannelLevel {
    /// `R`, `G`, `B`, `L` (luminance of an RGB image) or `Mono`.
    pub name: &'static str,
    pub median_adu: f64,
    pub noise_adu: f64,
}

/// Levels of a single-plane frame: per Bayer colour for CFA data, one `Mono` entry otherwise.
pub fn plane_levels(frame: &RawFrame) -> Vec<ChannelLevel> {
    let width = frame.header.info.width;
    let height = frame.header.info.height;
    let pixels = &frame.pixels;
    if width < 4 || height < 4 || pixels.len() < width * height {
        return Vec::new();
    }
    match (frame.header.info.mosaic, frame.header.info.bayer_pattern) {
        (Mosaic::Bayer, Some(pattern)) => {
            [(Color::Red, "R"), (Color::Green, "G"), (Color::Blue, "B")]
                .into_iter()
                .filter_map(|(color, name)| {
                    cfa_channel_level(pixels, width, height, pattern, color, name)
                })
                .collect()
        }
        _ => level_from("Mono", |push_value, push_diff| {
            let row_step = row_step(height, width);
            for y in (0..height).step_by(row_step) {
                let row = &pixels[y * width..(y + 1) * width];
                for x in 0..width {
                    push_value(row[x]);
                    if x + 1 < width {
                        push_diff(row[x + 1] - row[x]);
                    }
                }
            }
        })
        .into_iter()
        .collect(),
    }
}

/// Levels of an RGB image (`[r, g, b]` pixels, row-major): R, G, B and luminance L.
pub fn rgb_levels(pixels: &[[f32; 3]], width: usize, height: usize) -> Vec<ChannelLevel> {
    if width < 4 || height < 4 || pixels.len() < width * height {
        return Vec::new();
    }
    let lum = |p: &[f32; 3]| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
    let mut out = Vec::with_capacity(4);
    for (c, name) in [(0usize, "R"), (1, "G"), (2, "B"), (3, "L")] {
        let value = |p: &[f32; 3]| if c == 3 { lum(p) } else { p[c] };
        if let Some(level) = level_from(name, |push_value, push_diff| {
            let row_step = row_step(height, width);
            for y in (0..height).step_by(row_step) {
                let row = &pixels[y * width..(y + 1) * width];
                for x in 0..width {
                    push_value(value(&row[x]));
                    if x + 1 < width {
                        push_diff(value(&row[x + 1]) - value(&row[x]));
                    }
                }
            }
        }) {
            out.push(level);
        }
    }
    out
}

fn cfa_channel_level(
    pixels: &[f32],
    width: usize,
    height: usize,
    pattern: BayerPattern,
    color: Color,
    name: &'static str,
) -> Option<ChannelLevel> {
    level_from(name, |push_value, push_diff| {
        // Same-colour neighbours sit two pixels apart along a row.
        let row_step = row_step(height, width / 2).max(1);
        let mut y = 0;
        while y < height {
            for py in [y, y + 1] {
                if py >= height {
                    continue;
                }
                let row = &pixels[py * width..(py + 1) * width];
                let Some(start) = (0..2).find(|&x| pattern.color(x, py) == color) else {
                    continue;
                };
                let mut x = start;
                while x < width {
                    push_value(row[x]);
                    if x + 2 < width {
                        push_diff(row[x + 2] - row[x]);
                    }
                    x += 2;
                }
            }
            y += 2 * row_step;
        }
    })
}

/// Row stride so that about `MAX_SAMPLES` samples are taken from a `height` x `per_row` grid.
fn row_step(height: usize, per_row: usize) -> usize {
    let total = height.saturating_mul(per_row.max(1));
    total.div_ceil(MAX_SAMPLES).max(1)
}

/// Collect values and neighbour differences with `fill`, then return median and noise.
fn level_from(
    name: &'static str,
    fill: impl FnOnce(&mut dyn FnMut(f32), &mut dyn FnMut(f32)),
) -> Option<ChannelLevel> {
    let mut values = Vec::new();
    let mut diffs = Vec::new();
    fill(
        &mut |v| {
            if v.is_finite() {
                values.push(v);
            }
        },
        &mut |d| {
            if d.is_finite() {
                diffs.push(d.abs());
            }
        },
    );
    if values.is_empty() {
        return None;
    }
    let median = median_of(&mut values);
    let noise = if diffs.is_empty() {
        0.0
    } else {
        1.4826 * median_of(&mut diffs) / std::f64::consts::SQRT_2
    };
    Some(ChannelLevel {
        name,
        median_adu: median,
        noise_adu: noise,
    })
}

fn median_of(values: &mut [f32]) -> f64 {
    let mid = values.len() / 2;
    let (_, m, _) = values.select_nth_unstable_by(mid, f32::total_cmp);
    f64::from(*m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fulgur_core::LayoutChoice;
    use fulgur_core::fits::{FitsHeader, FrameInfo};

    /// Deterministic pseudo-Gaussian noise (sum of uniforms), mean 0, sigma 1.
    fn noise(i: usize) -> f32 {
        let mut s = (i as u64)
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mut acc = 0.0_f32;
        for _ in 0..12 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            acc += (s % 10_000) as f32 / 10_000.0;
        }
        acc - 6.0
    }

    fn frame(width: usize, height: usize, pixels: Vec<f32>, bayer: bool) -> RawFrame {
        let mut info = FrameInfo::synthetic(width, height);
        info.apply_layout(if bayer {
            LayoutChoice::Bayer(BayerPattern::RGGB)
        } else {
            LayoutChoice::Mono
        });
        RawFrame {
            header: FitsHeader::from_synthetic(info, Vec::new()),
            pixels,
        }
    }

    #[test]
    fn mono_median_and_noise_ignore_gradient() {
        let (w, h) = (400, 300);
        // Strong horizontal gradient + noise sigma 20 around 1000.
        let pixels = (0..w * h)
            .map(|i| 1000.0 + (i % w) as f32 * 0.5 + 20.0 * noise(i))
            .collect();
        let levels = plane_levels(&frame(w, h, pixels, false));
        assert_eq!(levels.len(), 1);
        assert_eq!(levels[0].name, "Mono");
        assert!(
            (levels[0].median_adu - 1100.0).abs() < 5.0,
            "{}",
            levels[0].median_adu
        );
        assert!(
            (levels[0].noise_adu - 20.0).abs() < 1.5,
            "{}",
            levels[0].noise_adu
        );
    }

    #[test]
    fn bayer_channels_are_measured_separately() {
        let (w, h) = (400, 300);
        let pattern = BayerPattern::RGGB;
        let pixels = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                let (level, sigma) = match pattern.color(x, y) {
                    Color::Red => (800.0, 10.0),
                    Color::Green => (1500.0, 15.0),
                    Color::Blue => (600.0, 8.0),
                };
                level + sigma * noise(i)
            })
            .collect();
        let levels = plane_levels(&frame(w, h, pixels, true));
        let names: Vec<_> = levels.iter().map(|l| l.name).collect();
        assert_eq!(names, ["R", "G", "B"]);
        for (l, (m, s)) in levels
            .iter()
            .zip([(800.0, 10.0), (1500.0, 15.0), (600.0, 8.0)])
        {
            assert!(
                (l.median_adu - m).abs() < 2.0,
                "{} {}",
                l.name,
                l.median_adu
            );
            assert!((l.noise_adu - s).abs() < 1.0, "{} {}", l.name, l.noise_adu);
        }
    }

    #[test]
    fn rgb_levels_include_luminance() {
        let (w, h) = (100, 80);
        let pixels: Vec<[f32; 3]> = (0..w * h)
            .map(|i| {
                [
                    100.0 + noise(i),
                    200.0 + noise(i + 7),
                    300.0 + noise(i + 13),
                ]
            })
            .collect();
        let levels = rgb_levels(&pixels, w, h);
        let names: Vec<_> = levels.iter().map(|l| l.name).collect();
        assert_eq!(names, ["R", "G", "B", "L"]);
        assert!((levels[1].median_adu - 200.0).abs() < 1.0);
    }
}
