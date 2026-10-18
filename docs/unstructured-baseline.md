# Unstructured baseline: nereus vs the cdors read lower bound on dart (round 1, Task 1)

Status (2026-10-18): dataset described, scripts written, compute-node run done (job 28228738, l20238,
16 min 58 s of a 45 min limit, 0.28 node-h).

> **Gate: passed. nereus is not within 2× of the cdors read lower bound.**
> - **Cold:** nereus is **10.6×** slower on OHC and **21.7×** slower on ice extent than cdors only reading the same
>   bytes. These ratios use nereus' best chunking.
> - **Warm page cache:** nereus comes closer, **2.3–3.7×** on OHC and **2.7–2.9×** on ice.
>   - It is still outside 2×, but only just.
>   - The reason: cdors' own read rate on these uncompressed NetCDF-4 files is only 1.8–3.0 GB/s on 128 cores,
>     cached or not.
>   - cdors gains little from a warm cache, so cdors' read path, not Lustre, is the bound here.
> - **Mesh setup:** nereus also pays **343 s** once per session (`load_mesh` 302 s, Atlantic mask 41 s).
>
> | 3 years, 30.2 GB decoded per tool, node, 128 threads | cdors read bound | nereus, best chunking | ratio |
> |---|---|---|---|
> | OHC 0–700 m, cold (16.4 GB) | 6.50 s (2.52 GB/s) | 68.6 s (0.24 GB/s) | 10.6× |
> | NH ice extent, cold (13.9 GB, daily) | 5.82 s (2.38 GB/s) | 126.3 s (0.11 GB/s) | 21.7× |
> | OHC, warm | 5.48–9.10 s | 20.5 s | 2.3–3.7× |
> | ice, warm | 5.57–5.99 s | 16.3 s | 2.7–2.9× |
> | peak RSS | 0.7–1.4 GB | 5.1–6.2 GB | |
> | fresh session, cold, both diagnostics | 12.3 s (+ mesh setup, Task 5) | 540 s (343 s mesh and mask + 195 s) | ~44× |

## Dataset

**AWI-CM3 v3.1, TCO319L137-DART, `hist1950d`: raw FESOM2 output on the dart mesh.**

- **Path:** `/work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom/`.
  It is AWI's own run output (owner a270210, project ab0995), read-only for us.
- **Years:** 1950–2014, 65 files per variable.
- **Layout:** one file per variable and year, `<var>.fesom.<year>.nc`. There are 60 variables (temp, salt,
  u, v, w, a_ice, m_ice, MLD1-3, …).
- **Mesh:** dart, 3 160 340 nodes, 6 262 485 elements, 80 interfaces / 79 layers (0–6550 m).
  - The global attribute `FESOM_MeshPath = "/work/ba1264/a270210/model/input/fesom2/dart/"` names the run's copy.
  - That copy is byte-identical to `/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart/`: `cmp` on
    `fesom.mesh.diag.nc`, `nod2d.out`, `elem2d.out`, `aux3d.out`, `nlvls.out` and `elvls.out`.
  - The mesh is identified by the node count, `nod2 = 3160340` = `nod_n` of the dart `mesh.diag`.
- **Sizes, per year:**

  | file | steps | shape | size | chunks |
  |---|---|---|---|---|
  | `temp.fesom.<y>.nc` | 12 (monthly) | float32 `(12, 79, 3160340)` | 12.14 GB | `1 × 4 × 197522` (3.2 MB), uncompressed |
  | `a_ice.fesom.<y>.nc` | 365/366 (**daily**) | float32 `(365, 3160340)` | 4.61–4.63 GB | `1 × 790085` (3.2 MB), uncompressed |

  65 years of temp come to 789 GB, and 65 years of a_ice to 301 GB. The run has no monthly a_ice. A cdo
  `monmean` of 1950–2014 exists in `outdata/fesom/post-processing/a_ice.fesom.1950-2014_monmean.nc`; it is not
  used here, so that both tools read the model's own files. The NH ice extent is therefore a daily series.
- **Format:** NetCDF-4 classic model, written by netCDF 4.8.1 / HDF5 1.12.1. There is no deflate or other filter
  (`--plan` reports `codecs: none`).

### Dimension order: `nz1` before `nod2`, the horizontal dimension is last

```
dimensions:
	nz1 = 79 ;
	nod2 = 3160340 ;
	time = UNLIMITED ; // (12 currently)
variables:
	double nz1(nz1) ;   long_name "depth at layer midpoint", units "m", positive "down", axis "Z"
	double time(time) ; long_name/standard_name "time", units "seconds since 1950-01-01 0:0:0", axis "T",
	                    stored_direction "increasing"
	float temp(time, nz1, nod2) ;
		temp:description = "temperature" ;
		temp:long_name = "temperature" ;
		temp:units = "C" ;
		temp:_ChunkSizes = 1, 4, 197522 ;
	float a_ice(time, nod2) ;   units "%" (the values are fractions 0–1), _ChunkSizes = 1, 790085
```

- ⚠️ **The plan's assumption `temp(time, nod2, nz1)` is wrong for current FESOM2.**
  - FESOM 2.1 and later write `(time, nz1, nod2)`: `io_meandata.F90` defines the variable with
    `dimid(ndim:1:-1)` and `dimID(1) = nz1`. This dataset and the Cycle3 orca25 files both have this order.
  - The old order `(time, nod2, nz1)` exists in FESOM 2.0 era files, for example the PHC climatologies in
    `/pool/data/AWICM/FESOM2/climatology_PHC/{pi,core2_old}/temp.fesom.1948.nc`.
  - **Consequences:**
    - The horizontal dimension is already last in the data that matters, so the remap executor's
      `trailing_hdims()` accepts it as it is.
    - The Task 5 item "executor for `(time, nod2, nz1)`" is only needed for old files. It can be demoted to
      "accept both orders", with the common case first.
    - The Task 2 fixture `fesom_pi_data.nc` should use `(time, nz1, nod2)` (the current order), plus one
      small old-order file for the "both orders" check.
- The dimension names are `nod2`, `nz1` and `time`. Element fields (u, v) use `elem`.
- Mesh files use other names: `nod_n` / `nl` / `nl1` / `elem_n` in dart's `fesom.mesh.diag.nc`, and `nod2` /
  `nz` / `nz1` in newer ones (ng5, D3, dars).

### Dry levels are stored as exactly 0.0 (no `_FillValue`, no NaN)

- `temp` has no `_FillValue` or `missing_value` attribute.
- **Checked:**
  - All 79 levels of July 1950 against `nlvls.out`. Every wet cell (layer k < nlevels − 1) is non-zero, every dry
    cell is exactly 0.0, and there is no NaN.
  - Six single columns across the depth range, e.g. node 1615364 (nlevels 74): 73 wet layers, then 6 zeros.
- ⚠️ **`fesom.mesh.diag.nc`'s `nlevels_nod2D` differs from `nlvls.out` at one node.**
  - Node 0 (index 0, at 177.3°W 78.1°S, under the Ross ice shelf) has `nlevels_nod2D = 3` but `nlvls.out = 5`.
  - The data follows `nlvls.out`: 4 wet layers.
  - All other 3 160 339 nodes agree. `nod_area` at node 0 is non-zero for only 2 levels, so `mesh.diag` is
    consistent with itself.
  - Task 5 should treat the stored zeros as the dry mask's ground truth only as a cross-check. It should use
    `nlevels_nod2D` from `mesh.diag` (its source), and the doc and `mesh info` can mention the one-node mismatch
    on dart.
- The mesh has no `ulevels_nod2D` (no cavities in the bottom-up sense), so the top level is 1 everywhere.

### Attributes

- **Calendar:** the time axis has **no `calendar` attribute**.
  - cdors (and CF) read it as `standard`. `FESOM_include_fleapyear = -1` and 366 daily steps in 2000 confirm
    that the run has leap years.
  - Newer FESOM2 source writes `calendar`, `time_origin` and `time_bounds`; this run predates that.
- **Time reference:** `units = "seconds since <file year>-01-01 0:0:0"`, so it changes from file to file.
  Concatenating files needs a re-reference (cdors `-cat` handles it).
- **Time stamps:** at the end of the averaging period minus one model step: `1975-01-31 23:56:00`, …, and daily
  `…T23:56:00`.
- **Vertical coordinate:** `nz1` has units `m`, `positive = "down"` and `axis = "Z"`; there are no bounds. The
  interfaces are only in the mesh (`aux3d.out`, `zbar`).
- **Global attributes:**
  - **`FESOM_autorotate_back_to_geo = -1` is present.** This version (`e2f45f07`) wrote Fortran logicals
    directly, so `.true.` is stored as -1 (Intel Fortran) and `.false.` as 0. Current source writes
    `merge(1, 0, …)`, i.e. 1/0. So vectors in this run were rotated back to geographic. Vectors are not used in
    round 1, but Task 5's reader should store the flag and treat any non-zero value as true.
  - Also present: `FESOM_model`, `FESOM_git_SHA = "e2f45f07"`, `FESOM_MeshPath`, `FESOM_which_ALE = "linfs"`,
    `FESOM_mix_scheme = "KPP"`, `FESOM_use_partial_cell = 0` (full cells, as round 1 assumes) and
    `FESOM_force_rotation = -1`.
  - `FESOM_mesh_representative_checksum` is empty, so a mesh cannot be matched by checksum: use the node count
    plus `FESOM_MeshPath`.
- **Data variables:** `temp` and `a_ice` carry `description`, `long_name` and `units`. `a_ice:units = "%"` is
  wrong: the values are fractions (0–1, max 1.0).

### What cdors says today (master `faec69c`)

```
$ cdors sinfo temp.fesom.2000.nc
     1 : v     79   3160340 float32 1x4x197522           : temp
   Grid coordinates :
     1 : generic                  : points=3160340 (3160340x0)
   Vertical coordinates :
     1 : nz1                      : levels=79 units=m
   Time coordinate :              time : 12 steps
     Units = seconds since 2000-01-01 0:0:0  Calendar = standard
     First = 2000-01-31T23:56:00  Last = 2000-12-31T23:56:00
$ cdors griddes temp.fesom.2000.nc
gridtype  = generic
gridsize  = 3160340
xsize     = 3160340
xdimname  = nod2
```

- `zaxisdes` is not implemented.
- `fldmean` runs on the `(time, nz1, nod2)` layout without complaint. It warns `equal_weights` ("the generic grid
  has no coordinates"), which is correct: until Task 5 its values are not area means.
- `-sellevel,<36 levels>` is pushed into the read. `--plan` reads 1728 of 3840 chunks per year (5.46 of
  12.14 GB), since the 0–700 m layers fill exactly 9 chunks of 4 levels.
- `-cat f1 f2 …` and quoted globs work across the per-year files.

## Measurements

**Scripts.**
- `bench/unstructured/baseline.py`: nereus OHC and ice extent, timed phase by phase.
- `bench/unstructured/baseline.sh`:
  - `smoke` mode for the login node;
  - `sbatch` for the node run (`--exclusive --mem=0 --hint=nomultithread`, 45 min limit).
- Results: `/scratch/a/a270088/cdors-bench/unst-baseline-28228738/` (`runs.tsv`, `*.time`, `nereus_*.json`,
  `*.plan.json`, result series).

**What each tool computes.**
- **nereus 0.4.1** (xarray 2025.12.0, dask 2026.1.1; threaded scheduler, 128 workers, netCDF4 engine):
  - **OHC:** `nr.heat_content(temp[:, :36], area, layer_thickness[:36], depth[:36], depth_max=700, mask=atl)`,
    where `atl = nr.get_region_mask(lon, lat, "Atlantic_Basin", "oceanBasins")` (924 967 nodes).
  - **Ice:** `nr.ice_extent_nh(a_ice, area, lat)`, threshold 0.15.
  - **Mesh:** from `nr.fesom.load_mesh(dart dir)`, i.e. ASCII `nod2d.out`/`elem2d.out`/`aux3d.out`, with
    surface areas computed from the triangles.
    - The NetCDF loader cannot be used: it needs `lon`/`lat` variables, which dart's `mesh.diag` lacks.
    - After the first (timed) load, the mesh comes from an `.npz` cache, so the per-run times below exclude it.
  - **Level subset:** the temp array is cut to the 36 layers above 700 m before the call. Without the cut,
    `depth_max` only zeroes the deeper layers and dask still reads all 79, i.e. 2.2× the bytes (`--full-column`).
  - **Chunking:**
    - `file`: `chunks={}`, the files' 3.2 MB chunks;
    - `nod2`: `{time: 1, nz1: 4, nod2: -1}`, 50 MB chunks.
- **cdors read probe** (master binary, `faec69c`):
  - `cdors -fldmean -sellevel,<36 levels> -selname,temp -cat <3 files>`;
  - `cdors -fldmean -selname,a_ice -cat <3 files>`.
  - The equal-weight means are meaningless, but the probe decodes exactly the bytes nereus decodes: `--plan`
    gives 16.38 GB and 13.86 GB for 3 years, and nereus' own count is 30.24 GB.
- **Cold and warm runs.**
  - The three-year sets are disjoint, so each tool's cold run reads pages no run on that node had touched:
    A = 1970–1972 for cdors, B = 1980–1982 for nereus `file`, C = 1990–1992 for nereus `nod2`.
  - The warm runs repeat a set on the same node right after.

### Compute node (job 28228738, 128 cores, `runs.tsv`)

| run | years | wall s | peak RSS GB | GB decoded | GB/s | phases (s) |
|---|---|---|---|---|---|---|
| nereus mesh + mask (cold) | – | 348.9 | 1.18 | – | – | load_mesh 302.3, mask 41.2, import 4.3 |
| cdors temp 0–700 m, cold | A | 6.50 | 0.90 | 16.38 | 2.52 | 4652 % CPU, 263 s system time |
| cdors a_ice, cold | A | 5.82 | 0.70 | 13.86 | 2.38 | |
| nereus `file` chunks, cold | B | 322.6 | 5.10 | 30.24 | 0.094 | ohc 171.7, ice 144.6 (21 % of one core) |
| nereus `nod2` chunks, cold | C | 201.1 | 5.20 | 30.24 | 0.150 | ohc 68.6, ice 126.3 |
| cdors temp, warm | A | 5.48 | 1.14 | 16.38 | 2.99 | |
| cdors a_ice, warm | A | 5.99 | 1.40 | 13.86 | 2.31 | |
| cdors temp, warm | C | 9.10 | 0.98 | 16.38 | 1.80 | |
| cdors a_ice, warm | C | 5.57 | 0.92 | 13.86 | 2.49 | |
| nereus `nod2`, warm | C | 56.7 | 6.22 | 30.24 | 0.53 | ohc 31.6, ice 19.1 |
| nereus `file`, warm | B | 41.6 | 5.21 | 30.24 | 0.73 | ohc 20.5, ice 16.3 |

**Reading the table.**
- **Cold nereus is bound by the read lock, not by compute.** xarray's netCDF4 backend serialises HDF5 reads
  behind one lock, so cold `file` runs use 21 % of one core (`/usr/bin/time`) and read at 0.09 GB/s.
  - Bigger dask chunks (`nod2`) help OHC 2.5×, because there are fewer small reads, and ice hardly at all.
  - Warm, the same code reaches 0.5–0.8 GB/s: the lock then guards memory copies instead of Lustre reads.
- **cdors runs at about 2.5 GB/s whether the cache is cold or warm.** System time dominates (263 s system vs
  40 s user on the cold temp run).
  - Lustre is therefore not the limit for cdors on this format. Its HDF5 path for 3.2 MB uncompressed chunks
    is the ceiling.
  - Raising it (e.g. preads of the uncompressed, contiguous chunk extents) would widen the margin over nereus
    most on warm data. That is not in round 1's scope, but worth a note for Task 13.
- **Run-to-run spread:** cdors warm temp took 5.5 s on A and 9.1 s on C. Task 13 should repeat each run at least
  twice and report the best.
- **nereus area vs mesh areas.** nereus' triangle-computed areas are 0.11 % larger than `mesh.diag`'s `nod_area`
  (ratio 0.99957–1.00217 per node, median 1.0011). The likely cause is nereus' Earth radius (6371 km) against
  FESOM's (6367.5 km): (6371 / 6367.5)² = 1.0011.
  - **Consequence for comparisons:** cdors with `nod_area` and nereus with its own area differ by about 1e-3
    relative, before any per-level effect.
  - Task 13 should therefore compare against nereus called with `mesh.diag`'s 2-D `nod_area` (`heat_content`
    accepts it) at `rel=1e-6`, or else allow 1.5e-3 and say why.

### Login node (indications only, before the switch to compute-node measurements)

- **Single year 1960, cold, 16 dask workers:**
  - nereus `load_mesh` 224 s and mask 36 s;
  - OHC 56 s for 5.46 GB (0.097 GB/s) and ice 48 s for 4.63 GB (0.096 GB/s); RSS 2.0 GB.
- **cdors on 2001 temp 0–700 m, cold, `-P 16 --io-threads 32`:** 4.4 s for 5.46 GB (1.25 GB/s), RSS 0.33 GB.
- **Ratio:** the same order as on the node, 12× for OHC.
- **Smoke test:** `baseline.sh smoke` reads 2 steps of temp and 30 of a_ice (about 1.3 GB per tool, 8 threads)
  and finishes in about 12 s with the mesh cache in place.

## Pass marks for Task 13

**Setup of the node run.**
- `sbatch bench/unstructured/baseline.sh`, extended with the cdors mesh commands.
- Same years and sets, 128 cores.
- Each cdors run at least twice, best time reported.
- Bars:
  - "read bound" is this job's cdors probe;
  - "nereus" is the best nereus configuration above.

| | metric | pass mark | basis |
|---|---|---|---|
| 1 | Atlantic OHC 0–700 m, 3 years, cold (`-setgrid,fesom:dart` + Atlantic region, per-level `nod_area`, weights cached) | ≤ 13 s | ≥ 5× nereus (68.6 s); ≤ 2× the read bound (6.5 s) |
| 2 | NH ice extent, 3 years of daily a_ice, cold | ≤ 12 s | ≥ 10× nereus (126 s); ≤ 2× the read bound (5.8 s) |
| 3 | both, warm | OHC ≤ 10 s, ice ≤ 8 s | ≥ 2× nereus warm (20.5 s, 16.3 s) |
| 4 | first use of the dart mesh (read `mesh.diag`, build the Atlantic weights), cold | ≤ 60 s | nereus: 343 s |
| 4b | the same with cached weights | ≤ 5 s overhead over row 1 | |
| 5 | peak RSS, any row | ≤ 2 GB | nereus 5.1–6.2 GB; probe 0.7–1.4 GB |
| 6 | values | OHC and extent equal to the nereus reference at `rel=1e-6` | nereus called with `nod_area` (2-D) |

If row 1 or 2 misses only the "≤ 2× the read bound" part but keeps the nereus factor, report that, do not fail
it. The read bound itself (about 2.5 GB/s) is a cdors I/O property, not a round-1 feature.

## Node-run cost

| job | resources | limit | actual | node-h |
|---|---|---|---|---|
| 28228738 | 1 exclusive compute node, `--mem=0 --hint=nomultithread`, account ab0995 | 45 min | 16 min 58 s | 0.28 |

