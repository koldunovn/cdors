//! Numerical kernels shared by the operators: pure functions on in-memory tiles, no I/O.
//!
//! - [`percentile`]: exact percentiles with CDO's methods (`--percentile`)

pub mod percentile;
