//! FESOM2 meshes, read from `fesom.mesh.diag.nc` (the diagnostic mesh file FESOM2 writes).
//!
//! - Places: `nodes` (with per-level areas `nod_area`) and `elements` (triangles, `elem_area`);
//!   the table `elem_nodes` (elements → their three nodes).
//! - Vertical: full layers between the interfaces `zbar`/`nz` (FESOM2's default
//!   `use_partial_cell=.false.`), wet levels per node and per element.
//! - Rules ([`rules`]): the dimensions of FESOM2 output files and what they are.

pub mod loader;
pub mod rules;

use super::Model;

pub static MODEL: Model = Model {
    name: "fesom2",
    prefix: "fesom",
    files: &["fesom.mesh.diag.nc"],
    options: &["units"],
    load: loader::load,
    version: 1,
    dims: rules::DIMS,
    native: &["per_level_area", "elem_nodes", "wet_levels"],
};
