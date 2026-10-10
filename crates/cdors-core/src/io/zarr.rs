//! Zarr v2 and v3 stores on the filesystem, through `zarrs`.
//!
//! Metadata comes from consolidated metadata when present (`.zmetadata` for v2,
//! `consolidated_metadata` in the root `zarr.json` for v3), otherwise from the arrays found in the
//! store's root directory. Only arrays directly under the root group are read (xarray layout).
//! Dimension names come from `_ARRAY_DIMENSIONS` (v2, xarray) or `dimension_names` (v3).
//!
//! Missing values: `_FillValue`/`missing_value` attributes; for v2 also the array's
//! `fill_value`, which is where xarray stores `_FillValue` in Zarr v2.
//!
//! Zarr v2 arrays whose compressor is gribscan's `gribscan.rawgrib` (kerchunk references to GRIB
//! files) store one GRIB message per chunk; cdors decodes the messages itself ([`super::grib`])
//! and restores the grid gribscan flattened ([`gribscan`]).

mod gribscan;

use super::{ChunkDecoder, ChunkGrid, ChunkSource, DecodedChunk, EncodedChunk, RawChunk, Values};
use crate::error::{Error, Result};
use crate::model::{
    self, AttrValue, Attrs, DType, Dataset, DimRole, Encoding, Format, VarDim, VarKind, Variable,
};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use zarrs::array::{
    Array, ArrayBytes, ArrayMetadata, ArrayMetadataV2, ArrayMetadataV3, ArrayToBytesCodecTraits,
    CodecOptions,
};
use zarrs::filesystem::FilesystemStore;
use zarrs::storage::{ReadableStorageTraits, StoreKey};

type Store = dyn ReadableStorageTraits;

/// One Zarr array with its cdors data type and CF encoding.
struct ZarrVar {
    array: Array<Store>,
    dtype: DType,
    encoding: Encoding,
    grid: ChunkGrid,
    /// The codec chain is just c-blosc over native-order, C-order elements: chunks are
    /// decompressed straight into the value buffer, bypassing `zarrs`' intermediate copies.
    direct_blosc: bool,
    /// Chunks are GRIB messages (`gribscan.rawgrib`); `zarrs` sees the array without a codec.
    grib: bool,
    /// The last stored dimension is presented as two (`value` as `lat, lon`, see [`gribscan`]),
    /// both one chunk: presented chunk indices have one more trailing 0 than the stored ones.
    split: bool,
}

fn zerr(e: impl std::fmt::Display) -> Error {
    Error::bad_data(format!("zarr: {e}"))
}

/// The part of a full chunk (`full`, C order) inside the array extent `ext`; the buffer itself
/// when the chunk is not cut by the array edge.
pub(crate) fn trim<T: Copy>(v: Vec<T>, full: &[usize], ext: &[usize]) -> Vec<T> {
    if full == ext {
        return v;
    }
    let n: usize = ext.iter().product();
    let mut out = Vec::with_capacity(n);
    if n == 0 {
        return out;
    }
    let nd = full.len();
    let row = ext[nd - 1];
    let mut idx = vec![0usize; nd.saturating_sub(1)];
    loop {
        let mut off = 0;
        for d in 0..nd - 1 {
            off = off * full[d] + idx[d];
        }
        off *= full[nd - 1];
        out.extend_from_slice(&v[off..off + row]);
        // odometer over the leading dimensions
        let mut d = nd - 1;
        loop {
            if d == 0 {
                return out;
            }
            d -= 1;
            idx[d] += 1;
            if idx[d] < ext[d] {
                break;
            }
            idx[d] = 0;
        }
    }
}

impl ZarrVar {
    /// The stored chunk indices of presented chunk `indices`.
    fn stored<'a>(&self, indices: &'a [u64]) -> &'a [u64] {
        if self.split {
            &indices[..indices.len() - 1]
        } else {
            indices
        }
    }

    fn decode_bytes(&self, indices: &[u64], bytes: Option<&[u8]>) -> Result<Values> {
        if self.grib
            && let Some(b) = bytes
        {
            let v = super::grib::decode(b).map_err(|e| Error::bad_data(format!("GRIB: {e}")))?;
            let n: usize = self.grid.chunk_shape.iter().product();
            if v.len() != n {
                return Err(Error::bad_data(format!(
                    "GRIB message with {} values in a chunk of {n}",
                    v.len()
                )));
            }
            return Ok(super::convert_vec(v, &self.encoding));
        }
        if self.direct_blosc
            && let Some(b) = bytes
        {
            let n: usize = self.grid.chunk_shape.iter().product();
            macro_rules! direct {
                ($t:ty) => {
                    super::blosc_decompress_into::<$t>(b, n)
                        .map(|v| super::convert_vec(v, &self.encoding))
                };
            }
            let v = match self.dtype {
                DType::I8 => direct!(i8),
                DType::I16 => direct!(i16),
                DType::I32 => direct!(i32),
                DType::I64 => direct!(i64),
                DType::U8 => direct!(u8),
                DType::U16 => direct!(u16),
                DType::U32 => direct!(u32),
                DType::U64 => direct!(u64),
                DType::F32 => direct!(f32),
                DType::F64 => direct!(f64),
                DType::Other => None,
            };
            // anything unexpected (size mismatch, invalid frame): zarrs decodes and reports it
            if let Some(v) = v {
                return Ok(v);
            }
        }
        let shape = self.array.chunk_shape(self.stored(indices)).map_err(zerr)?;
        let dt = self.array.data_type();
        let fv = self.array.fill_value();
        let ab = match bytes {
            Some(b) => self
                .array
                .codecs()
                .decode(Cow::Borrowed(b), &shape, dt, fv, &CodecOptions::default())
                .map_err(zerr)?,
            None => {
                let n: u64 = shape.iter().map(|x| x.get()).product();
                ArrayBytes::new_fill_value(dt, n, fv).map_err(zerr)?
            }
        };
        let raw = ab.into_fixed().map_err(zerr)?;
        super::convert_bytes(self.dtype, &raw, &self.encoding)
    }
}

impl ChunkDecoder for ZarrVar {
    fn decode(&self, indices: &[u64], bytes: Option<&[u8]>) -> Result<DecodedChunk> {
        let values = self.decode_bytes(indices, bytes)?;
        let full = &self.grid.chunk_shape;
        let ext = self.grid.extent(indices);
        let values = match values {
            Values::F32(v) => Values::F32(trim(v, full, &ext)),
            Values::F64(v) => Values::F64(trim(v, full, &ext)),
        };
        Ok(DecodedChunk {
            origin: self.grid.origin(indices),
            shape: ext,
            values,
        })
    }

    fn codecs(&self) -> String {
        if self.grib {
            return "gribscan.rawgrib".to_owned();
        }
        codec_names(self.array.metadata()).join(",")
    }
}

/// Whether chunks of an array with metadata `m` (as JSON) are one c-blosc frame of
/// little-endian, C-order elements of a numeric type, with no other codec: Zarr v2 with a blosc
/// compressor and no filters, or Zarr v3 with exactly the codecs `bytes` (little endian) and
/// `blosc`. Such chunks can be decompressed straight into the value buffer.
fn direct_blosc(m: &Value, dtype: DType) -> bool {
    if !cfg!(target_endian = "little") || dtype == DType::Other {
        return false;
    }
    let name = |c: &Value| -> Option<String> {
        c.get("name")
            .or_else(|| c.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    if let Some(cs) = m.get("codecs").and_then(Value::as_array) {
        // v3
        let [bytes, blosc] = cs.as_slice() else {
            return false;
        };
        let endian = bytes
            .pointer("/configuration/endian")
            .and_then(Value::as_str);
        name(bytes).as_deref() == Some("bytes")
            && matches!(endian, None | Some("little"))
            && name(blosc).as_deref() == Some("blosc")
    } else {
        // v2
        let dt = m.get("dtype").and_then(Value::as_str).unwrap_or("");
        let no_filters = match m.get("filters") {
            None | Some(Value::Null) => true,
            Some(Value::Array(a)) => a.is_empty(),
            Some(_) => false,
        };
        (dt.starts_with('<') || dt.starts_with('|'))
            && m.get("order").and_then(Value::as_str) == Some("C")
            && no_filters
            && m.get("compressor").and_then(name).as_deref() == Some("blosc")
    }
}

/// Codec names from the array metadata (v3 `codecs`, v2 `filters` + `compressor`).
fn codec_names(m: &ArrayMetadata) -> Vec<String> {
    let v = serde_json::to_value(m).unwrap_or(Value::Null);
    let name = |c: &Value| -> Option<String> {
        c.get("name")
            .or_else(|| c.get("id"))
            .and_then(Value::as_str)
            .map(|s| {
                let cname = c
                    .pointer("/configuration/cname")
                    .or_else(|| c.get("cname"))
                    .and_then(Value::as_str);
                match cname {
                    Some(cn) => format!("{s}:{cn}"),
                    None => s.to_owned(),
                }
            })
    };
    let mut out = Vec::new();
    if let Some(cs) = v.get("codecs").and_then(Value::as_array) {
        for c in cs {
            out.extend(name(c));
            // sharding: list the inner codecs too
            if let Some(inner) = c.pointer("/configuration/codecs").and_then(Value::as_array) {
                out.extend(inner.iter().filter_map(name).map(|n| format!("shard/{n}")));
            }
        }
    } else {
        if let Some(fs) = v.get("filters").and_then(Value::as_array) {
            out.extend(fs.iter().filter_map(name));
        }
        if let Some(c) = v.get("compressor").filter(|c| !c.is_null()) {
            out.extend(name(c));
        }
    }
    out
}

/// A Zarr store opened for reading.
pub struct ZarrSource {
    ds: Dataset,
    vars: HashMap<String, Arc<ZarrVar>>,
    /// Coordinates computed when the store was opened (restored gribscan grids), served
    /// instead of the stored arrays of the same name.
    synthetic: HashMap<String, Vec<f64>>,
}

fn json_attrs(m: &Map<String, Value>) -> Attrs {
    Attrs(
        m.iter()
            .filter(|(k, _)| k.as_str() != "_ARRAY_DIMENSIONS")
            .map(|(k, v)| (k.clone(), AttrValue::from_json(v)))
            .collect(),
    )
}

/// Standard base64 (with padding) as used by xarray for `_FillValue` in Zarr v3 attributes.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let b = s.trim().trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(b.len() * 3 / 4);
    for chunk in b.chunks(4) {
        let mut acc = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= val(c)? << (18 - 6 * i);
        }
        let n = chunk.len().checked_sub(1)?;
        out.extend_from_slice(&acc.to_be_bytes()[1..1 + n]);
    }
    Some(out)
}

/// xarray writes `_FillValue` of Zarr v3 arrays as base64 of the little-endian value bytes.
/// Turns such a string into a number (in the array's type, or f64/f32 by length).
fn decode_fill_attr(v: &AttrValue, dtype: DType) -> Option<AttrValue> {
    let s = v.as_str()?;
    if s.trim().parse::<f64>().is_ok() {
        return None;
    }
    let b = base64_decode(s)?;
    let f = match (b.len(), dtype) {
        (8, DType::I64) => i64::from_le_bytes(b.try_into().ok()?) as f64,
        (8, DType::U64) => u64::from_le_bytes(b.try_into().ok()?) as f64,
        (8, _) => f64::from_le_bytes(b.try_into().ok()?),
        (4, DType::I32) => i32::from_le_bytes(b.try_into().ok()?) as f64,
        (4, DType::U32) => u32::from_le_bytes(b.try_into().ok()?) as f64,
        (4, _) => f32::from_le_bytes(b.try_into().ok()?) as f64,
        (2, DType::U16) => u16::from_le_bytes(b.try_into().ok()?) as f64,
        (2, _) => i16::from_le_bytes(b.try_into().ok()?) as f64,
        (1, DType::U8) => b[0] as f64,
        (1, _) => b[0] as i8 as f64,
        _ => return None,
    };
    Some(AttrValue::F64s(vec![f]))
}

fn fill_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => match s.as_str() {
            "NaN" => Some(f64::NAN),
            "Infinity" => Some(f64::INFINITY),
            "-Infinity" => Some(f64::NEG_INFINITY),
            _ => None,
        },
        _ => None,
    }
}

impl ZarrSource {
    /// Opens a store (metadata only; coordinate variables are read to build grids and time).
    pub fn open(path: &str) -> Result<Self> {
        let root = path.trim_end_matches('/');
        let fs = FilesystemStore::new(root)
            .map_err(|e| Error::io(format!("cannot open Zarr store '{root}': {e}")))?;
        let list_dirs = || -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(root)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !n.starts_with('.'))
                .collect();
            names.sort();
            names
        };
        Self::open_store(root, Arc::new(fs), &list_dirs)
    }

    /// Opens a Zarr hierarchy from any `zarrs` store (a filesystem directory or kerchunk
    /// references). `root` names the dataset; `list_dirs` lists the root group's members and is
    /// used only when there is no consolidated metadata.
    pub(crate) fn open_store(
        root: &str,
        store: Arc<Store>,
        list_dirs: &dyn Fn() -> Vec<String>,
    ) -> Result<Self> {
        let get = |key: &str| -> Result<Option<Value>> {
            let k = StoreKey::new(key).map_err(zerr)?;
            match store.get(&k).map_err(|e| Error::io(format!("zarr: {e}")))? {
                Some(b) => Ok(Some(serde_json::from_slice(&b).map_err(|e| {
                    Error::bad_data(format!("invalid JSON in '{root}/{key}': {e}"))
                })?)),
                None => Ok(None),
            }
        };
        // (name, metadata, attrs-for-v2) per array, plus global attributes
        let mut arrays: Vec<(String, ArrayMetadata)> = Vec::new();
        let mut grib_arrays: Vec<String> = Vec::new();
        let format;
        let global: Map<String, Value>;
        if let Some(rootmeta) = get("zarr.json")? {
            format = Format::Zarr3;
            if rootmeta.get("node_type").and_then(Value::as_str) == Some("array") {
                let name = std::path::Path::new(root)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("data")
                    .to_owned();
                let m: ArrayMetadataV3 = serde_json::from_value(rootmeta).map_err(zerr)?;
                arrays.push((name, ArrayMetadata::V3(m)));
                global = Map::new();
            } else {
                global = rootmeta
                    .get("attributes")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let consolidated = rootmeta
                    .pointer("/consolidated_metadata/metadata")
                    .and_then(Value::as_object);
                let members: Vec<(String, Value)> = match consolidated {
                    Some(c) => c.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    None => {
                        let mut v = Vec::new();
                        for n in list_dirs() {
                            if let Some(m) = get(&format!("{n}/zarr.json"))? {
                                v.push((n, m));
                            }
                        }
                        v
                    }
                };
                for (name, m) in members {
                    if name.contains('/')
                        || m.get("node_type").and_then(Value::as_str) != Some("array")
                    {
                        continue;
                    }
                    let m: ArrayMetadataV3 = serde_json::from_value(m)
                        .map_err(|e| zerr(format!("array '{name}': {e}")))?;
                    arrays.push((name, ArrayMetadata::V3(m)));
                }
            }
        } else {
            format = Format::Zarr2;
            let mut entries: Vec<(String, Value, Option<Value>)> = Vec::new();
            let mut gattrs = None;
            if let Some(zm) = get(".zmetadata")? {
                let meta = zm
                    .get("metadata")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                gattrs = meta.get(".zattrs").cloned();
                for (k, v) in &meta {
                    if let Some(name) = k.strip_suffix("/.zarray")
                        && !name.contains('/')
                    {
                        let at = meta.get(&format!("{name}/.zattrs")).cloned();
                        entries.push((name.to_owned(), v.clone(), at));
                    }
                }
                entries.sort_by(|a, b| a.0.cmp(&b.0));
            } else if let Some(za) = get(".zarray")? {
                let name = std::path::Path::new(root)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("data")
                    .to_owned();
                entries.push((name, za, get(".zattrs")?));
            } else {
                if get(".zgroup")?.is_none() {
                    return Err(Error::bad_data(format!(
                        "'{root}' is not a Zarr store (no zarr.json, .zgroup or .zarray)"
                    )));
                }
                gattrs = get(".zattrs")?;
                for n in list_dirs() {
                    if let Some(za) = get(&format!("{n}/.zarray"))? {
                        let at = get(&format!("{n}/.zattrs"))?;
                        entries.push((n, za, at));
                    }
                }
            }
            global = gattrs
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            for (name, mut za, at) in entries {
                if za.pointer("/compressor/id").and_then(Value::as_str) == Some("gribscan.rawgrib")
                {
                    za["compressor"] = Value::Null;
                    grib_arrays.push(name.clone());
                }
                let mut m: ArrayMetadataV2 =
                    serde_json::from_value(za).map_err(|e| zerr(format!("array '{name}': {e}")))?;
                if let Some(Value::Object(a)) = at {
                    m.attributes = a;
                }
                arrays.push((name, ArrayMetadata::V2(m)));
            }
        }

        let mut vars_map = HashMap::new();
        let mut vars = Vec::new();
        let mut dims: Vec<(String, usize)> = Vec::new();
        for (name, meta) in arrays {
            let raw = serde_json::to_value(&meta).map_err(zerr)?;
            let (dtype_name, v2_fill) = match &meta {
                ArrayMetadata::V2(_) => (
                    raw.get("dtype").and_then(Value::as_str).unwrap_or(""),
                    raw.get("fill_value").and_then(fill_to_f64),
                ),
                ArrayMetadata::V3(_) => (
                    raw.get("data_type").and_then(Value::as_str).unwrap_or(""),
                    None,
                ),
            };
            let dtype = DType::from_name(dtype_name);
            let array = match Array::new_with_metadata(store.clone(), &format!("/{name}"), meta) {
                Ok(a) => a,
                Err(e) => {
                    // unsupported data type or codec: skip the array, keep the dataset usable
                    crate::exec::threads::warn(
                        "array_skipped",
                        &format!("skipping Zarr array '{name}': {e}"),
                    );
                    continue;
                }
            };
            let attrs_json = array.attributes().clone();
            let mut attrs = json_attrs(&attrs_json);
            for (k, v) in &mut attrs.0 {
                if (k == "_FillValue" || k == "missing_value")
                    && let Some(d) = decode_fill_attr(v, dtype)
                {
                    *v = d;
                }
            }
            let shape: Vec<usize> = array.shape().iter().map(|&x| x as usize).collect();
            let dim_names: Vec<String> = match array.dimension_names() {
                Some(dn) => dn
                    .iter()
                    .enumerate()
                    .map(|(i, d)| d.clone().unwrap_or_else(|| format!("{name}_dim{i}")))
                    .collect(),
                None => match attrs_json
                    .get("_ARRAY_DIMENSIONS")
                    .and_then(Value::as_array)
                {
                    Some(a) => a
                        .iter()
                        .map(|d| d.as_str().unwrap_or("").to_owned())
                        .collect(),
                    None => (0..shape.len()).map(|i| format!("{name}_dim{i}")).collect(),
                },
            };
            if dim_names.len() != shape.len() {
                return Err(Error::bad_data(format!(
                    "array '{name}': {} dimension names for {} dimensions",
                    dim_names.len(),
                    shape.len()
                )));
            }
            let zero = vec![0u64; shape.len()];
            let chunk_shape: Vec<usize> = array
                .chunk_shape(&zero)
                .map_err(zerr)?
                .iter()
                .map(|x| x.get() as usize)
                .collect();
            for (d, &s) in dim_names.iter().zip(&shape) {
                match dims.iter().find(|(n, _)| n == d) {
                    Some(&(_, s0)) if s0 != s => {
                        return Err(Error::bad_data(format!(
                            "dimension '{d}' has sizes {s0} and {s} in '{root}'"
                        )));
                    }
                    Some(_) => {}
                    None => dims.push((d.clone(), s)),
                }
            }
            let encoding = super::encoding_from_attrs(&attrs, dtype, v2_fill);
            let grid = ChunkGrid {
                shape: shape.clone(),
                chunk_shape: chunk_shape.clone(),
            };
            vars.push(Variable {
                name: name.clone(),
                kind: VarKind::Data,
                dtype,
                dims: dim_names
                    .iter()
                    .zip(&shape)
                    .map(|(n, &s)| VarDim {
                        name: n.clone(),
                        size: s,
                        role: DimRole::Other,
                    })
                    .collect(),
                attrs,
                chunks: chunk_shape,
                encoding: encoding.clone(),
                grid: None,
                zaxis: None,
            });
            let grib = grib_arrays.contains(&name);
            vars_map.insert(
                name,
                Arc::new(ZarrVar {
                    direct_blosc: direct_blosc(&raw, dtype),
                    grib,
                    split: false,
                    array,
                    dtype,
                    encoding,
                    grid,
                }),
            );
        }

        let mut src = Self {
            ds: Dataset {
                source: root.to_owned(),
                format,
                attrs: json_attrs(&global),
                dims,
                vars,
                grids: Vec::new(),
                zaxes: Vec::new(),
                time: None,
            },
            vars: vars_map,
            synthetic: HashMap::new(),
        };
        let reduced = if grib_arrays.is_empty() {
            Vec::new()
        } else {
            gribscan::restore(&mut src, &grib_arrays)?
        };
        let mut ds = std::mem::replace(
            &mut src.ds,
            Dataset {
                source: String::new(),
                format,
                attrs: Attrs::default(),
                dims: Vec::new(),
                vars: Vec::new(),
                grids: Vec::new(),
                zaxes: Vec::new(),
                time: None,
            },
        );
        model::classify(&mut ds, &|n| src.read_var(n))?;
        src.ds = ds;
        gribscan::set_reduced(&mut src, reduced);
        Ok(src)
    }

    /// A chunk whose file is missing (references outlive the files they point to, as the EERIE
    /// gribscan references of 2007-2014): which time step it holds and what to do.
    fn missing_file_hint(&self, var: &str, indices: &[u64], err: Error) -> Error {
        if err.code != crate::error::ErrorCode::MissingInput {
            return err;
        }
        let step = self
            .ds
            .var(var)
            .zip(self.ds.time.as_ref())
            .and_then(|(v, t)| {
                let d = v.dims.iter().position(|d| d.name == t.dim)?;
                let k = indices[d] as usize * self.vars.get(var)?.grid.chunk_shape[d];
                Some((k, t.steps.get(k)?.datetime.iso()))
            });
        let at = match step {
            Some((k, date)) => format!(" (time step {}, {date})", k + 1),
            None => String::new(),
        };
        err.with_hint(format!(
            "the store refers to a file that does not exist{at}; select the time steps whose \
             files exist (-seltimestep, -selyear, -seldate) or have the references fixed"
        ))
    }

    fn var(&self, name: &str) -> Result<&Arc<ZarrVar>> {
        self.vars.get(name).ok_or_else(|| {
            Error::bad_arguments(format!(
                "variable '{name}' not found in '{}'",
                self.ds.source
            ))
        })
    }
}

impl ChunkSource for ZarrSource {
    fn dataset(&self) -> &Dataset {
        &self.ds
    }

    fn chunk_grid(&self, var: &str) -> Result<ChunkGrid> {
        if let Some(v) = self.synthetic.get(var) {
            return Ok(ChunkGrid {
                shape: vec![v.len()],
                chunk_shape: vec![v.len().max(1)],
            });
        }
        Ok(self.var(var)?.grid.clone())
    }

    fn read_chunk(&self, var: &str, indices: &[u64]) -> Result<RawChunk> {
        if let Some(v) = self.synthetic.get(var) {
            self.chunk_grid(var)?.check(var, indices)?;
            return Ok(RawChunk::Decoded(DecodedChunk {
                origin: vec![0],
                shape: vec![v.len()],
                values: Values::F64(v.clone()),
            }));
        }
        let zv = self.var(var)?;
        zv.grid.check(var, indices)?;
        let bytes = zv
            .array
            .retrieve_encoded_chunk(zv.stored(indices))
            .map_err(|e| {
                let err = Error::io(format!("zarr: reading chunk {indices:?} of '{var}': {e}"))
                    .with("source", self.ds.source.as_str())
                    .with("variable", var)
                    .with("chunk", indices.to_vec());
                self.missing_file_hint(var, indices, err)
            })?;
        Ok(RawChunk::Encoded(EncodedChunk {
            indices: indices.to_vec(),
            bytes,
            decoder: zv.clone() as Arc<dyn ChunkDecoder>,
        }))
    }

    fn read_var(&self, var: &str) -> Result<Vec<f64>> {
        if let Some(v) = self.synthetic.get(var) {
            return Ok(v.clone());
        }
        let zv = self.var(var)?;
        if zv.grib {
            return Err(Error::bad_data(format!(
                "'{var}' is stored as GRIB messages and is read chunk by chunk only"
            )));
        }
        let ab: ArrayBytes<'static> = zv
            .array
            .retrieve_array_subset(&zv.array.subset_all())
            .map_err(|e| Error::io(format!("zarr: reading '{var}': {e}")))?;
        let raw = ab.into_fixed().map_err(zerr)?;
        Ok(super::convert_bytes(zv.dtype, &raw, &zv.encoding)?.to_f64())
    }

    fn codecs(&self, var: &str) -> Option<String> {
        if self.synthetic.contains_key(var) {
            return None;
        }
        self.vars.get(var).map(|v| ChunkDecoder::codecs(v.as_ref()))
    }

    fn stored_size(&self, var: &str, indices: &[u64]) -> Option<u64> {
        if self.synthetic.contains_key(var) {
            return None;
        }
        let zv = self.vars.get(var)?;
        let key = zv.array.chunk_key(zv.stored(indices));
        match zv.array.storage().size_key(&key) {
            Ok(n) => Some(n.unwrap_or(0)),
            Err(_) => None,
        }
    }
}
