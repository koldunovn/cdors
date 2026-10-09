# cdors

cdors is a prototype of a Rust re-implementation of an analysis subset of
[CDO](https://code.mpimet.mpg.de/projects/cdo) (Climate Data Operators). It reads Zarr (v2 and v3),
kerchunk references and NetCDF natively and works chunk by chunk: selections decide which chunks
are read at all, consecutive operators are merged into one pass, and memory is bounded by tiling.
Its command language is CDO's: same operator names, arguments and chaining. Its primary users are
AI agents; humans come second.

It is a prototype: about a dozen operator families, tested against cdo 2.6.0 on small fixtures
(`tests/`). Where cdors deliberately differs from cdo, the difference is listed in
[docs/deviations.md](docs/deviations.md).

## Building (Levante)

```sh
source env.sh                 # Rust toolchain, build dir, caches, netCDF-C/HDF5, cdo 2.6.0 on PATH
cargo build --release         # binary: $CARGO_TARGET_DIR/release/cdors
tests/make_fixtures.sh        # tiny test inputs (once)
tests/run_cases.sh            # compare cdors with cdo, row by row (tests/cases.txt)
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

Inputs: Zarr stores, NetCDF files (NetCDF-4 chunks are read directly through a cached chunk
index), kerchunk references (JSON or Parquet), `http(s)://` URLs of Zarr stores, and glob patterns
(`'data/*.nc'`, quoted), which are concatenated along time. The output format follows the suffix
(`.nc`: NetCDF-4, `.zarr`: Zarr v3) unless `-f` says otherwise.

**Outputs never replace anything by accident.** An existing output (also a dangling symlink) is
refused unless `-O` is given, and the refusal is atomic: a file is published with `link()`, which
fails if the name appeared while cdors ran, and a Zarr store reserves its name with an exclusive
`mkdir` before writing (the name holds an empty directory during the run) and is renamed over
that still-empty directory at the end. `-O` replaces an existing file atomically (`rename`), never
a directory. An output that is an input, lies inside an input (a Zarr store), or matches an input
glob pattern is refused (`bad_arguments`), also with `-O`; symlinks are resolved. While running,
cdors writes to `.<out>.cdors-tmp-<host>-<pid>-<random>` next to the output and, on failure,
removes only that file or directory and only if it created it. Temporary files of killed runs
(`kill -9`, node failure) keep that name and are never removed automatically by other runs; remove
them by hand.

`cdors --help` lists all options.

## For agents

**Discover operators.** `cdors ops --json` returns one JSON object. Its `operators` list holds
every implemented operator first, with `usage`, `inputs` (-1: any number), `outputs` (0:
prints to stdout), `args` (`name`, `type` int/float/str, `default`, `repeated`), access `class`
(selection, pointwise, reduction, whole_extent, info), `description` and `notes` (deviations from
cdo), followed by every other cdo operator with `"implemented": false`, its cdo `section` and
`description`. `cdors --json help <op>` gives cdo's help text (`cdo_help`) and the notes. A cdo
operator that cdors does not implement fails with `not_implemented`: run that step with `cdo`.

**Plan before running.** `cdors --plan --json <chain> [output]` opens the inputs (metadata only),
plans the run and prints one JSON object without reading data or writing anything. The output
file may be left out (`output` is then null); a last token that is an existing file, a URL or a
glob pattern is always taken as an input, so `cdors --plan --json -yearmean -mergetime a.nc b.nc`
plans both files. Main fields (schema version `"cdors_plan": 1`):

| field | meaning |
|---|---|
| `stages[]` | `operators` merged into the stage in execution order, `fold_kernel` (the statistic or remapping that ends it, or null), `passes` (how often the input is read: more than 1 when the lane states do not fit the budget and one chunk holds cells of several lane waves), `lanes`, `waves` (`lane_major`), `tiles_in_flight`, `lane_state_bytes`, `peak_bytes_estimate`, `writes_intermediate` (inner stages of a chain: name, bytes and chunks of the in-memory result), `tiles`, `chunks_read`, `bytes_decoded` (over all passes), `bytes_compressed`, and per variable the stored variables (`leaves`) it reads |
| `totals` | `chunks_read`, `bytes_decoded` (exact), `bytes_compressed` (estimate: stored sizes of a few sampled chunks scaled to all; null if the store cannot tell), `passes` |
| `memory` | `budget_bytes` (`--mem`, or the default: 60 % of the Slurm allocation, on login nodes a quarter of the available memory, at most 4 GiB), `peak_bytes_estimate` (the largest stage, with tiles in flight, lane states and output held for an intermediate, plus the intermediates alive while it runs, plus remap weights) and its `parts`. The planner sizes the tile window, the lanes, their waves and the passes so that the estimate stays within the budget; a single lane that does not fit is refused (`memory_limit`) |
| `remap_weights[]` | per remapping: `generator` (`cdo gen…`), `cached`, `path` (null when `CDORS_CACHE` is not set); uncached weights are generated by cdo on first use, which can take minutes for large grids |
| `settings` | compute `threads`, `io_threads` (reads in flight), `tiles_in_flight`, `slurm_job` |
| `read_limit` | `limit`, its `source`, `bytes` (decoded bytes read from files, stores and URLs; reads of in-memory intermediates do not count) and `exceeded` (the run would be refused) |
| `print` | only for the value-printing operators: `operator`, `values` (or `fields` for `info`/`infon`), `max_values`, `exceeded` |

`--plan` never runs cdo and writes nothing, not even into `$CDORS_CACHE` (which it does not
need). For a remapping it reads cdo's target-grid template if one is cached, and otherwise
describes the target grid itself: `r<nx>x<ny>`, `global_<inc>`, `hpz<zoom>[_nested|_ring]`,
`hp<nside>[_nested|_ring]` and `lon=<x>_lat=<y>` exactly as cdo makes them, grid description
files and datasets from the files. Other grid names (`F<N>`, `N<N>`, `t<N>grid`, ...) are
refused by `--plan` until a first run has cached the template. Until the weights are cached,
the estimate counts the whole source grid; a run with cached weights reads only the chunks
holding the source cells the weights use.

A run applies the same order: the output checks (existing output without `-O`, output equal to
an input), `--max-read` and `--max-values` are evaluated on a plan made without cdo, the
estimate `--plan` shows. Only a run that passes them runs cdo. The first run per pair of source
and target grid then has cdo write the target-grid template and the weights (`cdo gen<method>`,
which can take minutes for large grids) into `$CDORS_CACHE`; later runs reuse them.

There is no wall-time estimate. As a rough guide, cdors decodes a few GB/s on a login node from
Lustre; cdo reaches about 1 GB/s on the same data. `--plan` without `--json` prints the same
information as short text.

**Print values.** `info`, `infon`, `output`, `outputf,<fmt>[,nelem]` and
`outputtab,<keys>` print the values of any operator chain, as in cdo:

```sh
cdors outputtab,date,lon,lat,value -fldmean -sellonlatbox,-30,40,30,75 -selyear,2001 in.zarr
cdors infon -yearmean -fldmean in.zarr                # per field: date, level, size, missing, min, mean, max
cdors outputf,%10.4f,6 -seltimestep,1 -sellonlatbox,0,10,50,55 in.nc
```

The text output is cdo 2.6.0's, byte for byte on the test fixtures. The `outputtab` keys are
cdo's: `value`, `name`, `param`, `code`, `lon`, `lat` (degrees), `x`, `y` (stored coordinates),
`xind`, `yind`, `lev`, `timestep`, `date`, `time`, `year`, `month`, `day`, and `nohead` (no header
line); `key:width` sets a column width. With `--json` they print one JSON object (schema
`"cdors_values": 1`) instead: `operator`, `variables[]` (`name`, `units`, `long_name`, `param`,
`dtype`, `gridsize`, `levels`) and `records[]`. Dates are ISO strings, numbers are numbers (values
of float32 variables in their shortest float32 form), missing values are null:

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

The values are computed by the ordinary pipeline, so the read limits and `--plan` apply (`--plan`
adds a `print` object with the number of values). To keep an agent's context from flooding,
cdors refuses to print more than `--max-values` values (default 1,000,000; for `info`/`infon`:
fields) before reading anything: `too_many_values`, exit code 4, with the hint to reduce the data
with `fldmean`/`sellonlatbox`/`seltimestep` or to write it to a file. `--max-values none` removes
the limit. Output piped into `head` ends quietly, as with any C tool.

**Errors and exit codes.** With `--json`, a failure prints one JSON object on stderr:

```json
{"error":"unknown_operator","message":"unknown operator 'yearmaen'","operator":"yearmaen",
 "suggestions":["yearmax","yearmean","yearmin"],"exit_code":1,"retryable":false,
 "hint":"did you mean yearmax or yearmean or yearmin?"}
```

`error` is a stable code; `hint` says how to fix the command; other fields depend on the error.
A run that fails after it started also reports how far it got (`stage`, `chunks_done`,
`chunks_total`) and whether its own temporary output was removed (`temporary_output_removed`);
the output name never holds a partial file. A panic (a bug) reports `internal` the same way and
also removes the temporary output. Warnings are one line of JSON each under `--json`
(`{"warning": kind, "message": ...}`).

| exit code | meaning | codes |
|---|---|---|
| 0 | success | |
| 1 | usage error | `unknown_operator`, `not_implemented`, `bad_arguments`, `missing_input`, `permission_denied` (also read-only file system), `no_space` (also quota exceeded) |
| 2 | data error | `no_coordinates`, `unsupported_grid`, `unsupported_dimension`, `intermediate_too_large`, `memory_limit`, `bad_data`, `io_failed` (an I/O failure not known to be transient), `internal` |
| 3 | I/O error, worth retrying | `io_error` (timeouts, connection reset or refused, EAGAIN/EINTR, HTTP 5xx and 429); only these have `"retryable": true` |
| 4 | refused | `read_limit`, `too_many_values`, `output_exists` |

**Limits on login nodes.** `--max-read <size>` refuses a run whose decoded bytes to read from
files, stores and URLs exceed the limit (`read_limit`, exit 4, with `bytes`, `limit` and a hint). The default is **64 GB on
login nodes** (no `SLURM_JOB_ID` in the environment) and **no limit inside Slurm jobs**.
`--max-read 2T` raises it, `--max-read none` removes it. Outside Slurm, cdors also uses at most 16
compute threads (and 64 reads in flight, as in Slurm jobs). Heavy runs belong on a compute node:

```sh
srun -p compute -A <account> -t 01:00:00 cdors -ydaymean in.zarr clim.nc
```

cdors never prompts and never reads stdin.

**Progress.** `--progress json` prints JSON lines on stderr: `start` (chunks and bytes to read),
`progress` about once per second (`stage`, `chunks_done`, `chunks_total`, `bytes_read` as stored,
`elapsed_s`), and a final `done` line (`status`, `wall_s`, `bytes_read`, `peak_rss_bytes`) that is
useful in Slurm logs.

**Typical chains.** Put selections innermost: they decide which chunks are read.

```sh
# climatologies and anomalies
cdors -ymonmean -selyear,1991/2020 -selname,tas in.zarr clim.nc
cdors -yearmean -selname,tas in.zarr yearly.nc
cdors -timmean -selname,tas in.zarr tmean.nc
cdors -sub -selname,tas in.zarr tmean.nc anom.nc    # one-timestep input is broadcast, as in cdo
# regional means (area-weighted)
cdors -fldmean -sellonlatbox,-10,30,35,70 -selname,pr in.zarr europe_pr.nc
# remapping to a common grid (weights from cdo gen*, cached)
cdors -remapcon,r360x180 -selname,tas in.zarr tas_1deg.nc
cdors -remap,r360x180,weights.nc in.zarr out.nc
# percentiles (check `cdors ops` for the percentile operators of your build)
cdors -timpctl,95 in.zarr -timmin in.zarr -timmax in.zarr p95.nc
```

A statistic can feed another operator (`-sub x -timmean x`, `-fldmean -yearmean`); the inner
result is kept in memory (see known limits).

## Caches

All caches live under `$CDORS_CACHE` (set by `env.sh` to `/work/ab0995/a270088/cdors-cache`).
Nothing in them is ever evicted; each file can be removed by hand when it is no longer needed.

- `weights/`: SCRIP weight files from `cdo gen<method>`, named by a hash of the method, the
  target grid and the source grid's coordinates (so NetCDF and Zarr copies of one dataset share
  weights); also target-grid templates (`grid-<hash>.nc`) and source-grid files (`src-<hash>.nc`).
- `nc4index/`: chunk indexes of NetCDF-4 files, keyed by path, size, modification time, inode
  and status-change time. Files changed less than 2 s ago are not cached (Lustre keeps
  modification times to the second). Written only by runs that pass their checks, never by
  `--plan`.

## Known limits of the prototype

- Chains with several statistics or remappings (`-fldmean -yearmean`, `-sub x -timmean x`) keep
  each inner result in memory until the stage reading it has finished; the inner results alive
  at the same time may take half of `--mem`, otherwise the run is refused before reading
  (`intermediate_too_large`).
- No GRIB input; no native NetCDF-3 reader (NetCDF-3 goes through netCDF-C).
- No rechunking stage: data stored one complete field per chunk (GRIB, NetCDF written per
  timestep) is read correctly, but percentiles and daily climatologies on it whose state does
  not fit `--mem` read the input several times (`passes` in `--plan`).
- Intermediate results are never spilled to disk.
- Remapping weights and target-grid templates are generated by cdo (`cdo` must be available
  and `CDORS_CACHE` set, or pass weights with `remap,<grid>,<weights.nc>`); the first run per
  pair of grids runs cdo, later runs read the cache.
- Bit-identical agreement with cdo is not a goal; results agree within a few float32 units in the
  last place (see [docs/deviations.md](docs/deviations.md)).
