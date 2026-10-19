# The four round-1 criteria (analysis on FESOM2 meshes)

Status 2026-10-19, master `32b1dbd`. The round-1 plan (`docs/plans/20261018-unstructured-round1.md`, Task 15) sets
four criteria. Below, each is checked against the gate (`docs/unstructured-baseline.md`), the real-data validation
(`docs/unstructured-results.md`) and the test suite. Correctness is met. Speed meets every mark but one: the
second run of the heat content took 10.2 s against 10 s, and that run was not warm. The agent check has not run
yet. Memory stays 6–9 % above a 2 GB budget.

| | Criterion | Result | Main evidence |
|---|---|---|---|
| 1 | Correct: regionmask masks identical, nereus within tolerance, no silent cell loss | **met** | masks identical to regionmask and nereus' Atlantic mask on dart; OHC 9.1e-15 and ice extent 3.0e-14 from nereus; 562 of 562 harness cases |
| 2 | Speed against the gate's marks | **met except row 3 (natural OHC chain, second run: 10.2 s vs ≤ 10 s)** | Atlantic OHC 0–700 m, 3 years, 8.3 s cold (8.3× nereus); NH ice extent 7.1 s (17.8×); first use of the mesh 16.3 s (nereus 343 s) |
| 3 | Agents: 5 of 5, at a cost no higher than the Python arm | **pending** | five FESOM2 tasks, references and runner prepared; the start of the sessions was blocked by the permission classifier |
| 4 | Memory bounded under `--mem` | **not met: 6–9 % over** | OHC under `--mem 2G` peaks at 2.13–2.18 GB; ice 1.03 GB |

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
| 3 | second run: OHC / ice | ≤ 10 s / ≤ 8 s (≥ 2× nereus warm 20.5 s / 16.3 s) | OHC 10.20 s natural chain, 8.41 s with `-sellevel`; ice 7.82 s | **natural OHC: miss by 0.2 s** (still 2.0× nereus); the rest pass |
| 4 | first use of the dart mesh (fresh cache, Atlantic mask, one step) | ≤ 60 s | 16.3 s (nereus 343 s) | pass |
| 4b | the same with the cache | ≤ 5 s over row 1's per-step cost | 1.06 s | pass |
| 6 | values against nereus | rel 1e-6 | 9.1e-15 / 3.0e-14 | pass |

**Row 3 was not a warm measurement.** The Lustre client keeps at most 24 GB of file data, and the validation read
about 30 GB of other data between two runs of the same set, so the "warm" runs mostly read from Lustre again. The read
probe of the same bytes took 7.6–8.7 s in those runs, against 1.8–2.0 s from the page cache with the same binary
and 1.0–1.3 s after the read fix (`docs/unstructured-results.md`, "Read path"). Row 3 was not re-run, so the miss of 0.2 s stands as measured.

**Where the time goes.** OHC and ice now take about as long as reading their bytes (0.9–1.3× the probe). Cold reads
run at 2.2–2.5 GB/s, against 3.5–4.5 GB/s for a plain `pread` of the same files. Spreading the reads over the
files' storage targets is the next speed-up (follow-up).

Before the round's fixes (jobs 28230544 and 28230657), OHC missed rows 1, 3 and 5: 15.4 s cold, with `volint`
running one lane per step. The fixes were `volint` in two folds, the depth range pushed down as a level selection,
and `maskregion` as a per-cell node.

## 3. Agents

**Pending.** The check is prepared (Task 14, `bench/agent_tasks.md`, FESOM2 set):
- **Tasks:** F1 Atlantic OHC 0–700 m, F2 NH ice-extent seasonal cycle, F3 Drake section, F4 Niño 3.4 anomalies,
  F5 Arctic mean SST, all on dart.
- **References:** independent NumPy references, with nereus as the second method (job 28230786).
- **Arms:** arm A gets cdors with `cdors guide` only; arm B gets Python with nereus.

Starting the sessions was blocked by the permission classifier ("Create Unsafe Agents"), so they must be started by
Nikolay. Commands, with the frozen binary:

```bash
cd ~/cdo
# pilot: one session (F5, cdors arm)
TASKSET=fesom TASKS=F5 ARMS=A CDORS_BIN=/work/ab0995/a270088/cdors-target/frozen/cdors-32b1dbd \
  sbatch --time=00:40:00 bench/agent_check.sbatch
# full: five tasks, two arms, ten sessions one after another (~$3–4)
TASKSET=fesom CDORS_BIN=/work/ab0995/a270088/cdors-target/frozen/cdors-32b1dbd \
  sbatch --time=02:30:00 bench/agent_check.sbatch
```

Scoring: `bench/agent_check.sh` scores each answer against the references with the per-number tolerances in
`bench/agent_tasks.md`, and reports wall time, tokens and USD per arm (`docs/agent-check.md`). The criterion is met if
arm A gets 5 of 5 right at a cost no higher than arm B's.

## 4. Memory under `--mem`

| Run (3 years, dart) | Budget | Plan's estimate | Peak RSS |
|---|---|---|---|
| Atlantic OHC 0–700 m | `--mem 2G` | 1.62 GB, with 0.34 GB of resident mesh arrays | 2.13–2.18 GB (6–9 % over) |
| NH ice extent | `--mem 2G` | | 1.03 GB |
| Atlantic OHC 0–700 m | default (4 GB) | | 2.4–3.9 GB |
| NH ice extent | default (4 GB) | | 1.0–1.6 GB |

- **What the plan counts:** since the round's fixes, the mesh's resident arrays and per-level areas are held
  outside the stages' budget (`memory.parts.mesh_resident`, `mesh_weights`). This cut the overshoot from 12 % to
  6–9 %.
- **What it misses:** about 0.5 GB of RSS: HDF5 and netCDF-C buffers, the allocator, and copies of tiles in
  flight.
- **Follow-up:** measure these per open file and per I/O thread and reserve them in the plan, or lower the tile
  budget by a fixed margin on NetCDF-4 inputs.
- **Bounded growth:** memory does not grow with the length of the series. One OHC step peaks at 3.8 GB at the
  default budget, 36 steps at 2.4–3.9 GB.

## Cost

| Item | Cost |
|---|---|
| Compute nodes: gate (job 28228738) | 0.28 node-h |
| Compute nodes: validation (jobs 28230544, 28230657, 28231368) | 0.20 node-h |
| Compute nodes: read path (jobs 28231623, 28231676) | 0.06 node-h |
| Compute nodes: Task 14 references (job 28230786) | 0.08 node-h (4 min 50 s) |
| Total compute | 0.62 node-h |
| Agent check | not yet run (~$3–4 estimated) |

## Open decisions

- Run the agent check (commands above).
- Start round 2 (neighbour operators, first plots) or not.
- Publish: the shared install with `share/regions/`, its README (`docs/levante.md`) and the guide page, each after
  the exact text is approved.
