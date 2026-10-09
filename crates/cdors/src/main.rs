//! cdors: CDO-style climate statistics on Zarr and NetCDF.

mod ops_cmd;
mod parse;
mod sigpipe;

use cdors_core::chain::{Command, Input};
use cdors_core::error::{Error, Result};
use cdors_core::ops::{self, AccessClass};
use std::io::Write;

const USAGE: &str =
    "usage: cdors [options] operator[,args] [-operator2[,args] ...] inputs... [output]
       cdors ops [--json]          list operators (implemented ones with arguments, all of cdo's)
       cdors help <operator>       cdo's help text plus cdors notes (also: cdors -h <operator>)

options:
  -O                  overwrite existing outputs
  -P <n>              number of compute threads (default: all cores, at most 16 outside Slurm)
  --io-threads <n>    blocking reads in flight (default: 128 in Slurm jobs, 64 on login
                      nodes and for URLs)
  -f <fmt>            output format: nc4, nc4c (written as nc4), nc (64-bit offset), zarr,
                      zarr2 (default: zarr for a name ending in .zarr, else nc4)
  -b <F32|F64>        output precision
  -s                  as cdo -s for showtimestamp (one line); as in cdo, warnings are
                      still printed
  -w                  no warnings (cdo's --disable_warnings), also no JSON warning lines
  --json              machine-readable output and errors (one JSON object on stderr on failure)
  --plan              print what will be read (chunks, bytes, memory, weights) and stop;
                      the output file may be left out; with --json as JSON
  --mem <size>        memory budget (e.g. 32G; default 60% of the Slurm allocation,
                      on login nodes 1/4 of available memory, at most 4G)
  --max-read <size>   refuse runs that decode more than this (default 64G on login nodes,
                      no limit inside Slurm jobs; `none` for no limit)
  --max-values <n>    refuse to print more values (info, output*: default 1000000; `none`)
  --chunks <spec>     output chunks, dim=n[,dim=n...]
  --timestat_date <first|middle|midhigh|last>
  --percentile <method>
  --no_history        do not write the history attribute
  --progress json     progress lines on stderr about once per second, and a summary line
  -v, -L, --force     accepted for cdo compatibility; no effect (cdo's other options, such
                      as -z, -k, -r, are refused)

exit codes: 0 success
            1 usage: unknown operator, not implemented, bad arguments, missing input,
              permission denied, no space, cdo not found (needed for remapping weights)
            2 data: bad data, coordinates, grid, dimension, memory limit, intermediate
              too large, I/O failure not known to be transient, internal error
            3 I/O error worth retrying (timeouts, connection errors, HTTP 5xx/429)
            4 refused: --max-read or --max-values limit, existing output without -O";

fn write_stdout(s: &str) -> Result<()> {
    let mut out = std::io::stdout().lock();
    match out.write_all(s.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(Error::io(format!("writing to stdout: {e}"))),
    }
}

fn run(cmd: &Command) -> Result<()> {
    if ops::output::handles(&cmd.root.name) {
        return write_stdout(&ops::output::run(cmd)?);
    }
    let spec = ops::lookup(&cmd.root.name).expect("parsed operator exists");
    if spec.class == AccessClass::Info {
        let src = match &cmd.root.inputs[0] {
            Input::Path(p) => cdors_core::io::open(p)?,
            Input::Op(o) if ops::files::is_concat(&o.name) => {
                ops::require_implemented(o)?;
                ops::files::open_concat(o)?
            }
            Input::Op(o) => {
                return Err(Error::new(
                    cdors_core::error::ErrorCode::NotImplemented,
                    format!(
                        "information operators on the output of another operator (-{}) are not implemented yet",
                        o.name
                    ),
                )
                .with_hint("run the information operator on a file or store"));
            }
        };
        let text = ops::info::run(&cmd.root.name, src.as_ref(), &cmd.options)?;
        return write_stdout(&text);
    }
    ops::require_implemented(&cmd.root)?;
    let text = cdors_core::exec::run(cmd)?;
    write_stdout(&text)
}

/// glibc malloc tuning, before any thread starts. Chunks and tiles are a few MB each and are
/// allocated and freed thousands of times per second from many threads. By default glibc serves
/// such blocks with mmap/munmap (or trims the heap after each free), so every chunk costs fresh
/// zeroed pages: millions of minor page faults and more system than user time. Raising the mmap
/// threshold to its maximum (32 MiB on 64-bit) and not trimming keeps freed blocks in the
/// arenas for reuse. Peak memory is still bounded by the tile window; memory freed after the
/// peak is not handed back to the system before the process ends. Since a freed block is reused
/// only by threads of the same arena, the default of one arena per thread (8 per core) lets
/// every one of the ~80 compute and I/O threads keep its own high-water mark: a run planned for
/// `--mem 400M` reached 1.3 GB RSS. Four arenas keep the RSS near the budget at the same speed.
/// Environment settings (`MALLOC_MMAP_THRESHOLD_`, `MALLOC_TRIM_THRESHOLD_`,
/// `MALLOC_ARENA_MAX`) take precedence.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn tune_malloc() {
    let set = |var: &str| std::env::var_os(var).is_some();
    // SAFETY: mallopt only changes allocator parameters; called on the main thread before
    // any other thread exists.
    unsafe {
        if !set("MALLOC_MMAP_THRESHOLD_") {
            libc::mallopt(libc::M_MMAP_THRESHOLD, 32 << 20);
        }
        if !set("MALLOC_TRIM_THRESHOLD_") {
            libc::mallopt(libc::M_TRIM_THRESHOLD, i32::MAX);
        }
        if !set("MALLOC_ARENA_MAX") {
            libc::mallopt(libc::M_ARENA_MAX, 4);
        }
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn tune_malloc() {}

fn main() {
    sigpipe::reset();
    tune_malloc();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("cdors {}", cdors_core::VERSION);
            println!("{}", cdors_core::native_library_versions());
            return;
        }
        Some("--help") => {
            println!("{USAGE}");
            return;
        }
        None => {
            eprintln!("{USAGE}");
            std::process::exit(1);
        }
        _ => {}
    }
    // Errors raised while parsing need to know about --json too.
    let json = args.iter().any(|a| a == "--json");
    cdors_core::exec::threads::set_json(json);
    // every panic: `internal`, own temporary output removed, exit code 2
    cdors_core::exec::publish::install_panic_hook(json);
    // subcommands `ops`, `help <op>`, `-h <op>`: the first token after the global options
    if let Some((sub, rest)) = parse::subcommand(&args) {
        let r = match sub {
            "ops" => Ok(ops_cmd::ops(json)),
            _ => match rest.iter().find(|a| !a.starts_with("--")) {
                Some(op) => ops_cmd::help(op.trim_start_matches('-'), json),
                None => Ok(format!("{USAGE}\n")),
            },
        };
        finish(r.and_then(|text| write_stdout(&text)), json);
        return;
    }
    finish(
        parse::parse(&args).and_then(|cmd| {
            cdors_core::exec::threads::set_no_warnings(cmd.options.no_warnings);
            // a small global pool for library work outside cdors' own pools (see exec::threads)
            cdors_core::exec::threads::init_global(cdors_core::exec::default_threads(&cmd).min(4))?;
            run(&cmd)
        }),
        json,
    );
}

/// Prints a failure (one JSON line under --json) and exits with its code.
fn finish(result: Result<()>, json: bool) {
    if let Err(e) = result {
        if json {
            eprintln!("{}", e.to_json());
        } else {
            eprintln!("{}", e.to_text());
        }
        std::process::exit(e.exit_code());
    }
}
