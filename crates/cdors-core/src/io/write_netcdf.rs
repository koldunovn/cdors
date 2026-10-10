//! NetCDF writer (NetCDF-4, or the 64-bit offset format for `-f nc`) through the `netcdf` crate.
//!
//! The layout follows what cdo writes, so that cdo reads the result with the same grid: CF
//! coordinate variables for regular grids, `coordinates` plus bounds for curvilinear and
//! unstructured grids, the grid-mapping variable for HEALPix, an unlimited time dimension with
//! the original time values, units, calendar and `time_bnds`. Missing values (NaN inside the
//! engine) become the variable's `_FillValue`, which is also written as `missing_value`.
//!
//! Every netCDF-C call goes through the `netcdf` crate's global lock (the spack HDF5 is not
//! thread-safe), and the pipeline writes chunks one at a time in canonical order. The pipeline
//! replaces NaN by the missing value while it assembles the chunks on its compute threads
//! (`missing_as`, `write_ready`), so the writer thread only calls HDF5.
//!
//! With `-z zip_N` the data variables carry HDF5's shuffle and deflate (level N) filters, which
//! every NetCDF-4 reader decodes, but HDF5 does not run them: through netCDF-C they would run on
//! the one writer thread, at 50-70 MB/s. netCDF-C defines the file and writes the coordinates
//! (those of 32 values or more compressed by HDF5, as cdo compresses them) and closes it; the
//! file is reopened through HDF5, and the chunks are shuffled and compressed on the write
//! threads, in parallel and in any order, and stored as they are with HDF5's direct chunk write
//! (`H5Dwrite_chunk`).

use crate::error::{Error, Result};
use crate::exec::{OutMeta, OutVar, Writer, out_meta};
use crate::io::Values;
use crate::io::write_zarr::pad;
use crate::model::{AttrValue, Attrs, DType};
use crate::plan::Plan;
use hdf5_metno::filters::Filter;
use netcdf::{AttributeValue, Extent, Extents, FileMut};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

pub struct NcWriter {
    file: Mutex<Option<FileMut>>,
    /// `-z zip`: the deflate level, and the file reopened through HDF5 for direct chunk writes.
    level: Option<u8>,
    direct: Mutex<Option<Direct>>,
    vars: Vec<OutVar>,
}

/// The HDF5 file and one dataset per data variable (closed by `finish`, or dropped on failure).
struct Direct {
    file: hdf5_metno::File,
    dsets: Vec<hdf5_metno::Dataset>,
}

fn h5err(path: &Path) -> impl Fn(hdf5_metno::Error) -> Error + '_ {
    move |e| Error::io(format!("HDF5: {e}")).with("path", path.display().to_string())
}

impl Direct {
    /// Opens the file netCDF-C has just written and closed, with the data variables extended to
    /// their full shape (the unlimited time dimension is still empty) and their filter pipeline
    /// checked: chunks are stored already filtered.
    fn open(path: &Path, lay: &[OutVar], level: u8) -> Result<Self> {
        let file = hdf5_metno::File::open_rw(path).map_err(h5err(path))?;
        let mut dsets = Vec::with_capacity(lay.len());
        for ov in lay {
            let ds = file.dataset(&ov.name).map_err(h5err(path))?;
            ds.resize(ov.shape.clone()).map_err(h5err(path))?;
            let filters = ds.filters();
            if filters != [Filter::Shuffle, Filter::Deflate(level)] {
                return Err(Error::internal(format!(
                    "netCDF-C defined the filters {filters:?} for '{}', not shuffle and deflate",
                    ov.name
                )));
            }
            dsets.push(ds);
        }
        Ok(Self { file, dsets })
    }
}

/// HDF5's byte shuffle of little-endian elements: all first bytes, then all second bytes, ...
fn shuffle<T: Copy, const N: usize>(x: &[T], le_bytes: fn(T) -> [u8; N]) -> Vec<u8> {
    let n = x.len();
    let mut out = vec![0u8; n * N];
    for (i, &v) in x.iter().enumerate() {
        for (k, b) in le_bytes(v).into_iter().enumerate() {
            out[k * n + i] = b;
        }
    }
    out
}

/// One chunk as HDF5's shuffle and deflate filters store it: padded to the full chunk shape with
/// the missing value (HDF5 stores edge chunks whole), byte-shuffled, then zlib-compressed.
fn encode_chunk(ov: &OutVar, shape: &[usize], data: Values, level: u8) -> Result<Vec<u8>> {
    let full = shape == ov.chunks.as_slice();
    let shuffled = match (ov.dtype, data) {
        (DType::F32, Values::F32(x)) if full => shuffle(&x, f32::to_le_bytes),
        (DType::F32, Values::F32(x)) => shuffle(
            &pad(&x, shape, &ov.chunks, ov.missval as f32),
            f32::to_le_bytes,
        ),
        (DType::F64, Values::F64(x)) if full => shuffle(&x, f64::to_le_bytes),
        (DType::F64, Values::F64(x)) => {
            shuffle(&pad(&x, shape, &ov.chunks, ov.missval), f64::to_le_bytes)
        }
        (t, _) => {
            return Err(Error::internal(format!(
                "chunk of '{}' does not have the variable's type {t:?}",
                ov.name
            )));
        }
    };
    let mut z = flate2::write::ZlibEncoder::new(
        Vec::with_capacity(shuffled.len() / 2),
        flate2::Compression::new(level.into()),
    );
    z.write_all(&shuffled)
        .and_then(|()| z.finish())
        .map_err(|e| Error::internal(format!("deflate: {e}")))
}

fn nc_attr(v: &AttrValue, classic: bool) -> AttributeValue {
    match v {
        AttrValue::Text(s) => AttributeValue::Str(s.clone()),
        AttrValue::Ints(x) => {
            if x.iter().all(|&i| i32::try_from(i).is_ok()) || classic {
                AttributeValue::Ints(x.iter().map(|&i| i as i32).collect())
            } else {
                AttributeValue::Longlongs(x.clone())
            }
        }
        AttrValue::F32s(x) => AttributeValue::Floats(x.iter().map(|&f| f as f32).collect()),
        AttrValue::F64s(x) => AttributeValue::Doubles(x.clone()),
    }
}

fn put_attrs(var: &mut netcdf::VariableMut<'_>, attrs: &Attrs, classic: bool) -> Result<()> {
    for (k, v) in attrs.iter() {
        var.put_attribute(k, nc_attr(v, classic))?;
    }
    Ok(())
}

fn extents(origin: &[usize], shape: &[usize]) -> Extents {
    if origin.is_empty() {
        return (..).into();
    }
    let e: Vec<Extent> = origin
        .iter()
        .zip(shape)
        .map(|(&o, &n)| (o..o + n).into())
        .collect();
    e.into()
}

impl NcWriter {
    /// Creates the file with all metadata and coordinates; data follows through `write`.
    pub fn create(
        path: &Path,
        plan: &Plan,
        lay: &[OutVar],
        classic: bool,
        history: Option<&str>,
    ) -> Result<Self> {
        let meta: OutMeta = out_meta(plan, lay, history)?;
        // `plan::out_compression` refuses zstd and NetCDF-3
        let level = plan.compression.filter(|_| !classic).map(|c| c.level);
        let _hdf5 = super::hdf5_lock();
        let opts = if classic {
            // cdo's `-f nc` also writes the 64-bit offset format (no 2 GiB offset limit)
            netcdf::Options::_64BIT_OFFSET | netcdf::Options::NOCLOBBER
        } else {
            netcdf::Options::NETCDF4 | netcdf::Options::NOCLOBBER
        };
        let mut f = netcdf::create_with(path, opts).map_err(|e| {
            let e = Error::from(e);
            Error::new(
                e.code,
                format!("cannot create '{}': {}", path.display(), e.message),
            )
        })?;
        // NOCLOBBER: the file did not exist before, so it is this process's own
        crate::exec::publish::mark_created(path, false);
        for (name, n, unl) in &meta.dims {
            if *unl {
                f.add_unlimited_dimension(name)?;
            } else {
                f.add_dimension(name, *n)?;
            }
        }
        for c in &meta.coords {
            let dims: Vec<&str> = c.dims.iter().map(String::as_str).collect();
            let mut v = match c.dtype {
                DType::F32 => f.add_variable::<f32>(&c.name, &dims)?,
                DType::I32 => f.add_variable::<i32>(&c.name, &dims)?,
                _ => f.add_variable::<f64>(&c.name, &dims)?,
            };
            // as cdo: grid coordinates of 32 values or more in one compressed chunk
            let unlimited = |d: &String| meta.dims.iter().any(|(n, _, unl)| n == d && *unl);
            if let Some(l) = level
                && c.values.len() >= 32
                && !c.dims.iter().any(unlimited)
            {
                v.set_chunking(&c.shape)?;
                v.set_compression(l.into(), true)?;
            }
            put_attrs(&mut v, &c.attrs, classic)?;
        }
        for (ov, attrs) in lay.iter().zip(&meta.var_attrs) {
            let dims: Vec<&str> = ov.dims.iter().map(String::as_str).collect();
            let mut v = match ov.dtype {
                DType::F32 => f.add_variable::<f32>(&ov.name, &dims)?,
                _ => f.add_variable::<f64>(&ov.name, &dims)?,
            };
            if !classic {
                v.set_chunking(&ov.chunks)?;
            }
            if let Some(l) = level {
                v.set_compression(l.into(), true)?;
            }
            match ov.dtype {
                DType::F32 => {
                    v.set_fill_value(ov.missval as f32)?;
                    v.put_attribute("missing_value", ov.missval as f32)?;
                }
                _ => {
                    v.set_fill_value(ov.missval)?;
                    v.put_attribute("missing_value", ov.missval)?;
                }
            }
            put_attrs(&mut v, attrs, classic)?;
        }
        for (k, v) in meta.global.iter() {
            f.add_attribute(k, nc_attr(v, classic))?;
        }
        if classic {
            f.enddef()?;
        }
        for c in &meta.coords {
            let mut v = f
                .variable_mut(&c.name)
                .ok_or_else(|| Error::internal(format!("coordinate '{}' vanished", c.name)))?;
            let origin = vec![0; c.shape.len()];
            let ext = extents(&origin, &c.shape);
            match c.dtype {
                DType::F32 => {
                    let x: Vec<f32> = c.values.iter().map(|&x| x as f32).collect();
                    v.put_values(&x, ext)?;
                }
                DType::I32 => {
                    let x: Vec<i32> = c.values.iter().map(|&x| x as i32).collect();
                    v.put_values(&x, ext)?;
                }
                _ => v.put_values(&c.values, ext)?,
            }
        }
        let (file, direct) = match level {
            None => (Some(f), None),
            Some(l) => {
                // netCDF-C creates the datasets, with their filters, when it closes the file
                f.close()?;
                (None, Some(Direct::open(path, lay, l)?))
            }
        };
        Ok(Self {
            file: Mutex::new(file),
            level,
            direct: Mutex::new(direct),
            vars: lay.to_vec(),
        })
    }

    /// `write_ready` of a compressed output: compressed on the calling thread, then stored as it
    /// is.
    fn write_direct(&self, var: usize, origin: &[usize], bytes: &[u8]) -> Result<()> {
        let offset: Vec<hdf5_metno_sys::h5::hsize_t> = origin.iter().map(|&o| o as _).collect();
        let _hdf5 = super::hdf5_lock();
        let g = self
            .direct
            .lock()
            .map_err(|_| Error::internal("HDF5 writer lock"))?;
        let d = g
            .as_ref()
            .ok_or_else(|| Error::internal("HDF5 file closed"))?;
        // SAFETY: the dataset is open until `finish` takes it (under the same lock); `offset`
        // has one entry per dimension of the dataset and `bytes` outlives the call.
        let rc = unsafe {
            hdf5_metno_sys::h5d::H5Dwrite_chunk(
                d.dsets[var].id(),
                hdf5_metno_sys::h5p::H5P_DEFAULT,
                0,
                offset.as_ptr(),
                bytes.len(),
                bytes.as_ptr().cast(),
            )
        };
        hdf5_metno::h5check(rc).map(|_| ()).map_err(|e| {
            Error::io(format!("HDF5: writing a chunk of '{}': {e}", self.vars[var].name))
        })
    }
}

impl Writer for NcWriter {
    fn ordered(&self) -> bool {
        // compressed chunks are stored in any order, from the write threads
        self.level.is_none()
    }

    fn missing_as(&self, var: usize) -> Option<f64> {
        self.vars.get(var).map(|v| v.missval)
    }

    fn write(&self, var: usize, origin: &[usize], shape: &[usize], mut data: Values) -> Result<()> {
        let mv = self.vars[var].missval;
        match &mut data {
            Values::F32(x) => {
                let mv = mv as f32;
                x.iter_mut().filter(|y| y.is_nan()).for_each(|y| *y = mv);
            }
            Values::F64(x) => x.iter_mut().filter(|y| y.is_nan()).for_each(|y| *y = mv),
        }
        self.write_ready(var, origin, shape, data)
    }

    fn write_ready(
        &self,
        var: usize,
        origin: &[usize],
        shape: &[usize],
        data: Values,
    ) -> Result<()> {
        let ov = &self.vars[var];
        if let Some(level) = self.level {
            let bytes = encode_chunk(ov, shape, data, level)?;
            return self.write_direct(var, origin, &bytes);
        }
        let _hdf5 = super::hdf5_lock();
        let mut g = self
            .file
            .lock()
            .map_err(|_| Error::internal("netCDF writer lock"))?;
        let f = g
            .as_mut()
            .ok_or_else(|| Error::internal("netCDF file closed"))?;
        let mut v = f
            .variable_mut(&ov.name)
            .ok_or_else(|| Error::internal(format!("variable '{}' vanished", ov.name)))?;
        let ext = extents(origin, shape);
        match data {
            Values::F32(x) => v.put_values(&x, ext)?,
            Values::F64(x) => v.put_values(&x, ext)?,
        }
        Ok(())
    }

    fn finish(&self) -> Result<()> {
        let _hdf5 = super::hdf5_lock();
        let direct = self
            .direct
            .lock()
            .map_err(|_| Error::internal("HDF5 writer lock"))?
            .take();
        if let Some(Direct { file, dsets }) = direct {
            let path = file.filename();
            drop(dsets);
            file.close().map_err(h5err(Path::new(&path)))?;
        }
        let f = self
            .file
            .lock()
            .map_err(|_| Error::internal("netCDF writer lock"))?
            .take();
        if let Some(f) = f {
            f.close()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shuffle_is_undone_by_the_readers_unshuffle() {
        let x: Vec<f64> = (0..7).map(|i| f64::from(i) * 1.5 - 2.0).collect();
        let raw: Vec<u8> = x.iter().flat_map(|v| v.to_le_bytes()).collect();
        let s = shuffle(&x, f64::to_le_bytes);
        assert_eq!(s[..7], raw.iter().step_by(8).copied().collect::<Vec<_>>()[..]);
        assert_eq!(crate::io::netcdf4_index::unshuffle(&s, 8), raw);
    }
}
