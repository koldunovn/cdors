# cdors guide for agents

cdors runs CDO operators: the same names, arguments and chains as cdo. It reads Zarr stores, kerchunk references
(JSON, Parquet), NetCDF files, quoted glob patterns of files, and http(s)/s3 URLs of Zarr stores, chunk by chunk.
It never prompts and never reads stdin. Add `--json` to any command for machine-readable output and errors.
`cdors --help` lists the options; this guide is all you need for most analyses.

## Workflow

1. **Operator.** `cdors ops --json` lists the implemented operators (`usage`, `args`, `inputs`, `outputs`, `notes`
   on differences from cdo), then cdo's others with `"implemented": false`. One operator: `cdors --json help <op>`.
2. **Input.** `cdors --json sinfo <input>` gives `variables` (name, units, `dims` with roles, shape, chunks),
   `grids` (kind, size, lon/lat ranges), `zaxes` and `time` (`count`, `first`, `last`, calendar). Check `time`
   before selecting dates: daily means are often stamped 00:00 of the next day.
3. **Plan.** `cdors --plan --json <chain> [output]` reads no data. Check `read_limit.exceeded`,
   `totals.bytes_decoded` and, for printing operators, `print.exceeded`.
4. **Run.** Print values as JSON, or write a file (`.nc`: NetCDF-4, `.zarr`: Zarr):
   `cdors --json outputtab,date,value <chain>` prints
   `{"cdors_values":1, ..., "records":[{"date":"2020-07-16","value":291.88}, ...]}`.

## Syntax

- `cdors [options] -opN,args ... -op1,args input [output]`: the input sits after the innermost operator, as in
  cdo: `cdors -yearmean -fldmean -sellonlatbox,-10,40,35,70 -selname,tas in.zarr out.nc`.
- Put selections innermost: `-selname,tas`, `-selyear,2000/2009`, `-selmon,6/8`, `-selseason,JJA`,
  `-seldate,2000-01-01,2000-12-31`, `-seltimestep,1/10`, `-sellevel,500`,
  `-sellonlatbox,west,east,south,north`. They decide which chunks are read.
- Operators with two inputs take them in order: `-sub -timmean a.zarr -timmean b.zarr out.nc`.
- Several files as one input: a quoted glob (`'dir/*.nc'`), or `-mergetime a b` / `-mergetime [ a b ]`
  (files or stores only; times must not overlap).
- An existing output is refused unless `-O` is given. `--lonlat` adds lon/lat to HEALPix outputs (viewers).
- `-z zip` compresses NetCDF output (about half the size, no slower); Zarr output is always compressed.

## Recipes

```sh
cdors -ymonmean -selyear,1991/2020 -selname,tas in.zarr clim.nc                 # monthly climatology
cdors -ymonsub -selname,tas in.zarr clim.nc anom.nc                             # anomalies
cdors --json outputtab,date,value -fldmean -sellonlatbox,-10,30,35,70 -selname,tas in.zarr   # regional mean
cdors --json outputtab,date,value -mulc,86400 -monmean -fldmean -selname,pr in.zarr   # kg m-2 s-1 -> mm/day
cdors --json outputtab,date,value -remapnn,lon=10_lat=53.55 -selname,tas in.zarr     # nearest point
cdors -timpctl,95 -selyear,2020 -selname,tas in.zarr p95.nc                     # exact percentile
cdors -remapbil,r360x180 -timmean -selname,tos in.nc tos_1deg.nc                # to a 1 degree grid
cdors -sub -timmean -selyear,2040/2049 b.zarr -timmean -selyear,1990/1999 a.zarr dT.nc   # change map
```

Other families: `yearmean`, `seasmean`, `daymean`, `timstd`, `runmean`, `ydaymean` (all with min, max, sum, var,
std, ...); `zonmean`, `vertmean`; `remapcon`, `remapdis`, `hpdegrade`. Remapping targets: `r<nx>x<ny>`,
`global_<deg>`, `hpz<zoom>`, `lon=<x>_lat=<y>`, a grid description file, or another dataset. The first remapping
between two grids runs cdo once to make the weights (cached in `$CDORS_CACHE`).

## Output shapes

- `outputtab,<keys>` keys: `date`, `time`, `value`, `name`, `lon`, `lat`, `lev`, `timestep`, `year`, `month`, `day`.
  With `--json`: `records[]` with these keys; dates are ISO strings, missing values `null`.
- `infon` with `--json`: one record per field with `date`, `name`, `level`, `min`, `mean`, `max`, `missing`.

## Errors

With `--json` a failure prints one object on stderr: `{"error": code, "message", "hint", "exit_code",
"retryable"}`. Follow the `hint`.

| Exit | Codes | What to do |
|---|---|---|
| 1 | `unknown_operator` (see `suggestions`), `bad_arguments`, `missing_input` | fix the command |
| 1 | `not_implemented` | the operator exists only in cdo: run that step with cdo |
| 2 | `bad_data`, `unsupported_grid`, `memory_limit`, `no_coordinates` | select less, raise `--mem`, or check the input |
| 3 | `io_error` (`"retryable": true`) | retry |
| 4 | `read_limit`, `too_many_values`, `output_exists` | run on a compute node, reduce what is printed, or add `-O` |

## Limits

Outside Slurm jobs cdors uses at most 16 threads, refuses runs that decode more than 64 GB (`--max-read`), and
plans within at most 4 GiB (`--mem`). Inside a job (`srun`, `salloc -p interactive`) it uses all allocated cores and
has no read limit. Printing operators stop at 1,000,000 values (`--max-values`): reduce first or write a file.

## Differences from cdo that change results

- Percentiles are exact for any number of values; cdo approximates above 50 values per cell. The three-input form
  `timpctl,p in -timmin in -timmax in` is accepted.
- Integer and packed variables are written as float64; cdo keeps integers.
- A time selection that selects nothing is an error, as in cdo.
- `sinfo`, `showname`, `showtimestamp` and `griddes` take files or stores, not chains.
- cdo options that cdors lacks (`-k`, `-r`, ...) are refused. `-z zstd` works for Zarr outputs only.
- Grids without cell bounds (HEALPix stores) get equal area weights, with a warning, as in cdo; HEALPix cells have
  equal areas.
