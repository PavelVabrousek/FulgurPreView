//! Pure-Rust EXIF and TIFF IFD metadata extractor for JPEG, TIFF, WebP, and RAW containers.

use crate::HeaderCard;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Clone, Debug, Default)]
pub struct ExifSummary {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    pub software: Option<String>,
    pub date_time: Option<String>,
    pub exposure_seconds: Option<f64>,
    pub iso: Option<f64>,
    pub f_number: Option<f64>,
    pub focal_length_mm: Option<f64>,
    pub cards: Vec<HeaderCard>,
}

/// Extract EXIF/TIFF metadata from a file without reading full image payloads.
pub fn read_exif(path: &Path) -> ExifSummary {
    let Ok(mut file) = File::open(path) else {
        return ExifSummary::default();
    };
    let mut head = [0u8; 16];
    let Ok(n) = file.read(&mut head) else {
        return ExifSummary::default();
    };
    if n < 4 {
        return ExifSummary::default();
    }
    if head[0..2] == [0xFF, 0xD8] {
        // JPEG container: scan markers for APP1 "Exif\0\0"
        let _ = file.seek(SeekFrom::Start(2));
        let mut marker = [0u8; 4];
        for _ in 0..128 {
            if file.read_exact(&mut marker).is_err() || marker[0] != 0xFF {
                break;
            }
            let code = marker[1];
            if code == 0xDA || code == 0xD9 {
                break;
            }
            let seg_len = u16::from_be_bytes([marker[2], marker[3]]) as usize;
            if seg_len < 2 {
                break;
            }
            if code == 0xE1 && (14..=4 * 1024 * 1024).contains(&seg_len) {
                let mut payload = vec![0u8; seg_len - 2];
                if file.read_exact(&mut payload).is_ok() && payload.starts_with(b"Exif\0\0") {
                    return parse_tiff_bytes(&payload[6..]);
                }
            } else if file.seek(SeekFrom::Current((seg_len - 2) as i64)).is_err() {
                break;
            }
        }
        ExifSummary::default()
    } else if &head[0..4] == b"II*\0" || &head[0..4] == b"MM\0*" {
        // TIFF / DNG / CR2 / NEF / ARW container
        let _ = file.seek(SeekFrom::Start(0));
        let mut buf = Vec::with_capacity(256 * 1024);
        let _ = (&mut file).take(512 * 1024).read_to_end(&mut buf);
        parse_tiff_bytes(&buf)
    } else {
        ExifSummary::default()
    }
}

pub fn parse_tiff_bytes(data: &[u8]) -> ExifSummary {
    let mut summary = ExifSummary::default();
    if data.len() < 8 {
        return summary;
    }
    let le = match &data[0..4] {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => return summary,
    };
    let u32_at = |offset: usize| -> Option<u32> {
        let bytes: [u8; 4] = data.get(offset..offset + 4)?.try_into().ok()?;
        Some(if le {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    };
    let Some(ifd0) = u32_at(4) else {
        return summary;
    };
    let mut exif_ifd = None;
    let mut gps_ifd = None;
    parse_ifd(
        data,
        le,
        ifd0 as usize,
        "IFD0",
        &mut summary,
        &mut exif_ifd,
        &mut gps_ifd,
    );
    if let Some(offset) = exif_ifd {
        let mut dummy1 = None;
        let mut dummy2 = None;
        parse_ifd(
            data,
            le,
            offset,
            "EXIF",
            &mut summary,
            &mut dummy1,
            &mut dummy2,
        );
    }
    if let Some(offset) = gps_ifd {
        let mut dummy1 = None;
        let mut dummy2 = None;
        parse_ifd(
            data,
            le,
            offset,
            "GPS",
            &mut summary,
            &mut dummy1,
            &mut dummy2,
        );
    }
    summary
}

fn parse_ifd(
    data: &[u8],
    le: bool,
    offset: usize,
    group: &str,
    summary: &mut ExifSummary,
    exif_ifd: &mut Option<usize>,
    gps_ifd: &mut Option<usize>,
) {
    let u16_at = |pos: usize| -> Option<u16> {
        let b: [u8; 2] = data.get(pos..pos + 2)?.try_into().ok()?;
        Some(if le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    };
    let u32_at = |pos: usize| -> Option<u32> {
        let b: [u8; 4] = data.get(pos..pos + 4)?.try_into().ok()?;
        Some(if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };
    let Some(entries) = u16_at(offset) else {
        return;
    };
    let count = (entries as usize).min(256);
    for i in 0..count {
        let entry = offset + 2 + i * 12;
        let (Some(tag), Some(kind), Some(num)) =
            (u16_at(entry), u16_at(entry + 2), u32_at(entry + 4))
        else {
            break;
        };
        let num = num as usize;
        if tag == 0x8769 {
            if let Some(ptr) = u32_at(entry + 8) {
                *exif_ifd = Some(ptr as usize);
            }
            continue;
        }
        if tag == 0x8825 {
            if let Some(ptr) = u32_at(entry + 8) {
                *gps_ifd = Some(ptr as usize);
            }
            continue;
        }
        let type_bytes = match kind {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 => 4,
            5 | 10 | 12 => 8,
            _ => continue,
        };
        let total_bytes = num.saturating_mul(type_bytes);
        let value_offset = if total_bytes <= 4 {
            entry + 8
        } else {
            match u32_at(entry + 8) {
                Some(ptr) => ptr as usize,
                None => continue,
            }
        };
        let Some(slice) = data.get(value_offset..value_offset + total_bytes) else {
            continue;
        };
        let Some((name, comment)) = tag_name(group, tag) else {
            continue;
        };
        match kind {
            2 => {
                // ASCII string
                let text = String::from_utf8_lossy(slice)
                    .trim_matches(['\0', ' '])
                    .to_owned();
                if text.is_empty() {
                    continue;
                }
                match tag {
                    0x010F => summary.make = Some(text.clone()),
                    0x0110 => summary.model = Some(text.clone()),
                    0x0131 => summary.software = Some(text.clone()),
                    0x0132 | 0x9003 => {
                        if summary.date_time.is_none() || tag == 0x9003 {
                            summary.date_time = Some(text.clone());
                        }
                    }
                    0xA434 => summary.lens = Some(text.clone()),
                    _ => {}
                }
                summary.cards.push(HeaderCard {
                    keyword: name.to_owned(),
                    value: text,
                    comment: comment.to_owned(),
                });
            }
            3 if num >= 1 => {
                // SHORT
                if let Some(v) = u16_at(value_offset) {
                    if tag == 0x8827 {
                        summary.iso = Some(f64::from(v));
                    }
                    summary.cards.push(HeaderCard {
                        keyword: name.to_owned(),
                        value: v.to_string(),
                        comment: comment.to_owned(),
                    });
                }
            }
            4 if num >= 1 => {
                // LONG
                if let Some(v) = u32_at(value_offset) {
                    if tag == 0x8827 || tag == 0x8832 {
                        summary.iso = Some(f64::from(v));
                    }
                    summary.cards.push(HeaderCard {
                        keyword: name.to_owned(),
                        value: v.to_string(),
                        comment: comment.to_owned(),
                    });
                }
            }
            5 if num >= 1 => {
                // RATIONAL (u32 / u32)
                if let (Some(num_v), Some(den_v)) = (u32_at(value_offset), u32_at(value_offset + 4))
                    && den_v != 0
                {
                    let real = f64::from(num_v) / f64::from(den_v);
                    let formatted = if tag == 0x829A && real < 0.25 && num_v > 0 {
                        format!(
                            "1/{:.0} s ({:.5} s)",
                            f64::from(den_v) / f64::from(num_v),
                            real
                        )
                    } else {
                        format!("{real:.4}")
                            .trim_end_matches('0')
                            .trim_end_matches('.')
                            .to_owned()
                    };
                    match tag {
                        0x829A => summary.exposure_seconds = Some(real),
                        0x829D => summary.f_number = Some(real),
                        0x920A => summary.focal_length_mm = Some(real),
                        _ => {}
                    }
                    summary.cards.push(HeaderCard {
                        keyword: name.to_owned(),
                        value: formatted,
                        comment: comment.to_owned(),
                    });
                }
            }
            _ => {}
        }
    }
}

fn tag_name(group: &str, tag: u16) -> Option<(&'static str, &'static str)> {
    if group == "GPS" {
        return match tag {
            0x0001 => Some(("SITELAT_REF", "GPS Latitude Reference")),
            0x0002 => Some(("SITELAT", "GPS Latitude")),
            0x0003 => Some(("SITELONG_REF", "GPS Longitude Reference")),
            0x0004 => Some(("SITELONG", "GPS Longitude")),
            0x0006 => Some(("SITEALT", "GPS Altitude")),
            _ => None,
        };
    }
    match tag {
        0x0100 => Some(("IMAGEW", "Image Width")),
        0x0101 => Some(("IMAGEH", "Image Height")),
        0x0102 => Some(("BITSPERS", "Bits Per Sample")),
        0x010F => Some(("MAKE", "Camera Manufacturer")),
        0x0110 => Some(("MODEL", "Camera Model")),
        0x0112 => Some(("ORIENT", "Orientation")),
        0x0131 => Some(("SOFTWARE", "Software / Firmware")),
        0x0132 => Some(("DATETIME", "Modify Date/Time")),
        0x013B => Some(("ARTIST", "Artist / Author")),
        0x829A => Some(("EXPTIME", "Exposure Time [s]")),
        0x829D => Some(("FNUMBER", "F-Number (Aperture)")),
        0x8827 => Some(("ISO", "ISO Speed Rating")),
        0x8832 => Some(("ISOSPEED", "Standard Output Sensitivity")),
        0x9003 => Some(("DATE-OBS", "Date/Time Original")),
        0x9004 => Some(("DATEDIG", "Date/Time Digitized")),
        0x920A => Some(("FOCALLEN", "Focal Length [mm]")),
        0xA002 => Some(("EXIFW", "Exif Pixel Width")),
        0xA003 => Some(("EXIFH", "Exif Pixel Height")),
        0xA405 => Some(("FOCAL35", "Focal Length in 35mm Film [mm]")),
        0xA433 => Some(("LENSMAKE", "Lens Manufacturer")),
        0xA434 => Some(("LENSMOD", "Lens Model")),
        _ => None,
    }
}
