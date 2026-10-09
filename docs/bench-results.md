# Benchmark results (Task 13)

Status 2026-10-09: W1–W3 and the first year of W4 (W4Y) are done (job 28000341). The full 1.1 TB W4 waits for a
go-ahead and for a change to the benchmark inputs (see the last section), because cdo misreads the store's last time
chunk.

## The run

Job 28000341, 2026-10-09 12:07–12:25 (18 min, 0.3 node-hours; the estimate was ≈ 1), node l10683, exclusive
(2 × AMD EPYC 7763, 128 cores, 251 GB). Settings: cdors `-P 128 -f nc4`, its default reads in flight (128 in a Slurm
job), `--mem 32G` on W4Y; cdo 2.6.0 `-P 16 -f nc4`; xarray + dask + flox with 128 workers (64 for the cloud). Each
workload runs cdors first on cold data and cdo after it, so cdo usually reads data that cdors just read: the
comparison favours cdo. On W1 and W4, cdo reads single-variable symlink views (`bench/make_view.py`) of the same
chunk files. Outputs:
`/scratch/a/a270088/cdors-bench/bench-28000341/` (`summary.md`, `runs.tsv`, `compare.tsv`, one `.cmd`/`.log`/`.time`
per run). Decoded GB/s = decoded float32 bytes / wall time.

| Run | Wall | Decoded GB | GB/s | Peak RSS (GiB) | vs cdo (`diffn`) |
|---|---|---|---|---|---|
| W1 `yearmean`, HEALPix z9 daily `tas`, 2020–2029: cdors on the store (cold) | 7.81 s | 46.1 | 5.9 | 1.0 | PASS |
| … cdors on cdo's view (same chunk files) | 4.16 s | 46.0 | 11.0 | 1.5 | PASS |
| … cdo (warm) | 42.8 s | 46.0 | 1.07 | 0.5 | – |
| … xarray | 42.8 s | 46.0 | 1.07 | 6.7 | PASS |
| W1 reads-in-flight check, 2030s, `--io-threads 64` (cold) | 5.71 s | 46.4 | 8.1 | 1.0 | – |
| … 2040s, default 128 (cold) | 4.93 s | 46.2 | 9.4 | 1.1 | – |
| W2 `fldmean -sellonlatbox` `pr`, 1950s, 240 raw blosc NetCDF-4 files: cdors on the Parquet refs (cold) | 4.10 s | 15.2 | 3.7 | 1.0 | PASS |
| … cdors on the raw files | 7.48 s | 15.2 | 2.0 | 3.0 | PASS |
| … cdo on the raw files | 36.4 s | 15.2 | 0.42 | 0.1 | – |
| … xarray on the Parquet refs | 41.5 s | 15.2 | 0.37 | 3.1 | PASS |
| … cdors from the EERIE cloud | 75.0 s | 15.2 | 0.20 | 0.2 | PASS |
| … xarray from the EERIE cloud | 96.7 s | 15.2 | 0.16 | 1.8 | PASS |
| W3 `remap,r360x180` (bilinear) `to`, 5 years, 120 raw files: cdors with cdo's weights (cold) | 5.28 s | 7.6 | 1.4 | 1.4 | PASS |
| … cdors `remapbil` (own weight cache) | 3.83 s | 7.6 | 2.0 | 2.2 | PASS |
| … cdo `remap` with the same weights | 19.3 s | 7.6 | 0.39 | 0.2 | – |
| W4Y `timpctl,95` HEALPix z9 3-hourly `tas`, 2020 (2928 steps): cdors (cold) | 8.56 s | 37.8 | 4.4 | 14.0 | PASS (bin) |
| … cdo | 416 s | 36.8 | 0.09 | 9.7 | – |
| W4Y `ydaymean`: cdors | 19.7 s | 37.4 | 1.9 | 13.1 | FAIL: cdo is wrong, see below |
| … cdo | 86.0 s | 36.8 | 0.43 | 11.6 | – |

## Pass marks

| Mark | Measured | Target | Result |
|---|---|---|---|
| W1: cdors on the store (cold) vs cdo (warm) | 5.5× | ≥ 5× | PASS |
| W1: cdors on cdo's view vs cdo | 10.3× | ≥ 5× | PASS |
| W2: cdors on the Parquet refs vs cdo on the raw files | 8.9× | ≥ 5× | PASS |
| W2: cdors vs cdo, both on the raw files | 4.9× | ≥ 5× | **FAIL** (narrowly; the cause is planning, see below) |
| W3: cdors vs cdo, both with cdo's weights | 3.7× | ≥ 3× | PASS |
| W3: cdors `remapbil` vs cdo `remap` | 5.0× | ≥ 3× | PASS |
| W4Y: peak memory under `--mem 32G` | ≤ 14.0 GiB | ≤ 32 GB | PASS |
| W4Y `timpctl,95` / `ydaymean`: cdors vs cdo | 48.6× / 4.4× | – | info |
| Remote vs local throughput (cdors cloud / cdors Parquet) | 0.05 | ≥ 0.5 (proposed) | FAIL; the mark is under revision: the EERIE cloud serves ≈ 0.2 GB/s whatever the client |
| Same endpoint: cdors vs xarray from the EERIE cloud | 1.3× | – | info: both run at the server's cap |
| W2 on Lustre: cdors vs xarray on the Parquet refs | 10.1× | – | info |

cdo's cold rates from the Task 2 baseline (`docs/baseline.md`) put the W1 and W2 factors at about 46× (360 s cold)
and 25× (101 s cold) instead.

## Correctness

11 of 12 comparisons pass `cdo --pedantic diffn` within the harness tolerances. The one failure, W4Y `ydaymean`
(26 of 366 days, up to 46 K), is a cdo error. cdors is right: on all 26 days, over six 16384-cell chunks spread across
the sphere, it matches daily means computed with zarr-python to 1.5e-5 K, half a float32 step. cdo's values are means
of other steps from the same chunk.

What happens in cdo: for a HEALPix variable, cdo reads a whole time chunk per call (start 2728, count 248) and does
not shorten the last call, where the array ends inside the chunk. netCDF-C rejects that call. Asking cdo for a single
step there ends with `NetCDF: Start+count exceeds dimension bound`. Reading through the whole input, cdo exits 0
without a message, but the 200 steps of that chunk come out rotated by 128: output step 2728 holds step 2856, and so
on. A small copy (nside 128, same chunks) reproduces this with `copy`, `daymean`, `ydaymean` and `yearmean`; `timmean`
is right, because it does not depend on the order of the steps.

- W4Y's view is 2928 steps (all of 2020) of a store chunked by 248, so its last chunk is partial. The `ydaymean`
  days from Dec 7 to Dec 31 are wrong, plus Jan 1, which takes its last step from 2021-01-01 00:00. Rotating
  the steps by 128 reproduces cdo's values for Dec 8–31 to float32 rounding.
- The W4Y `timpctl` comparison stays valid: a percentile over the whole year does not depend on the order of the
  steps.
- W1's view (3652 days, chunks of 30) also ends inside a chunk, but that chunk lies within 2029, so the 2029 yearly
  mean is unaffected. cdo's 2029 values match zarr-python to 1.5e-5 K.
- The store itself is affected too: 87664 steps is not a multiple of 248, and cdo cannot read the last 120 steps of
  `ngc4008_PT3H_9.zarr` (Dec 17–31 of its last year). The full W4 comparison would hit this.

Details are in `docs/deviations.md`, under "cdo bugs observed".

## Where cdors loses time

- **Planning on inputs of many NetCDF-4 files.** In the job's plan-only pass, planning W2 on the raw files took
  8.1 s, against 0.67 s on the Parquet refs to the same bytes, even with the chunk index cached. On the login node
  it takes 9.1 s warm (42 s cold), of which only 3.3 s is CPU. The rest is about 27,000 small reads of HDF5
  metadata, one file after another, over the 240 files. W3 (120 files) shows the same pattern: 5.4 s warm, 1.8 s
  CPU, about 18,600 reads. This explains W2's miss of the 5× mark (7.48 s against 4.10 s on the Parquet refs) and
  most of W3's time. Reading the files' metadata concurrently, or caching what the planner needs alongside the
  chunk index, should bring W2 on the raw files close to the Parquet time.
- **`ydaymean` on a whole HEALPix z9 year** takes 19.7 s (1.9 GB/s), against 8.6 s for `timpctl` and 4.4 s for
  `-timmax`/`-timmin` on the same data. It keeps only about 16 of 128 cores busy and spends 137 s in the kernel.
  It is worth profiling before the full W4, which is 30 times the data.
- **The first cdors run of the job** (W1 on the store, 7.81 s) was 1.6× slower than the same command on another
  cold decade (4.93 s). The cause is not known. W1 passes even with it.

## Reads in flight

The 2040s decade with the new default of 128 took 4.93 s (9.4 GB/s); the 2030s with 64 took 5.71 s (8.1 GB/s). Both
were cold, and each was run once. That is 1.16× in favour of 128, in the direction the baseline predicted but
smaller. The default stays at 128 inside Slurm jobs.

## Before the full W4 run

The full W4 needs ≈ 2.5 node-hours and a go-ahead. It also needs one change to the inputs: cdo would rotate the
store's last 120 steps, so the W4 view should end on a whole chunk (87544 steps = 353 × 248, dropping Dec 17–31 of
the last year), with cdors given the same `-seltimestep,1/87544`. If W4Y is run again, the same rule gives 2976 steps
(12 chunks, 2020 plus Jan 1–7 2021).
