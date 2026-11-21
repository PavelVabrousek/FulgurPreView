//! Core readers and metadata structures for FulgurPreView:
//! FITS (all BITPIX & MEF), XISF 1.0, Standard Images (TIFF/JPEG/PNG/WebP/BMP),
//! EXIF, camera catalogue (`cameras.json`), and safe `_Rejected` culling.

pub mod cameras;
pub mod culling;
pub mod derive;
pub mod exif;
pub mod fits;
pub mod raw;
pub mod standard;
pub mod telemetry;
pub mod xisf;

use serde::Serialize;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Invalid(&'static str),
    /// A failure with a message built at run time (catalogue paths, solver diagnostics).
    Message(String),
    Cancelled,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::Invalid(message) => f.write_str(message),
            Self::Message(message) => f.write_str(message),
            Self::Cancelled => f.write_str("Operation cancelled."),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

pub fn check_cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// Upper bound on any single decoded frame buffer derived from file headers (2 GiB).
///
/// Header fields of a damaged or hostile file can claim absurd geometry. Allocating such a
/// buffer would abort the whole process (allocation failure is not a recoverable panic),
/// so every header-derived allocation is checked against this budget first.
pub const MAX_DECODE_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// Hard ceiling on the decoded working size (f32 samples) of one frame (8 GiB, ADR-023).
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024 * 1024;

/// Frame budget used when the free RAM cannot be read (4 GiB).
const FALLBACK_FRAME_BYTES: usize = 4 * 1024 * 1024 * 1024;

/// Working-memory budget for one frame: half of the currently available RAM, at most
/// [`MAX_FRAME_BYTES`]; [`FALLBACK_FRAME_BYTES`] when the available RAM is unknown.
pub fn frame_budget_bytes(available_ram: Option<u64>) -> usize {
    available_ram
        .filter(|&b| b > 0)
        .map_or(FALLBACK_FRAME_BYTES, |b| {
            usize::try_from(b / 2)
                .unwrap_or(usize::MAX)
                .min(MAX_FRAME_BYTES)
        })
}

/// Refuse a frame whose decoded working size (`needed` bytes) does not fit the budget for
/// `available_ram`, with a message the user can act on.
pub fn check_frame_memory_with(needed: usize, available_ram: Option<u64>) -> Result<()> {
    let budget = frame_budget_bytes(available_ram);
    if needed <= budget {
        return Ok(());
    }
    let gb = |b: f64| b / (1024.0 * 1024.0 * 1024.0);
    let needed_gb = gb(needed as f64);
    let message = if needed > MAX_FRAME_BYTES {
        format!(
            "Image needs {needed_gb:.1} GB of memory, more than the {:.0} GB limit for one image.",
            gb(MAX_FRAME_BYTES as f64)
        )
    } else if let Some(available) = available_ram.filter(|&b| b > 0) {
        format!(
            "Image needs {needed_gb:.1} GB of memory, only {:.1} GB is available \
             (one image may use half of the {:.1} GB of free RAM). Close other programs and try again.",
            gb(budget as f64),
            gb(available as f64)
        )
    } else {
        format!(
            "Image needs {needed_gb:.1} GB of memory, only {:.1} GB is available.",
            gb(budget as f64)
        )
    };
    Err(Error::Io(std::io::Error::new(
        std::io::ErrorKind::OutOfMemory,
        message,
    )))
}

/// [`check_frame_memory_with`] against the RAM available right now.
pub fn check_frame_memory(needed: usize) -> Result<()> {
    check_frame_memory_with(
        needed,
        telemetry::system_memory().map(|m| m.available_bytes),
    )
}

/// Upper bound on the number of pixels of a single decoded frame (200 Mpx).
pub const MAX_DECODE_PIXELS: usize = 200_000_000;

/// Chunk size for cancellable bulk reads (8 MiB).
const CANCEL_READ_CHUNK: usize = 8 * 1024 * 1024;

/// Allocate a zero-filled buffer of `len` elements, refusing sizes above
/// [`MAX_DECODE_BYTES`] or that the allocator cannot satisfy, instead of aborting.
pub fn try_alloc_zeroed<T: Copy + Default>(len: usize) -> Result<Vec<T>> {
    let bytes = len
        .checked_mul(std::mem::size_of::<T>())
        .filter(|&b| b <= MAX_DECODE_BYTES)
        .ok_or(Error::Invalid("Image exceeds the decode memory budget."))?;
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(len)
        .map_err(|_| Error::Invalid("Not enough memory to decode this image."))?;
    debug_assert!(bytes <= MAX_DECODE_BYTES);
    buffer.resize(len, T::default());
    Ok(buffer)
}

/// `read_exact` in large chunks, returning [`Error::Cancelled`] as soon as `cancel` is set.
pub fn read_exact_cancellable(
    reader: &mut impl std::io::Read,
    buffer: &mut [u8],
    cancel: &AtomicBool,
) -> Result<()> {
    for chunk in buffer.chunks_mut(CANCEL_READ_CHUNK) {
        check_cancelled(cancel)?;
        reader.read_exact(chunk)?;
    }
    Ok(())
}

/// Read a whole file into memory in cancellable chunks, bounded by [`MAX_DECODE_BYTES`].
pub fn read_file_cancellable(path: &std::path::Path, cancel: &AtomicBool) -> Result<Vec<u8>> {
    check_cancelled(cancel)?;
    let mut file = std::fs::File::open(path)?;
    let len = usize::try_from(file.metadata()?.len())
        .map_err(|_| Error::Invalid("File is too large to read."))?;
    let mut bytes = try_alloc_zeroed::<u8>(len)?;
    read_exact_cancellable(&mut file, &mut bytes, cancel)?;
    Ok(bytes)
}

/// One metadata record displayed in the Metadata Inspector.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HeaderCard {
    pub keyword: String,
    pub value: String,
    pub comment: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Red,
    Green,
    Blue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub enum BayerPattern {
    GRBG,
    RGGB,
    GBRG,
    BGGR,
}

impl BayerPattern {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "GRBG" => Ok(Self::GRBG),
            "RGGB" => Ok(Self::RGGB),
            "GBRG" => Ok(Self::GBRG),
            "BGGR" => Ok(Self::BGGR),
            _ => Err(Error::Invalid("Unsupported or missing BAYERPAT.")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::GRBG => "GRBG",
            Self::RGGB => "RGGB",
            Self::GBRG => "GBRG",
            Self::BGGR => "BGGR",
        }
    }

    pub fn rows_swapped(self) -> Self {
        match self {
            Self::GRBG => Self::BGGR,
            Self::RGGB => Self::GBRG,
            Self::GBRG => Self::RGGB,
            Self::BGGR => Self::GRBG,
        }
    }

    pub fn cols_swapped(self) -> Self {
        match self {
            Self::GRBG => Self::RGGB,
            Self::RGGB => Self::GRBG,
            Self::GBRG => Self::BGGR,
            Self::BGGR => Self::GBRG,
        }
    }

    #[inline]
    pub fn color(self, x: usize, y: usize) -> Color {
        let red = match self {
            Self::GRBG => (1, 0),
            Self::RGGB => (0, 0),
            Self::GBRG => (0, 1),
            Self::BGGR => (1, 1),
        };
        if x % 2 == red.0 && y % 2 == red.1 {
            Color::Red
        } else if x % 2 != red.0 && y % 2 != red.1 {
            Color::Blue
        } else {
            Color::Green
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mosaic {
    Bayer,
    Mono,
    Rgb,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub enum LayoutChoice {
    #[default]
    Auto,
    Mono,
    Bayer(BayerPattern),
}

impl LayoutChoice {
    pub const ALL: [Self; 6] = [
        Self::Auto,
        Self::Mono,
        Self::Bayer(BayerPattern::RGGB),
        Self::Bayer(BayerPattern::GRBG),
        Self::Bayer(BayerPattern::GBRG),
        Self::Bayer(BayerPattern::BGGR),
    ];

    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "mono" => Ok(Self::Mono),
            other => BayerPattern::parse(other).map(Self::Bayer).map_err(|_| {
                Error::Invalid("Sensor layout must be auto, mono or a Bayer pattern.")
            }),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Mono => "Mono",
            Self::Bayer(pattern) => pattern.name(),
        }
    }
}

impl Serialize for LayoutChoice {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

pub struct RawFrame {
    pub header: fits::FitsHeader,
    pub pixels: Vec<f32>,
}

pub struct RgbImage {
    pub width: usize,
    pub height: usize,
    pub demosaic_method: DemosaicMethod,
    pub pixels: Vec<[f32; 3]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DemosaicMethod {
    Superpixel,
    Mhc,
    #[default]
    Vng,
    None,
    External,
}

impl DemosaicMethod {
    pub fn name(self) -> &'static str {
        match self {
            Self::Superpixel => "SUPERPIXEL",
            Self::Mhc => "MHC",
            Self::Vng => "VNG",
            Self::None => "NONE",
            Self::External => "EXTERNAL",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn frame_budget_is_half_of_free_ram_capped_at_8_gib() {
        assert_eq!(frame_budget_bytes(Some(14 * GIB)), 7 * GIB as usize);
        assert_eq!(frame_budget_bytes(Some(64 * GIB)), MAX_FRAME_BYTES);
        assert_eq!(frame_budget_bytes(None), FALLBACK_FRAME_BYTES);
        assert_eq!(frame_budget_bytes(Some(0)), FALLBACK_FRAME_BYTES);
    }

    #[test]
    fn drizzle_master_fits_with_enough_ram_and_gets_a_clear_message_otherwise() {
        // 12496 x 8352 x 3 f32 = FulgurStack 2x drizzle master (1.17 GiB).
        let needed = 12_496 * 8_352 * 3 * 4;
        assert!(check_frame_memory_with(needed, Some(14 * GIB)).is_ok());

        let err = check_frame_memory_with(needed, Some(2 * GIB))
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("Image needs 1.2 GB of memory, only 1.0 GB is available"),
            "{err}"
        );

        let err = check_frame_memory_with(9 * GIB as usize, Some(64 * GIB))
            .unwrap_err()
            .to_string();
        assert!(err.contains("more than the 8 GB limit"), "{err}");
    }
}
