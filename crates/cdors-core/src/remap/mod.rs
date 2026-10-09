//! Horizontal remapping with SCRIP weights.
//!
//! cdors never computes interpolation weights itself: `generate` (gen.rs) asks cdo (`cdo gen<method>`) for a
//! SCRIP weight file and caches it, `weights` reads such a file into a sparse matrix (CSR by
//! destination cell) and applies it to fields with CDO's missing-value rules.
//!
//! Typical use (the operator wiring calls exactly this):
//!
//! ```no_run
//! use cdors_core::remap::{GenMethod, RemapWeights, WeightCache, WeightRequest};
//! # fn main() -> Result<(), cdors_core::remap::RemapError> {
//! let cache = WeightCache::from_env()?;
//! let req = WeightRequest::new(GenMethod::Con, "r18x9", "in.nc".as_ref());
//! let path = cache.weights_for(&req)?;
//! let w = RemapWeights::read(&path)?;
//! let src = vec![1.0f32; w.src_size() * 10]; // 10 fields (timesteps/levels)
//! let mut dst = vec![0.0f32; w.dst_size() * 10];
//! w.apply_batch(&src, &mut dst)?;
//! # Ok(()) }
//! ```

// `gen` is a reserved keyword in edition 2024.
#[path = "gen.rs"]
pub mod generate;
pub mod target;
pub mod weights;

pub use generate::{GenMethod, SourceIdentity, WeightCache, WeightRequest};
pub use weights::{MapMethod, Normalization, RemapWeights, Value};

use std::path::PathBuf;

/// Hint attached to every failure of weight generation.
pub const PRECOMPUTED_HINT: &str = "pass precomputed weights with remap,<grid>,<weights.nc>";

/// Errors of weight generation, reading and application.
#[derive(Debug, thiserror::Error)]
pub enum RemapError {
    #[error("cannot read weight file {path}: {source}")]
    Netcdf {
        path: PathBuf,
        #[source]
        source: netcdf::Error,
    },
    #[error("weight file {path}: {msg}")]
    Format { path: PathBuf, msg: String },
    #[error("{0}")]
    Unsupported(String),
    #[error("field size {got} does not match the weights' {what} grid size {expected}")]
    Size {
        what: &'static str,
        expected: usize,
        got: usize,
    },
    #[error("cdo not available ({reason}); hint: {hint}")]
    CdoMissing { reason: String, hint: &'static str },
    #[error("`{command}` failed: {stderr}; hint: {hint}")]
    CdoFailed {
        command: String,
        stderr: String,
        hint: &'static str,
    },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
