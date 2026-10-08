//! NetCDF-4 chunk index: list chunk byte ranges once through HDF5, then read and decode chunks
//! without HDF5.
//!
//! [`Nc4Index::open`] lists, for every variable of a NetCDF-4 (HDF5) file, each stored chunk's
//! byte offset, stored size and filter mask, plus dtype, byte order, chunk shape, fill value and
//! the filter pipeline. Contiguous variables become one byte range split into virtual chunks
//! along the first dimension. The index is cached as JSON in `$CDORS_CACHE/nc4index/<hash>.json`,
//! keyed by canonical path, file size and modification time.
//!
//! [`Nc4File::read_chunk`] then preads the stored bytes and undoes the pipeline in Rust
//! (fletcher32: strip the checksum; deflate: inflate; shuffle: unshuffle; blosc (HDF5 filter
//! 32001): c-blosc decompress; big endian: swap), so
//! chunks can be decoded from many threads at once. Variables with any other filter, compact
//! storage or non-numeric types carry `unsupported = Some(reason)`; the caller falls back to
//! netCDF-C for those.
//!
//! [`Nc4Index::to_kerchunk`] expresses the same index as kerchunk references with synthesized
//! Zarr v2 metadata (filters `shuffle`, `zlib`, `fletcher32`), the way kerchunk serves NetCDF-4
//! files, so the file can also be read through `zarrs`.

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;
use zarrs::storage::Bytes;

use super::kerchunk::{KerchunkStore, Ref};

/// Bumped whenever the index layout or its derivation changes, so old cache entries are not used.
const INDEX_FORMAT: u32 = 1;
/// Target size of the virtual chunks a contiguous variable is split into.
const CONTIGUOUS_TARGET_BYTES: u64 = 16 << 20;

/// Errors from building, caching or reading a NetCDF-4 chunk index.
#[derive(Debug, thiserror::Error)]
pub enum Nc4Error {
    #[error("{path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("{path}: HDF5: {msg}")]
    Hdf5 { path: String, msg: String },
    #[error("{path}: variable {var}: {msg}")]
    Decode {
        path: String,
        var: String,
        msg: String,
    },
    #[error("variable {0} not in index")]
    NoVariable(String),
    #[error("variable {var} is not readable without netCDF-C: {reason}")]
    Unsupported { var: String, reason: String },
}

/// Element type of a variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Nc4Dtype {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
}

impl Nc4Dtype {
    /// Element size in bytes.
    pub fn size(self) -> usize {
        match self {
            Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::I64 | Self::U64 | Self::F64 => 8,
        }
    }

    /// Zarr v2 / numpy type string without byte-order character, e.g. `f4`.
    fn numpy(self) -> &'static str {
        match self {
            Self::I8 => "i1",
            Self::U8 => "u1",
            Self::I16 => "i2",
            Self::U16 => "u2",
            Self::I32 => "i4",
            Self::U32 => "u4",
            Self::I64 => "i8",
            Self::U64 => "u8",
            Self::F32 => "f4",
            Self::F64 => "f8",
        }
    }
}

/// One HDF5 filter, in pipeline (write) order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "id")]
pub enum Nc4Filter {
    Shuffle,
    Deflate {
        level: u8,
    },
    Fletcher32,
    /// HDF5 filter 32001: each chunk is one c-blosc frame (shuffle happens inside blosc).
    /// `compressor` is the blosc compressor code (0 blosclz, 1 lz4, 2 lz4hc, 3 snappy, 4 zlib,
    /// 5 zstd), `shuffle` 0 none, 1 byte, 2 bit; both only describe the data, decoding reads the
    /// frame header.
    Blosc {
        clevel: u8,
        shuffle: u8,
        compressor: u8,
    },
}

/// HDF5 registered filter id of the blosc filter.
const H5Z_FILTER_BLOSC: i32 = 32001;

/// Storage layout of a variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Nc4Layout {
    Chunked,
    /// One unfiltered byte range, presented as virtual chunks along the first dimension.
    Contiguous,
    /// Compact or other layouts; never read directly.
    Other,
}

/// One stored chunk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nc4Chunk {
    /// Chunk grid coordinates (element offset / chunk shape).
    pub coords: Vec<u64>,
    /// Byte offset in the file.
    pub offset: u64,
    /// Stored (filtered) size in bytes.
    pub size: u64,
    /// HDF5 filter mask: bit `i` set means filter `i` of the pipeline was skipped for this chunk.
    pub filter_mask: u32,
}

/// Index of one variable.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Nc4Variable {
    /// netCDF name; variables in groups are `group/name`.
    pub name: String,
    pub shape: Vec<u64>,
    pub chunk_shape: Vec<u64>,
    pub dtype: Nc4Dtype,
    pub big_endian: bool,
    pub layout: Nc4Layout,
    /// Filter pipeline in write order.
    pub filters: Vec<Nc4Filter>,
    /// Fill value as little-endian bytes of one element (`_FillValue`, else the HDF5 fill value).
    pub fill_value: Option<Vec<u8>>,
    /// Why this variable cannot be read without netCDF-C (`None`: it can).
    pub unsupported: Option<String>,
    /// Stored chunks, sorted by `coords` (C order). Chunks never written are absent.
    pub chunks: Vec<Nc4Chunk>,
}

/// A decoded chunk: raw elements in native byte order, C order, full chunk shape
/// (edge chunks are padded as stored, missing chunks are filled with the fill value).
#[derive(Clone, Debug)]
pub struct DecodedChunk {
    pub data: Vec<u8>,
    /// Chunk shape (elements per dimension).
    pub shape: Vec<u64>,
    /// Element offset of the chunk's first element in the variable.
    pub origin: Vec<u64>,
}

impl Nc4Variable {
    /// Number of chunks along each dimension.
    pub fn chunk_grid(&self) -> Vec<u64> {
        self.shape
            .iter()
            .zip(&self.chunk_shape)
            .map(|(s, c)| s.div_ceil(*c))
            .collect()
    }

    /// Stored chunk at grid coordinates, `None` if never written.
    pub fn chunk(&self, coords: &[u64]) -> Option<&Nc4Chunk> {
        self.chunks
            .binary_search_by(|c| c.coords.as_slice().cmp(coords))
            .ok()
            .map(|i| &self.chunks[i])
    }

    /// Bytes of one decoded chunk.
    pub fn chunk_bytes(&self) -> usize {
        self.chunk_shape.iter().product::<u64>() as usize * self.dtype.size()
    }

    /// True if every stored chunk applied the whole pipeline, so the variable can be described by
    /// Zarr v2 metadata (and read through `zarrs`).
    pub fn zarr_compatible(&self) -> bool {
        let full = self.chunk_bytes() as u64;
        self.unsupported.is_none()
            && self.layout != Nc4Layout::Other
            && self.chunks.iter().all(|c| c.filter_mask == 0)
            && (self.layout != Nc4Layout::Contiguous || self.chunks.iter().all(|c| c.size == full))
    }

    /// Decode stored chunk bytes (`raw`, as read from the file) into native-order elements.
    pub fn decode(&self, raw: Vec<u8>, filter_mask: u32) -> Result<Vec<u8>, String> {
        let esize = self.dtype.size();
        let expect = self.chunk_bytes();
        let mut data = raw;
        if self.layout == Nc4Layout::Contiguous && data.len() < expect {
            // Short last virtual chunk of a contiguous variable: pad with the fill value
            // (stored order, swapped below together with the data).
            let mut one = self.fill_value.clone().unwrap_or_else(|| vec![0; esize]);
            if self.big_endian {
                one.reverse();
            }
            while data.len() < expect {
                data.extend_from_slice(&one);
            }
        }
        for (pos, filter) in self.filters.iter().enumerate().rev() {
            if filter_mask & (1 << pos) != 0 {
                continue;
            }
            data = match filter {
                Nc4Filter::Fletcher32 => {
                    let n = data
                        .len()
                        .checked_sub(4)
                        .ok_or("fletcher32: chunk shorter than its checksum")?;
                    data.truncate(n);
                    data
                }
                Nc4Filter::Deflate { .. } => inflate(&data, expect)?,
                Nc4Filter::Shuffle => unshuffle(&data, esize),
                Nc4Filter::Blosc { .. } => blosc_decompress(&data)?,
            };
        }
        if data.len() != expect {
            return Err(format!(
                "decoded {} bytes, expected {expect} (chunk shape {:?})",
                data.len(),
                self.chunk_shape
            ));
        }
        if self.big_endian != cfg!(target_endian = "big") && esize > 1 {
            for e in data.chunks_exact_mut(esize) {
                e.reverse();
            }
        }
        Ok(data)
    }

    /// One chunk filled with the fill value (zero if none), native order.
    pub fn fill_chunk(&self) -> Vec<u8> {
        let esize = self.dtype.size();
        let n = self.chunk_bytes() / esize;
        let mut one = self.fill_value.clone().unwrap_or_else(|| vec![0; esize]);
        if cfg!(target_endian = "big") {
            one.reverse();
        }
        one.repeat(n)
    }
}

/// Chunk index of all variables of one NetCDF-4 file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Nc4Index {
    pub format: u32,
    /// Canonical path of the file.
    pub path: PathBuf,
    pub file_size: u64,
    /// Modification time, nanoseconds since the Unix epoch.
    pub mtime_ns: i128,
    pub variables: Vec<Nc4Variable>,
}

impl Nc4Index {
    /// Load the cached index of `path`, or build it through HDF5 and cache it
    /// (in `$CDORS_CACHE/nc4index/`; without `CDORS_CACHE` nothing is cached).
    pub fn open(path: &Path) -> Result<Self, Nc4Error> {
        let (canon, size, mtime_ns) = file_identity(path)?;
        let cache = cache_path(&canon, size, mtime_ns);
        if let Some(cache) = &cache
            && let Ok(text) = std::fs::read(cache)
            && let Ok(index) = serde_json::from_slice::<Nc4Index>(&text)
            && index.format == INDEX_FORMAT
            && index.path == canon
            && index.file_size == size
            && index.mtime_ns == mtime_ns
        {
            return Ok(index);
        }
        let index = Self::build(path)?;
        if let Some(cache) = &cache {
            // A cache that cannot be written only costs a rebuild next time.
            let _ = write_atomic(cache, &serde_json::to_vec(&index).unwrap_or_default());
        }
        Ok(index)
    }

    /// Build the index through HDF5 (no cache), under [`super::hdf5_lock`] from opening the file
    /// to dropping the last HDF5 handle.
    pub fn build(path: &Path) -> Result<Self, Nc4Error> {
        let (canon, file_size, mtime_ns) = file_identity(path)?;
        let _hdf5 = super::hdf5_lock();
        let herr = |e: hdf5_metno::Error| Nc4Error::Hdf5 {
            path: canon.display().to_string(),
            msg: e.to_string(),
        };
        let file = hdf5_metno::File::open(&canon).map_err(herr)?;
        let mut datasets = Vec::new();
        collect_datasets(&file, &mut datasets).map_err(herr)?;
        let mut variables = Vec::with_capacity(datasets.len());
        for ds in datasets {
            if is_dimension_only(&ds) {
                continue;
            }
            variables.push(index_variable(&ds).map_err(herr)?);
        }
        variables.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            format: INDEX_FORMAT,
            path: canon,
            file_size,
            mtime_ns,
            variables,
        })
    }

    /// Variable by netCDF name.
    pub fn variable(&self, name: &str) -> Option<&Nc4Variable> {
        self.variables.iter().find(|v| v.name == name)
    }

    /// The index as kerchunk references with synthesized Zarr v2 metadata, served as a
    /// [`KerchunkStore`]. Only variables for which [`Nc4Variable::zarr_compatible`] holds are
    /// included; the others need [`Nc4File::read_chunk`] or netCDF-C.
    pub fn to_kerchunk(&self) -> KerchunkStore {
        let url: Arc<str> = Arc::from(self.path.to_string_lossy().as_ref());
        let mut refs = HashMap::new();
        let inline = |v: serde_json::Value| Ref::Inline(Bytes::from(v.to_string().into_bytes()));
        refs.insert(".zgroup".to_owned(), inline(json!({"zarr_format": 2})));
        for var in self.variables.iter().filter(|v| v.zarr_compatible()) {
            let order = match (var.dtype.size(), var.big_endian) {
                (1, _) => "|",
                (_, true) => ">",
                (_, false) => "<",
            };
            let filters: Vec<_> = var
                .filters
                .iter()
                .map(|f| match f {
                    Nc4Filter::Shuffle => json!({"id": "shuffle", "elementsize": var.dtype.size()}),
                    Nc4Filter::Deflate { level } => json!({"id": "zlib", "level": level}),
                    Nc4Filter::Fletcher32 => json!({"id": "fletcher32"}),
                    Nc4Filter::Blosc {
                        clevel,
                        shuffle,
                        compressor,
                    } => {
                        let cname = ["blosclz", "lz4", "lz4hc", "snappy", "zlib", "zstd"]
                            .get(*compressor as usize)
                            .copied()
                            .unwrap_or("lz4");
                        json!({
                        "id": "blosc",
                        "cname": cname,
                        "clevel": clevel,
                        "shuffle": shuffle,
                        "blocksize": 0,
                        })
                    }
                })
                .collect();
            let zarray = json!({
                "zarr_format": 2,
                "shape": var.shape,
                "chunks": var.chunk_shape,
                "dtype": format!("{order}{}", var.dtype.numpy()),
                "compressor": null,
                "filters": if filters.is_empty() { serde_json::Value::Null } else { filters.into() },
                "fill_value": fill_json(var),
                "order": "C",
            });
            refs.insert(format!("{}/.zarray", var.name), inline(zarray));
            refs.insert(format!("{}/.zattrs", var.name), inline(json!({})));
            for c in &var.chunks {
                let key = if c.coords.is_empty() {
                    "0".to_owned()
                } else {
                    c.coords
                        .iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(".")
                };
                refs.insert(
                    format!("{}/{key}", var.name),
                    Ref::Range {
                        url: url.clone(),
                        offset: c.offset,
                        length: Some(c.size),
                    },
                );
            }
        }
        KerchunkStore::from_refs(refs)
    }
}

/// A NetCDF-4 file opened for direct chunk reads; `read_chunk` takes `&self` and is safe to call
/// from many threads (positioned reads, no HDF5).
pub struct Nc4File {
    index: Nc4Index,
    file: File,
}

impl Nc4File {
    /// Open `path`, loading or building its chunk index.
    pub fn open(path: &Path) -> Result<Self, Nc4Error> {
        Self::with_index(Nc4Index::open(path)?)
    }

    /// Open the file an existing index describes.
    pub fn with_index(index: Nc4Index) -> Result<Self, Nc4Error> {
        let file = File::open(&index.path).map_err(|source| Nc4Error::Io {
            path: index.path.display().to_string(),
            source,
        })?;
        Ok(Self { index, file })
    }

    pub fn index(&self) -> &Nc4Index {
        &self.index
    }

    /// Stored (still filtered) bytes of one chunk.
    pub fn read_raw(&self, chunk: &Nc4Chunk) -> Result<Vec<u8>, Nc4Error> {
        let mut buf = vec![0u8; chunk.size as usize];
        self.file
            .read_exact_at(&mut buf, chunk.offset)
            .map_err(|source| Nc4Error::Io {
                path: self.index.path.display().to_string(),
                source,
            })?;
        Ok(buf)
    }

    /// Read and decode the chunk at grid coordinates `coords` of variable `name`.
    pub fn read_chunk(&self, name: &str, coords: &[u64]) -> Result<DecodedChunk, Nc4Error> {
        let var = self
            .index
            .variable(name)
            .ok_or_else(|| Nc4Error::NoVariable(name.to_owned()))?;
        if let Some(reason) = &var.unsupported {
            return Err(Nc4Error::Unsupported {
                var: name.to_owned(),
                reason: reason.clone(),
            });
        }
        let data = match var.chunk(coords) {
            None => var.fill_chunk(),
            Some(c) => var
                .decode(self.read_raw(c)?, c.filter_mask)
                .map_err(|msg| Nc4Error::Decode {
                    path: self.index.path.display().to_string(),
                    var: name.to_owned(),
                    msg: format!("chunk {coords:?}: {msg}"),
                })?,
        };
        Ok(DecodedChunk {
            data,
            shape: var.chunk_shape.clone(),
            origin: coords
                .iter()
                .zip(&var.chunk_shape)
                .map(|(i, c)| i * c)
                .collect(),
        })
    }
}

fn file_identity(path: &Path) -> Result<(PathBuf, u64, i128), Nc4Error> {
    let ioerr = |source| Nc4Error::Io {
        path: path.display().to_string(),
        source,
    };
    let canon = std::fs::canonicalize(path).map_err(ioerr)?;
    let meta = std::fs::metadata(&canon).map_err(ioerr)?;
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i128);
    Ok((canon, meta.len(), mtime_ns))
}

/// `$CDORS_CACHE/nc4index/<fnv1a64>.json`, `None` without `CDORS_CACHE`.
fn cache_path(canon: &Path, size: u64, mtime_ns: i128) -> Option<PathBuf> {
    let dir = std::env::var_os("CDORS_CACHE")?;
    let key = format!(
        "{INDEX_FORMAT}\0{}\0{size}\0{mtime_ns}",
        canon.to_string_lossy()
    );
    // FNV-1a: stable across Rust versions, unlike `DefaultHasher`.
    let hash = key.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    Some(
        PathBuf::from(dir)
            .join("nc4index")
            .join(format!("{hash:016x}.json")),
    )
}

/// Write via a temporary name and rename, so concurrent readers never see a partial file.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

fn collect_datasets(
    group: &hdf5_metno::Group,
    out: &mut Vec<hdf5_metno::Dataset>,
) -> hdf5_metno::Result<()> {
    out.extend(group.datasets()?);
    for g in group.groups()? {
        collect_datasets(&g, out)?;
    }
    Ok(())
}

/// netCDF-4 writes dimensions without a coordinate variable as datasets whose `NAME` attribute
/// says so; they are not netCDF variables.
fn is_dimension_only(ds: &hdf5_metno::Dataset) -> bool {
    use hdf5_metno::types::FixedAscii;
    ds.attr("NAME")
        .and_then(|a| a.read_scalar::<FixedAscii<128>>())
        .is_ok_and(|s| {
            s.as_str()
                .starts_with("This is a netCDF dimension but not a netCDF variable")
        })
}

fn index_variable(ds: &hdf5_metno::Dataset) -> hdf5_metno::Result<Nc4Variable> {
    use hdf5_metno::types::{FloatSize, IntSize, TypeDescriptor as T};
    let full = ds.name();
    let mut name = full.trim_start_matches('/').to_owned();
    // netCDF renames a variable that shares a dimension's name without being its coordinate.
    if let Some((dir, base)) = name.rsplit_once('/') {
        if let Some(b) = base.strip_prefix("_nc4_non_coord_") {
            name = format!("{dir}/{b}");
        }
    } else if let Some(b) = name.strip_prefix("_nc4_non_coord_") {
        name = b.to_owned();
    }
    let shape: Vec<u64> = ds.shape().iter().map(|&s| s as u64).collect();
    let datatype = ds.dtype()?;
    let big_endian = matches!(
        datatype.byte_order(),
        hdf5_metno::datatype::ByteOrder::BigEndian
    );
    let mut unsupported = None;
    let dtype = match datatype.to_descriptor() {
        Ok(T::Integer(IntSize::U1)) => Nc4Dtype::I8,
        Ok(T::Integer(IntSize::U2)) => Nc4Dtype::I16,
        Ok(T::Integer(IntSize::U4)) => Nc4Dtype::I32,
        Ok(T::Integer(IntSize::U8)) => Nc4Dtype::I64,
        Ok(T::Unsigned(IntSize::U1)) => Nc4Dtype::U8,
        Ok(T::Unsigned(IntSize::U2)) => Nc4Dtype::U16,
        Ok(T::Unsigned(IntSize::U4)) => Nc4Dtype::U32,
        Ok(T::Unsigned(IntSize::U8)) => Nc4Dtype::U64,
        Ok(T::Float(FloatSize::U4)) => Nc4Dtype::F32,
        Ok(T::Float(FloatSize::U8)) => Nc4Dtype::F64,
        other => {
            unsupported = Some(format!("data type {other:?}"));
            Nc4Dtype::U8
        }
    };
    let dcpl = ds.dcpl()?;
    let mut filters = Vec::new();
    match dcpl.get_filters() {
        Ok(list) => {
            for f in list {
                match f {
                    hdf5_metno::filters::Filter::Shuffle => filters.push(Nc4Filter::Shuffle),
                    hdf5_metno::filters::Filter::Deflate(level) => {
                        filters.push(Nc4Filter::Deflate { level })
                    }
                    hdf5_metno::filters::Filter::Fletcher32 => filters.push(Nc4Filter::Fletcher32),
                    // cd_values: filter rev, blosc version, typesize, chunk bytes, clevel,
                    // shuffle, compressor (the last three optional).
                    hdf5_metno::filters::Filter::User(id, cd) if id == H5Z_FILTER_BLOSC => {
                        let at = |i: usize, default: u32| cd.get(i).copied().unwrap_or(default);
                        filters.push(Nc4Filter::Blosc {
                            clevel: at(4, 5) as u8,
                            shuffle: at(5, 1) as u8,
                            compressor: at(6, 0) as u8,
                        });
                    }
                    other => {
                        unsupported.get_or_insert(format!("filter {other:?}"));
                    }
                }
            }
        }
        Err(e) => {
            unsupported.get_or_insert(format!("filter pipeline: {e}"));
        }
    }
    let fill_value = if unsupported.is_none() {
        fill_value_bytes(ds, dtype)
    } else {
        None
    };
    let esize = dtype.size() as u64;
    let mut chunks = Vec::new();
    let (layout, chunk_shape) = match ds.layout() {
        hdf5_metno::dataset::Layout::Chunked => {
            let chunk_shape: Vec<u64> = ds
                .chunk()
                .unwrap_or_default()
                .iter()
                .map(|&c| c as u64)
                .collect();
            if unsupported.is_none() {
                ds.chunks_visit(|info| {
                    chunks.push(Nc4Chunk {
                        coords: info
                            .offset
                            .iter()
                            .zip(&chunk_shape)
                            .map(|(o, c)| o / c)
                            .collect(),
                        offset: info.addr,
                        size: info.size,
                        filter_mask: info.filter_mask,
                    });
                    0
                })?;
            }
            (Nc4Layout::Chunked, chunk_shape)
        }
        hdf5_metno::dataset::Layout::Contiguous => {
            let row_elems: u64 = shape.iter().skip(1).product();
            let rows = shape.first().copied().unwrap_or(1);
            let per_chunk =
                (CONTIGUOUS_TARGET_BYTES / (row_elems * esize).max(1)).clamp(1, rows.max(1));
            let mut chunk_shape = shape.clone();
            if let Some(first) = chunk_shape.first_mut() {
                *first = per_chunk;
            }
            // `offset()` is undefined while no data was written: all chunks are fill.
            // The last virtual chunk may be short; `decode` pads it with the fill value.
            if let (Some(addr), None) = (ds.offset(), &unsupported) {
                let total = rows * row_elems * esize;
                let step = per_chunk * row_elems * esize;
                let mut i = 0;
                while i * step < total {
                    let mut coords = vec![0; shape.len()];
                    if let Some(first) = coords.first_mut() {
                        *first = i;
                    }
                    chunks.push(Nc4Chunk {
                        coords,
                        offset: addr + i * step,
                        size: step.min(total - i * step),
                        filter_mask: 0,
                    });
                    i += 1;
                }
            }
            (Nc4Layout::Contiguous, chunk_shape)
        }
        other => {
            unsupported.get_or_insert(format!("layout {other:?}"));
            (Nc4Layout::Other, shape.clone())
        }
    };
    chunks.sort_by(|a, b| a.coords.cmp(&b.coords));
    Ok(Nc4Variable {
        name,
        shape,
        chunk_shape,
        dtype,
        big_endian,
        layout,
        filters,
        fill_value,
        unsupported,
        chunks,
    })
}

/// `_FillValue` (or the HDF5 dataset fill value) as little-endian bytes of one element.
fn fill_value_bytes(ds: &hdf5_metno::Dataset, dtype: Nc4Dtype) -> Option<Vec<u8>> {
    macro_rules! fill {
        ($t:ty) => {{
            let attr = ds
                .attr("_FillValue")
                .ok()
                .and_then(|a| a.read_raw::<$t>().ok())
                .and_then(|v| v.first().copied());
            let value = match attr {
                Some(v) => Some(v),
                None => ds.dcpl().ok()?.get_fill_value_as::<$t>().ok().flatten(),
            };
            value.map(|v| v.to_le_bytes().to_vec())
        }};
    }
    match dtype {
        Nc4Dtype::I8 => fill!(i8),
        Nc4Dtype::U8 => fill!(u8),
        Nc4Dtype::I16 => fill!(i16),
        Nc4Dtype::U16 => fill!(u16),
        Nc4Dtype::I32 => fill!(i32),
        Nc4Dtype::U32 => fill!(u32),
        Nc4Dtype::I64 => fill!(i64),
        Nc4Dtype::U64 => fill!(u64),
        Nc4Dtype::F32 => fill!(f32),
        Nc4Dtype::F64 => fill!(f64),
    }
}

/// Zarr v2 `fill_value` JSON for a variable's fill bytes.
fn fill_json(var: &Nc4Variable) -> serde_json::Value {
    let Some(b) = &var.fill_value else {
        return serde_json::Value::Null;
    };
    let float = |v: f64| {
        if v.is_nan() {
            json!("NaN")
        } else if v.is_infinite() {
            json!(if v > 0.0 { "Infinity" } else { "-Infinity" })
        } else {
            json!(v)
        }
    };
    macro_rules! le {
        ($t:ty) => {
            <$t>::from_le_bytes(b.as_slice().try_into().unwrap_or_default())
        };
    }
    match var.dtype {
        Nc4Dtype::I8 => json!(le!(i8)),
        Nc4Dtype::U8 => json!(le!(u8)),
        Nc4Dtype::I16 => json!(le!(i16)),
        Nc4Dtype::U16 => json!(le!(u16)),
        Nc4Dtype::I32 => json!(le!(i32)),
        Nc4Dtype::U32 => json!(le!(u32)),
        Nc4Dtype::I64 => json!(le!(i64)),
        Nc4Dtype::U64 => json!(le!(u64)),
        Nc4Dtype::F32 => float(f64::from(le!(f32))),
        Nc4Dtype::F64 => float(le!(f64)),
    }
}

/// Decompress one c-blosc frame (thread-safe: `blosc_decompress_ctx` keeps no global state).
fn blosc_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut nbytes = 0usize;
    // SAFETY: c-blosc reads at most `data.len()` bytes; it validates the header against that
    // length before trusting `nbytes`, and writes at most `nbytes` bytes into `out`.
    unsafe {
        if blosc_src::blosc_cbuffer_validate(data.as_ptr().cast(), data.len(), &mut nbytes) != 0 {
            return Err("blosc: invalid frame header".into());
        }
        let mut out = vec![0u8; nbytes];
        let n = blosc_src::blosc_decompress_ctx(
            data.as_ptr().cast(),
            out.as_mut_ptr().cast(),
            nbytes,
            1,
        );
        if n < 0 || n as usize != nbytes {
            return Err(format!("blosc: decompression failed ({n})"));
        }
        Ok(out)
    }
}

fn inflate(data: &[u8], expect: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut out = Vec::with_capacity(expect);
    flate2::read::ZlibDecoder::new(data)
        .read_to_end(&mut out)
        .map_err(|e| format!("inflate: {e}"))?;
    Ok(out)
}

/// Undo HDF5 byte shuffle: the stored bytes are all first bytes, then all second bytes, ...
/// Trailing bytes that do not fill a whole element are stored unshuffled.
fn unshuffle(data: &[u8], esize: usize) -> Vec<u8> {
    if esize <= 1 {
        return data.to_vec();
    }
    let n = data.len() / esize;
    let mut out = vec![0u8; data.len()];
    for (b, plane) in data[..n * esize].chunks_exact(n).enumerate() {
        for (i, &v) in plane.iter().enumerate() {
            out[i * esize + b] = v;
        }
    }
    out[n * esize..].copy_from_slice(&data[n * esize..]);
    out
}
