# cdors: status

## Round 1: analysis on FESOM2 meshes (2026-10-19)

Briefing for Nikolay. Plan with per-task notes: `docs/plans/completed/20261018-unstructured-round1.md`. The four criteria:
`docs/criteria-unstructured.md`. Nothing of round 1 is pushed, published or installed yet.

**In short.** All 15 tasks are done, and three of the four criteria are met. Correctness, speed and memory are met:
the warm heat-content row passes when measured warm (5.9–6.5 s), and since `25aea2f` the heat content peaks at
1.72 GB under `--mem 2G`. The agent check (job 28244863) is not met on cost: ten of ten answers are correct, and with
cdors the agents took half the time (838 s against 1697 s), but at 1.10× the cost of Python with nereus ($2.24
against $2.03).

- **Built:**
  - **Meshes.** `-setgrid,fesom:<dir>` reads the FESOM2 mesh from `fesom.mesh.diag.nc`:
    - both file layouts, with the units of the node coordinates decided and shown;
    - areas per level, also on the interfaces, and wet levels (dry levels become missing);
    - both FESOM dimension orders, and FESOM 2.0 files without `nz1`;
    - level bounds from `zbar`;
    - a UGRID-Zarr mesh cache, which `--plan` never writes, and `cdors mesh info`.
  - **Regions.**
    - A catalog: AR6, SREX, Giorgi, PRUDENCE and the Niño boxes are embedded; Natural Earth and nereus' basins and
      MOC basins come from a regions directory.
    - `regmask`, `regmin/max/sum/int/mean/std` and `maskregion,regions=`.
    - cdo's `selregion` and `maskregion` with region files.
  - **Bands and histograms:** `zonmean`, `zonsum` and `zonint` with `zonal_<DY>` on any grid with corners and on
    meshes; `fldhist`.
  - **Vertical:**
    - `vert*` with the mesh's layers;
    - `vertint[,z0/z1]` and `volint`/`volmean[,z0/z1]`, in two folds, with the depth range pushed down into the
      read;
    - `intlevel` (and `fld*` after it by the layer that contains each target depth), `bottomvalue`, `fldint`.
  - **Sampling:** `selpoints` and `transect` (nearest node or barycentric); `tonodes` and `toelems`.
  - **Agent surface:**
    - `origin` and `extensions` in `ops --json`;
    - help texts for the 15 cdors operators;
    - new `outputtab` keys;
    - six FESOM2 recipes in `cdors guide`;
    - integrals in float64 with their units.
  - **Read path:** NetCDF-4 chunks without filters are read straight into their element type. From the page cache:
    8.4 → 15.9 GB/s.
  - 207 operators in all (187 before).
- **Correctness:**
  - masks identical to regionmask's and to nereus' Atlantic mask on dart (0 of 924 967 nodes differ);
  - OHC agrees with nereus to 9.1e-15, ice extent to 3.0e-14;
  - all 10 login-node real-data cases pass.
- **Speed** (job 28231368, 0.06 node-h; marks from the gate):
  - Atlantic OHC 0–700 m over 3 years: 8.3 s cold, 8.3× nereus;
  - NH ice extent: 7.1 s, 17.8×;
  - first use of the mesh: 16.3 s (nereus 343 s);
  - warm OHC (job 28236403, only the 16 GB of the chain read between repeats): 5.9–6.5 s, 2.0× nereus warm
    (13.1 s). The first validation's 10.2 s was not warm: the Lustre client's 24 GB cache had been flushed by the
    other runs.
- **Memory** (job 28236588): OHC under `--mem 2G` peaks at 1.72 GB, before `25aea2f` 2.15–2.28 GB. The peak
  came from per-level area reads of the mesh cache holding up to four copies of a level each.
- **Tests:** on `25aea2f`, `cargo test --release` passes (37 tests) and `tests/run_cases.sh` passes 562 of 562
  cases on both NetCDF read paths, with the Zarr and planner variants (109 s on the first run of the build, 30 s
  with `CDORS_NC4=netcdf` after it); `tests/selftest_ref.sh` ok. The cases include 109 rows against references
  (67 NumPy, 28 regionmask, 14 nereus) and 67 `json:` rows.
- **Compute used:** 1.14 node-h in all, and $4.60 of agent sessions:
  - gate 0.28 (job 28228738);
  - validation 0.20 (28230544, 28230657, 28231368);
  - read path 0.06 (28231623, 28231676);
  - Task 14 references 0.08 (28230786);
  - warm re-run and memory 0.09 (28236403, 28236468, 28236548, 28236588);
  - agent check pilot 0.03 (28236330, shared partition) and $0.33;
  - agent check 0.4 (28244863, shared partition, 44 min) and $4.27.

**The agent check (Task 14).**
- **Per task:** cdors is cheaper on the two mesh-weighted series: F1 $0.45 against $0.55, F2 $0.40 against $0.76.
  It is dearer on F3–F5, which one NumPy script answers in 6–7 turns.
- **Where the cdors sessions' extra cost goes:**
  - learning cdors: the guide, operator help, `sinfo`, `mesh info` and `--plan`, 5–6 calls per session;
  - one call per session on threads, which the guide does not cover in a Slurm job;
  - in F3, two failed attempts to print station names from the written file.
- **Spread:** a single session varies by about 15 % (the pilot's F5 cost $0.33 against $0.38 here).
- Details: `docs/criteria-unstructured.md` §3 and `docs/agent-check.md`.

**Open follow-ups.**
1. **Memory estimate:** the peak is still 0.1 GB (6 %) above the plan's estimate (compressed bytes of mesh levels
   being loaded, thread stacks). The budget holds because the tiles do not fill their share; a tighter budget
   would need these counted.
2. **Cold reads:** 2.2–2.5 GB/s against 3.5–4.5 GB/s for a plain `pread` of the same files. Spread the reads over
   the files' storage targets: interleave the chunks of `-cat` and glob members, and read a file's progressive
   layout components at once (`docs/unstructured-results.md`, "Read path").
3. **Agent check with traps:** the 1.10× is accepted for now (2026-10-19). Five tasks that both arms answer
   correctly measure only cost, so the next set should have tasks where agents without cdors get the answer wrong:
   - **weights:** per-level areas, dry levels stored as 0, month lengths;
   - **grids other than regular lon-lat:** elements, reduced Gaussian, HEALPix nested.
   For each task, the naive answer has to fall outside the tolerance, or the task is dropped.
4. **Publication, after you approve the exact text:**
   - the shared install, with `share/regions/` (the regions directory, 12 MB);
   - its README (= `docs/levante.md`, Example 7 and "Three years of FESOM2");
   - the guide page;
   - the push.

**Decisions for you.** Whether to start round 2 (neighbour operators and the first plots). Whether `dcw:` should map
to Natural Earth's countries.

**Cleanup candidates (your call; nothing was deleted).** Sizes from `/usr/bin/du`, measured 2026-10-19 02:10–02:40:

| Path | Size | What |
|---|---|---|
| `<session scratchpad>/rv/.o.nc.cdors-tmp-levante1-1163793-4b168adb` | 51 GB | the temporary output of a killed run (cdors never removes these); it lies under `/scratch/a/a270088/tmp/`, and its exact path is in the round-1 report, not here, because it contains a session id |
| the rest of that session scratchpad directory | 2.4 GB | agents' scratch files |
| `/work/ab0995/a270088/cdors-target-r1-*` (16 directories) | 227 GB together | the agents' build trees, with their fixture and cdo-reference copies, harness run directories and mesh caches |
| | | sizes: t12 35, t4 31, t5 24, t9 20, t10 18, t8 16, readpath 15, t6 15, t11 12, t15 12, t2 11 GB; review 6.2, t3 5.9, t13 4.5, t14 1.7 GB; t1 80 MB |
| `/work/ab0995/a270088/cdors-wt/*` (16 worktrees: t1-gate … t15-docs, review, readpath) | 2.8–4.4 MB each, 63 MB together | the agents' git worktrees and their branches `r1-*` |
| `/scratch/a/a270088/cdors-bench/unst-validate-28230544`, `-28230657`, `-28231368` | 1.2, 2.4, 1.2 GB | Task 13 node runs: outputs, mesh caches, the nereus reference |
| `/scratch/a/a270088/cdors-bench/unst-baseline-28228738` | 77 MB | Task 1 gate run |
| `/scratch/a/a270088/cdors-bench/readpath-28231623`, `-28231676`, `readpath-bin` | 2.0 MB, 2.7 MB, 449 MB | read-path jobs and their candidate binaries |
| `/scratch/a/a270088/cdors-bench/t14`, `t14-refs-28230786` | 1.2, 1.2 GB | Task 14 references; the agent check has run (keep them if the cdors arm is re-run) |
| `/work/ab0995/a270088/cdors-target/frozen/cdors-d47d8fc`, `cdors-32b1dbd` | 150 MB each | frozen binaries; `cdors-32b1dbd` ran the agent check (keep it for a re-run) |

Commands. Check the list first; the worktrees go through git:

```bash
rm <session scratchpad>/rv/.o.nc.cdors-tmp-levante1-1163793-4b168adb   # exact path: round-1 report
cd ~/cdo && for w in /work/ab0995/a270088/cdors-wt/*; do git worktree remove "$w"; done   # then: git branch -D <the r1-* branches>
rm -rf /work/ab0995/a270088/cdors-target-r1-*
rm -rf /scratch/a/a270088/cdors-bench/readpath-bin /work/ab0995/a270088/cdors-target/frozen/cdors-d47d8fc
rm -rf /scratch/a/a270088/cdors-bench/unst-validate-28230544/cache /scratch/a/a270088/cdors-bench/unst-validate-28231368/cache \
       /scratch/a/a270088/cdors-bench/unst-validate-28230657/{cache,cache_meshonly}   # mesh caches inside the records
```

Keep `/work/ab0995/a270088/cdors-target`: the build tree, the shared fixtures with `regions/`, the cdo references and
`frozen/cdors-32b1dbd` for the agent check. Also keep the job records (`*.out`, `runs.tsv`, `compare.json`,
`*.time`, `*.plan.json`) in the scratch directories until the round is closed.

---

# Prototype (2026-10-09)

Briefing for Nikolay. Everything below is committed on `master` in `~/cdo` and published at
https://github.com/koldunovn/cdors.
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
- **GRIB through gribscan references (2026-10-10).** The EERIE IFS-FESOM GRIB data are readable: cdors decodes the
  messages itself (identical to ecCodes) and restores the grid gribscan flattened, so field means are area-weighted
  (0.25° set: 684 months of `fldmean` at 850 hPa in 1.3 s, cdo 2.5.0 13 s, same values; cdo 2.6.0 cannot read these
  messages). Reduced Gaussian grids (the native O1280) are weighted by cell area, where cdo weights them equally.
  Field means that have to use equal weights now say so in a framed warning and in `--plan`.
- Correctness: every operator is compared with cdo 2.6.0 by `tests/run_cases.sh` (262 rows, about 30 s, both
  NetCDF read paths, and a planner check that forces tiny chunks, tiny memory and several passes and must give
  bit-identical output). The real-data check (`bench/realdata_check.sh`, 23 cases on small slices of W1–W4, ICON
  R2B8, FESOM, HadGEM3 ORCA1, the EERIE cloud), last run on the final binary: 17 identical, 2 within tolerance (a percentile
  vs cdo's histogram method; a mean vs numpy at 6.6e-10 relative), 4 plan/memory checks ok, 0 different.
- Speed on the login node (indications, not the benchmark): typically 5–30× faster than cdo where reading
  dominates (up to 170× on W4 `ydaymean`); on tiny inputs cdo is faster, because cdors has ≈ 0.2–0.3 s of start-up
  and planning overhead.
- Slurm: the baseline (job 27994857), W1–W3 plus one year of W4 (job 28000341), and W2, W3 plus the full W4
  (job 28007818) ran with your go-ahead, 3.1 node-hours together. Every speed mark is met (W2 10.8×, W3 8.6×, W4
  `ydaymean` 72× on 1.1 TB within 23.7 GiB). Nothing was deleted.
- Agent check (2026-10-09): all ten headless sessions correct, five tasks with cdors and five with cdo/Python. With
  cdors the agents needed 704 s of wall time against 1684 s, but 2.7× the tokens ($2.73 against $1.57), mostly
  for reading the docs (`docs/agent-check.md`). It also found a wrong reference (T3), now corrected. Re-check
  with `cdors guide` (2026-10-10): 5 of 5, 426 s, $1.98, so 1.3× the cost of cdo + Python and 4× faster.

## What exists

| Area | Operators / features |
|---|---|
| Information | `sinfo` (`--json`), `showname`, `showtimestamp`, `griddes`; `info`, `infon`, `output`, `outputf`, `outputtab` (byte-identical to cdo where compared; `--json` records for agents) |
| Selection | `selname`, `sellevel`, `seltimestep`, `seldate`, `selyear`, `selmon`, `selseason`, `sellonlatbox` (regular, curvilinear, unstructured, HEALPix) — selections decide which chunks are read |
| Arithmetic | `add/sub/mul/div`, `*c`, `ifthen`, `ymon/yday/yseas` × `add/sub/mul/div` |
| Time statistics | `tim/hour/day/mon/seas/year` and `ymon/yday/yseas` × mean, avg, min, max, sum, range, std, std1, var, var1; `run*`; exact percentiles `tim/hour/day/mon/seas/yearpctl` (all 16 cdo methods, bit-identical to cdo where cdo is exact) |
| Space statistics | `fld*`, `zon*`, `mer*`, `vert*` with cdo's area/thickness weights; reduced Gaussian grids by cell area; equal weights only with a loud warning (`--plan` names the source of the weights) |
| GRIB | gribscan's kerchunk references to GRIB2 files (EERIE IFS-FESOM): own decoder (simple and CCSDS packing, bit-maps), regular and Gaussian grids restored as lon-lat, reduced Gaussian grids with their rows, pressure levels in Pa |
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
5. ~~Publication~~ — **done** 2026-10-09 with your go-ahead: the public repository https://github.com/koldunovn/cdors
   (BSD-3-Clause, CDO's license in `LICENSE-CDO`) and a shared install for Levante users in
   `/work/ab0995/a270088/cdors` (guide: `docs/levante.md`).
6. **Next round or not** — your call; the candidates are in the plan's Post-Completion and under Known gaps below.

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
  ensemble statistics, EOFs, attribute editing, GRIB without gribscan references and GRIB1 (ERA5),
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
- `-f nc4c` writes the same as `nc4`. NetCDF output is compressed with deflate only (`-z zip`, since 2026-10-10);
  `-z zstd` is for Zarr.
- `s3://` inputs are implemented but were never run against a real bucket; remote reads were tested on the EERIE
  cloud only.
- "Multi-TB" memory was shown at 1.1 TB (1.9 TB decoded for percentiles); the 13.2 TB PT15M store was not run.
- No rechunk-to-scratch stage: percentiles or daily climatologies on data stored one field per chunk need several
  passes over the input.
- Remap weights are generated by cdo (first run per grid pair needs cdo on the machine; cdors calls it with `-L`,
  because cdo 2.6.0 can segfault when two of its own threads open NetCDF-4 files at once).

## Cleanup candidates (your call)

Done by Nikolay on 2026-10-09: the agent worktrees and their branches, the agents' build trees
(`/work/ab0995/a270088/cdors-target-*`), the rustup installer, `~/cdo/nc4index`, the botched re-score files and the
agent-check session directories under `~/.claude/projects/`. What is left, measured 2026-10-09 22:30 (files on
`/scratch` are also removed by DKRZ's automatic scratch cleanup after a while):

| Path | Size | What |
|---|---|---|
| `/work/ab0995/a270088/cdors-target/runs/` | ≈ 11 MB per run | harness run directories (the harness never deletes; new runs keep adding to it) |
| `/scratch/a/a270088/cdors-bench/{bench-28000341,bench-28007818,baseline-27994857}/*.nc` | 11.5 + 10.7 + 6.6 GB | benchmark outputs; the `.tsv`, `.md`, `.time` and `.log` files next to them are the record and stay |
| `/scratch/a/a270088/cdors-bench/{prelim,fixture-*,plan-20261009-*,plan-check-1}` | ≈ 2.1 GB | early cdo tests and smoke runs of `bench.sh` |
| `/scratch/a/a270088/cdors-realdata/20261009-0*` | ≈ 0.7 GB | the three night runs of the real-data check; the latest run (`20261009-215314-…`) stays |
| `/scratch/a/a270088/cdors-agentcheck/bin-0415117/`, `/scratch/a/a270088/cdors-bin/0415117/` | 140 MB each | frozen binaries of the agent check |

Commands (check the list first):

```bash
rm -rf /work/ab0995/a270088/cdors-target/runs
rm /scratch/a/a270088/cdors-bench/{bench-28000341,bench-28007818,baseline-27994857}/*.nc
rm -r /scratch/a/a270088/cdors-bench/{prelim,fixture-*,plan-20261009-*,plan-check-1}
rm -r /scratch/a/a270088/cdors-realdata/20261009-0*
rm -r /scratch/a/a270088/cdors-agentcheck/bin-0415117 /scratch/a/a270088/cdors-bin/0415117
```

Keep: `/work/ab0995/a270088/cdors/` (the shared install), `/work/ab0995/a270088/cdors-target` (build tree, test
fixtures, cdo references), `/work/ab0995/a270088/cdors-cache` (weights and indexes), the agent-check transcripts in
`/scratch/a/a270088/cdors-agentcheck/2026*` and the job records next to the benchmark outputs.

## Try it

The installed version, for everyone on Levante (guide: `docs/levante.md`, also at
`/work/ab0995/a270088/cdors/README.md`): `export PATH=/work/ab0995/a270088/cdors/bin:$PATH`. The build tree:

```bash
source ~/cdo/env.sh; C=$CARGO_TARGET_DIR/release/cdors; W1=/work/kd1453/rechunked_ngc4008/ngc4008_P1D_9.zarr
$C ops --json | head -c 300
$C --plan -yearmean -selyear,2020/2021 -selname,tas $W1
$C --json outputtab,date,value -fldmean -sellonlatbox,-10,40,35,70 -selmon,7 -selyear,2020 -selname,tas $W1
```
