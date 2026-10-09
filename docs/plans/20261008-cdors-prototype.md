# cdors prototype: CDO-style climate statistics on Zarr, in Rust

## Overview

`cdors` (working name) is a Rust command-line tool that re-implements an analysis subset of CDO and reads Zarr,
kerchunk references and NetCDF natively. Its primary users are AI agents analysing large DestinE and EERIE datasets;
humans come second.

- **Problem.** CDO reaches Zarr only through netCDF-C's NCZarr layer. NetCDF-4 and NCZarr streams take a global I/O
  lock (`libcdi/src/stream.c:668`), and every operator consumes one 2D field per variable, level and timestep
  (`src/operators/Timstat.cc`, `run_sync`). Time-chunked Zarr stores, remote stores and multi-TB percentile or
  climatology jobs are therefore slow or run out of memory.
- **Approach (chosen in the brainstorm, 2026-10-08).** A new engine that works chunk by chunk, guided by a planner:
  selections decide which chunks are read at all, consecutive operators are merged into one pass, and memory is
  bounded by tiling. Operator semantics follow CDO: numerical routines are ported from CDO where they are sound,
  deviations are documented, and `cdo` itself is the test reference. The planner idea is borrowed from Earthmover's
  Zax (announced 2026-09-15); the interface stays CDO's command language, which agents already know.
- **Interface.** Same operator names, arguments and chaining as `cdo`, plus `--plan` (what will be read, passes, peak
  memory, before running) and `--json` (machine-readable output, warnings and errors).
- **Scope of this plan.** A prototype of roughly 2–4 weeks, about a dozen kernels exposed under CDO operator names.
  It must prove four things before a second round is considered:
  1. faster than `cdo` on the same node, with results matching within tolerance;
  2. analysis directly on remote Zarr (EERIE cloud over HTTPS) is practical;
  3. agents use it reliably;
  4. memory stays bounded on multi-TB inputs (percentiles, daily climatologies) — demonstrated on data chunked in
     space; data stored one complete field per chunk (GRIB, NetCDF written per timestep) needs a rechunking stage
     that is deferred to round 2.

## Context (from discovery)

**Repository.** `/home/a/a270088/cdo` holds only `cdo-2.6.5.tar.gz` (CDO source, BSD-3-Clause; the bundled YAC core
is BSD-3-Clause too). CDO 2.6.5: 719 operators in 217 modules, about 170k lines of C++ in `src/`, 99k lines of C in
`libcdi`, 36k lines in `libyac_core`.

**CDO facts the design relies on** (paths relative to the extracted source, all checked on 2026-10-08):
- Percentiles (`timpctl` and friends): each grid point keeps raw values while it has at most 50 of them
  (101 bins × 2 bytes hold 50 floats), and for those cdo computes exact percentiles with the nearest-rank method, the
  default (`src/percentiles.cc:47`; `--percentile` selects others). Above 50 values it switches to a 101-bin histogram
  bounded by the min/max input files (`src/percentiles_hist.cc`, `CDO_PCTL_NBINS`), which is approximate.
- The weighted field mean sums with `#pragma omp parallel for simd reduction` (`src/varray.cc:725`), so cdo's own
  last bits depend on build and thread count. Bit-identical agreement with cdo is not a meaningful target.
- Most operators overwrite outputs silently; only interactive mode asks (`src/fileStream.cc:116`). `cat` appends to an
  existing output unless `-O` is given (`src/operators/Cat.cc:128`).
- HEALPix is recognised via `grid_mapping_name = "healpix"` with `healpix_nside` and `healpix_order`
  (`libcdi/src/grid.c:898`, `:3929`). Grid names `hpz<zoom>` and `hp<nside>` with optional `_nested`/`_ring`
  (`src/grid_from_name.cc:835`). HEALPix operators: `hpdegrade`, `hpupgrade`.
- Output timestamps of time statistics follow `--timestat_date` (first, middle, midhigh, last); CDO writes
  `time_bnds`; it adds `cell_methods` only in CMOR, `setpartab` and ETCCDI operators.
- `genbil`/`remapbil` abort on unstructured and GME source grids (`src/operators/Remapweights.cc:248`).
- **`cdo diffn` as the test comparator** (`src/operators/Diff.cc`): a field counts as different if
  `absm > abslim` or `relm >= rellim`; `abslim` defaults to 0 and `rellim` to 1 (`:223`, `:364`). The relative
  difference is only computed where both values have the same sign (`:56`). A different number of timesteps only
  produces a warning and exit code 0 (`:561`) unless `--pedantic` turns warnings into errors
  (`src/cdo_def_options.cc:261`). Timestamps are not compared at all.
- `cdo -T` timers run only with one process and one OpenMP thread (`src/cdo.cc:504`). `--no_history` suppresses the
  history attribute. `setgridtype,unstructured` turns a regular grid into an unstructured one; `duplicate` repeats a
  dataset (test fixtures).

**Environment (Levante).**
- No Rust toolchain installed. **Home quota is tight** (writes failed with `EDQUOT` on 2026-10-08), so rustup,
  cargo, the build directory and all caches go under `/work/ab0995/a270088` or `/scratch/a/a270088`, never home.
- `cdo` on PATH is 2.2.2; modules `cdo/2.0.4` … `cdo/2.6.0` exist. Tests use `module load cdo/2.6.0`.
- netCDF-C 4.9.3-rc1 on PATH (spack `netcdf-c-main`). Both the `netcdf` and `hdf5-metno` crates must link against
  the HDF5 that this netCDF-C uses; the separate HDF5 in mambaforge must not end up in the same process.
- Mambaforge (`/work/ab0995/a270088/mambaforge`) provides xarray for writing Zarr test fixtures and for the remote
  baseline in the benchmarks.
- Agents run on the login node; heavy runs need `srun`/`sbatch` on 128-core compute nodes.

**Data.**
- EERIE intake catalog `/pool/data/Catalogs/dkrz_eerie.yaml`: `disk` reads raw files (NetCDF, GRIB) in `/work/bm1344`
  through kerchunk references in `/work/bm1344/DKRZ/kerchunks` — these need our kerchunk reader (Task 10); `cloud` is
  `https://eerie.cloud.dkrz.de/intake.yaml` (xpublish, Zarr v2 with consolidated metadata at
  `https://eerie.cloud.dkrz.de/datasets/<id>/zarr`); `dkrz_ngc3` is nextGEMS Cycle 3
  (`https://data.nextgems-h2020.eu/catalog.yaml`, HEALPix Zarr).
- AWI data under `/work/bm1344/AWI` (`EERIE`, `DestinE`, `Cycle3`, …).
- **Survey results (2026-10-08, details in `bench/datasets.md`):**
  - EERIE kerchunk references are **Parquet** (e.g. `.../Kerchunk/erc2002/.../atm_2d_1d_mean_remap025.parq`), not JSON.
  - The raw ICON-ESM-ER NetCDF-4 files use the **HDF5 blosc filter** (id 32001), one field per chunk. cdo 2.6.0 links
    netCDF-C 4.10.0 + HDF5 1.14.6 and reads them; our spack netCDF-C 4.9.3-rc1 + HDF5 1.14.3 may lack the plugin.
  - The EERIE cloud serves datasets at `https://eerie.cloud.dkrz.de/datasets/<id>/kerchunk`; `/zarr` returns 403 and
    `intake.yaml` 404.
  - cdo reads the nextGEMS Zarr stores only through single-variable views (`bench/make_view.py`, symlinks); on the
    full 111-variable store it is unusably slow. It reports the HEALPix grid as `gridtype=projection`.
  - Benchmarks: W1 = `/work/kd1453/rechunked_ngc4008/ngc4008_P1D_9.zarr` `tas` (HEALPix zoom 9, chunks 30×65536,
    blosc-lz4; one decade = 45.9 GB decoded); W2 = ICON-ESM-ER control-1950 daily `pr` 0.25° (raw NetCDF-4 + Parquet
    refs + cloud); W3 = the same model's daily ocean `to` at 1 m on a regular 0.25° grid; W4 =
    `ngc4008_PT3H_9.zarr` `tas` (chunks 248×16384, 1.1 TB decoded; `ngc4008_PT15M_9.zarr` at 13.2 TB is the
    stress option).
  - Login-node indications: cdo reaches ≈ 0.95 GB/s decoded on W1 (read share 76–84 %, `-P 8` does not help) and
    0.1–0.2 GB/s on W2–W4 (read share ≈ 95 %).

**Rust dependencies.**
- `zarrs` 0.23.x: Zarr v3 and the v3-compatible subset of v2, sharding. gzip, zstd and blosc are stable;
  `numcodecs.shuffle`, `numcodecs.zlib` and `fletcher32` are experimental. The async API is experimental and slower
  than the sync API. Stores: filesystem, HTTP (sync), `object_store`, OpenDAL, Icechunk. **No kerchunk store**: we
  write our own adapter.
- `object_store` (HTTP, S3), `tokio`, `rayon`, `netcdf` (georust, netCDF-C bindings), `hdf5-metno` (chunk index via
  `chunk_info`), `cdshealpix`, `serde`/`serde_json`, `thiserror`, `glob`. No DataFusion, no Arrow.
- `cftime-rs` looks unmaintained; CF calendars are our own code.

**Constraints (from `~/.claude/CLAUDE.md`).**
- Never delete anything without Nikolay's explicit yes. The tool removing its *own* temporary output after a failed
  run is part of its design; Claude removing files during development is not allowed without asking.
- Estimate cost (node-hours, tokens, wall time) and wait for an explicit yes before any multi-hour run or batch of
  agent sessions. Submit Slurm jobs, report job ids, end the turn; never poll for hours.
- No Workflow tool, no agent fleets beyond 3–5 agents with a deliverable and a time box each.
- The git repository stays local. Nothing is pushed or published without the exact text approved first, and no Claude
  session links or "generated by Claude" markers go into commits or anything that leaves the machine.

## Development Approach

- **Testing approach: Regular** — implement first, then add the operator's rows to the cdo-comparison table and run
  it. A task is done when its rows pass.
- **Lean testing is a hard rule** (Nikolay, 2026-10-08): test effort never exceeds the effort of writing the code, and
  the whole suite runs in seconds. Concretely:
  - every task that adds or changes operator behaviour adds rows to `tests/cases.txt` — one line per case;
  - rows cover each kernel, each statistic and each period once, not every combination of them;
  - no separate unit-test or property-test suites; a unit test is written only to pin down a concrete bug;
  - the planner check reuses existing rows (re-run under tiny chunks and, from Task 7, a tiny memory budget).
- **All rows must pass before starting the next task.**
- Complete each task fully before moving to the next; make small, focused changes.
- **Update this plan file when scope changes during implementation.**
- Port numerical formulas from the extracted CDO source where they are numerically sound; where they are weak, deviate
  and record the deviation in `docs/deviations.md`.

## Testing Strategy

**One table, one script.** `tests/run_cases.sh` reads `tests/cases.txt`. Each row is a tolerance tag, a cdo argument
string with `{in}`/`{in2}` placeholders, and the fixtures to run it on:

```
# tag    | arguments                                   | fixtures
exact    | -selname,tas {in}                           | r36x18_std hpz2_noleap unst_360
ulp      | -yearmean {in}                              | r36x18_std hpz2_noleap unst_360
exact    | -monpctl,90 {in} -monmin {in} -monmax {in}  | hpz2_noleap      # <= 50 values per group: cdo is exact
bin      | -timpctl,90 {in} -timmin {in} -timmax {in}  | hpz2_noleap      # > 50 values: cdo uses histograms
text     | griddes {in}                                | r36x18_std hpz2_noleap unst_360
err:read_limit | --max-read 1 -timmean {in}            | r36x18_std
```

For each row and fixture the script:
1. runs `cdo --no_history <args>` (output cached under `$CARGO_TARGET_DIR/cdo-ref/`, keyed by cdo version, row and
   fixture, so cdo runs only when a row or fixture changes) and `cdors --no_history <args>`;
2. compares values with `cdo --pedantic diffn,abslim=<x>`, with `rellim` left at its default, and compares
   `cdo -s showtimestamp` of both outputs as text. The tag sets `abslim`:
   - `exact`: 0 (selections, min, max, copies, small-group percentiles);
   - `ulp`: two float32 units in the last place at the reference's largest magnitude, `2 · 2⁻²⁴ · max|ref|`, computed
     with cdo from the cached reference (sums, means, variances; fixtures are F32);
   - `bin`: the largest histogram bin width of the input, `max(timmax − timmin) / 101`, computed with cdo;
   - `text`: plain `diff` of stdout (`griddes`, `showname`);
   - `err:<code>`: cdors only; must fail with that error code.

**Variants, on one fixture only** (`hpz2_noleap`, which has missing values):
- Zarr input: the same rows on Zarr v2 and v3 copies written by xarray, so our reader is tested against real Zarr
  rather than our own writer;
- planner check: the same rows on a tiny-chunk Zarr copy (from Task 7 also with `--mem 1M`), which forces tiling and
  several passes; the output must match the first cdors output exactly (`abslim=0`). This holds for reductions too,
  because state is always folded in a fixed order (see Technical Details > Determinism).

Rows run in parallel (`xargs -P $(nproc)`). Fixtures are tiny and generated on demand by `tests/make_fixtures.sh`
(not committed): a 36×18 regular grid, HEALPix zoom 2 (192 cells), an unstructured grid made with
`setgridtype,unstructured`; three years of daily steps; standard, noleap and 360_day calendars; missing values; a
two-level variant for vertical operators. Target runtime of the whole suite: under 30 seconds once the cdo outputs are
cached. Rows needing `cdo` are skipped with a notice where it isn't available.

**Benchmarks are not tests.** `bench/bench.sh` runs each workload once per tool and prints a table (Task 13).

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- update plan if implementation deviates from original scope
- keep plan in sync with actual work done

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): tasks achievable within this repository — code, the test table,
  scripts, documentation.
- **Post-Completion** (no checkboxes): decisions and actions outside the repository.

## Cost and Time (estimates, to confirm before each run)

- Implementation: about 2–4 weeks of sessions. If executed by agents, very roughly 20–50M tokens in total; estimate
  again before starting any autonomous execution.
- Compute: Task 2 baseline about 1–2 node-hours; Task 6 checkpoint about 0.5 node-hour; Task 13 benchmarks about
  10–20 node-hours.
- Agent check (Task 14): 10 headless sessions, estimated 0.3–0.8M tokens; a one-session pilot comes first and the
  estimate is redone from it.
- Every compute run and the agent check need Nikolay's explicit go-ahead first.

## Implementation Steps

### Task 1: Toolchain and workspace skeleton

**Files:**
- Create: `env.sh`
- Create: `Cargo.toml`
- Create: `crates/cdors-core/Cargo.toml`, `crates/cdors-core/src/lib.rs`
- Create: `crates/cdors/Cargo.toml`, `crates/cdors/src/main.rs`
- Create: `.gitignore`

- [x] install rustup with `RUSTUP_HOME=/work/ab0995/a270088/rust/rustup`, `CARGO_HOME=/work/ab0995/a270088/rust/cargo`
      and `--no-modify-path` (no edits to shell profiles); stable toolchain
- [x] write `env.sh`: the two variables above, `PATH`, `CARGO_TARGET_DIR=/work/ab0995/a270088/cdors-target`,
      `CDORS_CACHE=/work/ab0995/a270088/cdors-cache`, `module load cdo/2.6.0`, and the netCDF-C and HDF5 locations of
      the spack netCDF-C (`nc-config --prefix`, its HDF5 dependency) for the `-sys` crates — not mambaforge's HDF5
- [x] extract the CDO reference source to `/work/ab0995/a270088/cdors-ref/cdo-2.6.5` (outside home)
- [x] create the workspace (library `cdors-core`, binary `cdors`) with the dependencies listed under Context; `git init`
      the repository locally (no remote)
- [x] `cargo build` succeeds, `cdors --version` runs, and `ldd` shows a single HDF5 library (no tests in this task)

### Task 2: Day-1 decisive experiment — cdo baseline and raw read throughput (GATE)

**Files:**
- Create: `bench/datasets.md`
- Create: `bench/baseline.sbatch`
- Create: `crates/cdors-core/examples/read_probe.rs`
- Create: `docs/baseline.md`

- [x] choose and record datasets in `bench/datasets.md`:
      W1 a daily 2D variable on HEALPix in a **genuine Zarr store on Lustre** (nextGEMS Cycle 3), a decade, and check
      that cdo can read it through NCZarr (if not, the cdo baseline uses the original files of the same data);
      W2 a daily or hourly 2D variable available both in the EERIE cloud and as raw files on Lustre (read through
      kerchunk from Task 10 on);
      W3 daily SST on a **regular or curvilinear** source grid (cdo's bilinear weights refuse unstructured sources);
      W4 a multi-TB variable in a store **chunked in space**, so that percentile passes read disjoint chunks
- [x] write `read_probe.rs`: read and decode every chunk of one variable through `zarrs` with N threads, from the
      filesystem (W1) and over HTTPS (W2 in the EERIE cloud); report compressed and decoded GB/s
- [x] write `baseline.sbatch`: on one exclusive compute node, run cdo for W1–W4 under `/usr/bin/time -v`; for the
      read-time share, run each workload's single operator again with `-P 1 -T` (cdo's timers need one process and one
      thread); then `read_probe` on W1 (local) and W2 (cloud)
- [x] **ask Nikolay before submitting** (about 1–2 node-hours); submit, report the job id, end the turn
      (approved 2026-10-09; job 27994857, 29 min, 0.5 node-hours)
- [x] write `docs/baseline.md`: wall time, CPU time, peak memory, cdo's read-time share, probe throughput
- [x] **gate:** if the probe's parallel read-and-decode throughput on W1 is not clearly above cdo's effective
      throughput (target at least 5×), stop and discuss before Task 3 (fallbacks: a Rust reader inside CDO, or a
      narrower scope); revisit the pass marks in Task 13 with these numbers
      — **passed**: on cold W1 decades the probe reads 16.4–17.2 GB/s decoded, cdo 0.13 GB/s cold and 0.93 GB/s
      warm (≈ 130× and 18×); W4 10.1 against 0.19 GB/s (≈ 50×)
- ➕ for Task 13: 128 reads in flight by default inside Slurm jobs (one node saturates at about 120; the default
      of 64 gives about half of that), checked by one extra W1 run; cdo on the full W4 extrapolates to ≈ 1.6 h for
      `ydaymean` and ≥ 5.3 h for `timpctl,95`; the EERIE cloud cap (0.15–0.2 GB/s) holds from a compute node too
- ⚠️ preliminary login-node probe (2026-10-09, `docs/baseline.md`): W1 cold 9.6 GB/s decoded with 64 reads in
      flight vs cdo ≈ 0.95 GB/s (≈ 10×; only ≈ 3× with 16 synchronous threads); W4 8.9 vs 0.18 GB/s. I/O concurrency
      must be a separate, larger knob than the decode pool. The EERIE cloud `/kerchunk` endpoint stays at ≈ 0.19 GB/s
      whatever the concurrency, so the pass mark "remote ≥ half of local" needs revisiting with Nikolay.
      `bench/baseline.sbatch` is ready (≈ 0.4–0.6 node-hours) and waits for his go-ahead.

### Task 3: Data model, CF time, CLI parser and first readers

**Files:**
- Create: `crates/cdors-core/src/error.rs`
- Create: `crates/cdors-core/src/model/{mod.rs,dataset.rs,grid.rs,zaxis.rs,time.rs}`
- Create: `crates/cdors-core/src/io/{mod.rs,zarr.rs,netcdf_fallback.rs}`
- Create: `crates/cdors-core/src/ops/{mod.rs,info.rs}`
- Create: `crates/cdors/src/parse.rs`
- Modify: `crates/cdors/src/main.rs`

- [x] `error.rs`: error codes, hints and exit codes (0 ok, 1 usage, 2 data, 3 retryable I/O, 4 refused by a limit or
      an existing output), rendered as text or as JSON
- [x] `parse.rs`: CDO chain grammar for fixed-arity operators (`-op,arg1,arg2` prefix chains, inputs, output) and the
      global options `-O -P -f -b --json --plan --mem --max-read --chunks --timestat_date --percentile --no_history
      --progress`; unknown operators get a did-you-mean hint
- [x] `model`: dataset, variable and dimension roles (time, vertical, horizontal; other dimensions listed but rejected
      by operators); grid kinds (regular, Gaussian, curvilinear, unstructured, HEALPix detected as in CDO); vertical
      axis with layer bounds; CF time with calendars standard (1582 switch), proleptic_gregorian, noleap/365_day,
      all_leap/366_day, 360_day, julian, stored as seconds since the reference date
- [x] `io`: the chunk-source interface (metadata, plus "chunk i of variable v" returning compressed bytes and codecs);
      Zarr through `zarrs` on the filesystem; NetCDF through the `netcdf` crate as the first, serial reader
- [x] `ops/info.rs` and the operator registry: `sinfo` (text and `--json`), `showname`, `griddes` (CDO's text format)
- [x] verification is Task 4's first rows (code first, then rows)

### Task 4: Fixtures and the cdo-comparison harness

**Files:**
- Create: `tests/make_fixtures.sh`
- Create: `tests/run_cases.sh`
- Create: `tests/cases.txt`

- [x] `make_fixtures.sh`: tiny F32 inputs generated by cdo into `$CARGO_TARGET_DIR/fixtures` — `r36x18`, `hpz2`,
      unstructured (`setgridtype,unstructured`); three years daily (`duplicate`, `settaxis`, `setcalendar`); values
      varying in space and time (`expr` with coordinate and time functions; a tiny generator only if cdo can't express
      it); missing values (`setrtomiss`); a two-level variant; Zarr v2 and v3 copies of `hpz2_noleap` written by
      xarray from mambaforge
- [x] `run_cases.sh`: the loop described under Testing Strategy — cached cdo references, `cdo --pedantic diffn` with
      per-row `abslim`, `showtimestamp` comparison, `text` and `err:` tags, rows in parallel, summary line, non-zero
      exit on failure; the Zarr and planner-check variants are switched on in Task 5
- [x] rows for `showname` and `griddes` on all fixtures; `sinfo` is checked only for valid JSON (its text format is
      CDO's and not compared)
- [x] run `tests/run_cases.sh` — must pass, in seconds

### Task 5: Engine core — planner stages, selections, pointwise operators, writers

**Files:**
- Create: `crates/cdors-core/src/plan/{mod.rs,stage.rs,tiling.rs}`
- Create: `crates/cdors-core/src/exec/{mod.rs,pipeline.rs}`
- Create: `crates/cdors-core/src/io/{write_netcdf.rs,write_zarr.rs}`
- Create: `crates/cdors-core/src/ops/{select.rs,arith.rs,files.rs}`
- Modify: `tests/run_cases.sh`, `tests/cases.txt`

- [x] operator interface: `describe` (input description to output description, no data touched) and an access class
      (selection, pointwise, reduction, whole-extent); registry entries carry arguments, types, defaults
- [x] planner v0: chain to stages; selections become index sets per dimension and a list of chunks to read; pointwise
      operators merged into the stage; tiles aligned to chunk boundaries; any running state is carried across chunk
      boundaries in a fixed order, never merged from per-chunk partials
- [x] executor: fetch, decode, compute, write as a pipeline with bounded queues; async I/O runtime plus a rayon pool;
      `-P` caps threads; ➕ reads in flight are a separate knob (default 64; lower on login nodes), per the probe
- [x] writers: output format from the suffix (`.nc` NetCDF-4, `.zarr` Zarr v3) unless `-f` says otherwise (`nc4`,
      `zarr`, `zarr2`); NetCDF-4 via `netcdf`; Zarr via `zarrs` (zstd, about 4 MB chunks with one timestep each,
      `--chunks`); write to a temporary name and rename on success; refuse an existing output without `-O`;
      `history` unless `--no_history`
- [x] operators: `selname`, `sellevel`, `seltimestep`, `seldate`, `selyear`, `selmon`, `selseason`, `sellonlatbox`
      (coordinates; HEALPix cells via `cdshealpix`), `add`/`sub`/`mul`/`div` (second input with one timestep is
      broadcast, as in CDO), `addc`/`subc`/`mulc`/`divc`, `ifthen`, `copy`, `setgrid`
- [x] switch on the Zarr-input variant and the planner check (tiny chunks only; `--mem` comes in Task 7)
- [x] rows for every operator above, plus `err:` rows for an unknown operator and an existing output without `-O`
- [x] run `tests/run_cases.sh` — must pass before Task 6

### Task 6: Time statistics by period and the first speed checkpoint

**Files:**
- Create: `crates/cdors-core/src/ops/timstat.rs`
- Modify: `crates/cdors-core/src/model/time.rs`, `crates/cdors-core/src/plan/stage.rs`
- Create: `docs/deviations.md`
- Modify: `tests/cases.txt`

- [x] kernel: per-cell state (sum, count, min, max, second moment) folded one timestep at a time in time order, carried
      across time chunks, cells processed in parallel; groups keyed by period in the variable's calendar; a group is
      finalised and written as soon as it closes; NaN-aware
- [x] read CDO's variance code in the reference source and port it if it is numerically sound; otherwise use Welford's
      update (sequential, so no merge formula is needed) and record the deviation in `docs/deviations.md`
- [x] operators: `tim`, `day`, `mon`, `seas`, `year` × `mean`, `min`, `max`, `sum`, `std`, `std1`, `var`, `var1`;
      `ymon*` and `yday*` (multi-year groups)
- [x] output time axis: `--timestat_date` rules, `time_bnds`, `cell_methods`
- [x] rows: every period with `mean` on all three fixtures (the calendars matter); every statistic once with `tim` on
      one fixture; `ymonmean` and `ydaymean` on all three
- [x] run `tests/run_cases.sh` — must pass
- [ ] **checkpoint:** with Nikolay's go-ahead (about 0.5 node-hour), run W1 with cdors and cdo on one node; if cdors
      is not on track for 5×, investigate before Task 7
- ⚠️ 2026-10-09: the grouping and output-time-axis rules exist in `model/timegroup.rs` (GroupTracker, ClimTracker)
      and match cdo 2.6.0 in 990 checks (timestamps and `time_bnds`, all `--timestat_date` values, seasons with
      `CDO_SEASON_START`, Feb 29 in `yday*`). Kernels and operator wiring still to do. CDI rewrites timestamps when
      time units are months/years since (`taxis.c:1033`); writers must reproduce it if such units are written.
- ⚠️ 2026-10-09: merged. Login-node speed: `yearmean` of 2 years of W1 tas in 1.4–1.9 s (4.9–6.7 GB/s) vs cdo 9.25 s
      (1.0 GB/s), identical values. Variance uses Welford (cdo: one-pass sum of squares), ≤ 1–2 float32 ulp, logged in
      `docs/deviations.md`. Gaps handed to Task 7: chaining after a statistic is refused (multi-stage chains with
      in-memory intermediates needed); data with one field per chunk folds serially (one lane) — split chunks into
      finer tiles for fold stages; `ydaymean` at zoom 9 needs ≈ 14 GB (multi-pass). The compute-node checkpoint
      still needs Nikolay's go-ahead.

### Task 7: Whole-extent time operators, memory budget and multi-pass

**Files:**
- Create: `crates/cdors-core/src/ops/{pctl.rs,runstat.rs}`
- Modify: `crates/cdors-core/src/ops/arith.rs`, `crates/cdors-core/src/plan/tiling.rs`
- Modify: `tests/run_cases.sh`, `tests/cases.txt`, `docs/deviations.md`

- [x] `timpctl`, `monpctl`, `yearpctl` computed exactly with cdo's default nearest-rank definition, `--percentile`
      honoured for cdo's other methods; accept CDO's three-input form and never evaluate the min/max inputs; record in
      `docs/deviations.md` that groups above 50 values differ from cdo's histogram result by up to one bin
- [x] `runmean` with CDO's output timestamps; `ymonsub` matching the climatology by month
- [ ] memory budget: `--mem`, defaulting to a fraction of the Slurm allocation or of free memory; tile sizing; several
      passes over the input when the state doesn't fit (percentiles, `ydaymean` with 366 open groups), each pass
      reading only its own spatial chunks on space-chunked data
- [ ] intermediate results between stages must fit in memory; otherwise fail with `intermediate_too_large` and a hint
      (no spilling to disk in the prototype)
- [ ] switch on `--mem 1M` in the planner-check variant
- [x] rows: `monpctl` (`exact`), `timpctl` (`bin`), `runmean`, `ymonsub`
- [ ] run `tests/run_cases.sh` — must pass
- ⚠️ 2026-10-09: exact percentile kernel (`kernels/percentile.rs`, branch `worktree-agent-a1b8b4bdf4f834f6b`, merge
      after Task 5) supports all 16 cdo methods; `monpctl` matches cdo to ≤ 1 ulp (99.9 % bit-identical), `timpctl` nrank
      within 0.96 histogram bins. cdo bug to record in `docs/deviations.md`: for hazen, weibull, median_unbiased and
      normal_unbiased near p = 100 cdo reads `x[n]` past its buffer (zeroed memory) and returns `(1−h)·x[n−1]`;
      cdors returns `x[n−1]`.
- ⚠️ 2026-10-09: operators merged (pctl for all periods, run*, ymon/yday/yseas arithmetic); percentiles are now
      bit-identical to cdo 2.6.0 for all 16 methods (cdo's build uses fused multiply-adds; cdors matches with
      mul_add). Memory budget, lane waves, multi-pass, multi-stage chains and finer tiles: planner agent running.

### Task 8: Space statistics

**Files:**
- Create: `crates/cdors-core/src/ops/fldstat.rs`
- Modify: `crates/cdors-core/src/model/grid.rs`, `crates/cdors-core/src/model/zaxis.rs`
- Modify: `tests/cases.txt`

- [x] cell areas: from the file if present, otherwise from cell bounds (spherical polygon area), otherwise analytic
      (regular, HEALPix)
- [x] `fldmean`, `fldmin`, `fldmax`, `fldsum`, `fldstd` with CDO's weighting; running sums carried across spatial
      chunks in cell order, timesteps processed in parallel
- [x] `zonmean` (regular grids by latitude row; HEALPix by iso-latitude ring if cdo supports it as a reference,
      otherwise document) and `vertmean` (layer-thickness weights as in CDO)
- [x] rows: `fldmean` on all three fixtures, the other statistics once each, `zonmean`, `vertmean`
- [x] run `tests/run_cases.sh` — must pass
- ⚠️ 2026-10-09: `model/area.rs` (merged) reproduces `cdo gridarea`/`gridweights` to ≤ 5e-15 relative on 13 cases and
      gives 0-ulp fldmean/fldstd/zonmean/mermean/vertmean; weighting rules per operator family are in the module
      (fld mean/std/var weighted, min/max/sum not; zon* per latitude row or HEALPix ring; vert* by layer thickness).
- ⚠️ 2026-10-09: merged. W1 fldmean over 720 steps: cdors 2.5 s (3.6 GB/s) vs cdo 10.0 s, identical output.
      Gap: a statistic must still be the outermost operator (chaining after it is refused) — Task 7.

### Task 9: Remapping with cdo-generated weights, and HEALPix degrade

**Files:**
- Create: `crates/cdors-core/src/io/scrip.rs`
- Create: `crates/cdors-core/src/ops/{remap.rs,healpix.rs}`
- Modify: `crates/cdors-core/src/model/grid.rs`
- Modify: `tests/cases.txt`

- [x] read SCRIP weight files; apply them in parallel over target cells with double-precision sums and CDO's
      missing-value handling
- [x] `remapnn`, `remapdis`, `remapbil`, `remapcon`: run `cdo gen<method>,<grid>` once per (source grid, target grid,
      method), writing a small source-grid NetCDF first when the input is remote Zarr; cache weights in
      `$CDORS_CACHE/weights/` keyed by a hash; if `cdo` is missing, fail with a hint to use `remap,<grid>,<weights.nc>`
- [x] `remap,<grid>,<weights.nc>` for any SCRIP file
- [x] target grids: CDO names (`r<nx>x<ny>`, `global_<inc>`, `hpz<zoom>`, `hp<nside>`), grid description files,
      another dataset's grid
- [x] `hpdegrade`: exact average over nested children, NaN-aware
- [x] rows: `remapnn`, `remapdis`, `remapcon` on all fixtures, `remapbil` on the regular and HEALPix fixtures only
      (cdo refuses unstructured sources), `hpdegrade` against cdo
- [x] run `tests/run_cases.sh` — must pass
- ⚠️ 2026-10-09: `remap/` core matches `cdo remap` within 1 float32 ulp with identical missing patterns in 60 cases.
      cdo regenerates weights whenever the missing-value mask changes; cdors generates weights for the unmasked grid
      and reproduces cdo per method by renormalising (rules documented at the top of `remap/weights.rs`).
- ⚠️ 2026-10-09: operators merged. Target grids come from a cdo-written template cached next to the weights;
      source grids are hashed by coordinates (NetCDF and Zarr copies share weights). W3 remapbil r360x180 of
      181 days: cdors 0.51 s (-P 16) vs cdo 1.5 s at best, identical output.

### Task 10: Fast local readers — kerchunk, NetCDF-4 chunk index, multi-file inputs

**Files:**
- Create: `crates/cdors-core/src/io/{kerchunk.rs,netcdf4_index.rs}`
- Modify: `crates/cdors-core/src/ops/files.rs`
- Modify: `tests/make_fixtures.sh`, `tests/run_cases.sh`, `tests/cases.txt`

- [x] kerchunk reference store: Parquet first (the format EERIE uses), JSON as well; chunk keys map to
      (file, offset, length) or inline data
- [x] NetCDF-4: list chunk byte ranges once via HDF5 (`hdf5-metno`, `chunk_info`, same HDF5 as netCDF-C), cache the
      index in `$CDORS_CACHE/nc4index/` keyed by path, size and modification time, then read and decode chunks in
      parallel (deflate, shuffle, fletcher32 and the HDF5 blosc filter used by the EERIE files); fall back to
      netCDF-C for filters we can't decode
- [x] `mergetime` over many files or a glob pattern as one virtual dataset (consistent grids checked)
- [x] harness: NetCDF-4 rows also run with the netCDF-C fallback disabled; one kerchunk fixture if a reference
      generator is available in mambaforge, otherwise kerchunk is covered by the benchmarks only; a `mergetime` row
- [x] run `tests/run_cases.sh` — must pass
- [x] ➕ one process-wide HDF5 lock shared by every `netcdf` crate call (fallback reader, NetCDF writer, SCRIP weights
      reader, Nc4Source metadata) and every `hdf5-metno` call (chunk indexing): the spack HDF5 is not thread-safe and
      the two crates' internal locks do not exclude each other (found by the Task 5 agent)
- ⚠️ 2026-10-09: merged engine hardening. zarrs read coordinates through rayon's global pool, which started one thread
      per core (256) per process and panicked at the per-user limit (2048); pools are now sized to the work with
      graceful fallback. `mergetime`/`cat` are wired (overlapping or backwards times refused, logged in deviations).
      Harness: 157 rows pass on both read paths at 8 parallel jobs in ≈ 20 s.

### Task 11: Remote reads

**Files:**
- Create: `crates/cdors-core/src/io/remote.rs`
- Modify: `crates/cdors-core/src/io/zarr.rs`, `crates/cdors-core/src/io/kerchunk.rs`

- [x] HTTPS and S3 through `object_store`: about 64 requests in flight, adjacent byte ranges merged, retries with
      backoff on timeouts and server errors, consolidated metadata used when present
- [x] retryable failures map to exit code 3 with the failing chunk key in the error
- [x] smoke check against the W2 dataset in the EERIE cloud (its `/kerchunk` endpoint; `sinfo --json`, a one-month
      `fldmean`); no network rows
      in the harness, so the suite stays fast and offline
- ⚠️ 2026-10-09: done and merged. The EERIE server answers ranged GETs with 206 but no Content-Length/Content-Range, so
      object_store rejects ranges there and whole objects are read (fine for its per-chunk objects; bad for kerchunk
      refs into large files on such servers). HTTP/1.1 is the default (HTTP/2 was slower at 64 in flight;
      `CDORS_HTTP2=1`). End-to-end check: one month of W2 `pr` copied from the cloud (1.7 s) is identical to the copy
      from the local Parquet refs (0.75 s). Cross-chunk range merging needs a batched read and is a follow-up.

### Task 12: Behaviour for agents — plan output, operator listing, limits, progress, usage docs

**Files:**
- Create: `crates/cdors-core/src/plan/explain.rs`
- Modify: `crates/cdors/src/main.rs`, `crates/cdors-core/src/ops/mod.rs`, `crates/cdors-core/src/error.rs`
- Create: `README.md`
- Modify: `tests/cases.txt`

- [x] `--plan` as text and JSON: stages, merged operators, chunks and bytes to read (compressed and decoded), passes,
      peak memory, weights to build; no wall-time estimate yet
- [x] `cdors ops --json`: implemented operators with arguments, types, defaults and descriptions, plus cdo operators
      not implemented yet **by name only**, extracted once from CDO's `OPERATORS` catalog; `cdors help <op>`
      mirroring `cdo -h <op>` for implemented operators
- [x] `--max-read` guard: low default and a thread cap on login nodes (no `SLURM_JOB_ID`) with an `srun` hint, higher
      inside Slurm jobs; never prompt; on failure remove the run's own temporary output and report how far it got
- [x] `--progress json` on stderr (stage, chunks done, bytes read)
- [x] `README.md` section on agent usage (`--plan`, `--json`, `ops`, error codes, limits) — needed by the agent
      check in Task 14
- [x] rows: `err:read_limit`, `err:no_coordinates` (FESOM-like fixture without coordinates), and `--plan --json`
      parsing as valid JSON
- [x] run `tests/run_cases.sh` — must pass
- ⚠️ 2026-10-09: merged. `--plan` JSON (`"cdors_plan": 1`) with per-stage chunks/bytes (compressed sizes sampled),
      memory estimate, remap weights, read limit; `ops --json` (182 implemented + 543 cdo-only operators), `help`
      with cdo's text plus cdors notes; `--max-read` 64 GB default on login nodes, none in Slurm jobs; `--progress
      json`; README with an agent section. Pass count and memory model to be filled in by the Task 7 planner work.

### ➕ Task 12a: Safety fixes from the code review (BLOCKS benchmarks and the agent check)

- [x] failure cleanup only removes what this process created (unique temp names: host+pid+random,
      `created` flag); atomic no-clobber publish without -O; refuse output == input / inside an input /
      matched by an input glob; no materialised timestep/year ranges (a huge `seltimestep` reached 71 GB)
- [x] panic hook with JSON error and own-temp cleanup; writer death cannot deadlock dispatch;
      retryable only for transient errors; nc4index cache key with inode+ctime; `_Unsigned`, `valid_range`,
      HEALPix order defaults (silent wrong numbers); minor items (JSON warnings, time fill values, missing
      refs.N.parq, dangling-symlink outputs, NaN sellonlatbox args, `--plan` writes no cache)
- ⚠️ 2026-10-09 review (report only) reproduced: `cdors -O -selname,tas in.nc in.nc` replaced in.nc; two
      concurrent runs without -O on one output both exited 0 (one result silently overwritten); cleanup could
      `remove_dir_all` a same-named temp of another process (same PID on another node). Do not use -O on real
      data until this item is merged.
- ⚠️ 2026-10-09 merged (230/230 rows on both read paths). Verified on the merged binary: `-O … in.nc in.nc` is
      refused with in.nc unchanged; `seltimestep,9223372036854775807` returns at once. Lustre: renameat2(NOREPLACE)
      is unusable (EINVAL), so files publish via link()+unlink() and Zarr via an exclusive mkdir reservation.
- [x] ➕ `--plan` still runs cdo to make remap grid templates (needs a native parser for r<nx>x<ny>, global_<inc>,
      hpz<z>, hp<nside>). Fixed: native parser (remap/target.rs) equal to cdo's templates for 24 names; --plan and
      refused runs never call cdo or write caches; output/limit checks run before weight generation
- [x] ➕ zarrs FilesystemStore fsyncs every chunk: a `--mem 1M` Zarr output took 65–80 s vs 0.2 s for NetCDF. Fixed: a wrapper store writes without fsync, one syncfs before publish
      (1M-chunk runs 7–15 s → 1.4–3.6 s; open: the scalar `healpix` mapping variable decodes differently in xarray
      from the zarr2 output than from NetCDF)

### ➕ Task 12b: Performance rounds before the benchmarks

- [x] round 1: allocation churn (buffer pool / malloc tuning), copy-free decode, branch-free kernel
      loops, 64 reads in flight on login nodes
- [x] round 2 (after the Task 7 planner merge): O(1)/parallel tile construction, no blocking on fold-lane locks
      (≥ -P active lanes), NetCDF writer work moved to the compute pool
- [x] ➕ value-printing operators for agents (`outputtab`, `info`/`infon`, `output`/`outputf`, with `--json`): the
      Task 14 prep found that agents had to fall back to ncdump (planned in Task 3, never built)
- [x] ➕ broken-pipe handling: `cdors --version | head -1` panics (agents pipe output into head)
- [x] ➕ remap reads only the source chunks its weights touch (point extraction with remapnn read a whole
      year of every cell: 21 s, 9 GB RSS on the login node)
- ⚠️ 2026-10-09 profile (login node, W1): yearmean reaches ≈ 20 % of the read+decode ceiling warm and 50–62 %
      cold; `MALLOC_MMAP_THRESHOLD_=4G` alone gave ≈ 2× and reached the ceiling cold (5–7× cdo); decode copies
      (`convert`, `trim`) take ≈ 50 % of user time; tile construction caps throughput at 10–15 GB/s; fldmean blocks
      12 of 16 threads on lane locks; `copy` is limited by the single NetCDF writer thread.
- ⚠️ 2026-10-09 round 1 merged (bit-identical outputs): W1 yearmean warm 7 → 14–17 GB/s, cold 4.4 → 9.8 GB/s (the
      probe ceiling); timmean warm 4.6 → 15.8 GB/s; fldmean 3 → 4.1–4.6 GB/s (still lane-limited → round 2).
      glibc needs both mallopt thresholds (mmap 32 MiB max on glibc 2.28, trim off); one alone is worse.
- ⚠️ 2026-10-09 usability merged: info/infon/output/outputf/outputtab on any chain, byte-identical to cdo 2.6.0 where
      compared, `--json` records (missing = null), `--max-values` flood guard (1e6), SIGPIPE → quiet exit 141,
      `--plan` keeps variadic inputs, intermediates freed after their stage, `--max-read` counts only real sources.
      Harness 226/226. Agent-check arm A now prints values with cdors (no ncdump).
- ⚠️ 2026-10-09 round 2 merged (bit-identical; 230/230 both read paths; no-clobber race test: one winner, one
      exit 4 in each of 3 rounds). Login node, merged master: W1 yearmean 2 years 0.77 s (cdo ≈ 9 s), fldmean 240
      steps 0.41 s (cdo 4.0 s); W1 decade plan 1.17 → 0.04 s; agent-check T4 (2020 p95 at Hamburg via
      `remapnn,lon=10.0_lat=53.55`) 1.5 s, 355 MB, 292.11996 K (reference 292.119965 K).

### ➕ Task 12c: Real-data validation

- [x] sweep on real data (report only): W1 HEALPix Zarr, W2 raw blosc NetCDF via mergetime and Parquet refs, W3
      remap, W4 ydaymean, EERIE cloud vs local, native ICON R2B8 and FESOM with -setgrid, HadGEM3 ORCA1 — values
      and timestamps identical to cdo (or exact xarray) in all but three cases; cdors 5–170× faster where reads
      dominate (W4 ydaymean 0.8 s vs cdo 139 s), slower than cdo on ICON R2B8/FESOM because of planning time
- [x] fixes: HEALPix sellonlatbox edge cells (cdo's centre formula), `param` attribute in
      info/outputtab, curvilinear remapbil fallback cell (fix or document), planning time on large unstructured
      grids, memory-budget enforcement when the estimate exceeds --mem, re-runnable bench/realdata_check.sh.
      Merged 2026-10-09: cell counts now equal cdo on all boxes; param IDs as CDI; curvilinear fallback rows
      detected (HadGEM3 missing patterns identical); ICON R2B8/FESOM planning 4.3/3.6 s → 0.9 s (cell areas in
      parallel); memory budget enforced (window shrinks or memory_limit), glibc arenas capped at 4

### ➕ Task 12d: Docs accuracy pass and polish

- [x] README and docs/deviations.md checked against the binary (every README example run; agent section rewritten as
      9 steps; --help exit codes completed; wrong ops notes fixed; duplicate deviation entries merged)
- [x] behaviour bugs found by the docs pass: remap to a Zarr/dataset target grid fails on the first runs
      (race after writing the target template); `remap,<grid>,<weights>` should not need CDORS_CACHE; clear error when
      cdo is missing; `--plan` JSON lists glob/mergetime inputs as one string; plan text repeats operators and lists
      timpctl's ignored inputs; `-s` does not silence warnings; `ops --json` uses `cdo_section` and `section`.
      Merged: the "race" was cdo itself segfaulting (two of its threads in non-thread-safe HDF5) — weights are now
      generated with `cdo -L`; warnings follow cdo (`-w` silences them, `-s` does not). Harness 233/233.

### Task 13: Benchmarks W1–W4

**Files:**
- Create: `bench/bench.sh`, `bench/bench.sbatch`, `bench/xarray_baseline.py`
- Create: `docs/bench-results.md`

- [x] `bench.sh`: each workload once per tool; W1 `yearmean` (HEALPix Zarr), W2 `fldmean -sellonlatbox`, W3
      `remapbil` from the regular or curvilinear SST grid with cached weights, W4 `timpctl,95` and `ydaymean` under
      `--mem 32G`; W2 both from Lustre (kerchunk) and from the EERIE cloud — the same-dataset local-versus-remote
      comparison — with xarray + dask + flox as the remote baseline; record wall time, peak memory
      (`/usr/bin/time -v`), bytes read and `cdo diffn` against cdo's result
- [ ] **ask Nikolay before submitting** (about 10–20 node-hours); submit, report job ids, end the turn
- [ ] write `docs/bench-results.md` against the pass marks, revised with the Task 2 numbers (proposed: at least 5× on
      W1/W2, 3× on W3, W4 within 32 GB, remote throughput at least half of local)
- ⚠️ 2026-10-09: scripts merged and smoke-tested (DRY, FIXTURE, PLAN_ONLY modes); `bench/bench.sbatch` estimate
      ≈ 5–6 node-hours, ≤ 8 (mostly cdo's 2 h timeouts on full W4); `WORKLOADS="W1 W2 W3 W4Y"` ≈ 1–1.5 node-hours.
      Python env for the xarray baseline: mambaforge `envs/hk25` (the only one with flox). Waits for Nikolay.
- ➕ 2026-10-09, after the Task 2 baseline (Nikolay approved both changes and the reduced run): cdors defaults to
      128 reads in flight inside Slurm jobs (64 on login nodes and for URLs); `bench.sh` no longer forces
      `--io-threads`, adds two cold W1 runs on other decades (64 vs the default), skips cdo `timpctl` on the full W4
      (`CDO_W4_TIMPCTL=1` runs it; ≥ 5.3 h extrapolated), and makes W3's weights in plan-only mode (without them
      the job's plan check would have stopped the job). Submitted `WORKLOADS="W1 W2 W3 W4Y"` as job 28000341
      (limit 2 h, node l50327 excluded so the baseline's page cache cannot make cold runs warm). The full W4
      (≈ 2.5 node-hours) waits for a separate go-ahead.

### Task 14: Agent check

**Files:**
- Create: `bench/agent_tasks.md`, `bench/agent_check.sh`
- Create: `docs/agent-check.md`

- [x] five plain-language tasks covering climatologies and anomalies, regional means, model–obs comparison on a common
      grid, and percentiles; reference answers computed beforehand with cdo or xarray; each reads at most a few tens
      of GB
- [x] `agent_check.sh`: runs `claude -p` headless, one session after another, once with cdors and once with cdo/xarray
      per task; records correctness, number of commands, wall time and tokens
- [ ] **only after Task 13 justifies it, and with Nikolay's go-ahead:** run a one-session pilot, re-estimate the token
      cost from it, then ask again before the remaining nine sessions
- [ ] write `docs/agent-check.md`
- ⚠️ 2026-10-09: tasks, independent reference answers (xarray, cross-checked with cdo) and the runner are merged
      (`bench/agent_tasks.md`, `bench/agent_check.sh`: dry run by default, `RUN=1 TASKS=T5 ARMS=A` is the pilot).
      Estimated 0.6–1.2M fresh tokens + 3–8M cache reads for 10 sessions. EERIE prompts use the kerchunk/cloud
      time axis (raw files are labelled 41 years later). Open for Nikolay: T3's global mean barely depends on the
      remapping (no remap misses by ≈ 2× the tolerance) — keep or change.

### Task 15: Verify acceptance criteria

- [ ] the four prototype criteria checked against `docs/baseline.md`, `docs/bench-results.md`, `docs/agent-check.md`
- [ ] `tests/run_cases.sh` passes in full, in seconds
- [ ] `docs/deviations.md` lists every known difference from cdo

### Task 16: [Final] Documentation

- [ ] `README.md`: building with `env.sh`, weights and index caches, link to `docs/deviations.md`, known limits of the
      prototype (no rechunking stage, no intermediate spills, no GRIB)
- [ ] record new project facts in the Claude project memory if any
- [ ] move this plan to `docs/plans/completed/`

## Technical Details

**Workspace layout.**

```
Cargo.toml                 workspace
env.sh                     toolchain, build dir, caches, cdo module, spack netCDF/HDF5 (nothing in home)
crates/cdors-core/src/
  error.rs                 codes, hints, exit codes, JSON rendering
  model/                   dataset, grid, zaxis, time (calendars)
  io/                      chunk-source interface, zarr, kerchunk, netcdf4_index, netcdf_fallback, remote,
                           scrip, write_netcdf, write_zarr
  ops/                     info, select, arith, timstat, pctl, runstat, fldstat, remap, healpix, files
  plan/                    stages, tiling, explain (--plan)
  exec/                    pipeline
crates/cdors/src/          main.rs (options, dispatch), parse.rs (CDO chain grammar)
tests/                     cases.txt, run_cases.sh, make_fixtures.sh
bench/                     datasets.md, read_probe, baseline/bench sbatch, bench.sh, xarray_baseline.py, agent check
docs/                      baseline.md, deviations.md, bench-results.md, agent-check.md, plans/
```

**Processing flow.** Parse the chain → open inputs lazily (metadata only) → each operator maps its input description to
an output description → the planner forms stages, turns selections into chunk lists, picks tiles and passes within the
memory budget → `--plan` prints and stops here → the executor streams tiles through fetch, decode, compute and write
with bounded queues → the output is renamed into place on success.

**Operator access classes.**

| Class | Examples | How it runs |
|---|---|---|
| selection | `selname`, `seldate`, `sellonlatbox` | index sets; decides which chunks are read |
| pointwise | `add`, `mulc`, `ifthen` | merged into the surrounding stage |
| reduction | `yearmean`, `ymonstd`, `fldmean`, `vertmean` | state folded in a fixed order, carried across chunk boundaries; groups emitted when closed |
| whole extent of one dimension | `timpctl`, `runmean`, `remap*` | tiles complete along that dimension; several passes if the state doesn't fit |

**Values and missing data.** Packed integers are unpacked on read; missing values are NaN inside the engine and become
the variable's `_FillValue` on write; accumulation in f64; output keeps the input type unless `-b F64`.

**Determinism.** Partial results are never merged. Time reductions fold one timestep at a time in time order, with
cells in parallel; space reductions fold cells in index order, carrying running sums across spatial chunks, with
timesteps in parallel. Output is therefore bit-identical for any thread count, chunk layout and tiling, which the
planner check in the harness enforces.

**Error format and exit codes.**

```json
{"error": "unknown_operator", "operator": "yearmeans", "hint": "did you mean yearmean?"}
{"error": "no_coordinates", "variable": "temp", "hint": "FESOM output needs -setgrid,<mesh file>"}
{"error": "read_limit", "bytes": 8.2e12, "limit": 2e12, "hint": "narrow with seldate/sellonlatbox or raise --max-read"}
```

| Exit code | Meaning |
|---|---|
| 0 | success |
| 1 | usage error (unknown operator, bad arguments) |
| 2 | data error (missing coordinates, unsupported grid or dimension, intermediate too large) |
| 3 | I/O error, worth retrying |
| 4 | refused: read limit, or existing output without `-O` |

**Caches** (all under `$CDORS_CACHE`, on `/work` via `env.sh`): `weights/` (SCRIP files from `cdo gen*`, keyed by a
hash of source grid, target grid and method) and `nc4index/` (NetCDF-4 chunk indexes, keyed by path, size, mtime).

**Deliberate deviations from cdo** (start of `docs/deviations.md`): no silent overwrite or append without `-O`; exact
percentiles also for groups above 50 values, where cdo uses histograms (CDO's three-input form accepted, min/max
inputs ignored); `cell_methods` added to statistics output; NaN as the internal missing value.

## Post-Completion

*Items requiring decisions or actions outside this repository — no checkboxes.*

**Decision after the prototype.** Whether the results earn a second round, and with what: a rechunking stage for data
stored one complete field per chunk (GRIB, NetCDF written per timestep), which percentiles and daily climatologies on
such data need; spilling oversized intermediates to scratch; `expr`, `trend`/`regres`, correlations, ETCCDI indices,
daily percentile climatologies, `intlevel`, ensemble statistics, EOFs, attribute editing; GRIB input (gribscan-style
index plus an ecCodes codec); native weight generation (port CDO's generators or link YAC); a native NetCDF-3 reader;
bracket syntax; an MCP or JSON-plan layer; packaging for conda/pip.

**Manual verification.** Nikolay uses the tool on a few real analyses of his own (EERIE IFS-FESOM, DestinE) and notes
where it surprises him or an agent.

**External.** A final name before anything is published; a public repository only with the exact text approved
first; possibly a conversation with the CDO developers at MPI-M about overlap with their Zarr work.
