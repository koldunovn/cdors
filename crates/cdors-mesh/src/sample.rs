//! Sampling of fields at points and along paths (`selpoints`, `transect`): stations, great-circle
//! paths, and their weights as a [`Csr`] (rows: stations or path points, columns: the cells of one
//! place, every non-empty row summing to 1).
//!
//! - **Stations** ([`parse_stations_csv`], [`parse_point`]): a CSV of `name,lon,lat` lines (an
//!   optional header line, `#` comments, no quoting), or points written as cdo writes them for
//!   `remapnn,lon=<x>_lat=<y>` (separator `_`, `/` or `x`, case ignored). Longitudes stay as given.
//! - **Paths** ([`waypoints`], [`parse_geojson_path`], [`sample_path`]): great-circle segments
//!   through two or more waypoints, or a GeoJSON LineString (a geometry, a Feature, or a
//!   FeatureCollection with one feature: one path per call). Each segment of length `d` is cut
//!   into `n = ceil(d / step)` equal arcs; its points are both ends and the `n - 1` points
//!   between, a waypoint shared by two segments appears once, and zero-length segments are
//!   skipped. The output is the cumulative distance (km, sphere of radius 6371 km), longitude in
//!   (−180, 180] and latitude of every point. Segments through a pole or across ±180° need
//!   nothing special (interpolation is on unit vectors); antipodal waypoints are refused (their
//!   great circle is not unique).
//! - **Default step** ([`Sampler::default_step_km`]): the median cell spacing along the path:
//!   the path is probed at 1024 equal arcs, the distinct cells nearest to the probes are
//!   collected, and the step is the lower median of each such cell's distance to its nearest
//!   other cell (cells at distance 0, duplicates, are passed over).
//! - **`nn` weights**: the nearest cell (KD-tree on unit vectors; equal distances: the smallest
//!   cell index), weight 1.
//! - **`linear` weights**: barycentric weights in a triangle of cells containing the point. On a
//!   place with a triangle table (FESOM2 nodes: `elem_nodes`) the triangles are the mesh's: the
//!   point is looked for in the elements around the nearest cell and their neighbours (two rings),
//!   then by walking across edges from the best of them (at most 64 steps). On other places (grid
//!   cells, FESOM2 element centres) the triangle is the one of the local spherical Delaunay
//!   triangulation of the 12 (then 48) nearest cells that contains the point; ties (cocircular
//!   cells, as on lon-lat grids) go to the triangle with the smallest cell indices.
//!   The weights are those of the point's central projection onto the triangle's plane (`w_a ∝ q ·
//!   (b × c)`, ...): they sum to 1 and reproduce exactly any field linear in x, y, z on that plane
//!   (FESOM2's P1 elements are linear on flat triangles). At q itself a field linear in x, y, z is
//!   off by O(h²) relative (the projection shortens q by 1 − n·q ≈ h²/8 for spacing h in radians);
//!   no 3-point scheme with weights summing to 1 does better on a sphere. Weights below 1e-12
//!   (points on an edge or a vertex) are dropped and the rest renormalised, so every listed cell
//!   matters. A point in no triangle (on land outside the mesh, beyond a regional grid) gets an
//!   empty row: missing, counted in [`Sampled::outside`] for a warning.
//! - **Levels** ([`wet_levels`], [`bottom_depth`]): a level is valid at a point only if every cell
//!   of its row is wet there (the intersection of their wet ranges), so values below the shallowest
//!   bottom of the cells are missing; `bottom_depth` is |interface| at the bottom of that range.
//!
//! **Determinism.** Points are processed in parallel but each row depends only on its point, and
//! every choice (nearest cell, candidate triangle, tie) is broken by cell or element index: the
//! weights are bit-identical for any number of threads.

use crate::geom::{distance, path, unit_vector};
use crate::locate::KdTree;
use crate::{Csr, Mesh, MeshError, Place, Result, WetLevels};
use rayon::prelude::*;
use std::sync::Arc;

/// Planet radius of path distances and steps (km), as cdo and nereus.
pub const EARTH_RADIUS_KM: f64 = 6371.0;
/// Weights at or below this are dropped; also the tolerance of the inside tests.
const EPS: f64 = 1e-12;
/// Most points a path may have.
const MAX_POINTS: usize = 10_000_000;
/// Probes of the path for the default step.
const PROBES: f64 = 1024.0;
/// Neighbour counts of the local Delaunay triangulation, tried in turn.
const LOCAL_K: [usize; 2] = [12, 48];
/// Most steps of the walk across mesh triangles.
const WALK: usize = 64;

fn invalid(m: impl Into<String>) -> MeshError {
    MeshError::Invalid(m.into())
}

// ---------------------------------------------------------------------------------------------
// Stations and paths
// ---------------------------------------------------------------------------------------------

/// A sampling point with a name.
#[derive(Debug, Clone, PartialEq)]
pub struct Station {
    pub name: String,
    pub lon: f64,
    pub lat: f64,
}

/// The longest prefix of `s` that is a number (as `%lf` scans it), and the rest.
fn scan_f64(s: &str) -> Option<(f64, &str)> {
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E')))
        .unwrap_or(s.len());
    (1..=end)
        .rev()
        .find_map(|n| s[..n].parse::<f64>().ok().map(|v| (v, &s[n..])))
}

/// A point written `lon=<x>_lat=<y>` (cdo's `remapnn,lon=<x>_lat=<y>`; separator `_`, `/` or
/// `x`; case ignored). `None` if `s` is not of this form or the latitude is outside [−90, 90].
pub fn parse_point(s: &str) -> Option<(f64, f64)> {
    let lc = s.trim().to_ascii_lowercase();
    let (lon, r) = scan_f64(lc.strip_prefix("lon=")?)?;
    let r = r.strip_prefix(['_', '/', 'x'])?.strip_prefix("lat=")?;
    let (lat, r) = scan_f64(r)?;
    (r.is_empty() && lon.is_finite() && (-90.0..=90.0).contains(&lat)).then_some((lon, lat))
}

/// Stations from CSV text: `name,lon,lat` per line, blank lines and `#` comments skipped; a
/// first line whose lon and lat are not numbers is a header.
pub fn parse_stations_csv(text: &str) -> Result<Vec<Station>> {
    let mut out = Vec::new();
    let mut first = true;
    for (k, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split(',').map(str::trim).collect();
        let nums = (f.len() == 3)
            .then(|| Some((f[1].parse::<f64>().ok()?, f[2].parse::<f64>().ok()?)))
            .flatten();
        match nums {
            Some((lon, lat)) if lon.is_finite() && (-90.0..=90.0).contains(&lat) => {
                out.push(Station {
                    name: f[0].to_owned(),
                    lon,
                    lat,
                })
            }
            None if first && f.len() == 3 => {}
            _ => {
                return Err(invalid(format!(
                    "line {}: '{line}' is not name,lon,lat (degrees, latitude in [-90, 90])",
                    k + 1
                )));
            }
        }
        first = false;
    }
    if out.is_empty() {
        return Err(invalid("no stations (expected lines name,lon,lat)"));
    }
    Ok(out)
}

/// Waypoints from `lon0, lat0, lon1, lat1, ...` (at least two points).
pub fn waypoints(values: &[f64]) -> Result<Vec<(f64, f64)>> {
    if values.len() < 4 || !values.len().is_multiple_of(2) {
        return Err(invalid(format!(
            "a path needs pairs lon,lat of at least two points ({} numbers given)",
            values.len()
        )));
    }
    let pts: Vec<(f64, f64)> = values.chunks(2).map(|p| (p[0], p[1])).collect();
    check_points(&pts)?;
    Ok(pts)
}

fn check_points(pts: &[(f64, f64)]) -> Result<()> {
    if let Some((x, y)) = pts
        .iter()
        .find(|(x, y)| !x.is_finite() || !(-90.0..=90.0).contains(y))
    {
        return Err(invalid(format!(
            "waypoint ({x}, {y}): longitude must be finite, latitude in [-90, 90]"
        )));
    }
    Ok(())
}

/// The waypoints of a GeoJSON LineString: a geometry, a Feature, or a FeatureCollection of one
/// feature (`[lon, lat, ...]` positions; further coordinates are ignored).
pub fn parse_geojson_path(bytes: &[u8]) -> Result<Vec<(f64, f64)>> {
    use serde_json::Value;
    let v: Value = serde_json::from_slice(bytes).map_err(|e| invalid(format!("GeoJSON: {e}")))?;
    let mut g = &v;
    if g["type"] == "FeatureCollection" {
        let f = g["features"].as_array().map_or(&[][..], Vec::as_slice);
        if f.len() != 1 {
            return Err(invalid(format!(
                "GeoJSON: a FeatureCollection of {} features; one path per call (one LineString)",
                f.len()
            )));
        }
        g = &f[0];
    }
    if g["type"] == "Feature" {
        g = &g["geometry"];
    }
    if g["type"] != "LineString" {
        return Err(invalid(format!(
            "GeoJSON: geometry {}; a path is one LineString",
            g["type"]
        )));
    }
    let pts: Option<Vec<(f64, f64)>> = g["coordinates"].as_array().and_then(|c| {
        c.iter()
            .map(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?)))
            .collect()
    });
    let pts =
        pts.ok_or_else(|| invalid("GeoJSON: LineString coordinates are not [lon, lat] pairs"))?;
    if pts.len() < 2 {
        return Err(invalid(
            "GeoJSON: a LineString needs at least two positions",
        ));
    }
    check_points(&pts)?;
    Ok(pts)
}

/// The sample points of a path.
#[derive(Debug, Clone, PartialEq)]
pub struct PathSamples {
    /// Cumulative distance from the first waypoint (km).
    pub distance_km: Vec<f64>,
    /// Degrees; longitudes in (−180, 180].
    pub lon: Vec<f64>,
    pub lat: Vec<f64>,
    pub step_km: f64,
    pub length_km: f64,
}

fn wrap_lon(x: f64) -> f64 {
    let w = x.rem_euclid(360.0);
    if w > 180.0 { w - 360.0 } else { w }
}

/// The points of the path through `waypoints` at most `step_km` apart (see the module docs).
pub fn sample_path(waypoints: &[(f64, f64)], step_km: f64) -> Result<PathSamples> {
    if !(step_km > 0.0 && step_km.is_finite()) {
        return Err(invalid(format!(
            "step={step_km}: must be a positive number of km"
        )));
    }
    check_points(waypoints)?;
    let step = step_km / EARTH_RADIUS_KM;
    let first = waypoints
        .first()
        .ok_or_else(|| invalid("a path needs at least two points"))?;
    let mut out = PathSamples {
        distance_km: vec![0.0],
        lon: vec![wrap_lon(first.0)],
        lat: vec![first.1],
        step_km,
        length_km: 0.0,
    };
    let mut cum = 0.0;
    for s in waypoints.windows(2) {
        let (a, b) = (s[0], s[1]);
        let d = distance(a.0, a.1, b.0, b.1);
        if d == 0.0 {
            continue;
        }
        if d > std::f64::consts::PI - 1e-9 {
            return Err(invalid(format!(
                "waypoints ({}, {}) and ({}, {}) are antipodal: their great circle is not \
                 unique; add a waypoint between them",
                a.0, a.1, b.0, b.1
            )));
        }
        let n = (d / step).ceil().max(1.0);
        if out.lon.len() as f64 + n > MAX_POINTS as f64 {
            return Err(invalid(format!(
                "step={step_km} km gives more than {MAX_POINTS} points"
            )));
        }
        let pts = path(a, b, step);
        let n = pts.len() - 1;
        for (k, &(x, y)) in pts.iter().enumerate().skip(1) {
            out.distance_km
                .push((cum + d * k as f64 / n as f64) * EARTH_RADIUS_KM);
            out.lon.push(wrap_lon(x));
            out.lat.push(y);
        }
        cum += d;
    }
    if out.lon.len() < 2 {
        return Err(invalid("the path has length 0 (all waypoints equal)"));
    }
    out.length_km = cum * EARTH_RADIUS_KM;
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Weights
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Nn,
    Linear,
}

impl Method {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "nn" => Some(Self::Nn),
            "linear" => Some(Self::Linear),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Nn => "nn",
            Self::Linear => "linear",
        }
    }
}

/// Weights of a set of points.
#[derive(Debug, Clone, PartialEq)]
pub struct Sampled {
    /// Rows: the points; columns: the cells of the place, ascending in each row.
    pub weights: Csr,
    /// Points with an empty row (no containing triangle for `linear`).
    pub outside: usize,
}

/// The triangle table over the cells of `place`: a table of `mesh` whose columns are the place
/// and whose rows all have three cells (FESOM2 `elem_nodes` for the nodes); `None` if there is
/// none.
pub fn triangles(mesh: &Mesh, place: &str) -> Result<Option<Arc<Csr>>> {
    for t in mesh.topology.iter().filter(|t| t.cols_place == place) {
        let csr = t.csr.get()?;
        if (0..csr.rows()).all(|r| csr.range(r).len() == 3) {
            return Ok(Some(csr));
        }
    }
    Ok(None)
}

struct Triangles {
    /// triangle → its three cells
    tri: Arc<Csr>,
    /// cell → its triangles, ascending
    of_cell: Csr,
}

/// Point lookups on one place: its KD-tree and, if it has one, its triangle table.
pub struct Sampler<'a> {
    place: &'a Place,
    tree: KdTree,
    pts: Vec<[f64; 3]>,
    tri: Option<Triangles>,
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

fn sub(a: &[f64; 3], b: &[f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// Weights of the central projection of `q` onto the plane of the triangle `p` (unit vectors),
/// and whether `q` lies in the spherical triangle (all weights ≥ −EPS, same side as the
/// triangle). `None` for a degenerate triangle (collinear or on one great circle).
fn barycentric(q: &[f64; 3], p: [&[f64; 3]; 3]) -> Option<([f64; 3], bool)> {
    let n = cross(&sub(p[1], p[0]), &sub(p[2], p[0]));
    let nn = dot(&n, &n).sqrt();
    let det = dot(p[0], &cross(p[1], p[2]));
    if nn == 0.0 || nn.is_nan() || det.abs() <= 1e-9 * nn {
        return None;
    }
    let m = [
        dot(q, &cross(p[1], p[2])),
        dot(q, &cross(p[2], p[0])),
        dot(q, &cross(p[0], p[1])),
    ];
    let s = m[0] + m[1] + m[2];
    let w = [m[0] / s, m[1] / s, m[2] / s];
    let inside = s * det > 0.0 && w.iter().all(|&x| x >= -EPS);
    Some((w, inside))
}

/// A row from (cell, weight) pairs: weights ≤ EPS dropped, the rest renormalised, cells
/// ascending.
fn clean_row(mut e: Vec<(u32, f64)>) -> Vec<(u32, f64)> {
    e.retain(|&(_, w)| w > EPS);
    let s: f64 = e.iter().map(|p| p.1).sum();
    e.iter_mut().for_each(|p| p.1 /= s);
    e.sort_by_key(|p| p.0);
    e
}

impl<'a> Sampler<'a> {
    /// Lookups on `place`; `triangles` (see [`triangles`]) enables mesh triangles for `linear`.
    pub fn new(place: &'a Place, triangles: Option<Arc<Csr>>) -> Result<Self> {
        let tri = match triangles {
            Some(t) if t.ncols != place.size => {
                return Err(invalid(format!(
                    "triangle table over {} cells, place '{}' has {}",
                    t.ncols, place.name, place.size
                )));
            }
            Some(t) => Some(Triangles {
                of_cell: t.transpose(),
                tri: t,
            }),
            None => None,
        };
        let pts = (0..place.size)
            .into_par_iter()
            .map(|i| unit_vector(place.lon[i], place.lat[i]))
            .collect();
        Ok(Self {
            place,
            tree: KdTree::new(&place.lon, &place.lat),
            pts,
            tri,
        })
    }

    /// The default step of a path (km): the median cell spacing along it (module docs).
    pub fn default_step_km(&self, waypoints: &[(f64, f64)]) -> Result<f64> {
        let probe = sample_path(waypoints, 1.0)?;
        let probe = sample_path(waypoints, probe.length_km / PROBES)?;
        let mut cells: Vec<usize> = self
            .tree
            .nearest_many(&probe.lon, &probe.lat)
            .into_iter()
            .flatten()
            .map(|(c, _)| c)
            .collect();
        cells.sort_unstable();
        cells.dedup();
        let mut d: Vec<f64> = cells
            .par_iter()
            .filter_map(|&c| {
                let nb = self.tree.nearest_k_xyz(self.pts[c], 2);
                let other = nb.iter().find(|&&(i, d)| i != c && d > 0.0)?;
                Some(2.0 * (0.5 * other.1.sqrt()).min(1.0).asin() * EARTH_RADIUS_KM)
            })
            .collect();
        d.sort_by(f64::total_cmp);
        d.get(d.len().saturating_sub(1) / 2)
            .copied()
            .filter(|&s| s > 0.0)
            .ok_or_else(|| {
                invalid(format!(
                    "place '{}': no cell spacing along the path (give step=<km>)",
                    self.place.name
                ))
            })
    }

    /// Weights of the points `lon`, `lat` (degrees) by `method`.
    pub fn weights(&self, method: Method, lon: &[f64], lat: &[f64]) -> Sampled {
        let rows: Vec<Vec<(u32, f64)>> = (0..lon.len())
            .into_par_iter()
            .map(|i| {
                let q = unit_vector(lon[i], lat[i]);
                match method {
                    Method::Nn => self
                        .tree
                        .nearest_xyz(q)
                        .map(|(c, _)| vec![(c as u32, 1.0)])
                        .unwrap_or_default(),
                    Method::Linear => match &self.tri {
                        Some(t) => self.in_mesh(t, &q),
                        None => self.in_local(&q),
                    },
                }
            })
            .collect();
        let outside = rows.iter().filter(|r| r.is_empty()).count();
        let mut row_ptr = Vec::with_capacity(rows.len() + 1);
        row_ptr.push(0);
        let (mut col, mut w) = (Vec::new(), Vec::new());
        for r in rows {
            for (c, x) in r {
                col.push(c);
                w.push(x);
            }
            row_ptr.push(col.len());
        }
        Sampled {
            weights: Csr {
                row_ptr,
                col,
                w: Some(w),
                ncols: self.place.size,
            },
            outside,
        }
    }

    fn tri_weights(&self, t: &Triangles, e: usize, q: &[f64; 3]) -> Option<([f64; 3], bool)> {
        let c = t.tri.row(e);
        barycentric(
            q,
            [
                &self.pts[c[0] as usize],
                &self.pts[c[1] as usize],
                &self.pts[c[2] as usize],
            ],
        )
    }

    fn tri_row(t: &Triangles, e: usize, w: [f64; 3]) -> Vec<(u32, f64)> {
        let c = t.tri.row(e);
        clean_row((0..3).map(|k| (c[k], w[k])).collect())
    }

    /// Linear weights in the mesh triangle containing `q`; empty if there is none.
    fn in_mesh(&self, t: &Triangles, q: &[f64; 3]) -> Vec<(u32, f64)> {
        let Some((n0, _)) = self.tree.nearest_xyz(*q) else {
            return Vec::new();
        };
        // two rings of triangles around the nearest cell, ascending
        let mut cand: Vec<u32> = Vec::new();
        for &e in t.of_cell.row(n0) {
            for &c in t.tri.row(e as usize) {
                cand.extend_from_slice(t.of_cell.row(c as usize));
            }
        }
        cand.sort_unstable();
        cand.dedup();
        let mut best: Option<(usize, f64)> = None;
        for &e in &cand {
            let Some((w, inside)) = self.tri_weights(t, e as usize, q) else {
                continue;
            };
            if inside {
                return Self::tri_row(t, e as usize, w);
            }
            let m = w[0].min(w[1]).min(w[2]);
            if best.is_none_or(|b| m > b.1) {
                best = Some((e as usize, m));
            }
        }
        // walk across the edge facing the most negative weight
        let Some((mut e, _)) = best else {
            return Vec::new();
        };
        let mut seen = vec![e];
        for _ in 0..WALK {
            let Some((w, inside)) = self.tri_weights(t, e, q) else {
                return Vec::new();
            };
            if inside {
                return Self::tri_row(t, e, w);
            }
            let k = (0..3).min_by(|&i, &j| w[i].total_cmp(&w[j])).unwrap();
            let c = t.tri.row(e);
            let (u, v) = (c[(k + 1) % 3] as usize, c[(k + 2) % 3] as usize);
            let next = t
                .of_cell
                .row(u)
                .iter()
                .find(|&&f| f as usize != e && t.of_cell.row(v).contains(&f));
            match next {
                Some(&f) if !seen.contains(&(f as usize)) => {
                    e = f as usize;
                    seen.push(e);
                }
                _ => return Vec::new(), // the mesh boundary, or a cycle
            }
        }
        Vec::new()
    }

    /// Linear weights in the triangle of the local Delaunay triangulation of the nearest cells
    /// that contains `q`; empty if there is none.
    fn in_local(&self, q: &[f64; 3]) -> Vec<(u32, f64)> {
        for k in LOCAL_K {
            let mut near: Vec<usize> = self
                .tree
                .nearest_k_xyz(*q, k)
                .into_iter()
                .map(|p| p.0)
                .collect();
            near.sort_unstable();
            let p: Vec<&[f64; 3]> = near.iter().map(|&c| &self.pts[c]).collect();
            let m = near.len();
            for a in 0..m {
                for b in a + 1..m {
                    for c in b + 1..m {
                        let Some((w, true)) = barycentric(q, [p[a], p[b], p[c]]) else {
                            continue;
                        };
                        // Delaunay: no other cell strictly inside the circumcircle (the cap cut
                        // off by the triangle's plane, on the side away from the centre)
                        let mut nrm = cross(&sub(p[b], p[a]), &sub(p[c], p[a]));
                        if dot(&nrm, p[a]) < 0.0 {
                            nrm = [-nrm[0], -nrm[1], -nrm[2]];
                        }
                        let len = dot(&nrm, &nrm).sqrt();
                        let h = dot(&nrm, p[a]) / len;
                        let empty = (0..m)
                            .filter(|&d| d != a && d != b && d != c)
                            .all(|d| dot(&nrm, p[d]) / len <= h + EPS);
                        if empty {
                            let cells = [near[a], near[b], near[c]];
                            return clean_row((0..3).map(|i| (cells[i] as u32, w[i])).collect());
                        }
                    }
                }
            }
            if m < k {
                break; // the place has no more cells
            }
        }
        Vec::new()
    }
}

// ---------------------------------------------------------------------------------------------
// Levels
// ---------------------------------------------------------------------------------------------

/// The wet mid-levels of each point: the intersection of the wet ranges of the cells of its row
/// (`wet` is the wet levels of the weights' place). Empty rows and empty intersections give an
/// empty range (0..0).
pub fn wet_levels(w: &Csr, wet: &WetLevels) -> WetLevels {
    let (mut top, mut bottom) = (Vec::with_capacity(w.rows()), Vec::with_capacity(w.rows()));
    for r in 0..w.rows() {
        let cells = w.row(r);
        let t = cells.iter().map(|&c| wet.top[c as usize]).max();
        let b = cells.iter().map(|&c| wet.bottom[c as usize]).min();
        match (t, b) {
            (Some(t), Some(b)) if t < b => {
                top.push(t);
                bottom.push(b);
            }
            _ => {
                top.push(0);
                bottom.push(0);
            }
        }
    }
    WetLevels {
        place: "points".into(),
        top,
        bottom,
    }
}

/// Depth of the bottom of each point's wet range: |interfaces[bottom]| (m); NaN for points
/// without wet levels.
pub fn bottom_depth(wet: &WetLevels, interfaces: &[f64]) -> Vec<f64> {
    (0..wet.top.len())
        .map(|i| {
            let r = wet.range(i);
            if r.is_empty() {
                f64::NAN
            } else {
                interfaces.get(r.end).map_or(f64::NAN, |z| z.abs())
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_and_stations() {
        assert_eq!(parse_point("lon=10.5_lat=-20"), Some((10.5, -20.0)));
        assert_eq!(parse_point("LON=-30/LAT=1e1"), Some((-30.0, 10.0)));
        for bad in ["lon=1", "lon=1_lat=95", "lat=1_lon=2", "lon=1_lat=2x"] {
            assert_eq!(parse_point(bad), None, "{bad}");
        }
        let s = parse_stations_csv("name,lon,lat\n# c\nA, 1.5, 2\n\nB,-170,-89.5\n").unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(
            (s[1].name.as_str(), s[1].lon, s[1].lat),
            ("B", -170.0, -89.5)
        );
        assert!(parse_stations_csv("A,1,2\nB,x,3\n").is_err());
        let g = br#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},
            "geometry":{"type":"LineString","coordinates":[[-65,-55,0],[-60,-63]]}}]}"#;
        assert_eq!(
            parse_geojson_path(g).unwrap(),
            vec![(-65.0, -55.0), (-60.0, -63.0)]
        );
        assert!(parse_geojson_path(br#"{"type":"Point","coordinates":[1,2]}"#).is_err());
    }

    #[test]
    fn paths_through_the_pole_and_across_the_dateline() {
        // along the 10/-170 meridian through the north pole: 20 degrees in 100 km steps
        let p = sample_path(&[(10.0, 80.0), (-170.0, 80.0)], 100.0).unwrap();
        let len = 20f64.to_radians() * EARTH_RADIUS_KM;
        assert!((p.length_km - len).abs() < 1e-9);
        let n = p.lon.len() - 1;
        assert_eq!(n, (len / 100.0).ceil() as usize);
        for k in 0..=n {
            assert!((p.distance_km[k] - len * k as f64 / n as f64).abs() < 1e-9);
            let along = 80.0 + 20.0 * (k as f64 / n as f64);
            let lat = if along <= 90.0 { along } else { 180.0 - along };
            assert!((p.lat[k] - lat).abs() < 1e-9, "{k}: {}", p.lat[k]);
            if lat < 90.0 - 1e-6 {
                let want = if along < 90.0 { 10.0 } else { -170.0 };
                assert!((p.lon[k] - want).abs() < 1e-9, "{k}: {}", p.lon[k]);
            }
        }
        // two segments across +-180, the shared waypoint once, lon in (-180, 180]
        let p = sample_path(&[(170.0, 10.0), (190.0, 10.0), (-160.0, 0.0)], 50.0).unwrap();
        assert!(
            p.lon
                .iter()
                .all(|x| x.abs() >= 170.0 - 1e-9 || *x <= -160.0 + 1e-9)
        );
        assert!(p.lon.iter().all(|&x| x > -180.0 && x <= 180.0));
        assert_eq!(
            p.lon.iter().filter(|&&x| (x + 170.0).abs() < 1e-9).count(),
            1
        );
        assert!(
            p.distance_km
                .windows(2)
                .all(|d| d[1] > d[0] && d[1] - d[0] <= 50.0 + 1e-9)
        );
        assert!(sample_path(&[(0.0, 0.0), (180.0, 0.0)], 10.0).is_err());
    }

    /// A 10 x 10 degree lon-lat grid (r36x18 centres).
    fn r36x18() -> Place {
        let (mut lon, mut lat) = (Vec::new(), Vec::new());
        for j in 0..18 {
            for i in 0..36 {
                lon.push(5.0 + 10.0 * i as f64);
                lat.push(-85.0 + 10.0 * j as f64);
            }
        }
        let n = lon.len();
        Place::new("cells", lon, lat, vec![1.0; n]).unwrap()
    }

    #[test]
    fn local_linear_and_nn_on_a_lon_lat_grid() {
        let g = r36x18();
        let s = Sampler::new(&g, None).unwrap();
        // points everywhere, the poles and the dateline included
        let (mut lon, mut lat) = (Vec::new(), Vec::new());
        for j in 0..=36 {
            for i in 0..23 {
                lon.push(-180.0 + 16.3 * i as f64);
                lat.push(-90.0 + 5.0 * j as f64);
            }
        }
        let lin = s.weights(Method::Linear, &lon, &lat);
        assert_eq!(lin.outside, 0);
        let f = |p: &[f64; 3]| 2.0 + 0.3 * p[0] - 1.1 * p[1] + 0.7 * p[2];
        for r in 0..lon.len() {
            let (c, w) = (lin.weights.row(r), lin.weights.weights(r).unwrap());
            assert!(!c.is_empty() && c.len() <= 3 && w.iter().all(|&x| x > 0.0));
            assert!((w.iter().sum::<f64>() - 1.0).abs() < 1e-12);
            // exact for a field linear in x, y, z at the point's projection onto the plane of
            // the cells (the point itself for one or two cells: a vertex or an edge)
            let q = unit_vector(lon[r], lat[r]);
            let mut proj = [0.0; 3];
            for (k, &ci) in c.iter().enumerate() {
                (0..3).for_each(|d| proj[d] += w[k] * s.pts[ci as usize][d]);
            }
            let x = cross(&proj, &q);
            assert!(
                dot(&x, &x).sqrt() < 1e-12,
                "row {r}: not on the ray of the point"
            );
            let got: f64 = c
                .iter()
                .zip(w)
                .map(|(&ci, wi)| wi * f(&s.pts[ci as usize]))
                .sum();
            assert!((got - f(&proj)).abs() < 1e-12, "row {r}");
        }
        // nn = brute force (smallest index on ties)
        let nn = s.weights(Method::Nn, &lon, &lat);
        for r in 0..lon.len() {
            let q = unit_vector(lon[r], lat[r]);
            let want = (0..g.size)
                .min_by(|&a, &b| {
                    let d = |i: usize| {
                        let v = sub(&s.pts[i], &q);
                        dot(&v, &v)
                    };
                    d(a).total_cmp(&d(b)).then(a.cmp(&b))
                })
                .unwrap();
            assert_eq!(nn.weights.row(r), &[want as u32]);
        }
        // the step: 10 degrees of longitude at 5 S..5 N on the equator
        let st = s.default_step_km(&[(-100.0, 0.0), (100.0, 0.0)]).unwrap();
        assert!(
            (st / (10f64.to_radians() * EARTH_RADIUS_KM) - 1.0).abs() < 0.01,
            "{st}"
        );
    }
}
