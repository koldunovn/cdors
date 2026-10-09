# cdors on Levante

**cdors runs CDO's analysis operators, many times faster.** You write the same commands as for cdo: the same
operator names, arguments and chains. cdors reads Zarr stores, kerchunk references and NetCDF files chunk by chunk
and in parallel, and it reads only the chunks a command needs. It also reads the EERIE cloud, which cdo cannot open,
and it tells you before a run what it will read and how much memory it needs. It is a prototype (version 0.1.0,
October 2026): 187 operators, each compared with cdo 2.6.0.

The same command with cdo and with cdors, wall time on one Levante compute node:

| Command | Data | cdo | cdors | Faster |
|---|---|---|---|---|
| `ydaymean`, 30 years of 3-hourly global data | 1.1 TB | 1 h 35 min | 79 s | **72×** |
| `timpctl,95`, one year of 3-hourly global data | 38 GB | 6 min 56 s | 8.6 s | **49×** |
| `fldmean` of a region, 240 NetCDF files | 15 GB | 38.5 s | 3.6 s | **11×** |
| `remap` to 1°, 120 NetCDF files | 7.6 GB | 23.7 s | 2.8 s | **8.6×** |
| `yearmean`, a decade of daily global data | 46 GB | 42.8 s | 7.8 s | **5.5×** |

Global data: the nextGEMS ICON run on a HEALPix grid of 3.1 million cells. The results agree with cdo's. cdors
read cold data, and cdo mostly read data that cdors had just read, which favours cdo. On tiny files cdo is faster,
because cdors needs 0.2–0.3 s to start. Details: `doc/bench-results.md`.

## Try it

One line makes `cdors` available. The example after it prints the July mean of 2 m temperature over Europe
(10°W–40°E, 35–70°N), area-weighted, for five years, from 30 years of daily global data:

```bash
export PATH=/work/ab0995/a270088/cdors/bin:$PATH     # add to ~/.bashrc to keep it

D=/work/kd1453/rechunked_ngc4008/ngc4008_P1D_9.zarr
cdors outputtab,date,value -yearmean -fldmean -sellonlatbox,-10,40,35,70 -selmon,7 -selyear,2020/2024 -selname,tas $D
```
```
#      date    value
 2020-07-16 291.8761
 2021-07-16 292.4609
 2022-07-16 292.7205
 2023-07-16 292.4945
 2024-07-16 292.9499
```

That took 0.6 s on a login node. The dataset is the nextGEMS ICON run `ngc4008`: daily means on a HEALPix grid at
zoom 9, 3,145,728 cells per field, 2020–2049. Of the 17,568 chunks of `tas`, cdors read the 60 that cover Europe in
July of these five years (0.47 GB).

Every command in this guide was run on a Levante login node on 2026-10-09, and the outputs are copied from those
runs.

## Example 1: look at a dataset

```bash
D=/work/kd1453/rechunked_ngc4008/ngc4008_P1D_9.zarr
cdors showname $D
cdors sinfo $D
```
```
   File format : Zarr v2
    -1 : T Levels    Points Dtype   Chunks               : Parameter name
     1 : v     73   3145728 float32 7x11x65536           : A_tracer_v_to
     2 : v      1   3145728 float32 30x65536             : FrshFlux_IceSalt
   ...
   Grid coordinates :
     1 : healpix                  : points=3145728 nside=512 order=Nested
   Vertical coordinates :
     1 : depth_half               : levels=73 units=m
   ...
   Time coordinate :
                             time : 10958 steps
     Units = seconds since 1970-01-01  Calendar = proleptic_gregorian
     First = 2020-01-02T00:00:00  Last = 2050-01-01T00:00:00
```

The same as JSON, for one variable:

```bash
cdors --json sinfo $D | jq '.variables[] | select(.name=="tas") | {units, shape, chunks, codecs}'
```
```json
{
  "units": "K",
  "shape": [10958, 3145728],
  "chunks": [30, 65536],
  "codecs": "blosc:lz4"
}
```

## Example 2: plan first, then run

A monthly climatology of 2 m temperature over ten years:

```bash
cdors --plan -ymonmean -selyear,2020/2029 -selname,tas $D tas_ymonmean.nc
```
```
inputs: /work/kd1453/rechunked_ngc4008/ngc4008_P1D_9.zarr
output: tas_ymonmean.nc (nc4)
stage 1 of 1: selname,tas -> selyear,2020/2029 -> ymonmean, folded by ymonmean along time, 1 pass(es)
  read: 5856 chunks, 46.1 GB decoded, ~25.7 GB stored (estimate from 8 chunks); 5856 tiles
memory: ~2.9 GB peak of 4.3 GB budget (...)
settings: 16 compute threads, 64 reads in flight (login node)
read limit: 64.0 GB (login-node default, no SLURM_JOB_ID)
```

46 GB is under the login node's limit of 64 GB, so it can run here:

```bash
time cdors -ymonmean -selyear,2020/2029 -selname,tas $D tas_ymonmean.nc     # 5.8 s, 1.3 GB of memory
cdors infon tas_ymonmean.nc
```
```
    -1 :       Date     Time   Level Gridsize    Miss :     Minimum        Mean     Maximum : Parameter name
     1 : 2029-01-31 00:00:00       0  3145728       0 :      226.34      284.85      304.57 : tas
     2 : 2029-02-28 00:00:00       0  3145728       0 :      226.70      285.21      306.14 : tas
   ...
```

The output is ordinary CF NetCDF, with `time_bnds` and `cell_methods = "time: mean"`. cdo and xarray read it.

`info`, `infon`, `output`, `outputf` and `outputtab` print the result of any chain, in cdo's format. To keep a run
from flooding your terminal, cdors refuses to print more than a million values (`--max-values`).

## Example 3: one point, a 95th percentile

The 95th percentile of 3-hourly 2 m temperature at Hamburg in 2020, from the 3-hourly store (1.1 TB in all):

```bash
H=/work/kd1453/rechunked_ngc4008/ngc4008_PT3H_9.zarr
cdors outputtab,date,lon,lat,value -timpctl,95 -remapnn,lon=10_lat=53.55 -selyear,2020 -selname,tas $H
```
```
#      date    lon    lat    value
 2020-07-02     10  53.55   292.12
```

That takes 2.3 s. `--plan` shows why it is fast: of the 67968 chunks of `tas`, cdors reads the 12 that hold this
cell in 2020 (195 MB). cdors computes percentiles exactly, for any number of values. cdo switches to an
approximation above 50 values per cell (`doc/deviations.md`).

## Example 4: EERIE data — kerchunk references, raw NetCDF files, the cloud

The ICON-ESM-ER control run (`eerie-control-1950`), daily means on a 0.25° grid. The same data can be read in three
ways.

**Kerchunk references** (Parquet, from the DKRZ EERIE catalog) read the raw files' chunks directly:

```bash
K=/work/bm1344/k202193/Kerchunk/erc2002/control_1950/v20240618/atm_2d_1d_mean_remap025.parq
cdors outputtab,date,value -mulc,86400 -monmean -fldmean -sellonlatbox,-60,0,20,60 -selyear,1950 -selname,pr $K
```
```
#      date    value
 1950-01-16 3.164306
 1950-02-15 3.216423
 ...
 1950-12-16 3.669047
```

That is North Atlantic precipitation in mm/day for each month of 1950, in 1.8 s.

**Raw NetCDF files.** A quoted glob pattern is one input: cdors sorts the files and joins them along time.

```bash
R=/work/bm1344/k202193/ICON/erc2002/postprocessing/interpolation/control_1950/atm_2d_1d_mean_remap025
cdors -fldmean -sellonlatbox,-60,0,20,60 -selname,pr "$R/run_199[1-5]*/*.nc" pr_natl.nc
```

That is 120 files, five years of daily data, 7.6 GB to decode. The first run took 21 s: cdors opens each file once
through netCDF-C and keeps a chunk index in `$CDORS_CACHE`. Later runs took 2.6 s. `--plan` does not write the index,
so a `--plan` before the first run is slow too (35 s here). These raw files carry the model's own years: 1991 in
the files is 1950 in the catalog and the kerchunk references.

**The EERIE cloud** over HTTPS. It is the same dataset, read from anywhere, also outside Levante:

```bash
U=https://eerie.cloud.dkrz.de/datasets/icon-esm-er.eerie-control-1950.v20240618.atmos.gr025.2d_daily_mean/kerchunk
cdors sinfo $U
cdors outputtab,date,value -timmean -fldmean -sellonlatbox,-10,40,35,70 -selmon,1 -selyear,1950 -selname,tas $U
```
```
#      date    value
 1950-01-16 274.6871
```

That takes 1.8 s from the cloud and 0.8 s from the kerchunk references, with the same value. The server delivers
about 0.2 GB/s to any client, so tens of GB take minutes. The dataset list is at
`https://eerie.cloud.dkrz.de/datasets`. cdo cannot read these URLs.

## Example 5: remap to a regular grid

HadGEM3 (CMIP6) sea surface temperature on the curvilinear ORCA1 grid, the 2000–2014 mean on a 1° grid:

```bash
F=/work/ik1017/CMIP6/data/CMIP6/CMIP/MOHC/HadGEM3-GC31-LL/historical/r1i1p1f3/Omon/tos/gn/v20190624/tos_Omon_HadGEM3-GC31-LL_historical_r1i1p1f3_gn_195001-201412.nc
cdors -remapbil,r360x180 -timmean -selyear,2000/2014 $F tos_clim_1deg.nc
```

The first run took 3.9 s, because cdo made the weights. cdors keeps them in `$CDORS_CACHE`, and the next run took
0.5 s. `remapnn`, `remapdis`, `remapbil`, `remapcon` and `remapycon` work with cdo's grid names (`r360x180`,
`global_1`, `hpz7`, `lon=10_lat=53.55`, ...), with grid description files and with the grid of another dataset.
`remap,<grid>,<weights.nc>` uses weights you made yourself. Missing values (land) are treated as cdo treats them,
with two rare exceptions for `remapbil` (`doc/deviations.md`, Remapping).

## Example 6: scripts and agents

Every command takes `--json`. Values come as one JSON object, and errors as one JSON object on stderr, with a stable
code and a hint:

```bash
cdors --json outputtab,date,value -fldmean -seltimestep,1/3 -selname,tas $D | jq -c '.records'
```
```json
[{"date":"2020-01-02","value":286.30957},{"date":"2020-01-03","value":286.11243},{"date":"2020-01-04","value":285.9965}]
```
```bash
cdors --json -yearmaen in.nc out.nc
```
```json
{"error":"unknown_operator","message":"unknown operator 'yearmaen'","operator":"yearmaen","suggestions":["yearmax","yearmean","yearmin"],"exit_code":1,"retryable":false,"hint":"did you mean yearmax or yearmean or yearmin?"}
```

From Python (3.7 or newer, e.g. `module load python3`):

```python
import json, subprocess

D = "/work/kd1453/rechunked_ngc4008/ngc4008_P1D_9.zarr"
cmd = ["cdors", "--json", "outputtab,date,value", "-fldmean", "-sellonlatbox,-10,40,35,70",
       "-selmon,7", "-selyear,2020", "-selname,tas", D]
r = subprocess.run(cmd, capture_output=True, text=True)
if r.returncode != 0:
    raise RuntimeError(json.loads(r.stderr.splitlines()[-1])["message"])
records = json.loads(r.stdout)["records"]    # 31 records: {'date': '2020-07-01', 'value': 291.0806}, ...
```

Exit codes:

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | the command is wrong (unknown operator, bad arguments, missing input, ...) |
| 2 | the data are the problem (bad data, unsupported grid, memory limit, ...) |
| 3 | an I/O error worth retrying (timeouts, HTTP 5xx) |
| 4 | refused (read limit, too many values, existing output) |

AI agents (Claude Code, Codex, ...) use cdors well when they are pointed at `doc/reference.md`. Its section "For
agents" describes `--plan --json`, `cdors ops --json` and the error codes. In a test, agents answered five of five
analysis questions correctly with cdors (`doc/criteria.md`).

## Big jobs: compute nodes

`--plan` tells you when a command is too big for a login node. Here is the daily climatology over all 30 years of
the 3-hourly store:

```bash
cdors --plan -ydaymean -seltimestep,1/87544 -selname,tas $H
```
```
stage 1 of 1: selname,tas -> seltimestep,1/87544 -> ydaymean, folded by ydaymean along time, 1 pass(es), 192 lanes in 14 waves (lane-major)
  read: 67776 chunks, 1.1 TB decoded, ~626 GB stored (estimate from 8 chunks); 67776 tiles
memory: ~4.2 GB peak of 4.3 GB budget (...)
read limit: 64.0 GB (login-node default, no SLURM_JOB_ID) -- EXCEEDED: the run would be refused
```

The memory does not depend on how many years you read: one year plans the same 4.2 GB. To fit a budget, cdors
splits the cells into groups ("waves") that it runs one after another.

Run it as a batch job (replace the account with your project):

```bash
#!/bin/bash
#SBATCH --job-name=cdors-ydaymean
#SBATCH --partition=compute
#SBATCH --account=<your project>
#SBATCH --nodes=1
#SBATCH --exclusive
#SBATCH --mem=0
#SBATCH --time=00:30:00
#SBATCH --output=cdors-ydaymean-%j.log
set -e
export PATH=/work/ab0995/a270088/cdors/bin:$PATH
IN=/work/kd1453/rechunked_ngc4008/ngc4008_PT3H_9.zarr
cdors --mem 32G --progress json -ydaymean -seltimestep,1/87544 -selname,tas $IN tas_ydaymean_2020-2049.nc
```

Or run one command interactively:

```bash
srun -p compute -A <your project> -N 1 --exclusive -t 00:30:00 cdors -ydaymean ... out.nc
```

Inside a Slurm job, cdors drops the read limit, uses all cores of the allocation and keeps 128 reads in flight. Its
default memory budget is 60 % of the job's memory; `--mem` sets it. This is the 72× command from the top of this
guide: 79 s and 14.3 GiB, where cdo took 1 h 35 min. `--progress json` writes one line per second to the log.

## The rules in one minute

- **Write commands as for cdo.** `cdors [options] -op3 -op2,args -op1 input output`. The leading dash of the first
  operator is optional, as in cdo.
- **Put selections innermost** (`-selname`, `-selyear`, `-sellonlatbox`, ...). cdors uses them to decide which
  chunks to read at all.
- **Look before you run.** `cdors --plan <command>` shows what would be read, how much memory it needs and how many
  passes it takes, without reading any data.
- **Outputs.** A name ending in `.zarr` gives Zarr, any other name NetCDF-4. An existing output is never
  overwritten unless you give `-O`.
- **Login nodes have limits.** cdors uses at most 16 threads there. It refuses commands that would read more than
  64 GB, and it plans within at most 4 GiB of memory. Bigger runs belong on a compute node (see "Big jobs").
- **For scripts, add `--json`.** It gives machine-readable output and errors.
- **Missing operators.** `cdors ops` lists what is implemented. For anything else, run that step with cdo.

## What the setup does

```bash
cdors --version
```
```
cdors 0.1.0
HDF5 1.14.3, netCDF-C 4.9.3-rc1
```

Nothing else is needed: no module, no conda environment. `cdors` is a small wrapper that sets two defaults before
starting the program:

- `CDORS_CACHE`: where cdors keeps remapping weights and NetCDF chunk indexes. The default is
  `/scratch/<first letter>/<user>/cdors-cache`. Old files there are removed by the scratch cleanup, and cdors simply
  makes them again.
- `CDO`: the cdo that makes remapping weights. The default is the cdo 2.6.0 that cdors was tested against.

Set either one yourself to override it.

Documentation next to the program, in `/work/ab0995/a270088/cdors/`:

| File | Contents |
|---|---|
| `README.md` | this guide |
| `doc/reference.md` | all options, JSON output, error codes, caches |
| `doc/deviations.md` | every known difference from cdo |
| `doc/bench-results.md`, `doc/criteria.md` | benchmarks against cdo and what the prototype has shown |
| `doc/LICENSE-CDO` | the license of CDO, from which cdors takes operator help texts and numerical routines |

## Differences from cdo you will notice

- An existing output is refused unless you give `-O`; `cat` never appends.
- The output format follows the output name (`.zarr` or NetCDF-4), not the input's format. Float32 variables stay
  float32; everything else (integers, packed data) becomes float64. cdo keeps integers, and rounds their means.
- NetCDF output is not compressed: `-z zip` is refused, and so are other cdo options cdors does not have (`-k`,
  `-r`, ...). `cdors --help` lists the options.
- Percentiles are exact, where cdo approximates them with a histogram for more than 50 values per cell. cdo's
  three-input form (`timpctl,95 in -timmin in -timmax in`) is accepted, and the min/max inputs are not read.
- `sinfo` prints its own summary (chunk shapes, codecs), not cdo's table.
- `mergetime` and `cat` refuse inputs whose times overlap or go backwards.
- Information operators (`sinfo`, `showname`, `griddes`, `showtimestamp`) take files, not chains.

The full list, with the reasons: `doc/deviations.md`. It also lists five cdo bugs found on the way. One of them
matters here: cdo 2.6.0 misreads the last time chunk of a HEALPix Zarr store when that chunk is partly filled.

## What it cannot do yet

- **GRIB input.** ERA5 in `/pool/data/ERA5` and the IFS outputs in GRIB are not readable; use cdo for those.
- **Many cdo operators.** Missing are, among others, `expr`, `trend`/`regres`, correlations, the ETCCDI indices,
  `ydaypctl`, `intlevel`, ensemble statistics and EOFs. `cdors ops` lists what exists, and a missing operator fails
  with `not_implemented`.
- **Compressed or NetCDF-4 classic output.** `-f nc4c` writes ordinary NetCDF-4.
- **Fast percentiles or daily climatologies on data stored one field per chunk** (GRIB-like NetCDF). These are read
  correctly but several times; `--plan` shows the number of passes.
- **Weights without cdo.** cdo makes the remapping weights, once per pair of grids.
- **Tested S3 access.** `s3://` inputs are implemented but have not been tried against a real bucket.

## Files cdors leaves behind

- `$CDORS_CACHE/weights/`: remapping weights and grid files from cdo.
- `$CDORS_CACHE/nc4index/`: chunk indexes of NetCDF-4 files.

Both can be deleted at any time; cdors makes them again when needed. A killed run (`kill -9`, node failure) can
leave a hidden `.<output>.cdors-tmp-...` file or directory next to the output. Remove it by hand.

## Version and feedback

The installed version is 0.1.0, commit `c93cb33` of 2026-10-09. The binary is the build that passed the tests
(`libexec/cdors-0.1.0-c93cb33`, checksum in `libexec/cdors-0.1.0-c93cb33.sha256`). New versions will be installed
next to it, and `bin/cdors` will point at the newest one. The source is at
[github.com/koldunovn/cdors](https://github.com/koldunovn/cdors).

This is a prototype, and reports help. Send wrong numbers, confusing errors and slow commands to Nikolay Koldunov
(Levante user a270088), with the command and the output of `cdors --version`.

cdors contains operator help texts and numerical routines derived from CDO, © 2002–2026 MPI für Meteorologie,
under the BSD 3-Clause license (`doc/LICENSE-CDO` next to the program).
