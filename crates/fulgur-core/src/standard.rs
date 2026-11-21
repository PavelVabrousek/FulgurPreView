//! Standard image reader (TIFF 8/16/32-bit, JPEG, PNG, WebP, BMP) with EXIF metadata.

use crate::exif::{ExifSummary, read_exif};
use crate::fits::FrameInfo;
use crate::{DemosaicMethod, Error, HeaderCard, Mosaic, Result, RgbImage, check_cancelled};
use image::{DynamicImage, GenericImageView};
use std::path::Path;
use std::sync::atomic::AtomicBool;

pub const STANDARD_EXTENSIONS: [&str; 7] = ["tif", "tiff", "jpg", "jpeg", "png", "webp", "bmp"];

pub fn is_standard_image(path: &Path) -> bool {
    path.extension().is_some_and(|ext| {
        STANDARD_EXTENSIONS
            .iter()
            .any(|expected| ext.eq_ignore_ascii_case(expected))
    })
}

pub struct StandardFrame {
    pub info: FrameInfo,
    pub cards: Vec<HeaderCard>,
    pub rgb: RgbImage,
    /// True when this is a 16-bit or 32-bit linear astro TIFF that benefits from MTF auto-stretch;
    /// false for standard rendered 8-bit images (JPEG, PNG, WebP, BMP, standard 8-bit TIFF).
    pub default_stretch: bool,
}

pub fn read_standard(path: &Path, cancel: &AtomicBool) -> Result<StandardFrame> {
    check_cancelled(cancel)?;
    let exif = read_exif(path);
    let reader = image::ImageReader::open(path)?.with_guessed_format()?;
    let guessed_format = reader.format();
    let mut format_name = guessed_format
        .map_or("Image", |f| {
            f.extensions_str().first().copied().unwrap_or("img")
        })
        .to_ascii_uppercase();
    let (width, height, bitpix, is_high_depth, pixels) = match reader.decode() {
        Ok(decoded) => {
            check_cancelled(cancel)?;
            let (width, height) = decoded.dimensions();
            let (bitpix, is_high_depth, pixels) = pixels_from_dynamic(&decoded);
            (
                width as usize,
                height as usize,
                bitpix,
                is_high_depth,
                pixels,
            )
        }
        Err(image_err) => {
            let is_tiff = guessed_format == Some(image::ImageFormat::Tiff)
                || path.extension().is_some_and(|e| {
                    e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff")
                });
            if !is_tiff {
                return Err(decode_error(format!(
                    "Could not decode standard image file: {image_err}"
                )));
            }
            format_name = "TIF".to_owned();
            read_tiff_direct(path, cancel).map_err(|tiff_err| match tiff_err {
                Error::Cancelled => Error::Cancelled,
                other => decode_error(format!(
                    "Could not decode TIFF file: {other} (image decoder: {image_err})"
                )),
            })?
        }
    };
    check_cancelled(cancel)?;
    if width == 0 || height == 0 {
        return Err(Error::Invalid("Standard image has zero dimensions."));
    }

    // ADR-004 / PROJECT_SPEC (ADR-021): rendered 8-bit images open unstretched.
    // Floating-point TIFFs are processing masters (PixInsight, Siril) and are
    // always linear; integer 16/32-bit TIFFs auto-stretch when their median is low.
    let default_stretch = bitpix < 0 || (is_high_depth && is_linear_signal(&pixels));

    finish_frame(
        width,
        height,
        bitpix,
        pixels,
        format_name,
        exif,
        default_stretch,
    )
}

fn decode_error(message: String) -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

/// Median of a sparse sample of the green channel, on the 0..65535 ADU scale,
/// below a quarter of full scale means the data has not been stretched yet.
fn is_linear_signal(pixels: &[[f32; 3]]) -> bool {
    let step = (pixels.len() / 200_000).max(1);
    let mut sample: Vec<f32> = pixels
        .iter()
        .step_by(step)
        .map(|p| p[1])
        .filter(|v| v.is_finite())
        .collect();
    if sample.is_empty() {
        return false;
    }
    let mid = sample.len() / 2;
    let (_, median, _) = sample.select_nth_unstable_by(mid, f32::total_cmp);
    *median < 0.25 * 65535.0
}

/// Decoded pixel planes: (width, height, bitpix, is_high_depth, RGB pixels in 0..65535 ADU).
type DirectTiff = (usize, usize, i32, bool, Vec<[f32; 3]>);

/// Read TIFF layouts the `image` crate does not decode (notably PixInsight's
/// mono 32-bit float masters) with the pure-Rust `tiff` crate.
fn read_tiff_direct(path: &Path, cancel: &AtomicBool) -> Result<DirectTiff> {
    use tiff::ColorType;
    use tiff::decoder::{Decoder, DecodingResult, Limits};

    let tiff_err = |e: tiff::TiffError| decode_error(e.to_string());
    let file = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut decoder = Decoder::new(file)
        .map_err(tiff_err)?
        .with_limits(Limits::unlimited());
    let (w, h) = decoder.dimensions().map_err(tiff_err)?;
    let (width, height) = (w as usize, h as usize);
    let channels = match decoder.colortype().map_err(tiff_err)? {
        ColorType::Gray(_) => 1,
        ColorType::GrayA(_) => 2,
        ColorType::RGB(_) => 3,
        ColorType::RGBA(_) => 4,
        other => {
            return Err(decode_error(format!(
                "unsupported TIFF colour type {other:?}"
            )));
        }
    };
    check_cancelled(cancel)?;
    let data = decoder.read_image().map_err(tiff_err)?;
    check_cancelled(cancel)?;

    // Every variant is mapped onto the app's 0..65535 ADU scale.
    let (bitpix, high, samples): (i32, bool, Vec<f32>) = match data {
        DecodingResult::U8(v) => (8, false, v.iter().map(|&x| f32::from(x) * 257.0).collect()),
        DecodingResult::U16(v) => (16, true, v.iter().map(|&x| f32::from(x)).collect()),
        DecodingResult::U32(v) => (
            32,
            true,
            v.iter().map(|&x| (f64::from(x) / 65537.0) as f32).collect(),
        ),
        DecodingResult::U64(v) => (
            64,
            true,
            v.iter()
                .map(|&x| (x as f64 / 281_479_271_743_489.0) as f32)
                .collect(),
        ),
        DecodingResult::I8(v) => (
            8,
            false,
            v.iter().map(|&x| (f32::from(x) + 128.0) * 257.0).collect(),
        ),
        DecodingResult::I16(v) => (
            16,
            true,
            v.iter().map(|&x| f32::from(x) + 32768.0).collect(),
        ),
        DecodingResult::I32(v) => (
            32,
            true,
            v.iter()
                .map(|&x| ((f64::from(x) + 2_147_483_648.0) / 65537.0) as f32)
                .collect(),
        ),
        DecodingResult::I64(v) => (
            64,
            true,
            v.iter()
                .map(|&x| ((x as f64 + 9.223_372_036_854_776e18) / 281_479_271_743_489.0) as f32)
                .collect(),
        ),
        DecodingResult::F16(v) => (
            -32,
            true,
            normalise_float(v.iter().map(|x| f32::from(*x)).collect()),
        ),
        DecodingResult::F32(v) => (-32, true, normalise_float(v)),
        DecodingResult::F64(v) => (
            -64,
            true,
            normalise_float(v.iter().map(|&x| x as f32).collect()),
        ),
    };
    if samples.len() < width * height * channels {
        return Err(Error::Invalid(
            "TIFF pixel data is shorter than its dimensions.",
        ));
    }
    let pixels = samples
        .chunks_exact(channels)
        .take(width * height)
        .map(|c| match channels {
            1 | 2 => [c[0], c[0], c[0]],
            _ => [c[0], c[1], c[2]],
        })
        .collect();
    Ok((width, height, bitpix, high, pixels))
}

/// Float data in the conventional 0..1 range is scaled to 0..65535 ADU; data
/// already on an ADU scale is kept. NaN/negative samples are clamped.
fn normalise_float(mut samples: Vec<f32>) -> Vec<f32> {
    let max_val = samples
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .fold(0.0_f32, f32::max);
    let scale = if max_val <= 1.5 { 65535.0 } else { 1.0 };
    for v in &mut samples {
        *v = if v.is_finite() {
            (*v * scale).clamp(0.0, 65535.0)
        } else {
            0.0
        };
    }
    samples
}

fn pixels_from_dynamic(decoded: &DynamicImage) -> (i32, bool, Vec<[f32; 3]>) {
    match decoded {
        DynamicImage::ImageLuma8(buf) => {
            let px = buf
                .pixels()
                .map(|p| {
                    let v = f32::from(p.0[0]) * 257.0;
                    [v, v, v]
                })
                .collect();
            (8, false, px)
        }
        DynamicImage::ImageLumaA8(buf) => {
            let px = buf
                .pixels()
                .map(|p| {
                    let v = f32::from(p.0[0]) * 257.0;
                    [v, v, v]
                })
                .collect();
            (8, false, px)
        }
        DynamicImage::ImageRgb8(buf) => {
            let px = buf
                .pixels()
                .map(|p| {
                    [
                        f32::from(p.0[0]) * 257.0,
                        f32::from(p.0[1]) * 257.0,
                        f32::from(p.0[2]) * 257.0,
                    ]
                })
                .collect();
            (8, false, px)
        }
        DynamicImage::ImageRgba8(buf) => {
            let px = buf
                .pixels()
                .map(|p| {
                    [
                        f32::from(p.0[0]) * 257.0,
                        f32::from(p.0[1]) * 257.0,
                        f32::from(p.0[2]) * 257.0,
                    ]
                })
                .collect();
            (8, false, px)
        }
        DynamicImage::ImageLuma16(buf) => {
            let px = buf
                .pixels()
                .map(|p| {
                    let v = f32::from(p.0[0]);
                    [v, v, v]
                })
                .collect();
            (16, true, px)
        }
        DynamicImage::ImageLumaA16(buf) => {
            let px = buf
                .pixels()
                .map(|p| {
                    let v = f32::from(p.0[0]);
                    [v, v, v]
                })
                .collect();
            (16, true, px)
        }
        DynamicImage::ImageRgb16(buf) => {
            let px = buf
                .pixels()
                .map(|p| [f32::from(p.0[0]), f32::from(p.0[1]), f32::from(p.0[2])])
                .collect();
            (16, true, px)
        }
        DynamicImage::ImageRgba16(buf) => {
            let px = buf
                .pixels()
                .map(|p| [f32::from(p.0[0]), f32::from(p.0[1]), f32::from(p.0[2])])
                .collect();
            (16, true, px)
        }
        DynamicImage::ImageRgb32F(buf) => {
            let max_val = buf.pixels().flat_map(|p| p.0).fold(0.0_f32, f32::max);
            let scale = if max_val <= 1.5 { 65535.0 } else { 1.0 };
            let px = buf
                .pixels()
                .map(|p| {
                    [
                        (p.0[0] * scale).clamp(0.0, 65535.0),
                        (p.0[1] * scale).clamp(0.0, 65535.0),
                        (p.0[2] * scale).clamp(0.0, 65535.0),
                    ]
                })
                .collect();
            (-32, true, px)
        }
        DynamicImage::ImageRgba32F(buf) => {
            let max_val = buf
                .pixels()
                .flat_map(|p| [p.0[0], p.0[1], p.0[2]])
                .fold(0.0_f32, f32::max);
            let scale = if max_val <= 1.5 { 65535.0 } else { 1.0 };
            let px = buf
                .pixels()
                .map(|p| {
                    [
                        (p.0[0] * scale).clamp(0.0, 65535.0),
                        (p.0[1] * scale).clamp(0.0, 65535.0),
                        (p.0[2] * scale).clamp(0.0, 65535.0),
                    ]
                })
                .collect();
            (-32, true, px)
        }
        other => {
            let rgb8 = other.to_rgb8();
            let px = rgb8
                .pixels()
                .map(|p| {
                    [
                        f32::from(p.0[0]) * 257.0,
                        f32::from(p.0[1]) * 257.0,
                        f32::from(p.0[2]) * 257.0,
                    ]
                })
                .collect();
            (8, false, px)
        }
    }
}

fn finish_frame(
    width: usize,
    height: usize,
    bitpix: i32,
    pixels: Vec<[f32; 3]>,
    format_name: String,
    exif: ExifSummary,
    default_stretch: bool,
) -> Result<StandardFrame> {
    let camera_label = match (&exif.make, &exif.model) {
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

    let mut cards = vec![
        HeaderCard {
            keyword: "FORMAT".to_owned(),
            value: format_name,
            comment: "Container format".to_owned(),
        },
        HeaderCard {
            keyword: "NAXIS1".to_owned(),
            value: width.to_string(),
            comment: "Width in pixels".to_owned(),
        },
        HeaderCard {
            keyword: "NAXIS2".to_owned(),
            value: height.to_string(),
            comment: "Height in pixels".to_owned(),
        },
        HeaderCard {
            keyword: "BITPIX".to_owned(),
            value: bitpix.to_string(),
            comment: "Bits per channel sample".to_owned(),
        },
    ];
    cards.extend(exif.cards);

    let info = FrameInfo {
        width,
        height,
        planes: 3,
        bitpix,
        bzero: 0.0,
        bscale: 1.0,
        bayer_pattern: None,
        mosaic: Mosaic::Rgb,
        mosaic_source: "rendered_rgb",
        exposure_seconds: exif.exposure_seconds.unwrap_or(0.0),
        object: None,
        filter: None,
        date_obs: exif.date_time,
        gain: exif.iso,
        ccd_temperature: None,
        instrument: camera_label,
        telescope: exif.lens,
        bias_informational: None,
        creator: exif.software,
        saturation_adu: None,
        derived: Vec::new(),
        rows_mirrored: false,
    };

    Ok(StandardFrame {
        info,
        cards,
        rgb: RgbImage {
            width,
            height,
            demosaic_method: DemosaicMethod::External,
            pixels,
        },
        default_stretch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiff::encoder::{TiffEncoder, colortype};

    fn write_tiff<C: colortype::ColorType>(path: &Path, w: u32, h: u32, data: &[C::Inner])
    where
        [C::Inner]: tiff::encoder::TiffValue,
    {
        let file = std::fs::File::create(path).unwrap();
        let mut encoder = TiffEncoder::new(file).unwrap();
        let mut image = encoder.new_image::<C>(w, h).unwrap();
        image.rows_per_strip(1).unwrap();
        image.write_data(data).unwrap();
    }

    #[test]
    fn mono_float32_tiff_like_pixinsight_masters_opens_linear_and_stretched() {
        // PixInsight master layout: 1 sample, 32-bit IEEE float in 0..1, one row per strip.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("master_L_float32.tif");
        let (w, h) = (48_u32, 32_u32);
        let mut data: Vec<f32> = (0..w * h).map(|i| 0.02 + (i % 7) as f32 * 0.001).collect();
        data[(16 * w + 24) as usize] = 1.0; // one saturated star
        write_tiff::<colortype::Gray32Float>(&path, w, h, &data);

        let frame = read_standard(&path, &AtomicBool::new(false)).unwrap();
        assert_eq!((frame.rgb.width, frame.rgb.height), (48, 32));
        assert_eq!(frame.info.bitpix, -32);
        assert!(
            frame.default_stretch,
            "linear float master must auto-stretch"
        );
        let px = frame.rgb.pixels[0];
        assert!((px[0] - 0.02 * 65535.0).abs() < 1.0 && px[0] == px[1] && px[1] == px[2]);
        assert!((frame.rgb.pixels[(16 * w + 24) as usize][0] - 65535.0).abs() < 0.5);
    }

    #[test]
    fn eight_bit_and_bright_sixteen_bit_tiffs_stay_unstretched() {
        let dir = tempfile::tempdir().unwrap();
        let rgb8 = dir.path().join("rendered.tif");
        write_tiff::<colortype::RGB8>(&rgb8, 4, 4, &[200_u8; 48]);
        let frame = read_standard(&rgb8, &AtomicBool::new(false)).unwrap();
        assert!(!frame.default_stretch);

        let gray16 = dir.path().join("stretched16.tif");
        write_tiff::<colortype::Gray16>(&gray16, 4, 4, &[40_000_u16; 16]);
        let frame = read_standard(&gray16, &AtomicBool::new(false)).unwrap();
        assert!(
            !frame.default_stretch,
            "already-stretched 16-bit data has a high median"
        );
    }

    #[test]
    fn undecodable_files_report_the_underlying_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.tif");
        std::fs::write(&path, b"II*\0garbage").unwrap();
        let Err(err) = read_standard(&path, &AtomicBool::new(false)) else {
            panic!("garbage must not decode");
        };
        let text = err.to_string();
        assert!(text.starts_with("Could not decode TIFF file: "), "{text}");
        assert!(text.contains("image decoder:"), "{text}");
    }
}
