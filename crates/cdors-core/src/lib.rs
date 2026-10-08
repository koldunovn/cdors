//! cdors-core: chunk-wise engine for CDO-style climate statistics on Zarr and NetCDF.
//!
//! - [`error`]: error codes, hints, exit codes, JSON rendering
//! - [`model`]: dataset description (variables, grids, vertical axes, CF time)
//! - [`io`]: the chunk-source interface and the Zarr and NetCDF readers
//! - [`chain`]: the parsed command (options and operator tree)
//! - [`ops`]: the operator registry and the operators
//! - [`plan`]: operator descriptions, stages and tiles
//! - [`exec`]: the streaming executor and output layout
//! - [`kernels`]: numerical kernels on in-memory tiles (percentiles)

pub mod chain;
pub mod error;
pub mod exec;
pub mod io;
pub mod kernels;
pub mod model;
pub mod ops;
pub mod plan;

pub mod remap;

/// Version of the cdors crates.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Versions of the native libraries this build is linked against (HDF5 and netCDF-C).
pub fn native_library_versions() -> String {
    let (major, minor, patch) = hdf5_metno::library_version();
    format!(
        "HDF5 {major}.{minor}.{patch}, netCDF-C {}",
        netcdf_version()
    )
}

fn netcdf_version() -> String {
    unsafe extern "C" {
        fn nc_inq_libvers() -> *const std::ffi::c_char;
    }
    // SAFETY: nc_inq_libvers returns a pointer to a static NUL-terminated string.
    let s = unsafe { std::ffi::CStr::from_ptr(nc_inq_libvers()) };
    s.to_string_lossy()
        .split_whitespace()
        .next()
        .unwrap_or("?")
        .to_owned()
}
