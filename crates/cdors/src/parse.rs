//! CDO's command grammar.
//!
//! `cdors [options] op1[,args] [-op2[,args] ...] inputs... [outputs...]`
//!
//! - Global options come first. Anything else starting with `-` (and the first token without a
//!   dash) is an operator; the leading dash of the first operator is optional, as in cdo.
//! - Operators have a fixed number of inputs and outputs from the registry. Each input is either
//!   a nested operator (a token starting with `-`) or a path/URL. Variadic operators
//!   (`mergetime`, `cat`, `copy`) take all remaining inputs except the outputs of the outermost
//!   operator.
//! - Information operators have no output and print to stdout; they cannot be nested.
//! - After the outermost operator and its inputs, exactly its number of outputs must remain.

use cdors_core::chain::{Command, Input, OpNode, Options, OutFormat, Precision, TimestatDate};
use cdors_core::error::{Error, Result};
use cdors_core::ops::{self, Inputs};

const OPTIONS: &[&str] = &[
    "-O",
    "-P",
    "-f",
    "-b",
    "-s",
    "-v",
    "-L",
    "-w",
    "--json",
    "--plan",
    "--mem",
    "--max-read",
    "--chunks",
    "--timestat_date",
    "--percentile",
    "--no_history",
    "--progress",
    "--io-threads",
    "--force",
];

const PERCENTILE_METHODS: &[&str] = &[
    "nrank",
    "nist",
    "rtype8",
    "numpy",
    "linear",
    "numpy_linear",
    "lower",
    "numpy_lower",
    "higher",
    "numpy_higher",
    "nearest",
    "numpy_nearest",
    "midpoint",
    "inverted_cdf",
    "averaged_inverted_cdf",
    "closest_observation",
    "interpolated_inverted_cdf",
    "hazen",
    "weibull",
    "median_unbiased",
    "normal_unbiased",
];

/// Parses a size: a number with an optional suffix k, M, G, T, P (powers of 1000; with `i`,
/// as in `Gi`, powers of 1024) and an optional `B`.
pub fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim();
    let t = t.strip_suffix(['B', 'b']).unwrap_or(t);
    let (t, binary) = match t.strip_suffix('i') {
        Some(r) => (r, true),
        None => (t, false),
    };
    let (num, exp) = match t.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => {
            let e = match c.to_ascii_uppercase() {
                'K' => 1,
                'M' => 2,
                'G' => 3,
                'T' => 4,
                'P' => 5,
                _ => return Err(bad_size(s)),
            };
            (&t[..i], e)
        }
        _ => (t, 0),
    };
    let v: f64 = num.trim().parse().map_err(|_| bad_size(s))?;
    if !(v >= 0.0 && v.is_finite()) {
        return Err(bad_size(s));
    }
    let base: f64 = if binary { 1024.0 } else { 1000.0 };
    Ok((v * base.powi(exp)).round() as u64)
}

fn bad_size(s: &str) -> Error {
    Error::bad_arguments(format!("invalid size '{s}'"))
        .with_hint("sizes are bytes with an optional suffix: 500M, 32G, 2T, 4Gi")
}

fn parse_chunks(s: &str) -> Result<Vec<(String, usize)>> {
    s.split(',')
        .map(|kv| {
            let (k, v) = kv.split_once('=').ok_or_else(|| bad_chunks(s))?;
            let n: usize = v.trim().parse().map_err(|_| bad_chunks(s))?;
            if k.trim().is_empty() || n == 0 {
                return Err(bad_chunks(s));
            }
            Ok((k.trim().to_owned(), n))
        })
        .collect()
}

fn bad_chunks(s: &str) -> Error {
    Error::bad_arguments(format!("invalid --chunks '{s}'"))
        .with_hint("--chunks takes dim=n[,dim=n...], e.g. --chunks time=1,cell=49152")
}

fn unknown_option(tok: &str) -> Error {
    let name = tok.split('=').next().unwrap_or(tok);
    let mut best: Vec<&str> = OPTIONS
        .iter()
        .copied()
        .filter(|o| o.len() > 2 && strsim(name, o) <= 2)
        .collect();
    best.truncate(2);
    let e = Error::bad_arguments(format!("unknown option '{name}'")).with("option", name);
    if best.is_empty() {
        e.with_hint(format!("options: {}", OPTIONS.join(" ")))
    } else {
        e.with_hint(format!("did you mean {}?", best.join(" or ")))
    }
}

fn strsim(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, &cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j + 1] + 1)
                .min(cur[j] + 1)
                .min(prev[j] + usize::from(ca != cb));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Parses global options; returns them and the index of the first operator token.
fn parse_options(args: &[String]) -> Result<(Options, usize)> {
    let mut o = Options::default();
    let mut i = 0;
    while i < args.len() {
        let tok = args[i].as_str();
        let (key, inline) = match tok.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k, Some(v.to_owned())),
            _ => (tok, None),
        };
        // value of an option taking one: `--opt=v` or `--opt v`
        let take = |i: &mut usize| -> Result<String> {
            if let Some(v) = &inline {
                return Ok(v.clone());
            }
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| Error::bad_arguments(format!("option '{key}' needs a value")))
        };
        match key {
            "-O" => o.overwrite = true,
            "-s" => o.silent = true,
            "-v" => o.verbose = true,
            // --force: cdo needs it for conservative remapping on HEALPix grids; cdors always
            // passes it to `cdo gencon` and accepts it for compatibility
            "-L" | "-w" | "--force" => {}
            "--json" => o.json = true,
            "--plan" => o.plan = true,
            "--no_history" | "--no-history" => o.no_history = true,
            "-P" => {
                let v = take(&mut i)?;
                let n: usize = v.parse().ok().filter(|&n| n >= 1).ok_or_else(|| {
                    Error::bad_arguments(format!("-P needs a thread count >= 1, got '{v}'"))
                })?;
                o.threads = Some(n);
            }
            "-f" => {
                let v = take(&mut i)?;
                o.format = Some(match v.to_ascii_lowercase().as_str() {
                    "nc" => OutFormat::Nc,
                    "nc4" => OutFormat::Nc4,
                    "nc4c" => OutFormat::Nc4c,
                    "zarr" | "zarr3" => OutFormat::Zarr,
                    "zarr2" => OutFormat::Zarr2,
                    _ => {
                        return Err(Error::bad_arguments(format!(
                            "unsupported output format '{v}'"
                        ))
                        .with_hint("-f takes nc4, nc4c, nc, zarr (v3) or zarr2"));
                    }
                });
            }
            "-b" => {
                let v = take(&mut i)?;
                o.precision = Some(match v.to_ascii_uppercase().as_str() {
                    "F32" | "32" => Precision::F32,
                    "F64" | "64" => Precision::F64,
                    _ => {
                        return Err(Error::bad_arguments(format!("unsupported precision '{v}'"))
                            .with_hint("-b takes F32 or F64"));
                    }
                });
            }
            "--io-threads" | "--io_threads" => {
                let v = take(&mut i)?;
                let n: usize = v.parse().ok().filter(|&n| n >= 1).ok_or_else(|| {
                    Error::bad_arguments(format!("--io-threads needs a count >= 1, got '{v}'"))
                })?;
                o.io_threads = Some(n);
            }
            "--mem" => o.mem = Some(parse_size(&take(&mut i)?)?),
            "--max-read" | "--max_read" => o.max_read = Some(parse_size(&take(&mut i)?)?),
            "--chunks" => o.chunks = Some(parse_chunks(&take(&mut i)?)?),
            "--timestat_date" => {
                let v = take(&mut i)?;
                o.timestat_date = Some(match v.as_str() {
                    "first" => TimestatDate::First,
                    "middle" => TimestatDate::Middle,
                    "midhigh" => TimestatDate::Midhigh,
                    "last" => TimestatDate::Last,
                    _ => {
                        return Err(
                            Error::bad_arguments(format!("invalid --timestat_date '{v}'"))
                                .with_hint("--timestat_date takes first, middle, midhigh or last"),
                        );
                    }
                });
            }
            "--percentile" => {
                let v = take(&mut i)?.to_ascii_lowercase();
                if !PERCENTILE_METHODS.contains(&v.as_str()) {
                    return Err(
                        Error::bad_arguments(format!("unknown percentile method '{v}'"))
                            .with_hint(format!("methods: {}", PERCENTILE_METHODS.join(", "))),
                    );
                }
                o.percentile = Some(v);
            }
            "--progress" => {
                let v = take(&mut i)?;
                if v != "json" {
                    return Err(Error::bad_arguments(format!("invalid --progress '{v}'"))
                        .with_hint("--progress takes json"));
                }
                o.progress_json = true;
            }
            _ if key.starts_with("--") => return Err(unknown_option(tok)),
            _ => break,
        }
        i += 1;
    }
    Ok((o, i))
}

struct Parser<'a> {
    toks: &'a [String],
    pos: usize,
    /// Tokens that must remain for the outputs of the outermost operator.
    reserve: usize,
}

impl Parser<'_> {
    fn op(&mut self, root: bool) -> Result<OpNode> {
        let tok = &self.toks[self.pos];
        self.pos += 1;
        let body = tok.strip_prefix('-').unwrap_or(tok);
        let mut parts = body.split(',');
        let name = parts.next().unwrap_or("").to_owned();
        let args: Vec<String> = parts.map(str::to_owned).collect();
        if name.is_empty() {
            return Err(Error::bad_arguments(format!("empty operator in '{tok}'")));
        }
        let spec = ops::lookup(&name).ok_or_else(|| ops::unknown_operator(&name))?;
        spec.check_args(&args)?;
        if !root && spec.outputs != 1 {
            return Err(Error::bad_arguments(format!(
                "operator '{name}' prints information and cannot be the input of another operator"
            ))
            .with("operator", name.clone())
            .with_hint(format!(
                "use it as the outermost operator: cdors {name} <input>"
            )));
        }
        if root {
            self.reserve = spec.outputs;
        }
        let mut node = OpNode {
            name: spec.name.clone(),
            args,
            inputs: Vec::new(),
        };
        match spec.inputs {
            Inputs::Fixed(n) => {
                for k in 0..n {
                    if self.pos >= self.toks.len() {
                        return Err(Error::bad_arguments(format!(
                            "operator '{name}' needs {n} input(s), got {k}"
                        ))
                        .with("operator", name.clone()));
                    }
                    node.inputs.push(self.input()?);
                }
            }
            Inputs::OneOrThree => {
                if self.pos >= self.toks.len() {
                    return Err(
                        Error::bad_arguments(format!("operator '{name}' needs an input"))
                            .with("operator", name.clone()),
                    );
                }
                node.inputs.push(self.input()?);
                // cdo's `pctl,p data min max`: as the outermost operator when more inputs
                // follow; nested, when the next two inputs are `-<x>min ...` and `-<x>max ...`
                let three = if root {
                    self.toks.len() - self.pos > self.reserve
                } else {
                    let mut probe = Parser {
                        toks: self.toks,
                        pos: self.pos,
                        reserve: self.reserve,
                    };
                    let mut is_op = |suffix: &str| {
                        probe.pos < probe.toks.len()
                            && matches!(probe.input(), Ok(Input::Op(o)) if o.name.ends_with(suffix))
                    };
                    is_op("min") && is_op("max")
                };
                if three {
                    for k in 1..3 {
                        if self.pos >= self.toks.len() {
                            return Err(Error::bad_arguments(format!(
                                "operator '{name}' takes 1 or 3 inputs, got {k}"
                            ))
                            .with("operator", name.clone()));
                        }
                        node.inputs.push(self.input()?);
                    }
                }
            }
            Inputs::Variadic => {
                while self.toks.len() - self.pos > self.reserve {
                    node.inputs.push(self.input()?);
                }
                if node.inputs.is_empty() {
                    return Err(Error::bad_arguments(format!(
                        "operator '{name}' needs at least one input"
                    ))
                    .with("operator", name.clone()));
                }
            }
        }
        Ok(node)
    }

    fn input(&mut self) -> Result<Input> {
        let t = &self.toks[self.pos];
        if t.len() > 1 && t.starts_with('-') {
            Ok(Input::Op(self.op(false)?))
        } else {
            self.pos += 1;
            Ok(Input::Path(t.clone()))
        }
    }
}

/// Parses a full command line (without the program name).
///
/// Long options (`--json`, `--mem 4G`, ...) are accepted anywhere on the line, since no operator
/// starts with `--`; short options (`-O`, `-P 4`, ...) must come before the first operator, as in
/// cdo.
pub fn parse(args: &[String]) -> Result<Command> {
    const LONG_WITH_VALUE: &[&str] = &[
        "--mem",
        "--max-read",
        "--max_read",
        "--chunks",
        "--timestat_date",
        "--percentile",
        "--progress",
        "--io-threads",
        "--io_threads",
    ];
    let mut long = Vec::new();
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a.starts_with("--") {
            long.push(a.clone());
            if LONG_WITH_VALUE.contains(&a.as_str())
                && let Some(v) = it.next()
            {
                long.push(v.clone());
            }
        } else {
            rest.push(a.clone());
        }
    }
    let nlong = long.len();
    long.extend(rest);
    let args = long;
    let (options, start) = parse_options(&args)?;
    debug_assert!(start >= nlong);
    let toks = &args[start..];
    if toks.is_empty() {
        return Err(Error::bad_arguments("no operator given").with_hint(
            "usage: cdors [options] operator[,args] [-operator2 ...] inputs... [output]",
        ));
    }
    let mut p = Parser {
        toks,
        pos: 0,
        reserve: 0,
    };
    let root = p.op(true)?;
    let spec = ops::lookup(&root.name).expect("parsed operator exists");
    let rest = &toks[p.pos..];
    if rest.len() < spec.outputs {
        return Err(Error::bad_arguments(format!(
            "operator '{}' needs {} output file(s), got {}",
            root.name,
            spec.outputs,
            rest.len()
        ))
        .with("operator", root.name.clone())
        .with_hint("the output file comes last: cdors -op in.nc out.nc"));
    }
    if rest.len() > spec.outputs {
        let extra = &rest[..rest.len() - spec.outputs];
        return Err(Error::bad_arguments(format!(
            "too many arguments: {} not used by the operator chain",
            extra.join(" ")
        ))
        .with("unused", extra.to_vec())
        .with_hint(if spec.outputs == 0 {
            format!("'{}' prints to stdout and takes no output file", root.name)
        } else {
            "check the number of inputs of each operator (cdors ops)".to_owned()
        }));
    }
    Ok(Command {
        options,
        root,
        outputs: rest.to_vec(),
    })
}
