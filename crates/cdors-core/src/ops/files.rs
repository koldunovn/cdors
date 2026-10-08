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
use crate::model::{DimRole, GridKind};
use crate::plan::{Desc, GridDesc, IndexMap, Sources};
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

fn setgrid(node: &OpNode, mut d: Desc, srcs: &mut Sources) -> Result<Desc> {
    let path = &node.args[0];
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
