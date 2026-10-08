//! Exact percentiles with CDO's definitions (`--percentile`).
//!
//! CDO keeps the raw values of a grid point only while it has at most 50 of them and computes
//! exact percentiles for those (`src/percentiles.cc`); above 50 values it switches to a 101-bin
//! histogram between the min and max input files (`src/percentiles_hist.cc`), which is
//! approximate. cdors always computes the exact value with the same method definitions, so results
//! equal CDO's for groups of up to 50 values and differ by at most about one histogram bin above.
//!
//! Every method is a function of the order statistics `x[0] <= x[1] <= ... <= x[n-1]` of the
//! valid (non-NaN) values, of `n` and of `q = p / 100`. The formulas below are those of CDO 2.6.5
//! `src/percentiles.cc` (identical in 2.6.0), evaluated in the same order in `f64`. The cdo build
//! contracts `a + b*c` into fused multiply-adds (GCC's default `-ffp-contract=fast`), so cdors
//! uses `mul_add` at the same places (the ranks of `rtype8`, the NumPy family, the continuous and
//! `closest_observation` variants, and both interpolations, as `fma(1-h, x[i], h*x[j])` and
//! `fma(d, x[k]-x[k-1], x[k-1])`); with that, `monpctl` matches cdo 2.6.0 bit for bit for all 16
//! methods (checked at p = 3, 37, 50, 66.6, 81.5, 95):
//!
//! - `nrank` (default, `percentile_nrank`): `x[clamp(ceil(n*q), 1, n) - 1]`.
//! - `nist` (`percentile_nist`): `r = (n+1)*q`, `k = trunc(r)`; `k == 0` gives `x[0]`, `k >= n`
//!   gives `x[n-1]`, otherwise `x[k-1] + (r-k)*(x[k] - x[k-1])`.
//! - `rtype8` (`percentile_Rtype8`): as `nist` with `r = 1/3 + (n + 1/3)*q`.
//! - NumPy family (`percentile_numpy`): `r = 1 + (n-1)*q`, `k = trunc(r)`; `k == 1` gives `x[0]`
//!   and `k >= n` gives `x[n-1]` for *every* variant; otherwise, with `lo = floor(r)`,
//!   `hi = ceil(r)`, `h = r - lo`:
//!   - `linear` (also `numpy`, `numpy_linear`): `(1-h)*x[lo-1] + h*x[hi-1]`;
//!   - `lower`/`higher` (also `numpy_lower`/`numpy_higher`): `x[lo-1]` / `x[hi-1]`;
//!   - `nearest` (also `numpy_nearest`): `x[lround(r)-1]`, i.e. halves round away from zero
//!     (not to even as in NumPy);
//!   - `midpoint`: `0.5*x[lo-1] + 0.5*x[hi-1]`;
//!   - continuous sample (R types 5-9): `interpolated_inverted_cdf` (a=0, b=1), `hazen`
//!     (0.5, 0.5), `weibull` (0, 0), `median_unbiased` (1/3, 1/3), `normal_unbiased` (3/8, 3/8):
//!     `m = a + q*(n + 1 - a - b)`, `j = floor(m + 4 eps)`, `h = m - j` (set to 0 if
//!     `|h| < 4 eps`); `0 < h < 1` gives `(1-h)*x[j-1] + h*x[j]`, else `x[j]` if `h >= 1` and
//!     `x[j-1]` otherwise;
//!   - discontinuous sample (R types 1-3): `inverted_cdf` (`m = n*q`, `h = [m > j]`),
//!     `averaged_inverted_cdf` (`m = n*q`, `h = ([m > j] + 1)/2`), `closest_observation`
//!     (`m = n*q - 1/2`, `h = [m != j or j odd]`), with `j = floor(m)` and the same selection
//!     rule as the continuous variants.
//!
//! One deviation: in the continuous variants with `b < 1` (`hazen`, `weibull`, `median_unbiased`,
//! `normal_unbiased`), CDO can compute `j == n` for `q` close to 1 and then reads `x[n]`, one past
//! the end of its buffer (zeroed memory in `timpctl`/`monpctl` for fewer than 50 values, so CDO
//! returns `(1-h)*x[n-1]`). cdors clamps the index to `n-1` and returns `x[n-1]` there, as NumPy
//! does. [`cdo_reads_past_end`] tells where this happens.

use std::cmp::Ordering;

use rayon::prelude::*;

/// NumPy-style variants of CDO's `--percentile` (`NumpyMethod` in `src/percentiles.cc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NumpyMethod {
    Linear,
    Lower,
    Higher,
    Nearest,
    Midpoint,
    InvertedCdf,
    AveragedInvertedCdf,
    ClosestObservation,
    InterpolatedInvertedCdf,
    Hazen,
    Weibull,
    MedianUnbiased,
    NormalUnbiased,
}

/// Percentile definition, as selected by CDO's `--percentile <method>`. The default is
/// [`PercentileMethod::Nrank`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PercentileMethod {
    /// Nearest rank (CDO's default).
    #[default]
    Nrank,
    /// Linear interpolation recommended by NIST (R type 6).
    Nist,
    /// R's type 8 (`rtype8`).
    Rtype8,
    Numpy(NumpyMethod),
}

impl PercentileMethod {
    /// Every distinct method, in CDO's order.
    pub const ALL: [Self; 16] = {
        use NumpyMethod as N;
        [
            Self::Nrank,
            Self::Nist,
            Self::Rtype8,
            Self::Numpy(N::Linear),
            Self::Numpy(N::Lower),
            Self::Numpy(N::Higher),
            Self::Numpy(N::Nearest),
            Self::Numpy(N::Midpoint),
            Self::Numpy(N::InvertedCdf),
            Self::Numpy(N::AveragedInvertedCdf),
            Self::Numpy(N::ClosestObservation),
            Self::Numpy(N::InterpolatedInvertedCdf),
            Self::Numpy(N::Hazen),
            Self::Numpy(N::Weibull),
            Self::Numpy(N::MedianUnbiased),
            Self::Numpy(N::NormalUnbiased),
        ]
    };

    /// Parses a `--percentile` argument with CDO's names, case-insensitively
    /// (`percentile_set_method`): `nrank`, `nist`, `rtype8`, `numpy`, `linear`/`numpy_linear`,
    /// `lower`/`numpy_lower`, `higher`/`numpy_higher`, `nearest`/`numpy_nearest`, `midpoint`,
    /// `inverted_cdf`, `averaged_inverted_cdf`, `closest_observation`,
    /// `interpolated_inverted_cdf`, `hazen`, `weibull`, `median_unbiased`, `normal_unbiased`.
    pub fn parse(s: &str) -> Option<Self> {
        use NumpyMethod as N;
        let m = match s.to_ascii_lowercase().as_str() {
            "nrank" => Self::Nrank,
            "nist" => Self::Nist,
            "rtype8" => Self::Rtype8,
            "numpy" | "linear" | "numpy_linear" => Self::Numpy(N::Linear),
            "lower" | "numpy_lower" => Self::Numpy(N::Lower),
            "higher" | "numpy_higher" => Self::Numpy(N::Higher),
            "nearest" | "numpy_nearest" => Self::Numpy(N::Nearest),
            "midpoint" => Self::Numpy(N::Midpoint),
            "inverted_cdf" => Self::Numpy(N::InvertedCdf),
            "averaged_inverted_cdf" => Self::Numpy(N::AveragedInvertedCdf),
            "closest_observation" => Self::Numpy(N::ClosestObservation),
            "interpolated_inverted_cdf" => Self::Numpy(N::InterpolatedInvertedCdf),
            "hazen" => Self::Numpy(N::Hazen),
            "weibull" => Self::Numpy(N::Weibull),
            "median_unbiased" => Self::Numpy(N::MedianUnbiased),
            "normal_unbiased" => Self::Numpy(N::NormalUnbiased),
            _ => return None,
        };
        Some(m)
    }

    /// The canonical `--percentile` name (accepted by both CDO and [`Self::parse`]).
    pub fn name(self) -> &'static str {
        use NumpyMethod as N;
        match self {
            Self::Nrank => "nrank",
            Self::Nist => "nist",
            Self::Rtype8 => "rtype8",
            Self::Numpy(m) => match m {
                N::Linear => "linear",
                N::Lower => "lower",
                N::Higher => "higher",
                N::Nearest => "nearest",
                N::Midpoint => "midpoint",
                N::InvertedCdf => "inverted_cdf",
                N::AveragedInvertedCdf => "averaged_inverted_cdf",
                N::ClosestObservation => "closest_observation",
                N::InterpolatedInvertedCdf => "interpolated_inverted_cdf",
                N::Hazen => "hazen",
                N::Weibull => "weibull",
                N::MedianUnbiased => "median_unbiased",
                N::NormalUnbiased => "normal_unbiased",
            },
        }
    }
}

/// A value type the kernels accept; NaN marks a missing value.
pub trait Sample: Copy + Default + Send + Sync {
    fn to_f64(self) -> f64;
    fn is_nan(self) -> bool;
    fn total_cmp(&self, other: &Self) -> Ordering;
}

impl Sample for f32 {
    #[inline]
    fn to_f64(self) -> f64 {
        self as f64
    }
    #[inline]
    fn is_nan(self) -> bool {
        f32::is_nan(self)
    }
    #[inline]
    fn total_cmp(&self, other: &Self) -> Ordering {
        f32::total_cmp(self, other)
    }
}

impl Sample for f64 {
    #[inline]
    fn to_f64(self) -> f64 {
        self
    }
    #[inline]
    fn is_nan(self) -> bool {
        f64::is_nan(self)
    }
    #[inline]
    fn total_cmp(&self, other: &Self) -> Ordering {
        f64::total_cmp(self, other)
    }
}

/// Which order statistics a method needs and how it combines them.
#[derive(Debug, Clone, Copy)]
enum Pick {
    /// `x[i]`.
    One(usize),
    /// `x[i] + d*(x[i+1] - x[i])` (`nist`, `rtype8`).
    Lerp(usize, f64),
    /// `(1-h)*x[i] + h*x[j]` with `j` in `{i, i+1}` (NumPy family).
    Blend(usize, usize, f64),
}

/// Result of [`pick`]: the order statistics to combine, and whether CDO itself would read one past
/// the end (see the module docs).
fn pick(n: usize, q: f64, method: PercentileMethod) -> (Pick, bool) {
    debug_assert!(n > 0);
    let nf = n as f64;
    let lerp = |rank: f64| {
        let k = rank as usize;
        if k == 0 {
            Pick::One(0)
        } else if k >= n {
            Pick::One(n - 1)
        } else {
            Pick::Lerp(k - 1, rank - k as f64)
        }
    };
    let one_clamped = |i: usize| Pick::One(i.clamp(1, n) - 1);
    let p = match method {
        PercentileMethod::Nrank => one_clamped((nf * q).ceil() as usize),
        PercentileMethod::Nist => lerp((n + 1) as f64 * q),
        PercentileMethod::Rtype8 => lerp((nf + 1.0 / 3.0).mul_add(q, 1.0 / 3.0)),
        PercentileMethod::Numpy(m) => return pick_numpy(n, q, m),
    };
    (p, false)
}

fn pick_numpy(n: usize, q: f64, m: NumpyMethod) -> (Pick, bool) {
    use NumpyMethod as N;
    let nf = n as f64;
    let rank = ((n - 1) as f64).mul_add(q, 1.0);
    let k = rank as usize;
    if k == 1 {
        return (Pick::One(0), false);
    }
    if k >= n {
        return (Pick::One(n - 1), false);
    }
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    let one_clamped = |i: usize| Pick::One(i.clamp(1, n) - 1);
    // Shared selection rule of the R-type variants: interpolate for 0 < h < 1, else pick one.
    // An index past the end (where CDO reads out of bounds) is replaced by `x[n-1]`.
    let select = |j: usize, h: f64| -> (Pick, bool) {
        if h > 0.0 && h < 1.0 {
            if j >= n {
                (Pick::One(n - 1), true)
            } else {
                (Pick::Blend(j.saturating_sub(1), j, h), false)
            }
        } else {
            let i = if h >= 1.0 { j } else { j.saturating_sub(1) };
            if i >= n {
                (Pick::One(n - 1), true)
            } else {
                (Pick::One(i), false)
            }
        }
    };
    let p = match m {
        N::Linear => Pick::Blend(lo - 1, hi - 1, rank - lo as f64),
        N::Lower => one_clamped(lo),
        N::Higher => one_clamped(hi),
        // C's lround: halves away from zero, as f64::round.
        N::Nearest => one_clamped(rank.round() as usize),
        N::Midpoint => Pick::Blend(lo - 1, hi - 1, 0.5),
        N::InterpolatedInvertedCdf
        | N::Hazen
        | N::Weibull
        | N::MedianUnbiased
        | N::NormalUnbiased => {
            let (a, b) = match m {
                N::InterpolatedInvertedCdf => (0.0, 1.0),
                N::Hazen => (0.5, 0.5),
                N::Weibull => (0.0, 0.0),
                N::MedianUnbiased => (1.0 / 3.0, 1.0 / 3.0),
                _ => (3.0 / 8.0, 3.0 / 8.0),
            };
            let nppn = q.mul_add(nf + 1.0 - a - b, a);
            let fuzz = 4.0 * f64::EPSILON;
            let j = (nppn + fuzz).floor() as usize;
            let mut h = nppn - j as f64;
            if h.abs() < fuzz {
                h = 0.0;
            }
            return select(j, h);
        }
        N::InvertedCdf | N::AveragedInvertedCdf | N::ClosestObservation => {
            let nppm = if m == N::ClosestObservation {
                nf.mul_add(q, -0.5)
            } else {
                nf * q
            };
            let j = nppm.floor() as usize;
            let jf = j as f64;
            let h = match m {
                N::InvertedCdf => f64::from(u8::from(nppm > jf)),
                N::AveragedInvertedCdf => (f64::from(u8::from(nppm > jf)) + 1.0) / 2.0,
                _ => f64::from(u8::from((nppm - jf).abs() > 0.0 || j % 2 == 1)),
            };
            return select(j, h);
        }
    };
    (p, false)
}

/// Whether CDO 2.6 reads one value past the end of its buffer for `n` valid values at percentile
/// `p` (continuous NumPy variants with `b < 1` near `p = 100`); there CDO's result is not a
/// percentile and cdors returns `x[n-1]` instead.
pub fn cdo_reads_past_end(n: usize, p: f64, method: PercentileMethod) -> bool {
    n > 0 && pick(n, p / 100.0, method).1
}

#[inline]
fn nth<T: Sample>(a: &mut [T], i: usize) -> f64 {
    a.select_nth_unstable_by(i, T::total_cmp).1.to_f64()
}

/// `(x[i], x[i+1])`: one selection, then the minimum of the upper partition.
#[inline]
fn nth_pair<T: Sample>(a: &mut [T], i: usize) -> (f64, f64) {
    let (_, v, upper) = a.select_nth_unstable_by(i, T::total_cmp);
    let v = v.to_f64();
    let next = upper
        .iter()
        .copied()
        .min_by(T::total_cmp)
        .map_or(v, T::to_f64);
    (v, next)
}

/// Percentile of values that are all valid (no NaN); reorders `valid`. NaN if `valid` is empty.
fn percentile_valid<T: Sample>(valid: &mut [T], q: f64, method: PercentileMethod) -> f64 {
    let n = valid.len();
    if n == 0 {
        return f64::NAN;
    }
    match pick(n, q, method).0 {
        Pick::One(i) => nth(valid, i),
        Pick::Lerp(i, d) => {
            let (vk, vk2) = nth_pair(valid, i);
            d.mul_add(vk2 - vk, vk)
        }
        Pick::Blend(i, j, h) => {
            let (a, b) = if j == i {
                let v = nth(valid, i);
                (v, v)
            } else {
                nth_pair(valid, i)
            };
            (1.0 - h).mul_add(a, h * b)
        }
    }
}

fn check_p(p: f64) {
    assert!(
        (0.0..=100.0).contains(&p),
        "percentile {p} out of range: percentiles must be in [0, 100]"
    );
}

/// Percentile `p` (in `[0, 100]`) of `values` with CDO's `method`. NaN values are missing and
/// excluded; the result is NaN if all values are missing. `values` is reordered (missing values
/// moved to the end, valid values partially ordered).
///
/// # Panics
/// If `p` is outside `[0, 100]` (CDO aborts there too).
pub fn percentile<T: Sample>(values: &mut [T], p: f64, method: PercentileMethod) -> f64 {
    check_p(p);
    let mut n = 0;
    for i in 0..values.len() {
        if !values[i].is_nan() {
            values.swap(n, i);
            n += 1;
        }
    }
    percentile_valid(&mut values[..n], p / 100.0, method)
}

/// Storage order of a tile of `ncells` cells × `nsteps` timesteps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// `data[cell * nsteps + step]`: each cell's series is contiguous.
    CellMajor,
    /// `data[step * ncells + cell]`: one field per timestep, as read from time-chunked data.
    TimeMajor,
}

/// Cells per task: the gather of a time-major tile reads `CELL_BLOCK` contiguous values per step.
const CELL_BLOCK: usize = 32;

/// Percentiles `ps` (each in `[0, 100]`) of every cell's series in a tile, in parallel over cells.
///
/// `data` holds `ncells * nsteps` values in `layout` order; NaN is missing. The result for cell `c`
/// and `ps[k]` goes to `out[c * ps.len() + k]` (NaN where the cell has no valid value). `data` is
/// not modified: each task gathers the valid values of a block of cells into a scratch buffer
/// reused across blocks, then selects order statistics in `O(nsteps)` per percentile. Results do
/// not depend on the number of threads.
///
/// # Panics
/// If a percentile is outside `[0, 100]` or a slice length does not match.
pub fn percentiles_tile<T: Sample>(
    data: &[T],
    ncells: usize,
    nsteps: usize,
    layout: Layout,
    ps: &[f64],
    method: PercentileMethod,
    out: &mut [f64],
) {
    ps.iter().for_each(|&p| check_p(p));
    let np = ps.len();
    assert_eq!(data.len(), ncells * nsteps, "tile size");
    assert_eq!(out.len(), ncells * np, "output size");
    if np == 0 || ncells == 0 {
        return;
    }
    let qs: Vec<f64> = ps.iter().map(|p| p / 100.0).collect();
    out.par_chunks_mut(CELL_BLOCK * np)
        .enumerate()
        .for_each_init(Vec::<T>::new, |scratch, (block, out_block)| {
            let c0 = block * CELL_BLOCK;
            let nb = out_block.len() / np;
            if scratch.len() < nb * nsteps {
                scratch.resize(nb * nsteps, T::default());
            }
            let mut count = [0usize; CELL_BLOCK];
            match layout {
                Layout::TimeMajor => {
                    for t in 0..nsteps {
                        let row = &data[t * ncells + c0..][..nb];
                        for (i, &v) in row.iter().enumerate() {
                            if !v.is_nan() {
                                scratch[i * nsteps + count[i]] = v;
                                count[i] += 1;
                            }
                        }
                    }
                }
                Layout::CellMajor => {
                    for (i, c) in count.iter_mut().enumerate().take(nb) {
                        let series = &data[(c0 + i) * nsteps..][..nsteps];
                        let dst = &mut scratch[i * nsteps..][..nsteps];
                        for &v in series {
                            if !v.is_nan() {
                                dst[*c] = v;
                                *c += 1;
                            }
                        }
                    }
                }
            }
            for i in 0..nb {
                let valid = &mut scratch[i * nsteps..][..count[i]];
                for (o, &q) in out_block[i * np..][..np].iter_mut().zip(&qs) {
                    *o = percentile_valid(valid, q, method);
                }
            }
        });
}
