# cdors: status after the first night (2026-10-09)

Morning briefing for Nikolay. Everything below is committed on `master` in `~/cdo` (local repository, no remote).
The plan with all checkboxes and per-task notes: `docs/plans/20261008-cdors-prototype.md`.

## In short

- The prototype is built through Task 12 of the plan, plus two performance rounds and a safety round. It reads Zarr
  v2/v3 (local and HTTPS/S3), kerchunk references (JSON and Parquet), NetCDF-4 (parallel, without HDF5 after a
  one-time chunk index) and multi-file inputs, and runs about 180 CDO operators.
- Correctness: every operator is compared with cdo 2.6.0 by `tests/run_cases.sh` (233 rows, about 35 s, both
  NetCDF read paths, and a planner check that forces tiny chunks, tiny memory and several passes and must give
  bit-identical output). The real-data check (`bench/realdata_check.sh`, 23 cases on small slices of W1–W4, ICON
  R2B8, FESOM, HadGEM3 ORCA1, the EERIE cloud) on the final master: 17 identical, 2 within tolerance (a percentile
  vs cdo's histogram method; a mean vs numpy at 6.6e-10 relative), 4 plan/memory checks ok, 0 different.
- Speed on the login node (indications, not the benchmark): typically 5–30× faster than cdo where reading
  dominates (up to 170× on W4 `ydaymean`); on tiny inputs cdo is faster, because cdors has ≈ 0.2–0.3 s of start-up
  and planning overhead.
- Nothing was submitted to Slurm, the agent check was not run, nothing was deleted, nothing was pushed.

## What exists

| Area | Operators / features |
|---|---|
| Information | `sinfo` (`--json`), `showname`, `showtimestamp`, `griddes`; `info`, `infon`, `output`, `outputf`, `outputtab` (byte-identical to cdo where compared; `--json` records for agents) |
| Selection | `selname`, `sellevel`, `seltimestep`, `seldate`, `selyear`, `selmon`, `selseason`, `sellonlatbox` (regular, curvilinear, unstructured, HEALPix) — selections decide which chunks are read |
| Arithmetic | `add/sub/mul/div`, `*c`, `ifthen`, `ymon/yday/yseas` × `add/sub/mul/div` |
| Time statistics | `tim/hour/day/mon/seas/year` and `ymon/yday/yseas` × mean, avg, min, max, sum, range, std, std1, var, var1; `run*`; exact percentiles `tim/hour/day/mon/seas/yearpctl` (all 16 cdo methods, bit-identical to cdo where cdo is exact) |
| Space statistics | `fld*`, `zon*`, `mer*`, `vert*` with cdo's area/thickness weights |
| Remapping | `remap,<grid>,<weights>`, `remapnn/dis/bil/con/ycon` (weights made once by cdo and cached), `hpdegrade/hpupgrade`; reads only the source cells the weights use (point extraction is cheap) |
| Files | `copy`, `setgrid`, `mergetime`, `cat` (many files or a glob as one virtual input) |
| Chains | any nesting, incl. statistics of statistics, via in-memory intermediates |
| Memory | `--mem` budget with lane waves and multi-pass; `ydaymean` on 2 months of W4 ran in 1.3 GB under `--mem 1G` |
| For agents | `--plan --json` (what will be read, memory, passes), `ops --json`, `help <op>` (cdo's text + cdors notes), JSON errors with stable codes and exit codes, `--max-read` (64 GB default on login nodes), `--max-values`, `--progress json`, never prompts, README section for agents |
| Safety | output written to a unique temp name and published atomically; never overwrites without `-O`; refuses output = input; failure cleanup removes only what the run created; panics become JSON errors |

Deliberate and observed differences from cdo: `docs/deviations.md` (including four cdo bugs found on the way).

## Numbers so far (login node, indicative)

| Workload | cdors | cdo | Note |
|---|---|---|---|
| W1 `yearmean`, 2 years of HEALPix z9 daily tas (9.4 GB decoded) | 0.77 s | ≈ 9.2 s | values identical |
| W1 `fldmean`, 240 steps | 0.41 s | 4.0 s | identical |
| W1 `-fldmean -ymonmean`, 2 years | 2.3 s | 9.2 s | identical (validation snapshot) |
| W2 raw blosc NetCDF, `monmean -mergetime` over 48 files | 2.8 s | 6.3 s | identical |
| W3 `timmean -remapbil,r360x180 -selmon,1` | 0.8 s | 5.6 s | identical |
| W4 `ydaymean -sellonlatbox… -selmon,1/2` | 0.8 s | 139 s | identical |
| Agent task T4: p95 of 2020 at Hamburg, one command | 0.38–1.5 s | — | 292.11996 K (reference 292.119965 K) |
| HEALPix `timpctl,95` on a box, 1 year (realdata check) | 0.37 s | 9.9 s | identical to exact xarray |
| ICON R2B8 native `fldmean` (realdata check) | 1.9 s | 3.7 s | identical; planning 4.3 → 0.7 s after fixes |
| Raw read ceiling (probe) | 9.6 GB/s cold, 64 reads in flight | cdo ≈ 0.95 GB/s | `docs/baseline.md` |
| Same on a compute node (baseline job, W1 decade, 46 GB) | 16–17 GB/s cold, 128–256 reads in flight | cdo 0.13 GB/s cold (360 s), 0.93 warm | gate passed |
| EERIE cloud over HTTPS | ≈ 0.19 GB/s | cdo cannot read it | server-side cap, independent of concurrency |

## Waiting for your decision

1. ~~Day-1 baseline job~~ — **done** 2026-10-09 (job 27994857, 29 min, 0.5 node-hours). **Gate passed:** on
   cold W1 data the read probe reaches 16–17 GB/s decoded, cdo 0.13 GB/s cold and 0.93 GB/s warm (≈ 130× / 18×);
   W4 10.1 against 0.19 GB/s. Details and what follows from them: `docs/baseline.md`, last section.
2. **Benchmarks W1–W4**: `sbatch bench/bench.sbatch` — ≈ 5–6 node-hours, at most 8 (mostly cdo's 2 h timeouts on
   the full 1.1 TB W4); a cheaper first pass is `WORKLOADS="W1 W2 W3 W4Y"` ≈ 1–1.5 node-hours.
3. **Agent check**: pilot one session first, `RUN=1 TASKS=T5 ARMS=A bash bench/agent_check.sh`, then re-estimate;
   the full check is 10 headless sessions, ≈ 0.6–1.2M fresh tokens plus 3–8M cache-read tokens.

## Open questions

- **Remote pass mark.** The EERIE cloud endpoint delivers ≈ 0.19 GB/s whatever the client does, so "remote ≥ half of
  local throughput" cannot be met against it. Proposal: compare cdors and xarray on the same endpoint and report
  throughput relative to the server's cap. The baseline job saw the same cap from a compute node (0.15–0.17 GB/s
  compressed, no faster at 64 requests than at 16); cdo reads the raw files of the same data at 0.15 GB/s.
- **W4 size.** 1.1 TB (PT3H) is set up; the 13.2 TB PT15M store is the stress option.
- **cdo timeout on full W4** (2 h each for `timpctl` and `ydaymean`); "did not finish" is recorded as a result.
  From the baseline year (×30): `ydaymean` ≈ 1.6 h (should just finish), `timpctl,95` ≥ 5.3 h (cannot finish).
  Skipping cdo's full-W4 `timpctl` and quoting the extrapolation would save up to 2 node-hours.
- **Reads in flight on compute nodes.** One node saturates Lustre at about 120 reads in flight; cdors' default is
  64. Proposal: 128 inside Slurm jobs, checked by one extra W1 run in the benchmark.
- **Agent-check task T3**: its global mean barely depends on the remapping (skipping the remap misses the tolerance
  by only ≈ 2×). Keep, or ask for a regional value instead?

## Known gaps (next round)

- Not implemented yet: `expr`, `trend/regres`, correlations, ETCCDI indices, `ydaypctl/ydrunpctl`, `intlevel`,
  ensemble statistics, EOFs, attribute editing, GRIB input (75 of 78 IFS-FESOM2 Parquet sets reference GRIB),
  bracket syntax, an MCP/JSON-plan layer.
- No rechunk-to-scratch stage: percentiles or daily climatologies on data stored one field per chunk need several
  passes over the input.
- Remap weights are generated by cdo (first run per grid pair needs cdo on the machine; cdors calls it with `-L`,
  because cdo 2.6.0 can segfault when two of its own threads open NetCDF-4 files at once).

## Cleanup candidates (nothing was deleted — your call)

| Path | Size | What |
|---|---|---|
| `/work/ab0995/a270088/cdors-target-*` except `cdors-target` | ≈ 85 GB+ (several dirs not measured) | each agent's private build tree and scratch outputs; regenerable |
| `/work/ab0995/a270088/cdors-target/runs/` | ≈ 11 MB per harness run, many runs | harness run directories (the harness never deletes) |
| `/scratch/a/a270088/cdors-bench/prelim/` | 544 MB | early cdo test outputs |
| `/scratch/a/a270088/cdors-realdata/` | ≈ 0.6 GB | runs of the real-data check (each run in its own directory) |
| `/work/ab0995/a270088/rust/rustup-init` | 21 MB | installer, no longer needed |
| `~/cdo/.claude/worktrees/` (≈ 30 worktrees) | ≈ 30 MB | agent worktrees; all their branches are merged |

Commands, if you want them (check the list first):

```bash
cd /work/ab0995/a270088 && ls -d cdors-target-*            # review
rm -rf /work/ab0995/a270088/cdors-target-{area,catalog,docs,fsync,hard,pctl,perf,perf1,perf2,polish,probe,remap,remote,review,rplan,safety,t10,t10b,t12,t6,t7a,t7b,t8,t9,tg,usab,valid,vfix}
rm -rf /work/ab0995/a270088/cdors-target/runs /scratch/a/a270088/cdors-bench/prelim /work/ab0995/a270088/rust/rustup-init
cd ~/cdo && git worktree list && git worktree prune          # after removing the worktree directories
```

## Try it

```bash
source ~/cdo/env.sh; C=$CARGO_TARGET_DIR/release/cdors; W1=/work/kd1453/rechunked_ngc4008/ngc4008_P1D_9.zarr
$C ops --json | head -c 300
$C --plan -yearmean -selyear,2020/2021 -selname,tas $W1
$C --json outputtab,date,value -fldmean -sellonlatbox,-10,40,35,70 -selmon,7 -selyear,2020 -selname,tas $W1
```
