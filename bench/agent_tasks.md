# Agent check (Task 14): tasks, prompts and reference answers

Three sets of five plain-language analysis tasks: the prototype's T1–T5 (HEALPix and regular-grid ICON data,
2026-10-09), round 1's F1–F5 (FESOM2 on the dart mesh, `TASKSET=fesom`, section
[FESOM2 task set](#fesom2-task-set-round-1-tasksetfesom)) and the trap set W1, W2, W4, W5, W6 (weights that
naive methods get wrong on FESOM2, IFS O1280 and ICON R2B8, `TASKSET=trap`, section
[Trap task set](#trap-task-set-tasksettrap)). Each task is given to a headless `claude -p`
session twice: arm **A** with `cdors` (and its built-in documentation) and arm **B** with `cdo` 2.6.0 and
Python (xarray; for F1–F5 and the W tasks also nereus, for W5 gribscan instead). The runner is `bench/agent_check.sh`; scoring and the summary are
`bench/agent/agent_score.py`.

**This file is the single source of the prompts and the reference answers.** The runner extracts the text
between a `<!-- prompt Tn -->` / `<!-- prompt Fn -->` / `<!-- prompt Wn -->` (or `<!-- arm A -->`,
`<!-- arm B -->`, `<!-- arm A fesom -->`, `<!-- arm B fesom -->`, `<!-- arm A trap -->`, `<!-- arm B trap -->`,
`<!-- footer -->`) marker and the fenced block that follows
it, and the JSON block after `<!-- reference -->`. Edit them here only.

Reference answers were computed on 2026-10-09 on a Levante login node, independently of cdors: with xarray
(mambaforge env `hk25`, `python -I`) as the primary method and cdo 2.6.0
(`/sw/spack-levante/cdo-2.6.0-akkxhz/bin/cdo`) as a cross-check; for T3 cdo's `remapbil` value is the
reference and xarray the cross-check (see the T3 note). Scripts: `bench/agent/ref_t1_climatology.py`,
`ref_t2_t5_pr.py`, `ref_t3_remap.py`, `ref_t4_percentile.py` and `bench/agent/ref_cdo.sh` (T1, T2, T3, T5).
Each ran in under 2 minutes (cdo T1 116 s, everything else < 30 s).

## Design decisions that apply to several tasks

- **Timestamps.** The nextGEMS ngc4008 stores stamp each mean at the *end* of its interval (the daily mean
  of 30 June is stamped 2020-07-01 00:00). The prompts therefore say "by its stored timestamp", which is what
  `cdo selmon`/`ymonmean` and a plain xarray `time.dt.month` do. The alternative (shift by one interval)
  changes T1 by 1.0e-5 relative and T4 not at all, so it would pass anyway.
- **EERIE dates.** The kerchunk references and the EERIE cloud label the ICON-ESM-ER control run 1950-01-01 to
  2050-12-31 (stamped 12:00). The raw NetCDF files hold the same daily means in the same order, but stamped
  with model dates 41 years later at 00:00 at the end of each day (reference day 1950-01-01 = raw stamp
  1991-01-02T00:00; raw directory `run_19910101T000000-*` = reference January 1950). Because the reference
  years and the raw model years have their leap days in different places (1952 vs 1992), a year selected by
  raw date is not the same set of days as the reference year. The prompts say that dates follow the reference
  axis and how the raw files map to it. That is why T3 asks for **January 1950** (raw model January 1991)
  rather than the "January 1991" of the first draft: all EERIE tasks then use one time axis.
- **Box edges.** The 0.25° grid has points exactly on the box edges (e.g. 0°, 20°N, 70°N). Excluding them
  changes T2 by 4e-3 and T5 by 7e-4 relative, more than the tolerance. The prompts say that edge points are
  included (cdo `sellonlatbox` includes them, as does xarray `sel(slice(...))`).
- **Area weights.** On a regular lon-lat grid the exact spherical cell area is proportional to the cosine of
  the cell-centre latitude (away from the poles), so cos-lat weights and cdo's cell areas agree to < 1e-6.
  HEALPix cells are equal-area, so the plain mean over cells is the area-weighted mean.
- **Tolerances** are relative unless marked otherwise. They are set well above float32 storage and
  summation-order effects (< 1e-6 here: xarray in float64 and cdo agree to 1e-6 or better on T1, T2, T5) and
  above legitimate method differences named in each task, but below the most likely mistakes (no area
  weights, edges excluded, wrong year cut, wrong percentile definition). The plausible wrong answers are
  listed as `variants` in the reference JSON; the scorer reports which one an answer matches.
- **Precision.** Answers are asked with at least 6 significant digits.

## The tasks

| | Task | Data | Reference answer | Tolerance | Minimal read | Naive read |
|---|---|---|---|---|---|---|
| T1 | July of the 2020–2024 monthly climatology, global mean | ngc4008 daily `tas`, HEALPix z9 Zarr (W1) | 288.695417 K | rel 1e-4 (0.029 K) | 2.1 GB on disk (3.8 GB decoded): 2 time blocks per July × 5 | 25.6 GB (decade view, cdo `selmon` reads every step) |
| T2 | annual means 1950–1954, North-Atlantic box | ICON-ESM-ER `pr` daily 0.25°, Parquet refs / raw NetCDF (W2) | 2.911299 2.882307 2.918659 2.897558 2.995661 mm/day | rel 2e-4 each | 6.6 GB on disk (1826 whole-field chunks) | same |
| T3 | bilinear to 1°, time mean, global mean | ICON-ESM-ER ocean `to` at 1 m, Jan 1950 (W3) | 17.979334 °C | rel 1.5e-3 (0.027 °C) | 64 MB (31 chunks) | ~1.5 GB if a tool reads all 33 variables of the raw month |
| T4 | 95th percentile at Hamburg, 2020 | ngc4008 3-hourly `tas`, HEALPix z9, chunked in space (W4) | 292.119965 K | abs 0.006 K | 0.11 GB on disk (12 chunks of one cell block) | 21 GB on disk (a whole year of all cells) |
| T5 | January 1950 European box mean from the cloud | EERIE cloud `/kerchunk`, `pr` (W2 remote) | 2.341090 mm/day | rel 3e-4 | 0.11 GB over HTTPS (31 chunks) | same |

Per-task notes (what was computed, cross-checks, variants):

- **T1.** xarray: `time.month == 7` and year 2020–2024 by stored stamp (155 days), mean over cells per day in
  float64, then over days → **288.695417 K**. cdo on the decade view
  (`-fldmean -timmean -selmon,7 -selyear,2020/2024`): 288.695417 (identical to 6 decimals). Shifted-day
  variant 288.698310 (1.0e-5). All Julys have 31 days, so mean of daily values = mean of the five monthly means.
- **T2.** xarray on the Parquet refs (cos-lat weights, edges included, ×86400): **2.911299 2.882307 2.918659
  2.897558 2.995661**. cdo on the 1991–1995 raw files, daily box means then yearly means by step index:
  2.911299 2.882307 2.918659 2.897557 2.995660 (≤ 4e-7). Variants: years cut by raw model year
  (1951: 2.883519, 1952: 2.917542; 4e-4 off), edges excluded (2.923745 …; 4e-3), unweighted (2.969027 …; 2 %).
- **T3.** cdo: `-fldmean -timmean -remapbil,<grid file> -select,name=to` on the raw January directory, with a
  grid file of the prompt's grid (centres 0.5 … 359.5 E, 89.5 S … 89.5 N) → **17.979334 °C**. **Corrected
  2026-10-09:** the value was 17.973827 until the agent check, computed with cdo's `r360x180`, whose first
  longitude is 0°, not 0.5°: a different grid from the one the prompt asks for. Both arms of the check answered
  17.979334 (cdors) and 17.9793339 (cdo) on the prompt's grid; cdo on a grid file reproduces it. No verdict
  changed (17.973827 is 3.1e-4 off, within the tolerance; listed as a variant). The 1° centres coincide with
  0.25° source nodes, so bilinear interpolation returns node values and methods differ only in which coastal 1°
  cells become missing: xarray `.interp(method="linear")` 17.979147 (1.0e-5 from the reference, 42754 cells),
  the same rule with the cell above/right of each node 17.958381 (1.2e-3). Not remapping at all (0.25°
  area-weighted mean) gives 17.924268 (3.1e-3) and node values without any corner rule 17.920180 (3.3e-3); both
  fail. The unweighted mean of the remapped field (13.830, from `cdo infon`) fails too. The
  reference is cdo's value because cdo `remapbil` is the conventional definition of "bilinear" for this
  community; the tolerance admits the other bilinear conventions. **Weakness:** a global mean barely depends
  on the remapping, so the margin between "remapped" and "not remapped" is only about 2×.
- **T4.** healpy: Hamburg lies in nested cell **189989** (centre 9.987°E 53.572°N, 2.6 km away; the next centre
  is 8.6 km away, so "containing" and "nearest" cell agree). 2927 values stamped in 2020 (rank ⌈0.95·2927⌉ =
  2781) → **292.119965 K**; the 2928 3-hour intervals of 2020 (rank 2782) give the same value. Neighbouring
  ranks 292.081421 / 292.148438, numpy's default linear percentile 292.108398 (−0.0116 K); all fail. cdo's own
  `timpctl` cannot serve as the reference: above 50 values it uses a 101-bin histogram (bin width here ≈ 0.28 K);
  an arm-B answer from `cdo timpctl` is expected to fail, and the prompt states the exact definition.
- **T5.** xarray on the cloud endpoint and on the local Parquet refs (same bytes): **2.341090 mm/day**; cdo on
  the raw January directory 2.341089. Variants: edges excluded 2.339373 (7.3e-4), unweighted 2.389362 (2 %).

## Arm A feasibility with the current cdors build (2026-10-09, release binary of 01:48)

Spot checks, not part of the reference (the references above are cdors-free):
- **T5:** `cdors -fldmean -sellonlatbox,-10,40,35,70 -selmon,1 -selyear,1950 -selname,pr <cloud URL> d.nc`, then
  `cdors -timmean -mulc,86400 d.nc t5.nc` → 2.34108949 (reference 2.341090). 1.8 s for the cloud read.
- **T4:** `cdors -remapnn,lon=10/lat=53.55 -selyear,2020 -selname,tas <PT3H store> p.nc`, then
  `cdors -timpctl,95 p.nc out.nc` → 292.119965 (identical). But `remapnn` read the whole year of all cells
  (21 s, **max RSS 9.0 GB** on the login node) instead of the one chunk column it needs.
- Findings that will shape arm A's transcripts: cdors cannot yet chain two statistics
  (`-timmean -fldmean …`, and even `-mulc -timmean …`, fail with `not_implemented` and the hint "run two
  commands"); it has no operator that prints values (`outputf`, `infon`, `output` are not implemented), so
  arm A prints values with cdors' own operators (info/outputtab, --json); `remapnn`/`remapbil` need `CDORS_CACHE` and call cdo for the weights
  (the runner sets `CDORS_CACHE` to a fresh directory per session and `CDO` to cdo 2.6.0);
  `cdors --version | head -1` panics on the broken pipe (println!).
- T1, T2 and T3 were not run with cdors; each needs two or three cdors commands because of the chaining limit.

## Running the check

```sh
bench/agent_check.sh                          # DRY RUN: tool checks, every prompt and claude command; no claude
SELFTEST=1 bench/agent_check.sh               # scorer test on bench/agent/selftest/*.jsonl; no claude
RUN=1 TASKS=T5 ARMS=A bench/agent_check.sh    # one-session pilot (only with Nikolay's go-ahead)
RUN=1 bench/agent_check.sh                    # all 10 sessions, one after another (go-ahead again)
TASKSET=fesom bench/agent_check.sh            # the FESOM2 set F1-F5 (DOCS=guide by default): dry run
RUN=1 TASKSET=fesom bench/agent_check.sh      # its 10 sessions (go-ahead first; see the FESOM2 section)
TASKSET=trap bench/agent_check.sh             # the trap set W1 W2 W4 W5 W6 (DOCS=guide): dry run
```

Session settings (in `claude_args` of the runner): `-p` with `--output-format stream-json --verbose` (the
full transcript, so tool calls can be counted; the last line is the same result record that
`--output-format json` prints), `--no-session-persistence` (nothing written to `~/.claude`, home quota),
`--strict-mcp-config` (no MCP connectors), `--disable-slash-commands` (no skills in the system prompt),
`--tools Bash,Read,Write,Edit,Glob,Grep` (no subagents, no web tools), `--permission-mode dontAsk` with these
tools allowed and `rm`, `rmdir`, `git`, `sbatch`, `srun`, `salloc`, `scancel` denied, `--max-turns 50`
(`MAX_TURNS`; the flag is not listed in `claude --help` of 2.1.295, so the pilot must confirm that it is
accepted — set `MAX_TURNS=` to drop it), and a wall-clock limit of 30 min (`TIME_LIMIT`, `timeout`). The
user's `~/.claude/CLAUDE.md` is still loaded (≈ 10 kB, the same for both arms). Each session runs in a new
directory `$BASE/<run-id>/<task>-<arm>/`; the scorer flags (`peeked`) any tool call that touches this
file, the reference scripts, the `prep/` directory, the parent directory or another session's directory.

**Cost estimate.** Per session about 10–30 turns with a context growing from ≈ 20k to 40–80k tokens: about
50–100k fresh input (cache creation) and 5–20k output tokens, plus 0.3–1M cache-read tokens. Ten sessions:
≈ 0.6–1.2M fresh input + output tokens and ≈ 3–8M cache-read tokens. Reading `cdors ops --json` whole
(170 kB) would add ≈ 45k tokens to every later turn of that session. The pilot's result record gives the
real numbers (`usage`, `total_cost_usd`); re-estimate from it before the other nine sessions. Data read:
at most ≈ 50 GB in total over all sessions (T1 naive decade reads dominate), each session a few minutes of
login-node I/O.

## Prompts

Each session gets: the task prompt, then a blank line and the arm note, then a blank line and the footer.
`{CDORS}`, `{CDORS_DOCS}`, `{CDO}`, `{PYTHON}` and `{NCDUMP}` are filled in by the runner. `{CDORS_DOCS}`
lists only what the build offers at run time (`cdors help`, `cdors help <op>`, `cdors ops [--json]`, and the
repository's README.md and docs/deviations.md, which are *copied* into the session directory: the
repository itself is not shown to the agent because it contains this file).

<!-- prompt T1 -->
```text
The Zarr store /work/kd1453/rechunked_ngc4008/ngc4008_P1D_9.zarr holds daily-mean output of the nextGEMS ICON simulation ngc4008 on a HEALPix grid (zoom 9, nside 512, nested ordering). Compute the monthly climatology of 2 m air temperature (variable tas) over the years 2020 to 2024, and report the area-weighted global mean of its July value in K. Assign each daily value to the month and year of its stored timestamp.
```

<!-- prompt T2 -->
```text
The EERIE ICON-ESM-ER control-1950 simulation has daily-mean atmosphere output on a regular 0.25 degree grid. On this machine it is available as a kerchunk reference in Parquet format, /work/bm1344/k202193/Kerchunk/erc2002/control_1950/v20240618/atm_2d_1d_mean_remap025.parq, which points into the raw NetCDF-4 files under /work/bm1344/k202193/ICON/erc2002/postprocessing/interpolation/control_1950/atm_2d_1d_mean_remap025/ (one directory per model month). Dates in this task follow the time axis of the kerchunk reference, which runs daily from 1950-01-01 to 2050-12-31 (stamped at 12:00). The raw files contain the same days in the same order (taking the directories and the files in them in name order, step i of the reference is step i of the raw files), but their own timestamps are model dates 41 years later, stamped at 00:00 at the end of each day: the reference day 1950-01-01 is stamped 1991-01-02T00:00 in the raw files.

For each year 1950, 1951, 1952, 1953 and 1954, compute the annual mean of the precipitation flux (variable pr) averaged over the North Atlantic box 80W-0E, 20N-70N, with area weighting, and include the grid points that lie exactly on the box edges. Report the five values in mm/day (pr is stored in kg m-2 s-1; 1 kg m-2 s-1 = 86400 mm/day), in the order 1950 to 1954.
```

<!-- prompt T3 -->
```text
The EERIE ICON-ESM-ER control-1950 simulation has daily-mean ocean output on a regular 0.25 degree grid. On this machine it is available as a kerchunk reference in Parquet format, /work/bm1344/k202193/Kerchunk/erc2002/control_1950/v20240618/oce_2d_1d_mean_remap025.parq, which points into the raw NetCDF-4 files under /work/bm1344/k202193/ICON/erc2002/postprocessing/interpolation/control_1950/oce_2d_1d_mean_remap025/ (one directory per model month). Dates in this task follow the time axis of the kerchunk reference, which runs daily from 1950-01-01 to 2050-12-31 (stamped at 12:00). The raw files contain the same days in the same order (taking the directories and the files in them in name order, step i of the reference is step i of the raw files), but their own timestamps are model dates 41 years later, stamped at 00:00 at the end of each day: the reference day 1950-01-01 is stamped 1991-01-02T00:00 in the raw files.

Take the daily sea water potential temperature at 1 m depth (variable to, level depth = 1 m) for the 31 days of January 1950. Remap each day bilinearly to a regular global 1 x 1 degree grid whose cell centres are at longitudes 0.5, 1.5, ..., 359.5 E and latitudes 89.5 S, 88.5 S, ..., 89.5 N; land points are missing values and stay missing. Then average over the 31 days, and report the area-weighted global mean over the valid (ocean) cells of the 1 degree grid, in degrees Celsius (the stored unit).
```

<!-- prompt T4 -->
```text
The Zarr store /work/kd1453/rechunked_ngc4008/ngc4008_PT3H_9.zarr holds 3-hourly-mean output of the nextGEMS ICON simulation ngc4008 on a HEALPix grid (zoom 9, nside 512, nested ordering). Take the grid cell nearest to Hamburg (53.55 N, 10.0 E), i.e. the cell that contains this point, and all 3-hourly values of 2 m air temperature (variable tas) at that cell whose stored timestamp falls in the year 2020. Report their 95th percentile in K, using the nearest-rank definition: sort the n values in ascending order and take the value at rank ceil(0.95 * n), counting ranks from 1.
```

<!-- prompt T5 -->
```text
The EERIE cloud serves the daily-mean atmosphere output of the ICON-ESM-ER control-1950 simulation (regular 0.25 degree grid) as a Zarr dataset over HTTPS at https://eerie.cloud.dkrz.de/datasets/icon-esm-er.eerie-control-1950.v20240618.atmos.gr025.2d_daily_mean/kerchunk . Read the data from this URL (do not look for copies on the local file system). Compute the mean daily precipitation (variable pr) over Europe, the box 10W-40E, 35N-70N, for January 1950: average over the box with area weighting, including the grid points that lie exactly on the box edges, and over the 31 days of January 1950. Report the value in mm/day (pr is stored in kg m-2 s-1; 1 kg m-2 s-1 = 86400 mm/day).
```

<!-- arm A -->
```text
Tools: do the data reading and the computation with the command-line tool cdors ({CDORS}). cdors re-implements a subset of CDO's operators (same operator names, arguments and operator chaining) and reads Zarr stores and kerchunk references, locally and over HTTPS. Its documentation: {CDORS_DOCS}. cdors writes results to NetCDF or Zarr files, and prints values directly with its information operators (info, infon, output, outputf, outputtab; add --json for machine-readable output). Do not use cdo, Python or other programs to read or compute the data; shell tools for looking at text output and small calculations that read no data are fine.
```

<!-- arm B -->
```text
Tools: do the data reading and the computation with cdo 2.6.0 ({CDO}, first on PATH as cdo) and/or Python 3 with xarray, zarr, dask, numpy, scipy, fsspec, healpy, xesmf and netCDF4 ({PYTHON}; run it as python -I). ncdump ({NCDUMP}) is available too. Do not use cdors.
```

<!-- footer -->
```text
You are on a shared login node of the Levante supercomputer: use at most 16 threads, do not submit batch jobs, and keep every file you create in the current directory. When you have the result, end your final message with one line of the form
ANSWER: <number> [<number> ...]
giving only the requested numbers, in the requested order, without units, with at least 6 significant digits.
```

## FESOM2 task set (round 1, `TASKSET=fesom`)

Five tasks on raw FESOM2 output of the AWI-CM3 run `hist1950d` on the dart mesh (3 160 340 nodes, 79 layers;
the dataset is described in `docs/unstructured-baseline.md`). Prepared 2026-10-19; not yet run.

**Design decisions.**
- **Mesh path, both arms the same:** every prompt names the data directory and the mesh directory
  `/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart/` (`fesom.mesh.diag.nc` and the ASCII files). Arm A passes it
  to cdors (`-setgrid,fesom:<dir>`); arm B to nereus (`load_mesh`, which reads the ASCII files in about 4–5 min
  on a login node) or reads `fesom.mesh.diag.nc` itself. The data files' `FESOM_MeshPath` attribute names
  another, byte-identical copy.
- **Definitions in the prompt, not left to the tool:** area weights are `nod_area` (per level, the area at the
  layer's top interface), so nereus' own triangle areas (Earth radius 6371 km instead of FESOM's 6367.5 km,
  +0.11 %) are a variant, not an accepted method. The dry layers stored as 0 are named, and so are ρ0 and cp,
  the threshold and hemisphere of the ice extent, the section points and sampling rule, the NINO3.4 box and
  base period, the Arctic cap and how months enter an annual mean.
- **SST** is the top layer of monthly `temp` (2.5 m). The 6-hourly `sst` files hold the same field (January
  1985: monthly mean of `sst` minus `temp` top layer ≤ 7e-4 °C at any node) but are 18.5 GB per year; the
  prompts say not to use them.
- **Data volume per task** (what cdors decodes): F1 24.3 GB (all 79 levels of 2 years; 11 GB if only the
  36 layers above 700 m are read), F2 9.2 GB, F3 1.0 GB, F4 0.38 GB, F5 1.1 GB. Years differ between tasks
  (F1/F2 1990–1991, F3 2000, F4 2001–2005, F5 2006–2010), so a task does not start with the previous task's
  files in the page cache. Within a task arm A runs before arm B and so reads cold; arm B may find the files
  cached (as in the prototype).
- **Region edges:** F4 has no node within 4.3e-5° of the box edges and F5 none within 1e-6° of 66.5° N, so
  inclusive or exclusive edges give the same nodes. The Atlantic mask (F1) has 924 967 nodes with shapely's
  `contains_xy` on nereus' `oceanBasins.geojson`, with nereus' own `get_region_mask` and with cdors'
  `maskregion,regions=basins` (the OHC agrees to 1e-14, so the masks are identical).
- **Tolerances** are far above the numerical differences (float32 data summed in float64 in a different
  order: cdors and the references agree to 1e-14 relative; cdors' float32 outputs of F4/F5 to 1e-6 °C or better) and
  below the nearest plausible mistake, which is listed as a variant.

| | Task | Data | Reference answer | Tolerance (nearest variant) |
|---|---|---|---|---|
| F1 | Atlantic OHC 0–700 m, each month of 1990–1991 | `temp` monthly, 2 years | 24 values, 2.2812e24 … 2.2948e24 J | rel 3e-5 (ρcp = 4.09e6 instead of 1025 × 3990: 6.1e-5) |
| F2 | NH ice-extent seasonal cycle, daily extents averaged per calendar month over 1990–1991 | `a_ice` daily, 730 days | 12 values, 14.4785 15.2316 … 13.0119 million km² | rel 2e-4 (nereus areas: 1.1e-3) |
| F3 | Drake Passage section, 65° W, 55–63° S, January 2000: thickness-weighted section mean, wet samples | `temp`, one month | 1.05733802 °C, 5234 | rel 5e-4 on the mean, the count exact (nearest node in degrees: 2.6e-3, 5242) |
| F4 | NINO3.4 anomalies of 2005, base 2001–2005 | `temp` top layer, 5 years | 12 values, −0.375454 … 0.634958 °C | abs 2e-3 °C (unweighted: up to 1.6e-2) |
| F5 | Arctic (> 66.5° N) mean SST, annual means 2006–2010 | `temp` top layer, 5 years | −0.196318 −0.055787 −0.039366 −0.110217 −0.079932 °C | abs 1e-3 °C (unweighted: ≥ 3.0e-3; days-weighted ≥ 4.3e-3) |

Per-task notes (scripts `bench/agent/ref_fesom_{ohc,ice,drake,nino,arctic}.py`, NumPy on netCDF4, float64;
all five run by `bench/agent/ref_fesom.sh`, job 28230786 on one compute node (4 min 50 s for the job, 228 s of it the references); outputs in
`/scratch/a/a270088/cdors-bench/t14-refs-28230786/`):
- **F1.** Σ over the 36 layers above 700 m (zbar[36] = 700 m), wet layers (k < `nlevels_nod2D` − 1), Atlantic
  nodes of T · `nod_area[k]` · dz_k, times 1025 · 3990. Second method: `nereus.heat_content` with
  `nereus.get_region_mask(lon, lat, "Atlantic_Basin", "oceanBasins")` and the 2-D `nod_area`: identical to the
  last bit. Variants: ρcp = 4.09e6 (the cdors guide's recipe constant; 6.1e-5), nereus areas (1.1e-3), the
  surface area at every level (4.0e-3).
- **F2.** Daily extent Σ `nod_area[0]` over nodes with lat > 0 and a_ice ≥ 0.15; mean of the daily extents per
  calendar month over both years. Second method: `nereus.ice_extent_nh` (4e-16). `> 0.15` gives the same
  values. Variants: nereus areas (1.1e-3), threshold on the monthly-mean concentration (2–5 %), ice area
  instead of extent (7–17 %).
- **F3.** Nearest node of each point by a KD-tree on unit vectors, checked against a brute-force haversine
  search (81 distinct nodes; bottom depths 700 m at 55° S, 3100–4200 m elsewhere); the wet samples from the
  mesh agree with the stored non-zeros. Variants: nearest node by distance in degrees (1.06006, 5242),
  barycentric interpolation in the mesh triangles (1.07597, 5197), unweighted mean of the wet samples (1.891),
  dry zeros averaged in (0.554, 6399).
- **F4.** Box means with `nod_area[0]` over the 21 443 box nodes; cross-check: the box mean of node-wise
  anomaly fields (5e-15). Variants: unweighted (up to 0.016 °C off), base 2001–2004 (up to 0.20 °C), the box
  means themselves.
- **F5.** Cap lat > 66.5° N (609 569 nodes); cross-check: the area mean of each year's mean field (1e-16).
  Variants: unweighted (3.0–6.2e-3 °C), months weighted by days (4.3–6.9e-3), lat > 66° N (≈ 0.05), nereus'
  Arctic_Basin (≈ 0.4).

**cdors cross-check** (the commands an expert would run, cdors b3285c3, same job, before the Python references
so that cdors read cold; `M` the mesh directory, `D` the data directory):

```sh
cdors --json outputtab,date,value -mulc,4.08975e6 -volint,0/700 \
  -maskregion,regions=basins,select=Atlantic_Basin -setgrid,fesom:$M -selname,temp "$D/temp.fesom.199[01].nc"  # F1 22 s
cdors --json outputtab,name,date,value -ymonmean -fldhist,edges=0.15/1 -sellonlatbox,-180,180,0,90 \
  -setgrid,fesom:$M -selname,a_ice "$D/a_ice.fesom.199[01].nc"           # F2 4.6 s (records of name a_ice)
T="-transect,-65,-55,-65,-63,step=11.12"; I="-setgrid,fesom:$M -selname,temp -seltimestep,1 $D/temp.fesom.2000.nc"
cdors $T $I drake.nc                                                    # F3: 81 points, nearest node
cdors --json outputtab,value -fldsum $T -vertint $I                     #   sum of T dz: 293754.934
cdors --json outputtab,value -fldsum -selname,bottom_depth drake.nc     #   sum of dz:   277825
cdors --json outputtab,distance,lev,value -selname,temp drake.nc | jq '[.records[]|select(.value!=null)]|length'  # 5234
cdors -fldmean -sellonlatbox,-170,-120,-5,5 -sellevel,2.5 -setgrid,fesom:$M -selname,temp "$D/temp.fesom.200[1-5].nc" box.nc
cdors --json outputtab,date,value -selyear,2005 -ymonsub box.nc -ymonmean box.nc                     # F4
cdors --json outputtab,date,value -yearmean -fldmean -sellonlatbox,-180,180,66.5,90 -sellevel,2.5 \
  -setgrid,fesom:$M -selname,temp -mergetime $D/temp.fesom.20{06,07,08,09,10}.nc                     # F5
```

All five agree with the references: F1 ≤ 1e-14 relative, F2 ≤ 1e-14, F3 1.057338016 and 5234 (identical), F4
≤ 8.6e-7 °C and F5 ≤ 1e-8 °C (cdors' float32 outputs). Findings on the way (for the orchestrator, not fixed
here):
- **`maskregion,regions=…` inflates the planned bytes 5×** (`--plan`: 60.5 GB and 7680 chunks for one year of
  temp, whose file is 12.1 GB in 3840 chunks; the run itself is not slower). The region input seems to be
  counted as data. On a login node the F1 command above is therefore **refused** (`read_limit`, 121 GB planned
  > 64 GB) although it reads 24 GB; it ran only because a Slurm job has no read limit. Arm A will hit this
  unless it is fixed, and must then split the years or select the levels.
- **`volint,0/700` does not narrow the levels read**: it decodes all 79 levels (12.1 GB per year) where the 36
  layers above 700 m are 5.5 GB. `-sellevel` with the 36 midpoints does narrow it.
- **`vertint` (and `vertmean`) after `transect` or `selpoints` lose the mesh's layers:** the sampled variable is
  off the mesh and its `nz1` has no bounds, so `vertint` falls back to constant weights 1 (with the
  `equal_vertical_weights` warning): `-fldsum -vertint -transect … -setgrid,fesom:` gives 9898.24 (Σ T), not
  293754.93 (Σ T dz). `vertint` before `transect` is right. Writing `nz1_bnds` from the mesh interfaces (or
  carrying the layers through the sampling) would make both orders agree.
- `cdors guide`'s OHC recipe uses ρcp = 4.09e6; F1 asks for 1025 × 3990 = 4.08975e6 and lists 4.09e6 as a
  variant, which fails (6.1e-5 > 3e-5). If an arm-A agent copies the guide's constant, that is a finding about
  the guide, not about cdors.

**Cost estimate for the 10 FESOM sessions.** The prototype's DOCS=guide run cost $1.98 for five arm-A sessions
(0.17M fresh + 1.42M cache-read tokens, 426 s) and arm B $1.57 (0.13M + 0.78M, 1684 s). These tasks are harder
for both arms: arm A has to find the mesh operators (the guide is now 7.7 kB, and F1 may hit the read limit
above), arm B has to learn nereus, which agents do not know from training, and handle the mesh itself. Expect
$0.3–0.8 per session: **about $4–6 for all ten (range $3–8)**, ≈ 0.25–0.5M fresh and 2.5–5M cache-read tokens.
Wall time: arm A 1–5 min per session; arm B 3–15 min (nereus' `load_mesh` alone takes 4–5 min, and xarray
reads these files at ≈ 0.1 GB/s cold, so F1 and F2 take a few minutes of I/O): ≈ 40–90 min for the run. Data
read on the login node: ≈ 40–90 GB over all sessions. A one-session pilot first:
`RUN=1 TASKSET=fesom TASKS=F5 ARMS=A bench/agent_check.sh`.

### Prompts F1–F5

Every FESOM prompt opens with the same paragraph on the data, the mesh and the conventions; then the task.
The arm notes `arm A fesom` and `arm B fesom` replace `arm A` and `arm B`; the footer is the same.

<!-- prompt F1 -->
```text
The AWI-CM3 historical simulation hist1950d has FESOM2 ocean and sea-ice output on the unstructured dart mesh (3160340 nodes, 79 layers) in /work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom/, one NetCDF file per variable and year (<variable>.fesom.<year>.nc). The mesh is in /pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart/ (fesom.mesh.diag.nc and the ASCII mesh files). The variables are on the mesh nodes (dimension nod2); 3-D variables are (time, nz1, nod2), and the files store 0 in the layers below the sea floor, which are not ocean and must be left out. Area weights are the mesh's node areas (nod_area in fesom.mesh.diag.nc; for a layer, the area at its top interface). Assign each value to the month and year of its stored timestamp.

Compute the heat content of the Atlantic Ocean between the surface and 700 m depth for each month of 1990 and 1991, from the monthly temperature temp (degrees Celsius): OHC = rho0 * cp * (sum of T * V over the ocean layers above 700 m at the Atlantic nodes), where V is the volume of a node's layer (its area at the layer's top interface times the layer thickness), rho0 = 1025 kg m-3 and cp = 3990 J kg-1 K-1, with no reference temperature. 700 m is a layer interface of the mesh, so no layer is cut. The Atlantic is the region Atlantic_Basin of the ocean-basin polygons of nereus (from MPAS geometric_features, oceanBasins); a node belongs to it if it lies inside. Report the 24 monthly values in J, in time order.
```

<!-- prompt F2 -->
```text
The AWI-CM3 historical simulation hist1950d has FESOM2 ocean and sea-ice output on the unstructured dart mesh (3160340 nodes, 79 layers) in /work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom/, one NetCDF file per variable and year (<variable>.fesom.<year>.nc). The mesh is in /pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart/ (fesom.mesh.diag.nc and the ASCII mesh files). The variables are on the mesh nodes (dimension nod2); 3-D variables are (time, nz1, nod2), and the files store 0 in the layers below the sea floor, which are not ocean and must be left out. Area weights are the mesh's node areas (nod_area in fesom.mesh.diag.nc; for a layer, the area at its top interface). Assign each value to the month and year of its stored timestamp.

From the daily sea-ice concentration a_ice (a fraction between 0 and 1, although its units attribute says %), compute the sea-ice extent of the Northern Hemisphere for every day of 1990 and 1991: the total surface area (the node areas of the top level) of the nodes north of the equator whose concentration is 0.15 or more. Then, for each calendar month, average the daily extents over all days of that month in both years. Report the 12 values, January to December, in million km2 (10^12 m2).
```

<!-- prompt F3 -->
```text
The AWI-CM3 historical simulation hist1950d has FESOM2 ocean and sea-ice output on the unstructured dart mesh (3160340 nodes, 79 layers) in /work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom/, one NetCDF file per variable and year (<variable>.fesom.<year>.nc). The mesh is in /pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart/ (fesom.mesh.diag.nc and the ASCII mesh files). The variables are on the mesh nodes (dimension nod2); 3-D variables are (time, nz1, nod2), and the files store 0 in the layers below the sea floor, which are not ocean and must be left out. Area weights are the mesh's node areas (nod_area in fesom.mesh.diag.nc; for a layer, the area at its top interface). Assign each value to the month and year of its stored timestamp.

Take the monthly temperature temp of January 2000 (the first time step of temp.fesom.2000.nc) on a section across the Drake Passage along the meridian 65 W: 81 points at latitudes 55.0 S, 55.1 S, ..., 63.0 S. Each point takes the values of the mesh node nearest to it (by great-circle distance; no interpolation). A sample (point, layer) counts if that layer is ocean at that node. Report two numbers: the section-mean temperature in degrees Celsius, defined as the sum of T * dz over the counted samples divided by the sum of dz over the same samples (dz is the layer thickness; every point has the same weight), and the number of counted samples.
```

<!-- prompt F4 -->
```text
The AWI-CM3 historical simulation hist1950d has FESOM2 ocean and sea-ice output on the unstructured dart mesh (3160340 nodes, 79 layers) in /work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom/, one NetCDF file per variable and year (<variable>.fesom.<year>.nc). The mesh is in /pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart/ (fesom.mesh.diag.nc and the ASCII mesh files). The variables are on the mesh nodes (dimension nod2); 3-D variables are (time, nz1, nod2), and the files store 0 in the layers below the sea floor, which are not ocean and must be left out. Area weights are the mesh's node areas (nod_area in fesom.mesh.diag.nc; for a layer, the area at its top interface). Assign each value to the month and year of its stored timestamp.

Compute the NINO3.4 index for 2005. SST is the top layer of the monthly temperature temp (nz1 = 2.5 m); do not use the 6-hourly sst files, which are much larger. For each month of 2001 to 2005, compute the area-weighted mean SST over the NINO3.4 box, 5 S to 5 N and 170 W to 120 W (the nodes inside the box). Then compute the anomalies of 2005 relative to the 2001-2005 climatology of these monthly box means: for each calendar month, the box mean of 2005 minus the average of the five box means of that month. Report the 12 anomalies, January to December 2005, in degrees Celsius.
```

<!-- prompt F5 -->
```text
The AWI-CM3 historical simulation hist1950d has FESOM2 ocean and sea-ice output on the unstructured dart mesh (3160340 nodes, 79 layers) in /work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom/, one NetCDF file per variable and year (<variable>.fesom.<year>.nc). The mesh is in /pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart/ (fesom.mesh.diag.nc and the ASCII mesh files). The variables are on the mesh nodes (dimension nod2); 3-D variables are (time, nz1, nod2), and the files store 0 in the layers below the sea floor, which are not ocean and must be left out. Area weights are the mesh's node areas (nod_area in fesom.mesh.diag.nc; for a layer, the area at its top interface). Assign each value to the month and year of its stored timestamp.

Compute the mean sea surface temperature of the Arctic north of 66.5 N for each year 2006 to 2010. SST is the top layer of the monthly temperature temp (nz1 = 2.5 m); do not use the 6-hourly sst files, which are much larger. For each month, take the area-weighted mean SST over all nodes north of 66.5 N (ice-covered nodes included); the annual mean is the plain average of the 12 monthly values of the year (every month has the same weight). Report the 5 annual means, 2006 to 2010, in degrees Celsius.
```

<!-- arm A fesom -->
```text
Tools: do the data reading and the computation with the command-line tool cdors ({CDORS}). cdors re-implements a subset of CDO's operators (same operator names, arguments and operator chaining), reads NetCDF files, Zarr stores and kerchunk references, and knows FESOM2 meshes. Its documentation: {CDORS_DOCS}. cdors writes results to NetCDF or Zarr files, and prints values directly with its information operators (info, infon, output, outputf, outputtab; add --json for machine-readable output). Do not use cdo, Python or other programs to read or compute the data; shell tools for looking at text output and small calculations that read no data are fine.
```

<!-- arm B fesom -->
```text
Tools: do the data reading and the computation with cdo 2.6.0 ({CDO}, first on PATH as cdo) and/or Python 3 with nereus 0.4.1 (diagnostics for unstructured ocean meshes: FESOM2 mesh loading, region masks, heat content, sea-ice extent and area, ...), xarray, dask, numpy, scipy, netCDF4, shapely, pandas and matplotlib ({PYTHON}; run it as python -I). ncdump ({NCDUMP}) is available too. Do not use cdors.
```

## Trap task set (`TASKSET=trap`)

Five tasks whose naive answers are wrong: the weights or the grid hide a convention that a careful NumPy
answer must know and that cdors applies by default. The FESOM2 set (F1–F5) was answered correctly by both arms
(10/10, cdors 1.10× the cost), because its prompts spelled out every convention (`nod_area` per level, the
zeros below the sea floor). These prompts state the quantity as a scientist would ask for it, and leave the
storage conventions to the agent. Candidates and their naive answers were checked first (step 1,
`docs/agent-check.md`, "Trap candidates"); W3 (month-length weights) was dropped because cdors has no
`yearmonmean`. Prepared 2026-10-19; not yet run.

**Design decisions.**
- **The prompt defines the quantity, not the method.** "Area-weighted over the ocean at that depth" means the
  horizontal area of ocean at that depth, which on FESOM2 is the per-level `nod_area[k]` (0 below the floor),
  not the surface area; "the water between 1000 m and 2000 m" means the layer that straddles 2000 m counts with
  its upper half. The prompts do not say that the files store 0 below the sea floor, which variable holds the
  areas, or that a reduced Gaussian point's weight is its row's share. Arm A gets these from cdors' defaults,
  arm B has to find them.
- **What the prompts do fix,** because a careful answer could otherwise legitimately differ: full cells between
  the `zbar` interfaces with no partial bottom cells (W2; cdors and nereus both ignore them), element
  membership by the centre of the three vertices (W4), the time step and its stamp (W5, W6), cells by their
  centre (W6), and how to open a gribscan reference in Python (W5), so that arm B fails on weights, not on
  reading GRIB.
- **Accepted alternatives** that the tolerance must admit: W4 planar centres (identical membership in this
  box), W5 cell areas from latitude bands midway between the Gaussian rows (8e-9), W6 spherical triangle areas
  from the vertices (identical) and cell barycentres instead of centres (2.2e-6).
- **Tolerance rel 1e-4 for all five**, above float32 storage and summation order (cdors and the references
  agree to 3e-7 or better) and above the accepted alternatives, below the nearest naive answer: W1 surface area
  over the wet nodes 6.0e-4, W2 cut layers with surface areas 4.4e-4, W4 all three vertices in the box 7.9e-3,
  W5 unweighted 5.6e-3, W6 the second time step 2.7e-4 (unweighted 1.0e-3).
- **Python environments for arm B:** nereus (W1, W2, W4, W6) and ecmwf for W5, the only env whose eccodes
  (2.34) decodes these GRIB messages (nereus' 2.44 and hk25's 2.41 fail with `KeyValueNotFoundError`). cdo 2.6.0
  cannot read them either. cdo does read W6 (`-setgrid,<grid file>` gives cell areas), so arm B has a correct
  tool for W6 if it chooses cdo.
- **Data read** per task is small (one month, one or 13 levels; W6 one day plus the grid's coordinates and
  areas): seconds of I/O. nereus' `load_mesh` takes 4–5 min on a login node if arm B uses it.
- **Risks in arm A:** writing the O1280 field to NetCDF and averaging the file loses the reduced rows (a cdors
  gap): cdors then averages with equal weights, warns `equal_weights` and gives the unweighted variant. W6
  without `-setgrid` gives the same warning and the unweighted variant.

| | Task | Data | Reference | Strongest naive answers (rel. error) |
|---|---|---|---|---|
| W1 | mean temperature of the ocean at the 1600 m level, January 1990 | dart `temp`, one level | 2.75282665 °C | unweighted −39 %, surface area with the zeros −13 %, unweighted wet −9.3 %, surface area wet +6.0e-4 |
| W2 | volume mean temperature 1000–2000 m, January 1990 | dart `temp`, 13 layers | 3.08316716 °C | nereus `volume_mean` with `mesh["area"]` −14 %, with per-level areas (whole layers by mid-depth) −1.3 %, no thickness +3.2 %, cut layers with surface area +4.4e-4 |
| W4 | mean surface u over 10–40° E, 45–30° S (Agulhas), January 1990 | dart `u` on elements, top layer | 0.0278706695 m/s | cos(lat) −14 %, unweighted +9.4 %, all three vertices in the box −0.79 % |
| W5 | global mean 2 m temperature, January 1950 | IFS O1280 `mean2t`, gribscan → GRIB | 284.897081 K | cos(lat) per point +1.8 %, unweighted +0.56 % (also what cdo's equal weights would give) |
| W6 | mean 2 m temperature south of 60° S, 1 January 1991 | ICON R2B8 raw `tas` + grid file | 265.801638 K | cos(lat) +1.0 %, unweighted −1.0e-3, second time step +2.7e-4 |

References: `bench/agent/ref_trap.py` (NumPy on netCDF4, float64); W1, W2, W4 in job 28250598
(`/scratch/a/a270088/cdors-bench/trap-refs-28250598/`), W5 in env ecmwf and W6 on the login node
(`/scratch/a/a270088/cdors-bench/trap-refs-step2-20261019-211112/`). The chains an expert would write, with
the frozen binary `cdors-32b1dbd` (`M` the mesh directory, `D` the dart data directory, `G` the gribscan
JSON, `I` the ICON file, `IG` the ICON grid file):

```sh
cdors --json outputtab,value -fldmean -sellevel,1600 -seltimestep,1 -setgrid,fesom:$M -selname,temp $D/temp.fesom.1990.nc  # W1 2.7528267
cdors --json outputtab,value -volmean,1000/2000 -seltimestep,1 -setgrid,fesom:$M -selname,temp $D/temp.fesom.1990.nc     # W2 3.083167
cdors --json outputtab,value -fldmean -sellonlatbox,10,40,-45,-30 -sellevel,2.5 -seltimestep,1 -setgrid,fesom:$M \
  -selname,u $D/u.fesom.1990.nc                                                                                          # W4 0.02787067
cdors --json outputtab,date,value -fldmean -seltimestep,1 -selname,mean2t $G                                             # W5 284.89715
cdors --json outputtab,value -fldmean -sellonlatbox,-180,180,-90,-60 -seltimestep,1 -setgrid,$IG -selname,tas $I        # W6 265.80164
```

All five agree with the references to 3e-7 or better (cdors' float32 outputs for W1, W2, W4, W6).

**Cost estimate for the 10 sessions.** The FESOM2 run (job 28244863) cost $4.27 for ten sessions ($0.23–0.76
each, 44 min). These tasks read far less data but have less guidance in the prompt; arm B may also spend turns
on gribscan (W5) and on element centres (W4). Expect $0.25–0.7 per session: **about $4–6 for all ten (range
$3–7)**, 30–60 min on one shared node.

### Prompts W1–W6

The three FESOM2 prompts open with the same paragraph on the data and the mesh, without the conventions that
the F prompts state. The arm notes `arm A trap` and `arm B trap` replace `arm A` and `arm B`; `{PYLIBS}` is the
library list of the arm-B environment (nereus, or ecmwf for W5); the footer is the same.

<!-- prompt W1 -->
```text
The AWI-CM3 historical simulation hist1950d has FESOM2 ocean output on the unstructured dart mesh (3160340 nodes, 6262485 triangular elements, 79 layers) in /work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom/, one NetCDF file of monthly means per variable and year (<variable>.fesom.<year>.nc). The mesh is in /pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart/ (fesom.mesh.diag.nc and the ASCII mesh files).

Compute the mean temperature of the ocean at the model level at 1600 m depth (nz1 = 1600, the layer between 1550 m and 1650 m) in January 1990, which is the first time step of temp.fesom.1990.nc (temp, in degrees Celsius). Average over the ocean at that depth only, weighted by horizontal area at that depth. Report the value in degrees Celsius.
```

<!-- prompt W2 -->
```text
The AWI-CM3 historical simulation hist1950d has FESOM2 ocean output on the unstructured dart mesh (3160340 nodes, 6262485 triangular elements, 79 layers) in /work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom/, one NetCDF file of monthly means per variable and year (<variable>.fesom.<year>.nc). The mesh is in /pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart/ (fesom.mesh.diag.nc and the ASCII mesh files).

Compute the volume-weighted mean temperature of the ocean water between 1000 m and 2000 m depth in January 1990, which is the first time step of temp.fesom.1990.nc (temp, in degrees Celsius). Treat the model layers as full cells between the mesh's layer interfaces (zbar in fesom.mesh.diag.nc), with no partial bottom cells, and count only the water between 1000 m and 2000 m. Report the value in degrees Celsius.
```

<!-- prompt W4 -->
```text
The AWI-CM3 historical simulation hist1950d has FESOM2 ocean output on the unstructured dart mesh (3160340 nodes, 6262485 triangular elements, 79 layers) in /work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom/, one NetCDF file of monthly means per variable and year (<variable>.fesom.<year>.nc). The mesh is in /pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart/ (fesom.mesh.diag.nc and the ASCII mesh files).

The zonal velocity u is stored on the mesh elements (dimension elem). Compute the area-weighted mean surface zonal velocity (the top layer, nz1 = 2.5 m) in January 1990, which is the first time step of u.fesom.1990.nc, over the Agulhas Current region, the box 10 E to 40 E, 45 S to 30 S. An element belongs to the box if its centre (the mean position of its three vertices) lies inside. Report the value in m/s.
```

<!-- prompt W5 -->
```text
The EERIE IFS-FESOM hist-1950 simulation has monthly-mean IFS atmosphere output on the native octahedral reduced Gaussian grid O1280 (6599680 grid points in 2560 latitude rows), stored as GRIB files and indexed by gribscan references (kerchunk-style JSON). The surface fields of 1950-1959 are in /work/bm1344/a270228/phase1_hist/gribscan_1m_NATIVE/jsons.1950-1959/sfc.dir/atm2d_avg.json. Compute the area-weighted global mean 2 m temperature (variable mean2t) of January 1950, which is the first time step. Report the value in K. (In Python the reference opens with import gribscan, which registers the GRIB codec, and xarray.open_zarr(fsspec.filesystem("reference", fo=<the JSON file>).get_mapper(""), consolidated=False).)
```

<!-- prompt W6 -->
```text
The EERIE ICON-ESM-ER control-1950 simulation (experiment erc2002) has raw daily-mean atmosphere output on the native ICON grid R02B08 (5242880 triangular cells). The file /work/bm1344/k203123/experiments/erc2002/run_19910101T000000-19910131T235900/erc2002_atm_2d_1d_mean_19910101T000000Z.nc holds the 31 daily means of model January 1991. It has no coordinates; its grid is /pool/data/ICON/grids/public/mpim/0033/icon_grid_0033_R02B08_G.nc. Take the daily-mean 2 m air temperature tas of 1 January 1991, which is the first time step of the file (each daily mean is stamped at the end of its day, so this one is stamped 1991-01-02T00:00). Compute its area-weighted mean over the cells whose centre lies south of 60 S. Report the value in K.
```

<!-- arm A trap -->
```text
Tools: do the data reading and the computation with the command-line tool cdors ({CDORS}). cdors re-implements a subset of CDO's operators (same operator names, arguments and operator chaining), reads NetCDF files, Zarr stores, kerchunk references and gribscan references to GRIB files, and knows FESOM2 meshes and ICON grid files. Its documentation: {CDORS_DOCS}. cdors writes results to NetCDF or Zarr files, and prints values directly with its information operators (info, infon, output, outputf, outputtab; add --json for machine-readable output). Do not use cdo, Python or other programs to read or compute the data; shell tools for looking at text output and small calculations that read no data are fine.
```

<!-- arm B trap -->
```text
Tools: do the data reading and the computation with cdo 2.6.0 ({CDO}, first on PATH as cdo) and/or Python 3 with {PYLIBS} ({PYTHON}; run it as python -I). ncdump ({NCDUMP}) is available too. Do not use cdors.
```

## Long-script task set (`TASKSET=long`)

The trap set (W1–W6) did not separate the arms: Opus, Sonnet and Haiku knew every convention in Python, and a
cdors chain saved too little over a short NumPy script to show in the cost. These tasks need a long script
without cdors. H1 is the Atlantic overturning from the vertical velocity: element averages of w and their
areas, the elements' wet interfaces, basin membership from a polygon, and a cumulative sum from the basin's
closed end. In cdors it is one operator (`moc`, commit 3ffcfbb). Prepared 2026-10-20; reference job 28281059
(`bench/agent/ref_moc.sh`, 1 min 49 s); not yet run.

**Design decisions.**
- **The prompt defines the quantity, not the method:** the streamfunction is the northward transport above a
  depth, computed from w by continuity. It does not say where to start the sum (from the north: the Atlantic
  is open at 34 S), whether to average w to the elements, or that w below the sea floor is 0.
- **What the prompt fixes:** the basin (Atlantic_MOC of nereus' MOC basins, the same polygon as cdors' set
  `moc`), the time step, and that w, not v, is used.
- **Accepted alternatives:** pyfesom2's `xmoc_data` method (the basin masked on the nodes, outside nodes counted
  as 0 in the element mean, arithmetic-mean centres) −6.0e-5; arithmetic-mean centres alone 0 (the same
  1887094 elements); the nodes instead of the elements (w · `nod_area[k]` over the basin's nodes north of the
  line) +3.4e-3. All three put the maximum at 950 m.
- **Naive answers:** the element's sea-floor interface included (w there is not 0: the nodes are shared with
  deeper elements) +36 % at 900 m; the whole ocean instead of the basin −19 %; the sum from the south (the
  Atlantic is open at 34 S) −46 % at 3300 m.
- **Tolerance:** 0.09 Sv on the strength (5.2e-3, above the nodes variant, 0.059 Sv) and 50 m on the depth (one
  interface either side: the profile is within 0.2 % of its maximum at 900 m and 1000 m).
- **Data read:** one time step of w (80 × 3160340 float32, 1.0 GB) and the mesh's `elem`, `nlevels`, `elem_area`
  or `nod_area`. The reference script (`ref_moc.py`, NumPy and shapely) takes 79 s on 16 cores, cdors 24 s.

| | Task | Data | Reference | Naive answers (rel. error) |
|---|---|---|---|---|
| H1 | AMOC at 26.5 N, January 1990: max over depth of the streamfunction from w | dart `w`, one month | 17.4749385 Sv at 950 m | sea floor included +36 %, no basin −19 %, from the south −46 % |

The chain an expert would write (`M` the mesh directory, `D` the dart data directory; `cdors-3ffcfbb`, agrees with
the reference to 1e-12 at every interface):

```sh
cdors --json outputtab,lev,value -moc,lat=26.5,regions=moc,select=Atlantic_MOC -setgrid,fesom:$M -seltimestep,1 \
  -selname,w $D/w.fesom.1990.nc                                                    # max 17.474939 at lev 950
```

**Cost estimate for the two sessions (Opus).** Arm A should find `moc` in the guide (one recipe line) and need
5–15 calls: $0.3–0.8. Arm B has to write the script above from scratch (mesh arrays, the elements' wet
interfaces, polygon membership for 6.3M elements, the sum from the north) and to check it: 20–40 calls,
$0.8–2.5, 10–25 min. **About $1.5–3 for both**, 20–40 min on one shared node.

<!-- prompt H1 -->
```text
The AWI-CM3 historical simulation hist1950d has FESOM2 ocean output on the unstructured dart mesh (3160340 nodes, 6262485 triangular elements, 79 layers, 80 layer interfaces) in /work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom/, one NetCDF file of monthly means per variable and year (<variable>.fesom.<year>.nc). The mesh is in /pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart/ (fesom.mesh.diag.nc and the ASCII mesh files).

Compute the strength of the Atlantic meridional overturning circulation (AMOC) at 26.5 N in January 1990, which is the first time step of w.fesom.1990.nc (the vertical velocity w, positive upward, in m/s, on the mesh nodes at the layer interfaces, dimension nz). The overturning streamfunction psi(z) at 26.5 N is the northward volume transport across 26.5 N in the Atlantic between the surface and the interface at depth z; compute it from w by continuity (do not use the horizontal velocities). The Atlantic is the region Atlantic_MOC of the MOC basin polygons of nereus (from MPAS geometric_features, MOCBasins). The AMOC strength is the maximum of psi over the interfaces. Report two numbers: the AMOC strength in Sv (10^6 m3/s) and the depth of the interface where the maximum is reached, in m.
```

## Reference answers (machine-readable)

`tol_rel`: |answer − ref| ≤ tol_rel · |ref|; `tol_abs`: |answer − ref| ≤ tol_abs. Every value must pass. A
list gives one tolerance per value (F3: the count must be exact).

<!-- reference -->
```json
{
  "T1": {"values": [288.695417], "units": "K", "tol_rel": 1e-4,
         "variants": {"shifted_day": [288.698310]}},
  "T2": {"values": [2.911299, 2.882307, 2.918659, 2.897558, 2.995661], "units": "mm/day", "tol_rel": 2e-4,
         "variants": {"raw_model_years": [2.911299, 2.883519, 2.917542, 2.897557, 2.995660],
                      "edges_excluded": [2.923745, 2.896933, 2.931251, 2.912174, 3.008708],
                      "unweighted": [2.969027, 2.932542, 2.987084, 2.962285, 3.030157],
                      "kg_m2_s": [3.369559e-05, 3.336003e-05, 3.378078e-05, 3.353655e-05, 3.467200e-05]}},
  "T3": {"values": [17.979334], "units": "degC", "tol_rel": 1.5e-3,
         "variants": {"xarray_interp": [17.979147], "bilinear_upper_right": [17.958381],
                      "cdo_r360x180_first_lon_0": [17.973827],
                      "not_remapped": [17.924268], "node_values_no_corner_rule": [17.920180],
                      "unweighted": [13.830]}},
  "T4": {"values": [292.119965], "units": "K", "tol_abs": 0.006,
         "variants": {"rank_minus_1": [292.081421], "rank_plus_1": [292.148438],
                      "numpy_linear": [292.108398]}},
  "T5": {"values": [2.341090], "units": "mm/day", "tol_rel": 3e-4,
         "variants": {"edges_excluded": [2.339373], "unweighted": [2.389362],
                      "kg_m2_s": [2.709595e-05]}},
  "F1": {"values": [2.281160102e+24, 2.273218119e+24, 2.268780122e+24, 2.269105939e+24, 2.272318323e+24, 2.278770008e+24, 2.284409348e+24, 2.290213016e+24, 2.295766293e+24, 2.299432137e+24, 2.297234508e+24, 2.290519637e+24, 2.282638759e+24, 2.275223376e+24, 2.270059225e+24, 2.268669402e+24, 2.272060413e+24, 2.277636285e+24, 2.283503058e+24, 2.290453038e+24, 2.297465506e+24, 2.301455362e+24, 2.301715914e+24, 2.294758279e+24], "units": "J", "tol_rel": 3e-5,
         "variants": {"rho_cp_4.09e6": [2.281299545e+24, 2.273357077e+24, 2.268918809e+24, 2.269244646e+24, 2.272457226e+24, 2.278909305e+24, 2.284548991e+24, 2.290353013e+24, 2.295906630e+24, 2.299572698e+24, 2.297374935e+24, 2.290659653e+24, 2.282778293e+24, 2.275362456e+24, 2.270197990e+24, 2.268808083e+24, 2.272199300e+24, 2.277775513e+24, 2.283642645e+24, 2.290593050e+24, 2.297605947e+24, 2.301596046e+24, 2.301856614e+24, 2.294898554e+24],
                      "nereus_areas_r6371": [2.283668544e+24, 2.275717828e+24, 2.271274951e+24, 2.271601127e+24, 2.274817043e+24, 2.281275822e+24, 2.286921364e+24, 2.292731414e+24, 2.298290798e+24, 2.301960673e+24, 2.299760627e+24, 2.293038372e+24, 2.285148828e+24, 2.277725290e+24, 2.272555460e+24, 2.271164110e+24, 2.274558849e+24, 2.280140852e+24, 2.286014077e+24, 2.292971700e+24, 2.299991879e+24, 2.303986122e+24, 2.304246961e+24, 2.297281675e+24],
                      "surface_area_all_levels": [2.290416570e+24, 2.282408437e+24, 2.277919857e+24, 2.278255594e+24, 2.281563548e+24, 2.288044871e+24, 2.293615410e+24, 2.299383415e+24, 2.305059984e+24, 2.308775748e+24, 2.306648574e+24, 2.299909047e+24, 2.291876703e+24, 2.284443728e+24, 2.279238199e+24, 2.277883384e+24, 2.281294451e+24, 2.286807036e+24, 2.292722561e+24, 2.299735679e+24, 2.306815196e+24, 2.310863039e+24, 2.311168862e+24, 2.304178266e+24]}},
  "F2": {"values": [14.478482233, 15.231635799, 15.416135757, 14.974616176, 14.102417459, 12.402015204, 10.496770253, 8.832439038, 8.431991248, 9.218611514, 10.710114209, 13.011889686], "units": "1e6 km2", "tol_rel": 2e-4,
         "variants": {"nereus_areas_r6371": [14.494403274, 15.248385034, 15.433087875, 14.991082784, 14.117924966, 12.415652890, 10.508312866, 8.842151494, 8.441263358, 9.228748618, 10.721891422, 13.026198012],
                      "threshold_on_monthly_mean_conc": [14.805563018, 15.544750092, 15.776967438, 15.353477066, 14.540532885, 12.889443569, 11.030411314, 9.221924073, 8.796782562, 9.535846959, 11.205685930, 13.495286273],
                      "ice_area": [13.445514269, 14.159212103, 14.325587852, 13.890810670, 12.897892790, 11.081964236, 8.970590931, 7.371627396, 7.051020300, 8.140693317, 9.853797583, 12.021620052]}},
  "F3": {"values": [1.057338016, 5234], "units": "degC, count", "tol_rel": [5e-4, 0],
         "variants": {"nearest_in_degrees": [1.060055750, 5242], "linear": [1.075973482, 5197],
                      "plain_mean_of_wet": [1.891142939, 5234], "zeros_included": [0.553680019, 6399]}},
  "F4": {"values": [-0.375454372, -0.200573347, -0.047399871, -0.243103691, -0.304233224, -0.120816687, 0.244296170, 0.520206383, 0.622435654, 0.786803642, 0.498286474, 0.634957722], "units": "degC", "tol_abs": 2e-3,
         "variants": {"unweighted": [-0.390470971, -0.208962869, -0.058543561, -0.247424457, -0.299764364, -0.115204902, 0.253073414, 0.535566168, 0.637047112, 0.785664349, 0.489681929, 0.619068417],
                      "base_without_last_year": [-0.469317965, -0.250716684, -0.059249839, -0.303879614, -0.380291530, -0.151020859, 0.305370212, 0.650257979, 0.778044567, 0.983504553, 0.622858092, 0.793697153],
                      "box_mean_not_anomaly": [24.824170819, 25.299310231, 26.041648614, 26.460685417, 26.763985356, 26.951369020, 27.000792934, 26.644750933, 26.283951816, 26.481071058, 26.379821465, 26.212549798]}},
  "F5": {"values": [-0.196317738, -0.055786993, -0.039365774, -0.110216592, -0.079931765], "units": "degC", "tol_abs": 1e-3,
         "variants": {"unweighted": [-0.199316882, -0.059312426, -0.044623059, -0.116396284, -0.085264337],
                      "days_weighted": [-0.190564178, -0.049414619, -0.035046585, -0.104333903, -0.073043536],
                      "lat_gt_66": [-0.137979286, -0.008627595, 0.013593602, -0.047890482, -0.015438926],
                      "nereus_arctic_basin": [-0.618014505, -0.566699424, -0.537093758, -0.807307764, -0.853199395]}},
  "W1": {"values": [2.752826646], "units": "degC", "tol_rel": 1e-4,
         "variants": {"surface_area_wet": [2.754466051], "unweighted_wet": [2.497588448],
                      "surface_area_with_zeros": [2.395082684], "unweighted_with_zeros": [1.667510185]}},
  "W2": {"values": [3.083167163], "units": "degC", "tol_rel": 1e-4,
         "variants": {"cut_surface_area_wet": [3.084537540], "nereus_volume_mean_level_area": [3.044035400],
                      "whole_layers_inside_range": [3.126302262], "no_thickness": [3.180841455],
                      "cut_surface_area_with_zeros": [2.692753915], "nereus_volume_mean_mesh_area": [2.655336580]}},
  "W4": {"values": [0.0278706695], "units": "m/s", "tol_rel": 1e-4,
         "variants": {"all_vertices_in_box": [0.0276510764], "unweighted": [0.0304948618],
                      "coslat_weights": [0.0239582316]}},
  "W5": {"values": [284.897080734], "units": "K", "tol_rel": 1e-4,
         "variants": {"unweighted": [286.484167498], "coslat_per_point": [290.036648937],
                      "gauss_weight_per_point": [290.036649349]}},
  "W6": {"values": [265.801637753], "units": "K", "tol_rel": 1e-4,
         "variants": {"second_time_step": [265.872575366], "unweighted": [265.535760076],
                      "coslat_weights": [268.509437328]}},
  "H1": {"values": [17.474938542, 950], "units": "Sv, m", "tol_abs": [0.09, 50],
         "variants": {"pyfesom2_like": [17.473898413, 950], "nodes_nod_area": [17.534001, 950],
                      "with_sea_floor": [23.739173, 900], "global": [14.238147, 950],
                      "from_south": [9.486877, 3300]}}
}
```

## Scoring

- **correct**: an `ANSWER:` line was found, it has the expected number of values, and every value is within
  tolerance. `no_answer` if no line was found (the scorer falls back to a line `ANSWER` written in bold or
  inside backticks, but never guesses from other text); `wrong_count` if the number of values differs.
- **commands**: tool calls in the transcript (all tools, and Bash alone); **turns**: `num_turns` of the result
  record; **wall time**: measured by the runner (and `duration_ms` from the result record);
  **tokens**: input, output, cache-creation and cache-read tokens, and `total_cost_usd`, from the result
  record (summed from the assistant messages when the session was cut off and no result record exists).
- A matched variant is reported next to the verdict (e.g. `fail (edges_excluded)`), which tells a
  misunderstanding apart from a numerical problem.
