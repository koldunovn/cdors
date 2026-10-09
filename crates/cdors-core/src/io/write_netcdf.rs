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

use crate::error::{Error, Result};
use crate::exec::{OutMeta, OutVar, Writer, out_meta};
use crate::io::Values;
use crate::model::{AttrValue, Attrs, DType};
use crate::plan::Plan;
use netcdf::{AttributeValue, Extent, Extents, FileMut};
use std::path::Path;
use std::sync::Mutex;

pub struct NcWriter {
    file: Mutex<Option<FileMut>>,
    vars: Vec<OutVar>,
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
        Ok(Self {
            file: Mutex::new(Some(f)),
            vars: lay.to_vec(),
        })
    }
}

impl Writer for NcWriter {
    fn ordered(&self) -> bool {
        true
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
