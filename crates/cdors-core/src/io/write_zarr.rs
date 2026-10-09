//! Zarr writer: v3 (default) or v2 (`-f zarr2`), through `zarrs` on the filesystem.
//!
//! Layout as xarray writes it: one group with the global attributes, one array per data and
//! coordinate variable, dimension names in `dimension_names` (v3) or `_ARRAY_DIMENSIONS` (v2),
//! zstd compression, NaN as the fill value of float arrays (missing values stay NaN), and CF
//! attributes (time units and calendar, `coordinates`, `grid_mapping`). Zarr v2 stores also get
//! consolidated metadata (`.zmetadata`). Chunks are encoded and stored in parallel, without a
//! per-chunk fsync: the store is synced once in `finish` (see [`OutputStore`]).

use crate::error::{Error, Result};
use crate::exec::{OutMeta, OutVar, Writer, out_meta};
use crate::io::Values;
use crate::model::{Attrs, DType};
use crate::plan::Plan;
use serde_json::{Map, Value, json};
use std::io::Write;
use std::os::fd::{AsRawFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zarrs::array::{Array, ArrayMetadata, ArrayMetadataV2, ArrayMetadataV3};
use zarrs::filesystem::FilesystemStore;
use zarrs::storage::byte_range::ByteRangeIterator;
use zarrs::storage::{
    Bytes, MaybeBytesIterator, OffsetBytesIterator, ReadableStorageTraits,
    ReadableWritableStorageTraits, StorageError, StoreKey, StorePrefix, WritableStorageTraits,
    store_set_partial_many,
};

type Store = dyn ReadableWritableStorageTraits;

/// zstd level of written chunks (1: fast; output writing is often the bottleneck).
const ZSTD_LEVEL: i32 = 1;

pub struct ZarrWriter {
    arrays: Vec<Array<Store>>,
    vars: Vec<OutVar>,
    /// The filesystem store (None for in-memory intermediates), synced by `finish`.
    fs: Option<Arc<OutputStore>>,
}

/// The output's filesystem store: zarrs' [`FilesystemStore`], but writes skip its per-file
/// `fsync`.
///
/// `FilesystemStore::set` calls `sync_all` on every file it writes. On Lustre each of these is a
/// synchronous server round trip, so an output of many small chunks took seconds (up to a minute
/// on a loaded file system) where the same NetCDF output took a fraction of a second. Here a
/// write is `open` + `write` + a checked `close` (Lustre and NFS report deferred write errors on
/// close), and [`OutputStore::sync`] flushes the whole store once, after the last chunk, with a
/// single `syncfs(2)` on its file system. (Fsyncing each file once at the end instead was
/// measured to be as slow as before: the cost is the per-file sync itself.)
///
/// One sync at the end is enough for durability: the store is the temporary output directory,
/// which becomes visible under the output name only through the rename in `exec::publish`, and
/// that runs after `Writer::finish`. A crash before the rename leaves only the temporary
/// directory; after it, the data of every chunk has already been flushed.
/// Reads and erases go to the wrapped `FilesystemStore`.
struct OutputStore {
    inner: FilesystemStore,
    root: PathBuf,
}

impl OutputStore {
    fn new(path: &Path) -> Result<Self> {
        Ok(Self {
            inner: FilesystemStore::new(path).map_err(zerr)?,
            root: path.to_path_buf(),
        })
    }

    fn write_file(path: &Path, value: &[u8]) -> std::io::Result<()> {
        // `create_dir_all` copes with another thread creating the same directory meanwhile
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::File::create(path)?;
        f.write_all(value)?;
        // `drop` ignores the result of close(2), which can carry deferred write errors
        let fd = f.into_raw_fd();
        // SAFETY: `fd` was just released by `into_raw_fd` and is closed exactly once here.
        if unsafe { libc::close(fd) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Flushes the file system holding the store with one `syncfs(2)` (which also returns
    /// write-back errors on Linux >= 5.8).
    fn sync(&self) -> Result<()> {
        let io = |e: std::io::Error| Error::from(e).with("path", self.root.display().to_string());
        let dir = std::fs::File::open(&self.root).map_err(io)?;
        // SAFETY: `dir` is an open descriptor for the duration of the call.
        if unsafe { libc::syncfs(dir.as_raw_fd()) } != 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        Ok(())
    }
}

impl ReadableStorageTraits for OutputStore {
    fn get_partial_many<'a>(
        &'a self,
        key: &StoreKey,
        byte_ranges: ByteRangeIterator<'a>,
    ) -> std::result::Result<MaybeBytesIterator<'a>, StorageError> {
        self.inner.get_partial_many(key, byte_ranges)
    }

    fn size_key(&self, key: &StoreKey) -> std::result::Result<Option<u64>, StorageError> {
        self.inner.size_key(key)
    }

    fn supports_get_partial(&self) -> bool {
        self.inner.supports_get_partial()
    }
}

impl WritableStorageTraits for OutputStore {
    fn set(&self, key: &StoreKey, value: Bytes) -> std::result::Result<(), StorageError> {
        Ok(Self::write_file(&self.inner.key_to_fspath(key), &value)?)
    }

    fn set_partial_many(
        &self,
        key: &StoreKey,
        offset_values: OffsetBytesIterator,
    ) -> std::result::Result<(), StorageError> {
        // read-modify-write through `set` (not used by the writer: chunks are stored whole)
        store_set_partial_many(self, key, offset_values)
    }

    fn erase(&self, key: &StoreKey) -> std::result::Result<(), StorageError> {
        self.inner.erase(key)
    }

    fn erase_prefix(&self, prefix: &StorePrefix) -> std::result::Result<(), StorageError> {
        self.inner.erase_prefix(prefix)
    }

    fn supports_set_partial(&self) -> bool {
        false
    }
}

fn zerr(e: impl std::fmt::Display) -> Error {
    Error::io(format!("zarr: {e}"))
}

fn attrs_json(a: &Attrs) -> Map<String, Value> {
    a.iter().map(|(k, v)| (k.clone(), v.to_json())).collect()
}

/// Array metadata as JSON (v2: `.zarray` with the attributes kept apart).
fn array_json(
    v2: bool,
    shape: &[usize],
    chunks: &[usize],
    dtype: DType,
    dims: &[String],
    attrs: &Map<String, Value>,
    compress: bool,
) -> Value {
    let (v3t, v2t, fill) = match dtype {
        DType::F32 => ("float32", "<f4", json!("NaN")),
        DType::I32 => ("int32", "<i4", json!(0)),
        _ => ("float64", "<f8", json!("NaN")),
    };
    if v2 {
        let mut a = attrs.clone();
        a.insert("_ARRAY_DIMENSIONS".into(), json!(dims));
        let compressor = if compress {
            json!({"id": "zstd", "level": ZSTD_LEVEL})
        } else {
            Value::Null
        };
        json!({
            "zarr_format": 2,
            "shape": shape,
            "chunks": chunks,
            "dtype": v2t,
            "compressor": compressor,
            "fill_value": fill,
            "order": "C",
            "filters": null,
            "dimension_separator": ".",
            "attributes": a,
        })
    } else {
        let mut codecs = vec![json!({"name": "bytes", "configuration": {"endian": "little"}})];
        if compress {
            codecs.push(
                json!({"name": "zstd", "configuration": {"level": ZSTD_LEVEL, "checksum": false}}),
            );
        }
        json!({
            "zarr_format": 3,
            "node_type": "array",
            "shape": shape,
            "data_type": v3t,
            "chunk_grid": {"name": "regular", "configuration": {"chunk_shape": chunks}},
            "chunk_key_encoding": {"name": "default", "configuration": {"separator": "/"}},
            "fill_value": fill,
            "codecs": codecs,
            "attributes": attrs,
            "dimension_names": dims,
        })
    }
}

fn make_array(store: &Arc<Store>, path: &str, meta: Value, v2: bool) -> Result<Array<Store>> {
    let md = if v2 {
        ArrayMetadata::V2(serde_json::from_value::<ArrayMetadataV2>(meta).map_err(zerr)?)
    } else {
        ArrayMetadata::V3(serde_json::from_value::<ArrayMetadataV3>(meta).map_err(zerr)?)
    };
    let a = Array::new_with_metadata(store.clone(), path, md).map_err(zerr)?;
    a.store_metadata().map_err(zerr)?;
    Ok(a)
}

fn write_json(store: &Store, key: &str, v: &Value) -> Result<()> {
    let s = serde_json::to_vec_pretty(v).map_err(zerr)?;
    let k = StoreKey::new(key).map_err(zerr)?;
    store.set(&k, s.into()).map_err(zerr)
}

/// Pads a trimmed edge chunk to the full chunk shape with NaN.
fn pad<T: Copy>(data: &[T], shape: &[usize], full: &[usize], fill: T) -> Vec<T> {
    if shape == full {
        return data.to_vec();
    }
    let n: usize = full.iter().product();
    let mut out = vec![fill; n];
    let nd = full.len();
    let row = shape[nd - 1];
    if shape.contains(&0) {
        return out;
    }
    let mut idx = vec![0usize; nd];
    let mut src = 0;
    loop {
        let mut off = 0;
        for d in 0..nd {
            off = off * full[d] + idx[d];
        }
        out[off..off + row].copy_from_slice(&data[src..src + row]);
        src += row;
        let mut d = nd - 1;
        loop {
            if d == 0 {
                return out;
            }
            d -= 1;
            idx[d] += 1;
            if idx[d] < shape[d] {
                break;
            }
            idx[d] = 0;
        }
    }
}

impl ZarrWriter {
    pub fn create(
        path: &Path,
        plan: &Plan,
        lay: &[OutVar],
        v2: bool,
        history: Option<&str>,
    ) -> Result<Self> {
        // exclusive: never writes into a directory this process did not create
        std::fs::create_dir(path)?;
        crate::exec::publish::mark_created(path, true);
        let fs = Arc::new(OutputStore::new(path)?);
        let mut w = Self::create_in(fs.clone(), plan, lay, v2, history, true)?;
        w.fs = Some(fs);
        Ok(w)
    }

    /// Writes the metadata and coordinates into any store (the filesystem, or memory for the
    /// intermediates of multi-stage chains); `compress: false` stores raw little-endian values.
    pub fn create_in(
        store: Arc<Store>,
        plan: &Plan,
        lay: &[OutVar],
        v2: bool,
        history: Option<&str>,
        compress: bool,
    ) -> Result<Self> {
        let meta: OutMeta = out_meta(plan, lay, history)?;
        zarrs::config::global_config_mut().set_include_zarrs_metadata(false);
        let global = attrs_json(&meta.global);
        let mut consolidated = Map::new();
        if v2 {
            let g = json!({"zarr_format": 2});
            write_json(&*store, ".zgroup", &g)?;
            write_json(&*store, ".zattrs", &Value::Object(global.clone()))?;
            consolidated.insert(".zgroup".into(), g);
            consolidated.insert(".zattrs".into(), Value::Object(global.clone()));
        }
        let mut record = |name: &str, m: &Value| {
            if !v2 {
                consolidated.insert(name.to_owned(), m.clone());
            } else {
                let mut za = m.clone();
                let attrs = za
                    .as_object_mut()
                    .and_then(|o| o.remove("attributes"))
                    .unwrap_or(json!({}));
                consolidated.insert(format!("{name}/.zarray"), za);
                consolidated.insert(format!("{name}/.zattrs"), attrs);
            }
        };
        for c in &meta.coords {
            let shape = c.shape.clone();
            let chunks: Vec<usize> = shape.iter().map(|&n| n.max(1)).collect();
            let m = array_json(
                v2,
                &shape,
                &chunks,
                c.dtype,
                &c.dims,
                &attrs_json(&c.attrs),
                compress,
            );
            record(&c.name, &m);
            let a = make_array(&store, &format!("/{}", c.name), m, v2)?;
            let idx = vec![0u64; shape.len()];
            match c.dtype {
                DType::F32 => {
                    let x: Vec<f32> = c.values.iter().map(|&x| x as f32).collect();
                    a.store_chunk(&idx, x).map_err(zerr)?;
                }
                DType::I32 => {
                    let x: Vec<i32> = c.values.iter().map(|&x| x as i32).collect();
                    a.store_chunk(&idx, x).map_err(zerr)?;
                }
                _ => a.store_chunk(&idx, c.values.clone()).map_err(zerr)?,
            }
        }
        let mut arrays = Vec::with_capacity(lay.len());
        for (ov, attrs) in lay.iter().zip(&meta.var_attrs) {
            let mut aj = attrs_json(attrs);
            aj.remove("_FillValue");
            aj.remove("missing_value");
            let m = array_json(v2, &ov.shape, &ov.chunks, ov.dtype, &ov.dims, &aj, compress);
            record(&ov.name, &m);
            arrays.push(make_array(&store, &format!("/{}", ov.name), m, v2)?);
        }
        if v2 {
            write_json(
                &*store,
                ".zmetadata",
                &json!({"metadata": consolidated, "zarr_consolidated_format": 1}),
            )?;
        } else {
            // root group with consolidated metadata (as zarr-python 3 writes it)
            write_json(
                &*store,
                "zarr.json",
                &json!({
                    "zarr_format": 3,
                    "node_type": "group",
                    "attributes": global,
                    "consolidated_metadata": {"kind": "inline", "must_understand": false, "metadata": consolidated},
                }),
            )?;
        }
        Ok(Self {
            arrays,
            vars: lay.to_vec(),
            fs: None,
        })
    }
}

impl Writer for ZarrWriter {
    fn ordered(&self) -> bool {
        false
    }

    fn write(&self, var: usize, origin: &[usize], shape: &[usize], data: Values) -> Result<()> {
        if crate::exec::publish::cancelled() {
            return Err(Error::internal("cancelled"));
        }
        let ov = &self.vars[var];
        let idx: Vec<u64> = origin
            .iter()
            .zip(&ov.chunks)
            .map(|(&o, &c)| (o / c) as u64)
            .collect();
        let a = &self.arrays[var];
        match data {
            Values::F32(x) => a
                .store_chunk(&idx, pad(&x, shape, &ov.chunks, f32::NAN))
                .map_err(zerr),
            Values::F64(x) => a
                .store_chunk(&idx, pad(&x, shape, &ov.chunks, f64::NAN))
                .map_err(zerr),
        }
    }

    /// Makes the written store durable (before `exec::publish` renames it into place).
    fn finish(&self) -> Result<()> {
        match &self.fs {
            Some(fs) => fs.sync(),
            None => Ok(()),
        }
    }
}
