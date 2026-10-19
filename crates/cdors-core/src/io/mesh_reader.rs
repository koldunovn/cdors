//! [`ArrayReader`] for mesh files: any [`ChunkSource`] (NetCDF through both read paths, Zarr),
//! so the mesh loaders of cdors-mesh read through the same readers as data.
//!
//! Hyperslabs are assembled from the chunks they touch. Contiguous NetCDF variables are read one
//! record (first-dimension index) per chunk, so one level of FESOM2's `nod_area(nl, nod)` is one
//! read, not the whole variable. Values are unpacked, missing values are NaN.

use super::{ChunkSource, Values};
use crate::error::Error;
use crate::model::AttrValue;
use cdors_mesh::{ArrayReader, Attr, MeshError, VarInfo};
use std::ops::Range;
use std::sync::Arc;

/// An [`ArrayReader`] over a chunk source.
pub struct SourceReader {
    src: Arc<dyn ChunkSource>,
}

impl SourceReader {
    pub fn new(src: Arc<dyn ChunkSource>) -> Self {
        Self { src }
    }

    /// Opens a file or store ([`super::open`]) as a reader.
    pub fn open(path: &str) -> crate::error::Result<Arc<dyn ArrayReader>> {
        Ok(Arc::new(Self::new(super::open(path)?)))
    }
}

fn read_err(e: Error) -> MeshError {
    MeshError::Read(e.message)
}

impl ArrayReader for SourceReader {
    fn describe(&self) -> String {
        self.src.dataset().source.clone()
    }

    fn dim(&self, name: &str) -> Option<usize> {
        self.src.dataset().dim_size(name)
    }

    fn var(&self, name: &str) -> Option<VarInfo> {
        let v = self.src.dataset().var(name)?;
        Some(VarInfo {
            dims: v.dims.iter().map(|d| d.name.clone()).collect(),
            shape: v.shape(),
        })
    }

    fn attr(&self, var: Option<&str>, name: &str) -> Option<Attr> {
        let ds = self.src.dataset();
        let attrs = match var {
            Some(v) => &ds.var(v)?.attrs,
            None => &ds.attrs,
        };
        Some(match attrs.get(name)? {
            AttrValue::Text(s) => Attr::Text(s.clone()),
            v => Attr::Num(v.as_f64s()),
        })
    }

    fn read(&self, var: &str) -> cdors_mesh::Result<Vec<f64>> {
        self.require(var)?;
        self.src.read_var(var).map_err(read_err)
    }

    fn read_range(&self, var: &str, ranges: &[Range<usize>]) -> cdors_mesh::Result<Vec<f64>> {
        let info = self.require(var)?;
        let shape = &info.shape;
        if ranges.len() != shape.len() || ranges.iter().zip(shape).any(|(r, &n)| r.end > n) {
            return Err(MeshError::Invalid(format!(
                "range {ranges:?} outside '{var}' of shape {shape:?}"
            )));
        }
        let count: usize = ranges.iter().map(|r| r.len()).product();
        if count == 0 {
            return Ok(Vec::new());
        }
        if shape.is_empty() {
            return self.read(var);
        }
        let grid = self.src.chunk_grid(var).map_err(read_err)?;
        let nd = shape.len();
        let cs = &grid.chunk_shape;
        // chunk index ranges touched along each dimension
        let lo: Vec<u64> = (0..nd).map(|d| (ranges[d].start / cs[d]) as u64).collect();
        let hi: Vec<u64> = (0..nd)
            .map(|d| ranges[d].end.div_ceil(cs[d]) as u64)
            .collect();
        let read = |idx: &[u64]| {
            self.src
                .read_chunk(var, idx)
                .and_then(|c| c.decode())
                .map_err(read_err)
        };
        // a range that is exactly one chunk (one level of per-level areas in the mesh cache): its
        // values as decoded, without a second buffer to assemble them in
        if (0..nd).all(|d| lo[d] + 1 == hi[d]) {
            let ch = read(&lo)?;
            let exact = (0..nd).all(|d| {
                ch.origin[d] as usize == ranges[d].start && ch.shape[d] == ranges[d].len()
            });
            if exact {
                return Ok(match ch.values {
                    Values::F64(v) => v,
                    Values::F32(v) => v.into_iter().map(f64::from).collect(),
                });
            }
        }
        let mut out = vec![f64::NAN; count];
        let ostride: Vec<usize> = (0..nd)
            .map(|d| ranges[d + 1..].iter().map(|r| r.len()).product())
            .collect();
        let mut idx = lo.clone();
        loop {
            let ch = read(&idx)?;
            let corig: Vec<usize> = ch.origin.iter().map(|&o| o as usize).collect();
            let cshape = &ch.shape;
            // the intersection of the chunk with the ranges, copied row by row (last dim)
            let a: Vec<usize> = (0..nd).map(|d| ranges[d].start.max(corig[d])).collect();
            let b: Vec<usize> = (0..nd)
                .map(|d| ranges[d].end.min(corig[d] + cshape[d]))
                .collect();
            if (0..nd).all(|d| a[d] < b[d]) {
                let len = b[nd - 1] - a[nd - 1];
                let nrows: usize = (0..nd - 1).map(|d| b[d] - a[d]).product();
                let mut p = a.clone();
                for row in 0..nrows {
                    // position of the row: `row` in mixed radix over the leading dimensions
                    let mut q = row;
                    for d in (0..nd - 1).rev() {
                        p[d] = a[d] + q % (b[d] - a[d]);
                        q /= b[d] - a[d];
                    }
                    let src = (0..nd).fold(0, |o, d| o * cshape[d] + (p[d] - corig[d]));
                    let dst: usize = (0..nd).map(|d| (p[d] - ranges[d].start) * ostride[d]).sum();
                    let row = &mut out[dst..dst + len];
                    match &ch.values {
                        Values::F64(v) => row.copy_from_slice(&v[src..src + len]),
                        Values::F32(v) => {
                            for (o, &x) in row.iter_mut().zip(&v[src..src + len]) {
                                *o = f64::from(x);
                            }
                        }
                    }
                }
            }
            // next chunk
            let mut d = nd;
            loop {
                if d == 0 {
                    return Ok(out);
                }
                d -= 1;
                idx[d] += 1;
                if idx[d] < hi[d] {
                    break;
                }
                idx[d] = lo[d];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_across_chunks() {
        // the tiny-chunk Zarr fixture of hpz2_noleap against whole reads
        let Some(dir) = std::env::var_os("CDORS_FIXTURES") else {
            return;
        };
        let path = std::path::Path::new(&dir).join("hpz2_noleap_tiny.zarr2");
        if !path.exists() {
            return;
        }
        let r = SourceReader::open(path.to_str().unwrap()).unwrap();
        let name = r.var("tas").map(|_| "tas").expect("hpz2_noleap has tas");
        let info = r.var(name).unwrap();
        let all = r.read(name).unwrap();
        let ranges: Vec<Range<usize>> = info.shape.iter().map(|&n| n / 3..n - n / 4).collect();
        let got = r.read_range(name, &ranges).unwrap();
        let mut want = Vec::new();
        let n = &info.shape;
        for t in ranges[0].clone() {
            for c in ranges[1].clone() {
                want.push(all[t * n[1] + c]);
            }
        }
        assert_eq!(got.len(), want.len());
        assert!(
            got.iter()
                .zip(&want)
                .all(|(a, b)| a.to_bits() == b.to_bits())
        );
    }
}
