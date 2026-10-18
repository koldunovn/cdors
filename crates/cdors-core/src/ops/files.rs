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
use crate::model::{DimRole, GridKind};
use crate::plan::{Desc, GridDesc, IndexMap, Sources};
use cdors_mesh::models::{self, DimRule, MeshSpec};
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

/// `setgrid,<mesh spec>`: each input grid whose dimension the model's rules name (`nod2` →
/// `nodes`), or else whose size one place has, becomes that place of the mesh: its centres,
/// its 2-D areas, and the mesh itself on the grid (`Grid::mesh`).
fn setgrid_mesh(spec: &MeshSpec, mut d: Desc) -> Result<Desc> {
    let loaded = mesh_cache::load(spec)?;
    let mesh = loaded.mesh;
    let places = || {
        mesh.places
            .iter()
            .map(|p| format!("{} {}", p.name, p.size))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut replaced = false;
    let mut unmatched = Vec::new();
    for gi in 0..d.grids.len() {
        let old = &d.grids[gi];
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
            return Err(Error::new(
                ErrorCode::GridMismatch,
                format!(
                    "setgrid: dimension '{dim}' has {} points, place '{}' of the mesh {} has {}",
                    old.size(),
                    place.name,
                    mesh.name,
                    place.size
                ),
            )
            .with("mesh", loaded.spec.clone())
            .with_hint(format!(
                "the data must be on the full mesh {} (places: {}); apply selections after \
                 -setgrid, and check that the mesh directory is the one the data was computed on",
                mesh.name,
                places()
            )));
        }
        let first = mesh.places.first().is_some_and(|p| p.name == place.name);
        let gsrc = crate::remap::target::mesh_place(&loaded.spec, &dim, place, !first)?;
        let mut grid = gsrc.dataset().grids.first().cloned().ok_or_else(|| {
            Error::internal(format!(
                "no grid for place '{}' of {}",
                place.name, mesh.name
            ))
        })?;
        grid.mesh = Some(mesh.clone());
        grid.mesh_place = Some(place.name.clone());
        d.grids[gi] = GridDesc {
            kind: grid.kind,
            sel: vec![IndexMap::identity(grid.size)],
            base: grid,
            src: Arc::new(gsrc),
            xvals: None,
            fixed: None,
        };
        replaced = true;
        for v in &mut d.vars {
            if v.grid == Some(gi) {
                for i in v.hdims() {
                    v.dims[i].role = DimRole::Horizontal;
                }
            }
        }
    }
    if !replaced {
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
        .with_hint(
            "the horizontal dimension must be named as the model writes it (FESOM2: nod2, elem) \
             or have the size of one place; check that the mesh is the one of the data",
        ));
    }
    Ok(d)
}

fn setgrid(node: &OpNode, mut d: Desc, srcs: &mut Sources) -> Result<Desc> {
    // a mesh spec may carry options: `-setgrid,fesom:<dir>,units=deg`
    if let Some(spec) = mesh_cache::parse_spec(&node.args.join(",")) {
        return setgrid_mesh(&spec?, d);
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
