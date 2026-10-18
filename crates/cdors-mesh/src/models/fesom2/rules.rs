//! FESOM2 rules: what the dimensions of FESOM2 output files are.
//!
//! - `nod2` (newer files) and `nod_n` (older mesh files): the nodes; `elem` and `elem_n`: the
//!   elements;
//! - `nz` / `nl`: level interfaces; `nz1` / `nl1`: mid-levels (layers).
//!
//! Levels below a cell's bottom (and above its top, under ice-shelf cavities) are dry: they are
//! missing whatever the file stores there (FESOM2 writes 0). The wet range of each cell is in
//! the mesh's [`crate::Vertical::wet`]; [`super::super::wet_range`] gives it for a dimension.

use crate::models::DimRule;

pub const DIMS: &[(&str, DimRule)] = &[
    ("nod2", DimRule::Place("nodes")),
    ("nod_n", DimRule::Place("nodes")),
    ("elem", DimRule::Place("elements")),
    ("elem_n", DimRule::Place("elements")),
    ("nz", DimRule::Interfaces),
    ("nl", DimRule::Interfaces),
    ("nz1", DimRule::Mid),
    ("nl1", DimRule::Mid),
];

/// Wet mid-levels of each cell from FESOM2's level counts: `nlevels` counts the interfaces
/// down to the bottom (a cell with `nlevels = 5` has the 4 layers 1..=4), `ulevels` is the
/// first wet layer (1-based; 1 without cavities). Returns 0-based half-open ranges
/// `top..bottom`.
pub fn wet_ranges(
    nlevels: &[f64],
    ulevels: Option<&[f64]>,
    nlev: usize,
    what: &str,
) -> crate::Result<(Vec<u16>, Vec<u16>)> {
    let bad = |i: usize, m: String| {
        crate::MeshError::Invalid(format!("{what} {}: {m} ({nlev} layers)", i + 1))
    };
    let mut top = Vec::with_capacity(nlevels.len());
    let mut bottom = Vec::with_capacity(nlevels.len());
    for (i, &n) in nlevels.iter().enumerate() {
        let u = ulevels.map_or(1.0, |u| u[i]);
        if !(n >= 1.0 && n <= nlev as f64 + 1.0 && u >= 1.0 && u <= n) {
            return Err(bad(i, format!("levels {u}..{n} out of range")));
        }
        top.push((u - 1.0) as u16);
        bottom.push((n - 1.0) as u16);
    }
    Ok((top, bottom))
}
