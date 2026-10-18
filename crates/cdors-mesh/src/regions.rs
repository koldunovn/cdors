//! Region sets: the catalog, GeoJSON parsing, and membership of cells.
//!
//! - **Catalog** ([`open`]): the small sets are compiled in ([`EMBEDDED`]: `ar6.all` with its
//!   subsets `ar6.land` and `ar6.ocean`, `srex`, `giorgi`, `prudence`, `nino`); the large ones
//!   (`ne.<set>` from Natural Earth, nereus' `basins` and `moc`) are read unsimplified from the
//!   regions directory ([`regions_dir`]: `$CDORS_REGIONS`, else `share/regions/` next to the
//!   install's `bin/`); anything ending in `.geojson` or containing `/` is a GeoJSON path.
//! - **Files**: a GeoJSON FeatureCollection of Polygons and MultiPolygons (lon/lat degrees), one
//!   feature per region with the properties `name`, `abbrev` and `number` (all optional), and
//!   an optional foreign member `cdors` with `name`, `overlap`, `source`, `license`,
//!   `attribution` and `subsets` (`tools/export_regions.py` writes them). Coordinates are read
//!   exactly (serde_json's `float_roundtrip`).
//! - **Membership** ([`RegionSet::members`]): a [`Csr`] with one row per region and the member
//!   cells in ascending order, by regionmask's rules ([`Regions::members`]): an overlapping set
//!   lists a cell under every region containing it, any other set under the last one only.
//!   [`fill_nearest`] hands cells outside every region to the region of the nearest member cell.

use crate::geom::{Region, Regions};
use crate::locate::KdTree;
use crate::mesh::Csr;
use crate::{MeshError, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The sets compiled into the binary: (file name, GeoJSON).
pub const EMBEDDED: &[(&str, &[u8])] = &[
    ("ar6.all", include_bytes!("../data/regions/ar6.all.geojson")),
    ("srex", include_bytes!("../data/regions/srex.geojson")),
    ("giorgi", include_bytes!("../data/regions/giorgi.geojson")),
    (
        "prudence",
        include_bytes!("../data/regions/prudence.geojson"),
    ),
    ("nino", include_bytes!("../data/regions/nino.geojson")),
];

/// Catalog names with a one-line description, for `help` and error hints.
pub const CATALOG: &[(&str, &str)] = &[
    ("ar6.all", "IPCC AR6 reference regions, land and ocean (58)"),
    ("ar6.land", "IPCC AR6 land regions (46)"),
    ("ar6.ocean", "IPCC AR6 ocean regions (15)"),
    ("srex", "IPCC SREX regions (26)"),
    ("giorgi", "Giorgi regions (21)"),
    ("prudence", "PRUDENCE European regions (8, overlapping)"),
    ("nino", "Nino 1+2, 3, 3.4 and 4 boxes (overlapping)"),
    (
        "basins",
        "ocean basins of nereus / MPAS geometric_features (9, overlapping; regions directory)",
    ),
    (
        "moc",
        "MOC basins of nereus / MPAS geometric_features (4, overlapping; regions directory)",
    ),
    (
        "ne.countries_50",
        "Natural Earth countries 1:50m (regions directory)",
    ),
    (
        "ne.countries_110",
        "Natural Earth countries 1:110m (regions directory)",
    ),
    ("ne.land_50", "Natural Earth land 1:50m (regions directory)"),
    (
        "ne.land_110",
        "Natural Earth land 1:110m (regions directory)",
    ),
    (
        "ne.ocean_basins_50",
        "Natural Earth ocean basins 1:50m, v5.1.2 (117; regions directory)",
    ),
];

/// A set of named regions, ready for membership tests.
#[derive(Debug, Clone)]
pub struct RegionSet {
    /// Catalog name (`ar6.ocean`) or the path it was read from.
    pub name: String,
    pub numbers: Vec<i64>,
    pub names: Vec<String>,
    pub abbrevs: Vec<String>,
    pub source: String,
    pub license: String,
    pub attribution: String,
    /// The outlines, with the set's overlap flag.
    pub regions: Regions,
}

/// The regions directory: `$CDORS_REGIONS`, else `<prefix>/share/regions` for a binary in
/// `<prefix>/bin`.
pub fn regions_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CDORS_REGIONS").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.parent()?.join("share").join("regions"))
}

/// Opens a set by catalog name or GeoJSON path.
pub fn open(spec: &str) -> Result<RegionSet> {
    if spec.ends_with(".geojson") || spec.contains('/') {
        let bytes = std::fs::read(spec)
            .map_err(|e| MeshError::Missing(format!("cannot read region file '{spec}': {e}")))?;
        return parse_geojson(spec, &bytes, None);
    }
    // a subset of an embedded set (`ar6.land` of `ar6.all`)
    let (file, subset) = match spec {
        "ar6.land" | "ar6.ocean" => ("ar6.all", Some(spec)),
        _ => (spec, None),
    };
    if let Some((_, bytes)) = EMBEDDED.iter().find(|(n, _)| *n == file) {
        return parse_geojson(spec, bytes, subset);
    }
    let known = CATALOG.iter().any(|(n, _)| *n == spec);
    let Some(dir) = regions_dir() else {
        return Err(MeshError::Missing(format!(
            "region set '{spec}': no regions directory (set CDORS_REGIONS)"
        )));
    };
    let path = dir.join(format!("{spec}.geojson"));
    match std::fs::read(&path) {
        Ok(bytes) => parse_geojson(spec, &bytes, None),
        Err(e) if known => Err(MeshError::Missing(format!(
            "region set '{spec}' is read from the regions directory, but {}: {e}",
            path.display()
        ))),
        Err(_) => Err(MeshError::Missing(format!(
            "unknown region set '{spec}' (not in the catalog, no {})",
            path.display()
        ))),
    }
}

fn invalid(name: &str, msg: impl std::fmt::Display) -> MeshError {
    MeshError::Invalid(format!("region file '{name}': {msg}"))
}

/// Rings of a Polygon (`[[[x, y], ...], ...]`) or MultiPolygon (one more level).
fn rings(name: &str, geom: &Value) -> Result<Vec<Vec<[f64; 2]>>> {
    let ty = geom.get("type").and_then(Value::as_str).unwrap_or("");
    let coords = geom
        .get("coordinates")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(name, "a geometry without coordinates"))?;
    let polys: Vec<&Value> = match ty {
        "Polygon" => vec![geom.get("coordinates").expect("checked")],
        "MultiPolygon" => coords.iter().collect(),
        other => {
            return Err(invalid(
                name,
                format!("geometry type '{other}' (Polygon or MultiPolygon expected)"),
            ));
        }
    };
    let mut out = Vec::new();
    for p in polys {
        for ring in p
            .as_array()
            .ok_or_else(|| invalid(name, "a polygon is not a list of rings"))?
        {
            let pts = ring
                .as_array()
                .ok_or_else(|| invalid(name, "a ring is not a list of points"))?;
            let ring: Vec<[f64; 2]> = pts
                .iter()
                .map(|q| match q.as_array().map(Vec::as_slice) {
                    Some([x, y, ..]) => match (x.as_f64(), y.as_f64()) {
                        (Some(x), Some(y)) => Ok([x, y]),
                        _ => Err(invalid(name, "a coordinate is not a number")),
                    },
                    _ => Err(invalid(name, "a point is not [lon, lat]")),
                })
                .collect::<Result<_>>()?;
            out.push(ring);
        }
    }
    Ok(out)
}

/// Parses a FeatureCollection; `subset` keeps the features that list it in `subsets`.
pub fn parse_geojson(name: &str, bytes: &[u8], subset: Option<&str>) -> Result<RegionSet> {
    let doc: Value = serde_json::from_slice(bytes).map_err(|e| invalid(name, e))?;
    let meta = doc.get("cdors").cloned().unwrap_or(Value::Null);
    let text = |k: &str| meta.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    let overlap = match subset {
        Some(s) => meta.pointer(&format!("/subsets/{s}/overlap")),
        None => meta.get("overlap"),
    }
    .and_then(Value::as_bool)
    .unwrap_or(false);
    let feats = doc
        .get("features")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(name, "not a GeoJSON FeatureCollection"))?;
    let mut set = RegionSet {
        name: name.to_owned(),
        numbers: Vec::new(),
        names: Vec::new(),
        abbrevs: Vec::new(),
        source: text("source"),
        license: text("license"),
        attribution: text("attribution"),
        regions: Regions::new(Vec::new(), overlap)?,
    };
    let mut regs = Vec::new();
    for (i, f) in feats.iter().enumerate() {
        let p = f.get("properties").cloned().unwrap_or(Value::Null);
        if let Some(s) = subset {
            let has = p
                .get("subsets")
                .and_then(Value::as_array)
                .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(s)));
            if !has {
                continue;
            }
        }
        let geom = f
            .get("geometry")
            .ok_or_else(|| invalid(name, format!("feature {i} has no geometry")))?;
        let reg = Region::new(&rings(name, geom)?)
            .map_err(|e| invalid(name, format!("feature {i}: {e}")))?;
        let s = |k: &str| p.get(k).and_then(Value::as_str).map(str::to_owned);
        let rname = s("name").unwrap_or_else(|| format!("region {i}"));
        set.abbrevs
            .push(s("abbrev").unwrap_or_else(|| rname.clone()));
        set.names.push(rname);
        set.numbers
            .push(p.get("number").and_then(Value::as_i64).unwrap_or(i as i64));
        regs.push(reg);
    }
    if regs.is_empty() {
        return Err(invalid(name, "no regions"));
    }
    set.regions = Regions::new(regs, overlap).map_err(|e| invalid(name, e))?;
    Ok(set)
}

impl RegionSet {
    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn overlap(&self) -> bool {
        self.regions.overlap
    }

    /// The regions named in `keys` (abbreviation, name or number), in the set's order, as a set
    /// of their own, as regionmask's `regions[[...]]`: its longitude base comes from the kept
    /// outlines, and without overlap a cell goes to the last kept region containing it.
    pub fn select(&self, keys: &[String]) -> Result<RegionSet> {
        let mut keep = vec![false; self.len()];
        for k in keys {
            let hit = (0..self.len()).find(|&i| {
                self.abbrevs[i] == *k
                    || self.names[i] == *k
                    || k.parse::<i64>().is_ok_and(|n| n == self.numbers[i])
            });
            match hit {
                Some(i) => keep[i] = true,
                None => {
                    let list: Vec<&str> = self.abbrevs.iter().map(String::as_str).collect();
                    return Err(MeshError::Missing(format!(
                        "region '{k}' is not in set '{}' (regions: {})",
                        self.name,
                        list.join(", ")
                    )));
                }
            }
        }
        let idx: Vec<usize> = (0..self.len()).filter(|&i| keep[i]).collect();
        let pick = |v: &[String]| idx.iter().map(|&i| v[i].clone()).collect::<Vec<_>>();
        Ok(RegionSet {
            name: self.name.clone(),
            numbers: idx.iter().map(|&i| self.numbers[i]).collect(),
            names: pick(&self.names),
            abbrevs: pick(&self.abbrevs),
            source: self.source.clone(),
            license: self.license.clone(),
            attribution: self.attribution.clone(),
            regions: Regions::new(
                idx.iter()
                    .map(|&i| self.regions.regions[i].clone())
                    .collect(),
                self.regions.overlap,
            )?,
        })
    }

    /// Member cells of each region (rows = regions, cells ascending), by regionmask's rules.
    pub fn members(&self, lon: &[f64], lat: &[f64]) -> Csr {
        self.regions.members(lon, lat)
    }
}

/// Hands every candidate cell (`candidate[i]`, all cells if `None`) that no row lists to the
/// rows of the nearest member cell (great-circle distance; on equal distances the smallest cell
/// index). Returns the new table (cells ascending in every row) and the number of cells filled;
/// a table without members is returned unchanged.
pub fn fill_nearest(m: &Csr, lon: &[f64], lat: &[f64], candidate: Option<&[bool]>) -> (Csr, usize) {
    use rayon::prelude::*;
    let n = m.ncols;
    // regions of each cell (cells x regions), and one tree over all member cells
    let t = m.transpose();
    let pts: Vec<([f64; 3], u32)> = (0..n)
        .into_par_iter()
        .filter(|&i| !t.row(i).is_empty())
        .map(|i| (crate::geom::unit_vector(lon[i], lat[i]), i as u32))
        .collect();
    if pts.is_empty() {
        return (m.clone(), 0);
    }
    let tree = KdTree::from_points(pts);
    // the cell whose regions each cell takes: itself, its nearest member, or none
    const NONE: u32 = u32::MAX;
    let from: Vec<u32> = (0..n)
        .into_par_iter()
        .with_min_len(1 << 12)
        .map(|i| {
            if !t.row(i).is_empty() {
                i as u32
            } else if candidate.is_none_or(|c| c[i]) && lon[i].is_finite() && lat[i].is_finite() {
                tree.nearest(lon[i], lat[i]).map_or(NONE, |(c, _)| c as u32)
            } else {
                NONE
            }
        })
        .collect();
    let filled = (0..n)
        .filter(|&i| from[i] != NONE && from[i] as usize != i)
        .count();
    // rows by counting, cells ascending
    let mut row_ptr = vec![0usize; m.rows() + 1];
    for &f in &from {
        if f != NONE {
            for &r in t.row(f as usize) {
                row_ptr[r as usize + 1] += 1;
            }
        }
    }
    for r in 0..m.rows() {
        row_ptr[r + 1] += row_ptr[r];
    }
    let mut next = row_ptr.clone();
    let mut col = vec![0u32; row_ptr[m.rows()]];
    for (i, &f) in from.iter().enumerate() {
        if f != NONE {
            for &r in t.row(f as usize) {
                col[next[r as usize]] = i as u32;
                next[r as usize] += 1;
            }
        }
    }
    (
        Csr {
            row_ptr,
            col,
            w: None,
            ncols: n,
        },
        filled,
    )
}

/// Whether `path` names a GeoJSON file rather than a catalog set.
pub fn is_path(spec: &str) -> bool {
    spec.ends_with(".geojson") || spec.contains('/') || Path::new(spec).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_sets_parse() {
        for (name, n, overlap) in [
            ("ar6.all", 58, false),
            ("ar6.land", 46, false),
            ("ar6.ocean", 15, false),
            ("srex", 26, false),
            ("giorgi", 21, false),
            ("prudence", 8, true),
            ("nino", 4, true),
        ] {
            let s = open(name).unwrap();
            assert_eq!((s.len(), s.overlap()), (n, overlap), "{name}");
            assert!(!s.attribution.is_empty(), "{name}");
        }
        let o = open("ar6.ocean").unwrap();
        assert_eq!(o.numbers[..3], [8, 19, 38]);
    }

    #[test]
    fn fill_goes_to_the_nearest_member() {
        // cells 0 and 2 are members of rows 0 and 1; 1 is nearer to 2, 3 to 0
        let m = Csr::new(vec![0, 1, 2], vec![0, 2], None, 4).unwrap();
        let (lon, lat) = ([0.0, 9.0, 10.0, -3.0], [0.0; 4]);
        let (f, k) = fill_nearest(&m, &lon, &lat, None);
        assert_eq!(k, 2);
        assert_eq!((f.row(0), f.row(1)), (&[0u32, 3][..], &[1u32, 2][..]));
    }
}
