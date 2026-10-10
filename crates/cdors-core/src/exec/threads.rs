//! Thread creation that degrades instead of panicking.
//!
//! Login nodes limit the number of processes and threads per user (`ulimit -u`), and agents run
//! several cdors processes next to builds and tests. When the system refuses a thread, a pool is
//! rebuilt with half as many threads (down to one, then retried a few times after short pauses),
//! and the run continues with what it got. One warning per process says so (one line of JSON
//! under `--json`). Only when not even one thread can be started does the run fail, with the
//! retryable `io_error`.
//!
//! The global rayon pool is started explicitly ([`init_global`]) with a few threads: libraries
//! (zarrs) use it for work outside cdors' own pools, such as reading coordinates while planning;
//! left to itself, rayon would start one thread per core of the node on first use and panic if
//! that fails.

use crate::error::{Error, ErrorCode, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static JSON: AtomicBool = AtomicBool::new(false);
static WARNED: AtomicBool = AtomicBool::new(false);
static NO_WARNINGS: AtomicBool = AtomicBool::new(false);

/// Pauses before retrying with a single thread: the limit is shared with other processes,
/// which may end in the meantime.
const PAUSES_MS: [u64; 3] = [50, 200, 800];

/// Renders warnings as JSON (`--json`).
pub fn set_json(json: bool) {
    JSON.store(json, Ordering::Relaxed);
}

/// Drops all warnings (`-w`, cdo's `--disable_warnings`).
pub fn set_no_warnings(off: bool) {
    NO_WARNINGS.store(off, Ordering::Relaxed);
}

/// Prints a warning on stderr: `cdors: warning: ...`, or one line of JSON under `--json`;
/// nothing under `-w`.
pub fn warn(kind: &str, message: &str) {
    if NO_WARNINGS.load(Ordering::Relaxed) {
        return;
    }
    if JSON.load(Ordering::Relaxed) {
        let v = serde_json::json!({"warning": kind, "message": message});
        eprintln!("{v}");
    } else {
        eprintln!("cdors: warning: {message}");
    }
}

/// A warning that must not be missed (results that are probably wrong, such as field means
/// with equal weights): framed and in capitals on stderr, one line of JSON under `--json` (as
/// [`warn`]); nothing under `-w`.
pub fn warn_loud(kind: &str, message: &str) {
    if NO_WARNINGS.load(Ordering::Relaxed) {
        return;
    }
    if JSON.load(Ordering::Relaxed) {
        warn(kind, message);
        return;
    }
    const WIDTH: usize = 84;
    let rule = format!("cdors: WARNING {}", "*".repeat(WIDTH));
    let mut out = vec![rule.clone()];
    let mut line = String::new();
    for word in message.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > WIDTH {
            out.push(format!("cdors: WARNING {line}"));
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(format!("cdors: WARNING {line}"));
    }
    out.push(rule);
    eprintln!("{}", out.join("\n"));
}

/// The one warning per process about threads the system refused.
fn warn_once(message: &str) {
    if !WARNED.swap(true, Ordering::Relaxed) {
        warn("threads_reduced", message);
    }
}

fn warn_reduced(what: &str, wanted: usize, got: usize, err: &str) {
    warn_once(&format!(
        "the system refused threads ({err}); running {what} with {got} instead of {wanted} \
         thread(s) (per-user limit: ulimit -u)"
    ));
}

fn no_threads(what: &str, err: &str) -> Error {
    Error::new(
        ErrorCode::IoError,
        format!("cannot start a thread for {what}: {err}"),
    )
    .with_hint("too many processes or threads for this user (ulimit -u); retry later, or lower -P and --io-threads")
}

/// Calls `build(k)` for `k = n, n/2, ..., 1`, then retries `k = 1` after short pauses, until it
/// succeeds. Warns once if fewer than `n` threads were started.
pub(crate) fn with_fallback<T, E: std::fmt::Display>(
    what: &str,
    n: usize,
    mut build: impl FnMut(usize) -> std::result::Result<T, E>,
) -> Result<T> {
    let n = n.max(1);
    let mut k = n;
    let mut pauses = PAUSES_MS.iter();
    let mut first_err: Option<String> = None;
    loop {
        match build(k) {
            Ok(t) => {
                if let Some(err) = first_err.filter(|_| k < n) {
                    warn_reduced(what, n, k, &err);
                }
                return Ok(t);
            }
            Err(e) => {
                let err = e.to_string();
                if k > 1 {
                    k /= 2;
                } else if let Some(&ms) = pauses.next() {
                    std::thread::sleep(Duration::from_millis(ms));
                } else {
                    return Err(no_threads(what, &err));
                }
                first_err.get_or_insert(err);
            }
        }
    }
}

/// A rayon pool of up to `n` threads named `cdors-<name>-<i>`.
pub fn build_pool(name: &'static str, n: usize) -> Result<rayon::ThreadPool> {
    with_fallback(&format!("the {name} pool"), n, |k| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(k)
            .thread_name(move |i| format!("cdors-{name}-{i}"))
            .build()
    })
}

/// [`build_pool`] for a pool the caller can do without: `None` (with the one warning, which
/// says what happens `instead`) if not even one thread can be started.
pub fn build_optional_pool(
    name: &'static str,
    n: usize,
    instead: &str,
) -> Option<rayon::ThreadPool> {
    build_pool(name, n)
        .map_err(|e| warn_once(&format!("{}; {instead}", e.message)))
        .ok()
}

/// Starts the global rayon pool with up to `n` threads (once per process).
pub fn init_global(n: usize) -> Result<()> {
    let mut already = false;
    let r = with_fallback(
        "the global pool",
        n,
        |k| match rayon::ThreadPoolBuilder::new()
            .num_threads(k)
            .thread_name(|i| format!("cdors-global-{i}"))
            .build_global()
        {
            Err(e) if e.to_string().contains("already") => {
                already = true;
                Ok(())
            }
            r => r,
        },
    );
    if already { Ok(()) } else { r }
}

/// Spawns a scoped thread made by `make`; retries after short pauses if the system refuses it.
pub fn spawn_scoped<'scope, 'env, T, F>(
    scope: &'scope std::thread::Scope<'scope, 'env>,
    name: String,
    make: impl Fn() -> F,
) -> Result<std::thread::ScopedJoinHandle<'scope, T>>
where
    F: FnOnce() -> T + Send + 'scope,
    T: Send + 'scope,
{
    let mut pauses = PAUSES_MS.iter();
    loop {
        match std::thread::Builder::new()
            .name(name.clone())
            .spawn_scoped(scope, make())
        {
            Ok(h) => return Ok(h),
            Err(e) => match pauses.next() {
                Some(&ms) => std::thread::sleep(Duration::from_millis(ms)),
                None => return Err(no_threads(&name, &e.to_string())),
            },
        }
    }
}

/// Spawns scoped threads `from..n` (`make(i)` gives the body of thread `i`) while the system
/// allows; returns how many started. A refusal ends the loop with one warning.
pub fn spawn_more<'scope, 'env, F>(
    scope: &'scope std::thread::Scope<'scope, 'env>,
    what: &str,
    from: usize,
    n: usize,
    make: impl Fn(usize) -> F,
) -> usize
where
    F: FnOnce() + Send + 'scope,
{
    for i in from..n {
        if let Err(e) = std::thread::Builder::new()
            .name(format!("cdors-{what}-{i}"))
            .spawn_scoped(scope, make(i))
        {
            warn_reduced(&format!("the {what} threads"), n, i, &e.to_string());
            return i - from;
        }
    }
    n.saturating_sub(from)
}
