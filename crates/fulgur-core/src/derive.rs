//! Reconstruction of frame metadata missing from FITS/XISF headers.
//!
//! Priority: primary header card -> alias header cards -> file-name tokens -> embedded camera catalogue.

use crate::BayerPattern;
use crate::cameras::{self, CameraModel};
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DerivedValue {
    /// "exposure_seconds", "sensor_layout", "gain" or "object".
    pub field: &'static str,
    pub value: String,
    /// Where it came from, e.g. "file name token '30.0s'".
    pub source: String,
    pub needs_confirmation: bool,
}

pub const EXPOSURE: &str = "exposure_seconds";
pub const LAYOUT: &str = "sensor_layout";

/// Header cards that some capture programs use instead of BAYERPAT.
const PATTERN_ALIASES: [&str; 4] = ["COLORTYP", "CFAPAT", "CFA-PAT", "CFATYPE"];
/// Header cards that may name the camera, in priority order.
const CAMERA_CARDS: [&str; 4] = ["INSTRUME", "CAMERA", "TELESCOP", "CREATOR"];

fn stem(name: &str) -> &str {
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

fn tokens(name: &str) -> impl Iterator<Item = &str> {
    stem(name).split(['_', ' ']).filter(|t| !t.is_empty())
}

fn positive(text: &str) -> Option<f64> {
    if !text.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    text.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
}

/// Exposure from a token such as "30.0s", "300s", "120sec" or "500ms".
fn exposure_token(token: &str) -> Option<f64> {
    let token = token.to_ascii_lowercase();
    if let Some(ms) = token.strip_suffix("ms") {
        return positive(ms).map(|v| v / 1000.0);
    }
    let seconds = token
        .strip_suffix("sec")
        .or_else(|| token.strip_suffix('s'))?;
    positive(seconds)
}

/// A file name gives a value only when it names exactly one.
fn single<T: PartialEq + Copy>(values: impl Iterator<Item = (T, String)>) -> Option<(T, String)> {
    let mut found: Option<(T, String)> = None;
    for (value, token) in values {
        match &found {
            None => found = Some((value, token)),
            Some((first, _)) if *first == value => {}
            Some(_) => return None,
        }
    }
    found
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExposureCard {
    pub seconds: f64,
    pub note: Option<String>,
}

pub(crate) const MAX_PLAUSIBLE_EXPOSURE_S: f64 = 3600.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unit {
    Seconds,
    Milliseconds,
}

pub(crate) fn unit_in_comment(comment: &str) -> Option<Unit> {
    let lower = comment.to_ascii_lowercase();
    let word = |text: &str| match text.trim() {
        "ms" | "msec" | "msecs" | "millisecond" | "milliseconds" => Some(Unit::Milliseconds),
        "s" | "sec" | "secs" | "second" | "seconds" => Some(Unit::Seconds),
        _ => None,
    };
    if let (Some(open), Some(close)) = (lower.find('['), lower.find(']'))
        && open < close
        && let Some(unit) = word(&lower[open + 1..close])
    {
        return Some(unit);
    }
    let mut found = None;
    for token in lower.split(|c: char| !c.is_ascii_alphanumeric()) {
        if let Some(unit) = word(token) {
            match found {
                None => found = Some(unit),
                Some(first) if first != unit => return None,
                _ => {}
            }
        }
    }
    found
}

pub(crate) fn unit_note(key: &str, card: &ExposureCard) -> Option<DerivedValue> {
    card.note.as_ref().map(|note| DerivedValue {
        field: EXPOSURE,
        value: format!("{} s", trim(card.seconds)),
        source: format!("header card {key} ({note})"),
        needs_confirmation: true,
    })
}

pub(crate) fn exposure(
    card: impl Fn(&str) -> Option<ExposureCard>,
    name: Option<&str>,
) -> Option<(f64, DerivedValue)> {
    let derived = |value: f64, source: String| DerivedValue {
        field: EXPOSURE,
        value: format!("{} s", trim(value)),
        source,
        needs_confirmation: true,
    };
    if let Some(found) = card("EXPOSURE").filter(|c| c.seconds.is_finite() && c.seconds > 0.0) {
        let source = match &found.note {
            Some(note) => format!("header card EXPOSURE ({note})"),
            None => "header card EXPOSURE".into(),
        };
        return Some((found.seconds, derived(found.seconds, source)));
    }
    let (value, token) = single(
        tokens(name?).filter_map(|t| exposure_token(t).map(|v| (v.to_bits(), t.to_owned()))),
    )?;
    let value = f64::from_bits(value);
    Some((value, derived(value, format!("file name token '{token}'"))))
}

pub(crate) fn gain(name: Option<&str>) -> Option<(f64, DerivedValue)> {
    let all: Vec<&str> = tokens(name?).collect();
    let (value, token) = single(all.iter().enumerate().filter_map(|(i, t)| {
        let lower = t.to_ascii_lowercase();
        let rest = lower.strip_prefix("gain")?;
        let text = if rest.is_empty() {
            all.get(i + 1)?.to_string()
        } else {
            rest.to_owned()
        };
        let value = text
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v >= 0.0)?;
        Some((value.to_bits(), t.to_string()))
    }))?;
    let value = f64::from_bits(value);
    Some((
        value,
        DerivedValue {
            field: "gain",
            value: trim(value),
            source: format!("file name token '{token}'"),
            needs_confirmation: false,
        },
    ))
}

pub(crate) fn object(name: Option<&str>) -> Option<(String, DerivedValue)> {
    let parts: Vec<&str> = stem(name?).split('_').collect();
    let index = parts
        .iter()
        .position(|p| p.eq_ignore_ascii_case("light") || p.eq_ignore_ascii_case("lights"))?;
    let target = parts.get(index + 1)?.trim();
    if target.is_empty() || exposure_token(target).is_some() || positive(target).is_some() {
        return None;
    }
    Some((
        target.to_owned(),
        DerivedValue {
            field: "object",
            value: target.to_owned(),
            source: "file name (after 'Light_')".into(),
            needs_confirmation: false,
        },
    ))
}

pub(crate) struct Layout {
    pub pattern: Option<BayerPattern>,
    pub source: &'static str,
    pub derived: DerivedValue,
}

pub(crate) fn layout(
    card: impl Fn(&str) -> Option<String>,
    name: Option<&str>,
    width: usize,
    height: usize,
) -> Option<Layout> {
    let derived = |value: String, source: String| DerivedValue {
        field: LAYOUT,
        value,
        source,
        needs_confirmation: true,
    };
    for key in PATTERN_ALIASES {
        if let Some(pattern) = card(key).and_then(|v| BayerPattern::parse(&v).ok()) {
            return Some(Layout {
                pattern: Some(pattern),
                source: "header_alias",
                derived: derived(
                    format!("colour {}", pattern.name()),
                    format!("header card {key}"),
                ),
            });
        }
    }
    if let Some(name) = name {
        let found = single(
            tokens(name).filter_map(|t| BayerPattern::parse(t).ok().map(|p| (p, t.to_owned()))),
        );
        if let Some((pattern, token)) = found {
            return Some(Layout {
                pattern: Some(pattern),
                source: "file_name",
                derived: derived(
                    format!("colour {}", pattern.name()),
                    format!("file name token '{token}'"),
                ),
            });
        }
    }
    let (camera, from) = CAMERA_CARDS
        .iter()
        .find_map(|key| {
            let text = card(key)?;
            cameras::identify(&text).map(|c| (c, format!("header card {key} '{text}'")))
        })
        .or_else(|| {
            tokens(name?).find_map(|t| {
                cameras::identify_token(t).map(|c| (c, format!("file name token '{t}'")))
            })
        })?;
    if !camera.is_colour() {
        return None;
    }
    let pattern = camera.bayer_pattern();
    Some(Layout {
        pattern,
        source: "camera_catalog",
        derived: derived(
            match pattern {
                Some(p) => format!("colour {}", p.name()),
                None => "colour, Bayer pattern not catalogued (choose it)".into(),
            },
            catalogue_source(camera, &from, width, height),
        ),
    })
}

fn catalogue_source(camera: &CameraModel, from: &str, width: usize, height: usize) -> String {
    let mut text = format!(
        "camera catalogue: {} ({}), identified from {from}; confidence {}",
        camera.label(),
        camera.sensor,
        camera.confidence
    );
    if camera.native_size(width, height) == Some(false) {
        text.push_str(&format!(
            "; frame {width}x{height} differs from the native {}x{}, the pattern may be shifted",
            camera.width.unwrap_or(0),
            camera.height.unwrap_or(0)
        ));
    }
    text
}

pub(crate) fn trim(value: f64) -> String {
    let text = format!("{value:.4}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}
