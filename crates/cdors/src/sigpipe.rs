//! Broken pipes: behave like a C command-line tool.
//!
//! The Rust runtime ignores SIGPIPE, so a write to a closed pipe (`cdors --version | head -1`,
//! `cdors outputtab,... | head`) returns `EPIPE` and `println!` panics. Restoring the default
//! action makes the process end quietly on SIGPIPE instead, as cdo and other C tools do.

use std::ffi::c_int;

const SIGPIPE: c_int = 13;
const SIG_DFL: usize = 0;

unsafe extern "C" {
    fn signal(signum: c_int, handler: usize) -> usize;
}

/// Restores the default SIGPIPE action (terminate quietly). Call once at startup, before any
/// thread is started.
pub fn reset() {
    // SAFETY: plain libc call at startup; SIG_DFL is a valid handler for SIGPIPE.
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}
