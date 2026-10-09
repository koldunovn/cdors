//! NetCDF (3 and 4) through netCDF-C, serially.
//!
//! The spack HDF5 under netCDF-C is not thread-safe, so every netCDF call goes through the
//! `netcdf` crate's global lock, and the open file sits behind a mutex. Chunks are the HDF5
//! chunks of NetCDF-4 variables; for contiguous storage (NetCDF-3 or unchunked NetCDF-4) a chunk
//! is one step of the first dimension (one record) when the variable has two or more
//! dimensions, otherwise the whole variable. Chunks come back already decoded.
//!
//! What opening reads through netCDF-C (dimensions, variables, attributes and the coordinate
//! values `classify` asks for) is kept as an [`NcHeader`]. The NetCDF-4 reader caches it with the
//! file's chunk index, and [`NetcdfSource::from_header`] rebuilds the source from it without
//! netCDF-C: on many files, opening each through netCDF-C one after another dominated planning.

use super::{ChunkGrid, ChunkSource, DecodedChunk, RawChunk, Values};
use crate::error::{Error, Result};
use crate::model::{
    self, AttrValue, Attrs, DType, Dataset, DimRole, Encoding, Format, VarDim, VarKind, Variable,
};
use netcdf::AttributeValue;
use netcdf::types::NcVariableType;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Mutex;

/// Version of [`NcHeader`]; a cached header of another version is ignored.
const HEADER_FORMAT: u32 = 1;

/// Largest number of coordinate values kept in a header (regular and small curvilinear grids,
/// time and bounds). Files with more (unstructured grids) open through netCDF-C every time.
const HEADER_MAX_VALUES: usize = 1 << 18;

/// What [`NetcdfSource::open`] reads through netCDF-C, enough to rebuild the source without the
/// file. Floats are kept as bit patterns, so NaN and the exact values survive JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NcHeader {
    format: u32,
    attrs: Vec<(String, HeaderAttr)>,
    vars: Vec<HeaderVar>,
    /// The variables `classify` read while opening, as f64 bit patterns.
    values: Vec<(String, Vec<u64>)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HeaderVar {
    name: String,
    dtype: DType,
    dims: Vec<(String, usize)>,
    chunks: Vec<usize>,
    attrs: Vec<(String, HeaderAttr)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum HeaderAttr {
    Text(String),
    Ints(Vec<i64>),
    F32s(Vec<u64>),
    F64s(Vec<u64>),
}

fn to_header(attrs: &Attrs) -> Vec<(String, HeaderAttr)> {
    let bits = |v: &[f64]| v.iter().map(|x| x.to_bits()).collect();
    attrs
        .iter()
        .map(|(k, v)| {
            let h = match v {
                AttrValue::Text(s) => HeaderAttr::Text(s.clone()),
                AttrValue::Ints(x) => HeaderAttr::Ints(x.clone()),
                AttrValue::F32s(x) => HeaderAttr::F32s(bits(x)),
                AttrValue::F64s(x) => HeaderAttr::F64s(bits(x)),
            };
            (k.clone(), h)
        })
        .collect()
}

fn from_header(attrs: &[(String, HeaderAttr)]) -> Attrs {
    let floats = |v: &[u64]| v.iter().map(|&b| f64::from_bits(b)).collect();
    Attrs(
        attrs
            .iter()
            .map(|(k, h)| {
                let v = match h {
                    HeaderAttr::Text(s) => AttrValue::Text(s.clone()),
                    HeaderAttr::Ints(x) => AttrValue::Ints(x.clone()),
                    HeaderAttr::F32s(x) => AttrValue::F32s(floats(x)),
                    HeaderAttr::F64s(x) => AttrValue::F64s(floats(x)),
                };
                (k.clone(), v)
            })
            .collect(),
    )
}

/// A NetCDF file opened for reading.
pub struct NetcdfSource {
    ds: Dataset,
    path: String,
    /// The open file; `None` until a read needs it when the source was rebuilt from a header.
    file: Mutex<Option<netcdf::File>>,
    grids: HashMap<String, (ChunkGrid, DType, Encoding)>,
    /// What opening read, for a cache (`None` when rebuilt from a header, or too large).
    header: Option<NcHeader>,
    /// Whole variables known without the file (rebuilt from a header): the coordinates and
    /// time values `classify` read, which planning reads again.
    known: HashMap<String, Vec<f64>>,
}

fn dtype_of(t: &NcVariableType) -> DType {
    use netcdf::types::{FloatType, IntType};
    match t {
        NcVariableType::Int(i) => match i {
            IntType::I8 => DType::I8,
            IntType::I16 => DType::I16,
            IntType::I32 => DType::I32,
            IntType::I64 => DType::I64,
            IntType::U8 => DType::U8,
            IntType::U16 => DType::U16,
            IntType::U32 => DType::U32,
            IntType::U64 => DType::U64,
        },
        NcVariableType::Float(FloatType::F32) => DType::F32,
        NcVariableType::Float(FloatType::F64) => DType::F64,
        _ => DType::Other,
    }
}

fn attr_value(v: AttributeValue) -> Option<AttrValue> {
    use AttributeValue as A;
    let ints = |v: Vec<i64>| Some(AttrValue::Ints(v));
    match v {
        A::Str(s) => Some(AttrValue::Text(s)),
        A::Strs(s) => Some(AttrValue::Text(s.join(" "))),
        A::Uchar(x) => ints(vec![x as i64]),
        A::Uchars(x) => ints(x.into_iter().map(i64::from).collect()),
        A::Schar(x) => ints(vec![x as i64]),
        A::Schars(x) => ints(x.into_iter().map(i64::from).collect()),
        A::Ushort(x) => ints(vec![x as i64]),
        A::Ushorts(x) => ints(x.into_iter().map(i64::from).collect()),
        A::Short(x) => ints(vec![x as i64]),
        A::Shorts(x) => ints(x.into_iter().map(i64::from).collect()),
        A::Uint(x) => ints(vec![x as i64]),
        A::Uints(x) => ints(x.into_iter().map(i64::from).collect()),
        A::Int(x) => ints(vec![x as i64]),
        A::Ints(x) => ints(x.into_iter().map(i64::from).collect()),
        A::Ulonglong(x) => ints(vec![x as i64]),
        A::Ulonglongs(x) => ints(x.into_iter().map(|y| y as i64).collect()),
        A::Longlong(x) => ints(vec![x]),
        A::Longlongs(x) => ints(x),
        A::Float(x) => Some(AttrValue::F32s(vec![x as f64])),
        A::Floats(x) => Some(AttrValue::F32s(x.into_iter().map(f64::from).collect())),
        A::Double(x) => Some(AttrValue::F64s(vec![x])),
        A::Doubles(x) => Some(AttrValue::F64s(x)),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

fn read_attrs<'a>(it: impl Iterator<Item = netcdf::Attribute<'a>>) -> Result<Attrs> {
    let mut out = Vec::new();
    for a in it {
        if let Some(v) = attr_value(a.value()?) {
            out.push((a.name().to_owned(), v));
        }
    }
    Ok(Attrs(out))
}

/// Reads a hyperslab of `var` as values.
fn read_slab(
    var: &netcdf::Variable<'_>,
    dtype: DType,
    enc: &Encoding,
    origin: &[usize],
    shape: &[usize],
) -> Result<Values> {
    let ext: Vec<netcdf::Extent> = origin
        .iter()
        .zip(shape)
        .map(|(&o, &n)| (o..o + n).into())
        .collect();
    let ext: netcdf::Extents = if ext.is_empty() {
        (..).into()
    } else {
        ext.into()
    };
    macro_rules! get {
        ($t:ty) => {
            super::convert_vec(var.get_values::<$t, _>(ext)?, enc)
        };
    }
    Ok(match dtype {
        DType::I8 => get!(i8),
        DType::I16 => get!(i16),
        DType::I32 => get!(i32),
        DType::I64 => get!(i64),
        DType::U8 => get!(u8),
        DType::U16 => get!(u16),
        DType::U32 => get!(u32),
        DType::U64 => get!(u64),
        DType::F32 => get!(f32),
        DType::F64 => get!(f64),
        DType::Other => {
            return Err(Error::bad_data(format!(
                "variable '{}' has a non-numeric type",
                var.name()
            )));
        }
    })
}

impl NetcdfSource {
    /// Opens a NetCDF file (metadata only; coordinate variables are read to build grids and time).
    pub fn open(path: &str) -> Result<Self> {
        let _hdf5 = super::hdf5_lock();
        let file = netcdf::open(path).map_err(|e| {
            Error::bad_data(format!("cannot open '{path}' as NetCDF: {e}"))
                .with("path", path)
                .with_hint("cdors reads NetCDF files and Zarr stores (directories, *.zarr)")
        })?;
        let attrs = to_header(&read_attrs(file.attributes())?);
        let mut vars = Vec::new();
        for v in file.variables() {
            let dims: Vec<(String, usize)> =
                v.dimensions().iter().map(|d| (d.name(), d.len())).collect();
            let shape: Vec<usize> = dims.iter().map(|d| d.1).collect();
            let chunks = match v.chunking()? {
                Some(c) if c.len() == shape.len() => c,
                _ => shape
                    .iter()
                    .enumerate()
                    .map(|(i, &n)| if i == 0 && shape.len() >= 2 { 1 } else { n })
                    .collect(),
            };
            vars.push(HeaderVar {
                name: v.name(),
                dtype: dtype_of(&v.vartype()),
                dims,
                chunks,
                attrs: to_header(&read_attrs(v.attributes())?),
            });
        }
        let mut src = Self::assemble(path, &attrs, &vars, Some(file));
        // classify reads coordinates; the values go into the header
        let read: RefCell<Vec<(String, Vec<u64>)>> = RefCell::new(Vec::new());
        let mut ds = src.ds.clone();
        model::classify(&mut ds, &|n| {
            let v = src.read_var(n)?;
            let mut r = read.borrow_mut();
            if !r.iter().any(|(k, _)| k == n) {
                r.push((n.to_owned(), v.iter().map(|x| x.to_bits()).collect()));
            }
            Ok(v)
        })?;
        src.ds = ds;
        let values = read.into_inner();
        if values.iter().map(|(_, v)| v.len()).sum::<usize>() <= HEADER_MAX_VALUES {
            src.header = Some(NcHeader {
                format: HEADER_FORMAT,
                attrs,
                vars,
                values,
            });
        }
        Ok(src)
    }

    /// Rebuilds the source from a header made by [`Self::open`] for this very file (the caller
    /// checks that), without netCDF-C. The file is opened through netCDF-C only if something is
    /// read later. `None` if the header has another format.
    pub fn from_header(path: &str, header: &NcHeader) -> Result<Option<Self>> {
        if header.format != HEADER_FORMAT {
            return Ok(None);
        }
        let mut src = Self::assemble(path, &header.attrs, &header.vars, None);
        src.known = header
            .values
            .iter()
            .map(|(k, bits)| (k.clone(), bits.iter().map(|&b| f64::from_bits(b)).collect()))
            .collect();
        let mut ds = src.ds.clone();
        model::classify(&mut ds, &|n| src.read_var(n))?;
        src.ds = ds;
        Ok(Some(src))
    }

    /// What opening read through netCDF-C (`None` if rebuilt from a header, or too large).
    pub fn header(&self) -> Option<&NcHeader> {
        self.header.as_ref()
    }

    /// The source before `classify`: dimensions, variables and chunk grids.
    fn assemble(
        path: &str,
        attrs: &[(String, HeaderAttr)],
        hvars: &[HeaderVar],
        file: Option<netcdf::File>,
    ) -> Self {
        let mut dims: Vec<(String, usize)> = Vec::new();
        let mut vars = Vec::new();
        let mut grids = HashMap::new();
        for hv in hvars {
            for (n, s) in &hv.dims {
                if !dims.iter().any(|(m, _)| m == n) {
                    dims.push((n.clone(), *s));
                }
            }
            let attrs = from_header(&hv.attrs);
            let encoding = super::encoding_from_attrs(&attrs, hv.dtype, None);
            grids.insert(
                hv.name.clone(),
                (
                    ChunkGrid {
                        shape: hv.dims.iter().map(|d| d.1).collect(),
                        chunk_shape: hv.chunks.iter().map(|&c| c.max(1)).collect(),
                    },
                    hv.dtype,
                    encoding.clone(),
                ),
            );
            vars.push(Variable {
                name: hv.name.clone(),
                kind: VarKind::Data,
                dtype: hv.dtype,
                dims: hv
                    .dims
                    .iter()
                    .map(|(n, s)| VarDim {
                        name: n.clone(),
                        size: *s,
                        role: DimRole::Other,
                    })
                    .collect(),
                attrs,
                chunks: hv.chunks.clone(),
                encoding,
                grid: None,
                zaxis: None,
            });
        }
        Self {
            ds: Dataset {
                source: path.to_owned(),
                format: Format::NetCdf,
                attrs: from_header(attrs),
                dims,
                vars,
                grids: Vec::new(),
                zaxes: Vec::new(),
                time: None,
            },
            path: path.to_owned(),
            file: Mutex::new(file),
            grids,
            header: None,
            known: HashMap::new(),
        }
    }

    fn entry(&self, var: &str) -> Result<&(ChunkGrid, DType, Encoding)> {
        self.grids.get(var).ok_or_else(|| {
            Error::bad_arguments(format!(
                "variable '{var}' not found in '{}'",
                self.ds.source
            ))
        })
    }

    fn read(&self, var: &str, origin: &[usize], shape: &[usize]) -> Result<Values> {
        let (_, dtype, enc) = self.entry(var)?;
        let _hdf5 = super::hdf5_lock();
        let mut file = self
            .file
            .lock()
            .map_err(|_| Error::internal("netCDF file lock poisoned"))?;
        if file.is_none() {
            let path = self.path.as_str();
            *file = Some(netcdf::open(path).map_err(|e| {
                Error::bad_data(format!("cannot open '{path}' as NetCDF: {e}")).with("path", path)
            })?);
        }
        let v = file
            .as_ref()
            .and_then(|f| f.variable(var))
            .ok_or_else(|| Error::internal(format!("variable '{var}' vanished")))?;
        read_slab(&v, *dtype, enc, origin, shape)
    }
}

impl ChunkSource for NetcdfSource {
    fn dataset(&self) -> &Dataset {
        &self.ds
    }

    fn chunk_grid(&self, var: &str) -> Result<ChunkGrid> {
        Ok(self.entry(var)?.0.clone())
    }

    fn read_chunk(&self, var: &str, indices: &[u64]) -> Result<RawChunk> {
        let grid = &self.entry(var)?.0;
        grid.check(var, indices)?;
        let origin = grid.origin(indices);
        let shape = grid.extent(indices);
        let o: Vec<usize> = origin.iter().map(|&x| x as usize).collect();
        let values = self.read(var, &o, &shape)?;
        Ok(RawChunk::Decoded(DecodedChunk {
            origin,
            shape,
            values,
        }))
    }

    fn read_var(&self, var: &str) -> Result<Vec<f64>> {
        if let Some(v) = self.known.get(var) {
            return Ok(v.clone());
        }
        let shape = self.entry(var)?.0.shape.clone();
        let origin = vec![0; shape.len()];
        Ok(self.read(var, &origin, &shape)?.to_f64())
    }
}
