//! The interface through which mesh loaders read their files.
//!
//! A loader (one per model, `models/<name>/`) sees a mesh file only as named variables,
//! dimensions and attributes. cdors-core implements [`ArrayReader`] for NetCDF and Zarr; tests use
//! [`MemReader`]. All values come as f64: indices and level counts are integers well below 2^53,
//! so they convert exactly.

use crate::{MeshError, Result};
use std::collections::HashMap;
use std::ops::Range;

/// An attribute value.
#[derive(Debug, Clone, PartialEq)]
pub enum Attr {
    Text(String),
    Num(Vec<f64>),
}

impl Attr {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            Self::Num(_) => None,
        }
    }

    /// The first number of a numeric attribute.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Num(v) => v.first().copied(),
            Self::Text(_) => None,
        }
    }
}

/// Dimensions of a variable, in storage order (C order, last dimension fastest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarInfo {
    pub dims: Vec<String>,
    pub shape: Vec<usize>,
}

impl VarInfo {
    pub fn len(&self) -> usize {
        self.shape.iter().product()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Read access to the variables of one mesh file or store.
pub trait ArrayReader: Send + Sync {
    /// What is being read (a path), for messages and [`crate::Source`].
    fn describe(&self) -> String;
    /// Length of a dimension.
    fn dim(&self, name: &str) -> Option<usize>;
    /// Dimensions of a variable, `None` if it does not exist.
    fn var(&self, name: &str) -> Option<VarInfo>;
    /// An attribute of a variable, or a global attribute for `var = None`.
    fn attr(&self, var: Option<&str>, name: &str) -> Option<Attr>;
    /// All values of a variable, in C order, as f64 (fill values as stored).
    fn read(&self, var: &str) -> Result<Vec<f64>>;
    /// The hyperslab `ranges` (one range per dimension) of a variable, in C order.
    fn read_range(&self, var: &str, ranges: &[Range<usize>]) -> Result<Vec<f64>>;

    /// [`ArrayReader::var`] or a [`MeshError::Missing`] naming the variable and the file.
    fn require(&self, name: &str) -> Result<VarInfo> {
        self.var(name).ok_or_else(|| {
            MeshError::Missing(format!(
                "variable '{name}' not found in {}",
                self.describe()
            ))
        })
    }
}

/// An in-memory [`ArrayReader`] (tests, and meshes built in memory).
#[derive(Debug, Clone, Default)]
pub struct MemReader {
    pub name: String,
    pub dims: HashMap<String, usize>,
    pub vars: HashMap<String, (VarInfo, Vec<f64>)>,
    /// Keyed by (variable or "" for global, attribute name).
    pub attrs: HashMap<(String, String), Attr>,
}

impl MemReader {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ..Self::default()
        }
    }

    /// Adds a variable over `dims` (name, length); the dimensions are registered too.
    pub fn with_var(mut self, name: &str, dims: &[(&str, usize)], values: Vec<f64>) -> Self {
        let info = VarInfo {
            dims: dims.iter().map(|d| d.0.to_owned()).collect(),
            shape: dims.iter().map(|d| d.1).collect(),
        };
        assert_eq!(info.len(), values.len(), "MemReader: size of '{name}'");
        for &(d, n) in dims {
            self.dims.insert(d.to_owned(), n);
        }
        self.vars.insert(name.to_owned(), (info, values));
        self
    }

    pub fn with_attr(mut self, var: Option<&str>, name: &str, value: Attr) -> Self {
        self.attrs
            .insert((var.unwrap_or("").to_owned(), name.to_owned()), value);
        self
    }
}

impl ArrayReader for MemReader {
    fn describe(&self) -> String {
        self.name.clone()
    }

    fn dim(&self, name: &str) -> Option<usize> {
        self.dims.get(name).copied()
    }

    fn var(&self, name: &str) -> Option<VarInfo> {
        self.vars.get(name).map(|v| v.0.clone())
    }

    fn attr(&self, var: Option<&str>, name: &str) -> Option<Attr> {
        self.attrs
            .get(&(var.unwrap_or("").to_owned(), name.to_owned()))
            .cloned()
    }

    fn read(&self, var: &str) -> Result<Vec<f64>> {
        self.require(var)?;
        Ok(self.vars[var].1.clone())
    }

    fn read_range(&self, var: &str, ranges: &[Range<usize>]) -> Result<Vec<f64>> {
        let info = self.require(var)?;
        let (shape, vals) = (&info.shape, &self.vars[var].1);
        if ranges.len() != shape.len() || ranges.iter().zip(shape).any(|(r, &n)| r.end > n) {
            return Err(MeshError::Invalid(format!(
                "range {ranges:?} outside '{var}' of shape {shape:?}"
            )));
        }
        let count: usize = ranges.iter().map(|r| r.len()).product();
        let mut out = Vec::with_capacity(count);
        if count == 0 {
            return Ok(out);
        }
        // odometer over all dimensions but the last, which is copied as one slice
        let nd = shape.len();
        let mut idx: Vec<usize> = ranges.iter().map(|r| r.start).collect();
        loop {
            let off = (0..nd).fold(0, |o, d| o * shape[d] + idx[d]);
            let last = &ranges[nd - 1];
            out.extend_from_slice(&vals[off..off + last.len()]);
            let mut d = nd - 1;
            loop {
                if d == 0 {
                    return Ok(out);
                }
                d -= 1;
                idx[d] += 1;
                if idx[d] < ranges[d].end {
                    break;
                }
                idx[d] = ranges[d].start;
            }
        }
    }
}
