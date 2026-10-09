# cdors

cdors is a prototype of a Rust re-implementation of an analysis subset of
[CDO](https://code.mpimet.mpg.de/projects/cdo) (Climate Data Operators). It reads Zarr (v2 and v3),
kerchunk references and NetCDF natively and works chunk by chunk: selections decide which chunks
are read at all, consecutive operators are merged into one pass, and memory is bounded by tiling.
Its command language is CDO's: same operator names, arguments and chaining. Its primary users are
AI agents; humans come second.

It is a prototype: about 190 operators in a dozen families (`cdors ops`), tested against cdo 2.6.0
on small fixtures (`tests/`). Where cdors deliberately differs from cdo, the difference is listed
in [docs/deviations.md](docs/deviations.md).

## Building (Levante)

```sh
source env.sh                 # Rust toolchain, build dir, caches, netCDF-C/HDF5, cdo 2.6.0 on PATH
cargo build --release         # binary: $CARGO_TARGET_DIR/release/cdors
tests/make_fixtures.sh        # tiny test inputs in $CDORS_FIXTURES (once)
tests/run_cases.sh            # compare cdors with cdo, row by row (tests/cases.txt)
bench/realdata_check.sh       # report only: real-data chains vs cdo/numpy, status table + timings
```

`env.sh` keeps the toolchain, the build tree and all caches under `/work`, not in `$HOME`.
cdors links against the spack netCDF-C and the HDF5 it uses; `cdors --version` prints both.

## Usage

```sh
cdors [options] operator[,args] [-operator2[,args] ...] inputs... [output]
cdors -yearmean -selname,tas in.zarr out.nc
cdors ops                     # operators: implemented ones with arguments, all of cdo's
cdors help yearmean           # cdo's help text plus cdors notes (also: cdors -h yearmean)
cdors --plan -yearmean in.zarr   # what would be read, without reading it
```

`cdors --help` lists all options.

**Inputs:** Zarr stores, NetCDF files (NetCDF-4 chunks are read directly through a cached chunk
index; NetCDF-3 goes through netCDF-C), kerchunk references (JSON or Parquet), `http(s)://` and
`s3://` URLs of Zarr stores or kerchunk JSON, and glob patterns (`'data/*.nc'`, quoted), whose
files are sorted and concatenated along time. **Output:** the format follows the suffix (`.nc`:
NetCDF-4, `.zarr`: Zarr v3) unless `-f nc4|nc4c|nc|zarr|zarr2` says otherwise.

**Outputs never replace anything by accident.** An existing output (also a dangling symlink) is
refused (`output_exists`) unless `-O` is given, and the refusal is atomic: a file is published
with `link()`, which fails if the name appeared while cdors ran, and a Zarr store reserves its
name with an exclusive `mkdir` before writing (the name holds an empty directory during the run)
and is renamed over that still-empty directory at the end. `-O` replaces an existing file
atomically (`rename`), never a directory. An output that is an input, lies inside an input (a Zarr
store), or matches an input glob pattern is refused (`bad_arguments`), also with `-O`; symlinks
are resolved. While running, cdors writes to `.<out>.cdors-tmp-<host>-<pid>-<random>` next to the
output and, on failure, removes only that file or directory and only if it created it. Temporary
files of killed runs (`kill -9`, node failure) keep that name and are never removed automatically;
remove them by hand.

## For agents

cdors never prompts and never reads stdin. Add `--json` to any command to get machine-readable
output and errors.

**1. Find the operator.** `cdors ops --json` returns one object: `implemented_count`,
`not_implemented_count` and `operators[]`. The implemented operators come first, each with `usage`
(`seldate,startdate,[enddate=startdate]`), `inputs` (-1: any number; 3 for the percentile
operators, which also accept 1 input), `outputs` (0: prints to stdout), `args` (`name`, `type`
int/float/str, `default`, `repeated`), access `class` (selection, pointwise, reduction,
whole_extent, info), `description`, `cdo_section` and `notes` (deviations from cdo, each naming
its entry in docs/deviations.md). Every other cdo operator follows with `"implemented": false`,
`section` and `description`. `cdors --json help <op>` gives `operator`, `usage`, `inputs`,
`outputs`, `class`, `description`, `notes` and cdo's help text (`cdo_help`) for one operator.
An operator that cdors does not implement fails with `not_implemented` (exit 1): run that step
with `cdo`.

**2. Look at the input.** `cdors --json sinfo in.zarr` prints variables (dims with their role,
shape, dtype, units, chunks, codecs), grids (kind, size, lon/lat ranges), vertical axes and the
time axis (units, calendar, count, first and last timestamp). `showname`, `showtimestamp` and
`griddes` print cdo's text. These information operators take a file or store, not the output of
another operator (`not_implemented`).

**3. Plan before running.** `cdors --plan --json <chain> [output]` opens the inputs (metadata
only), plans the run and prints one JSON object without reading data or writing anything, not even
into `$CDORS_CACHE`. It exits 0 also when the run would be refused; check the `exceeded` fields
(it does not check whether the output exists).
The output may be left out (`output` is then null); a last token that is an existing file, a URL
or a glob pattern is always taken as an input, so `cdors --plan --json -yearmean -mergetime a.nc
b.nc` plans both files. Main fields (schema version `"cdors_plan": 1`):

| field | meaning |
|---|---|
| `inputs`, `output`, `format` | what is read and written |
| `stages[]` | `operators` merged into the stage in execution order, `fold_kernel` (the statistic or remapping that ends it, or null) and `fold_dim`, `passes` (how often the input is read: more than 1 when the lane states do not fit the budget and one chunk holds cells of several lane waves), `lanes`, `waves`, `tiles_in_flight`, `lane_state_bytes`, `peak_bytes_estimate`, `writes_intermediate` (inner stages of a chain: `name`, `bytes` and `chunk_shapes` of the in-memory result, else null), `tiles`, `chunks_read`, `bytes_decoded` (over all passes), `bytes_compressed`, and `variables[]` with the stored variables each reads (`leaves`: source, chunk shape, chunks read, bytes) |
| `totals` | `chunks_read`, `bytes_decoded` (exact), `bytes_compressed` (estimate from the stored sizes of a few sampled chunks; null if the store cannot tell), `passes` |
| `memory` | `budget_bytes` and `budget_source` (`--mem` or the default, see 6.), `peak_bytes_estimate` (the largest stage: tiles in flight, lane states, output held for an intermediate, the intermediates alive while it runs, remap weights) and its `parts`. The planner sizes tiles, lanes, waves and passes so that the estimate stays within the budget; a single lane that does not fit is refused (`memory_limit`) |
| `remap_weights[]` | per remapping: `generator` (`cdo gen…`), `target`, `cached`, `path` (null when `CDORS_CACHE` is not set). Uncached weights are generated by cdo on the first run, which can take minutes for large grids |
| `settings` | compute `threads`, `io_threads` (reads in flight), `tiles_in_flight`, `slurm_job` |
| `read_limit` | `limit`, its `source`, `bytes` (decoded bytes read from files, stores and URLs; reads of in-memory intermediates do not count) and `exceeded` (the run would be refused with `read_limit`) |
| `print` | only for the value-printing operators: `operator`, `values` (`fields` for `info`/`infon`), `max_values`, `exceeded` |

For a remapping, `--plan` reads cdo's target-grid template if one is cached and otherwise
describes the target grid itself: `r<nx>x<ny>`, `global_<inc>`, `hpz<zoom>[_nested|_ring]`,
`hp<nside>[_nested|_ring]` and `lon=<x>_lat=<y>` exactly as cdo makes them, grid description
files and datasets from the files. Other grid names (`F<N>`, `n<N>`, `t<N>grid`, ...) fail under
`--plan` with `unsupported_grid` until a first run has cached the template. Until the weights are
cached, the estimate counts the whole source grid; a run with cached weights reads only the
chunks holding the source cells the weights use.

A run checks in the same order before it reads anything: the output checks (existing output
without `-O`, output equal to an input), then `--max-read` and `--max-values` on the same plan
`--plan` shows. Only a run that passes them runs cdo, which on the first run for a pair of source
and target grid writes the target-grid template and the weights into `$CDORS_CACHE`.

There is no wall-time estimate. As a rough guide, cdors decodes a few GB/s on a login node from
Lustre; cdo reaches about 1 GB/s on the same data. `--plan` without `--json` prints the same
information as short text.

**4. Read values.** `info`, `infon`, `output`, `outputf,<fmt>[,nelem]` and `outputtab,<keys>`
print the values of any operator chain, as in cdo:

```sh
cdors outputtab,date,lon,lat,value -fldmean -sellonlatbox,-30,40,30,75 -selyear,2001 in.zarr
cdors infon -yearmean -fldmean in.zarr                # per field: date, level, size, missing, min, mean, max
cdors outputf,%10.4f,6 -seltimestep,1 -sellonlatbox,0,10,50,55 in.nc
```

The text output is cdo 2.6.0's, byte for byte on the test fixtures. The `outputtab` keys are
cdo's: `value`, `name`, `param`, `code`, `lon`, `lat` (degrees), `x`, `y` (stored coordinates),
`xind`, `yind`, `lev`, `timestep`, `date`, `time`, `year`, `month`, `day`, and `nohead` (no header
line); `key:width` sets a column width. With `--json` they print one JSON object instead (schema
`"cdors_values": 1`): `operator`, `variables[]` (`name`, `units`, `long_name`, `param`, `dtype`,
`gridsize`, `levels`), `records[]` and, for `outputtab`, `keys`. Dates are ISO strings, numbers
are numbers (values of float32 variables in their shortest float32 form), missing values are null:

```sh
cdors --json outputtab,date,lon,lat,value -fldmean -seltimestep,1/2 in.nc
# {"cdors_values":1,"operator":"outputtab","variables":[{"name":"tas","units":"K",...}, ...],
#  "records":[{"date":"2000-01-01","lon":0.0,"lat":0.0,"value":299.59595}, ...],
#  "keys":["date","lon","lat","value"]}
```

| operator | one record per | fields of a record |
|---|---|---|
| `info`, `infon` | field (timestep, variable, level) | `index`, `timestep`, `date` (`2000-01-01T12:00:00`), `name`, `param`, `level`, `gridsize`, `missing`, `valid`, `min`, `mean`, `max` (null when no value is valid) |
| `output`, `outputf` | field | `name`, `timestep`, `date`, `level`, `values[]` (`outputf`'s format does not apply) |
| `outputtab` | value | the requested keys: `date` `"2000-01-01"`, `time` `"12:00:00"`, `name`/`param` strings, `code`, `timestep`, `xind`, `yind`, `year`, `month`, `day` integers, the others numbers |

The values are computed by the ordinary pipeline, so the read limits and `--plan` apply. Output
piped into `head` ends quietly, as with any C tool.

**5. Errors and exit codes.** With `--json`, a failure prints one JSON object on stderr:

```json
{"error":"unknown_operator","message":"unknown operator 'yearmaen'","operator":"yearmaen",
 "suggestions":["yearmax","yearmean","yearmin"],"exit_code":1,"retryable":false,
 "hint":"did you mean yearmax or yearmean or yearmin?"}
```

`error` is a stable code; `hint` says how to fix the command; other fields depend on the error
(`operator`, `path`, `bytes`, `limit`, ...). A run that fails after it started also reports how
far it got (`stage`, `chunks_done`, `chunks_total`) and whether its own temporary output was
removed (`temporary_output_removed`); the output name never holds a partial file. A panic (a bug)
reports `internal` the same way. Warnings are one line of JSON each on stderr
(`{"warning": kind, "message": ...}`), for example for a selected year that does not exist.

| exit code | meaning | codes |
|---|---|---|
| 0 | success | |
| 1 | usage error | `unknown_operator`, `not_implemented`, `bad_arguments` (also: a time selection that selects nothing, `CDORS_CACHE` not set for a remapping), `missing_input`, `permission_denied` (also read-only file system), `no_space` (also quota exceeded) |
| 2 | data error | `bad_data`, `no_coordinates`, `unsupported_grid`, `unsupported_dimension`, `memory_limit`, `intermediate_too_large`, `io_failed` (an I/O failure not known to be transient), `internal` |
| 3 | I/O error, worth retrying | `io_error` (timeouts, connection reset or refused, EAGAIN/EINTR, HTTP 5xx and 429); only these have `"retryable": true` |
| 4 | refused | `read_limit`, `too_many_values`, `output_exists` |

**6. Limits on login nodes.** cdors tells login nodes from Slurm jobs by `SLURM_JOB_ID`. All
three limits are checked on the plan, before anything is read or written.

- `--max-read <size>`: refuses a run that would decode more bytes from files, stores and URLs
  than the limit (`read_limit`, exit 4, with `bytes`, `limit`, `limit_source`). Default **64 GB
  on login nodes, no limit inside Slurm jobs**. `--max-read 2T` raises it, `--max-read none`
  removes it.
- `--max-values <n>`: refuses to print more than n values (for `info`/`infon`: fields), so that
  printed output cannot flood the context (`too_many_values`, exit 4). Default 1,000,000;
  `--max-values 5M` or `none`. Reduce with `fldmean`, `sellonlatbox`, `seltimestep`, or write a
  file instead.
- `--mem <size>`: memory budget (e.g. `32G`). Default: 60 % of the Slurm allocation; on login
  nodes a quarter of the available memory, at most 4 GiB. A run that cannot be planned within it
  fails with `memory_limit` or `intermediate_too_large` (exit 2); the hint says what to raise or
  select.

Outside Slurm, cdors also uses at most 16 compute threads (`-P`); reads in flight default to 64
everywhere (`--io-threads`). Heavy runs belong on a compute node:

```sh
srun -p compute -A <account> -t 01:00:00 cdors -ydaymean in.zarr clim.nc
```

**7. Progress.** `--progress json` prints JSON lines on stderr: `start` (`stages`,
`chunks_total`, `bytes_decoded_total`), `progress` about once per second (`stage`, `chunks_done`,
`chunks_total`, `bytes_read` as stored, `elapsed_s`), and a final `done` line (`status`, `wall_s`,
`chunks_read`, `bytes_read`, `peak_rss_bytes`, `output`) that is useful in Slurm logs.

**8. Caches.** All caches live under `$CDORS_CACHE` (set by `env.sh` to
`/work/ab0995/a270088/cdors-cache`). Nothing in them is ever evicted; each file can be removed by
hand when it is no longer needed. `--plan` and refused runs write nothing there.

- `weights/gen<method>-<hash>.nc`: SCRIP weights from `cdo gen<method>`, keyed by the method,
  the target grid and the source grid's coordinates (so NetCDF and Zarr copies of one dataset
  share weights). Every remapping needs `CDORS_CACHE` (else `bad_arguments`), also
  `remap,<grid>,<weights.nc>`, which generates no weights. `remap<method>` also needs cdo (`$CDO`
  or `cdo` on `PATH`) until its weights are cached.
- `weights/grid-<hash>.nc`: cdo's templates of named target grids (`r360x180`, `n32`, ...);
  `weights/tgt-<hash>.nc` and `weights/src-<hash>.nc`: grid files cdors writes for cdo when a
  target or source grid comes from a dataset cdo cannot read (Zarr); `weights/tmp/`: files being
  generated.
- `nc4index/<hash>.json`: chunk indexes of NetCDF-4 files, keyed by path, size, modification
  time, inode and status-change time. Files changed less than 2 s ago are not cached (Lustre keeps
  modification times to the second).

**9. Typical chains.** Put selections innermost: they decide which chunks are read. A statistic
can feed another operator (`-sub x -timmean x`, `-fldmean -yearmean`); the inner result is kept in
memory (see known limits).

```sh
# climatology and anomalies
cdors -ymonmean -selyear,1991/2020 -selname,tas in.zarr clim.nc       # monthly climatology
cdors -ymonsub -selname,tas in.zarr clim.nc anom.nc                   # anomalies against it
cdors -ydaymean -selname,tas in.zarr dclim.nc                         # daily climatology
cdors -sub -selname,tas in.zarr -timmean -selname,tas in.zarr anom_tm.nc   # one command
# regional mean time series (area-weighted), as JSON or as a file
cdors --json outputtab,date,value -fldmean -sellonlatbox,-10,30,35,70 -selname,tas in.zarr
cdors -fldmean -sellonlatbox,-10,30,35,70 -selname,pr in.zarr europe_pr.nc
# remap to a common grid (weights from cdo gen*, cached)
cdors -remapcon,r360x180 -selname,tas in.zarr tas_1deg.nc
cdors -remapbil,target.nc -selname,tas in.zarr tas_on_target.nc      # grid of another dataset
cdors -remap,r360x180,weights.nc -selname,tas in.zarr tas_1deg.nc    # precomputed SCRIP weights
# point extraction (nearest neighbour)
cdors --json outputtab,date,value -remapnn,lon=10_lat=50 -selname,tas in.zarr
# percentiles (exact; also hourpctl, daypctl, monpctl, seaspctl; method: --percentile)
cdors -timpctl,95 -selname,tas in.zarr p95.nc
cdors -yearpctl,90 -selname,tas in.zarr p90_yearly.nc
```

cdo's three-input percentile form (`-timpctl,95 in.zarr -timmin in.zarr -timmax in.zarr p95.nc`)
is accepted; the min/max inputs are not read.

## Known limits of the prototype

- Chains with several statistics or remappings (`-fldmean -yearmean`, `-sub x -timmean x`) keep
  each inner result in memory until the stage reading it has finished; the inner results alive
  at the same time may take half of `--mem`, otherwise the run is refused before reading
  (`intermediate_too_large`).
- Information operators (`sinfo`, `showname`, `showtimestamp`, `griddes`) and `mergetime`/`cat`
  take files or stores only, not the output of another operator.
- No GRIB input; no native NetCDF-3 reader (NetCDF-3 goes through netCDF-C).
- No rechunking stage: data stored one complete field per chunk (GRIB, NetCDF written per
  timestep) is read correctly, but percentiles and daily climatologies on it whose state does
  not fit `--mem` read the input several times (`passes` in `--plan`).
- Intermediate results are never spilled to disk.
- Remapping weights and target-grid templates are generated by cdo; the first run per pair of
  grids runs cdo, later runs read the cache. Remapping needs `CDORS_CACHE` even with a given
  weight file.
- Bit-identical agreement with cdo is not a goal; results agree within a few float32 units in the
  last place (see [docs/deviations.md](docs/deviations.md)).
