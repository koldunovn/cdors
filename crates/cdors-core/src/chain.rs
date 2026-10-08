//! The parsed command: global options and the operator tree (what the planner consumes).
//!
//! `cdors [options] op1[,args] [-op2[,args] ...] inputs... [outputs...]` parses into a
//! [`Command`]: the root [`OpNode`], whose inputs are nested operators or paths, and the output
//! paths of the root operator.

use serde::Serialize;

/// One operator application.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpNode {
    /// Canonical operator name (aliases resolved).
    pub name: String,
    /// Raw comma-separated arguments (`-selname,tas,pr` gives `["tas", "pr"]`).
    pub args: Vec<String>,
    pub inputs: Vec<Input>,
}

/// An operator input: a nested operator or a path/URL.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Input {
    Op(OpNode),
    Path(String),
}

impl OpNode {
    /// All input paths of the tree, depth first.
    pub fn paths(&self) -> Vec<&str> {
        let mut out = Vec::new();
        for i in &self.inputs {
            match i {
                Input::Op(o) => out.extend(o.paths()),
                Input::Path(p) => out.push(p.as_str()),
            }
        }
        out
    }
}

/// Output file format (`-f`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutFormat {
    Nc,
    Nc4,
    Nc4c,
    Zarr,
    Zarr2,
}

/// Output precision (`-b`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Precision {
    F32,
    F64,
}

/// Timestamp of time-statistics output (`--timestat_date`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TimestatDate {
    First,
    #[default]
    Middle,
    Midhigh,
    Last,
}

/// Global options.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Options {
    /// `-O`: overwrite existing outputs.
    pub overwrite: bool,
    /// `-P <n>`: thread cap.
    pub threads: Option<usize>,
    /// `--io-threads <n>`: blocking reads in flight (separate from `-P`).
    pub io_threads: Option<usize>,
    /// `-f <fmt>`.
    pub format: Option<OutFormat>,
    /// `-b <F32|F64>`.
    pub precision: Option<Precision>,
    /// `-s`: silent (also changes line breaks of `show*` output, as in cdo).
    pub silent: bool,
    /// `-v`.
    pub verbose: bool,
    /// `--json`: machine-readable output and errors.
    pub json: bool,
    /// `--plan`: print the plan and stop.
    pub plan: bool,
    /// `--mem <size>` in bytes.
    pub mem: Option<u64>,
    /// `--max-read <size>` in bytes.
    pub max_read: Option<u64>,
    /// `--chunks <spec>`: output chunking, `dim=n[,dim=n...]`.
    pub chunks: Option<Vec<(String, usize)>>,
    /// `--timestat_date` (None: the environment, then the operator's default).
    pub timestat_date: Option<TimestatDate>,
    /// `--percentile <method>` (cdo's names).
    pub percentile: Option<String>,
    /// `--no_history`.
    pub no_history: bool,
    /// `--progress json`.
    pub progress_json: bool,
}

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Command {
    pub options: Options,
    pub root: OpNode,
    pub outputs: Vec<String>,
}
