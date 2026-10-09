//! NetCDF (3 and 4) through netCDF-C, serially.
//!
//! The spack HDF5 under netCDF-C is not thread-safe, so every netCDF call goes through the
//! `netcdf` crate's global lock, and the open file sits behind a mutex. Chunks are the HDF5
//! chunks of NetCDF-4 variables; for contiguous storage (NetCDF-3 or unchunked NetCDF-4) a chunk
//! is one step of the first dimension (one record) when the variable has two or more
//! dimensions, otherwise the whole variable. Chunks come back already decoded.

use super::{ChunkGrid, ChunkSource, DecodedChunk, RawChunk, Values};
use crate::error::{Error, Result};
use crate::model::{
    self, AttrValue, Attrs, DType, Dataset, DimRole, Encoding, Format, VarDim, VarKind, Variable,
};
use netcdf::AttributeValue;
use netcdf::types::NcVariableType;
use std::collections::HashMap;
use std::sync::Mutex;

/// A NetCDF file opened for reading.
pub struct NetcdfSource {
    ds: Dataset,
    file: Mutex<netcdf::File>,
    grids: HashMap<String, (ChunkGrid, DType, Encoding)>,
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
        let attrs = read_attrs(file.attributes())?;
        let mut dims: Vec<(String, usize)> = Vec::new();
        let mut vars = Vec::new();
        let mut grids = HashMap::new();
        for v in file.variables() {
            let name = v.name();
            let dtype = dtype_of(&v.vartype());
            let vdims: Vec<VarDim> = v
                .dimensions()
                .iter()
                .map(|d| VarDim {
                    name: d.name(),
                    size: d.len(),
                    role: DimRole::Other,
                })
                .collect();
            for d in &vdims {
                if !dims.iter().any(|(n, _)| n == &d.name) {
                    dims.push((d.name.clone(), d.size));
                }
            }
            let shape: Vec<usize> = vdims.iter().map(|d| d.size).collect();
            let chunks = match v.chunking()? {
                Some(c) if c.len() == shape.len() => c,
                _ => shape
                    .iter()
                    .enumerate()
                    .map(|(i, &n)| if i == 0 && shape.len() >= 2 { 1 } else { n })
                    .collect(),
            };
            let attrs = read_attrs(v.attributes())?;
            let encoding = super::encoding_from_attrs(&attrs, dtype, None);
            grids.insert(
                name.clone(),
                (
                    ChunkGrid {
                        shape: shape.clone(),
                        chunk_shape: chunks.iter().map(|&c| c.max(1)).collect(),
                    },
                    dtype,
                    encoding.clone(),
                ),
            );
            vars.push(Variable {
                name,
                kind: VarKind::Data,
                dtype,
                dims: vdims,
                attrs,
                chunks,
                encoding,
                grid: None,
                zaxis: None,
            });
        }
        let mut src = Self {
            ds: Dataset {
                source: path.to_owned(),
                format: Format::NetCdf,
                attrs,
                dims,
                vars,
                grids: Vec::new(),
                zaxes: Vec::new(),
                time: None,
            },
            file: Mutex::new(file),
            grids,
        };
        let mut ds = src.ds.clone();
        model::classify(&mut ds, &|n| src.read_var(n))?;
        src.ds = ds;
        Ok(src)
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
        let file = self
            .file
            .lock()
            .map_err(|_| Error::internal("netCDF file lock poisoned"))?;
        let v = file
            .variable(var)
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
        let shape = self.entry(var)?.0.shape.clone();
        let origin = vec![0; shape.len()];
        Ok(self.read(var, &origin, &shape)?.to_f64())
    }
}
