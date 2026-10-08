//! cdors: CDO-style climate statistics on Zarr and NetCDF.

mod ops_cmd;
mod parse;

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
  --io-threads <n>    blocking reads in flight (default: 64 in Slurm jobs, 32 otherwise)
  -f <fmt>            output format: nc4, nc4c, nc, zarr, zarr2
  -b <F32|F64>        output precision
  -s                  silent
  --json              machine-readable output and errors (one JSON object on stderr on failure)
  --plan              print what will be read (chunks, bytes, memory, weights) and stop;
                      the output file may be left out; with --json as JSON
  --mem <size>        memory budget for tiles in flight (e.g. 32G; default 2G)
  --max-read <size>   refuse runs that decode more than this (default 64G on login nodes,
                      no limit inside Slurm jobs; `none` for no limit)
  --chunks <spec>     output chunks, dim=n[,dim=n...]
  --timestat_date <first|middle|midhigh|last>
  --percentile <method>
  --no_history        do not write the history attribute
  --progress json     progress lines on stderr about once per second, and a summary line

exit codes: 0 success, 1 usage (unknown operator, bad arguments, not implemented),
            2 data (coordinates, grid, dimension), 3 I/O (worth retrying),
            4 refused (--max-read limit, existing output without -O)";

fn write_stdout(s: &str) -> Result<()> {
    let mut out = std::io::stdout().lock();
    match out.write_all(s.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(Error::io(format!("writing to stdout: {e}"))),
    }
}

fn run(cmd: &Command) -> Result<()> {
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

fn main() {
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
