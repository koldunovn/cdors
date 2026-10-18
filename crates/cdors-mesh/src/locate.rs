//! Nearest-cell lookup: a KD-tree on the unit vectors of the cell centres.
//!
//! The tree is implicit (no node allocations): the points are permuted so that the median of
//! each sub-range along its split axis (the axis of largest spread) sits at the middle of the
//! range. Ranges of at most [`LEAF`] points are scanned. Nearest by chord length, which orders
//! points like the great-circle distance; on equal distances the smallest cell index wins, so
//! the answer does not depend on how the tree was built.

use crate::geom::unit_vector;
use rayon::prelude::*;

/// Points per leaf range.
const LEAF: usize = 8;
/// Sub-ranges larger than this are built on two threads.
const PAR_BUILD: usize = 1 << 15;

pub struct KdTree {
    /// Unit vectors in tree order.
    pts: Vec<[f64; 3]>,
    /// Cell index of each point in tree order.
    idx: Vec<u32>,
    /// Split axis of the node at the middle of each sub-range.
    axis: Vec<u8>,
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
        let (pts, idx) = pts.into_iter().unzip();
        Self { pts, idx, axis }
    }

    pub fn len(&self) -> usize {
        self.pts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pts.is_empty()
    }

    /// Size in bytes.
    pub fn bytes(&self) -> u64 {
        29 * self.pts.len() as u64
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
        self.search(0, self.pts.len(), &q, &mut best);
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

    fn consider(&self, i: usize, q: &[f64; 3], best: &mut (f64, u32)) {
        let p = &self.pts[i];
        let d = (p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2);
        if d < best.0 || (d == best.0 && self.idx[i] < best.1) {
            *best = (d, self.idx[i]);
        }
    }

    fn search(&self, lo: usize, hi: usize, q: &[f64; 3], best: &mut (f64, u32)) {
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
        let (near, far) = if d < 0.0 {
            ((lo, mid), (mid + 1, hi))
        } else {
            ((mid + 1, hi), (lo, mid))
        };
        self.search(near.0, near.1, q, best);
        // `<=`: a point at the same distance on the far side may have a smaller index
        if d * d <= best.0 {
            self.search(far.0, far.1, q, best);
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
        let (mut lon, mut lat): (Vec<f64>, Vec<f64>) = (0..3000).map(|_| pt()).unzip();
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
            let (i, ang) = tree.nearest(x, y).unwrap();
            assert_eq!(i, bi, "query {k}");
            assert!((ang - 2.0 * (0.5 * bd.sqrt()).asin()).abs() < 1e-15);
        }
        assert_eq!(tree.nearest(lon[17], lat[17]).unwrap().0, 17);
    }
}
