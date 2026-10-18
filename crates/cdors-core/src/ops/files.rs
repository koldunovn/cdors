//! `copy` (format change and rechunking happen in the writer: `-f`, `--chunks`),
//! `setgrid,<file>` (take the horizontal grid of another dataset, e.g. the mesh file of FESOM
//! output, when the number of points matches), and `mergetime` / `cat` (several files, or one
//! glob pattern, as one input concatenated along time).
//!
//! `mergetime` orders its inputs by their first timestep, `cat` keeps the argument order (a
//! glob pattern expands to sorted names). Both read the inputs as one virtual dataset
//! ([`MultiFileSource`]) rather than copying them, so the rest of the chain sees one input.
//! Differences from cdo: time must increase from one input to the next (cdo `mergetime`
//! interleaves overlapping inputs step by step and keeps repeated timesteps unless
//! `skip_same_time`; cdo `cat` appends in any order, and appends to an existing output
//! without `-O`); inputs must be files or stores, not the output of other operators.

use crate::chain::{Input, OpNode};
use crate::error::{Error, ErrorCode, Result};
use crate::io::ChunkSource;
use crate::io::multifile::{MultiFileSource, expand_glob, is_glob};
use crate::model::mesh_cache;
use crate::model::{AttrValue, Attrs, DimRole, Grid, GridKind, ZAxis};
use crate::plan::{CdoUse, Desc, Expr, GridDesc, IndexMap, Sources, VarDesc, WetMask, ZDesc};
use cdors_mesh::models::{self, DimRule, MeshSpec};
use cdors_mesh::{Mesh, Place};
use std::sync::Arc;

/// Whether `name` is an operator that concatenates its input files along time.
pub fn is_concat(name: &str) -> bool {
    matches!(name, "mergetime" | "cat")
}

/// Input paths of `mergetime` / `cat`, glob patterns expanded.
fn concat_paths(node: &OpNode) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for i in &node.inputs {
        match i {
            Input::Path(p) if is_glob(p) && !std::path::Path::new(p).exists() => {
                paths.extend(expand_glob(p)?);
            }
            Input::Path(p) => paths.push(p.clone()),
            Input::Op(o) => {
                return Err(Error::new(
                    ErrorCode::NotImplemented,
                    format!(
                        "{} of the output of another operator (-{}) is not implemented yet",
                        node.name, o.name
                    ),
                )
                .with("operator", node.name.clone())
                .with_hint(format!(
                    "give {} files or a quoted glob pattern, and apply -{} outside it",
                    node.name, o.name
                )));
            }
        }
    }
    Ok(paths)
}

/// Opens the inputs of `mergetime` / `cat` as one dataset.
pub fn open_concat(node: &OpNode) -> Result<Arc<dyn ChunkSource>> {
    let paths = concat_paths(node)?;
    match paths.as_slice() {
        [] => Err(Error::bad_arguments(format!("{} needs inputs", node.name))),
        [one] => crate::io::open(one),
        _ => Ok(Arc::new(MultiFileSource::open_with(
            &paths,
            node.name == "mergetime",
        )?)),
    }
}

/// Output description of `mergetime` / `cat`: the concatenated dataset as one source.
pub fn describe_concat(node: &OpNode, srcs: &mut Sources) -> Result<Desc> {
    crate::ops::require_implemented(node)?;
    let key = format!("-{} {}", node.name, concat_paths(node)?.join(" "));
    let si = match srcs.paths.iter().position(|p| *p == key) {
        Some(i) => i,
        None => {
            let src = open_concat(node)?;
            srcs.paths.push(key);
            srcs.srcs.push(src);
            srcs.srcs.len() - 1
        }
    };
    crate::plan::describe_source(srcs, si)
}

/// The grid of the cells of `place` of `mesh` over the data's dimension `dim`: centres, 2-D
/// areas and the mesh itself (`Grid::mesh`). The mesh's first place has coordinates `lon`/`lat`,
/// the others `lon_<dim>`/`lat_<dim>` (`remap::target::mesh_place`).
pub(crate) fn mesh_grid(
    origin: &str,
    mesh: &Arc<Mesh>,
    place: &Place,
    dim: &str,
) -> Result<GridDesc> {
    let first = mesh.places.first().is_some_and(|p| p.name == place.name);
    let gsrc = crate::remap::target::mesh_place(origin, dim, place, !first)?;
    let mut grid = gsrc.dataset().grids.first().cloned().ok_or_else(|| {
        Error::internal(format!(
            "no grid for place '{}' of {}",
            place.name, mesh.name
        ))
    })?;
    grid.mesh = Some(mesh.clone());
    grid.mesh_place = Some(place.name.clone());
    Ok(GridDesc {
        kind: grid.kind,
        sel: vec![IndexMap::identity(grid.size)],
        base: grid,
        src: Arc::new(gsrc),
        xvals: None,
        fixed: None,
    })
}

/// Masks the dry levels of `v` on `place` of `mesh`: the value `setgrid` receives (a stored
/// variable, arithmetic on it, an intermediate result) is wrapped in an [`Expr::Mask`] when it has
/// a vertical dimension whose levels are known on the mesh. The mesh level of each index along it
/// comes from, in this order:
/// - a mask the value already has on that dimension (`setgrid` twice, a result of mesh operators);
/// - a stored dimension the model's rules name (FESOM2 `nz1` mid-levels, `nz` interfaces), or a
///   vertical one with as many levels as the mesh has layers (interfaces), followed by a leaf: the
///   stored index is the mesh level (selections before `setgrid` included);
/// - the depths of the level axis matched to the mesh's mid-levels (interfaces), for results whose
///   stored levels are a selection (`-setgrid -yearmean -sellevel,...`).
///
/// 2-D variables and levels the mesh does not know are left as they are.
fn mask_dry_levels(
    v: &mut VarDesc,
    zaxes: &[ZDesc],
    mesh: &Arc<Mesh>,
    place: &Place,
    srcs: &Sources,
) {
    let Some(vert) = mesh
        .vertical
        .as_ref()
        .filter(|z| z.wet(&place.name).is_some())
    else {
        return;
    };
    let Some(&hd) = v.hdims().first() else {
        return;
    };
    let named = |name: &str| match models::dim_rule(mesh, name) {
        Some(r @ (DimRule::Mid | DimRule::Interfaces)) => Some(r),
        _ => None,
    };
    let Some(zd) = (0..v.dims.len()).find(|&i| {
        i != hd && (v.dims[i].role == DimRole::Vertical || named(&v.dims[i].name).is_some())
    }) else {
        return;
    };
    let nlev = vert.nlev();
    let count = |r: DimRule| if r == DimRule::Mid { nlev } else { nlev + 1 };
    let by_size = |n: usize| {
        (n == nlev)
            .then_some(DimRule::Mid)
            .or((n == nlev + 1).then_some(DimRule::Interfaces))
    };
    // (a) a mask the value already has
    let known = v
        .expr
        .wet_mask()
        .filter(|(m, dl, _)| *dl == zd && Arc::ptr_eq(&m.mesh, mesh))
        .map(|(m, _, levels)| (m.rule, levels));
    // (b) a leaf whose stored dimension is the mesh's levels
    let stored = || {
        v.expr.leaves().into_iter().find_map(|leaf| {
            let dvar = srcs.srcs.get(leaf.src)?.dataset().var(&leaf.var)?;
            let p = leaf.maps.iter().position(|(od, _)| *od == zd)?;
            let sd = dvar.dims.get(p)?;
            let rule = named(&sd.name).or_else(|| by_size(sd.size))?;
            (sd.size == count(rule)).then(|| (rule, leaf.maps[p].1.clone()))
        })
    };
    // (c) the depths of the level axis
    let depths = || {
        let values = &zaxes.get(v.zaxis?)?.axis.values;
        if values.len() != v.dims[zd].size {
            return None;
        }
        let rules = match named(&v.dims[zd].name) {
            Some(r) => vec![r],
            None => vec![DimRule::Mid, DimRule::Interfaces],
        };
        rules.into_iter().find_map(|r| {
            let z = if r == DimRule::Mid {
                &vert.mid
            } else {
                &vert.interfaces
            };
            let idx: Option<Vec<usize>> = values
                .iter()
                .map(|&x| {
                    z.iter()
                        .position(|&m| (m.abs() - x.abs()).abs() <= 1e-6 * x.abs().max(1.0))
                })
                .collect();
            idx.map(|i| (r, IndexMap::from_list(i)))
        })
    };
    let Some((rule, levels)) = known.or_else(stored).or_else(depths) else {
        return;
    };
    let mask = WetMask {
        mesh: mesh.clone(),
        place: place.name.clone(),
        rule,
        level: (zd, levels),
        cell: (hd, IndexMap::identity(place.size)),
    };
    let e = std::mem::replace(
        &mut v.expr,
        Expr::Leaf(crate::plan::Leaf {
            src: 0,
            var: String::new(),
            maps: Vec::new(),
        }),
    );
    v.expr = Expr::Mask(Arc::new(mask), Box::new(e));
}

/// Gives the level axis of every variable on the mesh's mid-levels the mesh's layers as bounds
/// (interfaces `k`, `k + 1` of mesh level `k`, in the sign of the axis values), unless it has
/// bounds already: operators that leave the mesh (`transect`, `selpoints`, ...) and written files
/// keep the layer thicknesses, so `vert*` after them weigh as on the mesh. Levels on the mesh's
/// interfaces get no bounds.
fn layer_bounds(d: &mut Desc) {
    for v in &d.vars {
        let (Some(zi), Some((m, zd, levels))) = (v.zaxis, v.expr.wet_mask()) else {
            continue;
        };
        let (Some(vert), DimRule::Mid) = (m.mesh.vertical.as_ref(), m.rule) else {
            continue;
        };
        let Some(z) = d.zaxes.get_mut(zi) else {
            continue;
        };
        let n = v.dims[zd].size;
        if z.axis.bounds.is_some() || z.axis.len() != n {
            continue;
        }
        let zb = &vert.interfaces;
        let down = z.axis.values.iter().all(|&x| x >= 0.0);
        let sign = |x: f64| if down { x.abs() } else { -x.abs() };
        let b: Option<Vec<[f64; 2]>> = (0..n)
            .map(|i| {
                let k = levels.get(i);
                Some([sign(*zb.get(k)?), sign(*zb.get(k + 1)?)])
            })
            .collect();
        z.axis.bounds = b;
    }
}

/// Rate at which building a mesh cache entry reads and writes the mesh (measured on a Levante
/// login node: FESOM2 ng5, 5.0 GB in 37 s), for `--plan`'s rough estimate.
const MESH_BUILD_BYTES_PER_S: f64 = 1.3e8;

/// `--plan` without a usable mesh cache entry: what the first run will build.
fn pending_warning(loaded: &mesh_cache::Loaded, srcs: &mut Sources) {
    let Some(entry) = &loaded.entry else {
        return;
    };
    let m = &loaded.mesh;
    let bytes = m.resident_bytes() + m.lazy_bytes().0;
    let secs = (bytes as f64 / MESH_BUILD_BYTES_PER_S).ceil().max(1.0);
    super::regions::warn(
        srcs,
        "mesh_cache_pending",
        format!(
            "the mesh cache entry {} does not exist yet (or is not usable): this plan read the mesh \
             {} from its source files and wrote nothing; the first run builds the entry \
             (about {} B, roughly {secs:.0} s)",
            entry.display(),
            m.name,
            bytes
        ),
        serde_json::json!({
            "mesh": loaded.spec,
            "cache_entry": entry.display().to_string(),
            "build_bytes": bytes,
            "estimated_build_s": secs,
        }),
    );
}

/// Variables whose place and level dimensions the reader took as one generic 2-D grid, as
/// FESOM 2.0 files without an `nz1` coordinate variable come (`temp(time, nod2, nz1)`: one grid
/// of 3140 × 47 points): when the model's rules name one of the two dimensions a place of the
/// mesh and the other its mid-levels or interfaces, with the mesh's sizes, the variable gets a
/// 1-D grid over the place dimension (the one other variables on it have, if any) and a level
/// axis with the mesh's depths (|Z| for mid-levels, |zbar| for interfaces, positive down; mid
/// levels get the layer bounds later, as files with the coordinate do). Either order.
fn split_levels(d: &mut Desc, mesh: &Mesh) -> Vec<usize> {
    let mut split = Vec::new();
    let Some(vert) = mesh.vertical.as_ref() else {
        return split;
    };
    for gi in 0..d.grids.len() {
        let g = &d.grids[gi];
        if g.sel.len() != 2 || g.base.dims.len() != 2 || g.kind != GridKind::Generic {
            continue;
        }
        let sizes = [g.sel[0].len(), g.sel[1].len()];
        if (0..2).any(|k| !g.sel[k].is_identity(sizes[k])) {
            continue;
        }
        let rules: Vec<Option<DimRule>> = g
            .base
            .dims
            .iter()
            .map(|n| models::dim_rule(mesh, n))
            .collect();
        let Some(h) = (0..2).find(|&k| {
            matches!(rules[k], Some(DimRule::Place(p))
                if mesh.place(p).is_some_and(|pl| pl.size == sizes[k]))
        }) else {
            continue;
        };
        let z = 1 - h;
        let (zrule, values) = match rules[z] {
            Some(DimRule::Mid) => (DimRule::Mid, &vert.mid),
            Some(DimRule::Interfaces) => (DimRule::Interfaces, &vert.interfaces),
            _ => continue,
        };
        if values.len() != sizes[z] {
            continue;
        }
        let (hname, zname) = (g.base.dims[h].clone(), g.base.dims[z].clone());
        // the 1-D grid over the place dimension
        let hgrid = match d.grids.iter().position(|o| {
            o.sel.len() == 1 && o.base.dims == [hname.clone()] && o.size() == sizes[h]
        }) {
            Some(i) => i,
            None => {
                let mut base = Grid::new(GridKind::Generic, vec![hname.clone()], sizes[h], 0);
                base.size = sizes[h];
                let src = d.grids[gi].src.clone();
                d.grids.push(GridDesc {
                    kind: GridKind::Generic,
                    base,
                    src,
                    sel: vec![IndexMap::identity(sizes[h])],
                    xvals: None,
                    fixed: None,
                });
                d.grids.len() - 1
            }
        };
        // the level axis
        let zi = match d.zaxes.iter().position(|a| a.axis.dim == zname) {
            Some(i) => i,
            None => {
                let long = if zrule == DimRule::Mid {
                    "depth at layer midpoint"
                } else {
                    "depth of level interfaces"
                };
                let text = |t: &str| AttrValue::Text(t.to_owned());
                d.zaxes.push(ZDesc {
                    axis: ZAxis {
                        var: zname.clone(),
                        dim: zname.clone(),
                        values: values.iter().map(|x| x.abs()).collect(),
                        units: Some("m".into()),
                        long_name: Some(long.into()),
                        standard_name: Some("depth".into()),
                        positive: Some("down".into()),
                        bounds: None,
                    },
                    attrs: Attrs(vec![
                        ("standard_name".into(), text("depth")),
                        ("long_name".into(), text(long)),
                        ("units".into(), text("m")),
                        ("positive".into(), text("down")),
                        ("axis".into(), text("Z")),
                        (
                            "comment".into(),
                            text(&format!("level depths of the mesh {}", mesh.name)),
                        ),
                    ]),
                    bounds_var: None,
                    labels: Vec::new(),
                });
                d.zaxes.len() - 1
            }
        };
        split.push(gi);
        for v in &mut d.vars {
            if v.grid != Some(gi) {
                continue;
            }
            v.grid = Some(hgrid);
            v.zaxis = Some(zi);
            for dim in &mut v.dims {
                if dim.name == zname {
                    dim.role = DimRole::Vertical;
                }
            }
        }
    }
    split
}

/// The hint for a 2-D generic grid whose size is that of a place times the mesh's levels: the
/// reader grouped cells and levels into one grid, and `split_levels` could not tell them apart
/// because the model's rules do not name both dimensions.
fn levels_hint(mesh: &Mesh, g: &GridDesc) -> Option<String> {
    let nlev = mesh.vertical.as_ref()?.nlev();
    if g.sel.len() != 2 {
        return None;
    }
    let n = g.size();
    mesh.places.iter().find_map(|p| {
        let k = [nlev, nlev + 1].into_iter().find(|&k| p.size * k == n)?;
        let known: Vec<&str> = models::model(&mesh.source.model)
            .map(|m| m.dims.iter().map(|d| d.0).collect())
            .unwrap_or_default();
        Some(format!(
            "{n} = {} {} x {k} levels: the reader took the cells and the levels ({}) as one \
             grid; setgrid separates them when the dimensions have the names the {} rules give \
             them ({})",
            p.size,
            p.name,
            g.base.dims.join(", "),
            mesh.source.model,
            known.join(", ")
        ))
    })
}

/// `setgrid,<mesh spec>`: each input grid whose dimension the model's rules name (`nod2` →
/// `nodes`), or else whose size one place has, becomes that place of the mesh: its centres,
/// its 2-D areas, and the mesh itself on the grid (`Grid::mesh`); the dry levels of variables
/// with levels become missing ([`mask_dry_levels`]).
fn setgrid_mesh(spec: &MeshSpec, mut d: Desc, srcs: &mut Sources) -> Result<Desc> {
    // only a run's final planning pass writes the mesh cache: `--plan` and the pass the
    // refusals are checked on read an existing entry or the source files
    let loaded = mesh_cache::load_with(spec, srcs.cdo == CdoUse::Now)?;
    if loaded.pending {
        match srcs.cdo {
            CdoUse::Defer => srcs.deferred = true,
            _ => pending_warning(&loaded, srcs),
        }
    }
    let mesh = loaded.mesh;
    if !srcs.meshes.iter().any(|m| Arc::ptr_eq(m, &mesh)) {
        srcs.meshes.push(mesh.clone());
    }
    let places = || {
        mesh.places
            .iter()
            .map(|p| format!("{} {}", p.name, p.size))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let split = split_levels(&mut d, &mesh);
    let mut replaced = false;
    let mut unmatched = Vec::new();
    for gi in 0..d.grids.len() {
        let old = &d.grids[gi];
        if split.contains(&gi) {
            // its variables moved to a grid over their place dimension
            continue;
        }
        let dim = old.base.dims.first().cloned().unwrap_or_default();
        let named = match models::dim_rule(&mesh, &dim) {
            Some(DimRule::Place(p)) => mesh.place(p),
            _ => None,
        };
        let place = match named {
            Some(p) => p,
            None if old.sel.len() == 1 => match mesh.place_of_size(old.size()) {
                Some(p) => p,
                None => {
                    unmatched.push(format!("{dim} {}", old.size()));
                    continue;
                }
            },
            None => {
                unmatched.push(format!("{} {}", old.base.dims.join("x"), old.size()));
                continue;
            }
        };
        if old.sel.len() != 1 || old.size() != place.size || old.base.size != place.size {
            let what = if old.sel.len() == 1 {
                format!("dimension '{dim}'")
            } else {
                format!("the grid {}", old.base.dims.join(" x "))
            };
            let hint = levels_hint(&mesh, old).unwrap_or_else(|| {
                format!(
                    "the data must be on the full mesh {} (places: {}); apply selections after \
                     -setgrid, and check that the mesh directory is the one the data was computed \
                     on",
                    mesh.name,
                    places()
                )
            });
            return Err(Error::new(
                ErrorCode::GridMismatch,
                format!(
                    "setgrid: {what} has {} points, place '{}' of the mesh {} has {}",
                    old.size(),
                    place.name,
                    mesh.name,
                    place.size
                ),
            )
            .with("mesh", loaded.spec.clone())
            .with_hint(hint));
        }
        d.grids[gi] = mesh_grid(&loaded.spec, &mesh, place, &dim)?;
        // the grid's copies of the centres and areas (written with the output) stay resident
        let b = &d.grids[gi].base;
        srcs.mesh_copies += 8
            * (3 * place.size
                + b.xvals.as_ref().map_or(0, Vec::len)
                + b.yvals.as_ref().map_or(0, Vec::len)) as u64;
        replaced = true;
        for v in &mut d.vars {
            if v.grid == Some(gi) {
                for i in v.hdims() {
                    v.dims[i].role = DimRole::Horizontal;
                }
                mask_dry_levels(v, &d.zaxes, &mesh, place, srcs);
            }
        }
    }
    layer_bounds(&mut d);
    if !replaced {
        let hint = d
            .grids
            .iter()
            .find_map(|g| levels_hint(&mesh, g))
            .unwrap_or_else(|| {
                "the horizontal dimension must be named as the model writes it (FESOM2: nod2, \
                 elem) or have the size of one place; check that the mesh is the one of the data"
                    .into()
            });
        return Err(Error::new(
            ErrorCode::GridMismatch,
            format!(
                "setgrid: no input grid fits the mesh {}: input grids {}, mesh places {}",
                mesh.name,
                if unmatched.is_empty() {
                    "none".into()
                } else {
                    unmatched.join(", ")
                },
                places()
            ),
        )
        .with("mesh", loaded.spec.clone())
        .with_hint(hint));
    }
    Ok(d)
}

fn setgrid(node: &OpNode, mut d: Desc, srcs: &mut Sources) -> Result<Desc> {
    // a mesh spec may carry options: `-setgrid,fesom:<dir>,units=deg`
    if let Some(spec) = mesh_cache::parse_spec(&node.args.join(",")) {
        return setgrid_mesh(&spec?, d, srcs);
    }
    let path = &node.args[0];
    if node.args.len() > 1 {
        return Err(Error::bad_arguments(format!(
            "setgrid,{}: options are for mesh specs only (fesom:<dir>,units=deg)",
            node.args.join(",")
        )));
    }
    if !std::path::Path::new(path).exists() {
        return Err(Error::new(
            ErrorCode::NotImplemented,
            format!("setgrid,{path}: only grids from files are supported (no such file)"),
        )
        .with_hint("give a NetCDF or Zarr file that has the target grid (e.g. a mesh file)"));
    }
    let si = srcs.open(path)?;
    let src = srcs.srcs[si].clone();
    let ds = src.dataset();
    let grid = ds
        .data_vars()
        .find_map(|v| v.grid)
        .map(|g| ds.grids[g].clone())
        .or_else(|| ds.grids.first().cloned())
        .filter(|g| g.kind != GridKind::Generic)
        .ok_or_else(|| {
            Error::new(
                ErrorCode::NoCoordinates,
                format!("'{path}' has no grid with coordinates"),
            )
            .with("path", path.clone())
        })?;
    let new = GridDesc {
        kind: grid.kind,
        sel: if grid.ysize > 0 && grid.dims.len() == 2 {
            vec![
                IndexMap::identity(grid.ysize),
                IndexMap::identity(grid.xsize),
            ]
        } else {
            vec![IndexMap::identity(grid.size)]
        },
        base: grid,
        src,
        xvals: None,
        fixed: None,
    };
    let mut replaced = false;
    for gi in 0..d.grids.len() {
        let old = &d.grids[gi];
        if old.size() != new.size() || old.sel.len() != new.sel.len() {
            continue;
        }
        if old.sel.len() == 2 && old.xy_size() != new.xy_size() {
            continue;
        }
        d.grids[gi] = new.clone();
        replaced = true;
        for v in &mut d.vars {
            if v.grid == Some(gi) {
                let hd = v.hdims();
                for (k, &i) in hd.iter().enumerate() {
                    v.dims[i].name = new.base.dims[k].clone();
                    v.dims[i].role = DimRole::Horizontal;
                }
            }
        }
    }
    if !replaced {
        let sizes: Vec<String> = d.grids.iter().map(|g| g.size().to_string()).collect();
        return Err(Error::new(
            ErrorCode::UnsupportedGrid,
            format!(
                "setgrid: the grid of '{path}' has {} points, the input grid(s) {}",
                new.size(),
                sizes.join(", ")
            ),
        )
        .with_hint("the new grid must have the same number of points (and the same rows and columns for 2-D grids)"));
    }
    Ok(d)
}

/// Output description of `copy` and `setgrid`.
pub fn describe(node: &OpNode, mut inputs: Vec<Desc>, srcs: &mut Sources) -> Result<Desc> {
    match node.name.as_str() {
        "copy" => {
            if inputs.len() > 1 {
                return Err(Error::new(
                    ErrorCode::NotImplemented,
                    "copy with several inputs (concatenation) is not implemented yet",
                )
                .with_hint("copy one input at a time"));
            }
            Ok(inputs.remove(0))
        }
        "setgrid" => setgrid(node, inputs.remove(0), srcs),
        other => Err(Error::internal(format!("'{other}' is not a file operator"))),
    }
}
