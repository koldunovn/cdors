# Source this file (`source env.sh`) before building or running cdors on Levante.
# Everything large (toolchain, build tree, caches, fixtures) lives under /work, not in $HOME.

_W=/work/ab0995/a270088

# Rust toolchain (installed with rustup --no-modify-path)
export RUSTUP_HOME=$_W/rust/rustup
export CARGO_HOME=$_W/rust/cargo
export CARGO_TARGET_DIR=$_W/cdors-target
export CARGO_BUILD_JOBS=16          # shared login node

# cdors runtime locations
export CDORS_CACHE=$_W/cdors-cache
export CDORS_FIXTURES=$CARGO_TARGET_DIR/fixtures

# Reference cdo 2.6.0 (spack build with NCZarr); `cdo` on PATH resolves to it
export CDO=/sw/spack-levante/cdo-2.6.0-akkxhz/bin/cdo

# netCDF-C 4.9.3-rc1 (spack netcdf-c-main) and the HDF5 it links against.
# netcdf-sys and hdf5-metno-sys must both use this HDF5 so that only one libhdf5 is loaded.
export NETCDF_DIR=/sw/spack-levante/netcdf-c-main-k4lh4v
export HDF5_DIR=/sw/spack-levante/hdf5-1.14.3-76f2fb
# HDF5 filter plugins (blosc, bzip2, ...) so the netCDF-C fallback can read blosc-compressed NetCDF-4 (EERIE ICON).
export HDF5_PLUGIN_PATH=/sw/spack-levante/netcdf-c-main-bdxvs5/plugins:/sw/spack-levante/netcdf-c-main-k4lh4v/plugins

# Embed the library directories as rpath so the binary runs without LD_LIBRARY_PATH.
# libnetcdf itself carries an rpath for its own dependencies (HDF5, MPI, NVHPC runtime).
export RUSTFLAGS="-C link-args=-Wl,-rpath,$NETCDF_DIR/lib64,-rpath,$HDF5_DIR/lib"

export PATH=$CARGO_HOME/bin:$(dirname "$CDO"):$NETCDF_DIR/bin:$PATH

unset _W
