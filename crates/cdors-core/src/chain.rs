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

/// Compressor of written chunks (`-z`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    /// Deflate (zlib).
    Zip,
    Zstd,
}

/// Output compression (`-z zip[_level]`, `-z zstd[_level]`). Written chunks are always
/// byte-shuffled before they are compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Compression {
    pub codec: Codec,
    pub level: u8,
}

impl Compression {
    /// What Zarr outputs get without `-z`.
    pub const ZARR_DEFAULT: Self = Self {
        codec: Codec::Zstd,
        level: 1,
    };
}

impl std::fmt::Display for Compression {
    /// As `-z` takes it: `zip_1`, `zstd_3`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self.codec {
            Codec::Zip => "zip",
            Codec::Zstd => "zstd",
        };
        write!(f, "{name}_{}", self.level)
    }
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
    /// `-z <zip|zstd>[_level]` (None: NetCDF uncompressed, Zarr [`Compression::ZARR_DEFAULT`]).
    pub compression: Option<Compression>,
    /// `-s`: silent (also changes line breaks of `show*` output, as in cdo). As in cdo, warnings
    /// are still printed.
    pub silent: bool,
    /// `-w` (`--disable_warnings`): no warnings, as in cdo (also no JSON warning lines).
    pub no_warnings: bool,
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
    /// `--max-values <n>`: values a printing operator may print (`u64::MAX`: no limit).
    pub max_values: Option<u64>,
    /// `--chunks <spec>`: output chunking, `dim=n[,dim=n...]`.
    pub chunks: Option<Vec<(String, usize)>>,
    /// `--timestat_date` (None: the environment, then the operator's default).
    pub timestat_date: Option<TimestatDate>,
    /// `--percentile <method>` (cdo's names).
    pub percentile: Option<String>,
    /// `--no_history`.
    pub no_history: bool,
    /// `--lonlat`: also write cell-centre longitudes and latitudes for grids stored without them
    /// (HEALPix), for viewers that need explicit coordinates.
    pub lonlat: bool,
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
