//! `selpoints` and `transect` (cdors operators, not in cdo): the values of scalar fields at
//! stations and along a great-circle path, on any grid with cell centres and on model meshes.
//!
//! - **`selpoints,file=<csv>|lon=<x>_lat=<y>[,lon=<x>_lat=<y>...][,method=nn|linear]`**: the
//!   stations of a CSV file (`name,lon,lat` per line, an optional header) or points written as for
//!   cdo's `remapnn,lon=<x>_lat=<y>`. Output dimension `station`, with `lon(station)`,
//!   `lat(station)` (as given) and the string variable `station_name` (an inline point is named by
//!   its text).
//! - **`transect,<lon0>,<lat0>,<lon1>,<lat1>[,...][,step=<km>][,method=nn|linear]`** or
//!   **`transect,file=<GeoJSON LineString>[,...]`**: great-circle segments through the waypoints,
//!   sampled in equal arcs of at most `step` km per segment (default: the median cell spacing along
//!   the path). One path per call. Output dimension `distance`, with the coordinate variable
//!   `distance` (km along the path), `lon(distance)` in (−180, 180] and `lat(distance)`.
//!
//! The sampling and its weights are `cdors_mesh::sample`'s: `nn` the nearest cell (ties: the
//! smallest index), `linear` barycentric weights in the mesh's own triangles (FESOM2 nodes) or, on
//! built-in grids, in the local spherical Delaunay triangle of the nearest cells. A mesh place
//! without triangles of its own (FESOM2 elements, or a subset of the nodes) is sampled by `nn`
//! with the warning `linear_as_nn`. A point in no triangle is missing (warning `points_outside`).
//!
//! Levels are kept as they are (no vertical interpolation); the sample dimension replaces the
//! horizontal one in place. A value is missing if any cell of its point's weights is missing there
//! (`MissingRule::AllOrNothing`); on a model mesh the levels below the shallowest bottom of those
//! cells are missing as well, and `bottom_depth(<dim>)` (m) gives that bottom for the mesh's first
//! place, `bottom_depth_<dim>` for the others. The weights are computed while describing (a
//! KD-tree on the cell centres) and run on the remap executor (`RemapWeights::from_csr`), reading
//! only the chunks the weights touch.

use super::remap::{FieldKernel, FieldLane, FieldMap, FieldVar, apply_t, no_pending_fold, prune};
use crate::chain::OpNode;
use crate::error::{Error, ErrorCode, Result};
use crate::io::{ChunkSource, Values};
use crate::model::{AttrValue, Attrs, DType, DimRole, GridKind, VarDim};
use crate::plan::{Desc, FixedGrid, Fold, GridCoords, GridDesc, IndexMap, Labels, Sources};
use crate::remap::{MissingRule, RemapWeights};
use cdors_mesh::models::{self, DimRule};
use cdors_mesh::sample::{self, Method, Sampler, Station};
use cdors_mesh::{Csr, MeshError, Place, WetLevels};
use std::sync::Arc;

pub fn handles(name: &str) -> bool {
    matches!(name, "selpoints" | "transect")
}

const SELPOINTS_USAGE: &str = "selpoints,file=<csv: name,lon,lat>|lon=<x>_lat=<y>[,lon=<x>_lat=<y>\
     ...][,method=nn|linear]";
const TRANSECT_USAGE: &str = "transect,<lon0>,<lat0>,<lon1>,<lat1>[,<lon>,<lat>...][,step=<km>]\
     [,method=nn|linear] or transect,file=<GeoJSON LineString>[,step=<km>][,method=nn|linear]";

fn text(s: &str) -> AttrValue {
    AttrValue::Text(s.to_owned())
}

fn usage(op: &str) -> &'static str {
    if op == "selpoints" {
        SELPOINTS_USAGE
    } else {
        TRANSECT_USAGE
    }
}

fn bad(op: &str, msg: impl Into<String>) -> Error {
    Error::bad_arguments(msg)
        .with("operator", op.to_owned())
        .with_hint(format!("usage: {}", usage(op)))
}

fn mesh_bad(op: &str, what: &str, e: MeshError) -> Error {
    bad(op, format!("{op}: {what}: {e}"))
}

fn read_file(op: &str, path: &str) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| {
        Error::new(
            ErrorCode::MissingInput,
            format!("{op}: cannot read '{path}': {e}"),
        )
        .with("operator", op.to_owned())
        .with("path", path.to_owned())
        .with_hint(format!("usage: {}", usage(op)))
    })
}

// ---------------------------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------------------------

enum Points {
    Stations(Vec<Station>),
    Path {
        waypoints: Vec<(f64, f64)>,
        step: Option<f64>,
    },
}

struct Spec {
    points: Points,
    method: Method,
}

fn parse_method(op: &str, v: &str) -> Result<Method> {
    Method::parse(v).ok_or_else(|| bad(op, format!("{op}: method={v}: nn or linear")))
}

fn parse(op: &str, args: &[String]) -> Result<Spec> {
    let mut method = Method::Nn;
    if op == "selpoints" {
        let mut st = Vec::new();
        for a in args.iter().map(|a| a.trim()) {
            if let Some(v) = a.strip_prefix("method=") {
                method = parse_method(op, v)?;
            } else if let Some(f) = a.strip_prefix("file=") {
                let bytes = read_file(op, f)?;
                let s = sample::parse_stations_csv(&String::from_utf8_lossy(&bytes))
                    .map_err(|e| mesh_bad(op, f, e))?;
                st.extend(s);
            } else if let Some((lon, lat)) = sample::parse_point(a) {
                st.push(Station {
                    name: a.to_owned(),
                    lon,
                    lat,
                });
            } else {
                return Err(bad(
                    op,
                    format!("{op}: '{a}' is not lon=<x>_lat=<y>, file=, or method="),
                ));
            }
        }
        if st.is_empty() {
            return Err(bad(op, format!("{op}: no stations")));
        }
        return Ok(Spec {
            points: Points::Stations(st),
            method,
        });
    }
    let (mut nums, mut file, mut step) = (Vec::new(), None, None);
    for a in args.iter().map(|a| a.trim()) {
        if let Some(v) = a.strip_prefix("method=") {
            method = parse_method(op, v)?;
        } else if let Some(v) = a.strip_prefix("step=") {
            match v.parse::<f64>() {
                Ok(s) if s > 0.0 && s.is_finite() => step = Some(s),
                _ => return Err(bad(op, format!("{op}: step={v}: a positive number of km"))),
            }
        } else if let Some(f) = a.strip_prefix("file=") {
            file = Some(f.to_owned());
        } else if let Ok(x) = a.parse::<f64>() {
            nums.push(x);
        } else {
            return Err(bad(op, format!("{op}: unexpected argument '{a}'")));
        }
    }
    let waypoints = match (file, nums.is_empty()) {
        (Some(_), false) => {
            return Err(bad(
                op,
                format!("{op}: waypoints and file= together; one path per call"),
            ));
        }
        (Some(f), true) => {
            let bytes = read_file(op, &f)?;
            sample::parse_geojson_path(&bytes).map_err(|e| mesh_bad(op, &f, e))?
        }
        (None, _) => sample::waypoints(&nums).map_err(|e| mesh_bad(op, "path", e))?,
    };
    Ok(Spec {
        points: Points::Path { waypoints, step },
        method,
    })
}

// ---------------------------------------------------------------------------------------------
// The map
// ---------------------------------------------------------------------------------------------

/// Sample weights per input grid and the wet levels of the points.
struct SampleMap {
    op: String,
    grids: Vec<Arc<RemapWeights>>,
    /// Wet levels of the points on each grid (model meshes with a vertical).
    wet: Vec<Option<WetLevels>>,
    var_grid: Vec<usize>,
    /// Per variable with levels: its level dimension, the mesh level of each index along it, and
    /// mid-levels or interfaces.
    dry: Vec<Option<(usize, IndexMap, DimRule)>>,
    info: Vec<serde_json::Value>,
}

fn mask_points<T: Copy>(
    v: &mut [T],
    nan: T,
    size: usize,
    levels: &[usize],
    wet: &WetLevels,
    rule: DimRule,
) {
    for (f, field) in v.chunks_mut(size).enumerate() {
        let k = levels[f];
        for (p, x) in field.iter_mut().enumerate() {
            if !models::wet_range(wet, rule, p).contains(&k) {
                *x = nan;
            }
        }
    }
}

impl FieldMap for SampleMap {
    fn apply(
        &self,
        var: usize,
        src: &Values,
        n: usize,
        out: DType,
        lane: &FieldLane,
    ) -> Result<Values> {
        let gi = self.var_grid[var];
        let w = &self.grids[gi];
        let r = match (src, out) {
            (Values::F32(s), DType::F32) => apply_t::<f32, f32>(w, s, n, Values::F32),
            (Values::F32(s), _) => apply_t::<f32, f64>(w, s, n, Values::F64),
            (Values::F64(s), DType::F32) => apply_t::<f64, f32>(w, s, n, Values::F32),
            (Values::F64(s), _) => apply_t::<f64, f64>(w, s, n, Values::F64),
        };
        let mut v = r.map_err(|e| Error::new(ErrorCode::BadData, format!("{}: {e}", self.op)))?;
        if let (Some((d, map, rule)), Some(wet)) = (&self.dry[var], &self.wet[gi]) {
            let levels: Vec<usize> = lane.index(*d).into_iter().map(|j| map.get(j)).collect();
            let size = w.dst_size();
            match &mut v {
                Values::F32(x) => mask_points(x, f32::NAN, size, &levels, wet, *rule),
                Values::F64(x) => mask_points(x, f64::NAN, size, &levels, wet, *rule),
            }
        }
        Ok(v)
    }

    fn plan_info(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({ "mesh_weights": self.info }))
    }
}

// ---------------------------------------------------------------------------------------------
// Describe
// ---------------------------------------------------------------------------------------------

/// A warning on stderr and in `--plan`'s `warnings[]`, with extra fields.
fn warn(srcs: &mut Sources, kind: &str, msg: String, fields: serde_json::Value) {
    let fields = match fields {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    crate::exec::threads::warn_with(kind, &msg, fields.clone());
    let mut v = serde_json::json!({"warning": kind, "message": msg});
    v.as_object_mut().expect("object").extend(fields);
    srcs.warnings.push(v);
}

/// The cells of one input grid as sampling sees them.
struct Cells {
    place: Place,
    triangles: Option<Arc<Csr>>,
    /// Wet levels of the cells (model meshes with a vertical), and the mesh's interfaces.
    wet: Option<(WetLevels, Vec<f64>)>,
    /// On a model mesh: the mesh name and whether the place is the mesh's first.
    mesh: Option<(String, bool)>,
    name: String,
}

fn cells_of(op: &str, g: &GridDesc) -> Result<Cells> {
    if let (Some(mesh), Some(place)) = (g.base.mesh.as_ref(), g.base.mesh_place()) {
        let cells = super::fldstat::selected_cells(g);
        let full = cells.len() == place.size && cells.iter().enumerate().all(|(k, &c)| k == c);
        let pick = |v: &[f64]| cells.iter().map(|&c| v[c]).collect::<Vec<f64>>();
        let p = Place::new(
            &place.name,
            pick(&place.lon),
            pick(&place.lat),
            pick(&place.area),
        )
        .map_err(|e| Error::internal(e.to_string()))?;
        let triangles = if full {
            sample::triangles(mesh, &place.name)
                .map_err(|e| crate::model::mesh_cache::mesh_error(e, op))?
        } else {
            None
        };
        let wet = mesh.vertical.as_ref().and_then(|z| {
            let w = z.wet(&place.name)?;
            let top = cells.iter().map(|&c| w.top[c]).collect();
            let bottom = cells.iter().map(|&c| w.bottom[c]).collect();
            Some((
                WetLevels {
                    place: place.name.clone(),
                    top,
                    bottom,
                },
                z.interfaces.clone(),
            ))
        });
        let first = mesh.places.first().is_some_and(|f| f.name == place.name);
        return Ok(Cells {
            place: p,
            triangles,
            wet,
            mesh: Some((mesh.name.clone(), first)),
            name: format!("the {} of {}", place.name, mesh.name),
        });
    }
    if g.kind == GridKind::Generic {
        return Err(Error::new(
            ErrorCode::NoCoordinates,
            format!("{op}: the grid has no cell centre coordinates"),
        )
        .with("operator", op.to_owned())
        .with_hint("attach coordinates with -setgrid,<grid or mesh file>"));
    }
    let (lon, lat) = super::regions::centres(op, g)?;
    let n = lon.len();
    let place =
        Place::new("cells", lon, lat, vec![0.0; n]).map_err(|e| Error::internal(e.to_string()))?;
    Ok(Cells {
        place,
        triangles: None,
        wet: None,
        mesh: None,
        name: format!("the {} grid", g.kind.name()),
    })
}

/// Output description of `selpoints` / `transect`.
pub fn describe(node: &OpNode, mut inputs: Vec<Desc>, srcs: &mut Sources) -> Result<Desc> {
    let op = node.name.as_str();
    let spec = parse(op, &node.args)?;
    let mut input = inputs
        .pop()
        .ok_or_else(|| Error::internal("sampling without input"))?;
    no_pending_fold(op, &input)?;
    // input grids in the order of first use
    let mut used: Vec<usize> = Vec::new();
    for v in &input.vars {
        let gi = v.grid.ok_or_else(|| {
            Error::new(
                ErrorCode::NoCoordinates,
                format!("variable '{}' has no horizontal grid", v.name),
            )
            .with("operator", op.to_owned())
            .with("variable", v.name.clone())
            .with_hint("select variables on a horizontal grid with -selname")
        })?;
        if !used.contains(&gi) {
            used.push(gi);
        }
    }
    if used.is_empty() {
        return Err(Error::bad_arguments(format!("{op}: no variables")));
    }
    let cells: Vec<Cells> = used
        .iter()
        .map(|&gi| cells_of(op, &input.grids[gi]))
        .collect::<Result<_>>()?;
    let samplers: Vec<Sampler> = cells
        .iter()
        .map(|c| Sampler::new(&c.place, c.triangles.clone()))
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::internal(e.to_string()))?;

    // the points
    let mut out_attrs: Vec<(String, AttrValue)> = Vec::new();
    let (dim, plon, plat, mut aux, labels) = match &spec.points {
        Points::Stations(st) => {
            let names: Vec<String> = st.iter().map(|s| s.name.clone()).collect();
            (
                "station",
                st.iter().map(|s| s.lon).collect::<Vec<f64>>(),
                st.iter().map(|s| s.lat).collect::<Vec<f64>>(),
                Vec::new(),
                vec![Labels {
                    name: "station_name".into(),
                    values: names,
                    attrs: Attrs(vec![("long_name".into(), text("station name"))]),
                }],
            )
        }
        Points::Path { waypoints, step } => {
            let (step, source) = match step {
                Some(s) => (*s, "given"),
                None => (
                    samplers[0]
                        .default_step_km(waypoints)
                        .map_err(|e| mesh_bad(op, "path", e))?,
                    "default: the median cell spacing along the path",
                ),
            };
            let p = sample::sample_path(waypoints, step).map_err(|e| mesh_bad(op, "path", e))?;
            let wp: Vec<String> = waypoints.iter().map(|(x, y)| format!("{x},{y}")).collect();
            out_attrs.push(("transect_path".into(), text(&wp.join(","))));
            out_attrs.push(("transect_step_km".into(), AttrValue::F64s(vec![step])));
            out_attrs.push(("transect_step_source".into(), text(source)));
            let aux = vec![(
                "distance".to_owned(),
                p.distance_km,
                Attrs(vec![
                    ("long_name".into(), text("distance along the path")),
                    ("units".into(), text("km")),
                ]),
            )];
            ("distance", p.lon, p.lat, aux, Vec::new())
        }
    };
    let n = plon.len();

    // weights per input grid
    let mut grids = Vec::new();
    let mut wets = Vec::new();
    let mut info = Vec::new();
    let mut methods = Vec::new();
    let mut outside_total = 0;
    for (k, &gi) in used.iter().enumerate() {
        let c = &cells[k];
        let method = if spec.method == Method::Linear && c.mesh.is_some() && c.triangles.is_none() {
            warn(
                srcs,
                "linear_as_nn",
                format!(
                    "{op}: {} have no triangles of their own: method=linear samples them by the \
                     nearest cell",
                    c.name
                ),
                serde_json::json!({"place": c.place.name}),
            );
            Method::Nn
        } else {
            spec.method
        };
        methods.push(method);
        let s = samplers[k].weights(method, &plon, &plat);
        if s.outside > 0 {
            outside_total += s.outside;
            warn(
                srcs,
                "points_outside",
                format!(
                    "{op}: {} of {n} points lie in no triangle of {} (method=linear): their \
                     values are missing",
                    s.outside, c.name
                ),
                serde_json::json!({"points": s.outside, "of": n}),
            );
        }
        // wet levels and bottom depth of the points
        let pwet = c.wet.as_ref().map(|(w, interfaces)| {
            let pw = sample::wet_levels(&s.weights, w);
            let depth = sample::bottom_depth(&pw, interfaces);
            (pw, depth)
        });
        if let (Some((_, depth)), Some((_, first))) = (&pwet, &c.mesh) {
            let v = input
                .vars
                .iter()
                .find(|v| v.grid == Some(gi))
                .expect("used");
            let hd = v.hdims().first().map(|&i| v.dims[i].name.clone());
            let name = if *first {
                "bottom_depth".to_owned()
            } else {
                format!("bottom_depth_{}", hd.unwrap_or_default())
            };
            aux.push((
                name,
                depth.clone(),
                Attrs(vec![
                    (
                        "standard_name".into(),
                        text("sea_floor_depth_below_sea_surface"),
                    ),
                    (
                        "long_name".into(),
                        text(&format!(
                            "bottom of the levels wet at every cell sampled at the point ({})",
                            c.place.name
                        )),
                    ),
                    ("units".into(), text("m")),
                    ("coordinates".into(), text("lat lon")),
                ]),
            ));
        }
        wets.push(pwet.map(|(w, _)| w));
        let rw = RemapWeights::from_csr(&s.weights, MissingRule::AllOrNothing)
            .map_err(|e| Error::internal(e.to_string()))?;
        info.push(serde_json::json!({
            "operator": op,
            "weights": method.name(),
            "mesh": c.mesh.as_ref().map_or_else(|| c.name.clone(), |m| m.0.clone()),
            "source": c.place.name,
            "target": dim,
            "rows": n,
            "links": s.weights.nnz(),
            "bytes": cdors_mesh::mesh::csr_bytes(n, s.weights.nnz(), true),
            "points_outside": s.outside,
        }));
        grids.push(Arc::new(rw));
    }

    // output variables
    let mut fvars = Vec::new();
    let mut out_vars = Vec::new();
    let mut dry = Vec::new();
    let mut var_grid = Vec::new();
    for v in &input.vars {
        let gi = v.grid.expect("checked");
        let k = used.iter().position(|&g| g == gi).expect("used");
        let fv = FieldVar::new(op, v, input.grids[gi].size(), vec![n])?;
        let (hfirst, hcount) = (fv.hfirst, fv.hcount);
        fvars.push(fv);
        var_grid.push(k);
        dry.push(v.expr.wet_mask().map(|(m, d, levels)| (d, levels, m.rule)));
        let mut ov = v.clone();
        ov.dims.splice(
            hfirst..hfirst + hcount,
            [VarDim {
                name: dim.to_owned(),
                size: n,
                role: DimRole::Horizontal,
            }],
        );
        ov.grid = Some(0);
        ov.attrs.0.retain(|(k, _)| k != "location");
        ov.attrs.0.push((
            "sampling".into(),
            text(&format!("{op},{}", node.args.join(","))),
        ));
        ov.attrs
            .0
            .push(("sampling_method".into(), text(methods[k].name())));
        ov.attrs.0.extend(out_attrs.iter().cloned());
        if outside_total > 0 {
            ov.attrs.0.push((
                "points_outside".into(),
                AttrValue::Ints(vec![outside_total as i64]),
            ));
        }
        out_vars.push(ov);
    }

    // read only the source cells the weights link
    for (k, &gi) in used.iter().enumerate() {
        let Some((sel, kept)) = prune(&input.grids[gi], &grids[k]) else {
            continue;
        };
        grids[k] = Arc::new(
            grids[k]
                .restrict_sources(&kept)
                .map_err(|e| Error::internal(e.to_string()))?,
        );
        let g = &mut input.grids[gi];
        if let Some(xv) = &mut g.xvals {
            *xv = sel[sel.len() - 1].to_vec().iter().map(|&i| xv[i]).collect();
        }
        for (j, m) in sel.iter().enumerate() {
            g.sel[j] = g.sel[j].compose(m);
        }
        if g.kind == GridKind::Healpix {
            g.kind = GridKind::Unstructured;
        }
        for (vi, v) in input.vars.iter_mut().enumerate() {
            if v.grid != Some(gi) {
                continue;
            }
            let hd = v.hdims();
            for (j, m) in sel.iter().enumerate() {
                v.select(hd[j], m);
            }
            fvars[vi].src_size = kept.len();
        }
    }

    // the output grid: the points, with the variables along them
    let gsrc = crate::remap::target::points(op, dim, &plon, &plat)?;
    let base = gsrc
        .dataset()
        .grids
        .first()
        .cloned()
        .ok_or_else(|| Error::internal("no grid for the sample points"))?;
    let coord = |std: &str, units: &str| {
        Attrs(vec![
            ("standard_name".into(), text(std)),
            ("long_name".into(), text(std)),
            ("units".into(), text(units)),
        ])
    };
    let fixed = FixedGrid {
        coords: GridCoords {
            xvals: plon,
            yvals: plat,
            xbounds: None,
            ybounds: None,
            nv: 0,
            xunits: "degrees_east".into(),
            yunits: "degrees_north".into(),
        },
        xattrs: coord("longitude", "degrees_east"),
        yattrs: coord("latitude", "degrees_north"),
        aux,
        labels,
    };
    let tgrid = GridDesc {
        kind: GridKind::Unstructured,
        base,
        src: Arc::new(gsrc),
        sel: vec![IndexMap::identity(n)],
        xvals: None,
        fixed: Some(Arc::new(fixed)),
    };
    let map = SampleMap {
        op: op.to_owned(),
        grids,
        wet: wets,
        var_grid,
        dry,
        info,
    };
    Ok(Desc {
        attrs: input.attrs.clone(),
        vars: out_vars,
        grids: vec![tgrid],
        zaxes: input.zaxes.clone(),
        time: input.time.clone(),
        fold: Some(Fold {
            input: Box::new(input),
            kernel: Arc::new(FieldKernel {
                map: Arc::new(map),
                vars: fvars,
            }),
        }),
    })
}
