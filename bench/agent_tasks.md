# Agent check (Task 14): tasks, prompts and reference answers

Five plain-language analysis tasks. Each is given to a headless `claude -p` session twice: arm **A** with
`cdors` (and its built-in documentation) and arm **B** with `cdo` 2.6.0 and Python/xarray. The runner is
`bench/agent_check.sh`; scoring and the summary are `bench/agent/agent_score.py`.

**This file is the single source of the prompts and the reference answers.** The runner extracts the text
between a `<!-- prompt Tn -->` (or `<!-- arm A -->`, `<!-- arm B -->`, `<!-- footer -->`) marker and the
fenced block that follows it, and the JSON block after `<!-- reference -->`. Edit them here only.

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
| T3 | bilinear to 1°, time mean, global mean | ICON-ESM-ER ocean `to` at 1 m, Jan 1950 (W3) | 17.973827 °C | rel 1.5e-3 (0.027 °C) | 64 MB (31 chunks) | ~1.5 GB if a tool reads all 33 variables of the raw month |
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
- **T3.** cdo: `-timmean -select,name=to` on the raw January directory, `-remapbil,r360x180`, `-fldmean` →
  **17.973827 °C** (42763 valid 1° cells); remapping each day before the time mean gives the same value. The 1°
  centres coincide with 0.25° source nodes, so bilinear interpolation returns node values and methods differ
  only in which coastal 1° cells become missing: xarray `.interp(method="linear")` 17.979147 (3.0e-4 from cdo,
  42754 cells), the same rule with the cell above/right of each node 17.958381 (8.6e-4). Not remapping at all
  (0.25° area-weighted mean) gives 17.924268 (2.8e-3) and node values without any corner rule 17.920180
  (3.0e-3); both fail. The unweighted mean of the remapped field (13.830, from `cdo infon`) fails too. The
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
  the arm-A note offers `ncdump`; `remapnn`/`remapbil` need `CDORS_CACHE` and call cdo for the weights
  (the runner sets `CDORS_CACHE` to a fresh directory per session and `CDO` to cdo 2.6.0);
  `cdors --version | head -1` panics on the broken pipe (println!).
- T1, T2 and T3 were not run with cdors; each needs two or three cdors commands because of the chaining limit.

## Running the check

```sh
bench/agent_check.sh                          # DRY RUN: tool checks, every prompt and claude command; no claude
SELFTEST=1 bench/agent_check.sh               # scorer test on bench/agent/selftest/*.jsonl; no claude
RUN=1 TASKS=T5 ARMS=A bench/agent_check.sh    # one-session pilot (only with Nikolay's go-ahead)
RUN=1 bench/agent_check.sh                    # all 10 sessions, one after another (go-ahead again)
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
Tools: do the data reading and the computation with the command-line tool cdors ({CDORS}). cdors re-implements a subset of CDO's operators (same operator names, arguments and operator chaining) and reads Zarr stores and kerchunk references, locally and over HTTPS. Its documentation: {CDORS_DOCS}. cdors writes its results to NetCDF or Zarr files; to look at the values in a small result file, use ncdump ({NCDUMP}). Do not use cdo, Python or other programs to read or compute the data; shell tools for looking at text output and small calculations that read no data are fine.
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

## Reference answers (machine-readable)

`tol_rel`: |answer − ref| ≤ tol_rel · |ref|; `tol_abs`: |answer − ref| ≤ tol_abs. Every value must pass.

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
  "T3": {"values": [17.973827], "units": "degC", "tol_rel": 1.5e-3,
         "variants": {"xarray_interp": [17.979147], "bilinear_upper_right": [17.958381],
                      "not_remapped": [17.924268], "node_values_no_corner_rule": [17.920180],
                      "unweighted": [13.830]}},
  "T4": {"values": [292.119965], "units": "K", "tol_abs": 0.006,
         "variants": {"rank_minus_1": [292.081421], "rank_plus_1": [292.148438],
                      "numpy_linear": [292.108398]}},
  "T5": {"values": [2.341090], "units": "mm/day", "tol_rel": 3e-4,
         "variants": {"edges_excluded": [2.339373], "unweighted": [2.389362],
                      "kg_m2_s": [2.709595e-05]}}
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
