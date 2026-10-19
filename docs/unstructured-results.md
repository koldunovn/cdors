# Unstructured round 1 on real data: cdors vs nereus on dart (Task 13)

Status (2026-10-19): re-run after the Task 13 fixes on master `d47d8fc`, as job 28231368 (3 min 40 s, 0.06 node-h).
The first measurement, before the fixes on `be0136d`, is kept below as "before": jobs 28230544 and 28230657.
Dataset, sets and pass marks: `docs/unstructured-baseline.md`. The follow-up the same morning (jobs 28236403 to
28236588, section "Follow-up") re-measured the warm rows warm and fixed the memory bound (`25aea2f`).

> **Every speed mark passes**, the warm OHC row after the follow-up's warm re-run (this run's "warm" OHC, 10.2 s,
> was not warm). **Values equal nereus to 9e-15 (OHC) and 3e-14 (ice extent).** The `--mem 2G` bound holds since
> the follow-up's mesh-reader fix: 1.72 GB.
> - **Atlantic OHC 0–700 m, 3 years**, natural chain
>   `-mulc,4089750 -volint,0/700 -maskregion,regions=basins,select=Atlantic_Basin -setgrid,fesom:dart -selname,temp`,
>   which no longer needs `-sellevel`:
>   - cold: **8.3 s**, **8.3×** faster than nereus (68.6 s); before the fixes 15.4 s, 4.5×;
>   - second run: 10.2 s here, but not warm; warm (follow-up): 5.9–6.5 s, 2.0× faster than nereus warm (13.1 s).
>     The old chain with `-sellevel`: 8.4 s here, 5.8–7.2 s warm;
>   - both chains read the same 16.38 GB and give bit-identical series.
> - **NH ice extent, 3 years of daily a_ice:** cold **7.1 s**, **17.8×** faster than nereus (126 s); warm 7.8 s.
> - **On par with the read probe.** OHC now takes about as long as reading its bytes. This run's probe took 9.4 s
>   cold and 7.6–8.7 s warm; OHC took 0.9× the probe cold and 1.0–1.3× warm.
>   - `volint` now folds in 1296 lanes instead of 36, and 42–64 cores are busy instead of 10–15.
>   - The ceiling is cdors' read path for these uncompressed NetCDF-4 files: 1.5–2.2 GB/s on this node. The
>     probe's warm runs were no faster than its cold run.
> - **First use of the dart mesh**, with a fresh cache and the Atlantic region, plus one step: **16.3 s**.
>   nereus needs 343 s for this. With the cache the mesh path adds **1.1 s** (before: 2.5 s).
> - **Memory:**
>   - Peak RSS with the default 4 GB budget: OHC 2.4–3.9 GB, ice 1.1–1.6 GB.
>   - Under `--mem 2G`: OHC peaked at 2.18–2.23 GB of 10⁹ bytes (first written as 2.13–2.18, KiB / 10⁶), ice at
>     1.05 GB. The plan estimates 1.62 GB for OHC. After the follow-up's fix OHC peaks at 1.72 GB.
>
> | 3 years, dart, 128 cores | cdors now | cdors before | nereus (Task 1) | read probe (this run) |
> |---|---|---|---|---|
> | OHC 0–700 m Atlantic, cold | 8.30 s (natural), 10.41 s (`-sellevel`) | 15.39 s | 68.6 s | 9.39 s |
> | OHC, warm | 10.20 s (natural), 8.41 s (`-sellevel`) | 13.92 s | 20.5 s | 7.63 s |
> | NH ice extent, cold | 7.11 s | 7.15 s | 126.3 s | 6.22 s |
> | ice extent, warm | 7.82 s | 6.15 s | 16.3 s | 6.43 s |
> | first use of the mesh + Atlantic | 16.3 s | 19.7 s | 343 s | – |
> | values vs nereus with `nod_area` | OHC 9.1e-15, extent 3.0e-14 | 2.9e-8, 3.0e-14 | – | – |

## Pass marks (job 28231368, master `d47d8fc`)

| | metric | mark | result | | note |
|---|---|---|---|---|---|
| 1 | OHC, 3 years, cold | ≤ 13 s (≥ 5× nereus 68.6 s) | natural 8.30 s (A), 11.13 s (C); `-sellevel` 10.41 s (B) | pass | 8.3× nereus; 0.88× this run's cold probe (9.39 s). Before: 15.39 s, miss |
| 2 | NH ice extent, cold | ≤ 12 s (≥ 10× nereus 126 s) | 7.11 s (A), 8.61 s (B) | pass | 17.8× nereus |
| 3 | warm: OHC / ice | ≤ 10 s / ≤ 8 s (≥ 2× nereus 20.5 s / 16.3 s) | OHC natural 10.20 s, `-sellevel` 8.41 s / ice 7.82 s | natural: **miss by 0.2 s**; `-sellevel` and ice: pass. Not warm; warm in the follow-up: **pass** | 2.0× / 2.4× / 2.1× nereus warm. The warm probe of the same bytes took 7.6–8.7 s, so the warm runs were bound by the read path, and the two OHC chains swapped places between the repeats (10.2/11.5 s vs 8.4/10.6 s). Before: 13.92 s, miss |
| 4 | first use of the dart mesh (fresh `CDORS_CACHE`, Atlantic region, one step) | ≤ 60 s | 16.34 s | pass | timed before any `--plan`; `--plan` no longer writes the cache |
| 4b | the same with the cache | ≤ 5 s overhead | 1.06 s | pass | 1.30 s for one step vs 0.24 s for the probe. Before: 2.5 s |
| 5 | peak RSS, any row | ≤ 2 GB | ice 1.0–1.6 GB; OHC 2.4–3.9 GB at the default 4 GB budget; OHC under `--mem 2G` 2.13–2.18 GB | **miss** (OHC); follow-up fix: **pass**, 1.72 GB | Under `--mem 2G` the plan estimates 1.62 GB, with 39 tiles in flight and 0.34 GB resident mesh. RSS is 6–9 % over the budget. One OHC step at the default budget peaks at 3.8 GB. Unaccounted: ~0.5 GB, first put down to HDF5/netCDF buffers and the allocator; the follow-up found copies in the mesh cache's level reads |
| 6 | values vs nereus with the mesh's `nod_area` | rel 1e-6 | OHC 9.1e-15 (36 months, both chains), extent 3.0e-14 (1096 days) | pass | against job 28230544's nereus reference on set A; timestamps equal; `volint` now writes f64 |

## Follow-up: warm re-run and memory (jobs 28236403, 28236468, 28236548, 28236588)

The same morning, on one exclusive node each (0.09 node-h in all), mode `v3` and `mem` of
`bench/unstructured/validate.sh`; records in `/scratch/a/a270088/cdors-bench/unst-validate-<job>/` (`runs.tsv`,
`cache.txt`, `rss_summary.txt`, `*.rss` sampled every 50 ms).

> **Warm, the natural OHC chain takes 5.9–6.5 s, 2.0–2.2× faster than nereus warm (13.1 s): row 3 passes. Under
> `--mem 2G` OHC now peaks at 1.72 GB instead of 2.15–2.28 GB: row 5 passes. Outputs are bit-identical.**
> - **Warm means warm** (job 28236403, binary `32b1dbd`). Only temp 1970–1972 (16.4 GB) was read between the
>   repeats. The Lustre client cache held 18.7 GB after the cold run and 20.2 GB after the cdors repeats, under
>   its 24 GB limit.
>   - cdors natural chain: cold 8.88 s; warm 6.51, 5.93, 6.35 s; after the nereus runs 3.13 and 3.46 s.
>   - With `-sellevel`: 7.16 and 5.75 s. Read probe: 4.20 and 3.98 s, later 1.35 s (12 GB/s).
>   - nereus, OHC only, Task 1's settings (file chunks, mesh cache, 128 threads): OHC phase 28.6 s on the first run
>     (the first Python start on the node), 13.1 s on the second.
>   - The first warm repeats spent 220–270 s of system time, the later runs 47–54 s. Why is not known.
> - **Where the memory went** (valgrind massif on one OHC step, job 28236468; RSS sampled every 50 ms). The run
>   stays within the estimate while it streams. The peak comes in the first second, while the 36 levels of
>   per-level areas (25 MB each) load from the mesh cache next to the first tiles. Each level read held the
>   compressed chunk, the decoded chunk, a clone and a NaN-filled assembly buffer, and many levels load at once.
>   glibc keeps freed memory for reuse, so the early peak stays the RSS. One step alone peaked at 2.00 GB.
> - **The fix** (`25aea2f`, job 28236588 against the frozen `32b1dbd` in the same job): a range that is exactly one
>   chunk is returned as decoded.
>
> | dart, 128 cores, warm | `32b1dbd` | `25aea2f` | wall `32b1dbd` → `25aea2f` |
> |---|---|---|---|
> | OHC 3 years, `--mem 2G` (estimate 1.62 GB) | 2.15 GB | **1.72 GB** | 4.52 → 4.34 s |
> | OHC one step, `--mem 2G` | 2.00 GB | 1.74 GB | 1.12 → 1.00 s |
> | OHC 3 years, default budget (estimate 2.77 GB) | 3.45 GB | 2.63 GB | 3.52 → 3.20 s |
>
> - **Variants of the new build under `--mem 2G`:** one malloc arena 1.65 GB, 16 threads 1.72 GB, netCDF-C read
>   path 1.67 GB, 1 MB mmap threshold 1.47 GB (but 5.6 s). Read probe without a mesh: 0.52 GB.
> - **Left:** the peak is 0.1 GB (6 %) above the estimate. The budget still holds because the tiles do not fill
>   their share of it.

## Read path (jobs 28231623 and 28231676, after the validation)

The validation's OHC and ice runs took about as long as the read probe, so the probe's own time was examined:
`-fldmean -sellevel,<36 levels> -selname,temp` on 3 years of dart `temp` (16.38 GB, uncompressed NetCDF-4,
chunks 3.2 MB), one exclusive node each (0.037 + 0.026 node-h), script `bench/unstructured/readpath.sbatch`, records in
`/scratch/a/a270088/cdors-bench/readpath-<job>/runs.tsv`.

> **From the page cache the probe now reads at 12.5–15.9 GB/s (was 8.1–9.2 GB/s). Cold reads stay at 2.2–2.5 GB/s:
> they are bound by Lustre, not by cdors' decoding.**
> - **The fix (commit 32b1dbd):** a chunk without filters is read straight into a vector of its element type, one
>   copy instead of zero fill, copy and conversion; compressed chunks skip the zero fill. User CPU of a warm probe
>   172–205 s → 53–62 s; outputs unchanged.
> - **"Warm" in the validation was not warm.** The Lustre client keeps at most 24 GB of file data
>   (`llite.*.max_cached_mb = 24576`). The validation's repeats read about 30 GB of distinct bytes (temp and a_ice)
>   between two runs of the same set, so each repeat read mostly from Lustre again. Its "warm" rows (OHC 10.2 s,
>   probe 7.6–8.7 s) are second cold runs; the warm mark of row 3 was not measured warm until the follow-up.
> - **Cold reads:** a plain C `pread` probe on the same files (32 or 128 threads) reads 3.5–4.5 GB/s cold (3.1 GB/s with the
>   client cache full, so that every read first evicts); cdors reads 2.2–2.5 GB/s (2.0 GB/s with the cache full).
>   The files have a progressive layout (first 1 GB on one storage target, the next 3 GB on 4, the rest on 16), and
>   cdors reads in time order, so mostly one `-cat` member at a time, and most reads of a member go to few targets.
>
> | probe, 3 years (16.38 GB) | before (cdors-base) | after (32b1dbd) | plain `pread`, 128 threads |
> |---|---|---|---|
> | cold (evicted) | 7.12–7.17 s, 2.28–2.30 GB/s | 7.42 s, 2.21 GB/s | 3.5–4.5 s, 3.5–4.5 GB/s |
> | cold, client cache full | 7.99 s, 2.05 GB/s | – | 5.24 s, 3.05 GB/s |
> | page cache (warm) | 1.79–2.03 s, 8.1–9.2 GB/s | 1.03–1.31 s, 12.5–15.9 GB/s | 0.24 s, 65 GB/s |

**Follow-up (not done in round 1):** spread cold reads across storage targets: interleave the chunk reads of
`-cat`/glob members (read the members' chunks in time order across files, not file after file), and order a file's
reads so that its PFL components are read at once. The pread probe's 3.5–4.5 GB/s is the bar; at that rate the
3-year OHC and ice runs, now bound by the cold read at 7–8 s, could take about 4–5 s.

## Before the fixes (master `be0136d`, jobs 28230544 and 28230657)


> **Values agree with nereus: OHC to 2.9e-8 and ice extent to 3e-14 relative. Ice passes every speed mark.
> OHC misses its speed and memory marks, and the cause is one kernel, `volint`.**
> - **NH ice extent, 3 years of daily a_ice:**
>   - cold: **7.2 s**, **17.7×** faster than nereus (126 s);
>   - warm: **6.2 s**, 2.7× faster than nereus warm;
>   - peak RSS 1.1–1.7 GB.
> - **Atlantic OHC 0–700 m, 3 years:**
>   - cold: **15.4 s**, 4.5× faster than nereus (68.6 s). The marks are ≤ 13 s and ≥ 5×.
>   - warm: **13.9 s**, 1.5× faster than nereus warm. The marks are ≤ 10 s and ≥ 2×.
>   - peak RSS 3.0–3.3 GB (mark ≤ 2 GB); 2.24 GB under `--mem 2G`.
> - **Why OHC is slow:** `volint` folds into one lane per output step, so 36 steps keep only 10–15 of the
>   128 cores busy. `fldmean` on the same mesh, with the same per-level areas and the same bytes, has
>   1296 lanes, uses ~90 cores and takes 9.7 s.
> - **The read bound was not reached.** This run's cdors read probe took 8.0–10.4 s cold (1.6–2.0 GB/s), slower
>   than Task 1's 6.5 s. OHC took 1.7–1.9× the probe; ice took 0.8× cold and 1.2× warm.
> - **First use of the dart mesh**, with a fresh cache and the Atlantic region, plus one step: **19.7 s**.
>   nereus needs 343 s for this. From the cache the mesh path adds 2.5 s.
>
> | 3 years, dart, 128 cores | cdors | nereus (Task 1) | cdors read probe (this run) |
> |---|---|---|---|
> | OHC 0–700 m Atlantic, cold | 15.4 s (best of 3 sets) | 68.6 s | 8.0–10.4 s |
> | OHC, warm | 13.9 s | 20.5 s | 6.7 s |
> | NH ice extent, cold | 7.2 s | 126.3 s | 8.8 s |
> | ice extent, warm | 6.2 s | 16.3 s | 5.1 s |
> | first use of the mesh + Atlantic weights | 19.7 s | 343 s | – |
> | values vs nereus with `nod_area` | OHC 2.9e-8, extent 3.0e-14 rel | – | – |

### Pass marks before

| | metric | mark | result | | cause / note |
|---|---|---|---|---|---|
| 1 | OHC, 3 years, cold, weights cached | ≤ 13 s (≥ 5× nereus 68.6 s) | 15.39 s (set C), 17.33 s (A), 17.65 s (B) | **miss** | 4.5× nereus. The bar was Task 1's read bound (6.5 s); against this run's probe (8.0–10.4 s cold) OHC is 1.7–1.9×, i.e. within the "≤ 2× read bound" part. The miss is the `volint` lane count (below), not the read path. |
| 2 | NH ice extent, cold | ≤ 12 s (≥ 10× nereus 126 s) | 7.15 s (B), 7.50 s (A) | pass | 17.7× nereus |
| 3 | warm: OHC / ice | ≤ 10 s / ≤ 8 s (≥ 2× nereus 20.5 s / 16.3 s) | 13.92 s / 6.15 s | **miss** / pass | OHC 1.47× nereus warm; ice 2.65×. Same cause as row 1. |
| 4 | first use of the dart mesh (mesh.diag → cache, Atlantic weights), cold | ≤ 60 s | 19.68 s | pass | `mesh info` with a fresh cache alone: 16.8 s; cache 1.26 GB |
| 4b | the same with the cache | ≤ 5 s overhead | 2.5 s | pass | 2.75 s for one OHC step vs 0.26 s for the probe of the same bytes. The 0.91 GB of per-level areas come from the cache. |
| 5 | peak RSS, any row | ≤ 2 GB | ice 1.1–1.7 GB; OHC 3.0–3.3 GB; OHC `--mem 2G` 2.24 GB | **miss** (OHC) | Default budget is 4 GB. The plan's estimate is 3.06 GB: per-level areas 0.91 GB + 97 tiles × 22 MB. The region mask doubles the tile size. Under `--mem 2G` the plan estimates 1.91 GB and the run peaks at 2.24 GB, 12 % over. The mesh's resident arrays (0.26 GB, `mesh info` `resident_bytes`) are not in the estimate. |
| 6 | values vs nereus with the mesh's `nod_area` | rel 1e-6 | OHC 2.9e-8 (36 months), extent 3.0e-14 (1096 days) | pass | timestamps equal; outputs bit-identical across runs, nodes and `--mem` |

## Values

- **Now (job 28231368):** OHC agrees to 9.1e-15, because `volint` now writes f64; the extent agrees to 3.0e-14.
  Both OHC chains, natural and `-sellevel`, give bit-identical series. The bullets below describe the definitions and the
  "before" run.

- **nereus reference:** `baseline.py --area nod_area` on set A.
  - Node centres come from mesh.diag `nodes`. On them `nr.get_region_mask(…, "Atlantic_Basin", "oceanBasins")` gives
    924 967 nodes, the same set as cdors' `-maskregion,regions=basins,select=Atlantic_Basin`: 0 nodes differ (login
    check `dart_regmask_atl`).
  - OHC is `nr.heat_content` with the 2-D per-level `nod_area[:36]`. Ice is `nr.ice_extent_nh` with `nod_area[0]`.
- **Same definition on both sides.**
  - The OHC chain is `-mulc,4089750 -volint,0/700 -maskregion,… -setgrid,fesom:dart -sellevel,<36 layers>
    -selname,temp`, with nereus' ρ·c_p = 1025 × 3990.
  - The 36 layers are the ones whose mid-depth is ≤ 700 m (nereus' rule). On dart they end exactly on the
    700 m interface, so cdors' cut `0/700` takes the same whole layers.
  - Dry levels are 0 in the files, and the per-level area of a dry node is 0, so both sides sum the same terms.
- **OHC 2.9e-8.** This is float32 rounding of the output: cdors writes the sum in the input's type (float32, as cdo
  does). The f64 accumulations agree far closer.
- **Ice extent 3e-14.** `fldhist` writes f64. `a_ice_above` is 0 on every day, so the bin is the whole extent.
- **Surface area only.** nereus with `nod_area[0]` for every layer is 0.40 % higher than cdors' per-level areas.
  Comparing per-level against surface areas is therefore wrong at the 4e-3 level.

## Where the OHC time went before the fixes (cheap splits, job 28230657)

All four causes below are fixed on master: the two-fold `volint`, the `maskregion` flag node, the z0/z1 push-down, and
the mesh memory in `--mem`. Now `volint` has 1296 lanes and keeps 42–64 cores busy, and the natural chain reads
16.38 GB.


| run (set A, warm unless noted) | wall s | CPU % | lanes | note |
|---|---|---|---|---|
| read probe `-fldmean -sellevel` (equal weights) | 6.7–6.9 | 8200–9300 | 1296 | the read path |
| `-fldmean -setgrid,fesom -sellevel` (per-level areas) | 9.6–9.8 | 8500–9000 | 1296 | mesh weighting costs ~3 s over the probe |
| `-volint,0/700` without the region | 12.3 | 1980 | 36 | |
| OHC (`volint` + `maskregion`) | 13.9–14.8 | 1150–1500 | 36 | region mask: +2 s |
| OHC, one year (12 steps) | 6.5–6.9 | 960–1160 | 12 | a third of the bytes, half the time |

- **The main cause is the lane count.** `volint` has one lane per output step: 36 for 3 years, 12 for one year.
  - The fixed-order fold runs the tiles of one lane one after another.
  - So at most about one core per step is busy: 10–15 cores for OHC, against ~90 for `fldmean`.
- **Follow-up (not done here):** fold `volint` per (step, level), as `fldmean` does, and sum the levels at the end
  in ascending order.
  - The combine order stays fixed, so outputs stay bit-identical.
  - The expected time is about the per-level `fldmean` time, ~10 s warm, which meets rows 1 and 3 on Task 1's bars.
- **Second cause, smaller:** the region mask is a whole-mesh leaf, 3.16 M values, attached to every tile.
  - It costs ~2 s, doubles the tile size (22 vs 9.5 MB) and halves the tiles in flight (97 vs 226).
  - In `--plan` it counts 65.5 GB of "decoded" bytes for 3 years.
  - A precomputed `Csr` region row would avoid both.
- **Fixed cost per run: 2.5 s.** It is the 0.91 GB of per-level areas read from the cache, plus the mask.
- **Recipe trap (for the guide):** `-volint,0/700` without `-sellevel` still reads all 79 levels.
  - That is 35.95 GB instead of 16.38 GB for 3 years, and 2.0 GB of per-level areas instead of 0.91 GB.
  - The depth range is not pushed down into the read. Either the recipe keeps `-sellevel,<layers above 700 m>`, or
    `volint,z0/z1` gets the push-down (follow-up).
- **The read probe was slower this time:** 8.0–10.4 s cold against Task 1's 6.5 s, on other nodes (l20451, l10444).
  - System time dominates the probe (380–720 s system vs 44 s user), as in Task 1.
  - Task 1's note still holds: cdors' HDF5 path for these uncompressed 3.2 MB chunks is the ceiling for the
    probe and for ice.

## Login-node check (Task 12, `bench/realdata_check.sh --only dart_…`)

- **Slices:**
  - temp: one month of 1975 (3 levels, or 11 for the 0–100 m `volint`);
  - a_ice: 10 days of 1975.
- **References:** NumPy on the same slice with mesh.diag's per-level `nod_area`, `nlevels_nod2D` and `nodes`. Also
  nereus' own Atlantic mask (Task 1's cache), regionmask 0.13 in the aqua environment, and scipy's cKDTree for the
  nearest node.
- **Re-run on `d47d8fc`** (fresh cache, so the loader-version-2 entry is rebuilt by the first case):
  - all 10 cases identical or within tolerance;
  - each case took ≤ 1.4 s of cdors, except the cache build in `dart_mesh_info` (14.7 s), and ≤ 1.4 GB RSS;
  - default threads (≤ 16 outside Slurm).
- `volint` now writes f64 (rel 7.9e-15). The remaining float32 outputs (fldmean, regmean, fldint) differ by ≤ 3.6e-8,
  which is float32 rounding.

| case | status | cdors s | note |
|---|---|---|---|
| dart_mesh_info | identical | 14.67 | 3 160 340 nodes, area sum 3.600464e14 m² (rel 1e-15), wet levels 2..79; cache built |
| dart_fldmean_lev | within tolerance | 0.83 | 3 levels, rel 2.4e-8 |
| dart_regmask_atl | identical | 0.72 | 924 967 nodes in both, 0 differ from nereus |
| dart_regmask_ar6 | identical | 0.86 | regionmask ar6.ocean on every 10th node: 316 034 nodes × 15 regions, 0 differ |
| dart_regmean_atl | within tolerance | 0.71 | 10 days, rel 3.6e-8 |
| dart_volint_atl | within tolerance | 0.83 | Atlantic OHC 0–100 m, rel 7.9e-15 (before: 2.7e-8, float32 output) |
| dart_extent | within tolerance | 0.68 | 10 days, rel 1.2e-14; `a_ice_above` 0 |
| dart_icearea | within tolerance | 0.67 | 10 days, rel 2.7e-8 |
| dart_transect | identical | 1.43 | Drake, 117 points × 3 levels, nearest node, dry levels missing |
| dart_selpoints | identical | 1.31 | 3 stations × 3 levels |

Runs:
- now: `/work/ab0995/a270088/cdors-target-r1-t13/realdata/20261019-012145-2061170/`;
- before: `…/20261019-000820-3886069/` and `…/20261019-000905-3903852/`. In the first of these, the
  `dart_regmask_ar6` reference failed because pyproj had no `PROJ_DATA`; that is fixed.

## Scripts and records

- `bench/unstructured/validate.sh`:
  - `sbatch` runs the node measurement;
  - `sbatch -t 25 … v2` runs the re-run after the fixes: both OHC chains, fresh-cache first use, `--mem 2G`, and the
    comparison with 28230544's nereus reference;
  - `sbatch -t 15 … extra` runs the first use without a plan before it, plus the timing splits.
  - Job 28230544 timed its "first use" after a `--plan` that had already built the cache (2.7 s). Its row 4 value
    is therefore from 28230657, and the script now times that run before its plan.
- `bench/unstructured/baseline.py --area nod_area`: the nereus reference with the mesh's own areas.
- `bench/unstructured/compare.py`: writes `compare.json`.
- Records: `/scratch/a/a270088/cdors-bench/unst-validate-28231368/` (now), `…-28230544/` and `…-28230657/` (`runs.tsv`, `*.time`, `*.plan.json`,
  outputs, `nereus_ref_A/`, `compare.json`).
  - nereus reference run: 142 s, RSS 11.1 GB (mask 52 s, OHC 32 s + 27 s with surface areas, ice 20 s).

## Node-run cost

| job | resources | limit | actual | node-h |
|---|---|---|---|---|
| 28230544 | 1 exclusive compute node, `--mem=0 --hint=nomultithread`, account ab0995 | 40 min | 6 min 05 s | 0.10 |
| 28230657 | the same (`extra`) | 15 min | 2 min 19 s | 0.04 |
| 28231368 | the same (`v2`, re-run after the fixes) | 25 min | 3 min 40 s | 0.06 |
| total | | | 12 min 04 s | 0.20 (plan: ≤ 0.25) |
