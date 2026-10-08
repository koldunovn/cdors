# Deliberate deviations from cdo

cdors follows cdo 2.6.0 (the reference build on Levante; source references are to CDO 2.6.5)
except for the points below. Each entry says what cdo does, what cdors does, and why.

## Files and outputs

- **No silent overwrite, no append.** cdo overwrites an existing output without asking
  (`src/fileStream.cc:116`), and `cat` appends to an existing output unless `-O` is given
  (`src/operators/Cat.cc:128`). cdors refuses an existing output (`output_exists`, exit code 4)
  unless `-O` is given, and never appends.
- **`-O` never deletes an existing Zarr directory.** `-O` replaces files only; an existing store
  must be removed by the user.
- **Read limit on login nodes.** cdo has none. cdors refuses a run that would decode more than
  `--max-read` (default 64 GB outside Slurm jobs, no limit inside them) with `read_limit`, exit
  code 4, before any output is created; `--max-read none` removes the limit.

## Statistics

- **Exact percentiles also above 50 values per group.** cdo computes exact percentiles only while
  a grid point holds at most 50 values and switches to a 101-bin histogram bounded by the min/max
  input files above that (`src/percentiles_hist.cc`), so its result is approximate. cdors is exact
  for any group size. cdo's three-input form (`timpctl,p in -timmin in -timmax in`) is accepted;
  the min/max inputs are ignored.
- **Percentile methods `hazen`, `weibull`, `median_unbiased`, `normal_unbiased` near p = 100.**
  cdo reads `x[n]`, one element past its buffer (zeroed memory), and returns `(1−h)·x[n−1]`;
  cdors returns `x[n−1]`.
- **Variance and standard deviation** (`*var`, `*var1`, `*std`, `*std1`). cdo accumulates Σx and
  Σx² and computes `(Σx² − (Σx)²/n) / (n − d)` at the end (`src/field2.cc:field2_var`,
  `fieldc_var`), clamping results in (−1e-5, 0) to 0 and setting negative results to missing. This
  one-pass formula loses precision by cancellation when the mean is large compared with the spread
  (e.g. temperatures in K, pressures in Pa). cdors uses Welford's update (running mean and sum of
  squared deviations, folded one timestep at a time in time order, so no merge formula is needed);
  the result is never negative. On the test fixtures both agree to the float32 output in all but a
  few values, which differ by one unit in the last place.
- **`cell_methods`.** cdo (through CDI) writes `cell_methods = "time: <method>"` only for `mean`,
  `avg`, `sum`, `range`, `min` and `max` statistics, and only when the output has time bounds
  (`libcdi/src/cdf_write.c:cdfDefineCellMethods`), so not for `*std`, `*var` and `yseas*`. cdors
  writes it for every time statistic, with the CF names `standard_deviation` and `variance` for
  the spread statistics. Like cdo, it replaces an existing `cell_methods` attribute.

## Time axis

- **`yseas*` timestamps follow cdo 2.6.0.** 2.6.0's `Yseasstat` stamps each season with the latest
  member date, counting December as December of the previous year, and ignores
  `--timestat_date`; 2.6.5 moved `yseas*` into `Ymonstat.cc` and uses the `last` rule. cdors
  matches 2.6.0, the reference build (details in `model/timegroup.rs`).
- **Time statistics on inputs with `months since` or `years since` units.** CDI encodes timestamps
  in such units with month-length rules that do not round-trip (`taxis.c:datetime2rtimeval`;
  2001-07-01 00:00 in `months since 2001-01-16` reads back as 2001-07-01 11:36:46). cdors writes
  the output time axis of a time statistic as `days since <the same reference>` instead, so the
  stored timestamps are exactly the group timestamps; `showtimestamp` therefore differs from cdo's
  output for such inputs.
- **Real Julian calendar.** CDI treats `julian` like `proleptic_gregorian` (cdo warns and falls
  back); cdors uses the Julian calendar (a leap year every four years).
- **An empty time selection is an error.** cdo warns and writes an output without timesteps;
  cdors fails, so that a mistyped date range is not silently accepted.

## Values

- **NaN is the internal missing value.** cdo carries the variable's missing value (default
  −9e33) through its computations; cdors uses NaN inside the engine and writes the variable's
  `_FillValue` on output. Results are the same unless an input contains NaN as a valid value.

## Space statistics

- **`cell_methods` on space statistics.** cdors adds `area: mean` (and the analogous
  `longitude: …` / `latitude: …` / vertical-coordinate entries) to `fld*`, `zon*`, `mer*` and
  `vert*` output; cdo does not.
- **HEALPix `zon*` summation order.** cdors sums each ring in stored (nested) order, cdo in ring
  order, so the last bits can differ. The ring latitudes cdors writes differ from cdo's in the
  15th significant digit.
- **`vert*` accumulates in double precision.** cdo accumulates vertical statistics in float32;
  cdors accumulates in f64 and rounds once on output.

## Remapping

- **Weights are generated for the unmasked grid.** cdo regenerates weights whenever the
  missing-value mask of a field changes; cdors generates them once for the unmasked grid and
  reproduces cdo's per-method missing-value rules by renormalising (see `remap/weights.rs`).
  Results agree with `cdo remap*` within 1 float32 ulp with identical missing patterns.
- **`hpdegrade,zoom=0`.** cdo 2.6.0 ignores `zoom=0` and keeps the input resolution; cdors
  follows cdo 2.6.5 and degrades to nside 1.
- **`--force` is accepted and ignored.** cdo needs it for `remapcon` from or to HEALPix grids;
  cdors always passes it to `cdo gencon`.

## cdo bugs observed (cdors does not reproduce them)

- **Percentiles near p = 100.** For the methods hazen, weibull, median_unbiased and
  normal_unbiased, cdo computes `j == n` and reads `x[n]` past its buffer (zeroed memory),
  returning `(1−h)·x[n−1]`; cdors returns `x[n−1]`.
- **Partial last chunk of a Zarr store read through NCZarr.** On a single-variable view of the
  nextGEMS W1 store with 731 timesteps (the last time chunk partial), cdo returned wrong values for
  11 timesteps (up to 0.21 K); cdors and xarray agree with each other.
