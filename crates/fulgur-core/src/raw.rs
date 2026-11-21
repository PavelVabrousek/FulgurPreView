//! Pure-Rust Two-Stage DSLR & Mirrorless Camera RAW reader
//! (`.cr2`, `.cr3`, `.nef`, `.arw`, `.dng`, `.orf`, `.rw2`, `.pef`, `.raf`).
//!
//! - **Stage 1 (`read_raw_stage1_preview`):** Instant (`< 20 ms`) extraction of the largest
//!   embedded JPEG preview (or fast strided CFA superpixel preview) plus full TIFF/EXIF/RAW
//!   metadata cards so rapid arrow-key browsing never stalls.
//! - **Stage 2 (`read_raw_stage2_linear`):** Background decoding of the raw 8/10/12/14/16-bit
//!   sensor CFA plane (uncompressed, packed 10/12/14-bit, Lossless JPEG `SOF3` with CR2 slicing,
//!   Deflate, Sony ARW2 delta, or linearized RGB fallback) into physical `[0..65535]` `f32` ADU.

use crate::cameras;
use crate::fits::{FitsHeader, FrameInfo};
use crate::standard::StandardFrame;
use crate::{
    BayerPattern, DemosaicMethod, Error, HeaderCard, LayoutChoice, Mosaic, RawFrame, Result,
    RgbImage, check_cancelled,
};
use image::ImageReader;
use std::io::Cursor;
use std::path::Path;
use std::sync::atomic::AtomicBool;

pub const RAW_EXTENSIONS: [&str; 9] = [
    "cr2", "cr3", "nef", "arw", "dng", "orf", "rw2", "pef", "raf",
];

pub fn is_raw_image(path: &Path) -> bool {
    path.extension().is_some_and(|ext| {
        RAW_EXTENSIONS
            .iter()
            .any(|expected| ext.eq_ignore_ascii_case(expected))
    }) && !path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with("._"))
}

/// Which stage of the Two-Stage RAW pipeline produced an `OverviewFrame`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawStage {
    /// Not a camera RAW file (e.g. FITS, XISF, TIFF, JPEG, PNG).
    NotRaw,
    /// Stage 1: Instant embedded preview (`< 20 ms`), waiting for background linear decode.
    Stage1Preview,
    /// Stage 2: Full linear sensor CFA decoded in background.
    Stage2Linear,
    /// Stage 2 finished, but this RAW's sensor data could not be decoded (unsupported
    /// compression such as Canon CR3 / CRX or Nikon compressed NEF). The image shown is the
    /// camera's embedded JPEG, de-gamma'd: it is white-balanced, sharpened and compressed,
    /// so it must not be used for star metrics (FWHM / HFR / tilt).
    Stage2PreviewOnly,
}

/// Result of Stage-2 background RAW decoding.
pub enum RawStage2Output {
    /// True single-channel linear `f32` Bayer/Mono CFA sensor data in `[0..65535]` ADU.
    Cfa(RawFrame),
    /// Linearized 3-channel `f32` RGB data in `[0..65535]` ADU, reconstructed from the
    /// embedded JPEG preview because the sensor data could not be decoded. Not real
    /// linear sensor data: see [`RawStage::Stage2PreviewOnly`].
    LinearRgb(StandardFrame),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Endian {
    Little,
    Big,
}

impl Endian {
    #[inline]
    fn u16(self, b: &[u8], off: usize) -> Option<u16> {
        let s = b.get(off..off + 2)?;
        Some(match self {
            Self::Little => u16::from_le_bytes([s[0], s[1]]),
            Self::Big => u16::from_be_bytes([s[0], s[1]]),
        })
    }

    #[inline]
    fn u32(self, b: &[u8], off: usize) -> Option<u32> {
        let s = b.get(off..off + 4)?;
        Some(match self {
            Self::Little => u32::from_le_bytes([s[0], s[1], s[2], s[3]]),
            Self::Big => u32::from_be_bytes([s[0], s[1], s[2], s[3]]),
        })
    }
}

#[derive(Clone, Debug, Default)]
struct IfdInfo {
    subfile_type: u32,
    width: usize,
    height: usize,
    bits_per_sample: u16,
    samples_per_pixel: u16,
    compression: u16,
    photometric: u16,
    strip_offsets: Vec<u32>,
    strip_byte_counts: Vec<u32>,
    tile_width: usize,
    tile_height: usize,
    tile_offsets: Vec<u32>,
    tile_byte_counts: Vec<u32>,
    jpeg_offset: u32,
    jpeg_length: u32,
    cfa_pattern: Option<BayerPattern>,
    black_level: [f32; 4],
    white_level: Option<f32>,
    active_area: Option<[usize; 4]>, // [top, left, bottom, right]
    cr2_slices: Option<[u16; 3]>,
    linearization_table: Vec<u16>,
    sony_raw_offset: u32,
    sony_raw_length: u32,
}

#[derive(Clone, Debug, Default)]
struct RawContainerMeta {
    endian: Option<Endian>,
    make: Option<String>,
    model: Option<String>,
    software: Option<String>,
    date_obs: Option<String>,
    lens: Option<String>,
    exposure_s: Option<f64>,
    iso: Option<f64>,
    fnumber: Option<f64>,
    focal_mm: Option<f64>,
    ifds: Vec<IfdInfo>,
    extra_jpegs: Vec<(usize, usize)>,
}

/// Stage 1: Instantly extract the embedded preview image and full metadata from a RAW file.
pub fn read_raw_stage1_preview(
    path: &Path,
    layout: LayoutChoice,
    cancel: &AtomicBool,
) -> Result<StandardFrame> {
    check_cancelled(cancel)?;
    let bytes = crate::read_file_cancellable(path, cancel)?;
    check_cancelled(cancel)?;

    let meta = parse_raw_container(&bytes);
    let bayer_pattern = resolve_raw_bayer_pattern(path, &meta, layout);
    let cards = build_raw_cards(path, &meta, bayer_pattern, "Stage 1 (Embedded Preview)");

    // 1. Try decoding the largest embedded JPEG preview stream
    if let Some((jpeg_off, jpeg_len)) = find_best_embedded_jpeg(&bytes, &meta) {
        if let Some(slice) = bytes.get(jpeg_off..jpeg_off.saturating_add(jpeg_len)) {
            if let Ok(reader) = ImageReader::new(Cursor::new(slice)).with_guessed_format() {
                if let Ok(dyn_img) = reader.decode() {
                    check_cancelled(cancel)?;
                    let rgb8 = dyn_img.to_rgb8();
                    let width = rgb8.width() as usize;
                    let height = rgb8.height() as usize;
                    let raw_bytes = rgb8.into_raw();
                    let mut pixels = Vec::with_capacity(width * height);
                    for chunk in raw_bytes.chunks_exact(3) {
                        pixels.push([
                            f32::from(chunk[0]),
                            f32::from(chunk[1]),
                            f32::from(chunk[2]),
                        ]);
                    }
                    let info = build_frame_info(width, height, 8, &meta, bayer_pattern, true);
                    return Ok(StandardFrame {
                        rgb: RgbImage {
                            width,
                            height,
                            demosaic_method: DemosaicMethod::External,
                            pixels,
                        },
                        info,
                        cards,
                        default_stretch: false, // Embedded JPEG is already sRGB tone-mapped
                    });
                }
            }
        }
    }

    // 2. If no embedded JPEG exists (e.g. pure linear/CFA DNG), decode Stage 2 directly
    match decode_stage2_from_bytes(path, &bytes, &meta, layout, cancel)? {
        RawStage2Output::Cfa(raw) => {
            let w = raw.header.info.width;
            let h = raw.header.info.height;
            let mut pixels = Vec::with_capacity((w / 2).max(1) * (h / 2).max(1));
            let pat = raw.header.info.bayer_pattern.unwrap_or(BayerPattern::RGGB);
            let qw = (w / 2).max(1);
            let qh = (h / 2).max(1);
            for qy in 0..qh {
                let y = (qy * 2).min(h - 1);
                let y1 = (y + 1).min(h - 1);
                for qx in 0..qw {
                    let x = (qx * 2).min(w - 1);
                    let x1 = (x + 1).min(w - 1);
                    let p00 = raw.pixels[y * w + x];
                    let p10 = raw.pixels[y * w + x1];
                    let p01 = raw.pixels[y1 * w + x];
                    let p11 = raw.pixels[y1 * w + x1];
                    let rgb = match pat {
                        BayerPattern::RGGB => [p00, 0.5 * (p10 + p01), p11],
                        BayerPattern::BGGR => [p11, 0.5 * (p10 + p01), p00],
                        BayerPattern::GRBG => [p10, 0.5 * (p00 + p11), p01],
                        BayerPattern::GBRG => [p01, 0.5 * (p00 + p11), p10],
                    };
                    pixels.push(rgb);
                }
            }
            let info = build_frame_info(qw, qh, 16, &meta, Some(pat), true);
            Ok(StandardFrame {
                rgb: RgbImage {
                    width: qw,
                    height: qh,
                    demosaic_method: DemosaicMethod::Superpixel,
                    pixels,
                },
                info,
                cards,
                default_stretch: true,
            })
        }
        RawStage2Output::LinearRgb(std_frame) => Ok(std_frame),
    }
}

/// Stage 2: Decode the raw linear sensor CFA plane (or linear RGB fallback) in the background.
pub fn read_raw_stage2_linear(
    path: &Path,
    layout: LayoutChoice,
    cancel: &AtomicBool,
) -> Result<RawStage2Output> {
    check_cancelled(cancel)?;
    let bytes = crate::read_file_cancellable(path, cancel)?;
    check_cancelled(cancel)?;
    let meta = parse_raw_container(&bytes);
    decode_stage2_from_bytes(path, &bytes, &meta, layout, cancel)
}

fn decode_stage2_from_bytes(
    path: &Path,
    bytes: &[u8],
    meta: &RawContainerMeta,
    layout: LayoutChoice,
    cancel: &AtomicBool,
) -> Result<RawStage2Output> {
    let bayer_pattern = resolve_raw_bayer_pattern(path, meta, layout);
    let endian = meta.endian.unwrap_or(Endian::Little);

    // Find the best full-resolution CFA IFD (highest pixel count with 1 sample/pixel or CFA photometric)
    let cfa_ifd = meta
        .ifds
        .iter()
        .filter(|ifd| {
            ifd.width >= 16
                && ifd.height >= 16
                && (ifd.samples_per_pixel <= 1
                    || ifd.photometric == 32803
                    || ifd.photometric == 34892)
                && (!ifd.strip_offsets.is_empty()
                    || !ifd.tile_offsets.is_empty()
                    || ifd.sony_raw_offset > 0)
        })
        .max_by_key(|ifd| ifd.width * ifd.height);

    if let Some(ifd) = cfa_ifd {
        if let Some(mut cfa_u16) = decode_cfa_ifd_pixels(bytes, endian, ifd, cancel) {
            check_cancelled(cancel)?;
            // Apply optional linearization LUT
            if !ifd.linearization_table.is_empty() {
                let lut = &ifd.linearization_table;
                let last = *lut.last().unwrap_or(&65535);
                for px in &mut cfa_u16 {
                    let idx = usize::from(*px);
                    *px = lut.get(idx).copied().unwrap_or(last);
                }
            }

            // Apply ActiveArea crop if specified and valid
            let mut width = ifd.width;
            let mut height = ifd.height;
            let mut pattern = ifd
                .cfa_pattern
                .or(bayer_pattern)
                .unwrap_or(BayerPattern::RGGB);
            if let LayoutChoice::Bayer(override_pat) = layout {
                pattern = override_pat;
            }

            if let Some([top, left, bottom, right]) = ifd.active_area {
                if bottom > top
                    && right > left
                    && bottom <= height
                    && right <= width
                    && (bottom - top) >= 16
                    && (right - left) >= 16
                {
                    let cw = right - left;
                    let ch = bottom - top;
                    let mut cropped = Vec::with_capacity(cw * ch);
                    for y in top..bottom {
                        let row_start = y * width + left;
                        cropped.extend_from_slice(&cfa_u16[row_start..row_start + cw]);
                    }
                    if left % 2 == 1 && layout == LayoutChoice::Auto {
                        pattern = pattern.cols_swapped();
                    }
                    if top % 2 == 1 && layout == LayoutChoice::Auto {
                        pattern = pattern.rows_swapped();
                    }
                    cfa_u16 = cropped;
                    width = cw;
                    height = ch;
                }
            }

            // Subtract black level and scale to physical 16-bit [0..65535] ADU
            let max_raw = cfa_u16.iter().copied().max().unwrap_or(4095);
            let default_white = if ifd.bits_per_sample > 0 && ifd.bits_per_sample <= 16 {
                ((1_u32 << ifd.bits_per_sample) - 1) as f32
            } else {
                f32::from(max_raw).max(1023.0)
            };
            let white = ifd
                .white_level
                .unwrap_or(default_white)
                .max(f32::from(max_raw))
                .max(255.0);

            let mut pixels = vec![0.0_f32; width * height];
            for y in 0..height {
                let row_off = y * width;
                for x in 0..width {
                    let cfa_idx = (y & 1) * 2 + (x & 1);
                    let black = ifd.black_level[cfa_idx];
                    let span = (white - black).max(1.0);
                    let raw_val = f32::from(cfa_u16[row_off + x]);
                    let norm = ((raw_val - black) / span).clamp(0.0, 1.0);
                    pixels[row_off + x] = norm * 65535.0;
                }
            }

            let mosaic_pat = if layout == LayoutChoice::Mono {
                None
            } else {
                Some(pattern)
            };
            let cards = build_raw_cards(path, meta, mosaic_pat, "Stage 2 (Linear Sensor CFA)");
            let mut info = build_frame_info(width, height, 16, meta, mosaic_pat, false);
            info.apply_layout(layout);
            let header = FitsHeader::from_synthetic(info, cards);
            return Ok(RawStage2Output::Cfa(RawFrame { header, pixels }));
        }
    }

    // Fallback: Linearize the highest-resolution embedded image via inverse sRGB transfer curve
    // into 16-bit linear ADU [0..65535] so MTF auto-stretch and linear histogram work uniformly.
    if let Some((jpeg_off, jpeg_len)) = find_best_embedded_jpeg(bytes, meta) {
        if let Some(slice) = bytes.get(jpeg_off..jpeg_off.saturating_add(jpeg_len)) {
            if let Ok(reader) = ImageReader::new(Cursor::new(slice)).with_guessed_format() {
                if let Ok(dyn_img) = reader.decode() {
                    check_cancelled(cancel)?;
                    let rgb8 = dyn_img.to_rgb8();
                    let width = rgb8.width() as usize;
                    let height = rgb8.height() as usize;
                    let raw_bytes = rgb8.into_raw();
                    let mut pixels = Vec::with_capacity(width * height);
                    for chunk in raw_bytes.chunks_exact(3) {
                        pixels.push([
                            srgb_u8_to_linear_adu16(chunk[0]),
                            srgb_u8_to_linear_adu16(chunk[1]),
                            srgb_u8_to_linear_adu16(chunk[2]),
                        ]);
                    }
                    let cards = build_raw_cards(
                        path,
                        meta,
                        bayer_pattern,
                        "Stage 2 (Embedded JPEG only, sensor data not decoded)",
                    );
                    let info = build_frame_info(width, height, 16, meta, bayer_pattern, true);
                    return Ok(RawStage2Output::LinearRgb(StandardFrame {
                        rgb: RgbImage {
                            width,
                            height,
                            demosaic_method: DemosaicMethod::External,
                            pixels,
                        },
                        info,
                        cards,
                        default_stretch: true,
                    }));
                }
            }
        }
    }

    Err(Error::Invalid("Unable to decode camera RAW file."))
}

#[inline]
fn srgb_u8_to_linear_adu16(v: u8) -> f32 {
    let s = f32::from(v) / 255.0;
    let lin = if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    };
    (lin * 65535.0).clamp(0.0, 65535.0)
}

fn resolve_raw_bayer_pattern(
    path: &Path,
    meta: &RawContainerMeta,
    layout: LayoutChoice,
) -> Option<BayerPattern> {
    match layout {
        LayoutChoice::Mono => return None,
        LayoutChoice::Bayer(p) => return Some(p),
        LayoutChoice::Auto => {}
    }
    for ifd in &meta.ifds {
        if let Some(p) = ifd.cfa_pattern {
            return Some(p);
        }
    }
    if let Some(model) = &meta.model {
        if let Some(cam) = cameras::identify(model) {
            return cam.bayer_pattern();
        }
    }
    if let Some(make) = &meta.make {
        if let Some(cam) = cameras::identify(make) {
            return cam.bayer_pattern();
        }
    }
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if let Some(cam) = cameras::identify(name) {
            return cam.bayer_pattern();
        }
    }
    Some(BayerPattern::RGGB)
}

fn build_frame_info(
    width: usize,
    height: usize,
    bitpix: i32,
    meta: &RawContainerMeta,
    bayer_pattern: Option<BayerPattern>,
    is_rgb: bool,
) -> FrameInfo {
    let instrument = match (&meta.make, &meta.model) {
        (Some(make), Some(model)) => {
            if model
                .to_ascii_lowercase()
                .starts_with(&make.to_ascii_lowercase())
            {
                Some(model.clone())
            } else {
                Some(format!("{make} {model}"))
            }
        }
        (None, Some(model)) => Some(model.clone()),
        (Some(make), None) => Some(make.clone()),
        (None, None) => None,
    };
    FrameInfo {
        width,
        height,
        planes: if is_rgb { 3 } else { 1 },
        bitpix,
        bzero: 0.0,
        bscale: 1.0,
        bayer_pattern: if is_rgb { None } else { bayer_pattern },
        mosaic: if is_rgb {
            Mosaic::Rgb
        } else if bayer_pattern.is_some() {
            Mosaic::Bayer
        } else {
            Mosaic::Mono
        },
        mosaic_source: "raw-header",
        exposure_seconds: meta.exposure_s.unwrap_or(0.0),
        object: None,
        filter: None,
        date_obs: meta.date_obs.clone(),
        gain: meta.iso,
        ccd_temperature: None,
        instrument,
        telescope: meta.lens.clone(),
        bias_informational: None,
        creator: meta.software.clone(),
        saturation_adu: Some(65535.0),
        derived: Vec::new(),
        rows_mirrored: false,
    }
}

fn build_raw_cards(
    path: &Path,
    meta: &RawContainerMeta,
    bayer: Option<BayerPattern>,
    stage_desc: &str,
) -> Vec<HeaderCard> {
    let mut cards = Vec::with_capacity(16);
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        cards.push(HeaderCard {
            keyword: "RAWFMT".to_owned(),
            value: ext.to_ascii_uppercase(),
            comment: "Camera RAW container format".to_owned(),
        });
    }
    cards.push(HeaderCard {
        keyword: "RAWSTAGE".to_owned(),
        value: stage_desc.to_owned(),
        comment: "Two-stage RAW decoding pipeline state".to_owned(),
    });
    if let Some(make) = &meta.make {
        cards.push(HeaderCard {
            keyword: "MAKE".to_owned(),
            value: make.clone(),
            comment: "Camera manufacturer".to_owned(),
        });
    }
    if let Some(model) = &meta.model {
        cards.push(HeaderCard {
            keyword: "INSTRUME".to_owned(),
            value: model.clone(),
            comment: "Camera model".to_owned(),
        });
    }
    if let Some(pat) = bayer {
        cards.push(HeaderCard {
            keyword: "BAYERPAT".to_owned(),
            value: pat.name().to_owned(),
            comment: "Sensor CFA Bayer mosaic pattern".to_owned(),
        });
    }
    if let Some(exp) = meta.exposure_s {
        cards.push(HeaderCard {
            keyword: "EXPTIME".to_owned(),
            value: format!("{exp:.5}"),
            comment: "Exposure time in seconds".to_owned(),
        });
    }
    if let Some(iso) = meta.iso {
        cards.push(HeaderCard {
            keyword: "ISOSPEED".to_owned(),
            value: format!("{iso:.0}"),
            comment: "ISO sensitivity".to_owned(),
        });
    }
    if let Some(fnm) = meta.fnumber {
        cards.push(HeaderCard {
            keyword: "FNUMBER".to_owned(),
            value: format!("f/{fnm:.1}"),
            comment: "Lens aperture f-number".to_owned(),
        });
    }
    if let Some(fl) = meta.focal_mm {
        cards.push(HeaderCard {
            keyword: "FOCALLEN".to_owned(),
            value: format!("{fl:.1} mm"),
            comment: "Focal length in mm".to_owned(),
        });
    }
    if let Some(lens) = &meta.lens {
        cards.push(HeaderCard {
            keyword: "TELESCOP".to_owned(),
            value: lens.clone(),
            comment: "Lens model".to_owned(),
        });
    }
    if let Some(sw) = &meta.software {
        cards.push(HeaderCard {
            keyword: "SWCREATE".to_owned(),
            value: sw.clone(),
            comment: "Firmware / creation software".to_owned(),
        });
    }
    if let Some(dt) = &meta.date_obs {
        cards.push(HeaderCard {
            keyword: "DATE-OBS".to_owned(),
            value: dt.clone(),
            comment: "Capture timestamp".to_owned(),
        });
    }
    cards
}

fn find_best_embedded_jpeg(bytes: &[u8], meta: &RawContainerMeta) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    let mut consider = |off: usize, len: usize| {
        if len >= 256
            && off.saturating_add(len) <= bytes.len()
            && bytes.get(off..off + 2) == Some(&[0xFF, 0xD8])
            && best.is_none_or(|(_, best_len)| len > best_len)
        {
            best = Some((off, len));
        }
    };

    for ifd in &meta.ifds {
        if ifd.jpeg_offset > 0 && ifd.jpeg_length > 0 {
            consider(ifd.jpeg_offset as usize, ifd.jpeg_length as usize);
        }
        if (ifd.compression == 6 || ifd.compression == 7)
            && ifd.strip_offsets.len() == 1
            && ifd.strip_byte_counts.len() == 1
        {
            consider(
                ifd.strip_offsets[0] as usize,
                ifd.strip_byte_counts[0] as usize,
            );
        }
    }
    for &(off, len) in &meta.extra_jpegs {
        consider(off, len);
    }
    best
}

fn parse_raw_container(bytes: &[u8]) -> RawContainerMeta {
    let mut meta = RawContainerMeta::default();
    if bytes.len() < 16 {
        return meta;
    }

    // 1. Fujifilm RAF container ("FUJIFILMCCD-RAW ")
    if bytes.starts_with(b"FUJIFILMCCD-RAW ") && bytes.len() >= 108 {
        let jpeg_off = Endian::Big.u32(bytes, 84).unwrap_or(0) as usize;
        let jpeg_len = Endian::Big.u32(bytes, 88).unwrap_or(0) as usize;
        if jpeg_off > 0 && jpeg_len > 0 && jpeg_off.saturating_add(jpeg_len) <= bytes.len() {
            meta.extra_jpegs.push((jpeg_off, jpeg_len));
            parse_jpeg_app1_exif(&bytes[jpeg_off..jpeg_off + jpeg_len], &mut meta);
        }
        return meta;
    }

    // 2. Canon CR3 ISO-BMFF container ("....ftypcrx ")
    if bytes.get(4..12) == Some(b"ftypcrx ") {
        scan_embedded_jpegs(bytes, &mut meta);
        return meta;
    }

    // 3. Standard TIFF / DNG / CR2 / NEF / ARW / ORF / PEF / RW2 container
    let endian = match &bytes[0..4] {
        [b'I', b'I', 0x2A, 0x00]
        | [b'I', b'I', 0x55, 0x00] // Panasonic RW2
        | [b'I', b'I', b'R', b'O'] // Olympus ORF
        | [b'I', b'I', b'R', b'S'] => Some(Endian::Little),
        [b'M', b'M', 0x00, 0x2A] | [b'M', b'M', b'O', b'R'] => Some(Endian::Big),
        _ => None,
    };

    if let Some(e) = endian {
        meta.endian = Some(e);
        if let Some(first_ifd) = e.u32(bytes, 4) {
            let mut visited = Vec::new();
            parse_tiff_ifd_chain(bytes, 0, first_ifd as usize, e, &mut meta, &mut visited, 0);
        }
    }
    meta
}

fn parse_jpeg_app1_exif(jpeg: &[u8], meta: &mut RawContainerMeta) {
    let mut pos = 2;
    while pos + 4 <= jpeg.len() {
        if jpeg[pos] != 0xFF {
            break;
        }
        let marker = jpeg[pos + 1];
        if marker == 0xDA || marker == 0xD9 {
            break;
        }
        let len = u16::from_be_bytes([jpeg[pos + 2], jpeg[pos + 3]]) as usize;
        if len < 2 || pos + 2 + len > jpeg.len() {
            break;
        }
        if marker == 0xE1 && len >= 14 && &jpeg[pos + 4..pos + 10] == b"Exif\0\0" {
            let tiff_base = pos + 10;
            let endian = match &jpeg[tiff_base..tiff_base + 2] {
                b"II" => Some(Endian::Little),
                b"MM" => Some(Endian::Big),
                _ => None,
            };
            if let Some(e) = endian {
                if let Some(first) = e.u32(jpeg, tiff_base + 4) {
                    let mut visited = Vec::new();
                    parse_tiff_ifd_chain(jpeg, tiff_base, first as usize, e, meta, &mut visited, 0);
                }
            }
            break;
        }
        pos += 2 + len;
    }
}

fn scan_embedded_jpegs(bytes: &[u8], meta: &mut RawContainerMeta) {
    // Fast scan for ISO-BMFF PRVW boxes or JPEG SOI..EOI streams
    let mut i = 0;
    while i + 16 < bytes.len() {
        if bytes[i] == 0xFF && bytes[i + 1] == 0xD8 && bytes[i + 2] == 0xFF {
            // Find matching EOI (0xFF 0xD9)
            let mut end = i + 4;
            while end + 1 < bytes.len() {
                if bytes[end] == 0xFF && bytes[end + 1] == 0xD9 {
                    let len = end + 2 - i;
                    if len >= 4096 {
                        meta.extra_jpegs.push((i, len));
                        if meta.make.is_none() {
                            parse_jpeg_app1_exif(&bytes[i..i + len], meta);
                        }
                        i = end + 2;
                        break;
                    }
                }
                end += 1;
            }
        }
        i += 1;
    }
}

fn parse_tiff_ifd_chain(
    bytes: &[u8],
    tiff_base: usize,
    mut ifd_rel_offset: usize,
    endian: Endian,
    meta: &mut RawContainerMeta,
    visited: &mut Vec<usize>,
    depth: usize,
) {
    if depth > 6 {
        return;
    }
    while ifd_rel_offset > 0 && visited.len() < 48 {
        let abs_ifd = tiff_base.saturating_add(ifd_rel_offset);
        if visited.contains(&abs_ifd) || abs_ifd + 2 > bytes.len() {
            break;
        }
        visited.push(abs_ifd);

        let Some(entry_count) = endian.u16(bytes, abs_ifd) else {
            break;
        };
        let count = usize::from(entry_count);
        if count > 512 || abs_ifd + 2 + count * 12 > bytes.len() {
            break;
        }

        let mut ifd = IfdInfo {
            samples_per_pixel: 1,
            ..IfdInfo::default()
        };
        let mut sub_ifds: Vec<usize> = Vec::new();
        let mut exif_ifd: Option<usize> = None;

        for idx in 0..count {
            let e_off = abs_ifd + 2 + idx * 12;
            let Some(tag) = endian.u16(bytes, e_off) else {
                continue;
            };
            let Some(field_type) = endian.u16(bytes, e_off + 2) else {
                continue;
            };
            let Some(num_values) = endian.u32(bytes, e_off + 4) else {
                continue;
            };
            let n = num_values as usize;
            let elem_size = match field_type {
                1 | 2 | 6 | 7 => 1usize,
                3 | 8 => 2usize,
                4 | 9 | 11 | 13 => 4usize,
                5 | 10 | 12 => 8usize,
                _ => continue,
            };
            let byte_len = n.saturating_mul(elem_size);
            let val_pos = if byte_len <= 4 {
                e_off + 8
            } else {
                let Some(rel) = endian.u32(bytes, e_off + 8) else {
                    continue;
                };
                tiff_base.saturating_add(rel as usize)
            };
            if val_pos.saturating_add(byte_len) > bytes.len() {
                continue;
            }

            match tag {
                0x00FE => {
                    ifd.subfile_type = read_u32_at(bytes, val_pos, field_type, endian).unwrap_or(0);
                }
                0x0100 => {
                    ifd.width =
                        read_u32_at(bytes, val_pos, field_type, endian).unwrap_or(0) as usize;
                }
                0x0101 => {
                    ifd.height =
                        read_u32_at(bytes, val_pos, field_type, endian).unwrap_or(0) as usize;
                }
                0x0102 => {
                    ifd.bits_per_sample =
                        read_u32_at(bytes, val_pos, field_type, endian).unwrap_or(0) as u16;
                }
                0x0103 => {
                    ifd.compression =
                        read_u32_at(bytes, val_pos, field_type, endian).unwrap_or(0) as u16;
                }
                0x0106 => {
                    ifd.photometric =
                        read_u32_at(bytes, val_pos, field_type, endian).unwrap_or(0) as u16;
                }
                0x010F => {
                    if meta.make.is_none() {
                        meta.make = read_ascii(bytes, val_pos, byte_len);
                    }
                }
                0x0110 => {
                    if meta.model.is_none() {
                        meta.model = read_ascii(bytes, val_pos, byte_len);
                    }
                }
                0x0111 => {
                    ifd.strip_offsets = read_u32_vec(bytes, val_pos, n, field_type, endian);
                }
                0x0115 => {
                    ifd.samples_per_pixel =
                        read_u32_at(bytes, val_pos, field_type, endian).unwrap_or(1) as u16;
                }
                0x0117 => {
                    ifd.strip_byte_counts = read_u32_vec(bytes, val_pos, n, field_type, endian);
                }
                0x0131 => {
                    if meta.software.is_none() {
                        meta.software = read_ascii(bytes, val_pos, byte_len);
                    }
                }
                0x0132 | 0x9003 => {
                    if meta.date_obs.is_none() || tag == 0x9003 {
                        meta.date_obs = read_ascii(bytes, val_pos, byte_len);
                    }
                }
                0x0142 => {
                    ifd.tile_width =
                        read_u32_at(bytes, val_pos, field_type, endian).unwrap_or(0) as usize;
                }
                0x0143 => {
                    ifd.tile_height =
                        read_u32_at(bytes, val_pos, field_type, endian).unwrap_or(0) as usize;
                }
                0x0144 => {
                    ifd.tile_offsets = read_u32_vec(bytes, val_pos, n, field_type, endian);
                }
                0x0145 => {
                    ifd.tile_byte_counts = read_u32_vec(bytes, val_pos, n, field_type, endian);
                }
                0x014A => {
                    for sub in read_u32_vec(bytes, val_pos, n, field_type, endian) {
                        sub_ifds.push(sub as usize);
                    }
                }
                0x0201 => {
                    ifd.jpeg_offset = read_u32_at(bytes, val_pos, field_type, endian).unwrap_or(0);
                }
                0x0202 => {
                    ifd.jpeg_length = read_u32_at(bytes, val_pos, field_type, endian).unwrap_or(0);
                }
                0x7031 => {
                    // Sony ARW raw data offset
                    ifd.sony_raw_offset =
                        read_u32_at(bytes, val_pos, field_type, endian).unwrap_or(0);
                }
                0x7032 => {
                    ifd.sony_raw_length =
                        read_u32_at(bytes, val_pos, field_type, endian).unwrap_or(0);
                }
                0x828E | 0xC617 => {
                    if byte_len >= 4 {
                        ifd.cfa_pattern = parse_cfa_4bytes(&bytes[val_pos..val_pos + 4]);
                    }
                }
                0x829A => {
                    meta.exposure_s = read_rational(bytes, val_pos, field_type, endian);
                }
                0x829D => {
                    meta.fnumber = read_rational(bytes, val_pos, field_type, endian);
                }
                0x8769 => {
                    if let Some(off) = read_u32_at(bytes, val_pos, field_type, endian) {
                        exif_ifd = Some(off as usize);
                    }
                }
                0x8827 => {
                    meta.iso = read_u32_at(bytes, val_pos, field_type, endian).map(f64::from);
                }
                0x920A => {
                    meta.focal_mm = read_rational(bytes, val_pos, field_type, endian);
                }
                0xA434 => {
                    if meta.lens.is_none() {
                        meta.lens = read_ascii(bytes, val_pos, byte_len);
                    }
                }
                0xC618 => {
                    // DNG LinearizationTable
                    let vals = read_u32_vec(bytes, val_pos, n.min(65536), field_type, endian);
                    ifd.linearization_table = vals.into_iter().map(|v| v as u16).collect();
                }
                0xC61A => {
                    // DNG BlackLevel
                    let vals = read_f32_vec(bytes, val_pos, n.min(4), field_type, endian);
                    if !vals.is_empty() {
                        for k in 0..4 {
                            ifd.black_level[k] = vals[k % vals.len()];
                        }
                    }
                }
                0xC61D => {
                    // DNG WhiteLevel
                    let vals = read_f32_vec(bytes, val_pos, n.min(4), field_type, endian);
                    if let Some(&first) = vals.first() {
                        ifd.white_level = Some(first);
                    }
                }
                0xC640 => {
                    // Canon CR2 Slice Info: [num_slices_minus_1, slice_width, last_slice_width]
                    let vals = read_u32_vec(bytes, val_pos, n.min(3), field_type, endian);
                    if vals.len() == 3 {
                        ifd.cr2_slices = Some([vals[0] as u16, vals[1] as u16, vals[2] as u16]);
                    }
                }
                0xC68D => {
                    // DNG ActiveArea: [top, left, bottom, right]
                    let vals = read_u32_vec(bytes, val_pos, n.min(4), field_type, endian);
                    if vals.len() == 4 {
                        ifd.active_area = Some([
                            vals[0] as usize,
                            vals[1] as usize,
                            vals[2] as usize,
                            vals[3] as usize,
                        ]);
                    }
                }
                _ => {}
            }
        }

        if ifd.width > 0 || ifd.jpeg_offset > 0 || !ifd.strip_offsets.is_empty() {
            meta.ifds.push(ifd);
        }

        if let Some(exif_off) = exif_ifd {
            parse_tiff_ifd_chain(bytes, tiff_base, exif_off, endian, meta, visited, depth + 1);
        }
        for sub_off in sub_ifds {
            parse_tiff_ifd_chain(bytes, tiff_base, sub_off, endian, meta, visited, depth + 1);
        }

        let next_pos = abs_ifd + 2 + count * 12;
        ifd_rel_offset = endian.u32(bytes, next_pos).unwrap_or(0) as usize;
    }
}

fn parse_cfa_4bytes(b: &[u8]) -> Option<BayerPattern> {
    // TIFF/EP & DNG CFAPattern: 0 = Red, 1 = Green, 2 = Blue
    match [b[0], b[1], b[2], b[3]] {
        [0, 1, 1, 2] => Some(BayerPattern::RGGB),
        [2, 1, 1, 0] => Some(BayerPattern::BGGR),
        [1, 0, 2, 1] => Some(BayerPattern::GRBG),
        [1, 2, 0, 1] => Some(BayerPattern::GBRG),
        _ => None,
    }
}

fn read_ascii(bytes: &[u8], pos: usize, len: usize) -> Option<String> {
    let slice = bytes.get(pos..pos + len)?;
    let trimmed = slice.split(|&b| b == 0).next().unwrap_or(slice);
    let s = String::from_utf8_lossy(trimmed).trim().to_owned();
    if s.is_empty() { None } else { Some(s) }
}

fn read_u32_at(bytes: &[u8], pos: usize, field_type: u16, endian: Endian) -> Option<u32> {
    match field_type {
        1 | 6 | 7 => bytes.get(pos).copied().map(u32::from),
        3 | 8 => endian.u16(bytes, pos).map(u32::from),
        4 | 9 | 13 => endian.u32(bytes, pos),
        _ => None,
    }
}

fn read_u32_vec(
    bytes: &[u8],
    pos: usize,
    count: usize,
    field_type: u16,
    endian: Endian,
) -> Vec<u32> {
    let step = match field_type {
        1 | 6 | 7 => 1usize,
        3 | 8 => 2usize,
        4 | 9 | 13 => 4usize,
        _ => return Vec::new(),
    };
    let mut out = Vec::with_capacity(count.min(65536));
    for i in 0..count.min(65536) {
        if let Some(v) = read_u32_at(bytes, pos + i * step, field_type, endian) {
            out.push(v);
        }
    }
    out
}

fn read_f32_vec(
    bytes: &[u8],
    pos: usize,
    count: usize,
    field_type: u16,
    endian: Endian,
) -> Vec<f32> {
    let mut out = Vec::new();
    for i in 0..count {
        match field_type {
            1 | 3 | 4 => {
                if let Some(v) = read_u32_at(
                    bytes,
                    pos + i * if field_type == 3 { 2 } else { 4 },
                    field_type,
                    endian,
                ) {
                    out.push(v as f32);
                }
            }
            5 | 10 => {
                if let Some(r) = read_rational(bytes, pos + i * 8, field_type, endian) {
                    out.push(r as f32);
                }
            }
            _ => {}
        }
    }
    out
}

fn read_rational(bytes: &[u8], pos: usize, field_type: u16, endian: Endian) -> Option<f64> {
    let num = endian.u32(bytes, pos)?;
    let den = endian.u32(bytes, pos + 4)?;
    if field_type == 10 {
        let n = num as i32;
        let d = den as i32;
        if d == 0 {
            None
        } else {
            Some(f64::from(n) / f64::from(d))
        }
    } else if den == 0 {
        None
    } else {
        Some(f64::from(num) / f64::from(den))
    }
}

/// Decodes a single-plane CFA IFD into `width * height` `u16` samples.
fn decode_cfa_ifd_pixels(
    bytes: &[u8],
    endian: Endian,
    ifd: &IfdInfo,
    cancel: &AtomicBool,
) -> Option<Vec<u16>> {
    let width = ifd.width;
    let height = ifd.height;
    let total = width.checked_mul(height)?;
    if total == 0 || total > crate::MAX_DECODE_PIXELS {
        return None;
    }

    // Case 1: Tiled IFD (common in DNG)
    if !ifd.tile_offsets.is_empty()
        && ifd.tile_offsets.len() == ifd.tile_byte_counts.len()
        && ifd.tile_width > 0
        && ifd.tile_height > 0
    {
        // Tile geometry comes straight from the file: bound it before any allocation.
        let tile_pixels_expected = ifd
            .tile_width
            .checked_mul(ifd.tile_height)
            .filter(|&n| n <= crate::MAX_DECODE_PIXELS)?;
        let mut out = vec![0_u16; total];
        let tiles_across = width.div_ceil(ifd.tile_width);
        for (t_idx, (&off, &len)) in ifd
            .tile_offsets
            .iter()
            .zip(ifd.tile_byte_counts.iter())
            .enumerate()
        {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return None;
            }
            let slice = bytes.get(off as usize..(off as usize).saturating_add(len as usize))?;
            let tile_pixels = decode_chunk_pixels(
                slice,
                endian,
                ifd.compression,
                ifd.bits_per_sample,
                tile_pixels_expected,
            )?;
            let tx = (t_idx % tiles_across) * ifd.tile_width;
            let ty = (t_idx / tiles_across) * ifd.tile_height;
            for r in 0..ifd.tile_height {
                if ty + r >= height {
                    break;
                }
                let copy_w = ifd.tile_width.min(width.saturating_sub(tx));
                let src_off = r * ifd.tile_width;
                let dst_off = (ty + r) * width + tx;
                if src_off + copy_w <= tile_pixels.len() && dst_off + copy_w <= out.len() {
                    out[dst_off..dst_off + copy_w]
                        .copy_from_slice(&tile_pixels[src_off..src_off + copy_w]);
                }
            }
        }
        return Some(out);
    }

    // Case 2: Striped IFD
    if !ifd.strip_offsets.is_empty() && ifd.strip_offsets.len() == ifd.strip_byte_counts.len() {
        // Single Lossless JPEG stream (e.g. Canon CR2 or single-strip DNG)
        if ifd.strip_offsets.len() == 1 && (ifd.compression == 7 || ifd.compression == 6) {
            let off = ifd.strip_offsets[0] as usize;
            let len = ifd.strip_byte_counts[0] as usize;
            let slice = bytes.get(off..off.saturating_add(len))?;
            let decoded = decode_lossless_jpeg(slice, total)?;
            if let Some(slices) = ifd.cr2_slices {
                return Some(unslice_cr2(&decoded, width, height, slices));
            }
            return Some(decoded);
        }

        let mut out = Vec::with_capacity(total);
        let strips_count = ifd.strip_offsets.len();
        let pixels_per_strip = total.div_ceil(strips_count);
        for (&off, &len) in ifd.strip_offsets.iter().zip(ifd.strip_byte_counts.iter()) {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return None;
            }
            let remaining = total.saturating_sub(out.len());
            if remaining == 0 {
                break;
            }
            let expected = pixels_per_strip.min(remaining);
            let slice = bytes.get(off as usize..(off as usize).saturating_add(len as usize))?;
            let chunk = decode_chunk_pixels(
                slice,
                endian,
                ifd.compression,
                ifd.bits_per_sample,
                expected,
            )?;
            out.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        }
        if out.len() == total {
            return Some(out);
        }
    }

    None
}

fn decode_chunk_pixels(
    slice: &[u8],
    endian: Endian,
    compression: u16,
    bits_per_sample: u16,
    expected_pixels: usize,
) -> Option<Vec<u16>> {
    match compression {
        // Uncompressed (1) or Sony/Nikon raw unpacked/packed
        1 => unpack_integer_samples(slice, endian, bits_per_sample, expected_pixels),
        // Lossless JPEG (7 = New JPEG / Lossless JPEG, 6 = Old JPEG / CR2 Lossless)
        6 | 7 => decode_lossless_jpeg(slice, expected_pixels),
        // Deflate / Zlib (8 or 32946 in DNG)
        8 | 32946 => {
            let decompressed = miniz_oxide::inflate::decompress_to_vec_zlib(slice).ok()?;
            unpack_integer_samples(&decompressed, endian, bits_per_sample, expected_pixels)
        }
        // Sony ARW2 12-bit delta compression (32767)
        32767 => decode_sony_arw2_delta(slice, expected_pixels),
        _ => None,
    }
}

fn unpack_integer_samples(
    slice: &[u8],
    endian: Endian,
    bits_per_sample: u16,
    expected_pixels: usize,
) -> Option<Vec<u16>> {
    let mut out = Vec::with_capacity(expected_pixels);
    match bits_per_sample {
        8 => {
            if slice.len() < expected_pixels {
                return None;
            }
            out.extend(slice[..expected_pixels].iter().map(|&b| u16::from(b)));
            Some(out)
        }
        10 => {
            // 5 bytes -> 4 pixels (big-endian bit packing)
            for chunk in slice.chunks_exact(5) {
                if out.len() >= expected_pixels {
                    break;
                }
                let b0 = u32::from(chunk[0]);
                let b1 = u32::from(chunk[1]);
                let b2 = u32::from(chunk[2]);
                let b3 = u32::from(chunk[3]);
                let b4 = u32::from(chunk[4]);
                out.push(((b0 << 2) | (b1 >> 6)) as u16);
                out.push((((b1 & 0x3F) << 4) | (b2 >> 4)) as u16);
                out.push((((b2 & 0x0F) << 6) | (b3 >> 2)) as u16);
                out.push((((b3 & 0x03) << 8) | b4) as u16);
            }
            out.truncate(expected_pixels);
            (out.len() == expected_pixels).then_some(out)
        }
        12 => {
            // Either unpacked 16-bit words (2 bytes/px) or packed 12-bit (3 bytes -> 2 pixels)
            if slice.len() >= expected_pixels * 2 {
                for i in 0..expected_pixels {
                    out.push(endian.u16(slice, i * 2)? & 0x0FFF);
                }
                return Some(out);
            }
            for chunk in slice.chunks_exact(3) {
                if out.len() >= expected_pixels {
                    break;
                }
                let b0 = u16::from(chunk[0]);
                let b1 = u16::from(chunk[1]);
                let b2 = u16::from(chunk[2]);
                out.push((b0 << 4) | (b1 >> 4));
                if out.len() < expected_pixels {
                    out.push(((b1 & 0x0F) << 8) | b2);
                }
            }
            (out.len() == expected_pixels).then_some(out)
        }
        14 => {
            if slice.len() >= expected_pixels * 2 {
                for i in 0..expected_pixels {
                    out.push(endian.u16(slice, i * 2)? & 0x3FFF);
                }
                return Some(out);
            }
            // 7 bytes -> 4 pixels
            for chunk in slice.chunks_exact(7) {
                if out.len() >= expected_pixels {
                    break;
                }
                let mut acc = 0_u64;
                for &b in chunk {
                    acc = (acc << 8) | u64::from(b);
                }
                out.push(((acc >> 42) & 0x3FFF) as u16);
                out.push(((acc >> 28) & 0x3FFF) as u16);
                out.push(((acc >> 14) & 0x3FFF) as u16);
                out.push((acc & 0x3FFF) as u16);
            }
            out.truncate(expected_pixels);
            (out.len() == expected_pixels).then_some(out)
        }
        16 => {
            if slice.len() < expected_pixels * 2 {
                return None;
            }
            for i in 0..expected_pixels {
                out.push(endian.u16(slice, i * 2)?);
            }
            Some(out)
        }
        _ => None,
    }
}

fn decode_sony_arw2_delta(slice: &[u8], expected_pixels: usize) -> Option<Vec<u16>> {
    // Sony ARW2 compresses 16 horizontal pixels into each 16-byte block (128 bits)
    if slice.len() < expected_pixels {
        return None;
    }
    let mut out = Vec::with_capacity(expected_pixels);
    for block in slice.chunks_exact(16) {
        if out.len() >= expected_pixels {
            break;
        }
        let mut bits = 0_u128;
        for (i, &b) in block.iter().enumerate() {
            bits |= u128::from(b) << (i * 8);
        }
        let max_val = (bits & 0x7FF) as u16;
        let min_val = ((bits >> 11) & 0x7FF) as u16;
        let imax = ((bits >> 22) & 0x0F) as usize;
        let imin = ((bits >> 26) & 0x0F) as usize;
        let mut shift = 0_u32;
        while shift < 4 && ((max_val.saturating_sub(min_val)) >> shift) >= 128 {
            shift += 1;
        }
        let mut bit_pos = 30_u32;
        for i in 0..16 {
            if out.len() >= expected_pixels {
                break;
            }
            let val = if i == imax {
                max_val
            } else if i == imin {
                min_val
            } else {
                let delta = ((bits >> bit_pos) & 0x7F) as u16;
                bit_pos += 7;
                min_val.saturating_add(delta << shift)
            };
            out.push(val << 1);
        }
    }
    (out.len() == expected_pixels).then_some(out)
}

fn unslice_cr2(decoded: &[u16], width: usize, height: usize, slices: [u16; 3]) -> Vec<u16> {
    let count_first = usize::from(slices[0]);
    let slice_w = usize::from(slices[1]);
    let last_w = usize::from(slices[2]);
    if slice_w == 0 || count_first * slice_w + last_w != width || decoded.len() < width * height {
        return decoded.to_vec();
    }
    let mut out = vec![0_u16; width * height];
    let mut src_idx = 0;
    let total_slices = count_first + 1;
    for s in 0..total_slices {
        let sw = if s < count_first { slice_w } else { last_w };
        let x0 = s * slice_w;
        for y in 0..height {
            let dst_row = y * width + x0;
            if src_idx + sw <= decoded.len() && dst_row + sw <= out.len() {
                out[dst_row..dst_row + sw].copy_from_slice(&decoded[src_idx..src_idx + sw]);
            }
            src_idx += sw;
        }
    }
    out
}

#[derive(Clone, Default)]
struct HuffTable {
    min_code: [i32; 17],
    max_code: [i32; 17],
    val_ptr: [usize; 17],
    huffval: Vec<u8>,
}

impl HuffTable {
    fn from_counts_and_vals(counts: &[u8; 16], vals: &[u8]) -> Self {
        let mut min_code = [0_i32; 17];
        let mut max_code = [-1_i32; 17];
        let mut val_ptr = [0_usize; 17];
        let mut code = 0_i32;
        let mut k = 0_usize;
        for bits in 1..=16 {
            let cnt = usize::from(counts[bits - 1]);
            if cnt > 0 {
                val_ptr[bits] = k;
                min_code[bits] = code;
                code += cnt as i32 - 1;
                max_code[bits] = code;
                k += cnt;
                code += 1;
            }
            code <<= 1;
        }
        Self {
            min_code,
            max_code,
            val_ptr,
            huffval: vals.to_vec(),
        }
    }
}

struct JpegBitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit_buf: u32,
    bits_left: u8,
}

impl<'a> JpegBitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            bit_buf: 0,
            bits_left: 0,
        }
    }

    #[inline]
    fn fill(&mut self) {
        while self.bits_left <= 24 && self.pos < self.data.len() {
            let b = self.data[self.pos];
            self.pos += 1;
            if b == 0xFF && self.pos < self.data.len() {
                let next = self.data[self.pos];
                if next == 0x00 {
                    self.pos += 1;
                } else if (0xD0..=0xD7).contains(&next) {
                    self.pos += 1;
                    continue;
                } else {
                    // Marker reached; pad with zeros
                    self.pos -= 1;
                    break;
                }
            }
            self.bit_buf = (self.bit_buf << 8) | u32::from(b);
            self.bits_left += 8;
        }
    }

    #[inline]
    fn get_bits(&mut self, n: u8) -> u32 {
        if n == 0 {
            return 0;
        }
        if self.bits_left < n {
            self.fill();
        }
        if self.bits_left < n {
            let shift = n - self.bits_left;
            let res = (self.bit_buf << shift) & ((1_u32 << n) - 1);
            self.bits_left = 0;
            self.bit_buf = 0;
            return res;
        }
        self.bits_left -= n;
        (self.bit_buf >> self.bits_left) & ((1_u32 << n) - 1)
    }

    #[inline]
    fn decode_huff(&mut self, table: &HuffTable) -> Option<u8> {
        let mut code = 0_i32;
        for bits in 1..=16 {
            code = (code << 1) | (self.get_bits(1) as i32);
            if code <= table.max_code[bits] {
                let idx = table.val_ptr[bits] + (code - table.min_code[bits]) as usize;
                return table.huffval.get(idx).copied();
            }
        }
        None
    }

    #[inline]
    fn decode_diff(&mut self, table: &HuffTable) -> Option<i32> {
        let ssss = self.decode_huff(table)?;
        if ssss == 0 {
            return Some(0);
        }
        if ssss == 16 {
            return Some(32768);
        }
        let bits = self.get_bits(ssss) as i32;
        let half = 1_i32 << (ssss - 1);
        if bits < half {
            Some(bits - ((1_i32 << ssss) - 1))
        } else {
            Some(bits)
        }
    }
}

/// Pure-Rust ITU-T.81 Lossless JPEG (`SOF3` `0xFFC3`) decoder supporting 1..4 components and predictors 1..7.
fn decode_lossless_jpeg(stream: &[u8], expected_pixels: usize) -> Option<Vec<u16>> {
    if stream.len() < 16 || stream[0] != 0xFF || stream[1] != 0xD8 {
        return None;
    }
    let mut pos = 2;
    let mut tables: [Option<HuffTable>; 4] = [None, None, None, None];
    let mut precision = 14_u8;
    let mut height = 0_usize;
    let mut width = 0_usize;
    let mut components = 1_usize;
    let mut comp_ids = [0_u8; 4];
    let mut comp_table = [0_usize; 4];
    let mut predictor = 1_u8;
    let mut point_transform = 0_u8;
    let mut entropy_start = None;

    while pos + 4 <= stream.len() {
        if stream[pos] != 0xFF {
            pos += 1;
            continue;
        }
        let marker = stream[pos + 1];
        if marker == 0xFF {
            pos += 1;
            continue;
        }
        if marker == 0xD9 {
            break;
        }
        let seg_len = u16::from_be_bytes([stream[pos + 2], stream[pos + 3]]) as usize;
        if seg_len < 2 || pos + 2 + seg_len > stream.len() {
            break;
        }
        let seg = &stream[pos + 4..pos + 2 + seg_len];

        match marker {
            0xC4 => {
                // DHT (Define Huffman Table)
                let mut p = 0;
                while p + 17 <= seg.len() {
                    let tc_th = seg[p];
                    let th = usize::from(tc_th & 0x0F).min(3);
                    let mut counts = [0_u8; 16];
                    counts.copy_from_slice(&seg[p + 1..p + 17]);
                    let total_vals: usize = counts.iter().map(|&c| usize::from(c)).sum();
                    p += 17;
                    if p + total_vals > seg.len() {
                        break;
                    }
                    tables[th] = Some(HuffTable::from_counts_and_vals(
                        &counts,
                        &seg[p..p + total_vals],
                    ));
                    p += total_vals;
                }
            }
            0xC3 => {
                // SOF3 (Start of Frame, Lossless Huffman)
                if seg.len() >= 6 {
                    precision = seg[0];
                    height = u16::from_be_bytes([seg[1], seg[2]]) as usize;
                    width = u16::from_be_bytes([seg[3], seg[4]]) as usize;
                    components = usize::from(seg[5]).clamp(1, 4);
                    for c in 0..components {
                        if 6 + c * 3 < seg.len() {
                            comp_ids[c] = seg[6 + c * 3];
                        }
                    }
                }
            }
            0xDA => {
                // SOS (Start of Scan)
                if seg.len() >= 1 + components * 2 + 3 {
                    let ns = usize::from(seg[0]).clamp(1, 4);
                    for c in 0..ns {
                        let cs = seg[1 + c * 2];
                        let td = usize::from(seg[1 + c * 2 + 1] >> 4).min(3);
                        if let Some(idx) = comp_ids[..components].iter().position(|&id| id == cs) {
                            comp_table[idx] = td;
                        } else {
                            comp_table[c] = td;
                        }
                    }
                    predictor = seg[1 + ns * 2];
                    point_transform = seg[1 + ns * 2 + 2] & 0x0F;
                }
                entropy_start = Some(pos + 2 + seg_len);
                break;
            }
            _ => {}
        }
        pos += 2 + seg_len;
    }

    let start = entropy_start?;
    let total_samples = width.checked_mul(height)?.checked_mul(components)?;
    if total_samples == 0 || total_samples > expected_pixels * 2 {
        return None;
    }

    let default_tbl = tables.iter().find_map(|t| t.clone())?;
    let resolved_tables: [HuffTable; 4] = [
        tables[comp_table[0]]
            .clone()
            .unwrap_or_else(|| default_tbl.clone()),
        tables[comp_table[1]]
            .clone()
            .unwrap_or_else(|| default_tbl.clone()),
        tables[comp_table[2]]
            .clone()
            .unwrap_or_else(|| default_tbl.clone()),
        tables[comp_table[3]].clone().unwrap_or(default_tbl),
    ];

    let mut reader = JpegBitReader::new(&stream[start..]);
    let stride = width * components;
    let mut out = vec![0_u16; total_samples];
    let init_pred = 1_i32 << (precision.saturating_sub(point_transform + 1));

    for y in 0..height {
        let row_off = y * stride;
        for x in 0..width {
            for (c, tbl) in resolved_tables.iter().enumerate().take(components) {
                let idx = row_off + x * components + c;
                let diff = reader.decode_diff(tbl).unwrap_or(0);
                let pred = if y == 0 && x == 0 {
                    init_pred
                } else if y == 0 {
                    i32::from(out[idx - components])
                } else if x == 0 {
                    i32::from(out[idx - stride])
                } else {
                    let ra = i32::from(out[idx - components]);
                    let rb = i32::from(out[idx - stride]);
                    let rc = i32::from(out[idx - stride - components]);
                    match predictor {
                        1 => ra,
                        2 => rb,
                        3 => rc,
                        4 => ra + rb - rc,
                        5 => ra + ((rb - rc) >> 1),
                        6 => rb + ((ra - rc) >> 1),
                        7 => (ra + rb) >> 1,
                        _ => ra,
                    }
                };
                let val = ((pred + diff) & 0xFFFF) as u16;
                out[idx] = val << point_transform;
            }
        }
    }

    out.truncate(expected_pixels);
    (out.len() == expected_pixels).then_some(out)
}

/// Synthesizes a valid DNG/TIFF RAW container in memory with both an embedded JPEG preview
/// (for Stage 1 `< 20 ms` instant preview) and a 14-bit RGGB CFA SubIFD (for Stage 2 linear CFA).
pub fn build_synthetic_dng_raw(width: usize, height: usize) -> Vec<u8> {
    // 1. Build a small embedded preview JPEG
    let mut preview_rgb = image::RgbImage::new(64, 48);
    for (x, y, px) in preview_rgb.enumerate_pixels_mut() {
        *px = image::Rgb([(x * 3) as u8, (y * 4) as u8, 120]);
    }
    let mut jpeg_bytes = Vec::new();
    preview_rgb
        .write_to(&mut Cursor::new(&mut jpeg_bytes), image::ImageFormat::Jpeg)
        .expect("encode embedded preview JPEG");

    // 2. Build a 14-bit RGGB Bayer CFA plane with a realistic sky background + bright star
    let mut cfa_bytes = Vec::with_capacity(width * height * 2);
    let cx = width as f32 * 0.5;
    let cy = height as f32 * 0.5;
    for y in 0..height {
        for x in 0..width {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let r2 = dx * dx + dy * dy;
            let star = 8000.0 * (-r2 / 12.0).exp();
            let sky = match (x & 1, y & 1) {
                (0, 0) => 512.0 + 180.0, // R
                (1, 1) => 512.0 + 150.0, // B
                _ => 512.0 + 260.0,      // G
            };
            let sample = (sky + star).clamp(0.0, 16383.0) as u16;
            cfa_bytes.extend_from_slice(&sample.to_le_bytes());
        }
    }

    // 3. Assemble little-endian TIFF/DNG with IFD0 (preview + camera EXIF) and SubIFD (14-bit CFA)
    let mut buf = Vec::new();
    buf.extend_from_slice(b"II\x2A\x00");
    buf.extend_from_slice(&8_u32.to_le_bytes()); // IFD0 starts at offset 8

    let make_str = b"Canon\0";
    let model_str = b"Canon EOS 6D\0";

    // Reserve space for IFD0 (7 entries: 2 + 7*12 + 4 = 90 bytes) and SubIFD (10 entries: 2 + 10*12 + 4 = 126 bytes)
    let ifd0_off = 8_usize;
    let subifd_off = ifd0_off + 90;
    let data_off = subifd_off + 126;

    let make_off = data_off;
    let model_off = make_off + make_str.len();
    let jpeg_off = model_off + model_str.len();
    let cfa_off = jpeg_off + jpeg_bytes.len();

    // Write IFD0
    let mut ifd0 = Vec::new();
    ifd0.extend_from_slice(&7_u16.to_le_bytes());
    write_ifd_entry(&mut ifd0, 0x00FE, 4, 1, 1); // SubfileType = Preview
    write_ifd_entry(&mut ifd0, 0x010F, 2, make_str.len() as u32, make_off as u32);
    write_ifd_entry(
        &mut ifd0,
        0x0110,
        2,
        model_str.len() as u32,
        model_off as u32,
    );
    write_ifd_entry(&mut ifd0, 0x014A, 4, 1, subifd_off as u32); // SubIFDs -> subifd_off
    write_ifd_entry(&mut ifd0, 0x0201, 4, 1, jpeg_off as u32); // JPEGInterchangeFormat
    write_ifd_entry(&mut ifd0, 0x0202, 4, 1, jpeg_bytes.len() as u32); // JPEGInterchangeFormatLength
    write_ifd_entry(&mut ifd0, 0x8827, 3, 1, 1600); // ISOSpeedRatings = 1600
    ifd0.extend_from_slice(&0_u32.to_le_bytes()); // next IFD = 0
    assert_eq!(ifd0.len(), 90);

    // Write SubIFD (14-bit uncompressed RGGB CFA)
    let mut subifd = Vec::new();
    subifd.extend_from_slice(&10_u16.to_le_bytes());
    write_ifd_entry(&mut subifd, 0x00FE, 4, 1, 0); // SubfileType = Full-res RAW
    write_ifd_entry(&mut subifd, 0x0100, 4, 1, width as u32);
    write_ifd_entry(&mut subifd, 0x0101, 4, 1, height as u32);
    write_ifd_entry(&mut subifd, 0x0102, 3, 1, 14); // 14 bits/sample
    write_ifd_entry(&mut subifd, 0x0103, 3, 1, 1); // Compression = Uncompressed
    write_ifd_entry(&mut subifd, 0x0106, 3, 1, 32803); // Photometric = CFA
    write_ifd_entry(&mut subifd, 0x0111, 4, 1, cfa_off as u32); // StripOffsets
    write_ifd_entry(&mut subifd, 0x0115, 3, 1, 1); // SamplesPerPixel = 1
    write_ifd_entry(&mut subifd, 0x0117, 4, 1, cfa_bytes.len() as u32); // StripByteCounts
    write_ifd_entry(
        &mut subifd,
        0x828E,
        1,
        4,
        u32::from_le_bytes([0, 1, 1, 2]), // RGGB
    );
    subifd.extend_from_slice(&0_u32.to_le_bytes());
    assert_eq!(subifd.len(), 126);

    buf.extend_from_slice(&ifd0);
    buf.extend_from_slice(&subifd);
    buf.extend_from_slice(make_str);
    buf.extend_from_slice(model_str);
    buf.extend_from_slice(&jpeg_bytes);
    buf.extend_from_slice(&cfa_bytes);
    buf
}

fn write_ifd_entry(buf: &mut Vec<u8>, tag: u16, field_type: u16, count: u32, val_or_off: u32) {
    buf.extend_from_slice(&tag.to_le_bytes());
    buf.extend_from_slice(&field_type.to_le_bytes());
    buf.extend_from_slice(&count.to_le_bytes());
    buf.extend_from_slice(&val_or_off.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_two_stage_raw_preview_and_linear_cfa() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dng_path = tmp.path().join("Light_M42_EOS6D.dng");
        let bytes = build_synthetic_dng_raw(128, 96);
        std::fs::write(&dng_path, &bytes).expect("write dng");

        let cancel = AtomicBool::new(false);
        let s1 = read_raw_stage1_preview(&dng_path, LayoutChoice::Auto, &cancel)
            .expect("Stage 1 preview");
        assert_eq!(s1.rgb.width, 64);
        assert_eq!(s1.rgb.height, 48);
        assert!(!s1.default_stretch);
        assert!(
            s1.cards
                .iter()
                .any(|c| c.keyword == "INSTRUME" && c.value.contains("EOS 6D"))
        );

        let s2 = read_raw_stage2_linear(&dng_path, LayoutChoice::Auto, &cancel)
            .expect("Stage 2 linear CFA");
        match s2 {
            RawStage2Output::Cfa(raw) => {
                assert_eq!(raw.header.info.width, 128);
                assert_eq!(raw.header.info.height, 96);
                assert_eq!(raw.header.info.bayer_pattern, Some(BayerPattern::RGGB));
                assert_eq!(raw.pixels.len(), 128 * 96);
            }
            RawStage2Output::LinearRgb(_) => panic!("expected CFA output"),
        }
    }
}
