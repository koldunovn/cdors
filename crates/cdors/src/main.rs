//! cdors: CDO-style climate statistics on Zarr and NetCDF.

mod parse;

use cdors_core::chain::{Command, Input};
use cdors_core::error::{Error, Result};
use cdors_core::ops::{self, AccessClass};
use std::io::Write;

const USAGE: &str =
    "usage: cdors [options] operator[,args] [-operator2[,args] ...] inputs... [output]

options:
  -O                  overwrite existing outputs
  -P <n>              number of threads
  -f <fmt>            output format: nc4, nc4c, nc, zarr, zarr2
  -b <F32|F64>        output precision
  -s                  silent
  --json              machine-readable output and errors
  --plan              print the plan (what will be read) and stop
  --mem <size>        memory budget (e.g. 32G)
  --max-read <size>   refuse runs that read more than this
  --chunks <spec>     output chunks, dim=n[,dim=n...]
  --timestat_date <first|middle|midhigh|last>
  --percentile <method>
  --no_history        do not write the history attribute
  --progress json     progress on stderr

information operators: sinfo, showname, showtimestamp, griddes";

fn write_stdout(s: &str) -> Result<()> {
    let mut out = std::io::stdout().lock();
    match out.write_all(s.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(Error::io(format!("writing to stdout: {e}"))),
    }
}

fn run(cmd: &Command) -> Result<()> {
    if cmd.options.plan {
        // The planner comes later; for now --plan shows the parsed operator tree.
        let v = serde_json::json!({"plan": null, "note": "planner not implemented yet", "command": cmd});
        return write_stdout(&format!("{v}\n"));
    }
    let spec = ops::lookup(&cmd.root.name).expect("parsed operator exists");
    if spec.class == AccessClass::Info {
        let path = match &cmd.root.inputs[0] {
            Input::Path(p) => p,
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
        let src = cdors_core::io::open(path)?;
        let text = ops::info::run(&cmd.root.name, src.as_ref(), &cmd.options)?;
        return write_stdout(&text);
    }
    ops::require_implemented(&cmd.root)?;
    Err(Error::internal("no executor yet"))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("cdors {}", cdors_core::VERSION);
            println!("{}", cdors_core::native_library_versions());
            return;
        }
        Some("-h" | "--help" | "help") => {
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
    let result = parse::parse(&args).and_then(|cmd| run(&cmd));
    if let Err(e) = result {
        if json {
            eprintln!("{}", e.to_json());
        } else {
            eprintln!("{}", e.to_text());
        }
        std::process::exit(e.exit_code());
    }
}
