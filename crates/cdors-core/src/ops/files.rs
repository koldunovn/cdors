//! `copy` (format change and rechunking happen in the writer: `-f`, `--chunks`) and
//! `setgrid,<file>` (take the horizontal grid of another dataset, e.g. the mesh file of FESOM
//! output, when the number of points matches).

use crate::chain::OpNode;
use crate::error::{Error, ErrorCode, Result};
use crate::model::{DimRole, GridKind};
use crate::plan::{Desc, GridDesc, IndexMap, Sources};

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
