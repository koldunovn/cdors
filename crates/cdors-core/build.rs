//! Links libaec (CCSDS decoding of GRIB2 data, `io::grib`) statically from `LIBAEC_DIR`. The
//! default is the libaec whose szip library the spack HDF5 loads, so that the process holds one
//! version of libaec's symbols.
fn main() {
    println!("cargo:rerun-if-env-changed=LIBAEC_DIR");
    let dir = std::env::var("LIBAEC_DIR")
        .unwrap_or_else(|_| "/sw/spack-levante/libaec-1.0.6-wejyoe".to_owned());
    println!("cargo:rustc-link-search=native={dir}/lib64");
    println!("cargo:rustc-link-search=native={dir}/lib");
    println!("cargo:rustc-link-lib=static=aec");
}
