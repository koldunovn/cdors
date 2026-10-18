//! cdors-mesh: the mesh vocabulary and the spherical geometry of cdors, in pure Rust (no
//! netCDF/HDF5), so that a Python module can later be built from it.
//!
//! - [`mesh`]: a [`Mesh`] is a set of [`Place`]s (nodes, elements, cells) with centres and areas,
//!   CSR incidence tables between them, and an optional [`Vertical`] with the wet levels of each
//!   cell. Components that grow with the number of cells and levels load lazily ([`Lazy`]), and
//!   report their size so that the caller can count them against its memory budget.
//! - [`geom`]: unit vectors, great-circle distances and paths, spherical polygon areas (cdo's
//!   L'Huilier fans), and point-in-polygon tests for regions in planar lon/lat with regionmask's
//!   rules.
//! - [`locate`]: a KD-tree on unit vectors for nearest-cell lookups.
//! - [`reader`]: the [`ArrayReader`] interface through which loaders read mesh files; the
//!   NetCDF and Zarr implementations live in cdors-core.

pub mod geom;
pub mod locate;
pub mod mesh;
pub mod reader;

pub use mesh::{
    Csr, Lazy, Levels, Mesh, Place, Polygons, Source, Table, Thickness, Vertical, WetLevels,
};
pub use reader::{ArrayReader, Attr, MemReader, VarInfo};

/// Errors of mesh loading and geometry setup. cdors-core maps them to its error codes.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum MeshError {
    /// A variable, dimension or attribute the loader needs is missing.
    #[error("{0}")]
    Missing(String),
    /// The values are inconsistent (sizes, indices out of range, bad polygons).
    #[error("{0}")]
    Invalid(String),
    /// The reader failed.
    #[error("{0}")]
    Read(String),
}

pub type Result<T> = std::result::Result<T, MeshError>;
