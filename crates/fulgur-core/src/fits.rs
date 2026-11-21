//! Extended FITS and XISF image reader supporting BITPIX 8, 16, 32, -32, -64,
//! 2D/3D cubes, and Multi-Extension FITS (MEF).

use crate::derive::{self, DerivedValue};
use crate::standard;
use crate::xisf::{self, XisfImage};
use crate::{
    BayerPattern, DemosaicMethod, Error, HeaderCard, LayoutChoice, Mosaic, RawFrame, Result,
    RgbImage, check_cancelled,
};
use rayon::prelude::*;
use serde::Serialize;
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub const ASTRO_EXTENSIONS: [&str; 5] = ["fit", "fits", "fts", "xisf", "xish"];

pub fn is_astro_input(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        ASTRO_EXTENSIONS
            .iter()
            .any(|expected| extension.eq_ignore_ascii_case(expected))
    }) && !path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with("._"))
}

pub fn is_supported_image(path: &Path) -> bool {
    !path.file_name().is_some_and(|name| {
        let name = name.to_string_lossy();
        // `._*` are macOS resource forks; `.*.fulgur-tmp-*` are our own in-progress WCS saves.
        name.starts_with("._") || name.contains(".fulgur-tmp-")
    }) && (is_astro_input(path)
        || standard::is_standard_image(path)
        || crate::raw::is_raw_image(path))
}

pub const BLOCK_SIZE: usize = 2880;
const MAX_HEADER_BLOCKS: usize = 512;
pub const ROI_READ_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct FrameInfo {
    pub width: usize,
    pub height: usize,
    pub planes: usize,
    pub bitpix: i32,
    pub bzero: f32,
    pub bscale: f32,
    pub bayer_pattern: Option<BayerPattern>,
    pub mosaic: Mosaic,
    pub mosaic_source: &'static str,
    pub exposure_seconds: f64,
    pub object: Option<String>,
    pub filter: Option<String>,
    pub date_obs: Option<String>,
    pub gain: Option<f64>,
    pub ccd_temperature: Option<f64>,
    pub instrument: Option<String>,
    pub telescope: Option<String>,
    pub bias_informational: Option<f64>,
    pub creator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saturation_adu: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub derived: Vec<DerivedValue>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub rows_mirrored: bool,
}

impl FrameInfo {
    /// Minimal 16-bit mono frame description (tests and synthetic frames).
    pub fn synthetic(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            planes: 1,
            bitpix: 16,
            bzero: 0.0,
            bscale: 1.0,
            bayer_pattern: None,
            mosaic: Mosaic::Mono,
            mosaic_source: "synthetic",
            exposure_seconds: 0.0,
            object: None,
            filter: None,
            date_obs: None,
            gain: None,
            ccd_temperature: None,
            instrument: None,
            telescope: None,
            bias_informational: None,
            creator: None,
            saturation_adu: None,
            derived: Vec::new(),
            rows_mirrored: false,
        }
    }

    pub fn apply_layout(&mut self, choice: LayoutChoice) {
        if self.mosaic == Mosaic::Rgb {
            return;
        }
        if choice != LayoutChoice::Auto {
            self.derived.retain(|d| d.field != derive::LAYOUT);
        }
        match choice {
            LayoutChoice::Auto => {}
            LayoutChoice::Mono => {
                self.bayer_pattern = None;
                self.mosaic = Mosaic::Mono;
                self.mosaic_source = "user";
            }
            LayoutChoice::Bayer(pattern) => {
                self.bayer_pattern = Some(pattern);
                self.mosaic = Mosaic::Bayer;
                self.mosaic_source = "user";
            }
        }
    }
}

#[derive(Clone)]
pub struct FitsHeader {
    cards: Vec<[u8; 80]>,
    pub extra_cards: Vec<HeaderCard>,
    pub info: FrameInfo,
    pub data_offset: u64,
    pub data_bytes: usize,
    pub file_bytes: u64,
    pub(crate) bzero: f64,
    pub(crate) bscale: f64,
    /// Scale factor mapping native physical samples into 0..65535 pipeline ADU.
    pub(crate) adu_scale: f64,
    pub xisf: Option<Arc<XisfImage>>,
    pub(crate) bottom_up: bool,
    pub hdu_index: usize,
    pub hdu_names: Vec<String>,
}

impl fmt::Debug for FitsHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FitsHeader")
            .field("info", &self.info)
            .field("data_offset", &self.data_offset)
            .field("hdu_index", &self.hdu_index)
            .finish_non_exhaustive()
    }
}

impl FitsHeader {
    /// Construct a synthetic `FitsHeader` wrapper for non-FITS linear CFA sources (such as DSLR/mirrorless RAW Stage 2).
    pub fn from_synthetic(info: FrameInfo, extra_cards: Vec<HeaderCard>) -> Self {
        let data_bytes = info.width.saturating_mul(info.height).saturating_mul(2);
        Self {
            cards: Vec::new(),
            extra_cards,
            info,
            data_offset: 0,
            data_bytes,
            file_bytes: data_bytes as u64,
            bzero: 0.0,
            bscale: 1.0,
            adu_scale: 1.0,
            xisf: None,
            bottom_up: false,
            hdu_index: 0,
            hdu_names: vec!["#0 RAW Sensor CFA".to_owned()],
        }
    }

    /// Extract all header cards for the Metadata Inspector.
    /// When `redact_gps` is true, `SITELAT` and `SITELONG` values are masked.
    pub fn header_cards(&self, redact_gps: bool) -> Vec<HeaderCard> {
        let mut list = Vec::with_capacity(self.cards.len() + self.extra_cards.len());
        for raw in &self.cards {
            let key = keyword(raw).to_owned();
            if key.is_empty() {
                continue;
            }
            if let Some(mut val) = value(raw) {
                let com = comment(raw).unwrap_or_default();
                if redact_gps && is_gps_keyword(&key) {
                    val = "[REDACTED GPS]".to_owned();
                }
                list.push(HeaderCard {
                    keyword: key,
                    value: val,
                    comment: com,
                });
            } else {
                let text = std::str::from_utf8(&raw[8..])
                    .unwrap_or("")
                    .trim()
                    .to_owned();
                if !text.is_empty() {
                    list.push(HeaderCard {
                        keyword: key,
                        value: text,
                        comment: String::new(),
                    });
                }
            }
        }
        for card in &self.extra_cards {
            let mut c = card.clone();
            if redact_gps && is_gps_keyword(&c.keyword) {
                c.value = "[REDACTED GPS]".to_owned();
            }
            list.push(c);
        }
        list
    }

    /// Look up the string value of a header keyword (case-insensitive).
    pub fn card_str(&self, key: &str) -> Option<String> {
        self.cards
            .iter()
            .find(|c| keyword(c).eq_ignore_ascii_case(key))
            .and_then(value)
            .or_else(|| {
                self.extra_cards
                    .iter()
                    .find(|c| c.keyword.eq_ignore_ascii_case(key))
                    .map(|c| c.value.clone())
            })
    }

    /// Look up a finite `f64` value for a header keyword (case-insensitive).
    pub fn card_f64(&self, key: &str) -> Option<f64> {
        self.card_str(key)
            .and_then(|v| v.trim().replace(['D', 'd'], "E").parse::<f64>().ok())
            .filter(|v| v.is_finite())
    }

    /// Extract approximate `(ra_deg, dec_deg)` hint from header cards (`CRVAL1/2`,
    /// `RA`/`DEC`, `OBJCTRA`/`OBJCTDEC`, `TELRA`/`TELDEC`, or XISF properties).
    pub fn approx_ra_dec_deg(&self) -> Option<(f64, f64)> {
        let candidates = [
            ("CRVAL1", "CRVAL2", false),
            ("RA", "DEC", false),
            ("OBJCTRA", "OBJCTDEC", true),
            ("TELRA", "TELDEC", false),
            ("Observation:Center:RA", "Observation:Center:Dec", false),
        ];
        for (ra_key, dec_key, default_hours) in candidates {
            let (Some(ra_raw), Some(dec_raw)) = (self.card_str(ra_key), self.card_str(dec_key))
            else {
                continue;
            };
            if let (Some(ra), Some(dec)) = (
                parse_ra_deg(&ra_raw, default_hours),
                parse_dec_deg(&dec_raw),
            ) {
                return Some((ra, dec));
            }
        }
        None
    }

    /// Estimate the plate scale in arcseconds per pixel (`″/px`) from header cards
    /// (`PIXSCALE`/`SCALE`, WCS `CD`/`CDELT` matrix, or `XPIXSZ` µm + `FOCALLEN` mm).
    pub fn pixel_scale_arcsec(&self) -> Option<f64> {
        let num = |k: &str| -> Option<f64> { self.card_f64(k) };

        for k in ["PIXSCALE", "SCALE", "SECPIX"] {
            if let Some(s) = num(k).filter(|&s| s > 0.01 && s < 1200.0) {
                return Some(s);
            }
        }

        if let (Some(cd11), Some(cd12)) = (num("CD1_1"), num("CD1_2")) {
            let deg_px = cd11.hypot(cd12);
            if deg_px > 1e-7 && deg_px < 1.0 {
                return Some(deg_px * 3600.0);
            }
        }
        if let Some(cdelt1) = num("CDELT1") {
            let deg_px = cdelt1.abs();
            if deg_px > 1e-7 && deg_px < 1.0 {
                return Some(deg_px * 3600.0);
            }
        }

        let pix_um = num("XPIXSZ")
            .or_else(|| num("PIXSIZE1"))
            .or_else(|| num("YPIXSZ"));
        let focal_mm = num("FOCALLEN");
        if let (Some(p), Some(f)) = (pix_um, focal_mm)
            && p > 0.2
            && p < 50.0
            && f > 5.0
            && f < 20_000.0
        {
            // ADR-023: drizzled masters (FulgurStack `DRIZZLE = n`) keep the sensor pixel
            // size, but each output pixel covers 1/n of it. Binning is not applied:
            // by the MaxIm DL / N.I.N.A. convention XPIXSZ already includes it.
            let drizzle = num("DRIZZLE")
                .filter(|&d| (1.0..=8.0).contains(&d))
                .unwrap_or(1.0);
            return Some(206.264_806_247 * p / f / drizzle);
        }

        None
    }
}

/// Samples sharing the top value before it counts as a clipped sensor limit.
const SATURATED_SAMPLES: usize = 16;
/// Tops at or above this are 16-bit data, handled by the fixed limits.
const FULL_RANGE_ADU: f32 = 62000.0;

/// Detect where a sub-16-bit sensor clips (e.g. 16383 on a 14-bit camera) when at least
/// 16 samples pile up at the exact maximum integer value.
pub fn saturation_level(pixels: &[f32], bitpix: i32) -> Option<f32> {
    if bitpix <= 0 {
        return None;
    }
    let top = pixels.iter().copied().fold(0.0_f32, f32::max);
    (top > 0.0 && top < FULL_RANGE_ADU)
        .then(|| pixels.iter().filter(|&&v| v == top).count())
        .filter(|&count| count >= SATURATED_SAMPLES)
        .map(|_| top)
}

/// Keywords that reveal the observing site (privacy: redacted by default in the metadata
/// panel and clipboard exports). Covers FITS site cards, geocentric `OBSGEO-*` coordinates,
/// EXIF GPS tags mapped by `exif.rs`, and XISF `Observation:Location:*` properties.
pub fn is_gps_keyword(key: &str) -> bool {
    let upper = key.trim().to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "SITELAT"
            | "SITELONG"
            | "SITEELEV"
            | "SITEALT"
            | "SITELAT_REF"
            | "SITELONG_REF"
            | "OBSGEO-B"
            | "OBSGEO-L"
            | "OBSGEO-H"
            | "OBSGEO-X"
            | "OBSGEO-Y"
            | "OBSGEO-Z"
            | "LAT-OBS"
            | "LONG-OBS"
            | "ALT-OBS"
            | "OBSLAT"
            | "OBSLONG"
            | "OBSELEV"
    ) || upper.contains("LOCATION:LATITUDE")
        || upper.contains("LOCATION:LONGITUDE")
        || upper.contains("LOCATION:ELEVATION")
}

fn keyword(card: &[u8; 80]) -> &str {
    std::str::from_utf8(&card[..8]).unwrap_or("").trim()
}

fn value(card: &[u8; 80]) -> Option<String> {
    if card[8] != b'=' {
        return None;
    }
    let text = std::str::from_utf8(&card[10..]).ok()?;
    let mut quoted = false;
    let mut end = text.len();
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\'' {
            quoted = !quoted;
        } else if byte == b'/' && !quoted {
            end = index;
            break;
        }
    }
    let text = text[..end].trim();
    Some(
        if text.starts_with('\'') && text.ends_with('\'') && text.len() >= 2 {
            text[1..text.len() - 1]
                .replace("''", "'")
                .trim_end()
                .to_owned()
        } else {
            text.to_owned()
        },
    )
}

fn find(cards: &[[u8; 80]], key: &str) -> Option<String> {
    cards
        .iter()
        .find(|card| keyword(card) == key)
        .and_then(value)
}

fn comment(card: &[u8; 80]) -> Option<String> {
    if card[8] != b'=' {
        return None;
    }
    let text = std::str::from_utf8(&card[10..]).ok()?;
    let mut quoted = false;
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\'' {
            quoted = !quoted;
        } else if byte == b'/' && !quoted {
            return Some(text[index + 1..].trim().to_owned());
        }
    }
    None
}

pub(crate) fn exposure_card(cards: &[[u8; 80]], key: &str) -> Option<derive::ExposureCard> {
    use derive::Unit;
    let raw = number(cards, key, None).ok()?;
    let declared = cards
        .iter()
        .find(|card| keyword(card) == key)
        .and_then(comment)
        .and_then(|text| derive::unit_in_comment(&text));
    let camera = find(cards, "INSTRUME").and_then(|text| crate::cameras::identify(&text));
    let (unit, how) = match (declared, camera) {
        (Some(unit), _) => (Some(unit), "unit given in the card's comment".to_owned()),
        (None, Some(camera)) => (
            camera.exposure_unit(),
            format!("as {} writes it, camera catalogue", camera.label()),
        ),
        (None, None) => (None, String::new()),
    };
    Some(match unit {
        Some(Unit::Milliseconds) => derive::ExposureCard {
            seconds: raw / 1000.0,
            note: Some(format!("{} ms, {how}", derive::trim(raw))),
        },
        Some(Unit::Seconds) => derive::ExposureCard {
            seconds: raw,
            note: None,
        },
        None if raw > derive::MAX_PLAUSIBLE_EXPOSURE_S => derive::ExposureCard {
            seconds: raw,
            note: Some(format!(
                "unit unknown: {} is over an hour for one frame, milliseconds?",
                derive::trim(raw)
            )),
        },
        None => derive::ExposureCard {
            seconds: raw,
            note: None,
        },
    })
}

fn number(cards: &[[u8; 80]], key: &str, default: Option<f64>) -> Result<f64> {
    let parsed = match find(cards, key) {
        Some(text) => text.replace(['D', 'd'], "E").parse::<f64>().ok(),
        None => default,
    };
    parsed
        .filter(|value| value.is_finite())
        .ok_or(Error::Invalid(
            "Missing, invalid, or non-finite numeric FITS metadata.",
        ))
}

fn read_cards(reader: &mut impl Read) -> Result<Vec<[u8; 80]>> {
    let mut cards = Vec::new();
    let mut end_found = false;
    for _ in 0..MAX_HEADER_BLOCKS {
        let mut block = [0_u8; BLOCK_SIZE];
        reader.read_exact(&mut block)?;
        for chunk in block.chunks_exact(80) {
            if chunk.iter().any(|byte| !(32..=126).contains(byte)) {
                return Err(Error::Invalid(
                    "FITS header must contain printable ASCII cards.",
                ));
            }
            let card: [u8; 80] = chunk.try_into().expect("80-byte card");
            if keyword(&card) == "END" {
                end_found = true;
                break;
            }
            cards.push(card);
        }
        if end_found {
            break;
        }
    }
    if !end_found {
        return Err(Error::Invalid(
            "FITS header END card not found within limit.",
        ));
    }
    Ok(cards)
}

fn sample_bytes_for_bitpix(bitpix: i32) -> Result<usize> {
    match bitpix {
        8 => Ok(1),
        16 => Ok(2),
        32 | -32 => Ok(4),
        -64 => Ok(8),
        _ => Err(Error::Invalid(
            "Unsupported FITS BITPIX (supported: 8, 16, 32, -32, -64).",
        )),
    }
}

struct DiscoveredHdu {
    /// Cards shown to the user: the HDU's own cards plus inherited primary-header cards.
    cards: Vec<[u8; 80]>,
    /// The HDU's own header cards exactly as stored (used when rewriting the header).
    own_cards: Vec<[u8; 80]>,
    /// File offset of the first header block of this HDU.
    header_start: u64,
    data_offset: u64,
    data_bytes: usize,
    label: String,
}

/// Scan a FITS file for all image HDUs (primary + IMAGE extensions).
fn discover_fits_hdus(
    reader: &mut (impl Read + Seek),
    file_bytes: u64,
) -> Result<Vec<DiscoveredHdu>> {
    reader.seek(SeekFrom::Start(0))?;
    let mut hdus = Vec::new();
    let mut primary_cards = Vec::new();
    for hdu_idx in 0..64 {
        let pos = reader.stream_position()?;
        if pos >= file_bytes {
            break;
        }
        let Ok(cards) = read_cards(reader) else {
            if hdu_idx == 0 {
                return Err(Error::Invalid("Invalid FITS primary header."));
            }
            break;
        };
        if hdu_idx == 0 {
            if cards.first().map(keyword) != Some("SIMPLE")
                || find(&cards, "SIMPLE").as_deref() != Some("T")
            {
                return Err(Error::Invalid(
                    "Not a valid FITS file (missing SIMPLE = T).",
                ));
            }
            primary_cards = cards.clone();
        }
        let data_offset = reader.stream_position()?;
        let bitpix = find(&cards, "BITPIX")
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(0);
        let naxis = find(&cards, "NAXIS")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        let pcount = find(&cards, "PCOUNT")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        let gcount = find(&cards, "GCOUNT")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1);
        let bytes_per_sample = (bitpix.unsigned_abs() as usize) / 8;

        let mut elements = if naxis == 0 { 0 } else { 1usize };
        for ax in 1..=naxis {
            let dim = find(&cards, &format!("NAXIS{ax}"))
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            elements = elements.saturating_mul(dim);
        }
        let total_bytes = elements
            .saturating_add(pcount)
            .saturating_mul(gcount)
            .saturating_mul(bytes_per_sample);
        let padded_bytes = total_bytes.div_ceil(BLOCK_SIZE) * BLOCK_SIZE;

        let is_image_ext = hdu_idx == 0
            || find(&cards, "XTENSION")
                .map(|x| x.eq_ignore_ascii_case("IMAGE"))
                .unwrap_or(false);
        let naxis1 = find(&cards, "NAXIS1")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        let naxis2 = find(&cards, "NAXIS2")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);

        if is_image_ext
            && (2..=3).contains(&naxis)
            && naxis1 >= 2
            && naxis2 >= 2
            && sample_bytes_for_bitpix(bitpix).is_ok()
            && data_offset + padded_bytes as u64 <= file_bytes
        {
            // Inherit missing metadata cards from primary header for MEF extensions
            let own_cards = cards.clone();
            let mut merged = cards;
            if hdu_idx > 0 {
                for pcard in &primary_cards {
                    let k = keyword(pcard);
                    if !k.is_empty()
                        && ![
                            "SIMPLE", "XTENSION", "BITPIX", "NAXIS", "NAXIS1", "NAXIS2", "NAXIS3",
                            "EXTEND", "PCOUNT", "GCOUNT", "BZERO", "BSCALE", "END",
                        ]
                        .contains(&k)
                        && !merged.iter().any(|c| keyword(c) == k)
                    {
                        merged.push(*pcard);
                    }
                }
            }
            let extname = find(&merged, "EXTNAME")
                .or_else(|| find(&merged, "HDUNAME"))
                .unwrap_or_else(|| {
                    if hdu_idx == 0 {
                        "Primary".to_owned()
                    } else {
                        format!("Ext {hdu_idx}")
                    }
                });
            let naxis3 = if naxis == 3 {
                find(&merged, "NAXIS3")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(1)
            } else {
                1
            };
            let label = format!(
                "#{} {} ({}x{}{}, BITPIX {})",
                hdus.len(),
                extname,
                naxis1,
                naxis2,
                if naxis3 > 1 {
                    format!("x{naxis3}")
                } else {
                    String::new()
                },
                bitpix
            );
            hdus.push(DiscoveredHdu {
                cards: merged,
                own_cards,
                header_start: pos,
                data_offset,
                data_bytes: total_bytes,
                label,
            });
        }

        let next_pos = data_offset + padded_bytes as u64;
        if next_pos > file_bytes || next_pos == pos {
            break;
        }
        if reader.seek(SeekFrom::Start(next_pos)).is_err() {
            break;
        }
    }
    if hdus.is_empty() {
        return Err(Error::Invalid("No supported image HDU found in FITS file."));
    }
    Ok(hdus)
}

impl FitsHeader {
    pub fn read_hdu(
        reader: &mut (impl Read + Seek),
        file_bytes: u64,
        file_name: Option<&str>,
        hdu_index: usize,
    ) -> Result<Self> {
        let mut hdus = discover_fits_hdus(reader, file_bytes)?;
        let hdu_names: Vec<String> = hdus.iter().map(|h| h.label.clone()).collect();
        let chosen_idx = if hdu_index < hdus.len() { hdu_index } else { 0 };
        let chosen = hdus.swap_remove(chosen_idx);

        let mut header = Self::from_cards(chosen.cards, file_name, false)?;
        header.data_offset = chosen.data_offset;
        header.data_bytes = chosen.data_bytes;
        header.file_bytes = file_bytes;
        header.hdu_index = chosen_idx;
        header.hdu_names = hdu_names;
        Ok(header)
    }

    fn from_cards(cards: Vec<[u8; 80]>, name: Option<&str>, force_rgb: bool) -> Result<Self> {
        let bitpix = find(&cards, "BITPIX")
            .and_then(|v| v.parse::<i32>().ok())
            .ok_or(Error::Invalid("Missing or invalid BITPIX."))?;
        let bytes_per_sample = sample_bytes_for_bitpix(bitpix)?;
        let naxis = find(&cards, "NAXIS")
            .and_then(|v| v.parse::<usize>().ok())
            .ok_or(Error::Invalid("Missing or invalid NAXIS."))?;
        if !(2..=3).contains(&naxis) {
            return Err(Error::Invalid("FITS image must have 2 or 3 axes."));
        }
        let width = find(&cards, "NAXIS1")
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&w| w >= 2)
            .ok_or(Error::Invalid("Invalid NAXIS1 width."))?;
        let height = find(&cards, "NAXIS2")
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&h| h >= 2)
            .ok_or(Error::Invalid("Invalid NAXIS2 height."))?;
        let planes = if force_rgb {
            3
        } else if naxis == 3 {
            find(&cards, "NAXIS3")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(1)
                .max(1)
        } else {
            1
        };
        let samples = width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(planes))
            .ok_or(Error::Invalid("Image geometry overflows memory size."))?;
        let data_bytes = samples
            .checked_mul(bytes_per_sample)
            .ok_or(Error::Invalid("Image geometry overflows memory size."))?;
        // ADR-023: decoded as f32, so at least 4 bytes per sample; checked against
        // half of the free RAM instead of a fixed cap.
        let working_bytes = samples
            .checked_mul(bytes_per_sample.max(4))
            .ok_or(Error::Invalid("Image geometry overflows memory size."))?;
        crate::check_frame_memory(working_bytes)?;

        let bzero = number(&cards, "BZERO", Some(0.0))?;
        let bscale = number(&cards, "BSCALE", Some(1.0))?;
        if bscale == 0.0 {
            return Err(Error::Invalid("BSCALE must be non-zero."));
        }

        let mut derived = Vec::new();
        let real = |key: &str| number(&cards, key, None).ok();
        let text = |key: &str| find(&cards, key).filter(|v| !v.trim().is_empty());

        let has_exptime = find(&cards, "EXPTIME").is_some();
        let mut exposure_seconds = match number(&cards, "EXPTIME", None) {
            Ok(v) if v >= 0.0 => v,
            _ if !has_exptime => {
                match derive::exposure(|key: &str| exposure_card(&cards, key), name) {
                    Some((v, d)) => {
                        derived.push(d);
                        v
                    }
                    None => 0.0,
                }
            }
            _ => 0.0,
        };
        if has_exptime
            && exposure_seconds > 0.0
            && let Some(card) = exposure_card(&cards, "EXPTIME")
        {
            exposure_seconds = card.seconds;
            derived.extend(derive::unit_note("EXPTIME", &card));
        }

        let rgb = force_rgb || planes >= 3;
        let mut bayer_pattern = if rgb {
            None
        } else {
            find(&cards, "BAYERPAT")
                .filter(|v| !v.trim().is_empty())
                .and_then(|v| BayerPattern::parse(&v).ok())
        };
        // Adjust Bayer phase if XBAYROFF / YBAYROFF are odd
        if let Some(mut pat) = bayer_pattern {
            let xoff = number(&cards, "XBAYROFF", Some(0.0)).unwrap_or(0.0) as isize;
            let yoff = number(&cards, "YBAYROFF", Some(0.0)).unwrap_or(0.0) as isize;
            if xoff.rem_euclid(2) == 1 {
                pat = pat.cols_swapped();
            }
            if yoff.rem_euclid(2) == 1 {
                pat = pat.rows_swapped();
            }
            bayer_pattern = Some(pat);
        }

        let (mosaic, mosaic_source) = match bayer_pattern {
            _ if rgb => (Mosaic::Rgb, "debayered_source"),
            Some(_) => (Mosaic::Bayer, "header"),
            None => match derive::layout(text, name, width, height) {
                Some(layout) => {
                    derived.push(layout.derived);
                    bayer_pattern = layout.pattern;
                    let m = if bayer_pattern.is_some() {
                        Mosaic::Bayer
                    } else {
                        Mosaic::Mono
                    };
                    (m, layout.source)
                }
                None => (Mosaic::Mono, "no_bayer_header"),
            },
        };

        let mut gain = real("GAIN")
            .or_else(|| real("ISOSPEED"))
            .or_else(|| real("ISO"));
        let mut object = text("OBJECT");
        if gain.is_none() {
            gain = derive::gain(name).map(|(v, d)| {
                derived.push(d);
                v
            });
        }
        if object.is_none() {
            object = derive::object(name).map(|(v, d)| {
                derived.push(d);
                v
            });
        }

        let adu_scale = if bitpix == 8 { 257.0 } else { 1.0 };

        let info = FrameInfo {
            width,
            height,
            planes: if rgb { 3 } else { 1 },
            bitpix,
            bzero: bzero as f32,
            bscale: bscale as f32,
            bayer_pattern,
            mosaic,
            mosaic_source,
            exposure_seconds,
            object,
            filter: text("FILTER"),
            date_obs: text("DATE-OBS"),
            gain,
            ccd_temperature: real("CCD-TEMP")
                .or_else(|| real("DET-TEMP"))
                .or_else(|| real("SET-TEMP")),
            instrument: text("INSTRUME").or_else(|| text("CAMERA")),
            telescope: text("TELESCOP"),
            bias_informational: real("BIAS"),
            creator: text("CREATOR").or_else(|| text("SWCREATE")),
            saturation_adu: None,
            derived,
            rows_mirrored: false,
        };
        let bottom_up = find(&cards, "ROWORDER").as_deref() == Some("BOTTOM-UP");
        Ok(Self {
            cards,
            extra_cards: Vec::new(),
            info,
            data_offset: 0,
            data_bytes,
            file_bytes: 0,
            bzero,
            bscale,
            adu_scale,
            xisf: None,
            bottom_up,
            hdu_index: 0,
            hdu_names: vec!["#0 Primary".to_owned()],
        })
    }

    fn from_xisf(path: &Path, name: Option<&str>, hdu_index: usize) -> Result<Self> {
        let opened = xisf::open_index(path, hdu_index)?;
        let rgb = opened.image.channels == 3;
        let mut header = Self::from_cards(opened.cards, name, rgb)?;
        header.extra_cards = opened.extra_cards;
        header.info.bitpix = opened.image.bitpix();
        header.info.bzero = 0.0;
        header.bzero = 0.0;
        header.bscale = 1.0;
        header.adu_scale = 1.0;
        header.file_bytes = std::fs::metadata(path)?.len();
        header.xisf = Some(opened.image);
        header.hdu_index = hdu_index.min(opened.hdu_names.len().saturating_sub(1));
        header.hdu_names = opened.hdu_names;
        Ok(header)
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn read_header(path: &Path) -> Result<FitsHeader> {
    read_header_hdu(path, 0)
}

pub fn read_header_hdu(path: &Path, hdu_index: usize) -> Result<FitsHeader> {
    if xisf::is_xisf(path) {
        return FitsHeader::from_xisf(path, Some(&file_name(path)), hdu_index);
    }
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut reader = BufReader::with_capacity(BLOCK_SIZE, file);
    let mut header = FitsHeader::read_hdu(&mut reader, len, Some(&file_name(path)), hdu_index)?;
    if header.info.bitpix < 0 {
        header.adu_scale = probe_float_adu_scale(&mut reader, &header)?;
    }
    Ok(header)
}

/// Number of evenly spaced rows sampled to classify a floating-point FITS plane.
const FLOAT_PROBE_ROWS: usize = 16;

/// Decide once per HDU whether floating-point samples are stored normalized to `0..1`
/// (PixInsight / FulgurStack masters) and must be scaled to `0..65535` pipeline ADU.
///
/// The decision is made from evenly spaced rows of the first plane, so every reader
/// (full frame, 1:1 ROI, RGB cube) applies the same scale. Deciding per buffer made a
/// dark ROI of an ADU-valued image look normalized and get scaled 65535x.
fn probe_float_adu_scale(reader: &mut (impl Read + Seek), header: &FitsHeader) -> Result<f64> {
    let sample_bytes = sample_bytes_for_bitpix(header.info.bitpix)?;
    let width = header.info.width;
    let height = header.info.height;
    let row_bytes = width
        .checked_mul(sample_bytes)
        .ok_or(Error::Invalid("FITS row size overflow."))?;
    let mut row = vec![0_u8; row_bytes];
    let rows = FLOAT_PROBE_ROWS.min(height).max(1);
    let mut max_v = 0.0_f64;
    for k in 0..rows {
        let y = if rows == 1 {
            0
        } else {
            k * (height - 1) / (rows - 1)
        };
        reader.seek(SeekFrom::Start(
            header.data_offset + (y as u64) * (row_bytes as u64),
        ))?;
        reader.read_exact(&mut row)?;
        for word in row.chunks_exact(sample_bytes) {
            let raw = if sample_bytes == 4 {
                f64::from(f32::from_be_bytes([word[0], word[1], word[2], word[3]]))
            } else {
                f64::from_be_bytes([
                    word[0], word[1], word[2], word[3], word[4], word[5], word[6], word[7],
                ])
            };
            let v = raw * header.bscale + header.bzero;
            if v.is_finite() && v > max_v {
                max_v = v;
            }
        }
    }
    Ok(if max_v > 0.0 && max_v <= 2.0 {
        65535.0
    } else {
        1.0
    })
}

/// Decode a single plane of raw FITS bytes into physical `f32` samples.
fn decode_fits_samples(
    bytes: &[u8],
    bitpix: i32,
    bzero: f64,
    bscale: f64,
    adu_scale: f64,
) -> Vec<f32> {
    match bitpix {
        8 => bytes
            .par_iter()
            .map(|&b| ((f64::from(b) * bscale + bzero) * adu_scale) as f32)
            .collect(),
        16 => bytes
            .par_chunks_exact(2)
            .map(|pair| {
                let raw = i16::from_be_bytes([pair[0], pair[1]]);
                ((f64::from(raw) * bscale + bzero) * adu_scale) as f32
            })
            .collect(),
        32 => bytes
            .par_chunks_exact(4)
            .map(|word| {
                let raw = i32::from_be_bytes([word[0], word[1], word[2], word[3]]);
                ((f64::from(raw) * bscale + bzero) * adu_scale) as f32
            })
            .collect(),
        -32 => bytes
            .par_chunks_exact(4)
            .map(|word| {
                let raw = f32::from_be_bytes([word[0], word[1], word[2], word[3]]);
                finite_or_zero((f64::from(raw) * bscale + bzero) * adu_scale)
            })
            .collect(),
        -64 => bytes
            .par_chunks_exact(8)
            .map(|word| {
                let raw = f64::from_be_bytes(word.try_into().unwrap());
                finite_or_zero((raw * bscale + bzero) * adu_scale)
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Convert a decoded floating-point sample to `f32`, mapping NaN / infinity to `0`.
#[inline]
fn finite_or_zero(v: f64) -> f32 {
    let v = v as f32;
    if v.is_finite() { v } else { 0.0 }
}

pub fn read_raw(path: &Path) -> Result<RawFrame> {
    read_raw_hdu(path, 0)
}

pub fn read_raw_hdu(path: &Path, hdu_index: usize) -> Result<RawFrame> {
    let header = read_header_hdu(path, hdu_index)?;
    read_raw_from_header(path, header)
}

pub fn read_raw_from_header(path: &Path, header: FitsHeader) -> Result<RawFrame> {
    read_raw_from_header_cancellable(path, header, &AtomicBool::new(false))
}

/// Read the full first plane, checking `cancel` between large chunks so an abandoned
/// navigation stops reading (and frees its buffer) quickly.
pub fn read_raw_from_header_cancellable(
    path: &Path,
    header: FitsHeader,
    cancel: &AtomicBool,
) -> Result<RawFrame> {
    if let Some(image) = &header.xisf {
        let pixels = image.read_plane(0, cancel)?;
        return Ok(RawFrame { header, pixels });
    }
    let sample_bytes = sample_bytes_for_bitpix(header.info.bitpix)?;
    let plane_bytes = header
        .info
        .width
        .checked_mul(header.info.height)
        .and_then(|n| n.checked_mul(sample_bytes))
        .ok_or(Error::Invalid("Plane byte size overflow."))?;

    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(header.data_offset))?;
    let mut bytes = crate::try_alloc_zeroed::<u8>(plane_bytes)?;
    crate::read_exact_cancellable(&mut file, &mut bytes, cancel)?;
    check_cancelled(cancel)?;

    let mut pixels = decode_fits_samples(
        &bytes,
        header.info.bitpix,
        header.bzero,
        header.bscale,
        header.adu_scale,
    );
    if header.bottom_up {
        flip_rows(&mut pixels, header.info.width);
    }
    Ok(RawFrame { header, pixels })
}

pub fn flip_rows(pixels: &mut [f32], width: usize) {
    if width == 0 {
        return;
    }
    let height = pixels.len() / width;
    for y in 0..height / 2 {
        let (upper, lower) = pixels.split_at_mut((height - 1 - y) * width);
        upper[y * width..(y + 1) * width].swap_with_slice(&mut lower[..width]);
    }
}

/// Read a planar 3-channel RGB image (or ROI) from either XISF or 3D FITS (`NAXIS3 >= 3`).
pub fn read_rgb_roi(
    path: &Path,
    source: &FitsHeader,
    [x, y, width, height]: [usize; 4],
    cancel: &AtomicBool,
) -> Result<RgbImage> {
    if width == 0
        || height == 0
        || x.checked_add(width).is_none_or(|v| v > source.info.width)
        || y.checked_add(height).is_none_or(|v| v > source.info.height)
    {
        return Err(Error::Invalid("Invalid RGB ROI bounds."));
    }
    if let Some(image) = &source.xisf {
        let mut pixels = vec![[0.0_f32; 3]; width * height];
        let mut plane = vec![0.0_f32; width * height];
        let stored_y = if source.bottom_up {
            source.info.height - y - height
        } else {
            y
        };
        for channel in 0..3 {
            image.read_region(
                channel.min(image.channels - 1),
                [x, stored_y, width, height],
                &mut plane,
                cancel,
            )?;
            if source.bottom_up {
                flip_rows(&mut plane, width);
            }
            for (pixel, &value) in pixels.iter_mut().zip(&plane) {
                pixel[channel] = value;
            }
        }
        return Ok(RgbImage {
            width,
            height,
            demosaic_method: DemosaicMethod::External,
            pixels,
        });
    }

    // 3D FITS cube (`NAXIS3 >= 3`)
    let sample_bytes = sample_bytes_for_bitpix(source.info.bitpix)?;
    let full_w = source.info.width;
    let full_h = source.info.height;
    let plane_samples = full_w * full_h;
    let stored_y = if source.bottom_up {
        full_h - y - height
    } else {
        y
    };

    let mut file = File::open(path)?;
    let mut pixels = vec![[0.0_f32; 3]; width * height];
    let mut row_bytes = vec![0u8; width * sample_bytes];

    for channel in 0..3 {
        let ch_idx = channel.min(source.info.planes.saturating_sub(1));
        let plane_offset = source.data_offset + (ch_idx * plane_samples * sample_bytes) as u64;
        for row in 0..height {
            check_cancelled(cancel)?;
            let src_row = stored_y + row;
            let row_pos = plane_offset + ((src_row * full_w + x) * sample_bytes) as u64;
            file.seek(SeekFrom::Start(row_pos))?;
            file.read_exact(&mut row_bytes)?;

            let target_row = if source.bottom_up {
                height - 1 - row
            } else {
                row
            };
            let dest_slice = &mut pixels[target_row * width..(target_row + 1) * width];
            decode_row_channel(
                &row_bytes,
                source.info.bitpix,
                source.bzero,
                source.bscale,
                source.adu_scale,
                channel,
                dest_slice,
            );
        }
    }

    Ok(RgbImage {
        width,
        height,
        demosaic_method: DemosaicMethod::External,
        pixels,
    })
}

#[allow(clippy::too_many_arguments)]
fn decode_row_channel(
    bytes: &[u8],
    bitpix: i32,
    bzero: f64,
    bscale: f64,
    adu_scale: f64,
    channel: usize,
    dest: &mut [[f32; 3]],
) {
    match bitpix {
        8 => {
            for (px, &b) in dest.iter_mut().zip(bytes) {
                px[channel] = ((f64::from(b) * bscale + bzero) * adu_scale) as f32;
            }
        }
        16 => {
            for (px, pair) in dest.iter_mut().zip(bytes.chunks_exact(2)) {
                let raw = i16::from_be_bytes([pair[0], pair[1]]);
                px[channel] = ((f64::from(raw) * bscale + bzero) * adu_scale) as f32;
            }
        }
        32 => {
            for (px, word) in dest.iter_mut().zip(bytes.chunks_exact(4)) {
                let raw = i32::from_be_bytes([word[0], word[1], word[2], word[3]]);
                px[channel] = ((f64::from(raw) * bscale + bzero) * adu_scale) as f32;
            }
        }
        -32 => {
            for (px, word) in dest.iter_mut().zip(bytes.chunks_exact(4)) {
                let raw = f32::from_be_bytes([word[0], word[1], word[2], word[3]]);
                px[channel] = finite_or_zero((f64::from(raw) * bscale + bzero) * adu_scale);
            }
        }
        -64 => {
            for (px, word) in dest.iter_mut().zip(bytes.chunks_exact(8)) {
                let raw = f64::from_be_bytes(word.try_into().unwrap());
                px[channel] = finite_or_zero((raw * bscale + bzero) * adu_scale);
            }
        }
        _ => {}
    }
}

/// Read a single-plane (CFA or Mono) ROI from FITS or XISF, preserving Bayer phase.
pub fn read_roi(
    path: &Path,
    source: &FitsHeader,
    bounds: [usize; 4],
    cancel: &AtomicBool,
) -> Result<RawFrame> {
    let [x, y, width, height] = bounds;
    if x % 2 != 0
        || y % 2 != 0
        || width < 6
        || height < 6
        || x.checked_add(width).is_none_or(|v| v > source.info.width)
        || y.checked_add(height).is_none_or(|v| v > source.info.height)
    {
        return Err(Error::Invalid("Invalid phase-preserving CFA ROI."));
    }

    if let Some(image) = &source.xisf {
        let mut pixels = vec![0.0_f32; width * height];
        let stored_y = if source.bottom_up {
            source.info.height - y - height
        } else {
            y
        };
        image.read_region(0, [x, stored_y, width, height], &mut pixels, cancel)?;
        if source.bottom_up {
            flip_rows(&mut pixels, width);
        }
        let mut header = source.clone();
        header.info.width = width;
        header.info.height = height;
        return Ok(RawFrame { header, pixels });
    }

    let sample_bytes = sample_bytes_for_bitpix(source.info.bitpix)?;
    let mut file = File::open(path)?;
    let stored_y = if source.bottom_up {
        source.info.height - y - height
    } else {
        y
    };
    file.seek(SeekFrom::Start(
        source.data_offset + ((stored_y * source.info.width + x) * sample_bytes) as u64,
    ))?;
    let mut file = BufReader::with_capacity(ROI_READ_BUFFER_BYTES, file);
    let mut pixels = vec![0.0_f32; width * height];
    let mut row_bytes = vec![0u8; width * sample_bytes];

    for row in 0..height {
        check_cancelled(cancel)?;
        file.read_exact(&mut row_bytes)?;
        let out_row = &mut pixels[row * width..(row + 1) * width];
        match source.info.bitpix {
            8 => {
                for (out, &b) in out_row.iter_mut().zip(&row_bytes) {
                    *out =
                        ((f64::from(b) * source.bscale + source.bzero) * source.adu_scale) as f32;
                }
            }
            16 => {
                for (out, pair) in out_row.iter_mut().zip(row_bytes.chunks_exact(2)) {
                    let raw = i16::from_be_bytes([pair[0], pair[1]]);
                    *out =
                        ((f64::from(raw) * source.bscale + source.bzero) * source.adu_scale) as f32;
                }
            }
            32 => {
                for (out, word) in out_row.iter_mut().zip(row_bytes.chunks_exact(4)) {
                    let raw = i32::from_be_bytes([word[0], word[1], word[2], word[3]]);
                    *out =
                        ((f64::from(raw) * source.bscale + source.bzero) * source.adu_scale) as f32;
                }
            }
            -32 => {
                for (out, word) in out_row.iter_mut().zip(row_bytes.chunks_exact(4)) {
                    let raw = f32::from_be_bytes([word[0], word[1], word[2], word[3]]);
                    *out = finite_or_zero(
                        (f64::from(raw) * source.bscale + source.bzero) * source.adu_scale,
                    );
                }
            }
            -64 => {
                for (out, word) in out_row.iter_mut().zip(row_bytes.chunks_exact(8)) {
                    let raw = f64::from_be_bytes(word.try_into().unwrap());
                    *out = finite_or_zero((raw * source.bscale + source.bzero) * source.adu_scale);
                }
            }
            _ => {}
        }
        if row + 1 < height {
            file.seek_relative(((source.info.width - width) * sample_bytes) as i64)?;
        }
    }
    if source.bottom_up {
        flip_rows(&mut pixels, width);
    }
    let mut header = source.clone();
    header.info.width = width;
    header.info.height = height;
    Ok(RawFrame { header, pixels })
}

fn parse_sexagesimal_parts(text: &str) -> Option<(f64, Vec<f64>)> {
    let mut s = text.trim();
    if s.is_empty() {
        return None;
    }
    let sign = if let Some(rest) = s.strip_prefix('-') {
        s = rest;
        -1.0
    } else if let Some(rest) = s.strip_prefix('+') {
        s = rest;
        1.0
    } else {
        1.0
    };
    let parts: Vec<f64> = s
        .split(|c: char| {
            c.is_whitespace() || matches!(c, ':' | 'h' | 'H' | 'm' | 'M' | 's' | 'S' | 'd' | 'D')
        })
        .filter(|tok| !tok.is_empty())
        .map(|tok| tok.parse::<f64>().ok())
        .collect::<Option<Vec<_>>>()?;
    if parts.is_empty() {
        return None;
    }
    Some((sign, parts))
}

fn parse_ra_deg(text: &str, default_hours: bool) -> Option<f64> {
    let trimmed = text.trim();
    if let Ok(val) = trimmed.replace(['D', 'd'], "E").parse::<f64>()
        && val.is_finite()
    {
        let deg = if default_hours && (0.0..=24.0).contains(&val) {
            val * 15.0
        } else {
            val
        };
        return (0.0..=360.0).contains(&deg).then(|| deg.rem_euclid(360.0));
    }
    let (sign, parts) = parse_sexagesimal_parts(trimmed)?;
    if sign < 0.0 || parts.len() < 2 {
        return None;
    }
    let h = parts[0];
    let m = parts.get(1).copied().unwrap_or(0.0);
    let sec = parts.get(2).copied().unwrap_or(0.0);
    let hours = h + m / 60.0 + sec / 3600.0;
    (0.0..=24.0)
        .contains(&hours)
        .then(|| (hours * 15.0).rem_euclid(360.0))
}

fn parse_dec_deg(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if let Ok(val) = trimmed.replace(['D', 'd'], "E").parse::<f64>()
        && val.is_finite()
    {
        return (-90.0..=90.0).contains(&val).then_some(val);
    }
    let (sign, parts) = parse_sexagesimal_parts(trimmed)?;
    let d = parts[0].abs();
    let m = parts.get(1).copied().unwrap_or(0.0);
    let sec = parts.get(2).copied().unwrap_or(0.0);
    let deg = sign * (d + m / 60.0 + sec / 3600.0);
    (-90.0..=90.0).contains(&deg).then_some(deg)
}

/// Returns `true` if `key` is a FITS WCS or SIP distortion card that should be
/// replaced when saving a fresh plate solution.
pub fn is_wcs_keyword(key: &str) -> bool {
    let k = key.trim().to_ascii_uppercase();
    if matches!(
        k.as_str(),
        "WCSAXES"
            | "CTYPE1"
            | "CTYPE2"
            | "CUNIT1"
            | "CUNIT2"
            | "RADESYS"
            | "RADECSYS"
            | "EQUINOX"
            | "LONPOLE"
            | "LATPOLE"
            | "CRVAL1"
            | "CRVAL2"
            | "CRPIX1"
            | "CRPIX2"
            | "CD1_1"
            | "CD1_2"
            | "CD2_1"
            | "CD2_2"
            | "CDELT1"
            | "CDELT2"
            | "CROTA1"
            | "CROTA2"
            | "PC1_1"
            | "PC1_2"
            | "PC2_1"
            | "PC2_2"
            | "A_ORDER"
            | "B_ORDER"
            | "AP_ORDER"
            | "BP_ORDER"
            | "A_DMAX"
            | "B_DMAX"
    ) {
        return true;
    }
    for prefix in ["A_", "B_", "AP_", "BP_", "PV1_", "PV2_", "PC00", "CD00"] {
        if let Some(rest) = k.strip_prefix(prefix)
            && rest.chars().all(|c| c.is_ascii_digit() || c == '_')
        {
            return true;
        }
    }
    false
}

/// Format an 80-byte ASCII FITS header card from `(keyword, value, comment)`.
pub fn format_fits_card(key: &str, val: &str, com: &str) -> [u8; 80] {
    let mut card = [b' '; 80];
    let key_upper = key.trim().to_ascii_uppercase();
    if key_upper == "END" {
        card[..3].copy_from_slice(b"END");
        return card;
    }
    if matches!(key_upper.as_str(), "HISTORY" | "COMMENT") {
        let line = if val.is_empty() {
            format!("{key_upper:<8}{com}")
        } else if com.is_empty() {
            format!("{key_upper:<8}{val}")
        } else {
            format!("{key_upper:<8}{val} {com}")
        };
        let clean: String = line
            .chars()
            .map(|c| if (' '..='~').contains(&c) { c } else { '?' })
            .collect();
        let n = clean.len().min(80);
        card[..n].copy_from_slice(&clean.as_bytes()[..n]);
        return card;
    }

    let formatted_val = if val.starts_with('\'') {
        format!("{val:<20}")
    } else {
        format!("{val:>20}")
    };
    let line = if com.is_empty() {
        format!("{key_upper:<8}= {formatted_val}")
    } else {
        format!("{key_upper:<8}= {formatted_val} / {com}")
    };
    let clean: String = line
        .chars()
        .map(|c| if (' '..='~').contains(&c) { c } else { '?' })
        .collect();
    let n = clean.len().min(80);
    card[..n].copy_from_slice(&clean.as_bytes()[..n]);
    card
}

/// Atomically write solved WCS cards `(keyword, value, comment)` into an existing
/// FITS or XISF file at `path` via `<filename>.tmp`, `sync_all()`, and `rename`.
///
/// All original pixel data bytes and non-WCS metadata are preserved bit-for-bit.
pub fn save_wcs_to_file(
    path: &Path,
    hdu_index: usize,
    wcs_cards: &[(String, String, String)],
) -> Result<()> {
    if xisf::is_xisf(path) {
        return xisf::save_wcs_to_xisf(path, hdu_index, wcs_cards);
    }
    save_wcs_to_fits(path, hdu_index, wcs_cards)
}

fn save_wcs_to_fits(
    path: &Path,
    target_image_hdu: usize,
    wcs_cards: &[(String, String, String)],
) -> Result<()> {
    use std::io::{BufWriter, Write};

    let mut src = File::open(path)?;
    let file_bytes = src.metadata()?.len();
    let mut reader = BufReader::with_capacity(64 * 1024, &mut src);

    // Use the exact same HDU enumeration as the reader, so the index the user sees in the
    // HDU selector is the HDU that gets written. An unknown index is an error: writing the
    // WCS of one HDU into another would silently corrupt the astrometry.
    let hdus = discover_fits_hdus(&mut reader, file_bytes)?;
    let hdu_count = hdus.len();
    let Some(target) = hdus.into_iter().nth(target_image_hdu) else {
        return Err(Error::Invalid(
            "Target FITS image HDU not found; WCS was not saved.",
        ));
    };
    let header_start = target.header_start;
    let target_data_offset = target.data_offset;
    let mut target_cards = target.own_cards;

    // Filter out existing WCS cards and previous FulgurPreView WCS history line
    target_cards.retain(|card| {
        let k = keyword(card);
        if is_wcs_keyword(k) {
            return false;
        }
        if k.eq_ignore_ascii_case("HISTORY")
            && let Ok(text) = std::str::from_utf8(&card[8..])
            && (text.contains("FulgurPreView WCS") || text.contains("FulgurStack WCS"))
        {
            return false;
        }
        true
    });

    for (k, v, c) in wcs_cards {
        target_cards.push(format_fits_card(k, v, c));
    }
    target_cards.push(format_fits_card("END", "", ""));

    let mut new_header_bytes = Vec::with_capacity(target_cards.len().div_ceil(36) * BLOCK_SIZE);
    for c in &target_cards {
        new_header_bytes.extend_from_slice(c);
    }
    while new_header_bytes.len() % BLOCK_SIZE != 0 {
        new_header_bytes.push(b' ');
    }
    let new_data_offset = header_start + new_header_bytes.len() as u64;

    let (tmp_path, out_file) = create_sibling_tmp(path)?;

    let write_res = (|| -> Result<()> {
        let mut writer = BufWriter::with_capacity(256 * 1024, out_file);

        // 1. Copy everything before the target HDU header
        reader.seek(SeekFrom::Start(0))?;
        if header_start > 0 {
            let mut prefix = (&mut reader).take(header_start);
            std::io::copy(&mut prefix, &mut writer)?;
        }

        // 2. Write the updated 2880-byte-padded FITS header
        writer.write_all(&new_header_bytes)?;

        // 3. Copy the untouched pixel data (and any trailing extensions) bit-for-bit
        reader.seek(SeekFrom::Start(target_data_offset))?;
        std::io::copy(&mut reader, &mut writer)?;

        writer.flush()?;
        let inner = writer.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        inner.sync_all()?;
        drop(inner);

        // 4. Verify before replacing the original: pixel data and everything after it must
        //    be byte-identical, and the rewritten file must still parse with the same HDUs.
        verify_same_tail(path, target_data_offset, &tmp_path, new_data_offset)?;
        let tmp_file = File::open(&tmp_path)?;
        let tmp_len = tmp_file.metadata()?.len();
        let reparsed = discover_fits_hdus(&mut BufReader::new(tmp_file), tmp_len)?;
        let wcs_present = reparsed
            .get(target_image_hdu)
            .is_some_and(|h| find(&h.own_cards, "CRVAL1").is_some());
        if reparsed.len() != hdu_count || !wcs_present {
            return Err(Error::Invalid(
                "Verification of the rewritten FITS file failed; original left unchanged.",
            ));
        }
        Ok(())
    })();

    drop(reader);
    drop(src);

    if let Err(err) = write_res {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Err(err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(Error::Io(err));
    }

    Ok(())
}

/// Create a new, uniquely named temporary file next to `path` (same directory, so the final
/// rename is atomic). Never deletes or overwrites an existing file.
pub(crate) fn create_sibling_tmp(path: &Path) -> Result<(std::path::PathBuf, File)> {
    let stem = path
        .file_stem()
        .ok_or(Error::Invalid("Path has no file name."))?
        .to_string_lossy()
        .into_owned();
    // Keep the original extension: readers (e.g. XISF `.xish`) decide the layout from it,
    // and the rewritten file is re-read for verification before it replaces the original.
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let pid = std::process::id();
    for attempt in 0..100_u32 {
        let tmp_path = path.with_file_name(format!(".{stem}.fulgur-tmp-{pid}-{attempt}{ext}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(file) => return Ok((tmp_path, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(Error::Io(err)),
        }
    }
    Err(Error::Invalid(
        "Could not create a temporary file next to the image.",
    ))
}

/// Check that `a` from `a_offset` to its end is byte-identical to `b` from `b_offset` to its end.
pub(crate) fn verify_same_tail(a: &Path, a_offset: u64, b: &Path, b_offset: u64) -> Result<()> {
    let mut fa = File::open(a)?;
    let mut fb = File::open(b)?;
    let len_a = fa.metadata()?.len();
    let len_b = fb.metadata()?.len();
    if len_a < a_offset || len_b < b_offset || len_a - a_offset != len_b - b_offset {
        return Err(Error::Invalid(
            "Verification failed: pixel data size changed; original left unchanged.",
        ));
    }
    fa.seek(SeekFrom::Start(a_offset))?;
    fb.seek(SeekFrom::Start(b_offset))?;
    let mut ra = BufReader::with_capacity(1 << 20, fa);
    let mut rb = BufReader::with_capacity(1 << 20, fb);
    let mut buf_a = vec![0_u8; 1 << 20];
    let mut buf_b = vec![0_u8; 1 << 20];
    let mut remaining = len_a - a_offset;
    while remaining > 0 {
        let n = remaining.min(buf_a.len() as u64) as usize;
        ra.read_exact(&mut buf_a[..n])?;
        rb.read_exact(&mut buf_b[..n])?;
        if buf_a[..n] != buf_b[..n] {
            return Err(Error::Invalid(
                "Verification failed: pixel data differs; original left unchanged.",
            ));
        }
        remaining -= n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gps_keywords_cover_site_and_geocentric_cards() {
        for key in [
            "SITELAT",
            "sitelong",
            "SITEELEV",
            "SITEALT",
            "OBSGEO-X",
            "OBSGEO-Y",
            "OBSGEO-Z",
            "OBSGEO-H",
            "Observation:Location:Latitude",
            "Observation:Location:Longitude",
            "Observation:Location:Elevation",
        ] {
            assert!(is_gps_keyword(key), "{key} must be redacted");
        }
        for key in ["OBJECT", "RA", "DEC", "EXPTIME", "CRVAL1"] {
            assert!(!is_gps_keyword(key), "{key} must stay visible");
        }
    }
    fn header(extra: &[(&str, &str)]) -> FitsHeader {
        let mut cards: Vec<[u8; 80]> = Vec::new();
        let base = [
            ("SIMPLE", "T"),
            ("BITPIX", "-32"),
            ("NAXIS", "2"),
            ("NAXIS1", "64"),
            ("NAXIS2", "48"),
        ];
        for (k, v) in base.iter().chain(extra) {
            let mut card = [b' '; 80];
            let text = format!("{k:<8}= {v:>20}");
            card[..text.len()].copy_from_slice(text.as_bytes());
            cards.push(card);
        }
        FitsHeader::from_cards(cards, None, false).unwrap()
    }

    /// ADR-023: optics-derived scale divides by FulgurStack's DRIZZLE factor; a WCS CD
    /// matrix (already drizzle-scaled) still wins.
    #[test]
    fn drizzle_factor_applies_to_optics_scale_only() {
        let optics = [("XPIXSZ", "2.9"), ("FOCALLEN", "160.0")];
        let native = header(&optics).pixel_scale_arcsec().unwrap();
        assert!((native - 3.7385).abs() < 1e-3, "{native}");

        let drizzled = header(&[optics[0], optics[1], ("DRIZZLE", "2")])
            .pixel_scale_arcsec()
            .unwrap();
        assert!((drizzled - native / 2.0).abs() < 1e-9);

        let with_wcs = header(&[
            optics[0],
            optics[1],
            ("DRIZZLE", "2"),
            ("CD1_1", "-5.119126576435533E-4"),
            ("CD1_2", "1.115204166856967E-5"),
        ])
        .pixel_scale_arcsec()
        .unwrap();
        assert!((with_wcs - 1.8433).abs() < 1e-3, "{with_wcs}");
    }
}
