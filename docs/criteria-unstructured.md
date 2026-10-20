# The four round-1 criteria (analysis on FESOM2 meshes)

Status 2026-10-19, master `25aea2f`. The round-1 plan (`docs/plans/completed/20261018-unstructured-round1.md`, Task 15) sets
four criteria. Below, each is checked against the gate (`docs/unstructured-baseline.md`), the real-data validation
(`docs/unstructured-results.md`) and the test suite. Correctness is met. Speed meets every mark: the warm
heat-content row, a miss by 0.2 s in the first validation, was re-measured warm and takes 5.9–6.5 s. Memory is met
since a fix to the mesh reader: 1.72 GB under a 2 GB budget. The agent check is not met on cost: all ten sessions
answered correctly, and with cdors the agents took half the time, but at 1.10× the cost of Python with nereus.

| | Criterion | Result | Main evidence |
|---|---|---|---|
| 1 | Correct: regionmask masks identical, nereus within tolerance, no silent cell loss | **met** | masks identical to regionmask and nereus' Atlantic mask on dart; OHC 9.1e-15 and ice extent 3.0e-14 from nereus; 562 of 562 harness cases |
| 2 | Speed against the gate's marks | **met** | Atlantic OHC 0–700 m, 3 years, 8.3 s cold (8.3× nereus), 5.9–6.5 s warm (2.0× nereus warm); NH ice extent 7.1 s (17.8×); first use of the mesh 16.3 s (nereus 343 s) |
| 3 | Agents: 5 of 5, at a cost no higher than the Python arm | **not met: cost 1.10×; accepted** | 5 of 5 correct in both arms (job 28244863); cdors 838 s and $2.24, Python with nereus 1697 s and $2.03 |
| 4 | Memory bounded under `--mem` | **met** | OHC under `--mem 2G` peaks at 1.72 GB (2.15–2.28 GB before the fix); ice 1.05 GB |

## 1. Correctness

**Masks.**
- `regmask` equals regionmask 0.13's `mask_3D` cell for cell on five test grids: r36x18, HEALPix z2, an
  unstructured grid, the FESOM2 pi mesh, and a 1° grid with centres on the AR6 outlines. The sets are ar6.ocean,
  ar6.land, srex, prudence (overlapping) and ne.ocean_basins_50 (`ref:regionmask…:exact`).
- On dart, regionmask's ar6.ocean agrees on every 10th node: 316 034 nodes × 15 regions, 0 differ.
- nereus' Atlantic basin holds the same 924 967 nodes as `-maskregion,regions=basins,select=Atlantic_Basin`
  (0 differ).

**Values on real data** (AWI-CM3 `hist1950d`, dart, 3 years, job 28231368; nereus given the mesh's own `nod_area`):

| Diagnostic | Points | Largest relative difference from nereus |
|---|---|---|
| Atlantic heat content 0–700 m | 36 months | 9.1e-15 (both chains: with and without `-sellevel`) |
| NH ice extent | 1096 days | 3.0e-14 |

The 10 login-node real-data cases (mesh info, `fldmean` per level, two masks, `regmean`, `volint`, `fldhist`
extent, `fldint` ice area, a Drake transect, `selpoints`) are all identical, or within float32 rounding where the
output is float32. `volint` now writes float64 (rel 7.9e-15).

**Test suite.**
- `tests/run_cases.sh` passes 562 of 562 cases on master `32b1dbd`, on both NetCDF read paths. That includes the
  Zarr variants and the planner variant, which must give bit-identical output with tiny chunks, small `--mem` and
  other thread counts.
- 109 rows compare with independent references: 67 NumPy, 28 regionmask, 14 nereus.
- 67 `json:` rows check counts, warnings, units and `--plan` fields.
- `cargo test --release` passes.

**No silent cell loss.**
- Cells outside every region of a set are counted: `regions_filled` with `fill=nearest`.
- Regions without cells are reported: `regions_empty`.
- Values outside the histogram's bins go to `<var>_below` and `<var>_above`.
- Points outside every triangle are counted: `points_outside`.
- Dry levels are missing values, not zeros.
- Each of these is a warning on stderr, an entry in `--plan` `warnings[]` and, where it applies, an output
  attribute. `json:` rows pin the counts (1396 filled nodes for ar6.ocean on pi, 22 for nereus' basins).

**Deviation from the plan's wording:** the criterion names pyfesom2 as a reference, but no `ref:pyfesom2` rows were
added. NumPy (on the mesh's own arrays), regionmask and nereus (which takes its basins from pyfesom2) cover the same
quantities. All differences from cdo and nereus are in `docs/deviations.md`.

## 2. Speed against the gate's marks

Marks from Task 1 (`docs/unstructured-baseline.md`). The measurement is job 28231368 on master `d47d8fc`: 3 years of
dart, one exclusive node, 128 cores, 0.06 node-h. The NetCDF-4 read fix (`32b1dbd`) came after it and is not in these
numbers.

| | Metric | Mark | Result | |
|---|---|---|---|---|
| 1 | Atlantic OHC 0–700 m, cold | ≤ 13 s (≥ 5× nereus 68.6 s) | 8.30 s with the natural chain (no `-sellevel`), 8.3× nereus; other year sets 10.4–11.1 s | pass |
| 2 | NH ice extent, daily, cold | ≤ 12 s (≥ 10× nereus 126 s) | 7.11 s, 17.8× | pass |
| 3 | second run: OHC / ice | ≤ 10 s / ≤ 8 s (≥ 2× nereus warm) | OHC warm (job 28236403): 5.93–6.51 s natural chain, 5.75–7.16 s with `-sellevel`, 2.0–2.2× nereus warm (13.1 s); ice 7.82 s (job 28231368, 2.1× nereus' 16.3 s) | pass |
| 4 | first use of the dart mesh (fresh cache, Atlantic mask, one step) | ≤ 60 s | 16.3 s (nereus 343 s) | pass |
| 4b | the same with the cache | ≤ 5 s over row 1's per-step cost | 1.06 s | pass |
| 6 | values against nereus | rel 1e-6 | 9.1e-15 / 3.0e-14 | pass |

**Row 3 was re-measured warm** (job 28236403, the same binary `32b1dbd`, 0.06 node-h). The first validation's
"warm" runs were not warm: the Lustre client keeps at most 24 GB of file data, and about 30 GB of other data was read
between two runs of the same set (10.2 s, a miss by 0.2 s). The re-run reads only temp 1970–1972 between the repeats
(16.4 GB of 36 layers):
- Lustre cache in use: 3.5 GB at the start, 18.7 GB after the cold run, 20.2 GB after the cdors repeats, 24.7 GB
  (full) after nereus.
- cdors, natural chain: cold 8.88 s; three warm repeats 6.51, 5.93 and 6.35 s; after the nereus runs 3.13 and
  3.46 s. With `-sellevel`: 7.16 and 5.75 s. Read probe of the same bytes: 4.20 and 3.98 s, later 1.35 s.
- nereus, OHC only (file chunks, Task 1's mesh cache, 128 threads): the OHC phase took 28.6 s on its first run
  (the first Python start on the node: import 16 s, opening the files 27 s) and 13.1 s on the second. Task 1's
  20.5 s was not warm either.
- The first warm repeats spent 220–270 s in the kernel, the later runs 47–54 s. Why it changed is not known.
- Every warm repeat is under 10 s, and the slowest (6.51 s) is 2.0× faster than nereus warm in the same job.

**Where the time goes.** OHC and ice now take about as long as reading their bytes (0.9–1.3× the probe). Cold reads
run at 2.2–2.5 GB/s, against 3.5–4.5 GB/s for a plain `pread` of the same files. Spreading the reads over the
files' storage targets is the next speed-up (follow-up).

Before the round's fixes (jobs 28230544 and 28230657), OHC missed rows 1, 3 and 5: 15.4 s cold, with `volint`
running one lane per step. The fixes were `volint` in two folds, the depth range pushed down as a level selection,
and `maskregion` as a per-cell node.

## 3. Agents

**Not met on cost; accepted for now** (Nikolay, 2026-10-19). Ten of ten answers are correct, and with cdors the agents took half the wall time (838 s
against 1697 s), but they cost $2.24 against $2.03 for Python with nereus (1.10×). The criterion asks for 5 of 5
at a cost no higher than the Python arm's.

The check (Task 14, `bench/agent_tasks.md`, FESOM2 set): F1 Atlantic OHC 0–700 m, F2 NH ice-extent seasonal cycle,
F3 Drake section, F4 Niño 3.4 anomalies, F5 Arctic mean SST, all on dart, scored against independent NumPy
references with nereus as the second method (job 28230786). Arm A gets cdors with `cdors guide` only, arm B Python
with nereus and cdo. Job 28244863 (shared partition, 44 min, frozen binary `cdors-32b1dbd`, model
claude-opus-5-5), one session per task and arm, `bench/agent/results-20261019-140945.md`:

| Task | cdors: turns, wall, USD | Python + nereus: turns, wall, USD |
|---|---|---|
| F1 OHC 0–700 m | 10, 109 s, $0.45 | 14, 721 s, $0.55 |
| F2 ice extent | 11, 187 s, $0.40 | 26, 545 s, $0.76 |
| F3 Drake section | 16, 168 s, $0.65 | 7, 198 s, $0.23 |
| F4 Niño 3.4 | 9, 124 s, $0.36 | 6, 127 s, $0.25 |
| F5 Arctic SST | 11, 252 s, $0.38 | 6, 106 s, $0.24 |
| **All five** | **57 turns, 838 s, $2.24** | **59 turns, 1697 s, $2.03** |

- **Where cdors wins:** the two mesh-weighted series. F1 and F2 cost the Python arm 40 turns and 21 minutes, building
  area and depth weights and masks by hand.
- **Where it loses:** F3–F5, which a single NumPy script answers in 6–7 turns. Every cdors session first spends 5–6
  calls learning: `cdors guide` (7.8 kB), the help of two to four operators (5–12 kB per call), `sinfo`,
  `mesh info` and a `--plan`. Those pages are then re-read from the cache on every later turn.
- **Spent on settings the guide does not cover:** all five cdors sessions spent one call looking up threads
  (`cdors help | grep thread; env | grep SLURM`), because the guide does not say that inside a Slurm job the
  defaults already use the job's cores.
- **F3, the most expensive session (16 turns, $0.65):**
  - `outputtab,station_name` on the written section file failed twice: cdors reads no string variables from stored
    files, as the error's hint said.
  - The first wet-depth count, `-vertint -addc,1 -mulc,0`, gave the full column depth. This follows cdo's rule that
    zero times missing is zero (not a cdors bug).
  - The session caught the wrong depth against `bottom_depth` before answering.
- **Spread:** each task ran once per arm. The pilot's F5 with cdors (job 28236330) cost $0.33 in 9 turns, against
  $0.38 in 11 turns here, so a single session varies by about 15 %, more than the 10 % between the arms.

Scoring: `bench/agent_check.sh` scores each answer against the references with the per-number tolerances in
`bench/agent_tasks.md`, and reports wall time, tokens and USD per arm (`docs/agent-check.md`).

## 4. Memory under `--mem`

Peak RSS in GB of 10⁹ bytes, as `--mem` counts. (`/usr/bin/time` reports KiB; the first validation's
"2.13–2.18 GB" were KiB / 10⁶, i.e. 2.18–2.23 GB.)

| Run (dart) | Budget | Plan's estimate | Before (`32b1dbd`) | Now (`25aea2f`) |
|---|---|---|---|---|
| Atlantic OHC 0–700 m, 3 years | `--mem 2G` | 1.62 GB | 2.15–2.28 GB (8–14 % over) | **1.72 GB** |
| the same, one step | `--mem 2G` | 1.62 GB | 2.00 GB | 1.74 GB |
| Atlantic OHC 0–700 m, 3 years | default (4 GB) | 2.77 GB | 3.45 GB | 2.63 GB |
| NH ice extent, 3 years | `--mem 2G` | | 1.05 GB | not affected |

- **Where the overshoot was** (valgrind massif on one step, job 28236468; RSS sampled every 50 ms, jobs 28236403 and
  28236588): the run stayed within the estimate once it streamed. The peak came in the first second, while the
  36 levels of per-level areas (25 MB each) were read from the mesh cache next to the first tiles. Each level read
  held the compressed chunk, the decoded chunk, a clone of it and a buffer to assemble the range in, and many
  levels load at once on 128 threads. glibc keeps freed memory for reuse (no trim, for speed), so that early peak
  stayed the RSS of the whole run.
- **The fix** (`25aea2f`): a range that is exactly one chunk, as each level is in the cache, is returned as
  decoded. Outputs are bit-identical, and the run is slightly faster (4.34 s against 4.52 s under `--mem 2G`).
- **What is left:** the peak is still 0.1 GB (6 %) above the plan's estimate. It stays within the budget because
  the tiles do not fill their share. The rest is compressed bytes of levels being loaded, thread stacks and 18 MB
  of mapped binary and libraries. Variants (one malloc arena, 16 threads, the netCDF-C read path, a 1 MB mmap
  threshold) peak at 1.47–1.72 GB.
- **Bounded growth:** memory does not grow with the length of the series: one step 1.74 GB, 36 steps 1.72 GB.

## Cost

| Item | Cost |
|---|---|
| Compute nodes: gate (job 28228738) | 0.28 node-h |
| Compute nodes: validation (jobs 28230544, 28230657, 28231368) | 0.20 node-h |
| Compute nodes: read path (jobs 28231623, 28231676) | 0.06 node-h |
| Compute nodes: Task 14 references (job 28230786) | 0.08 node-h (4 min 50 s) |
| Compute nodes: warm re-run and memory (jobs 28236403, 28236468, 28236548, 28236588) | 0.09 node-h |
| Agent check pilot (job 28236330, shared partition) | 0.03 node-h, $0.33 |
| Agent check, full run (job 28244863, shared partition, 44 min) | 0.4 node-h, $4.27 (cdors $2.24, Python $2.03) |
| Total compute | 1.14 node-h |
| Total agent sessions | $4.60 |

## Open decisions

- Criterion 3: the 1.10× is accepted for now (Nikolay, 2026-10-19). The next agent check should have tasks where
  agents without cdors get the answer wrong: weights and grids other than regular lon-lat (`docs/STATUS.md`,
  follow-up 3). That set (W1, W2, W4, W5, W6; job 28256601) gave 10/10 correct again: the Python arm avoided every
  naive answer, and cdors cost 1.25× as much, because arm A checked which weights cdors had used
  (`docs/agent-check.md`, "Trap set run"). With the method stated in the results, the ratio became 1.14×
  (0.77× without W4). With Sonnet 5.5 the ratio was 1.07×, with Haiku
  5.5 0.97×; both models' arm B also avoided every trap.
- Start round 2 (neighbour operators, first plots) or not.
- Publish: the shared install with `share/regions/`, its README (`docs/levante.md`) and the guide page, each after
  the exact text is approved.
