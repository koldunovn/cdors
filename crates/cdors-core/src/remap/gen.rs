//! Weight generation by cdo (`cdo gen<method>`) with an on-disk cache.
//!
//! Weights are always generated for the *unmasked* source grid (`-setmisstoc,0` on the first
//! timestep), so they depend only on the grids and the method; `weights.rs` applies cdo's
//! missing-value rules at application time. The cache file is
//! `<cache>/<key>.nc`, `key` = 128-bit FNV-1a of (method, target grid, source grid identity).
//! A file is written under `<cache>/tmp/` and renamed into place, so concurrent callers never see
//! half-written weights. Nothing in the cache is ever evicted or removed; a failed cdo run leaves
//! its temporary file in `<cache>/tmp/`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{PRECOMPUTED_HINT, RemapError};

/// Hint attached to failures of target-grid template generation.
pub const TEMPLATE_HINT: &str = "the target grid must be one cdo accepts: r<nx>x<ny>, global_<inc>, \
     hpz<zoom>[_nested|_ring], hp<nside>, a grid description file or a NetCDF file";

/// Methods whose weights cdo can generate for cdors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GenMethod {
    Nn,
    Dis,
    Bil,
    Con,
    /// Alias of `Con` in cdo 2.6 (`genycon` → `gencon`).
    Ycon,
}

impl GenMethod {
    /// Parse `nn`, `dis`, `bil`, `con`, `ycon` (also with a `remap`/`gen` prefix).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s
            .strip_prefix("remap")
            .or_else(|| s.strip_prefix("gen"))
            .unwrap_or(s);
        Some(match s {
            "nn" => Self::Nn,
            "dis" => Self::Dis,
            "bil" => Self::Bil,
            "con" => Self::Con,
            "ycon" => Self::Ycon,
            _ => return None,
        })
    }

    /// cdo operator name (`ycon` maps to `gencon`, which is what cdo itself does).
    pub fn cdo_operator(self) -> &'static str {
        match self {
            Self::Nn => "gennn",
            Self::Dis => "gendis",
            Self::Bil => "genbil",
            Self::Con | Self::Ycon => "gencon",
        }
    }
}

/// How the source grid is identified for the cache key.
#[derive(Debug, Clone, Copy)]
pub enum SourceIdentity<'a> {
    /// Hash the static (time-independent, non-vertical) variables of the source NetCDF file:
    /// coordinates, bounds, grid mapping, with their attributes and values.
    HashFile,
    /// Caller-provided identity bytes (e.g. a hash of coordinates and bounds from the data model).
    Bytes(&'a [u8]),
}

/// One request for weights.
#[derive(Debug, Clone)]
pub struct WeightRequest<'a> {
    pub method: GenMethod,
    /// Target grid as cdo accepts it: `r360x180`, `global_1`, `hpz7`, a griddes file, a dataset.
    pub target: &'a str,
    /// NetCDF file (local path, readable by cdo) that carries the source grid.
    pub source: &'a Path,
    /// Variable whose grid is the source grid (default: cdo takes the first remappable grid).
    pub variable: Option<&'a str>,
    pub identity: SourceIdentity<'a>,
}

impl<'a> WeightRequest<'a> {
    pub fn new(method: GenMethod, target: &'a str, source: &'a Path) -> Self {
        Self {
            method,
            target,
            source,
            variable: None,
            identity: SourceIdentity::HashFile,
        }
    }
}

/// The weight cache directory plus the cdo executable used to fill it.
#[derive(Debug, Clone)]
pub struct WeightCache {
    dir: PathBuf,
    cdo: Option<PathBuf>,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl WeightCache {
    /// `$CDORS_CACHE/weights`, cdo from `$CDO` or `cdo` on `PATH`.
    pub fn from_env() -> Result<Self, RemapError> {
        let base = std::env::var_os("CDORS_CACHE").ok_or_else(|| {
            RemapError::Unsupported("CDORS_CACHE is not set (source env.sh)".into())
        })?;
        Ok(Self::new(Path::new(&base).join("weights"), find_cdo()))
    }

    pub fn new(dir: PathBuf, cdo: Option<PathBuf>) -> Self {
        Self { dir, cdo }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether a cdo executable was found (`$CDO` or `cdo` on `PATH`).
    pub fn has_cdo(&self) -> bool {
        self.cdo.is_some()
    }

    /// Cache file name of a request (computing the source identity if needed).
    pub fn key(&self, req: &WeightRequest<'_>) -> Result<String, RemapError> {
        let mut h = Fnv128::new();
        h.update(b"cdors-weights-v1\0");
        // ycon and con produce the same weights.
        h.update(req.method.cdo_operator().as_bytes());
        h.update(b"\0");
        self.target_hash(&mut h, req.target)?;
        h.update(b"\0source\0");
        match req.identity {
            // The caller's identity names the source grid itself; the variable only picks that
            // grid in the file, so the same grid read from another file hits the same weights.
            SourceIdentity::Bytes(b) => h.update(b),
            SourceIdentity::HashFile => {
                hash_source_grid(req.source, &mut h)?;
                if let Some(v) = req.variable {
                    h.update(b"\0var\0");
                    h.update(v.as_bytes());
                }
            }
        }
        Ok(format!("{}-{:032x}", req.method.cdo_operator(), h.finish()))
    }

    /// Hash of a target grid argument: its name, or the bytes of a grid file.
    fn target_hash(&self, h: &mut Fnv128, target: &str) -> Result<(), RemapError> {
        let path = Path::new(target);
        if path.is_file() {
            let bytes = std::fs::read(path).map_err(|source| RemapError::Io {
                path: path.to_owned(),
                source,
            })?;
            h.update(b"file\0");
            h.update(&bytes);
        } else {
            h.update(b"name\0");
            h.update(target.as_bytes());
        }
        Ok(())
    }

    /// Cache key and path of the grid template of `target` (whether it exists or not); reads
    /// nothing but a grid file named by `target`, writes nothing.
    pub fn template_path(&self, target: &str) -> Result<(String, PathBuf), RemapError> {
        let mut h = Fnv128::new();
        h.update(b"cdors-grid-v1\0");
        self.target_hash(&mut h, target)?;
        let key = format!("grid-{:032x}", h.finish());
        let path = self.dir.join(format!("{key}.nc"));
        Ok((key, path))
    }

    /// A small NetCDF file on the target grid, written by cdo (`cdo -f nc4 const,0,<grid>`) and
    /// cached as `<cache>/grid-<key>.nc`, so that output grids (coordinates, bounds, HEALPix
    /// mapping, attributes) are exactly what cdo writes for `remap*,<grid>`.
    pub fn grid_template(&self, target: &str) -> Result<PathBuf, RemapError> {
        let (key, path) = self.template_path(target)?;
        if path.is_file() {
            return Ok(path);
        }
        let cdo = self.cdo.as_deref().ok_or_else(|| RemapError::CdoMissing {
            reason: "neither $CDO nor `cdo` on PATH".into(),
            hint: TEMPLATE_HINT,
        })?;
        let tmp = self.tmp_path(&key)?;
        let args: Vec<String> = vec![
            "-s".into(),
            "--no_history".into(),
            "-f".into(),
            "nc4".into(),
            format!("const,0,{target}"),
            tmp.to_string_lossy().into_owned(),
        ];
        run_cdo_hint(cdo, &args, TEMPLATE_HINT)?;
        self.publish(&tmp, &path)?;
        Ok(path)
    }

    /// A fresh temporary path under `<cache>/tmp/`.
    pub fn tmp_path(&self, key: &str) -> Result<PathBuf, RemapError> {
        let tmpdir = self.dir.join("tmp");
        std::fs::create_dir_all(&tmpdir).map_err(|source| RemapError::Io {
            path: tmpdir.clone(),
            source,
        })?;
        Ok(tmpdir.join(format!(
            "{key}.{}.{}.nc",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        )))
    }

    /// Renames a finished temporary file into place.
    pub fn publish(&self, tmp: &Path, path: &Path) -> Result<(), RemapError> {
        std::fs::rename(tmp, path).map_err(|source| RemapError::Io {
            path: path.to_owned(),
            source,
        })
    }

    /// Path of cached weights for the request, generating them with cdo on a cache miss.
    pub fn weights_for(&self, req: &WeightRequest<'_>) -> Result<PathBuf, RemapError> {
        let key = self.key(req)?;
        let path = self.dir.join(format!("{key}.nc"));
        if path.is_file() {
            return Ok(path);
        }
        let cdo = self.cdo.as_deref().ok_or_else(|| RemapError::CdoMissing {
            reason: "neither $CDO nor `cdo` on PATH".into(),
            hint: PRECOMPUTED_HINT,
        })?;
        if req.method == GenMethod::Bil {
            check_bilinear_source(cdo, req)?;
        }
        let tmp = self.tmp_path(&key)?;
        // -L serializes cdo's file access across its threads. Without it, `gen<method>` reads a
        // NetCDF-4 target grid file (H5Fopen in `cdo_define_grid`) while the thread of the
        // chained `-setmisstoc -seltimestep` reads the source through HDF5, which is not
        // thread-safe: cdo crashed with SIGSEGV and no message in about a third of the runs
        // with a Zarr (or NetCDF-4) target. -L is cdo's own remedy for a non-thread-safe HDF5.
        let mut args: Vec<String> = vec!["-s".into(), "--no_history".into(), "-L".into()];
        if matches!(req.method, GenMethod::Con | GenMethod::Ycon) {
            // cdo refuses conservative weights for HEALPix grids without --force (cell edges are
            // not great circles); --force has no other effect on gen* (src/remaplib.cc:253).
            args.push("--force".into());
        }
        args.push(format!("{},{}", req.method.cdo_operator(), req.target));
        // Unmasked source grid: one field of the first timestep, missing values set to 0.
        args.push("-setmisstoc,0".into());
        args.push("-seltimestep,1".into());
        if let Some(v) = req.variable {
            args.push(format!("-selname,{v}"));
        }
        args.push(req.source.to_string_lossy().into_owned());
        args.push(tmp.to_string_lossy().into_owned());
        run_cdo(cdo, &args)?;
        self.publish(&tmp, &path)?;
        Ok(path)
    }
}

/// `$CDO` if set and non-empty, else `cdo` found on `PATH`.
pub fn find_cdo() -> Option<PathBuf> {
    if let Some(c) = std::env::var_os("CDO").filter(|c| !c.is_empty()) {
        return Some(PathBuf::from(c));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("cdo"))
        .find(|p| p.is_file())
}

fn run_cdo(cdo: &Path, args: &[String]) -> Result<String, RemapError> {
    run_cdo_hint(cdo, args, PRECOMPUTED_HINT)
}

fn run_cdo_hint(cdo: &Path, args: &[String], hint: &'static str) -> Result<String, RemapError> {
    let command = format!("{} {}", cdo.display(), args.join(" "));
    let out = Command::new(cdo)
        .args(args)
        .output()
        .map_err(|e| RemapError::CdoMissing {
            reason: format!("cannot run {}: {e}", cdo.display()),
            hint,
        })?;
    if !out.status.success() {
        let mut stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        if stderr.is_empty() {
            stderr = format!(
                "{} without a message on stderr; stdout: {:?}",
                out.status,
                String::from_utf8_lossy(&out.stdout).trim()
            );
        }
        return Err(RemapError::CdoFailed {
            command,
            stderr,
            hint,
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// cdo's genbil aborts on unstructured and GME source grids (src/operators/Remapweights.cc:248);
/// ask cdo for the grid type first so the error names the reason.
fn check_bilinear_source(cdo: &Path, req: &WeightRequest<'_>) -> Result<(), RemapError> {
    let mut args = vec!["-s".to_owned(), "griddes".to_owned()];
    if let Some(v) = req.variable {
        args.push(format!("-selname,{v}"));
    }
    args.push(req.source.to_string_lossy().into_owned());
    let griddes = run_cdo(cdo, &args)?;
    let first_type = griddes
        .lines()
        .filter_map(|l| l.trim().strip_prefix("gridtype"))
        .filter_map(|l| l.trim_start().strip_prefix('='))
        .map(str::trim)
        .next()
        .unwrap_or("");
    if first_type == "unstructured" || first_type == "gme" {
        return Err(RemapError::Unsupported(format!(
            "remapbil: cdo does not support bilinear interpolation from {first_type} source grids \
             ({}); use remapdis, remapnn or remapcon",
            req.source.display()
        )));
    }
    Ok(())
}

/// Hash every variable without a time dimension and without a vertical axis: coordinates,
/// bounds and grid mappings, including attributes and values.
fn hash_source_grid(path: &Path, h: &mut Fnv128) -> Result<(), RemapError> {
    let nc_err = |source| RemapError::Netcdf {
        path: path.to_owned(),
        source,
    };
    let _hdf5 = crate::io::hdf5_lock();
    let file = netcdf::open(path).map_err(nc_err)?;
    let is_time_dim = |name: &str, unlimited: bool| {
        unlimited
            || file.variable(name).is_some_and(|v| {
                matches!(
                    v.attribute("units").and_then(|a| a.value().ok()),
                    Some(netcdf::AttributeValue::Str(s)) if s.contains(" since ")
                )
            })
    };
    let is_vertical = |v: &netcdf::Variable<'_>| {
        v.attribute("positive").is_some()
            || matches!(
                v.attribute("axis").and_then(|a| a.value().ok()),
                Some(netcdf::AttributeValue::Str(s)) if s == "Z"
            )
    };
    // Non-time, non-vertical dimensions (a HEALPix file may carry no coordinate variables).
    let mut dims: Vec<(String, usize)> = file
        .dimensions()
        .filter(|d| {
            !is_time_dim(&d.name(), d.is_unlimited())
                && !file.variable(&d.name()).is_some_and(|v| is_vertical(&v))
        })
        .map(|d| (d.name(), d.len()))
        .collect();
    dims.sort();
    for (name, len) in dims {
        h.update(name.as_bytes());
        h.update(&(len as u64).to_le_bytes());
    }
    let mut names: Vec<String> = file.variables().map(|v| v.name()).collect();
    names.sort();
    for name in names {
        let Some(v) = file.variable(&name) else {
            continue;
        };
        if v.dimensions()
            .iter()
            .any(|d| is_time_dim(&d.name(), d.is_unlimited()))
            || is_vertical(&v)
        {
            continue;
        }
        h.update(name.as_bytes());
        h.update(b"\0");
        for d in v.dimensions() {
            h.update(d.name().as_bytes());
            h.update(&(d.len() as u64).to_le_bytes());
        }
        let mut atts: Vec<(String, String)> = v
            .attributes()
            .map(|a| {
                let val = a.value().map(|x| format!("{x:?}")).unwrap_or_default();
                (a.name().to_owned(), val)
            })
            .collect();
        atts.sort();
        for (k, val) in atts {
            h.update(k.as_bytes());
            h.update(b"=");
            h.update(val.as_bytes());
            h.update(b"\0");
        }
        // Numeric values; non-numeric (string/char) variables contribute metadata only.
        if let Ok(vals) = v.get_values::<f64, _>(..) {
            for x in vals {
                h.update(&x.to_bits().to_le_bytes());
            }
        }
    }
    Ok(())
}

/// 128-bit FNV-1a: stable across runs and platforms, no dependency.
pub(crate) struct Fnv128(u128);

impl Fnv128 {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013B;
    pub(crate) fn new() -> Self {
        Self(Self::OFFSET)
    }
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u128::from(b);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }
    pub(crate) fn finish(&self) -> u128 {
        self.0
    }
}
