//! Nearest-cell lookup: a KD-tree on the unit vectors of the cell centres.
//!
//! The tree is implicit (no node allocations): the points are permuted so that the median of
//! each sub-range along its split axis (the axis of largest spread) sits at the middle of the
//! range. Ranges of at most [`LEAF`] points are scanned. Nearest by chord length, which orders
//! points like the great-circle distance; on equal distances the smallest cell index wins, so
//! the answer does not depend on how the tree was built.
//!
//! A sub-range is searched only if the query's distance to it is within the best distance so
//! far: the distance to the bounding box of its points for the upper nodes (sub-ranges of more
//! than [`BOX_MIN`] points, heap-numbered), and below them the distance to the cell cut out by
//! the splits above, tracked incrementally per axis (Arya and Mount). Pruning by the split plane
//! alone visits almost the whole tree for a query far from all points (a cell outside a cluster
//! of points, as `fill=nearest` asks).

use crate::geom::unit_vector;
use rayon::prelude::*;

/// Points per leaf range.
const LEAF: usize = 8;
/// Sub-ranges of more than this many points get a bounding box.
const BOX_MIN: usize = 32;
/// Sub-ranges larger than this are built on two threads.
const PAR_BUILD: usize = 1 << 15;

pub struct KdTree {
    /// Unit vectors in tree order.
    pts: Vec<[f64; 3]>,
    /// Cell index of each point in tree order.
    idx: Vec<u32>,
    /// Split axis of the node at the middle of each sub-range.
    axis: Vec<u8>,
    /// Bounding boxes (min xyz, max xyz) of the upper nodes, by heap number (root 0, children
    /// of h: 2h + 1 and 2h + 2).
    boxes: Vec<[f64; 6]>,
}

impl KdTree {
    /// A tree over the cell centres `lon`, `lat` (degrees). NaN centres are left out.
    pub fn new(lon: &[f64], lat: &[f64]) -> Self {
        assert_eq!(lon.len(), lat.len());
        let pts: Vec<([f64; 3], u32)> = (0..lon.len())
            .into_par_iter()
            .filter(|&i| !(lon[i].is_nan() || lat[i].is_nan()))
            .map(|i| (unit_vector(lon[i], lat[i]), i as u32))
            .collect();
        Self::from_points(pts)
    }

    /// A tree over unit vectors with their cell indices.
    pub fn from_points(mut pts: Vec<([f64; 3], u32)>) -> Self {
        let mut axis = vec![0u8; pts.len()];
        build(&mut pts, &mut axis);
        let (pts, idx): (Vec<[f64; 3]>, Vec<u32>) = pts.into_iter().unzip();
        // heap numbers of all nodes of the depths whose sub-ranges exceed BOX_MIN points
        let mut depth = 0;
        let mut size = pts.len();
        while size > BOX_MIN {
            size /= 2;
            depth += 1;
        }
        let mut t = Self {
            pts,
            idx,
            axis,
            boxes: vec![[0.0; 6]; (1usize << depth) - 1],
        };
        if !t.boxes.is_empty() {
            t.make_box(0, 0, t.pts.len());
        }
        t
    }

    /// Bounding box of sub-range `lo..hi` (node `h`), stored for the upper nodes.
    fn make_box(&mut self, h: usize, lo: usize, hi: usize) -> [f64; 6] {
        let mut b = [
            f64::INFINITY,
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        let add = |b: &mut [f64; 6], o: &[f64; 6]| {
            for k in 0..3 {
                b[k] = b[k].min(o[k]);
                b[k + 3] = b[k + 3].max(o[k + 3]);
            }
        };
        if h >= self.boxes.len() || hi - lo <= LEAF {
            for p in &self.pts[lo..hi] {
                add(&mut b, &[p[0], p[1], p[2], p[0], p[1], p[2]]);
            }
            return b;
        }
        let mid = lo + (hi - lo) / 2;
        let l = self.make_box(2 * h + 1, lo, mid);
        let r = self.make_box(2 * h + 2, mid + 1, hi);
        let p = self.pts[mid];
        add(&mut b, &l);
        add(&mut b, &r);
        add(&mut b, &[p[0], p[1], p[2], p[0], p[1], p[2]]);
        self.boxes[h] = b;
        b
    }

    /// Squared distance from `q` to the box of node `h`, 0 if it has none.
    #[inline]
    fn box_dist(&self, h: usize, q: &[f64; 3]) -> f64 {
        let Some(b) = self.boxes.get(h) else {
            return 0.0;
        };
        let mut d = 0.0;
        for k in 0..3 {
            let e = (b[k] - q[k]).max(0.0).max(q[k] - b[k + 3]);
            d += e * e;
        }
        d
    }

    pub fn len(&self) -> usize {
        self.pts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pts.is_empty()
    }

    /// Size in bytes.
    pub fn bytes(&self) -> u64 {
        29 * self.pts.len() as u64 + 48 * self.boxes.len() as u64
    }

    /// The cell nearest to (lon, lat) (degrees) and its central angle (radians); `None` for an
    /// empty tree.
    pub fn nearest(&self, lon: f64, lat: f64) -> Option<(usize, f64)> {
        self.nearest_xyz(unit_vector(lon, lat))
    }

    /// The cell nearest to a unit vector, and its central angle (radians).
    pub fn nearest_xyz(&self, q: [f64; 3]) -> Option<(usize, f64)> {
        if self.pts.is_empty() {
            return None;
        }
        let mut best = (f64::INFINITY, u32::MAX);
        self.search(0, 0, self.pts.len(), &q, &mut best, &mut [0.0; 3], 0.0);
        let chord = best.0.sqrt();
        Some((best.1 as usize, 2.0 * (0.5 * chord).min(1.0).asin()))
    }

    /// [`KdTree::nearest`] for many points, in parallel.
    pub fn nearest_many(&self, lon: &[f64], lat: &[f64]) -> Vec<Option<(usize, f64)>> {
        (0..lon.len())
            .into_par_iter()
            .map(|i| self.nearest(lon[i], lat[i]))
            .collect()
    }

    /// The `k` cells nearest to a unit vector, nearest first (equal distances: smallest index
    /// first), with their squared chord lengths; fewer if the tree has fewer points.
    pub fn nearest_k_xyz(&self, q: [f64; 3], k: usize) -> Vec<(usize, f64)> {
        let mut best: Vec<(f64, u32)> = Vec::with_capacity(k + 1);
        if k > 0 && !self.pts.is_empty() {
            self.search_k(0, self.pts.len(), &q, k, &mut best);
        }
        best.into_iter().map(|(d, i)| (i as usize, d)).collect()
    }

    fn consider_k(&self, i: usize, q: &[f64; 3], k: usize, best: &mut Vec<(f64, u32)>) {
        let p = &self.pts[i];
        let d = (p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2);
        let key = (d, self.idx[i]);
        let less = |a: &(f64, u32), b: &(f64, u32)| a.0 < b.0 || (a.0 == b.0 && a.1 < b.1);
        if best.len() == k && !less(&key, &best[k - 1]) {
            return;
        }
        let at = best.partition_point(|b| less(b, &key));
        best.insert(at, key);
        best.truncate(k);
    }

    fn search_k(&self, lo: usize, hi: usize, q: &[f64; 3], k: usize, best: &mut Vec<(f64, u32)>) {
        let n = hi - lo;
        if n <= LEAF {
            for i in lo..hi {
                self.consider_k(i, q, k, best);
            }
            return;
        }
        let mid = lo + n / 2;
        let a = self.axis[mid] as usize;
        self.consider_k(mid, q, k, best);
        let d = q[a] - self.pts[mid][a];
        let (near, far) = if d < 0.0 {
            ((lo, mid), (mid + 1, hi))
        } else {
            ((mid + 1, hi), (lo, mid))
        };
        self.search_k(near.0, near.1, q, k, best);
        if best.len() < k || d * d <= best[k - 1].0 {
            self.search_k(far.0, far.1, q, k, best);
        }
    }

    fn consider(&self, i: usize, q: &[f64; 3], best: &mut (f64, u32)) {
        let p = &self.pts[i];
        let d = (p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2);
        if d < best.0 || (d == best.0 && self.idx[i] < best.1) {
            *best = (d, self.idx[i]);
        }
    }

    /// Searches the sub-range `lo..hi` (node `h`), whose cell lies `off[k]` from the query along
    /// each axis (squared distance `rd`, the sum of their squares).
    #[allow(clippy::too_many_arguments)]
    fn search(
        &self,
        h: usize,
        lo: usize,
        hi: usize,
        q: &[f64; 3],
        best: &mut (f64, u32),
        off: &mut [f64; 3],
        rd: f64,
    ) {
        let n = hi - lo;
        if n <= LEAF {
            for i in lo..hi {
                self.consider(i, q, best);
            }
            return;
        }
        let mid = lo + n / 2;
        let a = self.axis[mid] as usize;
        self.consider(mid, q, best);
        let d = q[a] - self.pts[mid][a];
        let ((nh, nlo, nhi), (fh, flo, fhi)) = if d < 0.0 {
            ((2 * h + 1, lo, mid), (2 * h + 2, mid + 1, hi))
        } else {
            ((2 * h + 2, mid + 1, hi), (2 * h + 1, lo, mid))
        };
        // `<=` with slack: a point at the same distance may have a smaller index, and rounding in
        // the bounds must not prune it
        let within = |bound: f64, best: f64| bound * (1.0 - 1e-12) <= best;
        if within(self.box_dist(nh, q).max(rd), best.0) {
            self.search(nh, nlo, nhi, q, best, off, rd);
        }
        // the far cell lies |d| from the query along axis a (at least the old offset)
        let old = off[a];
        let rd_far = rd - old * old + d * d;
        if within(self.box_dist(fh, q).max(rd_far), best.0) {
            off[a] = d;
            self.search(fh, flo, fhi, q, best, off, rd_far);
            off[a] = old;
        }
    }
}

impl std::fmt::Debug for KdTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KdTree({} points)", self.pts.len())
    }
}

fn build(pts: &mut [([f64; 3], u32)], axis: &mut [u8]) {
    let n = pts.len();
    if n <= LEAF {
        return;
    }
    let (mut lo, mut hi) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
    for (p, _) in pts.iter() {
        for k in 0..3 {
            lo[k] = lo[k].min(p[k]);
            hi[k] = hi[k].max(p[k]);
        }
    }
    let a = (0..3)
        .max_by(|&i, &j| (hi[i] - lo[i]).total_cmp(&(hi[j] - lo[j])))
        .unwrap();
    let mid = n / 2;
    pts.select_nth_unstable_by(mid, |p, q| p.0[a].total_cmp(&q.0[a]).then(p.1.cmp(&q.1)));
    axis[mid] = a as u8;
    let (l, r) = pts.split_at_mut(mid);
    let (al, ar) = axis.split_at_mut(mid);
    let (r, ar) = (&mut r[1..], &mut ar[1..]);
    if n > PAR_BUILD {
        rayon::join(|| build(l, al), || build(r, ar));
    } else {
        build(l, al);
        build(r, ar);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_matches_brute_force() {
        // a small LCG: 3000 random centres (one duplicated, to exercise the tie rule) and 500
        // queries
        let mut s = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut pt = || {
            (
                360.0 * rnd() - 180.0,
                (2.0 * rnd() - 1.0).asin().to_degrees(),
            )
        };
        // the whole sphere, then a cluster (30 x 30 degrees) that most queries are far from
        for cluster in [false, true] {
            let (mut lon, mut lat): (Vec<f64>, Vec<f64>) = (0..3000)
                .map(|_| {
                    let (x, y) = pt();
                    if cluster {
                        (x / 12.0, 40.0 + y / 6.0)
                    } else {
                        (x, y)
                    }
                })
                .unzip();
            lon[2999] = lon[17];
            lat[2999] = lat[17];
            let tree = KdTree::new(&lon, &lat);
            for k in 0..500 {
                let (x, y) = if k == 0 { (lon[17], lat[17]) } else { pt() };
                let q = unit_vector(x, y);
                let (bi, bd) = (0..lon.len())
                    .map(|i| {
                        let p = unit_vector(lon[i], lat[i]);
                        (
                            i,
                            (p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2),
                        )
                    })
                    .min_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)))
                    .unwrap();
                let mut all: Vec<(usize, f64)> = (0..lon.len())
                    .map(|i| {
                        let p = unit_vector(lon[i], lat[i]);
                        (
                            i,
                            (p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2),
                        )
                    })
                    .collect();
                all.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
                let knn = tree.nearest_k_xyz(q, 12);
                assert_eq!(
                    knn.iter().map(|p| p.0).collect::<Vec<_>>(),
                    all[..12].iter().map(|p| p.0).collect::<Vec<_>>(),
                    "query {k}"
                );
                let (i, ang) = tree.nearest(x, y).unwrap();
                assert_eq!(i, bi, "query {k}");
                assert!((ang - 2.0 * (0.5 * bd.sqrt()).asin()).abs() < 1e-15);
            }
            assert_eq!(tree.nearest(lon[17], lat[17]).unwrap().0, 17);
        }
    }
}
