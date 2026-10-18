//! Region operators: `regmask` (cdors) and cdo's `selregion` and `maskregion`.
//!
//! - **`regmask,regions=<set>[,select=<abbrev,...>][,fill=nearest]`** writes the membership of
//!   every cell of the input's grid in every region of a catalog set
//!   ([`cdors_mesh::regions`]): `mask(region, <cells>)` as int8 (1: the cell centre lies in the
//!   region, by regionmask's rules), with the coordinate `region` (the region numbers) and the
//!   string variables `region_name` and `region_abbrev`. The grid stays as it is (regular grids:
//!   `mask(region, lat, lon)`); `region` is an axis of the vertical kind, so cdo reads the file as
//!   a field with one "level" per region. Overlapping sets (prudence, nino, basins) list a cell
//!   under every region containing it. `fill=nearest` hands the cells outside every region to the
//!   region of the nearest member cell (meant for ocean sets on ocean grids: every cell of the
//!   grid counts as wet); the count is reported (warning `regions_filled`, `--plan`
//!   `warnings[]`, attribute `regions_filled`). A region without cells gives an all-zero row and
//!   the warning `regions_empty`.
//! - **`selregion,<file>[,<file>...]`** and **`maskregion,<file>[,<file>...]`** read cdo's ASCII
//!   region files (lon lat per line, `&` between polygons, `#` comments) and test cell centres
//!   with cdo's own rules (`Selregion.cc`, `Maskbox.cc`, `region.cc` of cdo 2.6.5), which differ
//!   from regionmask's at outlines: each polygon on its own (a cell inside any is selected), the
//!   crossing test `(y >= yi && y < yj) || (y > yj && y <= yi)`, the polygon's latitude range
//!   (inclusive for `maskregion` on cell grids; exclusive for `maskregion` on regular grids and
//!   for `selregion`, as cdo 2.6.0 behaves), and longitudes shifted by 0, -360 or
//!   +360 into the polygon's longitude range (the first shift that fits). `selregion` writes the
//!   selected cells as an unstructured grid over `ncells`, in ascending order, each cell once
//!   (cdo 2.6.0 repeats the cells of earlier polygons, `docs/deviations.md`). `maskregion` keeps
//!   the grid and sets the cells outside to missing.
//! - **`maskregion,regions=<set>[,select=...][,fill=nearest]`** is a cdors extension: the union of
//!   the selected catalog regions as the mask. `dcw:<code>` (GMT's country database) is not
//!   implemented: `ne.countries_50` has the countries.
//!
//! The masks are computed while describing (cell centres and outlines only) and read through a
//! [`MaskSource`], which makes the values of a region's row on demand, so the rest of the chain
//! plans and runs as on stored data.

use crate::chain::OpNode;
use crate::error::{Error, ErrorCode, Result};
use crate::io::{ChunkGrid, ChunkSource, DecodedChunk, RawChunk, Values};
use crate::model::{
    AttrValue, Attrs, DType, Dataset, DimRole, Encoding, Format, GridKind, VarDim, VarKind,
    Variable, ZAxis,
};
use crate::plan::{
    CellMask, Desc, Expr, FixedGrid, GridCoords, GridDesc, IndexMap, Labels, Leaf, Sources,
    VarDesc, ZDesc,
};
use cdors_mesh::regions::{self, RegionSet};
use cdors_mesh::{Csr, MeshError};
use std::sync::Arc;

pub fn handles(name: &str) -> bool {
    matches!(name, "regmask" | "selregion" | "maskregion")
}

fn bad(op: &str, msg: impl Into<String>) -> Error {
    Error::bad_arguments(msg).with("operator", op.to_owned())
}

fn text(s: &str) -> AttrValue {
    AttrValue::Text(s.to_owned())
}

// ---------------------------------------------------------------------------------------------
// Catalog sets
// ---------------------------------------------------------------------------------------------

/// `regions=<set>[,select=<a>,<b>...][,fill=nearest]`.
pub(super) struct Spec {
    pub set: String,
    pub select: Vec<String>,
    pub fill: bool,
}

impl Spec {
    /// The options as written into the output.
    pub(super) fn describe(&self) -> String {
        let mut s = format!("regions={}", self.set);
        if !self.select.is_empty() {
            s.push_str(&format!(",select={}", self.select.join(",")));
        }
        if self.fill {
            s.push_str(",fill=nearest");
        }
        s
    }
}

/// The catalog form of the arguments, `None` if they do not use `regions=` (cdo region files).
pub(super) fn parse_spec(op: &str, args: &[String]) -> Result<Option<Spec>> {
    if !args.iter().any(|a| a.trim().starts_with("regions=")) {
        return Ok(None);
    }
    let usage = "regions=<set>[,select=<abbrev,...>][,fill=nearest]";
    let (mut set, mut select, mut fill, mut in_select) = (None, Vec::new(), false, false);
    for a in args.iter().map(|a| a.trim()) {
        match a.split_once('=') {
            Some(("regions", v)) => (set, in_select) = (Some(v.to_owned()), false),
            Some(("select", v)) => {
                select.push(v.to_owned());
                in_select = true;
            }
            Some(("fill", "nearest")) => (fill, in_select) = (true, false),
            Some(("fill", v)) => {
                return Err(bad(op, format!("{op}: fill={v}: only fill=nearest exists"))
                    .with_hint(format!("usage: {op},{usage}")));
            }
            Some((k, _)) => {
                return Err(bad(op, format!("{op}: unknown key '{k}'"))
                    .with_hint(format!("usage: {op},{usage}")));
            }
            None if in_select => select.push(a.to_owned()),
            None => {
                return Err(bad(op, format!("{op}: unexpected argument '{a}'"))
                    .with_hint(format!("usage: {op},{usage}")));
            }
        }
    }
    let set = set
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad(op, format!("{op}: regions= needs a set name")))?;
    select.retain(|s| !s.is_empty());
    Ok(Some(Spec { set, select, fill }))
}

/// Names of the catalog sets, for hints.
fn catalog_hint() -> String {
    let names: Vec<&str> = regions::CATALOG.iter().map(|(n, _)| *n).collect();
    format!(
        "region sets: {} (the ne.*, basins and moc sets are read from $CDORS_REGIONS or the \
         install's share/regions/), or a GeoJSON file",
        names.join(", ")
    )
}

pub(super) fn open_set(op: &str, spec: &Spec) -> Result<RegionSet> {
    let map = |e: MeshError| match e {
        MeshError::Missing(m) => bad(op, m).with_hint(catalog_hint()),
        MeshError::Invalid(m) => Error::new(ErrorCode::BadData, m).with("operator", op.to_owned()),
        MeshError::Read(m) => Error::new(ErrorCode::IoFailed, m).with("operator", op.to_owned()),
    };
    let set = regions::open(&spec.set).map_err(map)?;
    if spec.select.is_empty() {
        return Ok(set);
    }
    set.select(&spec.select).map_err(|e| match e {
        MeshError::Missing(m) => bad(op, m).with_hint(
            "select= takes the abbreviations, names or numbers of the set's regions \
             (Natural Earth countries: the English name, e.g. select=Germany)",
        ),
        e => map(e),
    })
}

/// A warning on stderr and in `--plan`'s `warnings[]`, with extra fields (an object).
pub(super) fn warn(srcs: &mut Sources, kind: &str, msg: String, fields: serde_json::Value) {
    let fields = match fields {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    crate::exec::threads::warn_with(kind, &msg, fields.clone());
    let mut v = serde_json::json!({"warning": kind, "message": msg});
    v.as_object_mut().expect("object").extend(fields);
    srcs.warnings.push(v);
}

/// The attributes that name a region set, its options, rules and attribution.
pub(super) fn region_attrs(spec: &Spec, set: &RegionSet, filled: Option<usize>) -> Attrs {
    let mut a = vec![
        ("regions".to_owned(), text(&spec.set)),
        ("regions_options".to_owned(), text(&spec.describe())),
        (
            "regions_overlap".to_owned(),
            AttrValue::Ints(vec![i64::from(set.overlap())]),
        ),
        (
            "regions_rules".to_owned(),
            text(
                "regionmask 0.13: cell centres in planar lon/lat, straight edges; a centre on an \
                 outline goes to the region west or south of it",
            ),
        ),
        ("regions_source".to_owned(), text(&set.source)),
        ("regions_license".to_owned(), text(&set.license)),
        ("regions_attribution".to_owned(), text(&set.attribution)),
    ];
    if let Some(k) = filled {
        a.push(("regions_filled".to_owned(), AttrValue::Ints(vec![k as i64])));
    }
    Attrs(a)
}

/// Membership of the cells (`lon`, `lat`) in the set's regions, filled if asked; reports the
/// filled cells and empty regions as warnings. Returns the table and the number filled.
pub(super) fn membership(
    op: &str,
    spec: &Spec,
    set: &RegionSet,
    g: &GridDesc,
    lon: &[f64],
    lat: &[f64],
    srcs: &mut Sources,
) -> (Csr, Option<usize>) {
    let mut m = set.members(lon, lat);
    let mut filled = None;
    if spec.fill {
        let wet = wet_cells(g);
        let (f, k) = regions::fill_nearest(&m, lon, lat, wet.as_deref());
        m = f;
        filled = Some(k);
        let msg = format!(
            "{op}: {k} of {} cells lie outside every region of '{}' and were given to the region \
             of the nearest member cell (fill=nearest)",
            lon.len(),
            spec.set
        );
        warn(srcs, "regions_filled", msg, serde_json::json!({"cells": k}));
    }
    let empty: Vec<&str> = (0..m.rows())
        .filter(|&r| m.row(r).is_empty())
        .map(|r| set.abbrevs[r].as_str())
        .collect();
    if !empty.is_empty() {
        let msg = format!(
            "{op}: {} region(s) of '{}' hold no cell centre of the grid: {}",
            empty.len(),
            spec.set,
            empty.join(", ")
        );
        warn(
            srcs,
            "regions_empty",
            msg,
            serde_json::json!({"regions": empty}),
        );
    }
    (m, filled)
}

// ---------------------------------------------------------------------------------------------
// Grids
// ---------------------------------------------------------------------------------------------

/// Flat indices into the stored grid of the cells of a grid description, in output order.
pub(super) fn selected_cells(g: &GridDesc) -> Vec<usize> {
    if g.sel.len() == 2 {
        let (ys, xs) = (g.sel[0].to_vec(), g.sel[1].to_vec());
        let nx = g.base.xsize;
        ys.iter()
            .flat_map(|&j| xs.iter().map(move |&i| j * nx + i))
            .collect()
    } else {
        g.sel[0].to_vec()
    }
}

fn to_degrees(v: &mut [f64], units: &str) {
    if units.trim().starts_with("radian") {
        v.iter_mut()
            .for_each(|x| *x = crate::model::hpcoords::rad2deg(*x));
    }
}

/// Cell centres (degrees) of a grid description, in output order.
pub(crate) fn centres(op: &str, g: &GridDesc) -> Result<(Vec<f64>, Vec<f64>)> {
    if let Some(f) = &g.fixed {
        let (mut x, mut y) = (f.coords.xvals.clone(), f.coords.yvals.clone());
        if x.len() != g.size() {
            // a regular grid made by an operator: expand rows and columns
            let (nx, ny) = (x.len(), y.len());
            x = (0..ny)
                .flat_map(|_| f.coords.xvals.iter().copied())
                .collect();
            y = f
                .coords
                .yvals
                .iter()
                .flat_map(|&v| std::iter::repeat_n(v, nx))
                .collect();
        }
        to_degrees(&mut x, &f.coords.xunits);
        to_degrees(&mut y, &f.coords.yunits);
        return Ok((x, y));
    }
    let read = |v: &str| g.src.read_var(v);
    let (lon, lat) = g.base.cell_centres(&read).map_err(|e| {
        e.with("operator", op.to_owned())
            .with_hint("attach coordinates with -setgrid,<grid or mesh file>")
    })?;
    let cells = selected_cells(g);
    if cells.len() == lon.len() && cells.iter().enumerate().all(|(k, &c)| k == c) {
        return Ok((lon, lat));
    }
    Ok((
        cells.iter().map(|&c| lon[c]).collect(),
        cells.iter().map(|&c| lat[c]).collect(),
    ))
}

/// The cells `fill=nearest` may fill: on a mesh with a vertical, those with at least one wet
/// level (FESOM2: every node); `None` (every cell) without one.
pub(super) fn wet_cells(g: &GridDesc) -> Option<Vec<bool>> {
    let place = g.base.mesh_place()?;
    let wet = g.base.mesh.as_ref()?.vertical.as_ref()?.wet(&place.name)?;
    Some(
        selected_cells(g)
            .into_iter()
            .map(|c| !wet.range(c).is_empty())
            .collect(),
    )
}

/// Grids used by data variables (index), skipping single points.
fn used_grids(d: &Desc) -> Vec<usize> {
    (0..d.grids.len())
        .filter(|&gi| d.vars.iter().any(|v| v.grid == Some(gi)) && d.grids[gi].size() > 1)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Masks as a chunk source
// ---------------------------------------------------------------------------------------------

/// Values of a membership table, made per region when read: variable `mask` of shape
/// `(region, <cells>)`, or `(<cells>)` for a table of one row; 1 for member cells, else 0.
/// One chunk per region.
struct MaskSource {
    ds: Dataset,
    rows: Arc<Csr>,
    hshape: Vec<usize>,
    region_dim: bool,
}

impl MaskSource {
    fn new(name: &str, rows: Csr, hdims: &[VarDim], region_dim: bool) -> Self {
        let mut dims: Vec<VarDim> = Vec::new();
        if region_dim {
            dims.push(VarDim {
                name: "region".into(),
                size: rows.rows(),
                role: DimRole::Vertical,
            });
        }
        dims.extend(hdims.iter().cloned());
        let hshape: Vec<usize> = hdims.iter().map(|d| d.size).collect();
        let var = Variable {
            name: "mask".into(),
            kind: VarKind::Data,
            dtype: DType::F32,
            chunks: dims
                .iter()
                .enumerate()
                .map(|(i, d)| if region_dim && i == 0 { 1 } else { d.size })
                .collect(),
            dims: dims.clone(),
            attrs: Attrs::default(),
            encoding: Encoding {
                unpacked_f32: true,
                ..Encoding::default()
            },
            grid: None,
            zaxis: None,
        };
        Self {
            ds: Dataset {
                source: name.to_owned(),
                format: Format::Zarr3,
                attrs: Attrs::default(),
                dims: dims.iter().map(|d| (d.name.clone(), d.size)).collect(),
                vars: vec![var],
                grids: Vec::new(),
                zaxes: Vec::new(),
                time: None,
            },
            rows: Arc::new(rows),
            hshape,
            region_dim,
        }
    }

    /// Registers the source and returns a leaf reading it along output dimensions `out`
    /// (region first, then the horizontal dimensions).
    fn leaf(self, srcs: &mut Sources, out: &[usize]) -> Leaf {
        let sizes: Vec<usize> = self.ds.vars[0].dims.iter().map(|d| d.size).collect();
        srcs.paths.push(self.ds.source.clone());
        srcs.srcs.push(Arc::new(self));
        Leaf {
            src: srcs.srcs.len() - 1,
            var: "mask".into(),
            maps: out
                .iter()
                .zip(sizes)
                .map(|(&d, n)| (d, IndexMap::identity(n)))
                .collect(),
        }
    }
}

impl ChunkSource for MaskSource {
    fn dataset(&self) -> &Dataset {
        &self.ds
    }

    fn chunk_grid(&self, _var: &str) -> Result<ChunkGrid> {
        let mut shape = self.hshape.clone();
        let mut chunk_shape = self.hshape.clone();
        if self.region_dim {
            shape.insert(0, self.rows.rows());
            chunk_shape.insert(0, 1);
        }
        Ok(ChunkGrid { shape, chunk_shape })
    }

    fn read_chunk(&self, var: &str, indices: &[u64]) -> Result<RawChunk> {
        let grid = self.chunk_grid(var)?;
        grid.check(var, indices)?;
        let r = if self.region_dim {
            indices[0] as usize
        } else {
            0
        };
        let n: usize = self.hshape.iter().product();
        let mut v = vec![0f32; n];
        for &c in self.rows.row(r) {
            v[c as usize] = 1.0;
        }
        Ok(RawChunk::Decoded(DecodedChunk {
            origin: grid.origin(indices),
            shape: grid.extent(indices),
            values: Values::F32(v),
        }))
    }

    fn read_var(&self, var: &str) -> Result<Vec<f64>> {
        Err(Error::internal(format!(
            "the region mask has no stored variable '{var}'"
        )))
    }
}

// ---------------------------------------------------------------------------------------------
// regmask
// ---------------------------------------------------------------------------------------------

fn regmask(node: &OpNode, d: Desc, srcs: &mut Sources) -> Result<Desc> {
    let op = "regmask";
    let spec = parse_spec(op, &node.args)?.ok_or_else(|| {
        bad(op, "regmask needs regions=<set>")
            .with_hint("usage: regmask,regions=<set>[,select=<abbrev,...>][,fill=nearest]")
    })?;
    let set = open_set(op, &spec)?;
    let v0 = d
        .vars
        .iter()
        .find(|v| v.grid.is_some_and(|g| d.grids[g].size() > 1))
        .ok_or_else(|| bad(op, "regmask: no variable on a grid of more than one cell"))?;
    let gi = v0.grid.expect("grid");
    let (lon, lat) = centres(op, &d.grids[gi])?;
    let (m, filled) = membership(op, &spec, &set, &d.grids[gi], &lon, &lat, srcs);
    let hd: Vec<VarDim> = v0.hdims().iter().map(|&i| v0.dims[i].clone()).collect();
    let nr = set.len();
    let mut dims = vec![VarDim {
        name: "region".into(),
        size: nr,
        role: DimRole::Vertical,
    }];
    dims.extend(hd.iter().cloned());
    let out_dims: Vec<usize> = (0..dims.len()).collect();
    let leaf =
        MaskSource::new(&format!("regions:{}", spec.set), m, &hd, true).leaf(srcs, &out_dims);
    let mut attrs = Attrs(vec![(
        "long_name".into(),
        text("region membership: 1 where the cell centre lies in the region"),
    )]);
    attrs.0.extend(region_attrs(&spec, &set, filled).0);
    let mask = VarDesc {
        name: "mask".into(),
        attrs,
        dtype: DType::I8,
        missval: 0.0,
        dims,
        grid: Some(gi),
        zaxis: Some(d.zaxes.len()),
        expr: Expr::Leaf(leaf),
    };
    let label = |name: &str, values: &[String], long: &str| Labels {
        name: name.to_owned(),
        values: values.to_vec(),
        attrs: Attrs(vec![("long_name".into(), text(long))]),
    };
    let mut zaxes = d.zaxes.clone();
    zaxes.push(ZDesc {
        axis: ZAxis {
            var: "region".into(),
            dim: "region".into(),
            values: set.numbers.iter().map(|&n| n as f64).collect(),
            units: None,
            long_name: Some("region number".into()),
            standard_name: None,
            positive: None,
            bounds: None,
        },
        attrs: Attrs(vec![("long_name".into(), text("region number"))]),
        bounds_var: None,
        labels: vec![
            label("region_name", &set.names, "region name"),
            label("region_abbrev", &set.abbrevs, "region abbreviation"),
        ],
    });
    Ok(Desc {
        attrs: d.attrs.clone(),
        vars: vec![mask],
        grids: d.grids.clone(),
        zaxes,
        time: None,
        fold: None,
    })
}

// ---------------------------------------------------------------------------------------------
// cdo region files and point-in-polygon
// ---------------------------------------------------------------------------------------------

/// The polygons of cdo region files (`read_regions_from_file`, `region.cc`): `lon lat` per line
/// (the first two numbers), `#` and empty lines skipped, `&` or `>` ends a polygon (a `>` on the
/// first line is skipped), an empty polygon ends the file, rings are closed.
fn read_cdo_regions(op: &str, args: &[String]) -> Result<Vec<Vec<[f64; 2]>>> {
    let mut out = Vec::new();
    for a in args {
        if let Some(code) = a.strip_prefix("dcw:") {
            return Err(Error::new(
                ErrorCode::NotImplemented,
                format!("{op}: DCW regions (dcw:{code}) are not implemented in cdors"),
            )
            .with("operator", op.to_owned())
            .with_hint(
                "Natural Earth has the countries: -maskregion,regions=ne.countries_50,select=<country \
                 name> (e.g. select=Germany), or regmask with the same set; or run this step with \
                 cdo",
            ));
        }
        let text = std::fs::read_to_string(a).map_err(|e| {
            Error::new(
                ErrorCode::MissingInput,
                format!("{op}: cannot read region file '{a}': {e}"),
            )
            .with("path", a.clone())
            .with_hint(format!(
                "{op} takes cdo region files (lon lat per line, & between polygons); catalog \
                 sets: maskregion,regions=<set>"
            ))
        })?;
        let mut lines = text.lines().enumerate().peekable();
        let mut seg = 0;
        loop {
            let mut ring: Vec<[f64; 2]> = Vec::new();
            for (ln, line) in lines.by_ref() {
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if ring.is_empty() && line.starts_with('>') {
                    continue;
                }
                if line.starts_with('&') || line.starts_with('>') {
                    break;
                }
                let mut it = line.split_whitespace().map(str::parse::<f64>);
                match (it.next(), it.next()) {
                    (Some(Ok(x)), Some(Ok(y))) => ring.push([x, y]),
                    _ => {
                        return Err(bad(
                            op,
                            format!(
                                "{op}: wrong value format in file {a} at polygon {seg} line {}: \
                                 '{line}' (expected: lon lat)",
                                ln + 1
                            ),
                        ));
                    }
                }
            }
            if ring.is_empty() {
                break;
            }
            let (f, l) = (ring[0], ring[ring.len() - 1]);
            if !(f[0] == l[0] && f[1] == l[1]) {
                ring.push(f);
            }
            if ring.len() < 3 {
                return Err(bad(
                    op,
                    format!("{op}: too few points for polygon {seg} in file {a} (min 3)"),
                ));
            }
            out.push(ring);
            seg += 1;
            if lines.peek().is_none() {
                break;
            }
        }
    }
    if out.is_empty() {
        return Err(bad(op, format!("{op}: no polygon in the region file(s)")));
    }
    Ok(out)
}

/// cdo's crossing test of one polygon (`point_is_inside`).
#[inline]
fn cdo_inside(x: f64, y: f64, ring: &[[f64; 2]]) -> bool {
    let n = ring.len();
    let mut c = false;
    let mut j = n - 1;
    for i in 0..n {
        let ([xi, yi], [xj, yj]) = (ring[i], ring[j]);
        if ((y >= yi && y < yj) || (y > yj && y <= yi))
            && (x < ((xj - xi) * (y - yi) / (yj - yi) + xi))
        {
            c = !c;
        }
        j = i;
    }
    c
}

/// cdo's longitude shift: the first of x, x - 360, x + 360 within the polygon's longitudes.
#[inline]
fn cdo_inside_shifted(x: f64, y: f64, xmin: f64, xmax: f64, ring: &[[f64; 2]]) -> bool {
    for off in [0.0, -360.0, 360.0] {
        let s = x + off;
        if s >= xmin && s <= xmax {
            return cdo_inside(s, y, ring);
        }
    }
    false
}

fn ring_bounds(ring: &[[f64; 2]]) -> [f64; 4] {
    ring.iter().fold(
        [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ],
        |b, p| {
            [
                b[0].min(p[0]),
                b[1].min(p[1]),
                b[2].max(p[0]),
                b[3].max(p[1]),
            ]
        },
    )
}

/// Cells (centres in degrees) inside any polygon (`sel_region_cell`, `mask_region_cell`). The
/// polygon's latitude range is inclusive for `maskregion` and exclusive for `selregion`: cdo
/// 2.6.0's `selregion` drops centres on the polygon's highest latitude that 2.6.5's source and
/// 2.6.0's `maskregion` keep (checked on r1x1_edges).
fn cdo_mask_cells(lon: &[f64], lat: &[f64], polys: &[Vec<[f64; 2]>], strict: bool) -> Vec<bool> {
    let mut mask = vec![false; lon.len()];
    for ring in polys {
        let b = ring_bounds(ring);
        for i in 0..lon.len() {
            let y = lat[i];
            let in_range = if strict {
                y > b[1] && y < b[3]
            } else {
                y >= b[1] && y <= b[3]
            };
            if !mask[i] && in_range && cdo_inside_shifted(lon[i], y, b[0], b[2], ring) {
                mask[i] = true;
            }
        }
    }
    mask
}

/// `maskregion` on a regular grid (`mask_region_regular`): the latitude range is exclusive.
fn cdo_mask_regular(xs: &[f64], ys: &[f64], polys: &[Vec<[f64; 2]>]) -> Vec<bool> {
    let nx = xs.len();
    let mut mask = vec![false; nx * ys.len()];
    for ring in polys {
        let b = ring_bounds(ring);
        for (j, &y) in ys.iter().enumerate() {
            if !(y > b[1] && y < b[3]) {
                continue;
            }
            for (i, &x) in xs.iter().enumerate() {
                if cdo_inside_shifted(x, y, b[0], b[2], ring) {
                    mask[j * nx + i] = true;
                }
            }
        }
    }
    mask
}

// ---------------------------------------------------------------------------------------------
// selregion and maskregion
// ---------------------------------------------------------------------------------------------

fn no_coords(op: &str) -> Error {
    Error::new(
        ErrorCode::NoCoordinates,
        format!("{op} needs cell centre coordinates; the grid has none"),
    )
    .with("operator", op.to_owned())
    .with_hint("attach coordinates with -setgrid,<grid or mesh file>")
}

fn selregion(node: &OpNode, mut d: Desc) -> Result<Desc> {
    let op = "selregion";
    if node.args.iter().any(|a| a.trim().starts_with("regions=")) {
        return Err(bad(
            op,
            "selregion takes cdo region files; regions=<set> is for maskregion and regmask",
        )
        .with_hint(
            "mask with -maskregion,regions=<set>, or select the cells of a cdo region file",
        ));
    }
    let polys = read_cdo_regions(op, &node.args)?;
    for gi in used_grids(&d) {
        let g = &d.grids[gi];
        if g.kind == GridKind::Generic {
            return Err(no_coords(op));
        }
        let (lon, lat) = centres(op, g)?;
        let inside = cdo_mask_cells(&lon, &lat, &polys, true);
        let cells: Vec<usize> = (0..inside.len()).filter(|&i| inside[i]).collect();
        if cells.is_empty() {
            return Err(
                bad(op, "selregion: no grid points found in the region(s)").with_hint(
                    "check the polygon (lon lat per line, degrees) against the grid's coordinates",
                ),
            );
        }
        let n = cells.len();
        let sel = IndexMap::from_list(cells.clone());
        let g = &mut d.grids[gi];
        let was_healpix = g.kind == GridKind::Healpix;
        if g.sel.len() == 1 {
            // one cell dimension: a selection along it, as sellonlatbox on unstructured grids
            g.sel[0] = g.sel[0].compose(&sel);
            g.kind = GridKind::Unstructured;
            for v in d.vars.iter_mut().filter(|v| v.grid == Some(gi)) {
                let hd = v.hdims();
                v.select(hd[0], &sel);
                if was_healpix {
                    v.dims[hd[0]].name = "ncells".to_owned();
                }
            }
            continue;
        }
        // two dimensions (regular, Gaussian, curvilinear): the cells become one dimension
        // `ncells`; each stored dimension follows it through its own index list
        let (nx, _) = g.xy_size();
        let rows: Vec<usize> = cells.iter().map(|&c| c / nx).collect();
        let cols: Vec<usize> = cells.iter().map(|&c| c % nx).collect();
        let flat: Vec<usize> = {
            let all = selected_cells(g);
            cells.iter().map(|&c| all[c]).collect()
        };
        let pick_attrs = |name: Option<&str>| -> Attrs {
            let src = name.and_then(|n| g.src.dataset().var(n).map(|v| v.attrs.clone()));
            Attrs(
                src.unwrap_or_default()
                    .iter()
                    .filter(|(k, _)| matches!(k.as_str(), "standard_name" | "long_name" | "units"))
                    .cloned()
                    .collect(),
            )
        };
        let xattrs = pick_attrs(g.base.x.as_ref().map(|a| a.var.as_str()));
        let yattrs = pick_attrs(g.base.y.as_ref().map(|a| a.var.as_str()));
        let unit = |a: &Option<crate::model::CoordAxis>, d: &str| {
            a.as_ref()
                .and_then(|a| a.units.clone())
                .unwrap_or_else(|| d.to_owned())
        };
        let (xu, yu) = (
            unit(&g.base.x, "degrees_east"),
            unit(&g.base.y, "degrees_north"),
        );
        let mut xv: Vec<f64> = cells.iter().map(|&c| lon[c]).collect();
        let mut yv: Vec<f64> = cells.iter().map(|&c| lat[c]).collect();
        // centres were converted to degrees; written in the grid's own units
        if xu.trim().starts_with("radian") {
            xv.iter_mut().for_each(|x| *x = x.to_radians());
        }
        if yu.trim().starts_with("radian") {
            yv.iter_mut().for_each(|y| *y = y.to_radians());
        }
        g.fixed = Some(Arc::new(FixedGrid {
            coords: GridCoords {
                xvals: xv,
                yvals: yv,
                xbounds: None,
                ybounds: None,
                nv: 0,
                xunits: xu,
                yunits: yu,
            },
            xattrs,
            yattrs,
            ..FixedGrid::default()
        }));
        g.sel = vec![IndexMap::from_list(flat)];
        g.kind = GridKind::Unstructured;
        g.xvals = None;
        let (rmap, cmap) = (IndexMap::from_list(rows), IndexMap::from_list(cols));
        for v in d.vars.iter_mut().filter(|v| v.grid == Some(gi)) {
            let hd = v.hdims();
            let (h0, h1) = (hd[0], hd[1]);
            v.expr.retarget(&|k| {
                if k == h0 {
                    (h0, Some(rmap.clone()))
                } else if k == h1 {
                    (h0, Some(cmap.clone()))
                } else if k > h1 {
                    (k - 1, None)
                } else {
                    (k, None)
                }
            });
            v.dims[h0] = VarDim {
                name: "ncells".into(),
                size: n,
                role: DimRole::Horizontal,
            };
            v.dims.remove(h1);
        }
    }
    Ok(d)
}

fn maskregion(node: &OpNode, mut d: Desc, srcs: &mut Sources) -> Result<Desc> {
    let op = "maskregion";
    let spec = parse_spec(op, &node.args)?;
    let polys = match &spec {
        Some(_) => Vec::new(),
        None => read_cdo_regions(op, &node.args)?,
    };
    let set = spec.as_ref().map(|s| open_set(op, s)).transpose()?;
    for gi in used_grids(&d) {
        let g = &d.grids[gi];
        if g.kind == GridKind::Generic {
            return Err(no_coords(op));
        }
        let inside: Vec<bool> = match (&spec, &set) {
            (Some(spec), Some(set)) => {
                let (lon, lat) = centres(op, g)?;
                let (m, _) = membership(op, spec, set, g, &lon, &lat, srcs);
                let mut inside = vec![false; lon.len()];
                m.col.iter().for_each(|&c| inside[c as usize] = true);
                inside
            }
            _ if matches!(g.kind, GridKind::Regular | GridKind::Gaussian) && g.sel.len() == 2 => {
                let c = g.coords()?.ok_or_else(|| no_coords(op))?;
                let (mut xs, mut ys) = (c.xvals, c.yvals);
                to_degrees(&mut xs, &c.xunits);
                to_degrees(&mut ys, &c.yunits);
                cdo_mask_regular(&xs, &ys, &polys)
            }
            _ => {
                let (lon, lat) = centres(op, g)?;
                cdo_mask_cells(&lon, &lat, &polys, false)
            }
        };
        let name = match &spec {
            Some(s) => format!("regions:{}", s.describe()),
            None => format!("regions:{}", node.args.join(",")),
        };
        let inside = Arc::new(inside);
        let users: Vec<usize> = (0..d.vars.len())
            .filter(|&i| d.vars[i].grid == Some(gi))
            .collect();
        for i in users {
            let v = &mut d.vars[i];
            let hd = v.hdims();
            // the cells outside are set missing in each evaluated tile (cdo's ifthen with a 0/1
            // mask); no mask values are read or gathered
            let mask = CellMask {
                name: name.clone(),
                inside: inside.clone(),
                dims: hd
                    .iter()
                    .map(|&k| (k, IndexMap::identity(v.dims[k].size)))
                    .collect(),
                sizes: hd.iter().map(|&k| v.dims[k].size).collect(),
            };
            let e = std::mem::replace(
                &mut v.expr,
                Expr::Leaf(Leaf {
                    src: 0,
                    var: String::new(),
                    maps: vec![],
                }),
            );
            v.expr = Expr::Cells(Arc::new(mask), Box::new(e));
        }
    }
    Ok(d)
}

/// Output description of a region operator.
pub fn describe(node: &OpNode, mut inputs: Vec<Desc>, srcs: &mut Sources) -> Result<Desc> {
    let d = inputs.remove(0);
    match node.name.as_str() {
        "regmask" => regmask(node, d, srcs),
        "selregion" => selregion(node, d),
        "maskregion" => maskregion(node, d, srcs),
        other => Err(Error::internal(format!(
            "'{other}' is not a region operator"
        ))),
    }
}

/// The catalog and the data attribution, for `cdors help`.
pub fn catalog_text() -> Vec<String> {
    let mut out: Vec<String> = regions::CATALOG
        .iter()
        .map(|(n, d)| format!("{n}: {d}"))
        .collect();
    out.push(
        "attribution: AR6 regions Iturbide et al. (2020), ESSD 12, 2959-2970 (CC-BY 4.0); SREX \
         (Seneviratne et al. 2012), Giorgi (Giorgi and Francisco 2000), PRUDENCE (Christensen \
         and Christensen 2007) via regionmask (MIT); Natural Earth v5.1.2 (public domain); \
         basins and moc: MPAS geometric_features, IHO seas via marineregions.org (CC-BY 4.0), \
         via nereus; nino: NOAA CPC box definitions. Each output names its source in the \
         attributes regions_source, regions_license and regions_attribution."
            .to_owned(),
    );
    out
}
