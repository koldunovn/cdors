//! Several files or stores presented as one dataset, concatenated along time.
//!
//! [`MultiFileSource::open`] opens every member (each through [`super::open`]), orders them by
//! their first timestep and checks that they fit together: same calendar, same data variables
//! with the same dimensions, types, chunking outside time, grids and vertical axes, and time
//! strictly increasing from one member to the next. The result is one dataset whose time axis is
//! the concatenation of the members' axes (re-expressed against the first member's reference
//! date) and whose variables with a time dimension span all members.
//!
//! Chunk grid: along time the virtual chunk size `g` is the greatest common divisor of the
//! members' time chunk sizes and of their start offsets, so every virtual chunk lies inside one
//! chunk of one member. Usually `g` equals the members' chunk size (daily files with one step per
//! chunk; Zarr stores whose lengths are multiples of the time chunk); otherwise a member chunk is
//! decoded once per virtual chunk it holds and sliced. Encoded chunks stay encoded: the decoder
//! is wrapped so decoding still runs on the compute pool.
//!
//! Inputs: [`open_many`] takes a list of paths (the inputs of `mergetime`, or any operator given
//! several files); [`expand_glob`] turns one argument with `*`, `?` or `[` into sorted paths, and
//! [`super::open`] does that automatically for a pattern that is not an existing path.

use super::{ChunkDecoder, ChunkGrid, ChunkSource, DecodedChunk, EncodedChunk, RawChunk, Values};
use crate::error::{Error, ErrorCode, Result};
use crate::model::time::{TimeAxis, TimeStep, TimeUnit, TimeUnits};
use crate::model::{Dataset, Variable};
use std::collections::HashMap;
use std::sync::Arc;

/// Whether `arg` contains glob metacharacters.
pub fn is_glob(arg: &str) -> bool {
    arg.contains(['*', '?', '['])
}

/// Expands a glob pattern into the matching paths, sorted. Errors if nothing matches.
pub fn expand_glob(pattern: &str) -> Result<Vec<String>> {
    let paths = glob::glob(pattern).map_err(|e| {
        Error::bad_arguments(format!("invalid file pattern '{pattern}': {e}")).with("path", pattern)
    })?;
    let mut out: Vec<String> = paths
        .filter_map(|p| p.ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    out.sort();
    if out.is_empty() {
        return Err(Error::new(
            ErrorCode::MissingInput,
            format!("no input matches '{pattern}'"),
        )
        .with("path", pattern));
    }
    Ok(out)
}

/// Opens several inputs as one dataset concatenated along time (one input: opened as is).
/// Arguments with glob characters that are not existing paths are expanded first.
pub fn open_many(args: &[String]) -> Result<Arc<dyn ChunkSource>> {
    let mut paths = Vec::new();
    for a in args {
        if is_glob(a) && !std::path::Path::new(a).exists() {
            paths.extend(expand_glob(a)?);
        } else {
            paths.push(a.clone());
        }
    }
    match paths.as_slice() {
        [] => Err(Error::bad_arguments("no inputs")),
        [one] => super::open(one),
        _ => Ok(Arc::new(MultiFileSource::open(&paths)?)),
    }
}

/// Per-variable mapping of the virtual chunk grid onto the members.
struct VarMap {
    grid: ChunkGrid,
    /// Position of the time dimension (`None`: the variable is read from the first member).
    tpos: Option<usize>,
    /// Time chunk size of each member.
    tchunk: Vec<usize>,
}

/// N members concatenated along time.
pub struct MultiFileSource {
    members: Vec<Arc<dyn ChunkSource>>,
    ds: Dataset,
    /// Global index of each member's first timestep.
    starts: Vec<usize>,
    vars: HashMap<String, VarMap>,
    /// Members disagree on the time units: time values are re-encoded on read.
    reencode_time: bool,
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

fn mismatch(paths: (&str, &str), what: String) -> Error {
    Error::bad_data(format!(
        "cannot concatenate '{}' and '{}' along time: {what}",
        paths.0, paths.1
    ))
    .with_hint("mergetime needs the same variables, grids and levels in every input")
}

/// The part of a variable that must agree between members (everything but the time size).
fn var_signature(v: &Variable, tdim: &str) -> String {
    let dims: Vec<String> = v
        .dims
        .iter()
        .map(|d| {
            if d.name == tdim {
                format!("{}=*", d.name)
            } else {
                format!("{}={}", d.name, d.size)
            }
        })
        .collect();
    let chunks: Vec<String> = v
        .dims
        .iter()
        .zip(&v.chunks)
        .map(|(d, c)| {
            if d.name == tdim {
                "*".to_owned()
            } else {
                c.to_string()
            }
        })
        .collect();
    format!(
        "{:?} {} [{}] chunks [{}] {:?}",
        v.kind,
        v.dtype.name(),
        dims.join(","),
        chunks.join(","),
        v.encoding
    )
}

impl MultiFileSource {
    /// Opens and checks the members (see the module docs).
    pub fn open(paths: &[String]) -> Result<Self> {
        let mut members: Vec<(String, Arc<dyn ChunkSource>)> = Vec::with_capacity(paths.len());
        for p in paths {
            let src = super::open(p)?;
            if src.dataset().time.as_ref().is_none_or(TimeAxis::is_empty) {
                return Err(Error::bad_data(format!(
                    "'{p}' has no time axis; only inputs with timesteps can be concatenated"
                )));
            }
            members.push((p.clone(), src));
        }
        let first_dt =
            |s: &Arc<dyn ChunkSource>| s.dataset().time.as_ref().map(|t| t.steps[0].datetime);
        members.sort_by_key(|(_, s)| first_dt(s));

        let (p0, m0) = &members[0];
        let d0 = m0.dataset();
        let t0 = d0.time.as_ref().expect("checked above");
        let tdim = t0.dim.clone();
        let mut steps: Vec<TimeStep> = Vec::new();
        let mut bounds: Option<Vec<[TimeStep; 2]>> = t0.bounds.as_ref().map(|_| Vec::new());
        let mut starts = Vec::with_capacity(members.len());
        let mut reencode_time = false;
        let rebase = |s: &TimeStep| TimeStep {
            datetime: s.datetime,
            seconds: s.datetime.seconds_since(&t0.reference, t0.calendar),
        };
        let time_vars: Vec<&str> = d0
            .vars
            .iter()
            .filter(|v| v.dims.iter().any(|d| d.name == tdim))
            .map(|v| v.name.as_str())
            .collect();
        for (k, (p, m)) in members.iter().enumerate() {
            let d = m.dataset();
            let t = d.time.as_ref().expect("checked above");
            let pair = (p0.as_str(), p.as_str());
            if t.calendar != t0.calendar || t.dim != tdim || t.var != t0.var {
                return Err(mismatch(
                    pair,
                    format!(
                        "time axis {} ({}, {}) vs {} ({}, {})",
                        t0.var,
                        t0.dim,
                        t0.calendar.cf_name(),
                        t.var,
                        t.dim,
                        t.calendar.cf_name()
                    ),
                ));
            }
            reencode_time |= t.units_attr != t0.units_attr;
            if k > 0 {
                let prev = steps.last().expect("members have steps");
                if t.steps[0].datetime <= prev.datetime {
                    return Err(mismatch(
                        pair,
                        format!(
                            "time does not increase: {} follows {} (overlapping inputs)",
                            t.steps[0].datetime.cdo_string().trim_start(),
                            prev.datetime.cdo_string().trim_start()
                        ),
                    ));
                }
                // everything except the time axis must agree with the first member
                let names = |d: &Dataset| -> Vec<String> {
                    d.data_vars().map(|v| v.name.clone()).collect()
                };
                if names(d) != names(d0) {
                    return Err(mismatch(
                        pair,
                        format!("variables {:?} vs {:?}", names(d0), names(d)),
                    ));
                }
                for name in &time_vars {
                    let v0 = d0.var(name).expect("listed from d0");
                    let Some(v) = d.var(name) else {
                        return Err(mismatch(pair, format!("variable '{name}' missing")));
                    };
                    let (s0, s) = (var_signature(v0, &tdim), var_signature(v, &tdim));
                    if s0 != s {
                        return Err(mismatch(pair, format!("variable '{name}': {s0} vs {s}")));
                    }
                }
                if format!("{:?}", d.grids) != format!("{:?}", d0.grids) {
                    return Err(mismatch(pair, "different horizontal grids".into()));
                }
                if format!("{:?}", d.zaxes) != format!("{:?}", d0.zaxes) {
                    return Err(mismatch(pair, "different vertical axes".into()));
                }
            }
            starts.push(steps.len());
            steps.extend(t.steps.iter().map(rebase));
            match (&mut bounds, &t.bounds) {
                (Some(b), Some(tb)) => b.extend(tb.iter().map(|[a, z]| [rebase(a), rebase(z)])),
                _ => bounds = None,
            }
        }
        let ntime = steps.len();
        if reencode_time {
            // values are recomputed from the decoded steps, which needs a fixed-length unit
            if !matches!(
                t0.units,
                TimeUnits::Relative {
                    unit: TimeUnit::Second | TimeUnit::Minute | TimeUnit::Hour | TimeUnit::Day,
                    ..
                }
            ) {
                return Err(Error::bad_data(format!(
                    "inputs have different time units and '{}' cannot be re-encoded",
                    t0.units_attr
                ))
                .with_hint("set common time units first (cdo settunits / setreftime)"));
            }
        }

        // the concatenated description
        let mut ds = d0.clone();
        ds.source = paths.join(" ");
        for (n, s) in &mut ds.dims {
            if *n == tdim {
                *s = ntime;
            }
        }
        let mut vars = HashMap::new();
        for v in &mut ds.vars {
            let grid0 = m0.chunk_grid(&v.name)?;
            let tpos = v.dims.iter().position(|d| d.name == tdim);
            let Some(tp) = tpos else {
                vars.insert(
                    v.name.clone(),
                    VarMap {
                        grid: grid0,
                        tpos: None,
                        tchunk: Vec::new(),
                    },
                );
                continue;
            };
            let mut tchunk = Vec::with_capacity(members.len());
            let mut g = 0;
            for (k, (p, m)) in members.iter().enumerate() {
                let gk = m.chunk_grid(&v.name)?;
                let mut a = gk.chunk_shape.clone();
                let mut b = grid0.chunk_shape.clone();
                a[tp] = 0;
                b[tp] = 0;
                if a != b {
                    return Err(mismatch(
                        (p0, p),
                        format!(
                            "variable '{}' chunks {:?} vs {:?}",
                            v.name, grid0.chunk_shape, gk.chunk_shape
                        ),
                    ));
                }
                tchunk.push(gk.chunk_shape[tp]);
                g = gcd(g, gk.chunk_shape[tp]);
                if k > 0 {
                    g = gcd(g, starts[k]);
                }
            }
            let mut grid = grid0;
            grid.shape[tp] = ntime;
            grid.chunk_shape[tp] = g.max(1);
            v.dims[tp].size = ntime;
            v.chunks[tp] = grid.chunk_shape[tp];
            vars.insert(v.name.clone(), VarMap { grid, tpos, tchunk });
        }
        if let Some(t) = &mut ds.time {
            t.steps = steps;
            t.bounds = bounds;
            if t.bounds.is_none() {
                t.bounds_var = None;
            }
        }
        Ok(Self {
            members: members.into_iter().map(|(_, s)| s).collect(),
            ds,
            starts,
            vars,
            reencode_time,
        })
    }

    fn map(&self, var: &str) -> Result<&VarMap> {
        self.vars.get(var).ok_or_else(|| {
            Error::bad_arguments(format!(
                "variable '{var}' not found in '{}'",
                self.ds.source
            ))
        })
    }

    /// Member holding global timestep `t`.
    fn member_of(&self, t: usize) -> usize {
        self.starts.partition_point(|&s| s <= t) - 1
    }

    /// Time values of the time variable or its bounds, re-encoded in the first member's units.
    fn reencoded_time(&self, var: &str) -> Option<Vec<f64>> {
        let t = self.ds.time.as_ref()?;
        let unit = match t.units {
            TimeUnits::Relative { unit, .. } => match unit {
                TimeUnit::Second => 1.0,
                TimeUnit::Minute => 60.0,
                TimeUnit::Hour => 3600.0,
                TimeUnit::Day => 86400.0,
                _ => return None,
            },
            TimeUnits::AbsoluteDay => return None,
        };
        if var == t.var {
            Some(t.steps.iter().map(|s| s.seconds as f64 / unit).collect())
        } else if t.bounds_var.as_deref() == Some(var) {
            let b = t.bounds.as_ref()?;
            Some(
                b.iter()
                    .flat_map(|[a, z]| [a.seconds as f64 / unit, z.seconds as f64 / unit])
                    .collect(),
            )
        } else {
            None
        }
    }
}

/// Copies `[start, start + len)` along dimension `pos` of a C-order array of shape `shape`.
fn slice_along<T: Copy>(v: &[T], shape: &[usize], pos: usize, start: usize, len: usize) -> Vec<T> {
    let outer: usize = shape[..pos].iter().product();
    let inner: usize = shape[pos + 1..].iter().product();
    let n = shape[pos];
    let mut out = Vec::with_capacity(outer * len * inner);
    for o in 0..outer {
        let base = (o * n + start) * inner;
        out.extend_from_slice(&v[base..base + len * inner]);
    }
    out
}

/// Where a virtual chunk comes from.
#[derive(Clone)]
struct Placement {
    /// Chunk indices in the member.
    local: Vec<u64>,
    tpos: usize,
    /// First step of the virtual chunk inside the member chunk, and its length.
    start: usize,
    len: usize,
    /// Global origin of the virtual chunk.
    origin: Vec<u64>,
}

impl Placement {
    fn apply(&self, d: DecodedChunk) -> DecodedChunk {
        let mut shape = d.shape;
        let values = if self.start == 0 && self.len == shape[self.tpos] {
            d.values
        } else {
            let (p, s, l) = (self.tpos, self.start, self.len);
            let v = match d.values {
                Values::F32(v) => Values::F32(slice_along(&v, &shape, p, s, l)),
                Values::F64(v) => Values::F64(slice_along(&v, &shape, p, s, l)),
            };
            shape[p] = l;
            v
        };
        DecodedChunk {
            origin: self.origin.clone(),
            shape,
            values,
        }
    }
}

/// A member's decoder, mapped onto the virtual chunk grid.
struct MappedDecoder {
    inner: Arc<dyn ChunkDecoder>,
    place: Placement,
}

impl ChunkDecoder for MappedDecoder {
    fn decode(&self, _indices: &[u64], bytes: Option<&[u8]>) -> Result<DecodedChunk> {
        let d = self.inner.decode(&self.place.local, bytes)?;
        Ok(self.place.apply(d))
    }

    fn codecs(&self) -> String {
        self.inner.codecs()
    }
}

/// Concatenates C-order arrays along dimension `pos`; `shapes[k]` is the shape of `parts[k]`.
fn concat_along(parts: &[Vec<f64>], shapes: &[Vec<usize>], pos: usize) -> Vec<f64> {
    let outer: usize = shapes[0][..pos].iter().product();
    let inner: usize = shapes[0][pos + 1..].iter().product();
    let mut out = Vec::with_capacity(parts.iter().map(Vec::len).sum());
    for o in 0..outer {
        for (p, s) in parts.iter().zip(shapes) {
            let n = s[pos] * inner;
            out.extend_from_slice(&p[o * n..(o + 1) * n]);
        }
    }
    out
}

impl ChunkSource for MultiFileSource {
    fn dataset(&self) -> &Dataset {
        &self.ds
    }

    fn chunk_grid(&self, var: &str) -> Result<ChunkGrid> {
        Ok(self.map(var)?.grid.clone())
    }

    fn read_chunk(&self, var: &str, indices: &[u64]) -> Result<RawChunk> {
        let vm = self.map(var)?;
        let Some(tp) = vm.tpos else {
            return self.members[0].read_chunk(var, indices);
        };
        vm.grid.check(var, indices)?;
        if self.reencode_time
            && tp == 0
            && vm.grid.chunk_shape[1..] == vm.grid.shape[1..]
            && let Some(all) = self.reencoded_time(var)
        {
            // time values in the first member's units, cut to the chunk
            let origin = vm.grid.origin(indices);
            let shape = vm.grid.extent(indices);
            let row: usize = vm.grid.shape[1..].iter().product();
            let start = origin[0] as usize * row;
            let v = all[start..start + shape.iter().product::<usize>()].to_vec();
            let f32 = self.ds.var(var).is_some_and(|x| x.encoding.unpacked_f32);
            return Ok(RawChunk::Decoded(DecodedChunk {
                origin,
                shape,
                values: if f32 {
                    Values::F32(v.into_iter().map(|x| x as f32).collect())
                } else {
                    Values::F64(v)
                },
            }));
        }
        let g = vm.grid.chunk_shape[tp];
        let t = indices[tp] as usize * g;
        let k = self.member_of(t);
        let local_t = t - self.starts[k];
        let c = vm.tchunk[k];
        let mut local = indices.to_vec();
        local[tp] = (local_t / c) as u64;
        let place = Placement {
            local,
            tpos: tp,
            start: local_t % c,
            len: vm.grid.extent(indices)[tp],
            origin: vm.grid.origin(indices),
        };
        Ok(match self.members[k].read_chunk(var, &place.local)? {
            RawChunk::Decoded(d) => RawChunk::Decoded(place.apply(d)),
            RawChunk::Encoded(e) => RawChunk::Encoded(EncodedChunk {
                indices: indices.to_vec(),
                bytes: e.bytes,
                decoder: Arc::new(MappedDecoder {
                    inner: e.decoder,
                    place,
                }),
            }),
        })
    }

    fn read_var(&self, var: &str) -> Result<Vec<f64>> {
        let vm = self.map(var)?;
        let Some(tp) = vm.tpos else {
            return self.members[0].read_var(var);
        };
        if self.reencode_time
            && let Some(v) = self.reencoded_time(var)
        {
            return Ok(v);
        }
        let mut parts = Vec::with_capacity(self.members.len());
        let mut shapes = Vec::with_capacity(self.members.len());
        for m in &self.members {
            shapes.push(m.chunk_grid(var)?.shape);
            parts.push(m.read_var(var)?);
        }
        Ok(concat_along(&parts, &shapes, tp))
    }

    fn codecs(&self, var: &str) -> Option<String> {
        self.members[0].codecs(var)
    }
}
