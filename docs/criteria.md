# The four prototype criteria (Task 15)

Status 2026-10-09. The plan set four things the prototype must show before a second round is considered
(`docs/plans/completed/20261008-cdors-prototype.md`, Overview). Below, each is checked against the baseline
(`docs/baseline.md`), the benchmarks (`docs/bench-results.md`), the agent check (`docs/agent-check.md`) and the
test suite. Three are met. The fourth, remote Zarr, is practical but misses its throughput mark, because that
mark measures the EERIE server rather than the client.

| | Criterion | Result | Main evidence |
|---|---|---|---|
| 1 | Faster than cdo on the same node, results within tolerance | **met** | 4.4–72× across W1–W4; every comparison with cdo passes, except one where cdo is wrong |
| 2 | Analysis directly on remote Zarr is practical | **met in practice; the throughput mark is not** | 15 GB decoded from the EERIE cloud in 75–97 s, 1.2–1.3× xarray on the same endpoint; cdo cannot read it; the server caps every client at ≈ 0.2 GB/s |
| 3 | Agents use it reliably | **met in a small check** | 5 of 5 tasks correct, no failed command, `--plan` used in every session; with `cdors guide`, 4× faster than cdo + Python at 1.3× the cost |
| 4 | Memory stays bounded on multi-TB inputs | **met at 1.1 TB** | full W4 (1.1 TB; 1.9 TB decoded for percentiles) within 23.7 GiB under `--mem 32G`; the 13 TB store was not run |

## 1. Faster than cdo, with matching results

Measured on an exclusive compute node (128 cores), cdors on cold data, cdo usually on data cdors had just read
(this favours cdo). Jobs 28000341 and 28007818:

| Workload | cdors | cdo | Factor |
|---|---|---|---|
| W1 `yearmean`, HEALPix z9 daily, one decade (46 GB) | 7.8 s | 42.8 s (warm) | 5.5×; 10.3× on cdo's view; ≈ 46× against cdo cold (360 s, baseline) |
| W2 `fldmean -sellonlatbox`, 240 raw NetCDF-4 files (15 GB) | 3.56 s | 38.5 s | 10.8× (8.8× from the Parquet refs) |
| W3 `remap` bilinear to 1°, 120 raw files (7.6 GB) | 2.76 s | 23.7 s | 8.6× with cdo's weights, 10.0× `remapbil` |
| W4Y `timpctl,95`, one year 3-hourly (38 GB) | 8.56 s | 416 s | 48.6× |
| W4Y `ydaymean` | 19.7 s | 86.0 s | 4.4× (cdo's output wrong on 26 days) |
| W4 `ydaymean`, 30 years (1.1 TB) | 78.8 s | 5673 s | 72× |
| W4 `timpctl,95`, 30 years | 332 s | not run (≥ 5.3 h) | 37–57× extrapolated |

- **Results.** All 8 comparisons of job 28007818 pass `cdo --pedantic diffn` within the test tolerances, the full
  W4 `ydaymean` included. Job 28000341 passed 11 of 12. The twelfth is a cdo bug: on HEALPix Zarr, cdo rotates the
  steps of a partial last time chunk, and cdors agrees with zarr-python (`docs/deviations.md`). With no cdo run to
  compare, the full-W4 percentiles were checked against numpy's exact percentile on 1536 cells: the largest
  difference is 0.0008 K.
- **Test suite.** `tests/run_cases.sh`: 237 rows against cdo 2.6.0, all passing, in about 30 s with 16 rows in
  parallel (50 s with the default 8 on a busy login node). Every row on the HEALPix fixture also runs on its Zarr
  v2 and v3 copies, compared with cdo as well. It also runs on a copy with tiny chunks and under a 1 MB budget
  (lane waves, several passes), and these two runs must reproduce the first output bit for bit.
- **Real data.** `bench/realdata_check.sh` compares small slices of W1–W4, ICON, FESOM, ORCA1 and the EERIE cloud
  with cdo or numpy. Re-run on the final binary: 17 of 23 identical, 2 within tolerance, 4 plan and memory checks
  ok, none different.
- **Where cdo stays ahead.** On tiny inputs cdo is faster, because cdors needs 0.2–0.3 s to start and plan. A first
  remapping between two grids runs cdo to make the weights, and so takes cdo's time.

## 2. Remote Zarr

- **The mark.** "Remote throughput at least half of local" (proposed in Task 13) is not met: the ratio is 0.05.
  cdors reads 0.16–0.20 GB/s from the EERIE cloud and 3.5–3.7 GB/s from Lustre through the Parquet refs.
- **Why.** The EERIE server gives one client about 0.2 GB/s, whatever the client does. The cap is the same from
  login and compute nodes, and 64 requests in flight are no faster than 16 (`docs/baseline.md`). xarray with dask
  reaches 0.13–0.16 GB/s on the same endpoint; cdors is 1.2–1.3× faster. No client can meet the mark on this
  endpoint.
- **What it means in practice.**
  - W2's decade of 0.25° precipitation (15.2 GB decoded) takes 75–97 s from the cloud. That is about as fast as
    cdo on the raw files on Lustre, cold (101 s in the baseline).
  - cdo cannot read the cloud store at all.
  - Selections decide which chunks are fetched, so a region or a point costs only its chunks.
  - In the agent check, the cloud task (T5, a January mean over Europe) took the agent 141 s in total and was
    answered correctly.
- **Not shown.** S3 is implemented (through `object_store`), but it was never run against a real bucket. The EERIE
  server answers range requests without `Content-Range`, so cdors reads whole objects there. This is fine for its
  one-object-per-chunk stores but would be slow for kerchunk references into large files on such a server.
- **Verdict.** Practical for analyses up to tens of GB, at the speed the server allows, and faster there than the
  usual Python stack. A mark that measures the client rather than the server would be "at least as fast as the
  best other client on the same endpoint"; that one is met.

## 3. Agents

- **The check.** Ten headless Claude Code sessions: five analysis tasks, each done once with cdors and once with cdo
  + Python.
- **Reliability.** With cdors, all five answers were correct and no command failed. Every session checked
  `--plan` before reading data. The wall time was 704 s, against 1684 s with cdo + Python, which also got five of
  five right.
- **Cost.** 1.7× the tool calls and 2.7× the tokens ($2.73 against $1.57 at list price). The agents had to learn
  cdors from its README and `docs/deviations.md` (34 kB), which they know nothing about from training. A re-check
  with `cdors guide` (5 kB, 2026-10-10) instead: 5 of 5 correct, 426 s, $1.98, so 1.3× the cost of cdo + Python
  and 4× faster (`docs/agent-check.md`).
- **Limits.** One session per task and arm, with one model, on one day.
- **Found while checking the criteria.** cdo options that cdors does not have (`-z zip`, `-k`, `-r`, ...) were
  taken for operators and failed with `unknown operator 'z'` and unrelated suggestions. They now fail as unknown
  options (`bad_arguments`), with a hint for `-z` and `-k`, and a test row covers this.

## 4. Bounded memory

- **Full W4.** 87544 3-hourly steps of HEALPix z9 `tas`: 1.1 TB as float32, 620 GB on disk. Under `--mem 32G`:
  - `ydaymean` peaked at 14.3 GiB (cdo: 11.8 GB, in 1 h 35 min).
  - `timpctl,95` peaked at 23.7 GiB. Exact percentiles keep every value of a cell until the cell is done, so the
    planner split the cells into 139 waves and read each time chunk twice: 1.88 TB decoded in 332 s.
- **Small budgets.** The planner sizes waves so that its estimate stays within the budget. With the current
  binary, two months of W4 `ydaymean` (9.4 GB decoded) peaked at:

  | Budget | Waves | Peak RSS | Time |
  |---|---|---|---|
  | `--mem 1G` | 9 | 0.65 GB | 5.2 s |
  | `--mem 2G` | 5 | 1.19 GB | 2.1 s |

  A smaller budget costs time, not correctness. The test suite runs every HEALPix row under a 1 MB budget.
  The budget bounds the planner's estimate (tiles, lane states, output buffers, remap weights). The binary, its
  libraries and the allocator add to it. In the real-data check, two runs under a 300 MB budget peaked at
  286 MB (estimate 149 MB) and 354 MB (estimate 299 MB). That is 55–140 MB above the estimate, which matters
  only for budgets of a few hundred MB.
- **Not shown.**
  - "Multi-TB" was shown at 1.1 TB of input (1.9 TB decoded); the 13.2 TB 15-minute store was not run. Memory does
    not grow with the length of the time axis: mergeable statistics keep one state per cell and open output group,
    and exact percentiles are answered with more waves, which costs time.
  - As the plan says, this holds for data chunked in space. Data stored one complete field per chunk (GRIB, NetCDF
    written per step) is read correctly. But percentiles and daily climatologies on it read the input several times
    until a rechunking stage exists.
  - W4 `timpctl` kept only about 35 of 128 cores busy; not profiled.

## Beyond the criteria

What the next round would need is listed in the plan's Post-Completion and in `docs/STATUS.md` (known gaps):
- GRIB input (75 of 78 IFS-FESOM2 Parquet sets reference GRIB);
- a rechunking stage;
- compressed NetCDF output and the classic model (`-f nc4c`);
- the operators not yet ported;
- a compact reference for agents, to cut the token cost.
