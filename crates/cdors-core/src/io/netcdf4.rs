//! NetCDF-4 (HDF5) files: metadata from netCDF-C, chunk data from the chunk index.
//!
//! [`Nc4Source::open`] first opens the file through netCDF-C ([`NetcdfSource`]) to build the
//! dataset description, so it is identical to the serial netCDF path. It then loads (or builds
//! once and caches) the file's [`Nc4Index`]. Every variable whose index entry is readable without
//! HDF5 and whose chunk grid equals netCDF-C's is read directly: `read_chunk` returns the stored
//! bytes (a positioned read, no HDF5, no lock) as [`RawChunk::Encoded`], and the filters
//! (deflate, shuffle, fletcher32, blosc) are undone by the decoder on the compute pool. The other
//! variables go through netCDF-C, serially.
//!
//! Contiguous variables keep netCDF-C's chunk grid (one record per chunk, or the whole variable
//! for 0-D and 1-D): their records are plain byte ranges in the file.
//!
//! `CDORS_NC4=netcdf` disables this reader; NetCDF-4 files are then read through netCDF-C only.

use super::netcdf_fallback::NetcdfSource;
use super::netcdf4_index::{Nc4File, Nc4Filter, Nc4Layout, Nc4Variable};
use super::{ChunkDecoder, ChunkGrid, ChunkSource, DecodedChunk, EncodedChunk, RawChunk, Values};
use crate::error::{Error, Result};
use crate::model::{DType, Dataset, Encoding};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// How a directly read variable is stored.
enum Storage {
    /// HDF5 chunks equal to the chunk grid; decoded through the filter pipeline.
    Chunked,
    /// One unfiltered byte range starting at this file offset (`None`: never written).
    Contiguous(Option<u64>),
}

/// A variable read without HDF5. Also the decoder of its chunks.
struct DirectVar {
    file: Arc<Nc4File>,
    /// Position in `file.index().variables`.
    pos: usize,
    storage: Storage,
    dtype: DType,
    encoding: Encoding,
    grid: ChunkGrid,
}

impl DirectVar {
    fn index(&self) -> &Nc4Variable {
        &self.file.index().variables[self.pos]
    }

    fn err(&self, msg: impl std::fmt::Display) -> Error {
        Error::bad_data(format!(
            "{}: variable '{}': {msg}",
            self.file.index().path.display(),
            self.index().name
        ))
    }

    /// File byte range of chunk `indices`, `None` if never written (fill value).
    fn byte_range(&self, indices: &[u64]) -> Option<(u64, u64)> {
        match self.storage {
            Storage::Chunked => self.index().chunk(indices).map(|c| (c.offset, c.size)),
            Storage::Contiguous(base) => {
                let esize = self.dtype.size() as u64;
                let origin = self.grid.origin(indices);
                let ext = self.grid.extent(indices);
                let row: u64 = self.grid.shape.iter().skip(1).product::<usize>() as u64 * esize;
                let len = ext.iter().product::<usize>() as u64 * esize;
                base.map(|b| (b + origin.first().copied().unwrap_or(0) * row, len))
            }
        }
    }
}

impl ChunkDecoder for DirectVar {
    fn decode(&self, indices: &[u64], bytes: Option<&[u8]>) -> Result<DecodedChunk> {
        let var = self.index();
        let ext = self.grid.extent(indices);
        let n: usize = ext.iter().product();
        let esize = self.dtype.size();
        // native-order elements: the full chunk (Chunked) or exactly the extent (Contiguous)
        let (raw, full): (Vec<u8>, &[usize]) = match (&self.storage, bytes) {
            (Storage::Chunked, Some(b)) => {
                let mask = var.chunk(indices).map_or(0, |c| c.filter_mask);
                let d = var
                    .decode(b.to_vec(), mask)
                    .map_err(|m| self.err(format!("chunk {indices:?}: {m}")))?;
                (d, &self.grid.chunk_shape)
            }
            (Storage::Chunked, None) => (var.fill_chunk(), &self.grid.chunk_shape),
            (Storage::Contiguous(_), Some(b)) => {
                if b.len() != n * esize {
                    return Err(self.err(format!(
                        "record {indices:?}: {} bytes, expected {}",
                        b.len(),
                        n * esize
                    )));
                }
                let mut d = b.to_vec();
                if var.big_endian != cfg!(target_endian = "big") && esize > 1 {
                    for e in d.chunks_exact_mut(esize) {
                        e.reverse();
                    }
                }
                (d, &ext)
            }
            (Storage::Contiguous(_), None) => {
                let mut one = var.fill_value.clone().unwrap_or_else(|| vec![0; esize]);
                if cfg!(target_endian = "big") {
                    one.reverse();
                }
                (one.repeat(n), &ext)
            }
        };
        let values = super::convert_bytes(self.dtype, &raw, &self.encoding)?;
        let values = if full == ext.as_slice() {
            values
        } else {
            match values {
                Values::F32(v) => Values::F32(super::zarr::trim(v, full, &ext)),
                Values::F64(v) => Values::F64(super::zarr::trim(v, full, &ext)),
            }
        };
        Ok(DecodedChunk {
            origin: self.grid.origin(indices),
            shape: ext,
            values,
        })
    }

    fn codecs(&self) -> String {
        let names: Vec<String> = self
            .index()
            .filters
            .iter()
            .map(|f| match f {
                Nc4Filter::Shuffle => "shuffle".to_owned(),
                Nc4Filter::Deflate { level } => format!("zlib:{level}"),
                Nc4Filter::Fletcher32 => "fletcher32".to_owned(),
                Nc4Filter::Blosc { compressor, .. } => format!(
                    "blosc:{}",
                    ["blosclz", "lz4", "lz4hc", "snappy", "zlib", "zstd"]
                        .get(*compressor as usize)
                        .copied()
                        .unwrap_or("?")
                ),
            })
            .collect();
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(",")
        }
    }
}

/// A NetCDF-4 file: netCDF-C metadata, direct chunk reads where possible.
pub struct Nc4Source {
    nc: NetcdfSource,
    direct: HashMap<String, Arc<DirectVar>>,
    /// Variables read through netCDF-C, with the reason (for `sinfo` and diagnostics).
    fallback: Vec<(String, String)>,
}

/// Whether the environment asks for netCDF-C reads only (`CDORS_NC4=netcdf`).
pub fn netcdf_only() -> bool {
    std::env::var("CDORS_NC4").is_ok_and(|v| v.eq_ignore_ascii_case("netcdf"))
}

impl Nc4Source {
    /// Opens a NetCDF-4 file. Metadata comes from netCDF-C; the chunk index is loaded from
    /// `$CDORS_CACHE/nc4index/` or built once through HDF5. If the index cannot be built, every
    /// variable is read through netCDF-C (with a warning).
    pub fn open(path: &str) -> Result<Self> {
        let nc = NetcdfSource::open(path)?;
        let file = match Nc4File::open(Path::new(path)) {
            Ok(f) => Arc::new(f),
            Err(e) => {
                crate::exec::threads::warn(
                    "no_chunk_index",
                    &format!("no chunk index for '{path}' ({e}); reading through netCDF-C"),
                );
                let fallback = nc
                    .dataset()
                    .vars
                    .iter()
                    .map(|v| (v.name.clone(), "no chunk index".to_owned()))
                    .collect();
                return Ok(Self {
                    nc,
                    direct: HashMap::new(),
                    fallback,
                });
            }
        };
        let mut direct = HashMap::new();
        let mut fallback = Vec::new();
        for v in &nc.dataset().vars {
            let grid = nc.chunk_grid(&v.name)?;
            match Self::direct_var(&file, v, grid) {
                Ok(dv) => {
                    direct.insert(v.name.clone(), Arc::new(dv));
                }
                Err(reason) => fallback.push((v.name.clone(), reason)),
            }
        }
        Ok(Self {
            nc,
            direct,
            fallback,
        })
    }

    fn direct_var(
        file: &Arc<Nc4File>,
        v: &crate::model::Variable,
        grid: ChunkGrid,
    ) -> std::result::Result<DirectVar, String> {
        let pos = file
            .index()
            .variables
            .iter()
            .position(|x| x.name == v.name)
            .ok_or("not in the chunk index")?;
        let iv = &file.index().variables[pos];
        if let Some(r) = &iv.unsupported {
            return Err(r.clone());
        }
        if !v.dtype.is_numeric() || v.dtype.size() != iv.dtype.size() {
            return Err(format!("data type {:?}", v.dtype));
        }
        let shape: Vec<u64> = grid.shape.iter().map(|&s| s as u64).collect();
        if iv.shape != shape {
            return Err(format!("shape {:?} != {:?}", iv.shape, shape));
        }
        let storage = match iv.layout {
            Nc4Layout::Chunked => {
                let cs: Vec<u64> = grid.chunk_shape.iter().map(|&c| c as u64).collect();
                if iv.chunk_shape != cs {
                    return Err(format!("chunks {:?} != {:?}", iv.chunk_shape, cs));
                }
                Storage::Chunked
            }
            Nc4Layout::Contiguous => {
                // netCDF-C's grid: records along the first dimension, or the whole variable
                let whole = grid.chunk_shape == grid.shape;
                let records = grid.chunk_shape.first() == Some(&1)
                    && grid.chunk_shape[1..] == grid.shape[1..];
                if !(whole || records) {
                    return Err(format!("contiguous with chunks {:?}", grid.chunk_shape));
                }
                Storage::Contiguous(iv.chunks.first().map(|c| c.offset))
            }
            Nc4Layout::Other => return Err("layout".into()),
        };
        Ok(DirectVar {
            file: file.clone(),
            pos,
            storage,
            dtype: v.dtype,
            encoding: v.encoding.clone(),
            grid,
        })
    }

    /// Variables read through netCDF-C, with the reason.
    pub fn fallback_vars(&self) -> &[(String, String)] {
        &self.fallback
    }
}

impl ChunkSource for Nc4Source {
    fn dataset(&self) -> &Dataset {
        self.nc.dataset()
    }

    fn chunk_grid(&self, var: &str) -> Result<ChunkGrid> {
        self.nc.chunk_grid(var)
    }

    fn read_chunk(&self, var: &str, indices: &[u64]) -> Result<RawChunk> {
        let Some(dv) = self.direct.get(var) else {
            return self.nc.read_chunk(var, indices);
        };
        dv.grid.check(var, indices)?;
        let bytes = match dv.byte_range(indices) {
            None => None,
            Some((offset, size)) => Some(
                dv.file
                    .read_raw(&super::netcdf4_index::Nc4Chunk {
                        coords: Vec::new(),
                        offset,
                        size,
                        filter_mask: 0,
                    })
                    .map_err(|e| Error::io(e.to_string()))?,
            ),
        };
        Ok(RawChunk::Encoded(EncodedChunk {
            indices: indices.to_vec(),
            bytes,
            decoder: dv.clone() as Arc<dyn ChunkDecoder>,
        }))
    }

    fn read_var(&self, var: &str) -> Result<Vec<f64>> {
        self.nc.read_var(var)
    }

    fn stored_size(&self, var: &str, indices: &[u64]) -> Option<u64> {
        let dv = self.direct.get(var)?;
        Some(dv.byte_range(indices).map_or(0, |(_, size)| size))
    }

    fn codecs(&self, var: &str) -> Option<String> {
        match self.direct.get(var) {
            Some(dv) => Some(ChunkDecoder::codecs(dv.as_ref())),
            None => self
                .fallback
                .iter()
                .find(|(n, _)| n == var)
                .map(|(_, r)| format!("netCDF-C ({r})")),
        }
    }
}
