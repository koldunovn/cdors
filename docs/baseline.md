# Baseline: cdo 2.6.0 on W1–W4 and raw read throughput (Task 2)

Status (2026-10-09): datasets chosen, login-node indications taken for cdo and for the read probe
(`crates/cdors-core/examples/read_probe.rs`), and the compute-node job run (job 27994857, 29 min). **The gate is
passed**: see "Baseline results (compute node)" at the end. The login-node sections below are kept as they were
written; where they disagree with the compute node (cdo's cold rate above all), the compute node counts.

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

## Preliminary probe (login node) — indications only

`read_probe` (Task 2) opens one Zarr array with zarrs 0.23.14, enumerates its chunks, reads the **encoded**
chunk bytes and decodes them in parallel on a rayon pool. It prints one JSON line per run.

```
read_probe <store path | https URL> <array> [--threads N] [--concurrency M] [--max-chunks K]
           [--dim0-range a:b] [--order index|spread] [--http-sync] [--http2]
```

- Local stores, default "fused": each of the N threads reads a chunk file and decodes it at once, so N reads are
  in flight. With `--concurrency M`, "pipelined": M reader threads feed the N decoders through a bounded queue.
- HTTPS: M async fetches in flight (object_store HTTP client through `zarrs_object_store`, tokio), decoded on N
  rayon threads. `--http-sync` instead has each rayon thread block on its own fetch; it was no faster (below), so
  the async path is the default.
- `--dim0-range a:b` selects chunk rows (first chunk index, not time steps); `--max-chunks K` keeps the first K
  chunks; `--order spread` visits them in a fixed shuffled order. Missing chunks count as fill
  (`missing_chunks`, `fill_bytes`), not as errors and not as decoded bytes. Every decoded chunk is read back (one
  value per 4 KiB is summed into `checksum`), so no decode can be optimised away.
- The time split: `fetch_thread_s` and `decode_thread_s` are summed over threads (or over requests);
  `fetch_share` is their ratio. `fetch_inflight_mean` is the mean number of reads in flight and `decode_busy` the
  fraction of the N decode threads in use.

Shared login node (`levante`, 256 cores, load ~5), 2026-10-09, at most 16 decode threads and 64 reads in flight,
each run under 6 s. GB = 10⁹ bytes. "Cold" = chunk rows that no earlier run had read; each cold run used its
own rows. W1 = ngc4008 `tas` daily (`ngc4008_P1D_9.zarr`), 300 chunks = 2.36 GB decoded / 1.31 GB encoded,
or 600 chunks = 4.72 / 2.62 GB; W4 = `ngc4008_PT3H_9.zarr` `tas`, 150 chunks = 2.44 / 1.38 GB; W2 = EERIE cloud
`/kerchunk` `pr`, 200 chunks = 0.83 / 0.72 GB.

| Store, rows | Mode | Threads / in flight | Chunks | Decoded GB/s | Encoded GB/s | Fetch share | Cache |
|---|---|---|---|---|---|---|---|
| W1 0:10 | fused | 16 / 16 | 300 | 3.66 | 2.04 | 93 % | first touch (partly cached?) |
| W1 0:10 | fused | 4 / 4 | 300 | 9.73 | 5.41 | 45 % | warm |
| W1 0:10 | fused | 8 / 8 | 300 | 17.6 | 9.77 | 50 % | warm |
| W1 0:10 | fused | 16 / 16 | 300 | 22.9 | 12.7 | 41 % | warm |
| W1 200:210 | fused | 16 / 16 | 300 | 2.82 | 1.57 | 95 % | cold |
| W1 220:230 | fused | 8 / 8 | 300 | 1.51 | 0.84 | 96 % | cold |
| W1 260:270 | fused, spread order | 16 / 16 | 300 | 2.92 | 1.62 | 95 % | cold |
| W1 280:300 | pipelined | 16 / 32 | 600 | 5.85 | 3.25 | 94 % | cold |
| W1 240:250 | pipelined | 16 / 64 | 300 | 7.73 | 4.30 | 91 % | cold |
| W1 300:320 | pipelined | 16 / 64 | 600 | **9.67** | **5.37** | 94 % (decode busy 21 %) | cold |
| W1 320:340 | pipelined | 8 / 64 | 600 | 9.54 | 5.30 | 95 % (decode busy 33 %) | cold |
| W4 100:101 | fused | 16 / 16 | 150 | 3.79 | 2.14 | 92 % | cold |
| W4 120:121 | pipelined | 16 / 64 | 150 | **8.90** | **5.05** | 87 % (decode busy 45 %) | cold |
| W2 cloud | async | 4 / 4 | 100 | 0.083 | 0.073 | 99 % | – |
| W2 cloud | async | 16 / 16 | 200 | 0.171 | 0.149 | > 99 % | – |
| W2 cloud | async, HTTP/2 allowed | 16 / 16 | 200 | 0.212 | 0.184 | > 99 % | – |
| W2 cloud | sync (`--http-sync`) | 16 / 16 | 200 | 0.179 | 0.155 | > 99 % | – |
| W2 cloud | async | 16 / 64 | 200 | 0.212 | 0.185 | > 99 % | – |
| W2 cloud | async, same rows again | 16 / 64 | 200 | 0.215 | 0.188 | > 99 % | – |

What the probe shows:
- **Decoding is cheap.** blosc-lz4 with shuffle decodes at about 4.4 GB/s of output per thread on W1 (2.36 GB in
  0.53 thread-seconds) and about 2.7 GB/s per thread on W2's `pr`. Two or three threads could decode 10 GB/s.
- **Cold Lustre reads are latency-bound, and the fix is reads in flight, not decode threads.** A 4.4 MB W1 chunk
  file takes about 40–45 ms cold (≈ 100 MB/s per stream, the same rate cdo's serial reader sees). With 16 reads in
  flight the probe reaches 2.8–2.9 GB/s decoded, with 32 it reaches 5.9 GB/s, and with 64 it reaches 9.5–9.7 GB/s
  (5.3 GB/s from Lustre). Eight or sixteen decode threads make no difference at 64 reads in flight. Visiting the
  chunks in a shuffled order does not help either. W4's 16 MB chunks behave the same way (3.8 → 8.9 GB/s).
- **Warm (page cache) reads** reach 23 GB/s decoded with 16 threads.
- **The EERIE cloud is limited on the server side at about 0.19 GB/s compressed** (0.21 GB/s decoded). The rate is
  the same at 16 and 64 requests in flight, with HTTP/1.1 or HTTP/2, with the async or the sync client, and when
  the same chunks are read again. Each request is then slow: at 64 in flight, one 3.6 MB chunk takes 1.1 s. This
  is the rate that Python with 8 threads got (0.18 GB/s). Whether this is a per-client limit or the server's
  total cannot be told from one host.

What the EERIE `/kerchunk` endpoint serves (checked 2026-10-09): Zarr **v2** only — `.zmetadata`, `.zgroup`,
`pr/.zarray` and `pr/.zattrs` answer 200, `zarr.json` answers 404. `pr/.zarray` is `shape [36890, 721, 1440]`,
`chunks [1, 721, 1440]`, `<f4`, `fill_value null`, `order C`, **`compressor null`, `filters [{id: blosc, cname
lz4, clevel 5, shuffle 1, blocksize 4152960}]`**. Chunks are served whole from nginx with `accept-ranges: bytes`
and `cache-control: max-age=604800`; HTTP/2 is offered. **A chunk key outside the array answers 503, not 404**, so
cdors cannot treat 404 as the only "missing chunk" answer from this server and must not retry 503 forever.
⚠️ zarrs 0.23.14 refuses this array ("the blosc codec cannot be created from numcodecs.blosc metadata
directly"): it only converts blosc to a v3 codec when blosc is the `compressor`, because it needs the element size.
The probe falls back to moving a trailing bytes-to-bytes filter into the empty compressor slot, which is the same
pipeline. cdors (Tasks 10 and 11) needs the same normalisation for every kerchunk store made from HDF5 filters.

Preliminary gate reading (login node; the compute-node job decides):
- On W1, cdo's effective throughput is about 0.95 GB/s decoded. The probe reads **cold** W1 chunks at
  **9.5–9.7 GB/s decoded with 64 reads in flight, about 10× cdo**, and warm chunks at 23 GB/s with 16 threads
  (about 24×). The 5× target (≥ 5 GB/s) is met on the login node, but only with more reads in flight than decode
  threads: the fused mode with 16 threads reaches only 3×. The design consequence for cdors is a reader with a
  separate, larger I/O concurrency (64 or more) feeding a smaller decode pool.
- Caveats: runs read only 1.3–2.6 GB from Lustre and last 0.3–0.8 s, the login node and Lustre are shared, and
  Lustre server caches may have held some rows. The compute-node job reads full decades (46 GB) with 128 threads
  and up to 256 reads in flight.
- On cold W4, the probe's 8.9 GB/s is about 50× cdo's 0.18 GB/s.
- For the cloud (W2), cdo cannot read the store at all. The probe matches the server's limit, which is about the
  rate cdo reaches on the raw files (0.22 GB/s). The proposed Task 13 mark "remote throughput at least half of
  local" cannot be met against this endpoint as long as the server caps a client at about 0.2 GB/s.

## The compute-node job (`bench/baseline.sbatch`) and its cost

One exclusive `compute` node, account `ab0995` (`sacctmgr` lists it, with the `normal` QOS), `--mem=0`,
time limit 1:30 h. It sources `env.sh` and writes to `/scratch/a/a270088/cdors-bench/baseline-<jobid>/`
(`summary.tsv` with wall, user, sys, max RSS and cdo's read/total timers; one `.log` and `.time` per run;
`probe.jsonl` with the JSON line of every probe run).

| Step | Runs | Expected wall (from the rates above) |
|---|---|---|
| W1 | cdo `-P 1 -T yearmean` decade 1 (cold); probe on decade 2 (chunk rows 122:244) with 128 threads and 256 reads in flight, and on decade 3 (rows 244:366) with 128 fused threads (both cold, 46 GB decoded each); cdo `-P 16 yearmean` and `-P 1 -T fldmean` (warm); probe on decade 1 (rows 0:122) with 1/16/64/128 fused threads and with 128 threads / 256 in flight (warm) | ~8 min |
| W4 (1 year) | cdo `-P 1 -T ydaymean` (cold); `-P 16 timpctl,95 in -timmin in -timmax in` (warm); probe on 6 time blocks (1152 chunks, 18.7 GB decoded) cold (rows 12:18) and warm (rows 0:6), 128 threads / 256 in flight | ~4–8 min |
| W2 (decade) | cdo `-P 16 fldmean -sellonlatbox -select,name=pr` over 240 files; `-P 1 -T fldmean` on 4 other raw months; probe on the cloud `/kerchunk` store, 2000 chunks each (7.2 GB compressed) with 64 and with 16 requests in flight | ~5–8 min |
| W3 (5 years) | `genbil`; `-P 16 remapbil` and cached `remap` over 120 files; `-P 1 -T remapbil` on 3 other raw months | ~3 min |

**Cost: expected 0.4–0.6 node-hours, at most 1.5 node-hours (the time limit),** within the 1–2 node-hours
that the plan budgets for Task 2. The cloud probe is now capped (2 × 2000 chunks, about 40 s each at the 0.19 GB/s
seen from the login node); each probe run also has a 15-minute `timeout`.

Before submitting:
- the `read_probe` example must be built on the login node (`source env.sh && cargo build --release --example
  read_probe`, into `$CARGO_TARGET_DIR=/work/ab0995/a270088/cdors-target`). The job stops at once if
  `$CARGO_TARGET_DIR/release/examples/read_probe` is missing, so no node time is spent without it;
- the W1 and W4 probe runs read the source stores with `--dim0-range` (chunk rows), not the views; the rows
  cover the same chunk files as the views (plus up to 8 days at the end of each decade);
- the views under `/scratch/a/a270088/cdors-bench/views/` already exist; the job recreates any that are missing.

## Baseline results (compute node)

Job 27994857, 2026-10-09 08:18–08:47 (29 min, 0.5 node-hours), node l50327 (2 × AMD EPYC 7763, 128 cores,
251 GB), exclusive. All 27 runs exited 0, and cdo's outputs have the expected number of steps (`yearmean` 10,
`ydaymean` 366, the 5-year remaps 1826; `-P 1` and `-P 16` `yearmean` agree). Outputs:
`/scratch/a/a270088/cdors-bench/baseline-27994857/` (`summary.tsv`, `probe.jsonl`, one `.log` and `.time` per
run). Decoded GB/s = decoded float32 bytes / wall time. "Cold" = chunk files that no earlier run on this node had
read; "warm" = read earlier in the job and still in the node's page cache.

### cdo 2.6.0

| Run | Data (decoded) | Wall | Decoded GB/s | Read share (`-T`) | User + sys | Max RSS | Cache |
|---|---|---|---|---|---|---|---|
| W1 `-P 1 -T yearmean` | decade 1, 3652 days (45.9 GB) | 360 s | **0.13** | 97 % | 28 + 12 s | 0.52 GB | cold |
| W1 `-P 16 yearmean` | same | 49.3 s | 0.93 | – | 245 + 11 s | 0.52 GB | warm |
| W1 `-P 1 -T fldmean` | same | 54.0 s | 0.85 | 60 % | 38 + 8 s | 0.47 GB | warm |
| W4 `-P 1 -T ydaymean` | 2020, 2928 3-hourly steps (36.8 GB) | 191 s | 0.19 | 91 % | 29 + 16 s | 11.6 GB | cold |
| W4 `-P 16 timpctl,95 in -timmin in -timmax in` | same, read 3 times | 632 s | 0.06 per pass (0.17 for all 3) | – | 892 + 24 s | 9.7 GB | warm |
| W2 `-P 16 fldmean -sellonlatbox -select,name=pr` | 240 raw files, decade (15.2 GB) | 101 s | 0.15 | – | 10 + 14 s | 0.09 GB | cold |
| W2 `-P 1 -T fldmean`, 4 raw months, all 21 variables | 2.4–2.6 GB each | 19–21 s each | 0.12–0.13 | 92–93 % | 2 + 1 s | 0.08 GB | cold |
| W3 `genbil,r360x180` | 1 field | 0.8 s | – | – | – | 0.22 GB | – |
| W3 `-P 16 remapbil,r360x180 -select,name=to` | 120 raw files, 5 years (7.6 GB) | 48.8 s | 0.16 | – | 216 + 6 s | 0.16 GB | cold |
| W3 `-P 16 remap,r360x180,<weights>` | same | 18.9 s | 0.40 | – | 200 + 4 s | 0.15 GB | warm |
| W3 `-P 1 -T remapbil`, 3 raw months, all variables | | 22–30 s each | – | 92–94 % | 4 + 1 s | 0.32 GB | cold |

### Read probe (zarrs 0.23.14)

| Store, chunk rows | Chunks, decoded / encoded | Mode | Threads / reads in flight (mean) | Time | Decoded GB/s | Encoded GB/s | Cache |
|---|---|---|---|---|---|---|---|
| W1 122:244 (decade 2) | 5856, 46.1 / 25.6 GB | pipelined | 128 / 256 (245) | 2.68 s | **17.2** | 9.5 | cold |
| W1 244:366 (decade 3) | 5856, 46.1 / 25.4 GB | fused | 128 / 128 (116) | 2.81 s | **16.4** | 9.0 | cold |
| W1 0:122 (decade 1) | 5856, 46.1 / 25.6 GB | fused | 1 / 1 | 20.9 s | 2.2 | 1.2 | warm |
| W1 0:122 | | fused | 16 / 16 | 1.73 s | 26.6 | 14.8 | warm |
| W1 0:122 | | fused | 64 / 64 | 1.22 s | 37.8 | 21.0 | warm |
| W1 0:122 | | fused | 128 / 128 | 1.33 s | 34.6 | 19.3 | warm |
| W1 0:122 | | pipelined | 128 / 256 | 2.66 s | 17.3 | 9.6 | warm |
| W4 12:18 (Jan–Jun 2021) | 1152, 18.7 / 10.5 GB | pipelined | 128 / 256 (191) | 1.86 s | **10.1** | 5.7 | cold |
| W4 0:6 (Jan–Jun 2020) | 1152, 18.7 / 10.5 GB | pipelined | 128 / 256 (242) | 2.21 s | 8.5 | 4.8 | warm (read by cdo) |
| W2 EERIE cloud `/kerchunk` `pr`, rows 10000: | 2000, 8.3 / 7.2 GB | async HTTP | 16 / 64 | 47.5 s | 0.175 | 0.152 | – |
| W2 EERIE cloud, rows 20000: | 2000, 8.3 / 7.2 GB | async HTTP | 16 / 16 | 41.5 s | 0.200 | 0.174 | – |

### Gate: passed

- **W1:** cdo's effective throughput is 0.13 GB/s decoded on cold data and 0.85–0.93 GB/s when the decade is in
  the page cache. `-P 16` does not raise it, because cdo's reads are serial: on cold data 97 % of its time is in
  `read`, one 4.4 MB chunk file every 60 ms (≈ 73 MB/s). The probe reads and decodes the cold decades at
  16.4–17.2 GB/s: **about 130× cdo on cold data and 18× cdo on warm data**, against a target of 5×. Warm against
  warm it is 37.8 against 0.93 GB/s, about 40×.
- **W4:** 10.1 GB/s cold against cdo's 0.19 GB/s, about 50×.
- **Raw NetCDF-4 (W2, W3):** cdo reaches 0.12–0.16 GB/s cold and spends 91–94 % of its time reading.
- Decision: continue as planned. Tasks 3–12 were built on this assumption and need no change.

### What else the numbers say

- **One node reads about 9–9.5 GB/s of W1 chunk files from Lustre.** 128 fused threads (116 reads in flight on
  average) and 256 pipelined reads reach the same rate. A single cold read runs at about 40–80 MB/s, as fast as
  cdo's serial reader, so throughput is reads in flight × per-read rate until the node saturates at roughly
  120 reads in flight.
- **cdors' default of 64 reads in flight (`exec::default_io_threads`) is probably too low on a compute node.** At
  ≈ 78 MB/s per read it gives about 5 GB/s encoded (≈ 9 GB/s decoded, as on the login node), against about 9 GB/s
  encoded (≈ 16 GB/s decoded) with 128. Not measured directly: the job ran no cold probe at 64. Proposal: 128
  reads in flight by default inside Slurm jobs, 64 on login nodes, checked in the benchmark by one extra W1 run.
- **Too many threads slow warm reads.** 128 decode threads plus 256 readers on 128 cores reach 17 GB/s warm
  against 35–38 GB/s with 64–128 fused threads (sys time 166 s against 24–35 s). More than about 128 reads in flight
  does not pay.
- **Cache state decides cdo's speed:** 360 s cold against 49–54 s warm on the same W1 decade (7×). The
  login-node cdo rates above (≈ 0.95 GB/s) were warm or helped by Lustre server caches. In the benchmark cdors reads
  first (cold) and cdo second (warm), which favours cdo.
- **cdo on the full W4 (1.1 TB, 30 × the year above), extrapolated:** `ydaymean` ≈ 30 × 191 s ≈ 1.6 h, just inside
  `bench.sbatch`'s 2 h timeout. `timpctl,95` ≥ 30 × 632 s ≈ 5.3 h, so it times out; cold it would be slower still,
  because the 620 GB of chunk files do not fit in the page cache.
- **The EERIE cloud gives 0.15–0.17 GB/s compressed (0.18–0.20 decoded) from a compute node too**, and 64
  requests in flight are no faster than 16. The cap seen from the login node is the server's. cdo reads the raw
  files of the same data at 0.15 GB/s, so cdors on the cloud runs about as fast as cdo on Lustre.
- cdo's memory: 0.5 GB for W1, 11.6 GB for W4 `ydaymean` (366 day-of-year sums in double precision), 9.7 GB for
  `timpctl`.
