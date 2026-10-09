//! Readers: the chunk-source interface and its Zarr and NetCDF implementations.
//!
//! A [`ChunkSource`] is opened lazily (metadata only) and describes its data as a
//! [`Dataset`]. Each variable is stored as a grid of chunks ([`ChunkGrid`]); the executor asks for
//! "chunk `indices` of variable `v`" and gets either
//! - [`RawChunk::Encoded`]: the stored bytes plus the decoder (Zarr), so decompression can run on
//!   the compute pool rather than on the I/O thread, or
//! - [`RawChunk::Decoded`]: values that are already decoded (the serial netCDF-C fallback).
//!
//! Decoded values are f32 or f64 ([`Values`]); missing values (`_FillValue`, `missing_value`)
//! are NaN and packed integers are unpacked (`scale_factor`, `add_offset`). Chunks at the array
//! edge are trimmed to the array extent.

pub mod kerchunk;
pub mod multifile;
pub mod netcdf4;
pub mod netcdf4_index;
pub mod netcdf_fallback;
pub mod normalize;
pub mod remote;
pub mod write_netcdf;
pub mod write_zarr;
pub mod zarr;

pub use multifile::open_many;

use crate::error::{Error, ErrorCode, Result};
use crate::model::{DType, Dataset, Encoding};
use std::path::Path;
use std::sync::Arc;

/// The process-wide HDF5 lock. The spack HDF5 (1.14.3) is not thread-safe, and netCDF-C calls
/// into it, so every call into the `netcdf` crate and into `hdf5-metno` runs under this lock.
/// It is the reentrant lock both crates take around each of their own calls
/// (`hdf5_metno_sys::LOCK`, re-exported by netcdf-sys as `libnetcdf_lock`); holding it across
/// a whole sequence (open, read metadata, close) keeps other threads out between the calls.
/// Being reentrant, nested use on one thread cannot deadlock. Rules that keep it deadlock-free
/// across threads: take it *before* any other lock held during netCDF/HDF5 calls (the file
/// mutexes of the netCDF reader and writer), and never wait for another thread while holding
/// it. Chunk reads through the NetCDF-4 index (pread + own decoding) do not take it.
#[must_use]
pub fn hdf5_lock() -> impl Sized {
    hdf5_metno_sys::LOCK.lock()
}

/// Decoded chunk values.
#[derive(Debug, Clone, PartialEq)]
pub enum Values {
    F32(Vec<f32>),
    F64(Vec<f64>),
}

impl Values {
    pub fn len(&self) -> usize {
        match self {
            Self::F32(v) => v.len(),
            Self::F64(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn to_f64(&self) -> Vec<f64> {
        match self {
            Self::F32(v) => v.iter().map(|&x| x as f64).collect(),
            Self::F64(v) => v.clone(),
        }
    }
}

/// A decoded chunk: its position in the variable and its values in C order.
#[derive(Debug, Clone)]
pub struct DecodedChunk {
    /// Index of the first element along each dimension.
    pub origin: Vec<u64>,
    /// Extent along each dimension (trimmed at the array edge).
    pub shape: Vec<usize>,
    pub values: Values,
}

/// Turns the stored bytes of one chunk into values. Implemented per variable by Zarr readers.
pub trait ChunkDecoder: Send + Sync {
    /// Decodes chunk `indices`; `bytes == None` means the chunk is not stored (fill value).
    fn decode(&self, indices: &[u64], bytes: Option<&[u8]>) -> Result<DecodedChunk>;
    /// Human-readable codec chain (for `--plan` and `sinfo`).
    fn codecs(&self) -> String;
}

/// The stored bytes of one chunk and the decoder for them.
pub struct EncodedChunk {
    pub indices: Vec<u64>,
    /// `None` if the chunk is absent from the store (it then holds the fill value).
    pub bytes: Option<Vec<u8>>,
    pub decoder: Arc<dyn ChunkDecoder>,
}

impl EncodedChunk {
    pub fn decode(&self) -> Result<DecodedChunk> {
        self.decoder.decode(&self.indices, self.bytes.as_deref())
    }

    /// Number of stored (compressed) bytes.
    pub fn encoded_len(&self) -> usize {
        self.bytes.as_ref().map_or(0, Vec::len)
    }
}

/// What [`ChunkSource::read_chunk`] returns.
pub enum RawChunk {
    Encoded(EncodedChunk),
    Decoded(DecodedChunk),
}

impl RawChunk {
    pub fn decode(self) -> Result<DecodedChunk> {
        match self {
            Self::Encoded(e) => e.decode(),
            Self::Decoded(d) => Ok(d),
        }
    }
}

/// The regular chunk grid of one variable.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkGrid {
    /// Array shape.
    pub shape: Vec<usize>,
    /// Shape of a full chunk.
    pub chunk_shape: Vec<usize>,
}

impl ChunkGrid {
    /// Number of chunks along each dimension.
    pub fn counts(&self) -> Vec<u64> {
        self.shape
            .iter()
            .zip(&self.chunk_shape)
            .map(|(&n, &c)| n.div_ceil(c.max(1)) as u64)
            .collect()
    }

    pub fn num_chunks(&self) -> u64 {
        self.counts().iter().product()
    }

    pub fn origin(&self, indices: &[u64]) -> Vec<u64> {
        indices
            .iter()
            .zip(&self.chunk_shape)
            .map(|(&i, &c)| i * c as u64)
            .collect()
    }

    /// Extent of chunk `indices`, trimmed at the array edge.
    pub fn extent(&self, indices: &[u64]) -> Vec<usize> {
        indices
            .iter()
            .zip(&self.chunk_shape)
            .zip(&self.shape)
            .map(|((&i, &c), &n)| c.min(n.saturating_sub(i as usize * c)))
            .collect()
    }

    /// Checks that `indices` address a chunk of this grid.
    pub fn check(&self, var: &str, indices: &[u64]) -> Result<()> {
        let counts = self.counts();
        if indices.len() != counts.len() || indices.iter().zip(&counts).any(|(i, n)| i >= n) {
            return Err(Error::bad_arguments(format!(
                "chunk {indices:?} out of range for variable '{var}' (chunk counts {counts:?})"
            )));
        }
        Ok(())
    }
}

/// A dataset opened for chunk-wise reading. Shared between threads.
pub trait ChunkSource: Send + Sync {
    /// Metadata of the dataset.
    fn dataset(&self) -> &Dataset;

    /// Chunk grid of a stored variable.
    fn chunk_grid(&self, var: &str) -> Result<ChunkGrid>;

    /// Reads chunk `indices` of variable `var`.
    fn read_chunk(&self, var: &str, indices: &[u64]) -> Result<RawChunk>;

    /// Reads a whole (small) variable as f64, unpacked, missing values as NaN. Used for
    /// coordinates and bounds.
    fn read_var(&self, var: &str) -> Result<Vec<f64>>;

    /// Codec chain of a variable (stored compression), if known.
    fn codecs(&self, _var: &str) -> Option<String> {
        None
    }

    /// Stored (compressed) size of one chunk when the source can tell without reading it
    /// (`--plan` samples a few chunks); `Some(0)` for a chunk absent from the store, `None` if
    /// unknown.
    fn stored_size(&self, _var: &str, _indices: &[u64]) -> Option<u64> {
        None
    }
}

/// Whether `path` is a Zarr store: a `.zarr` suffix, or a directory with `.zgroup`, `.zarray`,
/// `.zmetadata` or `zarr.json`.
pub fn is_zarr(path: &str) -> bool {
    let p = path.trim_end_matches('/');
    if p.ends_with(".zarr") || p.contains(".zarr/") || p.starts_with("zarr:") {
        return true;
    }
    let dir = Path::new(p);
    dir.is_dir()
        && [".zgroup", ".zarray", ".zmetadata", "zarr.json"]
            .iter()
            .any(|f| dir.join(f).exists())
}

/// Whether `path` starts with the HDF5 signature (NetCDF-4 files are HDF5 files).
fn is_hdf5(path: &str) -> bool {
    use std::os::unix::fs::FileExt;
    let mut head = [0u8; 8];
    std::fs::File::open(path)
        .and_then(|f| f.read_exact_at(&mut head, 0))
        .is_ok_and(|()| head == *b"\x89HDF\r\n\x1a\n")
}

/// Opens an input (lazily: metadata only):
/// - an `http(s)://` or `s3://` URL: a remote Zarr store, or kerchunk JSON references if the
///   URL ends in `.json` ([`remote::open`]);
/// - a glob pattern that is not an existing path: the matching files, sorted, concatenated
///   along time ([`multifile::MultiFileSource`]);
/// - kerchunk references (Parquet directory or JSON file, see [`kerchunk::is_kerchunk`]): a Zarr
///   source over [`kerchunk::KerchunkStore`];
/// - a Zarr store (see [`is_zarr`]);
/// - a NetCDF-4 (HDF5) file: metadata from netCDF-C, chunks read directly through the chunk
///   index ([`netcdf4::Nc4Source`]), unless `CDORS_NC4=netcdf`;
/// - anything else (NetCDF-3 classic / 64-bit offset / CDF5): netCDF-C.
pub fn open(path: &str) -> Result<Arc<dyn ChunkSource>> {
    if remote::is_url(path) {
        return Ok(Arc::new(remote::open(path)?));
    }
    if !Path::new(path).exists() {
        if multifile::is_glob(path) {
            return open_many(&multifile::expand_glob(path)?);
        }
        return Err(Error::new(
            ErrorCode::MissingInput,
            format!("input '{path}' does not exist"),
        )
        .with("path", path));
    }
    if kerchunk::is_kerchunk(Path::new(path)) {
        Ok(Arc::new(kerchunk::open_source(path)?))
    } else if is_zarr(path) {
        Ok(Arc::new(zarr::ZarrSource::open(path)?))
    } else if is_hdf5(path) && !netcdf4::netcdf_only() {
        Ok(Arc::new(netcdf4::Nc4Source::open(path)?))
    } else {
        Ok(Arc::new(netcdf_fallback::NetcdfSource::open(path)?))
    }
}

/// Element types that chunks can hold, converted to f64 for unpacking.
pub(crate) trait Elem: Copy + PartialEq + Send {
    const SIZE: usize;
    fn to_f64(self) -> f64;
    /// The value of signed integer storage read as unsigned (`_Unsigned = "true"`).
    fn to_f64_unsigned(self) -> f64 {
        self.to_f64()
    }
    fn from_f64(v: f64) -> Self;
    fn from_ne(b: &[u8]) -> Self;
}

macro_rules! impl_elem {
    ($($t:ty),*) => {$(
        impl Elem for $t {
            const SIZE: usize = std::mem::size_of::<$t>();
            fn to_f64(self) -> f64 { self as f64 }
            fn from_f64(v: f64) -> Self { v as $t }
            fn from_ne(b: &[u8]) -> Self {
                <$t>::from_ne_bytes(b.try_into().expect("element size"))
            }
        }
    )*};
}
impl_elem!(u8, u16, u32, u64, f32, f64);

macro_rules! impl_elem_signed {
    ($($t:ty => $u:ty),*) => {$(
        impl Elem for $t {
            const SIZE: usize = std::mem::size_of::<$t>();
            fn to_f64(self) -> f64 { self as f64 }
            fn to_f64_unsigned(self) -> f64 { self as $u as f64 }
            fn from_f64(v: f64) -> Self { v as $t }
            fn from_ne(b: &[u8]) -> Self {
                <$t>::from_ne_bytes(b.try_into().expect("element size"))
            }
        }
    )*};
}
impl_elem_signed!(i8 => u8, i16 => u16, i32 => u32, i64 => u64);

/// Converts stored values to [`Values`]: masks missing values (compared in the stored type)
/// and values outside the valid range, reads `_Unsigned` storage as unsigned, unpacks with
/// scale/offset, and returns f32 or f64 as the encoding says.
pub(crate) fn convert<T: Elem>(raw: &[T], enc: &Encoding) -> Values {
    // a fill value given as unsigned (255 for bytes) is stored as its signed bit pattern
    let miss: Vec<T> = enc
        .missing
        .iter()
        .map(|&m| {
            let span = unsigned_span::<T>();
            if enc.unsigned && span > 0.0 && m >= span / 2.0 && m < span {
                T::from_f64(m - span)
            } else {
                T::from_f64(m)
            }
        })
        .collect();
    let scale = enc.scale_factor.unwrap_or(1.0);
    let offset = enc.add_offset.unwrap_or(0.0);
    let packed = enc.is_packed();
    let f = |x: T| -> f64 {
        if miss.contains(&x) {
            return f64::NAN;
        }
        let v = if enc.unsigned {
            x.to_f64_unsigned()
        } else {
            x.to_f64()
        };
        if enc.out_of_range(v) {
            f64::NAN
        } else if packed {
            v * scale + offset
        } else {
            v
        }
    };
    if enc.unpacked_f32 {
        Values::F32(raw.iter().map(|&x| f(x) as f32).collect())
    } else {
        Values::F64(raw.iter().map(|&x| f(x)).collect())
    }
}

/// 2^bits of an integer element (0 for floats): an unsigned fill value `m` above the signed
/// range is stored as `m - 2^bits`.
fn unsigned_span<T: Elem>() -> f64 {
    if T::from_f64(0.5).to_f64() == 0.5 {
        0.0
    } else {
        2f64.powi(8 * T::SIZE as i32)
    }
}

fn from_bytes<T: Elem>(bytes: &[u8]) -> Vec<T> {
    bytes.chunks_exact(T::SIZE).map(T::from_ne).collect()
}

/// [`convert`] for native-endian bytes of type `dtype`.
pub(crate) fn convert_bytes(dtype: DType, bytes: &[u8], enc: &Encoding) -> Result<Values> {
    Ok(match dtype {
        DType::I8 => convert(&from_bytes::<i8>(bytes), enc),
        DType::I16 => convert(&from_bytes::<i16>(bytes), enc),
        DType::I32 => convert(&from_bytes::<i32>(bytes), enc),
        DType::I64 => convert(&from_bytes::<i64>(bytes), enc),
        DType::U8 => convert(&from_bytes::<u8>(bytes), enc),
        DType::U16 => convert(&from_bytes::<u16>(bytes), enc),
        DType::U32 => convert(&from_bytes::<u32>(bytes), enc),
        DType::U64 => convert(&from_bytes::<u64>(bytes), enc),
        DType::F32 => convert(&from_bytes::<f32>(bytes), enc),
        DType::F64 => convert(&from_bytes::<f64>(bytes), enc),
        DType::Other => return Err(Error::bad_data("non-numeric data type")),
    })
}

/// Builds the missing-value and packing description from CF attributes.
pub(crate) fn encoding_from_attrs(
    attrs: &crate::model::Attrs,
    dtype: DType,
    extra_missing: Option<f64>,
) -> Encoding {
    use crate::model::AttrValue;
    let mut missing = Vec::new();
    for k in ["_FillValue", "missing_value"] {
        if let Some(v) = attrs.get(k) {
            missing.extend(v.as_f64s().into_iter().filter(|x| !x.is_nan()));
        }
    }
    if attrs.get("_FillValue").is_none()
        && let Some(m) = extra_missing.filter(|x| !x.is_nan())
    {
        missing.push(m);
    }
    let scale_factor = attrs.get_f64("scale_factor");
    let add_offset = attrs.get_f64("add_offset");
    let packed = scale_factor.is_some() || add_offset.is_some();
    let unpacked_f32 = if packed {
        matches!(
            attrs.get("scale_factor").or(attrs.get("add_offset")),
            Some(AttrValue::F32s(_))
        )
    } else {
        dtype == DType::F32
    };
    let signed_int = matches!(dtype, DType::I8 | DType::I16 | DType::I32 | DType::I64);
    let float = |v: &AttrValue| matches!(v, AttrValue::F32s(_) | AttrValue::F64s(_));
    let float_var = matches!(dtype, DType::F32 | DType::F64);
    // cdo (CDI `scan_valid_range_attr`): an attribute whose type is not of the variable's kind
    // (integer/float) is ignored; `valid_range` wins over `valid_min`/`valid_max`
    let range_attr = |k: &str, n: usize| -> Option<Vec<f64>> {
        attrs
            .get(k)
            .filter(|v| float(v) == float_var)
            .map(AttrValue::as_f64s)
            .filter(|v| v.len() == n)
    };
    let (mut valid_min, mut valid_max) = match range_attr("valid_range", 2) {
        Some(r) if r[0] <= r[1] => (Some(r[0]), Some(r[1])),
        _ => (
            range_attr("valid_min", 1).map(|v| v[0]),
            range_attr("valid_max", 1).map(|v| v[0]),
        ),
    };
    let unsigned = signed_int
        && (attrs
            .get_str("_Unsigned")
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("true"))
            // CDI: a byte variable with valid_range 0..255 is unsigned
            || (dtype == DType::I8 && valid_min == Some(0.0) && valid_max == Some(255.0)));
    // cdo applies the valid range only to variables with a missing value
    if missing.is_empty() {
        (valid_min, valid_max) = (None, None);
    }
    Encoding {
        missing,
        scale_factor,
        add_offset,
        unpacked_f32,
        unsigned,
        valid_min,
        valid_max,
    }
}
