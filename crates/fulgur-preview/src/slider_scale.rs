//! Non-linear scale for the manual Black / White stretch sliders.
//!
//! Linear astro images keep almost all pixels in a narrow band around the sky background
//! (the median), a few percent of the full range. A linear slider spends nearly all of its
//! travel on bright values nobody needs and leaves the background band a pixel or two wide.
//!
//! This scale maps slider position `s ∈ [0, 1]` to a normalized value `v ∈ [0, 1]` through
//!
//! ```text
//! s(v) = (asinh((v - median) / width) - asinh((0 - median) / width))
//!        / (asinh((1 - median) / width) - asinh((0 - median) / width))
//! ```
//!
//! `asinh` behaves linearly within about one `width` of the median and logarithmically
//! further out, so the slider is most sensitive at the median, still reaches 0 and 1, and
//! becomes coarser towards the bright end. `width` is the background noise (MAD sigma) of
//! the frame, with a small floor.

/// Smallest width used for the sensitive zone (normalized units), for nearly noiseless
/// frames.
const MIN_WIDTH: f64 = 2e-4;

/// Mapping between slider position and normalized pixel value, centred on the frame median.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MedianSliderScale {
    median: f64,
    width: f64,
    lo: f64,
    span: f64,
}

impl MedianSliderScale {
    /// `median` and `sigma` of the frame in normalized `[0, 1]` units.
    pub fn new(median: f32, sigma: f32) -> Self {
        let median = f64::from(median).clamp(0.0, 1.0);
        let width = f64::from(sigma).max(MIN_WIDTH);
        let lo = ((0.0 - median) / width).asinh();
        let hi = ((1.0 - median) / width).asinh();
        Self {
            median,
            width,
            lo,
            span: (hi - lo).max(1e-12),
        }
    }

    /// Slider position for a normalized value.
    pub fn position(&self, value: f64) -> f64 {
        let v = value.clamp(0.0, 1.0);
        ((((v - self.median) / self.width).asinh() - self.lo) / self.span).clamp(0.0, 1.0)
    }

    /// Normalized value for a slider position.
    pub fn value(&self, position: f64) -> f64 {
        let s = position.clamp(0.0, 1.0);
        (self.median + self.width * (self.lo + s * self.span).sinh()).clamp(0.0, 1.0)
    }

    /// Value change per unit of slider travel at `value` (smaller = finer control).
    pub fn step_at(&self, value: f64) -> f64 {
        let u = (value - self.median) / self.width;
        self.width * self.span * (1.0 + u * u).sqrt()
    }
}

/// Black, midtone and white points of the three colour channels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelPoints {
    pub black: [f32; 3],
    pub mid: [f32; 3],
    pub white: [f32; 3],
}

impl ChannelPoints {
    /// The single values the manual sliders show: mean black / white, geometric-mean midtone.
    pub fn master(&self) -> (f32, f32, f32) {
        let mean = |v: [f32; 3]| (v[0] + v[1] + v[2]) / 3.0;
        let geo = (self.mid.iter().map(|m| m.max(1e-6).ln()).sum::<f32>() / 3.0).exp();
        (mean(self.black), geo, mean(self.white))
    }

    /// Apply new master slider values. `linked`: all channels get the same values. Unlinked:
    /// each channel keeps its own offset (black / white shifted by the same amount, midtone
    /// scaled by the same factor), so an unlinked colour balance survives manual tuning.
    pub fn with_master(&self, black: f32, mid: f32, white: f32, linked: bool) -> Self {
        let (b0, m0, w0) = self.master();
        let mut out = if linked {
            Self {
                black: [black; 3],
                mid: [mid; 3],
                white: [white; 3],
            }
        } else {
            let factor = mid.max(1e-6) / m0.max(1e-6);
            Self {
                black: self.black.map(|b| b + (black - b0)),
                mid: self.mid.map(|m| m * factor),
                white: self.white.map(|w| w + (white - w0)),
            }
        };
        for c in 0..3 {
            out.black[c] = out.black[c].clamp(0.0, 0.999);
            out.white[c] = out.white[c].clamp(out.black[c] + 0.001, 1.0);
            out.mid[c] = out.mid[c].clamp(0.0005, 0.9995);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unlinked() -> ChannelPoints {
        ChannelPoints {
            black: [0.030, 0.036, 0.024],
            mid: [0.010, 0.020, 0.040],
            white: [0.90, 0.95, 1.00],
        }
    }

    #[test]
    fn unlinked_channels_keep_their_offsets() {
        let p = unlinked();
        let (b, m, w) = p.master();
        let q = p.with_master(b + 0.005, m * 2.0, w - 0.1, false);
        for c in 0..3 {
            assert!((q.black[c] - (p.black[c] + 0.005)).abs() < 1e-6);
            assert!((q.mid[c] - p.mid[c] * 2.0).abs() < 1e-6);
        }
        assert!(((q.black[0] - q.black[1]) - (p.black[0] - p.black[1])).abs() < 1e-6);
        assert!((q.white[0] - 0.80).abs() < 1e-6 && (q.white[1] - 0.85).abs() < 1e-6);
        let (b2, m2, _) = q.master();
        assert!((b2 - (b + 0.005)).abs() < 1e-6);
        assert!((m2 - m * 2.0).abs() < 1e-5);
    }

    #[test]
    fn linked_sets_every_channel_and_points_stay_ordered() {
        let q = unlinked().with_master(0.05, 0.1, 0.04, true);
        assert_eq!(q.black, [0.05; 3]);
        assert_eq!(q.mid, [0.1; 3]);
        for c in 0..3 {
            assert!(q.white[c] > q.black[c], "white must stay above black");
        }
        let q = unlinked().with_master(-1.0, 5.0, 2.0, false);
        for c in 0..3 {
            assert!(q.black[c] >= 0.0 && q.white[c] <= 1.0 && q.mid[c] < 1.0);
        }
    }

    #[test]
    fn endpoints_and_roundtrip() {
        let scale = MedianSliderScale::new(0.04, 0.003);
        assert!(scale.value(0.0).abs() < 1e-12);
        assert!((scale.value(1.0) - 1.0).abs() < 1e-12);
        for v in [0.0, 0.01, 0.039, 0.04, 0.041, 0.1, 0.5, 0.99] {
            assert!((scale.value(scale.position(v)) - v).abs() < 1e-9, "{v}");
        }
    }

    #[test]
    fn monotonic() {
        let scale = MedianSliderScale::new(0.04, 0.003);
        let mut prev = -1.0;
        for i in 0..=1000 {
            let v = scale.value(i as f64 / 1000.0);
            assert!(v > prev || (i == 0 && v == 0.0));
            prev = v;
        }
    }

    #[test]
    fn most_sensitive_at_the_median() {
        let scale = MedianSliderScale::new(0.04, 0.003);
        let at_median = scale.step_at(0.04);
        assert!(at_median < scale.step_at(0.02));
        assert!(at_median < scale.step_at(0.2));
        assert!(at_median < scale.step_at(0.9));
        // Within one sigma of the median the slider moves through a large share of its
        // travel; a linear slider would give it 0.6 %.
        let share = scale.position(0.043) - scale.position(0.037);
        assert!(share > 0.15, "{share}");
        // The dark side (0 .. median) keeps a usable part of the travel.
        assert!(scale.position(0.04) > 0.25, "{}", scale.position(0.04));
    }

    #[test]
    fn degenerate_inputs_are_safe() {
        let scale = MedianSliderScale::new(0.0, 0.0);
        assert!(scale.value(0.5).is_finite());
        let scale = MedianSliderScale::new(1.5, -1.0);
        assert!((scale.value(1.0) - 1.0).abs() < 1e-12);
    }
}
