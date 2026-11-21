//! XISF 1.0 reader (specification revisions 1.00 to 1.02): monolithic and
//! distributed units, every data block location, codec, checksum and pixel
//! layout. Supports multi-image XISF units and exposes all FITSKeywords and
//! XISF Properties for the Metadata Inspector.

use crate::fits::ROI_READ_BUFFER_BYTES;
use crate::{BayerPattern, Error, HeaderCard, Result, check_cancelled};
use base64::Engine;
use roxmltree::{Document, Node};
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const SIGNATURE: &[u8; 8] = b"XISF0100";
const BLOCKS_SIGNATURE: &[u8; 8] = b"XISB0100";
const MAX_HEADER_BYTES: usize = 256 * 1024 * 1024;
const MAX_PROPERTY_BYTES: usize = 1024 * 1024;
const MAX_INDEX_NODES: usize = 1 << 20;

const STORAGE_KEYWORDS: [&str; 11] = [
    "SIMPLE", "BITPIX", "NAXIS", "NAXIS1", "NAXIS2", "NAXIS3", "EXTEND", "BZERO", "BSCALE",
    "BLANK", "ROWORDER",
];

const CFA_KEYWORDS: [&str; 5] = ["BAYERPAT", "BAYERPATN", "COLORTYP", "XBAYROFF", "YBAYROFF"];

const PROPERTY_KEYWORDS: [(&str, &str, bool); 11] = [
    ("Instrument:ExposureTime", "EXPTIME", false),
    ("Instrument:FrameExposureTime", "EXPTIME", false),
    ("Observation:Time:Start", "DATE-OBS", true),
    ("Instrument:Filter:Name", "FILTER", true),
    ("Instrument:Camera:Name", "INSTRUME", true),
    ("Instrument:Telescope:Name", "TELESCOP", true),
    ("Observation:Object:Name", "OBJECT", true),
    ("Instrument:Sensor:Temperature", "CCD-TEMP", false),
    ("Observation:Location:Latitude", "SITELAT", false),
    ("Observation:Location:Longitude", "SITELONG", false),
    ("XISF:CreatorApplication", "CREATOR", true),
];

pub fn is_xisf(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("xisf") || e.eq_ignore_ascii_case("xish"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sample {
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
}

impl Sample {
    fn parse(text: &str) -> Result<Self> {
        match text {
            "UInt8" => Ok(Self::U8),
            "UInt16" => Ok(Self::U16),
            "UInt32" => Ok(Self::U32),
            "UInt64" => Ok(Self::U64),
            "Float32" => Ok(Self::F32),
            "Float64" => Ok(Self::F64),
            "Complex32" | "Complex64" => Err(Error::Invalid(
                "Complex XISF images have no intensity plane to display.",
            )),
            _ => Err(Error::Invalid("Unknown XISF sample format.")),
        }
    }
    fn bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 | Self::F32 => 4,
            Self::U64 | Self::F64 => 8,
        }
    }
    fn bitpix(self) -> i32 {
        match self {
            Self::U8 => 8,
            Self::U16 => 16,
            Self::U32 => 32,
            Self::U64 => 64,
            Self::F32 => -32,
            Self::F64 => -64,
        }
    }
    fn upper(self) -> f64 {
        match self {
            Self::U8 => 255.,
            Self::U16 => 65535.,
            Self::U32 => 4_294_967_295.,
            Self::U64 => 18_446_744_073_709_551_615.,
            Self::F32 | Self::F64 => 1.,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Codec {
    Zlib,
    Lz4,
    Zstd,
}

#[derive(Clone, Debug)]
struct Compression {
    codec: Codec,
    uncompressed: usize,
    item_size: usize,
    subblocks: Vec<(usize, usize)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Algorithm {
    Sha1,
    Sha256,
    Sha512,
    Sha3_256,
    Sha3_512,
}

#[derive(Clone, Debug)]
struct Checksum {
    algorithm: Algorithm,
    digest: String,
}

#[derive(Clone, Debug)]
enum Stored {
    File {
        path: PathBuf,
        position: u64,
        size: u64,
    },
    Memory(Arc<Vec<u8>>),
}

#[derive(Clone, Debug)]
struct Block {
    stored: Stored,
    compression: Option<Compression>,
    checksum: Option<Checksum>,
}

fn sha<D: sha2::Digest>(bytes: &[u8]) -> Vec<u8> {
    D::digest(bytes).to_vec()
}

impl Checksum {
    fn parse(text: &str) -> Result<Self> {
        let (name, digest) = text
            .split_once(':')
            .ok_or(Error::Invalid("Invalid XISF checksum."))?;
        let algorithm = match name.to_ascii_lowercase().as_str() {
            "sha-1" | "sha1" => Algorithm::Sha1,
            "sha-256" | "sha256" => Algorithm::Sha256,
            "sha-512" | "sha512" => Algorithm::Sha512,
            "sha3-256" => Algorithm::Sha3_256,
            "sha3-512" => Algorithm::Sha3_512,
            _ => return Err(Error::Invalid("Unknown XISF checksum algorithm.")),
        };
        Ok(Self {
            algorithm,
            digest: digest.trim().to_ascii_lowercase(),
        })
    }

    fn verify(&self, bytes: &[u8]) -> Result<()> {
        let digest = match self.algorithm {
            Algorithm::Sha1 => <sha1::Sha1 as sha1::Digest>::digest(bytes).to_vec(),
            Algorithm::Sha256 => sha::<sha2::Sha256>(bytes),
            Algorithm::Sha512 => sha::<sha2::Sha512>(bytes),
            Algorithm::Sha3_256 => <sha3::Sha3_256 as sha3::Digest>::digest(bytes).to_vec(),
            Algorithm::Sha3_512 => <sha3::Sha3_512 as sha3::Digest>::digest(bytes).to_vec(),
        };
        let text: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        if text == self.digest {
            Ok(())
        } else {
            Err(Error::Invalid("XISF data block fails its checksum."))
        }
    }
}

impl Compression {
    fn parse(text: &str, subblocks: Option<&str>) -> Result<Self> {
        let invalid = Error::Invalid("Invalid XISF compression attribute.");
        let mut parts = text.split(':');
        let name = parts.next().unwrap_or("");
        let (codec, shuffled) = match name.to_ascii_lowercase().as_str() {
            "zlib" => (Codec::Zlib, false),
            "zlib+sh" => (Codec::Zlib, true),
            "lz4" | "lz4hc" => (Codec::Lz4, false),
            "lz4+sh" | "lz4hc+sh" => (Codec::Lz4, true),
            "zstd" => (Codec::Zstd, false),
            "zstd+sh" => (Codec::Zstd, true),
            _ => return Err(Error::Invalid("Unknown XISF compression codec.")),
        };
        let uncompressed = parts
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or(Error::Invalid("Invalid XISF compression attribute."))?;
        let item_size = match (shuffled, parts.next()) {
            (true, Some(size)) => size.parse().ok().filter(|s| *s > 0).ok_or(invalid)?,
            (true, None) => return Err(invalid),
            (false, _) => 1,
        };
        let subblocks = subblocks
            .into_iter()
            .flat_map(|list| list.split(':'))
            .filter(|pair| !pair.trim().is_empty())
            .map(|pair| {
                let (stored, plain) = pair
                    .split_once(',')
                    .ok_or(Error::Invalid("Invalid XISF subblocks attribute."))?;
                match (stored.trim().parse(), plain.trim().parse()) {
                    (Ok(stored), Ok(plain)) => Ok((stored, plain)),
                    _ => Err(Error::Invalid("Invalid XISF subblocks attribute.")),
                }
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            codec,
            uncompressed,
            item_size,
            subblocks,
        })
    }

    fn inflate(&self, stored: &[u8], plain: usize, out: &mut Vec<u8>) -> Result<()> {
        let invalid = Error::Invalid("Corrupt compressed XISF data block.");
        let start = out.len();
        match self.codec {
            Codec::Zlib => {
                let data = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(stored, plain)
                    .map_err(|_| Error::Invalid("Corrupt compressed XISF data block."))?;
                out.extend_from_slice(&data);
            }
            Codec::Lz4 => {
                out.resize(start + plain, 0);
                let written = lz4_flex::block::decompress_into(stored, &mut out[start..])
                    .map_err(|_| Error::Invalid("Corrupt compressed XISF data block."))?;
                out.truncate(start + written);
            }
            Codec::Zstd => {
                let mut source = stored;
                let decoder = ruzstd::decoding::StreamingDecoder::new(&mut source)
                    .map_err(|_| Error::Invalid("Corrupt compressed XISF data block."))?;
                decoder
                    .take(plain as u64 + 1)
                    .read_to_end(out)
                    .map_err(|_| Error::Invalid("Corrupt compressed XISF data block."))?;
            }
        }
        if out.len() - start == plain {
            Ok(())
        } else {
            Err(invalid)
        }
    }

    fn decompress(&self, stored: &[u8]) -> Result<Vec<u8>> {
        let whole = [(stored.len(), self.uncompressed)];
        let subblocks = if self.subblocks.is_empty() {
            &whole[..]
        } else {
            &self.subblocks[..]
        };
        let total = |pick: fn(&(usize, usize)) -> usize| {
            subblocks
                .iter()
                .map(pick)
                .try_fold(0usize, |sum, size| sum.checked_add(size))
        };
        if total(|s| s.1) != Some(self.uncompressed)
            || total(|s| s.0).is_none_or(|t| t > stored.len())
        {
            return Err(Error::Invalid(
                "XISF subblock sizes do not match the block.",
            ));
        }
        let mut out = Vec::with_capacity(self.uncompressed);
        let mut position = 0;
        for &(size, plain) in subblocks {
            let part = &stored[position..position + size];
            if size == plain {
                out.extend_from_slice(part);
            } else {
                self.inflate(part, plain, &mut out)?;
            }
            position += size;
        }
        Ok(unshuffle(&out, self.item_size))
    }
}

fn unshuffle(input: &[u8], item_size: usize) -> Vec<u8> {
    if item_size <= 1 {
        return input.to_vec();
    }
    let items = input.len() / item_size;
    let mut output = vec![0; input.len()];
    for byte in 0..item_size {
        let column = &input[byte * items..(byte + 1) * items];
        for (item, &value) in column.iter().enumerate() {
            output[item * item_size + byte] = value;
        }
    }
    let used = items * item_size;
    output[used..].copy_from_slice(&input[used..]);
    output
}

fn decode_text(text: &str, encoding: &str) -> Result<Vec<u8>> {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    match encoding {
        "base64" => base64::engine::general_purpose::STANDARD
            .decode(compact)
            .map_err(|_| Error::Invalid("Invalid Base64 XISF data block.")),
        "hex" => {
            let digit = |c: u8| (c as char).to_digit(16).map(|v| v as u8);
            let invalid = Error::Invalid("Invalid hexadecimal XISF data block.");
            if compact.len() % 2 != 0 {
                return Err(invalid);
            }
            compact
                .as_bytes()
                .chunks_exact(2)
                .map(|p| Some(digit(p[0])? * 16 + digit(p[1])?))
                .collect::<Option<Vec<u8>>>()
                .ok_or(invalid)
        }
        _ => Err(Error::Invalid("Unknown XISF block encoding.")),
    }
}

fn parse_integer(text: &str) -> Option<u64> {
    let text = text.trim();
    let radix = |prefix: &str, radix| {
        text.strip_prefix(prefix)
            .and_then(|v| u64::from_str_radix(v, radix).ok())
    };
    if text.len() > 2 && text.starts_with('0') {
        match text.as_bytes()[1] {
            b'x' | b'X' => return radix(&text[..2], 16),
            b'o' | b'O' => return radix(&text[..2], 8),
            b'b' | b'B' => return radix(&text[..2], 2),
            _ => {}
        }
    }
    text.parse().ok()
}

fn index_block(path: &Path, id: u64) -> Result<(u64, u64, u64)> {
    let mut file = BufReader::new(File::open(path)?);
    let mut head = [0; 16];
    file.read_exact(&mut head)?;
    if &head[..8] != BLOCKS_SIGNATURE {
        return Err(Error::Invalid("Not an XISF data blocks file."));
    }
    let word = |bytes: &[u8]| u64::from_le_bytes(bytes.try_into().unwrap());
    let mut node = 16;
    for _ in 0..MAX_INDEX_NODES {
        file.seek(SeekFrom::Start(node))?;
        file.read_exact(&mut head)?;
        let length = u32::from_le_bytes(head[..4].try_into().unwrap());
        let next = word(&head[8..16]);
        for _ in 0..length {
            let mut element = [0; 40];
            file.read_exact(&mut element)?;
            if word(&element[..8]) == id && word(&element[8..16]) != 0 {
                return Ok((
                    word(&element[8..16]),
                    word(&element[16..24]),
                    word(&element[24..32]),
                ));
            }
        }
        if next == 0 {
            break;
        }
        node = next;
    }
    Err(Error::Invalid("XISF data block is not in its index."))
}

struct Unit<'a> {
    path: &'a Path,
    directory: PathBuf,
    monolithic: bool,
    length: u64,
}

impl Unit<'_> {
    fn local_path(&self, text: &str) -> PathBuf {
        match text.strip_prefix("@header_dir/") {
            Some(relative) => self.directory.join(relative),
            None => PathBuf::from(windows_path(text)),
        }
    }

    fn external(&self, location: &str) -> Result<(PathBuf, Option<u64>)> {
        let (kind, rest) = location
            .split_once('(')
            .ok_or(Error::Invalid("Invalid XISF block location."))?;
        let close = rest
            .rfind(')')
            .ok_or(Error::Invalid("Invalid XISF block location."))?;
        let index = match rest[close + 1..].strip_prefix(':') {
            Some(id) => Some(
                parse_integer(id).ok_or(Error::Invalid("Invalid XISF block index identifier."))?,
            ),
            None => None,
        };
        let target = &rest[..close];
        let path = match kind {
            "path" => self.local_path(target),
            _ => {
                let lower = target.to_ascii_lowercase();
                if !lower.starts_with("file:") {
                    return Err(Error::Invalid(
                        "Remote XISF data blocks are not read; copy them next to the header.",
                    ));
                }
                let local = target[5..].trim_start_matches('/');
                let local = if local.as_bytes().get(1) == Some(&b':') {
                    local.to_owned()
                } else {
                    format!("/{local}")
                };
                PathBuf::from(percent_decoded(&local)?)
            }
        };
        Ok((path, index))
    }

    fn block(&self, node: Node<'_, '_>) -> Result<Block> {
        let location = node
            .attribute("location")
            .ok_or(Error::Invalid("XISF object has no data block location."))?;
        let mut holder = node;
        let stored = if location == "embedded" {
            let data = node
                .children()
                .find(|c| c.has_tag_name_local("Data"))
                .ok_or(Error::Invalid("Embedded XISF data block has no Data."))?;
            holder = data;
            let encoding = data
                .attribute("encoding")
                .ok_or(Error::Invalid("Embedded XISF data block has no encoding."))?;
            Stored::Memory(Arc::new(decode_text(data.text().unwrap_or(""), encoding)?))
        } else if let Some(encoding) = location.strip_prefix("inline:") {
            Stored::Memory(Arc::new(decode_text(node.text().unwrap_or(""), encoding)?))
        } else if let Some(place) = location.strip_prefix("attachment:") {
            if !self.monolithic {
                return Err(Error::Invalid("Attached block in a distributed XISF unit."));
            }
            let (position, size) = place
                .split_once(':')
                .and_then(|(p, s)| Some((p.parse().ok()?, s.parse().ok()?)))
                .ok_or(Error::Invalid("Invalid XISF attachment location."))?;
            Stored::File {
                path: self.path.to_owned(),
                position,
                size,
            }
        } else {
            let (path, index) = self.external(location)?;
            let (position, size) = match index {
                Some(id) => {
                    let (position, size, _) = index_block(&path, id)?;
                    (position, size)
                }
                None => (0, std::fs::metadata(&path)?.len()),
            };
            Stored::File {
                path,
                position,
                size,
            }
        };
        let attribute = |name| holder.attribute(name).or_else(|| node.attribute(name));
        Ok(Block {
            stored,
            compression: attribute("compression")
                .map(|c| Compression::parse(c, attribute("subblocks")))
                .transpose()?,
            checksum: attribute("checksum").map(Checksum::parse).transpose()?,
        })
    }

    fn text_of(&self, node: Node<'_, '_>) -> Option<String> {
        if let Some(value) = node.attribute("value") {
            return Some(value.to_owned());
        }
        if node.attribute("location").is_none() {
            return node.text().map(|t| t.trim().to_owned());
        }
        let block = self.block(node).ok()?;
        let bytes = block.bytes(MAX_PROPERTY_BYTES).ok()?;
        String::from_utf8(bytes.to_vec()).ok()
    }
}

fn windows_path(text: &str) -> &str {
    match text.as_bytes() {
        [b'/', _, b':', ..] => &text[1..],
        _ => text,
    }
}

fn percent_decoded(text: &str) -> Result<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let digit = |c: Option<&u8>| c.and_then(|c| (*c as char).to_digit(16));
            let (Some(a), Some(b)) = (digit(bytes.get(i + 1)), digit(bytes.get(i + 2))) else {
                return Err(Error::Invalid("Invalid XISF file URL."));
            };
            out.push((a * 16 + b) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| Error::Invalid("Invalid XISF file URL."))
}

trait NodeExt {
    fn has_tag_name_local(&self, name: &str) -> bool;
}
impl NodeExt for Node<'_, '_> {
    fn has_tag_name_local(&self, name: &str) -> bool {
        self.is_element() && self.tag_name().name() == name
    }
}

impl Block {
    fn stored_size(&self) -> u64 {
        match &self.stored {
            Stored::File { size, .. } => *size,
            Stored::Memory(bytes) => bytes.len() as u64,
        }
    }

    fn read_stored(&self, limit: usize) -> Result<Arc<Vec<u8>>> {
        match &self.stored {
            Stored::Memory(bytes) => Ok(bytes.clone()),
            Stored::File {
                path,
                position,
                size,
            } => {
                if *size > limit as u64 {
                    return Err(Error::Invalid("XISF data block exceeds its size limit."));
                }
                let mut file = File::open(path)?;
                file.seek(SeekFrom::Start(*position))?;
                let mut bytes = vec![0; *size as usize];
                file.read_exact(&mut bytes)?;
                Ok(Arc::new(bytes))
            }
        }
    }

    fn bytes(&self, limit: usize) -> Result<Arc<Vec<u8>>> {
        let stored = self.read_stored(limit)?;
        if let Some(checksum) = &self.checksum {
            checksum.verify(&stored)?;
        }
        match &self.compression {
            None => Ok(stored),
            Some(compression) if compression.uncompressed <= limit => {
                compression.decompress(&stored).map(Arc::new)
            }
            Some(_) => Err(Error::Invalid("XISF data block exceeds its size limit.")),
        }
    }
}

struct Band {
    first: usize,
    rows: usize,
    planes: Vec<Vec<f32>>,
}

/// Pixel data of one XISF image, in the pipeline's ADU (0..65535).
pub struct XisfImage {
    block: Block,
    sample: Sample,
    big_endian: bool,
    planar: bool,
    pub width: usize,
    pub height: usize,
    stored_channels: usize,
    pub channels: usize,
    add: f64,
    scale: f64,
    guard: Option<(PathBuf, u64)>,
    verified: Mutex<bool>,
    band: Mutex<Option<Band>>,
    band_rows: AtomicUsize,
}

impl fmt::Debug for XisfImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XisfImage")
            .field("sample", &self.sample)
            .field("channels", &self.channels)
            .finish_non_exhaustive()
    }
}

static DECODE_LIMIT: AtomicUsize = AtomicUsize::new(4);
static DECODES: Mutex<usize> = Mutex::new(0);
static DECODE_DONE: Condvar = Condvar::new();

struct DecodePermit;

impl DecodePermit {
    fn acquire(cancel: &AtomicBool) -> Result<Self> {
        let mut running = DECODES
            .lock()
            .map_err(|_| Error::Invalid("XISF decoder lock failed."))?;
        while *running >= DECODE_LIMIT.load(Ordering::Relaxed) {
            check_cancelled(cancel)?;
            running = DECODE_DONE
                .wait_timeout(running, Duration::from_millis(100))
                .map_err(|_| Error::Invalid("XISF decoder lock failed."))?
                .0;
        }
        *running += 1;
        Ok(Self)
    }
}

impl Drop for DecodePermit {
    fn drop(&mut self) {
        if let Ok(mut running) = DECODES.lock() {
            *running -= 1;
        }
        DECODE_DONE.notify_one();
    }
}

impl XisfImage {
    pub fn bitpix(&self) -> i32 {
        self.sample.bitpix()
    }

    fn seekable(&self) -> bool {
        self.block.compression.is_none() && matches!(self.block.stored, Stored::File { .. })
    }

    fn check_guard(&self) -> Result<()> {
        if let Some((path, length)) = &self.guard
            && std::fs::metadata(path)?.len() != *length
        {
            return Err(Error::Invalid("Source XISF changed during processing."));
        }
        Ok(())
    }

    fn adu(&self, stored: f64) -> f32 {
        ((stored + self.add) * self.scale) as f32
    }

    fn convert(&self, bytes: &[u8], step: usize, out: &mut [f32]) {
        let big = self.big_endian;
        macro_rules! read {
            ($kind:ty, $size:expr) => {
                for (out, sample) in out.iter_mut().zip(bytes.chunks(step)) {
                    let raw: [u8; $size] = sample[..$size].try_into().unwrap();
                    let value = if big {
                        <$kind>::from_be_bytes(raw)
                    } else {
                        <$kind>::from_le_bytes(raw)
                    };
                    *out = self.adu(value as f64);
                }
            };
        }
        match self.sample {
            Sample::U8 => {
                for (out, sample) in out.iter_mut().zip(bytes.chunks(step)) {
                    *out = self.adu(f64::from(sample[0]));
                }
            }
            Sample::U16 => read!(u16, 2),
            Sample::U32 => read!(u32, 4),
            Sample::U64 => read!(u64, 8),
            Sample::F32 => read!(f32, 4),
            Sample::F64 => read!(f64, 8),
        }
    }

    fn layout(&self, channel: usize) -> (usize, usize, usize) {
        let bytes = self.sample.bytes();
        if self.planar {
            (
                self.width * bytes,
                bytes,
                channel * self.width * self.height * bytes,
            )
        } else {
            let pixel = self.stored_channels * bytes;
            (self.width * pixel, pixel, channel * bytes)
        }
    }

    fn checksum_once(&self) -> Result<()> {
        let Some(checksum) = &self.block.checksum else {
            return Ok(());
        };
        let mut verified = self
            .verified
            .lock()
            .map_err(|_| Error::Invalid("XISF lock failed."))?;
        if !*verified {
            checksum.verify(&self.block.read_stored(usize::MAX)?)?;
            *verified = true;
        }
        Ok(())
    }

    fn read_file(
        &self,
        channel: usize,
        [x, y, width, height]: [usize; 4],
        out: &mut [f32],
        cancel: &AtomicBool,
    ) -> Result<()> {
        let Stored::File { path, position, .. } = &self.block.stored else {
            return Err(Error::Invalid("XISF block is not in a file."));
        };
        self.checksum_once()?;
        self.check_guard()?;
        let (stride, step, start) = self.layout(channel);
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(
            position + (start + y * stride + x * step) as u64,
        ))?;
        let mut file = BufReader::with_capacity(ROI_READ_BUFFER_BYTES, file);
        let mut row = vec![0; (width - 1) * step + self.sample.bytes()];
        for line in 0..height {
            check_cancelled(cancel)?;
            file.read_exact(&mut row)?;
            self.convert(&row, step, &mut out[line * width..(line + 1) * width]);
            if line + 1 < height {
                file.seek_relative((stride - row.len()) as i64)?;
            }
        }
        Ok(())
    }

    fn read_band(
        &self,
        channel: usize,
        [x, y, width, height]: [usize; 4],
        out: &mut [f32],
        cancel: &AtomicBool,
    ) -> Result<()> {
        let mut band = self
            .band
            .lock()
            .map_err(|_| Error::Invalid("XISF lock failed."))?;
        let limit = self.band_rows.load(Ordering::Relaxed);
        let covered = band
            .as_ref()
            .is_some_and(|b| b.first <= y && y + height <= b.first + b.rows);
        if !covered {
            *band = None;
            let permit = DecodePermit::acquire(cancel)?;
            self.check_guard()?;
            let bytes = self.block.bytes(usize::MAX)?;
            let rows = if limit == 0 {
                self.height
            } else {
                limit.clamp(height, self.height)
            };
            let first = y
                .saturating_sub((rows - height) / 8)
                .min(self.height - rows);
            let mut planes = Vec::with_capacity(self.channels);
            for plane in 0..self.channels {
                check_cancelled(cancel)?;
                let (stride, step, start) = self.layout(plane);
                let mut values = vec![0.; self.width * rows];
                for line in 0..rows {
                    let at = start + (first + line) * stride;
                    self.convert(
                        &bytes[at..at + (self.width - 1) * step + self.sample.bytes()],
                        step,
                        &mut values[line * self.width..(line + 1) * self.width],
                    );
                }
                planes.push(values);
            }
            drop(permit);
            *band = Some(Band {
                first,
                rows,
                planes,
            });
        }
        let kept = band.as_ref().unwrap();
        for line in 0..height {
            let at = (y + line - kept.first) * self.width + x;
            out[line * width..(line + 1) * width]
                .copy_from_slice(&kept.planes[channel][at..at + width]);
        }
        if limit > 0 && limit < height {
            *band = None;
        }
        Ok(())
    }

    pub fn read_region(
        &self,
        channel: usize,
        bounds: [usize; 4],
        out: &mut [f32],
        cancel: &AtomicBool,
    ) -> Result<()> {
        let [x, y, width, height] = bounds;
        if channel >= self.channels
            || width == 0
            || height == 0
            || x + width > self.width
            || y + height > self.height
            || out.len() != width * height
        {
            return Err(Error::Invalid("Invalid XISF image region."));
        }
        if self.seekable() {
            self.read_file(channel, bounds, out, cancel)
        } else {
            self.read_band(channel, bounds, out, cancel)
        }
    }

    pub fn read_plane(&self, channel: usize, cancel: &AtomicBool) -> Result<Vec<f32>> {
        let mut pixels = vec![0.; self.width * self.height];
        self.read_region(
            channel,
            [0, 0, self.width, self.height],
            &mut pixels,
            cancel,
        )?;
        Ok(pixels)
    }
}

pub(crate) struct Opened {
    pub image: Arc<XisfImage>,
    pub cards: Vec<[u8; 80]>,
    pub extra_cards: Vec<HeaderCard>,
    pub hdu_names: Vec<String>,
}

fn header_text(path: &Path) -> Result<(String, bool, u64)> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut head = [0; 16];
    let monolithic = !path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("xish"));
    let bytes = if monolithic {
        file.read_exact(&mut head)?;
        if &head[..8] != SIGNATURE {
            return Err(Error::Invalid("Not a monolithic XISF file."));
        }
        let size = u32::from_le_bytes(head[8..12].try_into().unwrap()) as usize;
        if !(65..=MAX_HEADER_BYTES).contains(&size) || 16 + size as u64 > length {
            return Err(Error::Invalid("Invalid XISF header length."));
        }
        let mut bytes = vec![0; size];
        file.read_exact(&mut bytes)?;
        bytes
    } else {
        if length > MAX_HEADER_BYTES as u64 {
            return Err(Error::Invalid("Invalid XISF header length."));
        }
        let mut bytes = vec![];
        file.read_to_end(&mut bytes)?;
        bytes
    };
    let text =
        String::from_utf8(bytes).map_err(|_| Error::Invalid("XISF header is not UTF-8 text."))?;
    Ok((text, monolithic, length))
}

fn card(key: &str, value: &str) -> Option<[u8; 80]> {
    if key.is_empty() || key.len() > 8 || !key.bytes().all(|b| (33..=126).contains(&b)) {
        return None;
    }
    let text: String = format!("{key:<8}= {value}")
        .chars()
        .map(|c| if (' '..='~').contains(&c) { c } else { '?' })
        .collect();
    let mut card = [b' '; 80];
    let length = text.len().min(80);
    card[..length].copy_from_slice(&text.as_bytes()[..length]);
    if text.len() > 80 && value.starts_with('\'') {
        card[79] = b'\'';
    }
    Some(card)
}

fn resolved<'a, 'input>(
    document: &'a Document<'input>,
    node: Node<'a, 'input>,
) -> Node<'a, 'input> {
    if node.has_tag_name_local("Reference")
        && let Some(uid) = node.attribute("ref")
        && let Some(target) = document
            .descendants()
            .find(|n| n.attribute("uid") == Some(uid))
    {
        return target;
    }
    node
}

struct Parsed<'a, 'input> {
    node: Node<'a, 'input>,
    sample: Sample,
    width: usize,
    height: usize,
    stored_channels: usize,
    channels: usize,
}

fn describe<'a, 'input>(node: Node<'a, 'input>) -> Result<Parsed<'a, 'input>> {
    let sample = Sample::parse(
        node.attribute("sampleFormat")
            .ok_or(Error::Invalid("XISF image has no sample format."))?,
    )?;
    let geometry: Vec<usize> = node
        .attribute("geometry")
        .ok_or(Error::Invalid("XISF image has no geometry."))?
        .split(':')
        .map(|v| v.trim().parse().ok().filter(|n| *n > 0))
        .collect::<Option<_>>()
        .ok_or(Error::Invalid("Invalid XISF image geometry."))?;
    let [width, height, stored_channels] = match geometry[..] {
        [width, height, channels] => [width, height, channels],
        [width, height, 1, channels] => [width, height, channels],
        _ => {
            return Err(Error::Invalid(
                "Only two-dimensional XISF images are supported.",
            ));
        }
    };
    if width < 6 || height < 6 {
        return Err(Error::Invalid(
            "Image dimensions must be integers of at least 6 pixels.",
        ));
    }
    let channels = match node.attribute("colorSpace").unwrap_or("Gray") {
        "Gray" => 1,
        "RGB" if stored_channels >= 3 => 3,
        "RGB" => return Err(Error::Invalid("RGB XISF image with too few channels.")),
        _ => return Err(Error::Invalid("Only Gray and RGB XISF images are read.")),
    };
    Ok(Parsed {
        node,
        sample,
        width,
        height,
        stored_channels,
        channels,
    })
}

fn choose_index<'a, 'input>(
    root: Node<'a, 'input>,
    hdu_index: usize,
) -> Result<(Parsed<'a, 'input>, Vec<String>)> {
    let mut valid_nodes = Vec::new();
    let mut hdu_names = Vec::new();
    let mut failure = None;
    for (idx, node) in root
        .children()
        .filter(|n| n.has_tag_name_local("Image"))
        .enumerate()
    {
        match describe(node) {
            Ok(image) => {
                let id = node
                    .attribute("id")
                    .or_else(|| node.attribute("imageType"))
                    .unwrap_or("Image");
                hdu_names.push(format!(
                    "#{} {} ({}x{}, {:?})",
                    idx, id, image.width, image.height, image.sample
                ));
                valid_nodes.push(image);
            }
            Err(error) => {
                failure.get_or_insert(error);
            }
        }
    }
    if valid_nodes.is_empty() {
        return Err(failure.unwrap_or(Error::Invalid("XISF unit holds no image.")));
    }
    let selected = if hdu_index < valid_nodes.len() {
        valid_nodes.swap_remove(hdu_index)
    } else {
        valid_nodes.swap_remove(0)
    };
    Ok((selected, hdu_names))
}

fn cfa_pattern(node: Node<'_, '_>) -> Result<Option<BayerPattern>> {
    let Some(cfa) = node
        .children()
        .find(|c| c.has_tag_name_local("ColorFilterArray"))
    else {
        return Ok(None);
    };
    match (
        cfa.attribute("pattern"),
        cfa.attribute("width"),
        cfa.attribute("height"),
    ) {
        (Some(pattern), Some("2"), Some("2")) => BayerPattern::parse(pattern)
            .map(Some)
            .map_err(|_| Error::Invalid("Only RGB Bayer colour filter arrays are read.")),
        _ => Err(Error::Invalid(
            "Only 2x2 Bayer colour filter arrays are read.",
        )),
    }
}

pub(crate) fn open_index(path: &Path, hdu_index: usize) -> Result<Opened> {
    let (text, monolithic, length) = header_text(path)?;
    let end = text
        .rfind("</xisf>")
        .ok_or(Error::Invalid("XISF header has no root element."))?;
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let document = Document::parse_with_options(&text[..end + 7], options)
        .map_err(|_| Error::Invalid("XISF header is not valid XML."))?;
    let root = document.root_element();
    if root.tag_name().name() != "xisf" || root.attribute("version") != Some("1.0") {
        return Err(Error::Invalid("Unsupported XISF version."));
    }
    let unit = Unit {
        path,
        directory: path.parent().unwrap_or(Path::new(".")).to_owned(),
        monolithic,
        length,
    };
    let (parsed, hdu_names) = choose_index(root, hdu_index)?;
    let node = parsed.node;
    let block = unit.block(node)?;
    let image_bytes = parsed
        .width
        .checked_mul(parsed.height)
        .and_then(|n| n.checked_mul(parsed.stored_channels))
        .and_then(|n| n.checked_mul(parsed.sample.bytes()))
        // Structural sanity only; the RAM-aware frame check runs in `FitsHeader::from_cards`.
        .filter(|&b| b <= crate::MAX_FRAME_BYTES)
        .ok_or(Error::Invalid("XISF image geometry is too large."))?;
    let stored_size = block.stored_size();
    let declared = block
        .compression
        .as_ref()
        .map_or(stored_size, |c| c.uncompressed as u64);
    if declared != image_bytes as u64 {
        return Err(Error::Invalid(
            "XISF data block does not match the image geometry.",
        ));
    }
    let guard = match &block.stored {
        Stored::File {
            path: data,
            position,
            size,
        } => {
            let file_length = if data.as_path() == path {
                unit.length
            } else {
                std::fs::metadata(data)?.len()
            };
            if position + size > file_length {
                return Err(Error::Invalid("Truncated XISF data block."));
            }
            Some((data.clone(), file_length))
        }
        Stored::Memory(_) => None,
    };
    let float = matches!(parsed.sample, Sample::F32 | Sample::F64);
    let bounds = node.attribute("bounds").and_then(|text| {
        let (lower, upper) = text.split_once(':')?;
        Some((lower.trim().parse().ok()?, upper.trim().parse().ok()?))
    });
    let (lower, upper): (f64, f64) = bounds.unwrap_or((0., parsed.sample.upper()));
    if upper.partial_cmp(&lower) != Some(std::cmp::Ordering::Greater) {
        return Err(Error::Invalid("Invalid XISF image bounds."));
    }
    let offset: f64 = node
        .attribute("offset")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.);
    let scale = if !float && bounds.is_none() && parsed.sample == Sample::U16 {
        1.
    } else {
        65535. / (upper - lower)
    };
    let image = XisfImage {
        sample: parsed.sample,
        big_endian: node.attribute("byteOrder") == Some("big"),
        planar: node.attribute("pixelStorage").unwrap_or("Planar") != "Normal",
        width: parsed.width,
        height: parsed.height,
        stored_channels: parsed.stored_channels,
        channels: parsed.channels,
        add: -(offset + lower),
        scale,
        guard,
        verified: Mutex::new(false),
        band: Mutex::new(None),
        band_rows: AtomicUsize::new(0),
        block,
    };
    let (cards, extra_cards) = cards(&document, root, &unit, &parsed)?;
    Ok(Opened {
        image: Arc::new(image),
        cards,
        extra_cards,
        hdu_names,
    })
}

fn cards<'a, 'input>(
    document: &'a Document<'input>,
    root: Node<'a, 'input>,
    unit: &Unit<'_>,
    parsed: &Parsed<'a, 'input>,
) -> Result<(Vec<[u8; 80]>, Vec<HeaderCard>)> {
    let node = parsed.node;
    let rgb = parsed.channels == 3;
    let number = |value: usize| value.to_string();
    let mut cards: Vec<[u8; 80]> = [
        ("SIMPLE", "T".to_owned()),
        ("BITPIX", parsed.sample.bitpix().to_string()),
        ("NAXIS", if rgb { "3" } else { "2" }.to_owned()),
        ("NAXIS1", number(parsed.width)),
        ("NAXIS2", number(parsed.height)),
    ]
    .iter()
    .filter_map(|(key, value)| card(key, value))
    .collect();
    if rgb {
        cards.extend(card("NAXIS3", "3"));
    }
    let mut extra_cards = Vec::new();
    let push = |cards: &mut Vec<[u8; 80]>, key: &str, value: &str| {
        let padded = format!("{key:<8}");
        if !cards.iter().any(|c| c.starts_with(padded.as_bytes())) {
            cards.extend(card(key, value));
        }
    };
    let pattern = if rgb { None } else { cfa_pattern(node)? };
    if let Some(pattern) = pattern {
        push(&mut cards, "BAYERPAT", &format!("'{}'", pattern.name()));
    }
    for keyword in node
        .children()
        .map(|c| resolved(document, c))
        .filter(|c| c.has_tag_name_local("FITSKeyword"))
    {
        let (Some(name), Some(value)) = (keyword.attribute("name"), keyword.attribute("value"))
        else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        let comment = keyword.attribute("comment").unwrap_or("").trim();
        if STORAGE_KEYWORDS.contains(&name)
            || (rgb && CFA_KEYWORDS.contains(&name))
            || (pattern.is_some() && name == "BAYERPAT")
        {
            continue;
        }
        if ["HISTORY", "COMMENT"].contains(&name) {
            extra_cards.push(HeaderCard {
                keyword: name.to_owned(),
                value: value.to_owned(),
                comment: comment.to_owned(),
            });
            continue;
        }
        if comment.is_empty() || value.len() + comment.len() + 13 > 80 {
            push(&mut cards, name, value);
        } else {
            push(&mut cards, name, &format!("{value:<20} / {comment}"));
        }
    }
    if let Some(kind) = node.attribute("imageType") {
        push(&mut cards, "IMAGETYP", &format!("'{kind}'"));
    }
    let metadata = root.children().find(|c| c.has_tag_name_local("Metadata"));
    let properties = node
        .children()
        .map(|c| resolved(document, c))
        .chain(root.children())
        .chain(metadata.into_iter().flat_map(|m| m.children()))
        .filter(|c| c.has_tag_name_local("Property"));
    for property in properties {
        let Some(id) = property.attribute("id") else {
            continue;
        };
        if let Some(text) = unit.text_of(property).filter(|t| !t.is_empty()) {
            extra_cards.push(HeaderCard {
                keyword: id.to_owned(),
                value: text.clone(),
                comment: property
                    .attribute("comment")
                    .unwrap_or("XISF Property")
                    .to_owned(),
            });
            for (name, key, quoted) in PROPERTY_KEYWORDS {
                if id == name {
                    let clean = text.trim_end_matches('Z').replace('\'', "''");
                    let value = if quoted { format!("'{clean}'") } else { clean };
                    push(&mut cards, key, &value);
                }
            }
        }
    }
    Ok((cards, extra_cards))
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if c >= ' ' => out.push(c),
            _ => {}
        }
    }
    out
}

/// Atomically write solved WCS `<FITSKeyword>` elements into an existing XISF file
/// via `<filename>.tmp`, `sync_all()`, and `rename`, preserving all image blocks bit-for-bit.
pub fn save_wcs_to_xisf(
    path: &Path,
    hdu_index: usize,
    wcs_cards: &[(String, String, String)],
) -> Result<()> {
    use std::io::{BufWriter, Write};

    let (text, monolithic, file_len) = header_text(path)?;
    let end = text
        .rfind("</xisf>")
        .ok_or(Error::Invalid("XISF header has no root element."))?;
    let xml_slice = &text[..end + 7];
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let document = Document::parse_with_options(xml_slice, options)
        .map_err(|_| Error::Invalid("XISF header is not valid XML."))?;
    let root = document.root_element();

    let images: Vec<Node<'_, '_>> = root
        .children()
        .filter(|n| n.has_tag_name_local("Image") && describe(*n).is_ok())
        .collect();
    if images.is_empty() {
        return Err(Error::Invalid("XISF unit holds no image."));
    }
    // Same image enumeration as the reader (`choose_index`); an unknown index is an error
    // instead of silently writing the WCS into a different image.
    let Some(&image_node) = images.get(hdu_index) else {
        return Err(Error::Invalid(
            "Target XISF image not found; WCS was not saved.",
        ));
    };
    let original = open_index(path, hdu_index)?;

    // Collect byte ranges of existing WCS <FITSKeyword> children inside the selected <Image>,
    // plus PixInsight's own astrometric solution properties. PixInsight prefers
    // `PCL:AstrometricSolution:*` properties over FITS keywords, so leaving them in place would
    // make it keep using the old solution after we save a new one.
    let mut remove_ranges: Vec<(usize, usize)> = Vec::new();
    for child in image_node.children().filter(|c| c.is_element()) {
        let is_stale = if child.has_tag_name_local("FITSKeyword") {
            child.attribute("name").is_some_and(|name| {
                let trimmed = name.trim();
                crate::fits::is_wcs_keyword(trimmed)
                    || (trimmed.eq_ignore_ascii_case("HISTORY")
                        && child.attribute("comment").is_some_and(|c| {
                            c.contains("FulgurPreView WCS") || c.contains("FulgurStack WCS")
                        }))
            })
        } else if child.has_tag_name_local("Property") {
            child
                .attribute("id")
                .is_some_and(|id| id.trim().starts_with(PCL_ASTROMETRY_PREFIX))
        } else {
            false
        };
        {
            if is_stale {
                let mut r = child.range();
                while r.start > 0 && matches!(xml_slice.as_bytes()[r.start - 1], b' ' | b'\t') {
                    r.start -= 1;
                }
                if r.end < xml_slice.len() && xml_slice.as_bytes()[r.end] == b'\r' {
                    r.end += 1;
                }
                if r.end < xml_slice.len() && xml_slice.as_bytes()[r.end] == b'\n' {
                    r.end += 1;
                }
                remove_ranges.push((r.start, r.end));
            }
        }
    }
    remove_ranges.sort_by_key(|r| r.0);

    let img_range = image_node.range();
    let img_xml = &xml_slice[img_range.clone()];
    let insert_pos = if let Some(rel_close) = img_xml.rfind("</Image>") {
        img_range.start + rel_close
    } else {
        return Err(Error::Invalid(
            "Self-closing <Image/> without body cannot hold FITSKeyword children.",
        ));
    };

    let mut wcs_xml = String::new();
    for (k, v, c) in wcs_cards {
        wcs_xml.push_str(&format!(
            "    <FITSKeyword name=\"{}\" value=\"{}\" comment=\"{}\"/>\n",
            xml_escape(k),
            xml_escape(v),
            xml_escape(c)
        ));
    }

    // Rebuild XML with old WCS keywords removed and new ones inserted before </Image>
    let mut updated_xml = String::with_capacity(xml_slice.len() + wcs_xml.len() + 256);
    let mut cursor = 0usize;
    for (r_start, r_end) in remove_ranges {
        if r_start >= cursor && r_start <= insert_pos {
            updated_xml.push_str(&xml_slice[cursor..r_start]);
            cursor = r_end;
        }
    }
    if cursor <= insert_pos {
        updated_xml.push_str(&xml_slice[cursor..insert_pos]);
        cursor = insert_pos;
    }
    updated_xml.push_str(&wcs_xml);
    updated_xml.push_str(&xml_slice[cursor..]);

    // Check minimum attachment offset in monolithic XISF files
    let min_attach_pos = find_min_attachment_pos(&updated_xml);

    let (tmp_path, out_file) = crate::fits::create_sibling_tmp(path)?;
    // Byte ranges (original offset, rewritten offset) whose tails must stay identical.
    let mut copied_tail: Option<(u64, u64)> = None;

    let write_res = (|| -> Result<()> {
        let mut writer = BufWriter::with_capacity(256 * 1024, out_file);

        if !monolithic {
            writer.write_all(updated_xml.as_bytes())?;
        } else if let Some(old_data_start) = min_attach_pos {
            let needed = 16 + updated_xml.len() + 64;
            let (final_xml, new_data_start) = if needed <= old_data_start as usize {
                (updated_xml, old_data_start)
            } else {
                let aligned = (needed + 4096).div_ceil(4096) * 4096;
                let delta = (aligned as u64).saturating_sub(old_data_start);
                (
                    shift_attachment_offsets(&updated_xml, delta),
                    old_data_start + delta,
                )
            };
            let xml_bytes = final_xml.as_bytes();
            if 16 + xml_bytes.len() > new_data_start as usize {
                return Err(Error::Invalid("XISF header attachment alignment overflow."));
            }
            writer.write_all(SIGNATURE)?;
            writer.write_all(&(xml_bytes.len() as u32).to_le_bytes())?;
            writer.write_all(&0u32.to_le_bytes())?;
            writer.write_all(xml_bytes)?;

            let pad_len = (new_data_start as usize) - (16 + xml_bytes.len());
            if pad_len > 0 {
                writer.write_all(&vec![0u8; pad_len])?;
            }

            let mut src = File::open(path)?;
            src.seek(SeekFrom::Start(old_data_start))?;
            let mut src_reader = BufReader::with_capacity(256 * 1024, src);
            std::io::copy(&mut src_reader, &mut writer)?;
            copied_tail = Some((old_data_start, new_data_start));
        } else {
            // Monolithic XISF with only embedded/inline blocks (no attachment offsets)
            let xml_bytes = updated_xml.as_bytes();
            writer.write_all(SIGNATURE)?;
            writer.write_all(&(xml_bytes.len() as u32).to_le_bytes())?;
            writer.write_all(&0u32.to_le_bytes())?;
            writer.write_all(xml_bytes)?;
            let _ = file_len;
        }

        writer.flush()?;
        let inner = writer.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        inner.sync_all()?;
        drop(inner);

        // Verify before replacing the original: attachments (pixel data) byte-identical and
        // the selected image still parses with the same geometry and sample format.
        if let Some((old_start, new_start)) = copied_tail {
            crate::fits::verify_same_tail(path, old_start, &tmp_path, new_start)?;
        }
        let rewritten = open_index(&tmp_path, hdu_index)?;
        let same_image = rewritten.image.width == original.image.width
            && rewritten.image.height == original.image.height
            && rewritten.image.channels == original.image.channels
            && rewritten.image.bitpix() == original.image.bitpix();
        if !same_image || rewritten.hdu_names.len() != original.hdu_names.len() {
            return Err(Error::Invalid(
                "Verification of the rewritten XISF file failed; original left unchanged.",
            ));
        }
        Ok(())
    })();

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

/// Id prefix of PixInsight's astrometric solution properties inside an `<Image>`.
const PCL_ASTROMETRY_PREFIX: &str = "PCL:AstrometricSolution";

/// Find the next `location="attachment:` or `location='attachment:` (either quote style is
/// valid XML). Returns the byte index just past the needle.
fn next_attachment_location(xml: &str) -> Option<usize> {
    ["location=\"attachment:", "location='attachment:"]
        .iter()
        .filter_map(|needle| xml.find(needle).map(|idx| idx + needle.len()))
        .min()
}

fn find_min_attachment_pos(xml: &str) -> Option<u64> {
    let mut min_pos: Option<u64> = None;
    let mut rest = xml;
    while let Some(after) = next_attachment_location(rest) {
        rest = &rest[after..];
        if let Some((pos_str, _)) = rest.split_once(':')
            && let Ok(pos) = pos_str.trim().parse::<u64>()
        {
            min_pos = Some(min_pos.map_or(pos, |m| m.min(pos)));
        }
    }
    min_pos
}

fn shift_attachment_offsets(xml: &str, delta: u64) -> String {
    let mut out = String::with_capacity(xml.len() + 64);
    let mut rest = xml;
    while let Some(after) = next_attachment_location(rest) {
        out.push_str(&rest[..after]);
        rest = &rest[after..];
        if let Some(colon) = rest.find(':')
            && let Ok(pos) = rest[..colon].trim().parse::<u64>()
        {
            out.push_str(&(pos + delta).to_string());
            rest = &rest[colon..];
        }
    }
    out.push_str(rest);
    out
}
