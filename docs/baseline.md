# Baseline: cdo 2.6.0 on W1–W4 and raw read throughput (Task 2)

Status (2026-10-08): datasets chosen, short login-node indications taken, `bench/baseline.sbatch` written and
**not submitted**. The section "Baseline results (compute node)" is filled in after Nikolay approves the job.

## Datasets

Details, paths and caveats: `bench/datasets.md`.

| | Workload | Input used for the baseline | Decoded / on disk | Chunks | cdo access |
|---|---|---|---|---|---|
| W1 | `yearmean` | ngc4008 `tas`, daily, HEALPix z9, Zarr v2 (`/work/kd1453/rechunked_ngc4008/ngc4008_P1D_9.zarr`), decade 2020–2029 through a single-variable view | 45.9 / ~25.6 GB (full store 137.9 / 76.6 GB) | 30 × 65536, blosc-lz4 | NCZarr `file://…#mode=zarr,file` |
| W2 | `fldmean -sellonlatbox,-30,40,30,75` | ICON-ESM-ER control `pr`, daily, 0.25°; 240 raw NetCDF-4 files (model years 1991–2000), Parquet kerchunk, and EERIE cloud `/kerchunk` | 15.2 / 13.2 GB (whole variable 153 / 133 GB) | 1 × 721 × 1440, blosc-lz4 HDF5 filter | raw files only (`-select,name=pr`) |
| W3 | `remapbil,r360x180` | ICON-ESM-ER control ocean `to` at 1 m (SST), daily, 0.25° regular, 120 raw files (1991–1995) | 7.6 / 3.8 GB | 1 × 1 × 721 × 1440 | raw files |
| W4 | `timpctl,95`, `ydaymean` | ngc4008 `tas`, 3-hourly, HEALPix z9, chunked in space; baseline on 1 year (2020) | 36.8 / ~21 GB (full: 1103 / ~620 GB) | 248 × 16384 (192 spatial chunks per time block) | NCZarr, through a view |
| W4+ | memory demonstration | ngc4008 `tas`, 15-minute, z9 | 13.2 TB / ~7.4 TB | 192 × 16384 | not run with cdo |

Findings that affect the plan:
- **cdo needs a single-variable view of the multi-variable Zarr store.** On the full 111-variable W1 store,
  `cdo fldmean -seltimestep,1/30 -selname,tas` did not finish in 90 s and had read 11.9 GB. On a view holding
  only `tas`, `time` and `crs` (`bench/make_view.py`, symlinks only), cdo takes 0.5 s. All cdo Zarr runs use
  views, and cdors should read the same views so that both tools see the same bytes.
- **For many raw NetCDF files, cdo needs `-select,name=pr <files>`.** A `-selname,pr -mergetime [ … ]` chain reads
  every variable: one year did not finish in 115 s, against 6.9 s with `select`.
- **W2's kerchunk references are Parquet** (fsspec `LazyReferenceMapper`, `record_size` 100000, columns
  `path, offset, size, raw`), not JSON. Task 10 should implement Parquet first.
- **EERIE cloud:** the `/zarr` endpoint returns HTTP 403 for every dataset tried, and `intake.yaml` returns 404.
  The `/kerchunk` endpoint works: it is Zarr v2 with consolidated metadata, and the raw chunk bytes are passed
  through unchanged. The probe and Task 11 should target `/kerchunk`. cdo cannot open the cloud store.
- **W2 timestamps differ between access paths.** The raw files say 1991–2091, stamped at the end of each day;
  kerchunk and cloud say 1950–2050, stamped at mid-day.
- **cdo 2.6.0 does not classify the W1/W4 grid as HEALPix.** It reports `gridtype = projection` with
  `grid_mapping_name = healpix` (the store has no `cell` coordinate). `griddes` text from cdors will differ unless
  cdors copies this behaviour.
- **netCDF-C caches a whole time block.** cdo's RSS is about one time block of chunks: 0.48 GB for W1 and
  3.1 GB for W4 per open stream. Each chunk is therefore decoded once, with no re-reading per timestep.
- The ngc4008 zoom-10 `tas` stores are empty (all chunks NaN). Only zoom 9 is used.

## Preliminary numbers — LOGIN NODE (indications only, not the baseline)

Shared login node, `cdo -O -f nc4`, Lustre page cache state as noted, each run under 2 minutes. Throughput is
decoded float32 bytes (steps × cells × 4) divided by cdo's wall time. The read share is the `read` row of
`cdo -T` divided by its `total`; it is available only for a single operator at `-P 1`.

| Run | Data | Wall | Decoded GB/s | Read share | Max RSS | Cache |
|---|---|---|---|---|---|---|
| W1 `-P 1 -T timmean` | 30 days (0.38 GB) | 0.50 s | 0.76 | 76 % | 0.48 GB | cold |
| W1 `-P 1 -T yearmean` | 365 days (4.59 GB) | 4.75 s | 0.97 | 83 % | 0.48 GB | mostly cold (2.1 GB read from Lustre) |
| W1 `-P 1 -T fldmean` | 365 days (4.59 GB) | 4.93 s | 0.93 | 66 % | 0.49 GB | warm |
| W1 `-P 1 -T yearmean` | 1096 days (13.8 GB) | 14.3 s | 0.97 | 84 % | 0.52 GB | years 2–3 cold (5.1 GB from Lustre) |
| W1 `yearmean`, `-P 1` vs `-P 8` | 365 days | 6.65 s vs 4.79 s | 0.69 vs 0.96 | – | 0.48 GB | warm, noisy |
| W1 full store, `fldmean -seltimestep,1/30 -selname,tas` | 30 days | > 90 s (killed) | < 0.004 | – | 0.79 GB | read 11.9 GB |
| W4 `-P 1 -T timmean` | 1 time block, 248 × 3-hourly (3.12 GB) | 17.6 s | 0.18 | 95 % | 3.17 GB | cold, CPU 4.1 s (I/O-bound, ~107 MB/s from Lustre) |
| W2 `-P 1 fldmean -sellonlatbox -select,name=pr` | 1 year, 24 raw files (1.52 GB) | 6.94 s | 0.22 | – | 0.08 GB | cold |
| W2 `-P 1 -T fldmean` on one raw month (21 variables) | 630 fields (2.62 GB; 1.87 GB on disk) | 22.9 s | 0.11 | 95 % | 0.08 GB | cold |
| W3 `-P 1 remapbil,r360x180 -select,name=to` | 30 days, raw file (0.125 GB) | 0.63 s | 0.20 | – | 0.09 GB | incl. weights |
| W3 `genbil,r360x180` | 1 field | 0.29 s | – | – | – | – |
| W3 `-P 1 -T remap,r360x180,<weights>` | 30 days, uncompressed copy | 0.26 s | – | 53 % | 0.06 GB | about 4 ms remap per field |
| W3 `-P 1 -T remapbil,hpz8` | 30 days, uncompressed copy | 0.65 s | – | 13 % | 0.22 GB | incl. weights |
| EERIE cloud `/kerchunk`, Python, 1 stream | 8 chunks of `pr` | 0.59 s | 0.056 (0.049 compressed) | – | – | – |
| EERIE cloud `/kerchunk`, Python, 8 threads | 64 chunks | 1.26 s | 0.21 (0.18 compressed) | – | – | – |

What the numbers suggest for the gate (to be confirmed on a compute node):
- On W1 with a single-variable view, **cdo's effective throughput is about 1 GB/s of decoded data at `-P 1`.**
  The read takes 76–84 % of the time, yet cdo uses only about half a CPU (2.3 s user for 4.6 GB). cdo is limited
  by the latency of serial chunk reads, not by decoding. `-P 8` does not help, because reads are serialised.
- The 5× gate on W1 therefore requires the probe to reach **≥ 5 GB/s decoded**. At W1's compression ratio of 0.56
  that is **≥ 2.8 GB/s of Lustre reads from one node**. This is plausible with many concurrent reads, but it is
  not certain. It is the one number the compute-node job must settle.
- On cold, space-chunked W4 data, and on the raw NetCDF files of W2/W3, cdo reaches only 0.1–0.2 GB/s, with a
  read share of about 95 %. There the gain from concurrent reads should be much larger than 5×.
- W3's remap itself is cheap (about 4 ms per 1440 × 721 field to r360x180). Like W2, W3 is an I/O workload
  under cdo.

## The compute-node job (`bench/baseline.sbatch`) and its cost

One exclusive `compute` node, account `ab0995` (`sacctmgr` lists it, with the `normal` QOS), `--mem=0`,
time limit 1:30 h. It sources `env.sh` and writes to `/scratch/a/a270088/cdors-bench/baseline-<jobid>/`
(`summary.tsv` with wall, user, sys, max RSS and cdo's read/total timers; one `.log` and `.time` per run).

| Step | Runs | Expected wall (from the rates above) |
|---|---|---|
| W1 | cdo `-P 1 -T yearmean` decade 1 (cold); probe on decade 2 with 128 threads (cold); cdo `-P 16 yearmean` and `-P 1 -T fldmean` (warm); probe on decade 1 with 1/16/64/128 threads (warm) | ~8 min |
| W4 (1 year) | cdo `-P 1 -T ydaymean` (cold); `-P 16 timpctl,95 in -timmin in -timmax in` (warm) | ~4–8 min |
| W2 (decade) | cdo `-P 16 fldmean -sellonlatbox -select,name=pr` over 240 files; `-P 1 -T fldmean` on 4 other raw months; probe on the cloud `/kerchunk` store with 64 threads (whole `pr`, 133 GB compressed, capped at 30 min) | ~5–35 min |
| W3 (5 years) | `genbil`; `-P 16 remapbil` and cached `remap` over 120 files; `-P 1 -T remapbil` on 3 other raw months | ~3 min |

**Cost: expected 0.5–0.8 node-hours, at most 1.5 node-hours (the time limit),** within the 1–2 node-hours
that the plan budgets for Task 2. Most of the uncertainty comes from the cloud probe. If the probe reads at
≥ 0.5 GB/s, it finishes in under 5 min.

Before submitting:
- the `read_probe` example must be built (`cargo build --release --example read_probe`). The job calls
  `$CARGO_TARGET_DIR/release/examples/read_probe <zarr-path-or-url> <variable> <threads>`. Without the binary,
  the probe rows fail (rc 127) and the cdo rows still run;
- the probe reads **every** chunk of the variable. For the cloud, that is 36 890 chunks (133 GB). If that is too
  much, give the probe an optional chunk or time limit and add it to the cloud call in the sbatch;
- the views under `/scratch/a/a270088/cdors-bench/views/` already exist; the job recreates any that are missing.

## Baseline results (compute node)

*(empty — to be filled after the job has run)*
