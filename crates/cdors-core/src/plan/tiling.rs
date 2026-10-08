//! Tiles aligned to chunk boundaries, the chunks each tile reads, and gathering tile values
//! from decoded chunks.
//!
//! A variable is split along every output dimension into segments: runs of consecutive output
//! indices whose stored index (through the primary leaf's map) falls into the same stored chunk.
//! A tile is one segment per dimension, so with plain selections a tile is exactly the selected
//! part of one stored chunk, and a chunk that holds no selected index is never read.

use super::{Expr, IndexMap, Leaf, VarDesc};
use crate::error::{Error, Result};
use crate::io::{ChunkGrid, ChunkSource, DecodedChunk, Values};
use std::ops::Range;
use std::sync::Arc;

/// A box in the output index space of one variable: one range per output dimension.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TileBox {
    pub ranges: Vec<Range<usize>>,
}

impl TileBox {
    pub fn shape(&self) -> Vec<usize> {
        self.ranges.iter().map(|r| r.end - r.start).collect()
    }

    pub fn len(&self) -> usize {
        self.ranges.iter().map(|r| r.end - r.start).product()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Segments of every output dimension; tiles are their cartesian product in C order.
#[derive(Debug, Clone)]
pub struct VarTiling {
    pub segments: Vec<Vec<Range<usize>>>,
}

/// Runs of consecutive output indices whose stored index lies in the same chunk.
fn segments(map: &IndexMap, chunk: usize) -> Vec<Range<usize>> {
    let n = map.len();
    let mut out = Vec::new();
    if n == 0 {
        return out;
    }
    if let IndexMap::Const { .. } = map {
        return std::iter::once(0..n).collect();
    }
    let c = chunk.max(1);
    let mut start = 0;
    let mut cur = map.get(0) / c;
    for i in 1..n {
        let k = map.get(i) / c;
        if k != cur {
            out.push(start..i);
            start = i;
            cur = k;
        }
    }
    out.push(start..n);
    out
}

impl VarTiling {
    /// Tiles of `var`, aligned to the chunks of its primary leaf (the first leaf that follows
    /// the most output dimensions).
    pub fn new(var: &VarDesc, leaves: &[LeafInfo]) -> Self {
        let primary = leaves
            .iter()
            .enumerate()
            .max_by_key(|(i, l)| (l.leaf.maps.len(), usize::MAX - i))
            .map(|(_, l)| l);
        let segments = var
            .dims
            .iter()
            .enumerate()
            .map(|(d, dim)| {
                let found = primary.and_then(|p| {
                    p.leaf
                        .maps
                        .iter()
                        .enumerate()
                        .find(|(_, (od, _))| *od == d)
                        .map(|(k, (_, m))| segments(m, p.grid.chunk_shape[k]))
                });
                found.unwrap_or_else(|| std::iter::once(0..dim.size).collect())
            })
            .collect();
        Self { segments }
    }

    pub fn num_tiles(&self) -> usize {
        self.segments.iter().map(Vec::len).product()
    }

    /// Segment index along every dimension of tile `k`.
    pub fn segment_indices(&self, mut k: usize) -> Vec<usize> {
        let mut seg = vec![0; self.segments.len()];
        for d in (0..self.segments.len()).rev() {
            let n = self.segments[d].len();
            seg[d] = k % n;
            k /= n;
        }
        seg
    }

    /// Tile `k` in C order (first dimension slowest).
    pub fn tile(&self, mut k: usize) -> TileBox {
        let mut ranges = vec![0..0; self.segments.len()];
        for d in (0..self.segments.len()).rev() {
            let n = self.segments[d].len();
            ranges[d] = self.segments[d][k % n].clone();
            k /= n;
        }
        TileBox { ranges }
    }
}

/// A leaf with the chunk grid of its stored variable.
#[derive(Clone)]
pub struct LeafInfo {
    pub leaf: Leaf,
    pub grid: ChunkGrid,
    pub src: Arc<dyn ChunkSource>,
}

impl LeafInfo {
    pub fn new(leaf: &Leaf, sources: &[Arc<dyn ChunkSource>]) -> Result<Self> {
        let src = sources[leaf.src].clone();
        let grid = src.chunk_grid(&leaf.var)?;
        if grid.shape.len() != leaf.maps.len() {
            return Err(Error::internal(format!(
                "leaf '{}' has {} maps for {} dimensions",
                leaf.var,
                leaf.maps.len(),
                grid.shape.len()
            )));
        }
        Ok(Self {
            leaf: leaf.clone(),
            grid,
            src,
        })
    }

    /// Leaves of an expression with their chunk grids.
    pub fn of(expr: &Expr, sources: &[Arc<dyn ChunkSource>]) -> Result<Vec<Self>> {
        expr.leaves()
            .into_iter()
            .map(|l| Self::new(l, sources))
            .collect()
    }
}

/// What one leaf reads for one tile: the chunks along each stored dimension and, for each
/// output index of the tile, which of those chunks holds its value and at which offset.
#[derive(Debug, Clone)]
pub struct LeafRead {
    /// Per stored dimension: chunk indices (ascending, distinct).
    pub chunk_sets: Vec<Vec<u64>>,
    /// Per stored dimension: for each local index along its output dimension,
    /// (position in `chunk_sets[k]`, offset within the chunk).
    pub pos: Vec<Vec<(u32, u32)>>,
    /// Output dimension followed by each stored dimension.
    pub out_dims: Vec<usize>,
}

impl LeafRead {
    pub fn new(li: &LeafInfo, tile: &TileBox) -> Self {
        let mut chunk_sets = Vec::with_capacity(li.leaf.maps.len());
        let mut pos = Vec::with_capacity(li.leaf.maps.len());
        let mut out_dims = Vec::with_capacity(li.leaf.maps.len());
        for (k, (d, map)) in li.leaf.maps.iter().enumerate() {
            let c = li.grid.chunk_shape[k].max(1);
            let r = &tile.ranges[*d];
            let idx: Vec<usize> = r.clone().map(|i| map.get(i)).collect();
            let mut set: Vec<u64> = idx.iter().map(|&s| (s / c) as u64).collect();
            set.sort_unstable();
            set.dedup();
            let p = idx
                .iter()
                .map(|&s| {
                    let ci = (s / c) as u64;
                    let slot = set.binary_search(&ci).expect("chunk in set") as u32;
                    (slot, (s - ci as usize * c) as u32)
                })
                .collect();
            chunk_sets.push(set);
            pos.push(p);
            out_dims.push(*d);
        }
        Self {
            chunk_sets,
            pos,
            out_dims,
        }
    }

    /// All chunks to read, in C order of their slots.
    pub fn chunks(&self) -> Vec<Vec<u64>> {
        let n: usize = self.chunk_sets.iter().map(Vec::len).product();
        let mut out = Vec::with_capacity(n);
        for mut k in 0..n {
            let mut idx = vec![0u64; self.chunk_sets.len()];
            for d in (0..self.chunk_sets.len()).rev() {
                let m = self.chunk_sets[d].len();
                idx[d] = self.chunk_sets[d][k % m];
                k /= m;
            }
            out.push(idx);
        }
        out
    }

    pub fn num_chunks(&self) -> usize {
        self.chunk_sets.iter().map(Vec::len).product()
    }

    /// Gathers the tile's values from the decoded chunks (in the order of [`Self::chunks`]).
    /// Output dimensions the leaf does not follow are broadcast.
    pub fn gather(&self, chunks: Vec<DecodedChunk>, tile_shape: &[usize]) -> Values {
        // fast path: one chunk, contiguous offsets 0..n along every stored dimension that
        // covers the whole chunk, and the tile follows the leaf dimension by dimension
        if chunks.len() == 1
            && self.out_dims.len() == tile_shape.len()
            && self.out_dims.iter().enumerate().all(|(k, &d)| d == k)
            && self.pos.iter().zip(&chunks[0].shape).all(|(p, &ext)| {
                p.len() == ext && p.iter().enumerate().all(|(i, &(_, o))| o as usize == i)
            })
        {
            return chunks.into_iter().next().expect("one chunk").values;
        }
        match &chunks[0].values {
            Values::F32(_) => Values::F32(self.gather_t(&chunks, tile_shape, |v| match v {
                Values::F32(x) => x.as_slice(),
                Values::F64(_) => unreachable!("chunks of one variable share a type"),
            })),
            Values::F64(_) => Values::F64(self.gather_t(&chunks, tile_shape, |v| match v {
                Values::F64(x) => x.as_slice(),
                Values::F32(_) => unreachable!("chunks of one variable share a type"),
            })),
        }
    }

    fn gather_t<T: Copy + Default>(
        &self,
        chunks: &[DecodedChunk],
        tile_shape: &[usize],
        get: impl Fn(&Values) -> &[T],
    ) -> Vec<T> {
        let nd = tile_shape.len();
        let total: usize = tile_shape.iter().product();
        let mut out = vec![T::default(); total];
        if total == 0 {
            return out;
        }
        let ns = self.out_dims.len();
        // slot strides (C order over chunk_sets)
        let mut sstride = vec![1usize; ns];
        for k in (0..ns.saturating_sub(1)).rev() {
            sstride[k] = sstride[k + 1] * self.chunk_sets[k + 1].len();
        }
        // element strides of each chunk (chunks at the array edge are trimmed)
        let cstride: Vec<Vec<usize>> = chunks
            .iter()
            .map(|c| {
                let mut s = vec![1usize; ns];
                for k in (0..ns.saturating_sub(1)).rev() {
                    s[k] = s[k + 1] * c.shape[k + 1];
                }
                s
            })
            .collect();
        let data: Vec<&[T]> = chunks.iter().map(|c| get(&c.values)).collect();
        // stored dims following each output dim
        let mut follow: Vec<Vec<usize>> = vec![Vec::new(); nd];
        for (k, &d) in self.out_dims.iter().enumerate() {
            follow[d].push(k);
        }
        let last = nd - 1;
        let row = tile_shape[last];
        // stored dim following the last output dim (if exactly one): rows can be copied by runs
        let last_k = if follow[last].len() == 1 {
            Some(follow[last][0])
        } else {
            None
        };
        let mut idx = vec![0usize; nd];
        let mut o = 0;
        loop {
            // slot and offset contributions of the leading dims
            let mut slot0 = 0;
            let mut ks: Vec<(usize, usize)> = Vec::with_capacity(ns); // (stored dim, local offset)
            for (d, fk) in follow.iter().enumerate().take(last) {
                for &k in fk {
                    let (s, off) = self.pos[k][idx[d]];
                    slot0 += s as usize * sstride[k];
                    ks.push((k, off as usize));
                }
            }
            match last_k {
                Some(lk) => {
                    let p = &self.pos[lk];
                    let mut i = 0;
                    while i < row {
                        let (s, off) = p[i];
                        let slot = slot0 + s as usize * sstride[lk];
                        let cs = &cstride[slot];
                        let base: usize = ks.iter().map(|&(k, off)| off * cs[k]).sum();
                        // run of consecutive offsets within the same chunk
                        let mut j = i + 1;
                        while j < row && p[j].0 == s && p[j].1 == p[j - 1].1 + 1 {
                            j += 1;
                        }
                        let st = base + off as usize * cs[lk];
                        out[o + i..o + j].copy_from_slice(&data[slot][st..st + (j - i)]);
                        i = j;
                    }
                }
                None => {
                    for i in 0..row {
                        let mut slot = slot0;
                        let mut kk = ks.clone();
                        for &k in &follow[last] {
                            let (s, off) = self.pos[k][i];
                            slot += s as usize * sstride[k];
                            kk.push((k, off as usize));
                        }
                        let cs = &cstride[slot];
                        let off: usize = kk.iter().map(|&(k, off)| off * cs[k]).sum();
                        out[o + i] = data[slot][off];
                    }
                }
            }
            o += row;
            // odometer over the leading dims
            let mut d = last;
            loop {
                if d == 0 {
                    return out;
                }
                d -= 1;
                idx[d] += 1;
                if idx[d] < tile_shape[d] {
                    break;
                }
                idx[d] = 0;
            }
        }
    }
}
