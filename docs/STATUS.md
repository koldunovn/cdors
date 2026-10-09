# cdors: status at the end of the prototype (2026-10-09)

Briefing for Nikolay. Everything below is committed on `master` in `~/cdo` (local repository, no remote).
The plan with all checkboxes and per-task notes: `docs/plans/completed/20261008-cdors-prototype.md`. How the
prototype meets its four criteria: `docs/criteria.md`.

## In short

- **The prototype is complete: all 16 plan tasks are done.** It reads Zarr v2/v3 (local and HTTPS/S3), kerchunk
  references (JSON and Parquet), NetCDF-4 (parallel, without HDF5 after a one-time chunk index) and multi-file inputs,
  and runs 187 CDO operators.
- **The four criteria** (`docs/criteria.md`): faster than cdo, met (4.4–72×, results match); remote Zarr practical,
  but the "half of local throughput" mark is not met, because the EERIE server caps every client at ≈ 0.2 GB/s
  (cdors is 1.2–1.3× xarray there; S3 was never tried on a real bucket); agents, met in a small check; bounded memory,
  met at 1.1 TB (the 13 TB store was not run).
- Correctness: every operator is compared with cdo 2.6.0 by `tests/run_cases.sh` (237 rows, about 30 s, both
  NetCDF read paths, and a planner check that forces tiny chunks, tiny memory and several passes and must give
  bit-identical output). The real-data check (`bench/realdata_check.sh`, 23 cases on small slices of W1–W4, ICON
  R2B8, FESOM, HadGEM3 ORCA1, the EERIE cloud), last run on the final binary: 17 identical, 2 within tolerance (a percentile
  vs cdo's histogram method; a mean vs numpy at 6.6e-10 relative), 4 plan/memory checks ok, 0 different.
- Speed on the login node (indications, not the benchmark): typically 5–30× faster than cdo where reading
  dominates (up to 170× on W4 `ydaymean`); on tiny inputs cdo is faster, because cdors has ≈ 0.2–0.3 s of start-up
  and planning overhead.
- Slurm: the baseline (job 27994857), W1–W3 plus one year of W4 (job 28000341), and W2, W3 plus the full W4
  (job 28007818) ran with your go-ahead, 3.1 node-hours together. Every speed mark is met (W2 10.8×, W3 8.6×, W4
  `ydaymean` 72× on 1.1 TB within 23.7 GiB). Nothing was deleted, nothing was pushed.
- Agent check (2026-10-09): all ten headless sessions correct, five tasks with cdors and five with cdo/Python. With
  cdors the agents needed 704 s of wall time against 1684 s, but 2.7× the tokens ($2.73 against $1.57), mostly
  for reading the docs (`docs/agent-check.md`). It also found a wrong reference (T3), now corrected.

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
| Memory | `--mem` budget with lane waves and multi-pass; `ydaymean` on 2 months of W4 ran in 0.65 GB under `--mem 1G` (9 waves) |
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
| Benchmark on a compute node (job 28000341): W1, W2 Parquet / raw, W3 | 7.8 s, 4.1 / 7.5 s, 5.3 s | 42.8 s, 36.4 s, 19.3 s (warm) | 5.5×, 8.9× / 4.9×, 3.7×; `docs/bench-results.md` |
| Same job, W4Y (2020): `timpctl,95`, `ydaymean` | 8.6 s, 19.7 s | 416 s, 86 s | 48.6×, 4.4×; cdo's `ydaymean` is wrong on 26 days (cdo bug) |
| Job 28007818 (after the planning fix): W2 raw, W3 `remap` / `remapbil` | 3.56 s, 2.76 / 2.36 s | 38.5 s, 23.7 s | 10.8×, 8.6× / 10.0×; all pass `diffn` |
| Same job, full W4 (1.1 TB, 87544 steps, `--mem 32G`): `ydaymean`, `timpctl,95` | 78.8 s, 332 s | 5673 s, not run | 72×, 37–57× extrapolated; 14.3 / 23.7 GiB peak |

## Waiting for your decision

1. ~~Day-1 baseline job~~ — **done** 2026-10-09 (job 27994857, 29 min, 0.5 node-hours). **Gate passed:** on
   cold W1 data the read probe reaches 16–17 GB/s decoded, cdo 0.13 GB/s cold and 0.93 GB/s warm (≈ 130× / 18×);
   W4 10.1 against 0.19 GB/s. Details and what follows from them: `docs/baseline.md`, last section.
2. **Benchmarks.** W1–W3 and one year of W4: **done** 2026-10-09 (job 28000341, 18 min, 0.3 node-hours), see
   `docs/bench-results.md`.
   - Every pass mark met except W2 on the raw files, 4.9× against 5×. The cause was planning, which opened 240
     files through netCDF-C one after another. **Fixed** (0415117): plan 6–9 s → 1.0–1.2 s on the login node.
   - Remote vs local is 0.05 because of the server cap; cdors is 1.3× faster than xarray on the same endpoint.
   - The one failed comparison is a cdo bug. On HEALPix Zarr, cdo rotates the steps of a partial last time
     chunk without any message; cdors matches zarr-python.
   - ~~Submitted~~ **done**: W2, W3 and the full W4 as job 28007818, 2026-10-09 16:04–18:24 (2.3 node-hours).
     W2 on the raw files is now 10.8× (pass), W3 8.6× / 10.0×, the full W4 `ydaymean` 72× and within 14.3 GiB,
     `timpctl` within 23.7 GiB; all 8 comparisons pass, and cdors' full-W4 percentiles match numpy on 1536 cells
     to 0.0008 K. The W4 view is cut to 87544 steps (whole chunks), because cdo cannot read the store's last 120
     steps. Details: `docs/bench-results.md`, last section.
3. ~~Agent check~~ — **done** 2026-10-09 with your go-ahead: the pilot plus nine sessions, all correct, 0.36M
   fresh tokens + 3.0M cache reads ($4.29 at list price) in total. Details and what follows: `docs/agent-check.md`.
4. ~~Tasks 15 and 16~~ — **done** 2026-10-09 with your go-ahead: `docs/criteria.md`, `docs/deviations.md`
   completed (and one wrong entry corrected), README status, plan moved to `docs/plans/completed/`. Found and
   fixed on the way: cdo options cdors lacks (`-z zip`, `-k`, ...) were taken for operators, and `-f nc` wrote
   CDF-1 instead of cdo's 64-bit offset format.
5. **Next round or not** — your call; the candidates are in the plan's Post-Completion and under Known gaps below.

## Open questions

- **Remote pass mark.** The EERIE cloud endpoint delivers ≈ 0.19 GB/s whatever the client does, so "remote ≥ half of
  local throughput" cannot be met against it. Proposal: compare cdors and xarray on the same endpoint and report
  throughput relative to the server's cap. The baseline job saw the same cap from a compute node (0.15–0.17 GB/s
  compressed, no faster at 64 requests than at 16); cdo reads the raw files of the same data at 0.15 GB/s.
- **W4 size.** 1.1 TB (PT3H) is set up; the 13.2 TB PT15M store is the stress option.
- ~~cdo timeout on full W4~~ — decided 2026-10-09: cdo `timpctl` on the full W4 is skipped (≥ 5.3 h extrapolated
  from the baseline year; `CDO_W4_TIMPCTL=1` runs it), cdo `ydaymean` keeps its 2 h timeout (≈ 1.6 h expected).
- ~~Reads in flight on compute nodes~~ — done 2026-10-09: 128 by default inside Slurm jobs, 64 on login nodes and
  for URLs; the benchmark compares 64 and the default on two cold W1 decades.
- ~~Agent-check task T3~~ — decided 2026-10-09: kept as it is (skipping the remap still fails, by ≈ 2× the
  tolerance, and the scorer names the variant an answer matches).

## Known gaps (next round)

- Not implemented yet: `expr`, `trend/regres`, correlations, ETCCDI indices, `ydaypctl/ydrunpctl`, `intlevel`,
  ensemble statistics, EOFs, attribute editing, GRIB input (75 of 78 IFS-FESOM2 Parquet sets reference GRIB),
  bracket syntax, an MCP/JSON-plan layer.
- ~~Planning on inputs of many NetCDF-4 files~~: fixed (header cached with the chunk index, members opened
  concurrently). The first run on a file still opens it through netCDF-C.
- ~~`ydaymean` on a whole HEALPix z9 year keeps about 16 of 128 cores busy~~: a fixed cost of ≈ 18 s (the 4.6 GB
  output and 16 GB of daily sums); the full W4 runs at 14 GB/s decoded.
- Full-W4 `timpctl` keeps about 35 of 128 cores busy (139 waves, two reads each); not profiled.
- ~~Output chunks of a statistic run in lane waves were one lane each~~ (W4 `ydaymean`: one day in 384 chunks of
  32 KB across the file, 19× slower to read cold than cdo's output): fixed, chunks span the lanes of a wave and the
  held output is in the memory plan.
- Agents spend most of their extra tokens learning cdors (README + `docs/deviations.md`, 34 kB, read in every
  session): a compact agent-facing reference or an MCP layer would cut that.
- NetCDF output is uncompressed (`-z` is refused); `-f nc4c` writes the same as `nc4`.
- `s3://` inputs are implemented but were never run against a real bucket; remote reads were tested on the EERIE
  cloud only.
- "Multi-TB" memory was shown at 1.1 TB (1.9 TB decoded for percentiles); the 13.2 TB PT15M store was not run.
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
| `/scratch/a/a270088/cdors-realdata/` | ≈ 0.7 GB | runs of the real-data check (each run in its own directory; the last, 0.3 GB, is the Task 15 check) |
| `/work/ab0995/a270088/rust/rustup-init` | 21 MB | installer, no longer needed |
| `~/cdo/.claude/worktrees/` (≈ 30 worktrees) | ≈ 30 MB | agent worktrees; all their branches are merged |
| `~/cdo/nc4index/` | 1.2 MB, 15 files | NetCDF-4 index files written into the working directory by a test with an empty `CDORS_CACHE` (2026-10-09; that bug is fixed); untracked, counts against the home quota |
| `/scratch/a/a270088/cdors-bench/bench-28000341/*.nc`, `bench-28007818/*.nc` | ≈ 11 GB each | benchmark outputs; the `.tsv`, `.md`, `.time` and `.log` files next to them are the record and stay |
| `~/.claude/projects/-scratch-a-a270088-cdors-agentcheck-*` | 10 directories, 72 kB | made by the agent-check sessions despite `--no-session-persistence` (two hold a saved tool output, eight are empty); counts against the home quota |
| `/scratch/a/a270088/cdors-agentcheck/bin-0415117/`, `/scratch/a/a270088/cdors-bin/0415117/` | 146 MB each | frozen cdors binaries of the agent check (written up in `docs/agent-check.md`) |
| `~/cdo/bench/agent/results-*-rescored.{tsv,md}` (6 files) | 20 kB | a botched re-scoring (shifted columns, 2026-10-09); the correct one is `bench/agent/rescored/`; untracked |

Commands, if you want them (check the list first):

```bash
cd /work/ab0995/a270088 && ls -d cdors-target-*            # review
rm -rf /work/ab0995/a270088/cdors-target-{area,catalog,docs,fsync,hard,pctl,perf,perf1,perf2,polish,probe,remap,remote,review,rplan,safety,t10,t10b,t12,t6,t7a,t7b,t8,t9,tg,usab,valid,vfix}
rm -rf /work/ab0995/a270088/cdors-target/runs /scratch/a/a270088/cdors-bench/prelim /work/ab0995/a270088/rust/rustup-init
rm -r ~/cdo/nc4index                                        # stray index files in the repository
rm /scratch/a/a270088/cdors-bench/bench-28000341/*.nc /scratch/a/a270088/cdors-bench/bench-28007818/*.nc
rm -r ~/.claude/projects/-scratch-a-a270088-cdors-agentcheck-*
rm ~/cdo/bench/agent/results-*-rescored.tsv ~/cdo/bench/agent/results-*-rescored.md
rm -r /scratch/a/a270088/cdors-agentcheck/bin-0415117 /scratch/a/a270088/cdors-bin/0415117
cd ~/cdo && git worktree list && git worktree prune          # after removing the worktree directories
```

## Try it

```bash
source ~/cdo/env.sh; C=$CARGO_TARGET_DIR/release/cdors; W1=/work/kd1453/rechunked_ngc4008/ngc4008_P1D_9.zarr
$C ops --json | head -c 300
$C --plan -yearmean -selyear,2020/2021 -selname,tas $W1
$C --json outputtab,date,value -fldmean -sellonlatbox,-10,40,35,70 -selmon,7 -selyear,2020 -selname,tas $W1
```
