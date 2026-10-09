//! The output's life cycle: refusing outputs that would overwrite an input or an existing file,
//! writing under a temporary name, publishing without replacing anything, and removing only
//! what this process created.
//!
//! - The temporary output is `.<out>.cdors-tmp-<host>-<pid>-<random>` in the output's
//!   directory, so no two runs (on any host) share a name. It is registered as this process's
//!   own only after its exclusive creation succeeded (`create_dir` for Zarr, `NOCLOBBER` for
//!   NetCDF); cleanup removes registered paths only, and only those exact paths.
//! - Without `-O`, a file is published with `link()` + `unlink()` of the temporary name (falling
//!   back to `renameat2(RENAME_NOREPLACE)` where the file system has no hard links), so an
//!   output that appeared in the meantime is never replaced. A Zarr directory reserves the output
//!   name with an exclusive `mkdir` before writing and is published by renaming the temporary
//!   directory over the still-empty reserved one (`rename` refuses a non-empty target).
//! - With `-O`, an existing file is replaced atomically by `rename`; an existing directory is
//!   always refused.
//! - Temporary outputs of killed runs keep their names and are never removed by other runs.
//!
//! A panic anywhere in the process goes through [`install_panic_hook`]: it reports `internal`,
//! removes this process's own temporary output and exits with code 2.

use crate::error::{Error, ErrorCode, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// What this process created and may remove again.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Own {
    /// A temporary file (NetCDF).
    File(PathBuf),
    /// A temporary directory (Zarr store).
    Dir(PathBuf),
    /// The empty directory that reserves a Zarr output's name.
    Reserved(PathBuf),
}

static OWN: Mutex<Vec<Own>> = Mutex::new(Vec::new());
static CANCELLED: AtomicBool = AtomicBool::new(false);

/// Set when the process is going down after a panic: writers stop creating files.
pub fn cancelled() -> bool {
    CANCELLED.load(Ordering::Relaxed)
}

fn own() -> std::sync::MutexGuard<'static, Vec<Own>> {
    OWN.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Records that this process created `path` exclusively (a temporary file or directory).
pub fn mark_created(path: &Path, dir: bool) {
    let p = path.to_path_buf();
    own().push(if dir { Own::Dir(p) } else { Own::File(p) });
}

fn forget(path: &Path) {
    own().retain(|o| match o {
        Own::File(p) | Own::Dir(p) | Own::Reserved(p) => p != path,
    });
}

fn remove(o: &Own) -> bool {
    match o {
        Own::File(p) => std::fs::remove_file(p).is_ok(),
        Own::Dir(p) => {
            // chunk writers still running may add entries while the tree is removed
            (0..3).any(|i| {
                if i > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                std::fs::remove_dir_all(p).is_ok()
            })
        }
        // only if still empty: never what someone else put there
        Own::Reserved(p) => std::fs::remove_dir(p).is_ok(),
    }
}

/// Removes this process's own temporary output (and its reservation of the output name).
/// Returns whether a temporary output existed and was removed.
pub fn remove_own(tmp: &Path, out: &Path) -> bool {
    let mut g = own();
    let mut removed = false;
    g.retain(|o| match o {
        Own::File(p) | Own::Dir(p) if p == tmp => {
            removed = remove(o);
            false
        }
        Own::Reserved(p) if p == out => {
            remove(o);
            false
        }
        _ => true,
    });
    removed
}

/// Removes everything this process created (panic path). Never blocks on the registry.
fn remove_all_own() -> bool {
    let Ok(mut g) = OWN.try_lock() else {
        return false;
    };
    let mut removed = false;
    for o in g.drain(..) {
        let ok = remove(&o);
        if !matches!(o, Own::Reserved(_)) {
            removed |= ok;
        }
    }
    removed
}

fn hostname() -> String {
    let h = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_default();
    let h: String = h
        .trim()
        .split('.')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    if h.is_empty() { "host".into() } else { h }
}

fn random_u32() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.finish() as u32
}

/// The temporary name of `out`: `.<name>.cdors-tmp-<host>-<pid>-<random>` in its directory.
pub fn temp_path(out: &Path) -> PathBuf {
    let dir = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = out
        .file_name()
        .map_or_else(|| "out".into(), |s| s.to_string_lossy().into_owned());
    dir.join(format!(
        ".{name}.cdors-tmp-{}-{}-{:08x}",
        hostname(),
        std::process::id(),
        random_u32()
    ))
}

fn exists_error(out: &str) -> Error {
    Error::new(ErrorCode::OutputExists, format!("output '{out}' exists"))
        .with("path", out.to_owned())
        .with_hint("add -O to overwrite it, or choose another output name")
}

fn dir_error(out: &str) -> Error {
    Error::new(
        ErrorCode::OutputExists,
        format!("output '{out}' is an existing directory"),
    )
    .with("path", out.to_owned())
    .with_hint("-O replaces files only; remove the existing store first or choose another name")
}

/// Refuses an existing output (any kind of entry, also a dangling symlink) without `-O`, and an
/// existing directory with `-O`. Early check only; publishing is atomic on its own.
pub fn check_output(out: &Path, overwrite: bool) -> Result<()> {
    let name = out.to_string_lossy().into_owned();
    match std::fs::symlink_metadata(out) {
        Ok(_) if !overwrite => Err(exists_error(&name)),
        Ok(m) if m.is_dir() || out.is_dir() => Err(dir_error(&name)),
        _ => Ok(()),
    }
}

/// Reserves the name of a Zarr output with an exclusive `mkdir` (refused if anything exists).
pub fn reserve_dir(out: &Path, overwrite: bool) -> Result<()> {
    let name = out.to_string_lossy().into_owned();
    match std::fs::create_dir(out) {
        Ok(()) => {
            own().push(Own::Reserved(out.to_path_buf()));
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(if overwrite && !out.is_dir() {
                exists_error(&name)
                    .with_hint("-O replaces a file by a file only; a Zarr output needs a new name")
            } else if overwrite {
                dir_error(&name)
            } else {
                exists_error(&name)
            })
        }
        Err(e) => {
            Err(Error::from_io(&e, format!("cannot create '{name}': {e}")).with("path", name))
        }
    }
}

/// `renameat2(RENAME_NOREPLACE)`: Ok(true) done, Ok(false) not supported here.
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    let c =
        |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(std::io::Error::other);
    let (f, t) = (c(from)?, c(to)?);
    // SAFETY: plain syscall on two NUL-terminated paths
    let r = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            f.as_ptr(),
            libc::AT_FDCWD,
            t.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP) => Ok(false),
        _ => Err(e),
    }
}

/// Moves the finished temporary output `tmp` to `out` without replacing anything (with `-O`:
/// replacing an existing file).
pub fn publish(tmp: &Path, out: &Path, dir: bool, overwrite: bool) -> Result<()> {
    let name = out.to_string_lossy().into_owned();
    let io = |e: std::io::Error| Error::from(e).with("path", name.clone());
    if dir {
        // over the empty directory reserved by `reserve_dir`; refused if it is not empty
        return match std::fs::rename(tmp, out) {
            Ok(()) => {
                forget(tmp);
                forget(out);
                Ok(())
            }
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTEMPTY | libc::EEXIST)) => {
                Err(exists_error(&name).with_hint(
                    "something was written into the output directory while cdors ran; choose another name",
                ))
            }
            Err(e) => Err(io(e)),
        };
    }
    if overwrite {
        if std::fs::symlink_metadata(out).is_ok_and(|m| m.is_dir()) {
            return Err(dir_error(&name));
        }
        std::fs::rename(tmp, out).map_err(io)?;
        forget(tmp);
        return Ok(());
    }
    match std::fs::hard_link(tmp, out) {
        Ok(()) => {
            forget(tmp);
            if let Err(e) = std::fs::remove_file(tmp) {
                crate::exec::threads::warn(
                    "temporary_output_left",
                    &format!(
                        "output written, but its temporary name '{}' could not be removed: {e}",
                        tmp.display()
                    ),
                );
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(exists_error(&name)),
        Err(link_err) => match rename_noreplace(tmp, out) {
            Ok(true) => {
                forget(tmp);
                Ok(())
            }
            Ok(false) => Err(Error::new(
                ErrorCode::BadArguments,
                format!(
                    "cannot publish '{name}' without risking to replace a file: the file system \
                     supports neither hard links ({link_err}) nor no-replace renames"
                ),
            )
            .with("path", name.clone())
            .with_hint("write the output to another file system, or add -O")),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(exists_error(&name)),
            Err(e) => Err(io(e)),
        },
    }
}

/// The output path with symlinks resolved (for a new output: its resolved directory).
fn canonical_out(out: &Path) -> Option<PathBuf> {
    if let Ok(p) = std::fs::canonicalize(out) {
        return Some(p);
    }
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Some(std::fs::canonicalize(parent).ok()?.join(out.file_name()?))
}

fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Refuses an output that is an input, lies inside an input (a Zarr store), or is matched by an
/// input glob pattern. `inputs` are the input paths and the operator arguments (weight files,
/// grid files); paths that do not exist are skipped. Symlinks are resolved.
pub fn check_not_input(out: &Path, inputs: &[&str]) -> Result<()> {
    let name = out.to_string_lossy().into_owned();
    let refuse = |input: &str| {
        Err(
            Error::bad_arguments(format!("output '{name}' is also an input ('{input}')"))
                .with("path", name.clone())
                .with("input", input.to_owned())
                .with_hint("write the output to a new file; cdors never writes into its inputs"),
        )
    };
    let Some(cout) = canonical_out(out) else {
        return Ok(());
    };
    let aout = absolute(out);
    for &inp in inputs {
        if crate::io::remote::is_url(inp) {
            continue;
        }
        if crate::io::multifile::is_glob(inp) && !Path::new(inp).exists() {
            let pat = absolute(Path::new(inp));
            if let Ok(p) = glob::Pattern::new(&pat.to_string_lossy())
                && (p.matches_path(&aout) || p.matches_path(&cout))
            {
                return refuse(inp);
            }
            for m in glob::glob(inp).into_iter().flatten().flatten() {
                if std::fs::canonicalize(&m).is_ok_and(|c| cout.starts_with(&c)) {
                    return refuse(inp);
                }
            }
            continue;
        }
        if let Ok(c) = std::fs::canonicalize(inp)
            && cout.starts_with(&c)
        {
            return refuse(inp);
        }
    }
    Ok(())
}

fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let p = info.payload();
    let msg = p
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into());
    match info.location() {
        Some(l) => format!("{msg} (at {}:{})", l.file(), l.line()),
        None => msg,
    }
}

/// Makes every panic (any thread, any pool) end the process the same way: `internal` (one line
/// of JSON under `--json`), this process's own temporary output removed, exit code 2.
pub fn install_panic_hook(json: bool) {
    std::panic::set_hook(Box::new(move |info| {
        // the first panicking thread reports and exits; others (several pool threads may panic
        // at once) wait for that instead of exiting halfway through the cleanup
        if CANCELLED.swap(true, Ordering::SeqCst) {
            loop {
                std::thread::park();
            }
        }
        let removed = remove_all_own();
        let (stage, done, total, _) = super::progress::snapshot();
        let e = Error::internal(format!(
            "cdors failed with an internal error: {}",
            panic_message(info)
        ))
        .with("stage", stage)
        .with("chunks_done", done)
        .with("chunks_total", total)
        .with("temporary_output_removed", removed)
        .with_hint("this is a bug in cdors; please report the command line");
        if json {
            eprintln!("{}", e.to_json());
        } else {
            eprintln!("{}", e.to_text());
        }
        // no atexit handlers: other threads may still be inside netCDF-C/HDF5
        // SAFETY: terminates the process
        unsafe { libc::_exit(e.exit_code()) };
    }));
}
