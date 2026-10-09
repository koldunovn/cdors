# Deliberate deviations from cdo

cdors follows cdo 2.6.0 (the reference build on Levante; source references are to CDO 2.6.5)
except for the points below. Each entry says what cdo does, what cdors does, and why. The notes
that `cdors help <op>` and `cdors ops --json` print name their entry here as `[section: entry]`.
Bugs of cdo that cdors does not reproduce are listed in the last section.

## Files and outputs

- **No silent overwrite, no append.** cdo overwrites an existing output without asking
  (`src/fileStream.cc:116`), and `cat` appends to an existing output unless `-O` is given
  (`src/operators/Cat.cc:128`). cdors refuses an existing output (`output_exists`, exit code 4)
  unless `-O` is given, and never appends.
- **`-O` never deletes an existing Zarr directory.** `-O` replaces files only; an existing store
  must be removed by the user (`output_exists`).
- **An output that is also an input is refused**, also with `-O` (`bad_arguments`): the same
  file (symlinks resolved), a path inside an input Zarr store, or a name matched by an input glob
  pattern. cdo has no such check (a NetCDF-4 input it reads happens to be protected by the HDF5
  file lock: `cdi error (cdf__create): Permission denied`).
- **`mergetime` and `cat` need time to increase from one input to the next.** cdo `mergetime`
  merges its inputs timestep by timestep, so overlapping inputs interleave, and keeps repeated
  timesteps unless `skip_same_time` (`src/operators/Mergetime.cc:256-287`); cdo `cat` appends in
  argument order whatever the dates (`src/operators/Cat.cc`). cdors reads the inputs as one
  virtual dataset without copying them: `mergetime` orders the inputs by their first timestep,
  `cat` keeps the argument order, and both refuse inputs whose times overlap, repeat or go
  backwards (`bad_data`), because the downstream operators assume a monotonic time axis. Their
  inputs must be files or stores (or one glob pattern), not the output of other operators
  (`not_implemented`).

## Limits

cdo has none of these limits; cdors checks them on the plan, before any data is read or any
output is created.

- **Read limit on login nodes.** cdors refuses a run that would decode more than `--max-read`
  (default 64 GB outside Slurm jobs, no limit inside them) with `read_limit`, exit code 4;
  `--max-read none` removes the limit.
- **Memory budget.** cdo allocates what a run needs. cdors plans every run within `--mem`
  (default 60 % of the Slurm allocation, on login nodes a quarter of the available memory, at most
  4 GiB) and refuses a run that cannot fit: `memory_limit` when the state of a single lane or two
  tiles in flight do not fit, `intermediate_too_large` when the inner results of a chain alive at
  the same time exceed half of the budget (exit code 2).
- **Printed values** are limited by `--max-values` (see "Printing values: Flood guard").

## Statistics

- **Exact percentiles also above 50 values per group.** cdo computes exact percentiles only while
  a grid point holds at most 50 values and switches to a 101-bin histogram bounded by the min/max
  input files above that (`src/percentiles_hist.cc`), so its result is approximate. cdors is exact
  for any group size; above 50 values it differs from cdo's result by up to about one bin
  (`(max − min) / 101`). cdo's three-input form (`timpctl,p in -timmin in -timmax in`) is accepted,
  but the min/max inputs are never opened or computed; the one-input form `timpctl,p in` (an
  error in cdo) is accepted too.
- **Percentiles of double-precision input.** cdo stores the values of a group as float32 before
  computing percentiles (`percentiles_hist.cc:histAddValue`); cdors keeps double precision.
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
  writes it for every time statistic except percentiles and running statistics (for which cdo
  writes none either), with the CF names `standard_deviation` and `variance` for the spread
  statistics. Like cdo, it replaces an existing `cell_methods` attribute.
- **Climatology arithmetic pairs variables by name** (`ymonsub` & co.) when both inputs have the
  same variable names; cdo pairs them by position. Zarr stores have no variable order, so pairing
  by position would combine the wrong variables.

## Space statistics

- **`cell_methods` on space statistics.** cdors adds `area: mean` (and the analogous
  `longitude: …` and vertical-coordinate entries) to `fld*`, `zon*` and `vert*` output; cdo does
  not.
- **HEALPix `zon*` summation order.** cdors sums each ring in stored (nested) order, cdo in ring
  order, so the last bits can differ. The ring latitudes cdors writes differ from cdo's in the
  15th significant digit.
- **`vert*` accumulates in double precision.** cdo accumulates vertical statistics in float32;
  cdors accumulates in f64 and rounds once on output.

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
- **Timestep, year and month ranges are not expanded.** `seltimestep,1/400000000` selects the
  existing timesteps at once; cdo expands the range into a list first. Ranges of more than 1000
  members are reported as one "not found" warning instead of one per member.
- **An empty time selection is an error.** cdo warns and writes an output without timesteps;
  cdors fails (`bad_arguments`), so that a mistyped date range is not silently accepted.
- **Time values beyond about 100 million years** from the reference (typically an unmasked fill
  value such as 9.97e36) are `bad_data`, not dates.

## Reading values

- **`_Unsigned = "true"`.** CDI honours it for byte variables only (and a byte variable with
  `valid_range = 0, 255`) and does not mask the `_FillValue` of such variables (a fill value of -1
  is read as 255; `stream_cdf_i.c`). cdors reads every signed integer type with `_Unsigned =
  "true"` as unsigned, as netCDF-Java and xarray do, and compares the missing values in the stored
  bit pattern, so the fill value stays missing.
- **`valid_range`, `valid_min`, `valid_max`** are applied on read as in CDI
  (`cdf_read.c:cdfDoInputDataTransformationDP`): stored values outside the range become missing,
  before unpacking; only for variables with a missing value (`_FillValue` or `missing_value`);
  attributes whose type kind (integer or float) differs from the variable's are ignored;
  `valid_range` takes precedence. One difference: with `valid_max` alone, CDI uses `DBL_MIN` (the
  smallest positive double) as the lower bound and so also masks all values ≤ 0; cdors leaves the
  lower end open. cdors does not copy `valid_range`, `valid_min`, `valid_max` and `_Unsigned` to
  its outputs (they describe the stored input, and results such as `-addc` may lie outside the
  range); cdo copies them.
- **HEALPix grid mappings need an order.** CDI takes any `healpix_order` that does not start with
  `nest` as ring order, also a missing one; cdors accepts `nested`/`nest`/`ring`
  (`healpix_order`, or CF's `indexing_scheme`) and fails with `bad_data` otherwise, and also for
  an nside (`healpix_nside`, or CF's `refinement_level`) that is not positive, too large for the
  dimension, or not a power of two for nested order.
- **NaN is the internal missing value.** cdo carries the variable's missing value (default
  −9e33) through its computations; cdors uses NaN inside the engine and writes the variable's
  `_FillValue` on output. Results are the same unless an input contains NaN as a valid value.

## Remapping

- **Weights are generated for the unmasked grid.** cdo regenerates weights whenever the
  missing-value mask of a field changes; cdors generates them once for the unmasked grid and
  reproduces cdo's per-method missing-value rules by renormalising (see `remap/weights.rs`).
  Results agree with `cdo remap*` within 1 float32 ulp. The missing patterns are identical
  wherever cdors can tell from the unmasked weights which rule cdo applied. For `remapbil` from a
  2-D (regular or curvilinear) source grid, cdo replaces the bilinear weights of a destination
  whose search fails by a distance-weighted average of the valid neighbours (the "Bilinear
  interpolation failed" warning), which cdors reproduces by renormalising; it recognises such
  rows by a destination latitude outside the source latitudes, by four links that are not one
  quad of the source index space, or by weights that are not bilinear. Two cases remain
  undetected and give a missing value where cdo has one: a fallback whose four nearest source
  points form a quad with distances symmetric enough to look bilinear, and a destination whose
  search fails in cdo only because masked cells change the nearest source point. On the
  HadGEM3-GC31-LL ORCA1 `tos` (curvilinear, 1950–59 and 2010–14, to r360x180, r144x72, n32) the
  missing patterns are identical.
- **`hpdegrade,zoom=0`.** cdo 2.6.0 ignores `zoom=0` and keeps the input resolution; cdors
  follows cdo 2.6.5 and degrades to nside 1.
- **`--force` is accepted and ignored.** cdo needs it for `remapcon` from or to HEALPix grids;
  cdors always passes it to `cdo gencon` and `cdo genycon`.

## Printing values (`info`, `infon`, `output`, `outputf`, `outputtab`)

- **Flood guard.** cdo prints whatever it is asked to. cdors refuses, before reading, to print
  more than `--max-values` values (default 1,000,000; for `info`/`infon` the limit counts
  fields, one line each) with `too_many_values`, exit code 4; `--max-values none` removes it.
- **Any chain as input.** The printing operators take the output of any operator chain, as in
  cdo; the other information operators (`sinfo`, `showname`, `showtimestamp`, `griddes`) take
  files or stores only (`not_implemented` otherwise).
- **`--json`.** cdo has no structured form; cdors prints one JSON object (README, "Read values").
  Values of float32 variables appear there in their shortest float32 form, also the `info` mean
  (cdo's text shows 5 significant digits of the double mean).
- **Parameter IDs** (`info`, `outputtab` keys `param` and `code`). cdors derives them as CDI
  does: a `param` attribute (`"52.1.0"`), a numeric `code` (with `table`) attribute, or a name
  `var<N>`/`code<N>`/`param<N>` give the ID; the code of a GRIB2-style parameter
  (`num.cat.dis`) is −(position in the output + 1). Without these, cdo numbers NetCDF variables
  −1, −2, … in file order; cdors uses the position among the data variables of the first input,
  which matches cdo for NetCDF; Zarr stores have no variable order, so the IDs of Zarr inputs
  without a `param` attribute follow the order in which cdors lists their variables.
- **Several grids.** cdo's `output*` operators refuse a dataset whose variables are on different
  grids (`Output.cc`: "Too many different grids!"); cdors prints each variable on its own grid.
- **`outputf` formats** must hold exactly one floating-point conversion (`%[flags][width]
  [.precision](e|f|g|a)`, plus literal text and `%%`); cdo passes any string to `printf`.
- **HEALPix `x`, `y`, `xind`, `yind`** (`outputtab`): cdo 2.6.0 treats HEALPix as a projection
  grid; on a subset it prints the cell centres in degrees and `yind` 1, which cdors matches; on
  a complete HEALPix grid cdo 2.6.0 crashes (see below), cdors prints the same columns.

## cdo bugs observed (cdors does not reproduce them)

- **Percentiles near p = 100.** For the methods `hazen`, `weibull`, `median_unbiased` and
  `normal_unbiased`, cdo computes `j == n` and reads `x[n]`, one element past its buffer (zeroed
  memory), returning `(1−h)·x[n−1]`; cdors returns `x[n−1]`.
- **Partial last chunk of a Zarr store read through NCZarr.** On a single-variable view of the
  nextGEMS W1 store with 731 timesteps (the last time chunk partial), cdo returned wrong values for
  11 timesteps (up to 0.21 K); cdors and xarray agree with each other.
- **Time bounds of a time selection from a CMIP6 file.** On
  `tos_Omon_HadGEM3-GC31-LL_historical_r1i1p1f3_gn_195001-201412.nc` (360_day calendar),
  `cdo -selyear,2000` and `cdo -seltimestep,601/603` write the time bounds of the file's first
  record (36000, 36030 days since 1850-01-01, i.e. January 1950) for every selected timestep; the
  time values themselves are right. cdors writes each step's own bounds (54000–54030, …).
- **`outputtab` with coordinate keys on a complete HEALPix grid.** cdo 2.6.0 ends with a
  segmentation fault on `outputtab,lon,lat,value -selname,tas hpz2_noleap.nc` (and with `x`,
  `y`); on a `sellonlatbox` subset it works. cdors prints the cell centres.
