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
    options: &[("units", &["deg", "degrees", "rad", "radians"])],
    load: loader::load,
    version: 2,
    dims: rules::DIMS,
    native: &["per_level_area", "elem_nodes", "wet_levels"],
    places: &[
        (
            "nodes",
            "the node coordinates of fesom.mesh.diag.nc (nodes)",
            "nod_area: layer k weighs with row k, the area at the layer's top interface, which is 0 \
             at and below the sea floor; 2-D fields with the surface row",
        ),
        (
            "elements",
            "the normalised mean of the unit vectors of the element's three nodes",
            "elem_area, one area for all levels",
        ),
    ],
};
