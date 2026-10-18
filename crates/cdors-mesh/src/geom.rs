//! Spherical geometry and region tests.
//!
//! - **Unit vectors and great circles**: [`unit_vector`], [`to_lonlat`], [`distance`] (central
//!   angle), [`interpolate`] and [`path`] along the great circle between two points (sections).
//! - **Spherical polygon areas** as cdo computes them: fans of triangles, each by L'Huilier's
//!   theorem ([`huiliers_area`], [`huiliers_area_centre`]; moved here from cdors-core's
//!   `model/area.rs`, which re-exports them unchanged).
//! - **Regions** ([`Region`], [`Regions`]): point in polygon in **planar lon/lat with straight
//!   edges**, as regionmask, shapely, rasterize and cdo's `selregion` treat region outlines (a
//!   long edge at high latitude is a straight line in lon/lat, not a great circle), with
//!   regionmask's rules for points on outlines, at −180°/0° and at −90° ([`Regions::members`]).
//!
//! Longitudes and latitudes are in degrees unless a function says radians.

use crate::mesh::Csr;
use crate::{MeshError, Result};
use rayon::prelude::*;

// ---------------------------------------------------------------------------------------------
// Unit vectors and great circles
// ---------------------------------------------------------------------------------------------

/// Unit vector of a point given in radians (`lonlat_to_xyz`, cdo `mpim_grid/grid_convert.h:9`).
#[inline]
pub fn xyz(lon: f64, lat: f64) -> [f64; 3] {
    let c = lat.cos();
    [c * lon.cos(), c * lon.sin(), lat.sin()]
}

/// Unit vector of a point given in degrees.
#[inline]
pub fn unit_vector(lon: f64, lat: f64) -> [f64; 3] {
    xyz(lon.to_radians(), lat.to_radians())
}

/// Longitude in (−180, 180] and latitude of a (not necessarily unit) vector, degrees.
pub fn to_lonlat(v: [f64; 3]) -> (f64, f64) {
    let h = v[0].hypot(v[1]);
    (v[1].atan2(v[0]).to_degrees(), v[2].atan2(h).to_degrees())
}

fn dot(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: &[f64; 3], b: &[f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn norm(a: &[f64; 3]) -> f64 {
    dot(a, a).sqrt()
}

/// Central angle (radians) between two points (degrees); multiply by the planet radius for a
/// distance. atan2(|a×b|, a·b), accurate for small and for near-antipodal angles.
pub fn distance(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    let (a, b) = (unit_vector(lon1, lat1), unit_vector(lon2, lat2));
    norm(&cross(&a, &b)).atan2(dot(&a, &b))
}

/// The point a fraction `f` (0..1) of the way from `a` to `b` along the great circle (`(lon,
/// lat)` in degrees). Undefined (NaN) for antipodal points, where the great circle is not
/// unique.
pub fn interpolate(a: (f64, f64), b: (f64, f64), f: f64) -> (f64, f64) {
    let (u, v) = (unit_vector(a.0, a.1), unit_vector(b.0, b.1));
    let d = norm(&cross(&u, &v)).atan2(dot(&u, &v));
    if d == 0.0 {
        return a;
    }
    let (s0, s1) = (((1.0 - f) * d).sin() / d.sin(), (f * d).sin() / d.sin());
    to_lonlat([
        s0 * u[0] + s1 * v[0],
        s0 * u[1] + s1 * v[1],
        s0 * u[2] + s1 * v[2],
    ])
}

/// Points from `a` to `b` (both included) along the great circle, equally spaced and at most
/// `step` radians apart. The first and last points are `a` and `b` exactly.
pub fn path(a: (f64, f64), b: (f64, f64), step: f64) -> Vec<(f64, f64)> {
    let d = distance(a.0, a.1, b.0, b.1);
    let n = if step > 0.0 {
        (d / step).ceil().max(1.0) as usize
    } else {
        1
    };
    let mut pts: Vec<(f64, f64)> = (0..n)
        .map(|k| interpolate(a, b, k as f64 / n as f64))
        .collect();
    pts[0] = a;
    pts.push(b);
    pts
}

// ---------------------------------------------------------------------------------------------
// Spherical polygon areas (cdo `grid_area.cc`)
// ---------------------------------------------------------------------------------------------

fn cross_norm(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    let c = [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ];
    (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt()
}

/// Spherical triangle area by L'Huilier's theorem (`mod_tri_area`, `grid_area.cc:138`).
fn tri_area(u: &[f64; 3], v: &[f64; 3], w: &[f64; 3]) -> f64 {
    let a = cross_norm(u, v).asin();
    let b = cross_norm(u, w).asin();
    let c = cross_norm(w, v).asin();
    let s = 0.5 * (a + b + c);
    let t = (s * 0.5).tan() * ((s - a) * 0.5).tan() * ((s - b) * 0.5).tan() * ((s - c) * 0.5).tan();
    (4.0 * t.abs().sqrt().atan()).abs()
}

/// Fan of triangles from vertex 0 (`mod_huiliers_area`, `grid_area.cc:180`). Radians.
pub fn huiliers_area(lon: &[f64], lat: &[f64]) -> f64 {
    let n = lon.len();
    if n < 3 {
        return 0.0;
    }
    let p1 = xyz(lon[0], lat[0]);
    let mut p2 = xyz(lon[1], lat[1]);
    let mut sum = 0.0;
    for i in 2..n {
        let p3 = xyz(lon[i], lat[i]);
        sum += tri_area(&p1, &p2, &p3);
        p2 = p3;
    }
    sum
}

/// Fan of triangles from the cell centre, skipping repeated vertices
/// (`mod_huiliers_area2`, `grid_area.cc:210`). Radians.
pub fn huiliers_area_centre(lon: &[f64], lat: &[f64], clon: f64, clat: f64) -> f64 {
    let n = lon.len();
    if n < 3 {
        return 0.0;
    }
    // is_equal (compare.h) is an exact comparison for these values
    let p1 = xyz(clon, clat);
    let mut p2 = xyz(lon[0], lat[0]);
    let mut sum = 0.0;
    for i in 1..n {
        if lon[i] == lon[i - 1] && lat[i] == lat[i - 1] {
            continue;
        }
        let p3 = xyz(lon[i], lat[i]);
        sum += tri_area(&p1, &p2, &p3);
        p2 = p3;
    }
    if !(lon[0] == lon[n - 1] && lat[0] == lat[n - 1]) {
        sum += tri_area(&p1, &p2, &xyz(lon[0], lat[0]));
    }
    sum
}

// ---------------------------------------------------------------------------------------------
// Regions: point in polygon in planar lon/lat
// ---------------------------------------------------------------------------------------------

/// One region: the rings (outer boundaries and holes, of all its parts) of a polygon or
/// multipolygon in lon/lat degrees, with straight edges. A point is inside if a ray from it
/// crosses the rings an odd number of times (even-odd rule), so holes are outside and the parts
/// of a valid multipolygon add up.
///
/// Prepared for many queries: the edges are binned into horizontal bands, so a test looks only
/// at the edges of the point's band (a few for regions of thousands of vertices).
#[derive(Clone)]
pub struct Region {
    /// xmin, ymin, xmax, ymax.
    bbox: [f64; 4],
    /// Edges as x1, y1, x2, y2 (horizontal edges dropped: they never cross a ray).
    edges: Vec<[f64; 4]>,
    /// Band height; band b covers ymin + b·dy ..
    dy: f64,
    /// Edges of band b: `band_edges[band_ptr[b]..band_ptr[b + 1]]`.
    band_ptr: Vec<u32>,
    band_edges: Vec<u32>,
}

impl Region {
    /// A region from its rings, each a list of (lon, lat) vertices; a closing vertex equal to the
    /// first is optional.
    pub fn new(rings: &[Vec<[f64; 2]>]) -> Result<Self> {
        let mut edges = Vec::new();
        let mut bbox = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        for ring in rings {
            let mut r: &[[f64; 2]] = ring;
            if r.len() > 1 && r[0] == r[r.len() - 1] {
                r = &r[..r.len() - 1];
            }
            if r.len() < 3 {
                return Err(MeshError::Invalid(format!(
                    "a region ring needs at least 3 vertices, found {}",
                    r.len()
                )));
            }
            for (i, p) in r.iter().enumerate() {
                if !(p[0].is_finite() && p[1].is_finite()) {
                    return Err(MeshError::Invalid(format!(
                        "region vertex {p:?} is not finite"
                    )));
                }
                bbox = [
                    bbox[0].min(p[0]),
                    bbox[1].min(p[1]),
                    bbox[2].max(p[0]),
                    bbox[3].max(p[1]),
                ];
                let q = r[(i + 1) % r.len()];
                if p[1] != q[1] {
                    edges.push([p[0], p[1], q[0], q[1]]);
                }
            }
        }
        if edges.is_empty() {
            return Err(MeshError::Invalid(
                "a region needs at least one ring".into(),
            ));
        }
        let nb = (edges.len() / 4).clamp(1, 1 << 16);
        let dy = (bbox[3] - bbox[1]) / nb as f64;
        let mut reg = Self {
            bbox,
            edges,
            dy,
            band_ptr: vec![0; nb + 1],
            band_edges: Vec::new(),
        };
        let span = |e: &[f64; 4], r: &Self| r.band(e[1].min(e[3]))..=r.band(e[1].max(e[3]));
        for e in &reg.edges {
            for b in span(e, &reg) {
                reg.band_ptr[b + 1] += 1;
            }
        }
        for b in 0..nb {
            reg.band_ptr[b + 1] += reg.band_ptr[b];
        }
        let mut next: Vec<u32> = reg.band_ptr[..nb].to_vec();
        let mut be = vec![0u32; reg.band_ptr[nb] as usize];
        for (i, e) in reg.edges.iter().enumerate() {
            for b in span(e, &reg) {
                be[next[b] as usize] = i as u32;
                next[b] += 1;
            }
        }
        reg.band_edges = be;
        Ok(reg)
    }

    /// The band of latitude y (monotone in y, so an edge's bands cover all its latitudes).
    #[inline]
    fn band(&self, y: f64) -> usize {
        let nb = self.band_ptr.len() - 1;
        if self.dy > 0.0 {
            (((y - self.bbox[1]) / self.dy) as usize).min(nb - 1)
        } else {
            0
        }
    }

    /// xmin, ymin, xmax, ymax.
    pub fn bounds(&self) -> [f64; 4] {
        self.bbox
    }

    /// Whether (x, y) is inside (even-odd rule; a point exactly on an edge may go either way:
    /// [`Regions::members`] shifts points off the outlines first).
    #[inline]
    pub fn contains(&self, x: f64, y: f64) -> bool {
        let b = &self.bbox;
        if !(x >= b[0] && x <= b[2] && y >= b[1] && y <= b[3]) {
            return false;
        }
        let k = self.band(y);
        let mut inside = false;
        for &i in &self.band_edges[self.band_ptr[k] as usize..self.band_ptr[k + 1] as usize] {
            let [x1, y1, x2, y2] = self.edges[i as usize];
            if (y1 > y) != (y2 > y) && x < (x2 - x1) * (y - y1) / (y2 - y1) + x1 {
                inside = !inside;
            }
        }
        inside
    }
}

impl std::fmt::Debug for Region {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Region({} edges, bounds {:?})",
            self.edges.len(),
            self.bbox
        )
    }
}

/// Which longitude range a set of regions is drawn in; points are wrapped into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LonBase {
    /// −180..180: points wrapped to [−180, 180).
    Lon180,
    /// 0..360: points wrapped to [0, 360).
    Lon360,
}

/// A set of regions masked with regionmask's rules (`regionmask/core/mask.py`, v0.13).
#[derive(Debug, Clone)]
pub struct Regions {
    pub regions: Vec<Region>,
    pub base: LonBase,
    /// The set's overlap flag: overlapping regions each get a shared cell; without it a cell
    /// goes to the last region containing it (regionmask's 2-D mask, made 3-D).
    pub overlap: bool,
}

/// regionmask's shift that gives points on an outline a consistent side: 1e-8° west and 1e-10°
/// south (`_mask_shapely`), so a point on an edge belongs to the region west or south of it.
const SHIFT_LON: f64 = 1e-8;
const SHIFT_LAT: f64 = 1e-10;

/// `np.isclose(a, b)` with its default tolerances.
#[inline]
fn isclose(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-8 + 1e-5 * b.abs()
}

impl Regions {
    /// The base follows regionmask's `_is_180` on the total bounds: 0..360 if any vertex lies
    /// east of 180° (rounded to 6 decimals), an error if the set spans both below 0° and above
    /// 180°.
    pub fn new(regions: Vec<Region>, overlap: bool) -> Result<Self> {
        let round6 = |x: f64| (x * 1e6).round() / 1e6;
        let lo = regions
            .iter()
            .map(|r| r.bbox[0])
            .fold(f64::INFINITY, f64::min);
        let hi = regions
            .iter()
            .map(|r| r.bbox[2])
            .fold(f64::NEG_INFINITY, f64::max);
        let (lo, hi) = (round6(lo), round6(hi));
        if lo < 0.0 && hi > 180.0 {
            return Err(MeshError::Invalid(format!(
                "regions span longitudes {lo}..{hi}: below 0 and above 180 at once"
            )));
        }
        let base = if hi <= 180.0 {
            LonBase::Lon180
        } else {
            LonBase::Lon360
        };
        Ok(Self {
            regions,
            base,
            overlap,
        })
    }

    /// A longitude wrapped into the base (`_wrapAngle180` / `_wrapAngle360`).
    pub fn wrap_lon(&self, lon: f64) -> f64 {
        match self.base {
            LonBase::Lon180 if (-180.0..180.0).contains(&lon) => lon,
            LonBase::Lon180 => (lon + 180.0).rem_euclid(360.0) - 180.0,
            LonBase::Lon360 => lon.rem_euclid(360.0),
        }
    }

    /// The cells (centres `lon`, `lat`) inside each region: a CSR with one row per region and
    /// the cell indices in ascending order (no weights), as regionmask's `mask_3D` with the
    /// set's overlap flag: with [`Regions::overlap`] every region containing a cell lists it,
    /// otherwise only the last one (AR6 `all` has slivers where land and ocean regions overlap).
    ///
    /// The rules of regionmask's `_mask` for unstructured points:
    /// 1. longitudes are wrapped into the regions' base;
    /// 2. a cell is in a region if its centre, shifted 1e-8° west and 1e-10° south, is inside;
    /// 3. centres at −180° (0° if no wrapped longitude is negative) and at −90° (both within
    ///    `np.isclose`) are tested again: west-shifted and moved by +360°, and with the latitude
    ///    set to −90° + 1e-10° at the pole; a region found here is added (without overlap: only
    ///    for cells no region holds yet).
    ///
    /// regionmask takes a rasterizing path for equally spaced lon-lat grids; its edge convention
    /// is the same on outlines through the cell centres (checked for edges and corners).
    pub fn members(&self, lon: &[f64], lat: &[f64]) -> Csr {
        assert_eq!(lon.len(), lat.len());
        let n = lon.len();
        let wrapped: Vec<f64> = lon.par_iter().map(|&x| self.wrap_lon(x)).collect();
        let edge_lon = if wrapped.iter().any(|&x| x < 0.0) {
            -180.0
        } else {
            0.0
        };
        const CHUNK: usize = 1 << 16;
        let pairs: Vec<Vec<(u32, u32)>> = (0..n.div_ceil(CHUNK))
            .into_par_iter()
            .map(|c| {
                let mut out = Vec::new();
                for i in c * CHUNK..((c + 1) * CHUNK).min(n) {
                    let (x, y) = (wrapped[i], lat[i]);
                    if x.is_nan() || y.is_nan() {
                        continue;
                    }
                    let at_edge = isclose(x, edge_lon);
                    let at_pole = isclose(y, -90.0);
                    let (xs, ys) = (x - SHIFT_LON, y - SHIFT_LAT);
                    let (xe, ye) = (
                        if at_edge { xs + 360.0 } else { xs },
                        if at_pole { -90.0 + SHIFT_LAT } else { ys },
                    );
                    if self.overlap {
                        for (r, reg) in self.regions.iter().enumerate() {
                            if reg.contains(xs, ys)
                                || ((at_edge || at_pole) && reg.contains(xe, ye))
                            {
                                out.push((r as u32, i as u32));
                            }
                        }
                    } else {
                        // the last region wins; the edge pass only for unassigned cells
                        let last = |x, y| self.regions.iter().rposition(|r| r.contains(x, y));
                        let mut r = last(xs, ys);
                        if r.is_none() && (at_edge || at_pole) {
                            r = last(xe, ye);
                        }
                        if let Some(r) = r {
                            out.push((r as u32, i as u32));
                        }
                    }
                }
                out
            })
            .collect();
        // counting sort by region; chunks are in cell order, so cells stay ascending
        let nr = self.regions.len();
        let mut row_ptr = vec![0usize; nr + 1];
        for &(r, _) in pairs.iter().flatten() {
            row_ptr[r as usize + 1] += 1;
        }
        for r in 0..nr {
            row_ptr[r + 1] += row_ptr[r];
        }
        let mut next = row_ptr.clone();
        let mut col = vec![0u32; row_ptr[nr]];
        for &(r, i) in pairs.iter().flatten() {
            col[next[r as usize]] = i;
            next[r as usize] += 1;
        }
        Csr {
            row_ptr,
            col,
            w: None,
            ncols: n,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x0: f64, y0: f64, d: f64) -> Vec<[f64; 2]> {
        vec![
            [x0, y0],
            [x0 + d, y0],
            [x0 + d, y0 + d],
            [x0, y0 + d],
            [x0, y0],
        ]
    }

    /// Region of each point (`None` outside all), from the membership table.
    fn which(rs: &Regions, pts: &[(f64, f64)]) -> Vec<Option<usize>> {
        let lon: Vec<f64> = pts.iter().map(|p| p.0).collect();
        let lat: Vec<f64> = pts.iter().map(|p| p.1).collect();
        let m = rs.members(&lon, &lat);
        (0..pts.len())
            .map(|i| (0..m.rows()).find(|&r| m.row(r).contains(&(i as u32))))
            .collect()
    }

    #[test]
    fn huiliers_area_unchanged() {
        // bit patterns from cdors-core's model/area.rs before the move (master 1e5d801)
        let d = std::f64::consts::PI / 180.0;
        let a = huiliers_area(
            &[0.0, 10.0 * d, 10.0 * d, 0.0],
            &[0.0, 0.0, 10.0 * d, 10.0 * d],
        );
        let b = huiliers_area_centre(
            &[0.0, 0.1, 0.1, 0.05, 0.0, 0.0],
            &[0.5, 0.5, 0.6, 0.65, 0.6, 0.6],
            0.05,
            0.57,
        );
        assert_eq!((a.to_bits(), b.to_bits()), (HUILIERS_A, HUILIERS_B));
    }
    const HUILIERS_A: u64 = 0x3f9f_1c80_0532_8ce2;
    const HUILIERS_B: u64 = 0x3f85_a1e4_5500_ae94;

    #[test]
    fn great_circle() {
        // a quarter of the equator, and London to New York (5570 km on a 6371 km sphere)
        assert!((distance(0.0, 0.0, 90.0, 0.0) - std::f64::consts::FRAC_PI_2).abs() < 1e-15);
        let d = distance(-0.1278, 51.5074, -74.0060, 40.7128) * 6371.0;
        assert!((d - 5570.2).abs() < 0.5, "{d}");
        let p = path((0.0, 0.0), (90.0, 0.0), 0.1);
        assert_eq!(p.len(), 17); // ceil((pi/2) / 0.1) = 16 segments
        assert!((p[8].0 - 45.0).abs() < 1e-12 && p[8].1.abs() < 1e-12);
    }

    #[test]
    fn regions_rules() {
        // four 10° squares meeting at (0, 0): 0 SW, 1 SE, 2 NW, 3 NE; same answers as
        // regionmask 0.13 (both its rasterize and shapely paths)
        let sq = |x, y| Region::new(&[square(x, y, 10.0)]).unwrap();
        let rs = Regions::new(
            vec![
                sq(-10.0, -10.0),
                sq(0.0, -10.0),
                sq(-10.0, 0.0),
                sq(0.0, 0.0),
            ],
            false,
        )
        .unwrap();
        let pts = [
            (0.0, 0.0),
            (0.0, 5.0),
            (5.0, 0.0),
            (-10.0, 5.0),
            (10.0, 5.0),
            (5.0, 10.0),
        ];
        let want = [Some(0), Some(2), Some(1), None, Some(3), Some(3)];
        assert_eq!(which(&rs, &pts), want);

        // the date line and the South Pole: regions touching -90 at both ends of the globe
        let a = Region::new(&[square(170.0, -90.0, 10.0)]).unwrap();
        let b = Region::new(&[square(-180.0, -90.0, 10.0)]).unwrap();
        let rs = Regions::new(vec![a, b], true).unwrap();
        assert_eq!(rs.base, LonBase::Lon180);
        let pts = [
            (-180.0, -85.0), // -> 180 - 1e-8: region 0
            (180.0, -85.0),  // wraps to -180, same
            (-175.0, -90.0), // nudged north: region 1
            (175.0, -90.0),
            (-180.0, -90.0),
            (170.0, -85.0),  // west edge of region 0: goes west, outside
            (-170.0, -85.0), // east edge of region 1: inside
        ];
        let want = [Some(0), Some(0), Some(1), Some(0), Some(0), None, Some(1)];
        assert_eq!(which(&rs, &pts), want);

        // a hole is outside
        let ring = Region::new(&[square(0.0, 0.0, 10.0), square(4.0, 4.0, 2.0)]).unwrap();
        assert!(ring.contains(2.0, 2.0) && !ring.contains(5.0, 5.0));

        // a long edge at high latitude is straight in lon/lat: from (0, 60) to (100, 80) it
        // passes lon 50 at lat 70, while the great circle passes it much further north
        let r =
            Region::new(&[vec![[0.0, 50.0], [100.0, 50.0], [100.0, 80.0], [0.0, 60.0]]]).unwrap();
        assert!(r.contains(50.0, 69.9) && !r.contains(50.0, 70.1) && !r.contains(50.0, 75.0));
        let nrm = cross(&unit_vector(0.0, 60.0), &unit_vector(100.0, 80.0));
        let l = 50f64.to_radians();
        let gc = (-(nrm[0] * l.cos() + nrm[1] * l.sin()) / nrm[2])
            .atan()
            .to_degrees();
        assert!(gc > 75.0, "{gc}");
    }

    #[test]
    fn region_bands_match_brute_force() {
        // a star with many edges: banded test == plain even-odd over all edges
        let n = 400;
        let ring: Vec<[f64; 2]> = (0..n)
            .map(|k| {
                let t = k as f64 / n as f64 * std::f64::consts::TAU;
                let r = if k % 2 == 0 { 30.0 } else { 12.0 };
                [r * t.cos(), r * t.sin()]
            })
            .collect();
        let reg = Region::new(&[ring.clone()]).unwrap();
        let brute = |x: f64, y: f64| {
            let mut inside = false;
            for i in 0..n {
                let ([x1, y1], [x2, y2]) = (ring[i], ring[(i + 1) % n]);
                if (y1 > y) != (y2 > y) && x < (x2 - x1) * (y - y1) / (y2 - y1) + x1 {
                    inside = !inside;
                }
            }
            inside
        };
        for i in 0..200 {
            for j in 0..200 {
                let (x, y) = (-31.0 + 0.31 * i as f64, -31.0 + 0.31 * j as f64);
                assert_eq!(reg.contains(x, y), brute(x, y), "({x}, {y})");
            }
        }
    }
}
