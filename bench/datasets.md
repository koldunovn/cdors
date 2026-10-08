# Benchmark datasets (W1–W4)

Chosen and checked on 2026-10-08 from the Levante login node. Sizes: "decoded" = shape × 4 bytes
(float32); "on disk" = compressed chunk bytes (measured with `du` or summed from the kerchunk
`size` column; sampled where marked ~). GB = 10⁹ bytes.

| | Workload (Task 13) | Data | Shape, chunks | Decoded / on disk | cdo reads it? |
|---|---|---|---|---|---|
| W1 | `yearmean` | nextGEMS ICON ngc4008, `tas`, daily, HEALPix z9, genuine Zarr v2 on Lustre | 10958 × 3145728, chunks 30 × 65536 | 137.9 / 76.6 GB (decade view: 45.9 / ~25.6 GB) | yes, NCZarr, but only through a single-variable view |
| W2 | `fldmean -sellonlatbox` | EERIE ICON-ESM-ER control-1950, `pr`, daily, 0.25° regular; NetCDF-4 on Lustre + Parquet kerchunk + EERIE cloud | 36890 × 721 × 1440, chunks 1 × 721 × 1440 | 153.2 / 133.3 GB (decade: 15.2 / 13.2 GB) | raw NetCDF-4 files: yes. Cloud: no |
| W3 | `remapbil` | same model, ocean `to` at 1 m depth (SST), daily, 0.25° regular, NetCDF-4 on Lustre | 36890 × 1 × 721 × 1440, chunks 1 × 1 × 721 × 1440 | 153.2 / 76.4 GB (5 years: 7.6 / 3.8 GB) | yes (raw NetCDF-4 files) |
| W4 | `timpctl,95`, `ydaymean` | ngc4008 `tas`, 3-hourly, HEALPix z9, Zarr v2, **chunked in space** | 87664 × 3145728, chunks 248 × 16384 | 1103 / ~620 GB | yes, NCZarr, through a view |
| W4+ | memory demonstration (cdors only) | ngc4008 `tas`, 15-minute, HEALPix z9, chunked in space | 1051968 × 3145728, chunks 192 × 16384 | 13 237 / ~7 400 GB | technically yes; a cdo run is too expensive |

## W1 — daily HEALPix Zarr on Lustre

- **Store:** `/work/kd1453/rechunked_ngc4008/ngc4008_P1D_9.zarr` (nextGEMS catalog
  `https://data.nextgems-h2020.eu/catalog.yaml` → `ICON/main.yaml` → `ngc4008`, `time=P1D`, `zoom=9`).
  ngc4008 is the nextGEMS "prefinal" ICON coupled run (2020–2049). It is listed in the catalog that the EERIE
  catalog calls `dkrz_ngc3`, but the run itself is not a Cycle 3 run. The Cycle 3 ICON stores in the same
  catalog (`ngc3028`) cover only about 2 years, too short for W1.
- **Format:** genuine Zarr v2 directory store (not kerchunk), consolidated `.zmetadata`, `dimension_separator "/"`.
  111 arrays in the store, of which `tas` is one.
- **Variable:** `tas` (2 m temperature, K), dims `(time, cell)`, `<f4`, fill value NaN (no missing cells).
- **Shape / chunks:** 10958 × 3145728; chunks 30 × 65536 = 7.86 MB decoded per chunk, 48 spatial chunks per
  time block, 366 time blocks, 17 568 chunks.
- **Codec:** compressor `blosc` (cname lz4, clevel 5, shuffle 1, blocksize 0), no filters. Compression ratio
  0.556 (210 MB per full time block on disk).
- **Grid:** HEALPix nside 512 (zoom 9), nested order, 3 145 728 cells. The `crs` variable carries
  `grid_mapping_name = "healpix"`, `healpix_nside = 512`, `healpix_order = "nest"`. There is no `cell`
  coordinate variable. cdo 2.6.0 reports the grid as `gridtype = projection` with that grid mapping, **not**
  as a HEALPix grid. It warns that `clat`/`clon` are missing, and `fldmean` runs without a weights warning.
- **Time:** `time` is `<i8`, `seconds since 1970-01-01`, `proleptic_gregorian`. It runs daily from 2020-01-02
  to 2050-01-01; each daily mean is stamped at 00:00 at the end of its day.
- **Size:** 137.9 GB decoded and 76.6 GB on disk (`du`: 73 043 MiB).
- **Benchmark input: one decade, via a view** (`bench/make_view.py`, symlinks only, no data copied):
  - `/scratch/a/a270088/cdors-bench/views/ngc4008_P1D_9_tas_10y.zarr`: steps 0–3651, 2020-01-02 to
    2029-12-31, 45.9 GB decoded, ~25.6 GB on disk;
  - `…/ngc4008_P1D_9_tas_10y_2030.zarr`: steps 3660–7311, 2030-01-09 to 2040-01-08, the cold-cache decade
    for the read probe;
  - also `…_30d`, `…_365d`, `…_3y` (the first 30, 365 and 1096 steps) for the short login-node tests.
- **cdo access:** `cdo sinfon 'file:///work/kd1453/rechunked_ngc4008/ngc4008_P1D_9.zarr#mode=zarr,file'` works.
  **However,** on the full 111-variable store the chain `-seltimestep,1/30 -selname,tas` did not finish 30
  timesteps in 90 s and read 11.9 GB from Lustre. cdo evidently touches far more than `tas`. On the
  single-variable view, the same 30 steps take 0.5 s. The cdo baseline therefore uses the views. The views
  hold exactly the bytes that cdors will read, so both tools see the same input.

## W2 — the same daily variable on Lustre (kerchunk) and in the EERIE cloud

- **Dataset:** `icon-esm-er.eerie-control-1950.v20240618.atmos.gr025.2d_daily_mean` (EERIE ICON-ESM-ER
  coupled control-1950, atmosphere, regridded to 0.25°).
- **Variable:** `pr` (precipitation flux, kg m-2 s-1), dims `(time, lat, lon)`, `<f4`.
- **Shape / chunks:** 36890 × 721 × 1440; chunks 1 × 721 × 1440 = 4.15 MB decoded. These are the HDF5
  chunks of the raw files, so the layout has **one complete field per chunk**.
- **Codec:** HDF5 filter 32001 (blosc lz4, clevel 5, shuffle, blocksize 4152960). In the kerchunk metadata it
  appears as `"compressor": null, "filters": [{"id": "blosc", ...}]`. The compression ratio for `pr` is poor
  (mean 3.61 MB per chunk, 0.87).
- **Grid:** regular lon-lat 1440 × 721, lon 0 to 359.75, lat −90 to 90, 0.25°.
- **Time:** in the kerchunk/cloud view, 1950-01-01T12:00 to 2050-12-31T12:00, daily, `minutes since 1950-01-01`,
  `gregorian`. **The raw files carry other dates:** the model years run from 1991-01-02 00:00 to 2091-12-01, with
  each daily mean stamped at the end of its day, and the kerchunk relabels these to 1950–2050 mid-day. Chunk
  `pr/0.0.0` is the first step of `…_19910101T000000Z.nc`. Any cdo-vs-cdors comparison across the two access
  paths must therefore ignore timestamps or shift them.
- **Raw files (Lustre):**
  `/work/bm1344/k202193/ICON/erc2002/postprocessing/interpolation/control_1950/atm_2d_1d_mean_remap025/run_<YYYYMM01>T000000-*/erc2002_atm_2d_1d_mean_remap025_*.nc`.
  These are NetCDF-4 (HDF5) files. Each month has one directory with two files: days 2 to the end of the month
  (≈1.9 GB, 21 variables), and a one-step file holding the first day of the next month. There are 2424 files
  (2.2 TB) in total. The decade used is `run_1991*`–`run_2000*`: 240 files, 3653 steps, 15.2 GB decoded and
  13.2 GB compressed for `pr`.
- **Kerchunk references: Parquet**, not JSON:
  `/work/bm1344/k202193/Kerchunk/erc2002/control_1950/v20240618/atm_2d_1d_mean_remap025.parq`. This is the fsspec
  `LazyReferenceMapper` layout: `.zmetadata` with `record_size: 100000`, then one directory per variable with
  `refs.0.parq`, whose columns are `path, offset, size, raw`. Small arrays such as `time` are inline in `raw`.
  Rows are in chunk-key order, and rows past the array's chunk count are null. The EERIE disk catalog
  (`/pool/data/Catalogs/dkrz_eerie.yaml` → `disk/model-output/icon-esm-er/eerie-control-1950/v20240618/atmos/gr025/main.yaml`,
  entry `2d_daily_mean`) points there as `reference::/…parq`. The JSON kerchunks named in the plan
  (`/work/bm1344/DKRZ/kerchunks/<exp>/*.json.NNNN`, `{"version": 1, "refs": …}`) exist for other datasets.
  **Task 10 needs the Parquet format first.**
- **EERIE cloud:** `https://eerie.cloud.dkrz.de/datasets/icon-esm-er.eerie-control-1950.v20240618.atmos.gr025.2d_daily_mean/kerchunk`
  (Zarr v2, consolidated `.zmetadata`, the same metadata as the Parquet refs). Chunks are passed through byte for
  byte: `pr/0.0.0` is 3 574 329 bytes, the same as the Parquet `size`.
  - ⚠️ The `/zarr` endpoint named in the plan answers **HTTP 403** for every dataset tried (2026-10-08).
  - ⚠️ `https://eerie.cloud.dkrz.de/intake.yaml` answers **404**. The dataset list is at
    `https://eerie.cloud.dkrz.de/datasets` (945 ids).
- **cdo access:** the raw files work, but use `-select,name=pr <files…>`. A `-selname,pr -mergetime [ files ]`
  chain reads every variable: one year did not finish in 115 s, against 6.9 s with `select`. cdo cannot open
  the cloud store: `#mode=zarr` gives "Malformed URL", and `#mode=zarr,s3` tries DAP4. cdo cannot read the
  Parquet refs either.

## W3 — daily SST on a regular grid

- **Dataset:** `icon-esm-er.eerie-control-1950.v20240618.ocean.gr025.2d_daily_mean` (disk catalog entry
  `ocean/gr025/main.yaml` → `2d_daily_mean`). The refs are
  `/work/bm1344/k202193/Kerchunk/erc2002/control_1950/v20240618/oce_2d_1d_mean_remap025.parq`, and the same
  `/kerchunk` cloud endpoint returns 200.
- **Variable:** `to` (sea water potential temperature, °C) on the single level `depth = 1 m`, which serves as the
  SST. Dims `(time, depth, lat, lon)`, `<f4`, `missing_value = -9e33` (land, 696 099 of 1 038 240 cells are ocean).
- **Shape / chunks / codec:** 36890 × 1 × 721 × 1440, chunks 1 × 1 × 721 × 1440 (4.15 MB), blosc lz4 as HDF5
  filter 32001. Mean chunk on disk 2.07 MB.
- **Grid:** regular 0.25° lon-lat, the same as W2. cdo's bilinear weights accept it ("Bilinear weights from
  lonlat (1440x721) to lonlat (360x180) grid, with source mask (696099)").
- **Raw files:** `/work/bm1344/k202193/ICON/erc2002/postprocessing/interpolation/control_1950/oce_2d_1d_mean_remap025/run_*/erc2002_oce_2d_1d_mean_remap025_*.nc`
  (NetCDF-4, 33 variables, ≈1.5 GB per month, 1.7 TB in total).
- **Benchmark range:** five years, `run_1991*`–`run_1995*`: 120 files, 1826 steps, 7.6 GB decoded and 3.8 GB
  compressed for `to`. The target grid is `r360x180`; weights come from `cdo genbil,r360x180`.
- No daily SST on a native curvilinear (ORCA) grid turned up in the EERIE disk catalog within the time box.
  If a curvilinear source is wanted, it needs a further search.

## W4 — multi-TB, chunked in space

- **Store:** `/work/kd1453/rechunked_ngc4008/ngc4008_PT3H_9.zarr` (catalog `ngc4008`, `time=PT3H`, `zoom=9`),
  a genuine Zarr v2 store with 93 arrays.
- **Variable:** `tas`, 3-hourly means, 2020-01-01T03:00 to 2050-01-01T00:00, `<f4`. Grid and time encoding as in W1.
- **Shape / chunks:** 87664 × 3145728; **chunks 248 × 16384** = 16.25 MB decoded. That gives **192 spatial
  chunks per time block**, so a percentile pass over one block of cells reads only 1/192 of the data. There are
  354 time blocks and 67 968 chunks.
- **Codec:** blosc lz4, clevel 5, shuffle. ~1.76 GB per full time block on disk (sampled), ratio ~0.56.
- **Size:** 1103 GB decoded, ~620 GB on disk.
- **Baseline input (Task 2):** one year, view `/scratch/a/a270088/cdors-bench/views/ngc4008_PT3H_9_tas_1y.zarr`
  (2928 steps, 36.8 GB decoded, ~21 GB on disk). The full 30 years is for Task 13.
- **cdo access:** works through NCZarr. netCDF-C caches one complete time block (192 chunks = 3.1 GB RSS) per
  open stream, so `timpctl,95 in -timmin in -timmax in` holds about 9.4 GB of chunk cache.
- **W4+** (if "multi-TB" must mean more than 1 TB): `/work/kd1453/rechunked_ngc4008/ngc4008_PT15M_9.zarr`, `tas`
  (6 variables in the store), 1 051 968 × 3145728, chunks 192 × 16384 (12.6 MB), blosc lz4. That is 13.2 TB
  decoded and ~7.4 TB on disk (sampled at 1.35 GB per block). A cdo `timpctl` over it would read about 40 TB,
  which is not feasible as a baseline; use it only to show that cdors keeps memory bounded.
- ⚠️ Do not use zoom 10 of ngc4008 for `tas`: every chunk of `ngc4008_P1D_10.zarr/tas` is 1366 bytes (all NaN),
  and the same holds for `PT3H_10`.

## Helper

`bench/make_view.py SRC.zarr OUT.zarr VAR [NTIME [START]]` builds the single-variable time-window views used
above. It only creates symlinks; `time` is rewritten as one small uncompressed chunk when the window does not
start at 0.
