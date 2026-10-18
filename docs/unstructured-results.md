# Unstructured round 1 on real data: cdors vs nereus on dart (Task 13)

Status (2026-10-19): two compute-node jobs (28230544, 28230657; 8 min 24 s together, 0.14 node-h) on master
`be0136d`. Dataset, sets and pass marks: `docs/unstructured-baseline.md`.

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

## Pass marks

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

## Where the OHC time goes (cheap splits, job 28230657)

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
- **Cost:** each case took ≤ 1.7 s of cdors and ≤ 1.3 GB RSS, with default threads (≤ 16 outside Slurm).
- **Result:** all 10 cases identical or within tolerance. The float32 outputs (fldmean, regmean, volint, fldint)
  differ by ≤ 3.6e-8, which is float32 rounding.

| case | status | cdors s | note |
|---|---|---|---|
| dart_mesh_info | identical | 0.96 | 3 160 340 nodes, area sum 3.600464e14 m² (rel 1e-15), wet levels 2..79 |
| dart_fldmean_lev | within tolerance | 1.27 | 3 levels, rel 2.4e-8 |
| dart_regmask_atl | identical | 0.88 | 924 967 nodes in both, 0 differ from nereus |
| dart_regmask_ar6 | identical | 1.08 | regionmask ar6.ocean on every 10th node: 316 034 nodes × 15 regions, 0 differ |
| dart_regmean_atl | within tolerance | 0.92 | 10 days, rel 3.6e-8 |
| dart_volint_atl | within tolerance | 1.67 | Atlantic OHC 0–100 m, rel 2.7e-8 |
| dart_extent | within tolerance | 0.73 | 10 days, rel 1.2e-14; `a_ice_above` 0 |
| dart_icearea | within tolerance | 0.68 | 10 days, rel 2.7e-8 |
| dart_transect | identical | 1.35 | Drake, 117 points × 3 levels, nearest node, dry levels missing |
| dart_selpoints | identical | 1.26 | 3 stations × 3 levels |

Runs: `/work/ab0995/a270088/cdors-target-r1-t13/realdata/20261019-000820-3886069/` (the first `dart_regmask_ar6` run
failed in the reference, because pyproj had no `PROJ_DATA`; fixed) and `…/20261019-000905-3903852/`.

## Scripts and records

- `bench/unstructured/validate.sh`:
  - `sbatch` runs the node measurement;
  - `sbatch -t 15 … extra` runs the first use without a plan before it, plus the timing splits.
  - Job 28230544 timed its "first use" after a `--plan` that had already built the cache (2.7 s). Its row 4 value
    is therefore from 28230657, and the script now times that run before its plan.
- `bench/unstructured/baseline.py --area nod_area`: the nereus reference with the mesh's own areas.
- `bench/unstructured/compare.py`: writes `compare.json`.
- Records: `/scratch/a/a270088/cdors-bench/unst-validate-28230544/` and `…-28230657/` (`runs.tsv`, `*.time`, `*.plan.json`,
  outputs, `nereus_ref_A/`, `compare.json`).
  - nereus reference run: 142 s, RSS 11.1 GB (mask 52 s, OHC 32 s + 27 s with surface areas, ice 20 s).

## Node-run cost

| job | resources | limit | actual | node-h |
|---|---|---|---|---|
| 28230544 | 1 exclusive compute node, `--mem=0 --hint=nomultithread`, account ab0995 | 40 min | 6 min 05 s | 0.10 |
| 28230657 | the same (`extra`) | 15 min | 2 min 19 s | 0.04 |
| total | | | 8 min 24 s | 0.14 (plan: ≤ 0.25) |
