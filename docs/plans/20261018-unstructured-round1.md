# cdors round 1: analysis on unstructured meshes (FESOM2 first)

## Overview

This round adds nereus-style analysis (`~/PYTHON/nereus`) to cdors, built natively for agents:
- region and latitude-band statistics;
- area-weighted histograms;
- vertical and volume integrals that know each cell's own bottom;
- point extraction and vertical sections.

Every grid is treated as a set of cells with areas. Model meshes come in through one shared mesh vocabulary, and
FESOM2 is the first model.

- **Problem.** On native ocean meshes cdo can do area-weighted field statistics, latitude bands (`zonaldes`) and
  polygon selection, but nothing that needs the mesh itself. It cannot read FESOM meshes. It has no per-cell bottoms
  or volumes, no multi-region tables and no sections. nereus does all of this in Python on xarray/dask, but it is
  slow on large meshes and needs a notebook. On 12.6M HEALPix cells the KD-tree build alone takes 6–11 s, and the
  computation then runs through dask graphs.
- **Approach (brainstorm 2026-10-18, validated section by section; plan revised after a plan review the same day).**
  - **One repository, one binary (`cdors`).** New code lives in a pure-Rust crate, `cdors-mesh` (no netCDF/HDF5), so
    a Python module can be built from it later.
  - **Horizontal operations are sparse matrices (targets × cells).**
    - Linear operations (means, sums, integrals, mapping between nodes and elements, points, sections) run on the
      existing remap executor. It is parallel, renormalises around NaN, and prunes chunks that no weight touches.
    - Non-linear ones (min, max, std, histograms) use a row reducer over the same matrix.
    - Weights are cached per mesh and target.
  - **Naming.** Operators with cdo names keep cdo semantics. New operators get new names and are marked
    `origin: "cdors"`.
- **Scope.** Round 1 covers:
  - the mesh vocabulary;
  - the FESOM2 mesh reader (from `fesom.mesh.diag.nc`);
  - brainstorm groups 1–3: (1) regions and bands, (2) vertical geometry, (3) sampling of scalar fields.

  Not in round 1 (see Post-Completion):
  - R2: neighbour operators and plotting;
  - R3: FESOM2-native transports, MOC, vectors on sections;
  - then IFS, FESOM3 and the other models.
- **No implementation starts** until Nikolay gives a separate, explicit yes to this plan with its cost estimate
  (see Technical Details → Cost). Task 1 (the day-1 gate) needs its own yes before any compute-node job, and
  Task 6 starts with a licence decision.

## Context (from discovery)

**cdors today (master `faec69c`).** A Rust workspace, ~31k lines: `crates/cdors-core` (library) and `crates/cdors`
(binary).

- **Grids, areas, levels**
  - `model/grid.rs`: `Grid`, with kinds Regular, Gaussian, Curvilinear, Unstructured, Healpix and Generic; `reduced`
    rows for GRIB reduced grids.
  - `model/area.rs`: cell areas (including `huiliers_area`), `zonal_rows`, and `layer_weights` (per level only) for
    `vert*`.
  - `model/zaxis.rs`: `ZAxis` with bounds.
- **Remap executor and weights**
  - `remap/weights.rs`: `RemapWeights` is CSR by destination, with private fields and `apply_batch`. The private
    `MissingRule` has three variants: `Renormalise`, `Drop`, `AllOrNothing`.
  - `ops/remap.rs`:
    - the weight cache lives in `$CDORS_CACHE/weights`;
    - the private `prune()` drops source cells that no link reads;
    - ⚠️ `trailing_hdims()` (`:254`) requires the horizontal dimensions last. Current FESOM2 (2.1 and later) writes
      `temp(time, nz1, nod2)`, which fits; FESOM 2.0 wrote `temp(time, nod2, nz1)`, which does not (Task 1).
- **Statistics operators**
  - `ops/fldstat.rs`: field weights are `Weights::Const` or `Weights::Each` (per cell), with no per-level variant.
    `zonal()` refuses unstructured grids.
- **Other code**
  - `ops/files.rs`: `setgrid`.
  - `ops/mod.rs`: the operator registry (`OpSpec`, `implemented`).
  - `error.rs`: error codes.
  - `crates/cdors/src/ops_cmd.rs`: `ops --json`.
  - `docs/agent-guide.md`: the text behind `cdors guide` (5.4 kB, `include_str!`).
  - Under `--json`, warnings go to stderr as JSON lines (`exec/threads.rs`). A `warnings[]` array exists only in
    `--plan` output.

**Test harness.** `tests/run_cases.sh` and `tests/cases.txt`.
- **Rows:** `tag | args | fixtures`. Tags are exact, ulp, bin, text, json and err. The `json` tag today only checks
  that stdout parses.
- **Fixtures:** `tests/make_fixtures.sh` builds `<name>.nc` files (r36x18_std, hpz2_noleap, unst_360, nocoord_360,
  plus Zarr copies). `{fix}` expands to the fixtures directory, and `{cdors:OPT}` passes options to cdors only.
- **Variants:** Zarr, grib, and the planner check (tiny chunks, `--mem 1M`, other thread counts). The planner check
  runs only when the row's first fixture is `VARIANT_FIXTURE=hpz2_noleap`.
- **Reference:** cdo 2.6.0; a full run takes ~30 s.

**FESOM2 meshes** live in `/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/<mesh>/`: core2, D3, dars, dart, farc, hr, mr, ng5,
orca25, pi.

- **What `fesom.mesh.diag.nc` holds:**
  - `nodes`, `elem`, `edges`, `edge_tri`, `nod_in_elem2D`;
  - `nod_area(nl, nod_n)`, an area per level;
  - `elem_area`;
  - `nlevels_nod2D` and `nlevels`; `ulevels_nod2D` only in newer meshes;
  - `zbar`, `Z`, `zbar_n_bottom`, `zbar_e_bottom`;
  - `edge_cross_dxdy` and `gradient_sca_x/y`.
- ⚠️ **`nodes` units vary.** They are radians in core2 and dart, degrees in ng5, and no file has a `units` attribute.
  The node dimension is `nod_n` in older files and `nod2` in newer ones.
- **Test mesh: pi** (3140 nodes, 5839 elements, 47 layers). The ASCII mesh files sit alongside, but they are not
  used in round 1.
- **Node order** is mostly local: a Drake section touched 2 of 32 chunks of 100k nodes on dart and 2 of 75 on ng5. It
  can jump on some meshes; then more chunks get read, which costs time but not correctness.
- **Memory:** on ng5 (~7.4M nodes × ~70 levels) per-level areas alone are ~4 GB in f64, so mesh components must load
  lazily.

**FESOM2 code facts** (`~/fesom2_sp/src`):
- `gen_modules_config.F90`:
  - `use_partial_cell=.false.` by default, so round 1 uses full layers;
  - Euler angles 50/15/−90.
- `io_meandata.F90`: `vec_autorotate=.FALSE.` by default, so output velocities are in the rotated frame. The global
  attribute `…autorotate_back_to_geo` records which frame was used. Vectors are not used in round 1.

**References and environments** (`/work/ab0995/a270088/mambaforge/envs/`; always run with `python -I`):
- `nereus`: nereus and shapely;
- `pyfesom2`: pyfesom2;
- `aqua` or `pcmdi`: regionmask and geopandas.

`jq` is at `/usr/bin/jq`.

**cdo semantics that apply** (`crates/cdors-core/data/cdo_help.txt`):
- `zonmean`, `zonsum` and `zonint` accept `zonaldes=zonal_<DY>` on unstructured grids.
- `selregion` and `maskregion`:
  - take ASCII polygon files (lon lat per line, `&` between polygons);
  - select by cell centre;
  - `dcw:<CC>` needs GMT's DCW database.
- `fldint` = Σ value·area.
- `vertmean` is weighted by layer thickness; `vertsum` is unweighted. cdo has no `vertint`.
- `intlevel` interpolates linearly between levels.
- `bottomvalue`.

**regionmask** (stable docs, read 2026-10-18).
- **Sets:**
  - ar6.all, ar6.land and ar6.ocean: 58, 46 and 15 regions;
  - srex 26; giorgi 21; prudence 8, overlapping;
  - Natural Earth countries, land and ocean_basins_50. Use v5.1.2 for ocean_basins_50 (117 regions mixing seas,
    bays, straits and rivers); older versions stop short of 180°E.
- **Rules:**
  - a cell belongs to a region if its centre is inside it;
  - edges are straight lines in lon/lat (as in shapely, rasterize and cdo `selregion`);
  - a point exactly on an outline goes to the polygon on its left (`rasterio.features.rasterize` convention);
  - −180° maps to 180°;
  - points at −90° are nudged 1e-10 north;
  - polygon holes are unmasked;
  - each set carries an overlap flag.

**nereus regions** (`diag/data/*.geojson`, ~25 MB). They come from MPAS geometric_features (IHO seas, via
marineregions.org), through pyfesom2:
- oceanBasins: 9;
- MOCBasins: 4;
- NinoRegions: 3.

## Development Approach

- **testing approach: Regular.**
  - Code first, then rows in the table-driven harness. References are cdo, regionmask, nereus, pyfesom2 and analytic
    fields.
  - Unit tests only for geometry primitives and to pin bugs.
  - Test effort never exceeds code effort, and the suite stays in seconds (`lean-fast-tests`).
- Complete each task before starting the next; keep changes small and focused.
- **Every task adds or updates tests** (harness rows and/or unit tests) for the code it changes.
- **All tests pass before the next task:** `cargo test` plus `tests/run_cases.sh` (all rows, both NetCDF read paths,
  all variants).
- **Update this plan** when the scope changes during implementation.
- **Backward compatibility:** every existing row of `tests/cases.txt` passes unchanged, and cdo-named operators keep
  cdo semantics on grids without a mesh.
- **Fixed combine order.** Every new reduction (region rows, histograms, `vertint`/`volint`) folds in a fixed order:
  cells ascending, levels ascending, time ascending, with the state carried across chunks. Outputs are then
  bit-identical across chunk layouts and thread counts, as in the prototype.

**Execution constraints** (`~/.claude/CLAUDE.md`):
- **Agents:** at most 3–5 at a time, each on one task, with a written deliverable and a time box. No Workflow tool.
- **What agents may not do:** commit (the orchestrator reviews and commits), delete anything, or submit Slurm jobs.
- **Approval first:** compute runs (the Task 1 node run, Task 13) and the agent check (Task 14) need Nikolay's
  explicit yes. After a submit, a background shell watches `sacct`; the model does not poll.
- **Publishing:** nothing is pushed or published, and nothing is written to the shared install, without approval of
  the exact text or content. No Claude markers or session links anywhere.
- **Paths:** builds and caches go under `/work/ab0995/a270088` or `/scratch/a/a270088`, never home. The build
  directory is `CARGO_TARGET_DIR /work/ab0995/a270088/cdors-target`.

## Testing Strategy

The harness gains three capabilities (Task 2), so that every check in this plan actually runs.

- **Non-cdo references: `ref:<lib>.<fn>[:abs=…][:rel=…]`**
  - `<lib>` (`regionmask`, `nereus`, `pyfesom2`, `numpy`) selects the conda environment. `<fn>` names a function in
    `tests/ref/reference.py`.
  - `reference.py` receives the row's expanded argument string, the fixture paths and the output path. It writes
    NetCDF with the same variable names and time axis as the cdors output, so the existing `diffn` and timestamp
    comparisons apply.
  - The cache key covers: the sha of `reference.py`, the package versions in the environment, the arguments, and the
    fixture files including a content listing of mesh directories (name, size, mtime).
  - A missing environment or package is a FAIL, not a skip.
  - Default tolerance: `rel=1e-6` for f64 accumulations compared with NumPy-based references. A row may tighten it
    (`exact`) or loosen it; any loosening must give its reason in a comment.
- **Assertions on JSON: `json:<jq-expr>`.** The expression runs on `{"out": <stdout JSON>, "stderr": [<stderr JSON
  lines>]}` and must evaluate to `true`. Every row about a warning, a count (`regions.filled`) or `mesh info` uses it.
- **Planner variant on FESOM.** `VARIANT_FIXTURE` becomes a list: `hpz2_noleap fesom_pi_data`. `fesom_pi_data` gets a
  tiny-chunk Zarr copy, `fesom_pi_data_tiny.zarr2`, whose chunks split both `nod2` and `nz1`. Every new operator has
  at least one row on `fesom_pi_data`, so its bit-identical-across-tilings check runs.
- **Fixtures** (Task 2):
  - `fesom_pi/`: a copy of the pi mesh directory;
  - `fesom_pi_deg/`: `mesh.diag` with nodes in degrees and the dimension named `nod2`;
  - `fesom_pi_data.nc`, 24 monthly steps, in FESOM's dimension order with dry levels stored as 0:
    - `temp(time, nz1, nod2)`, analytic in lon, lat and z;
    - `salt(time, nz1, nod2)`;
    - `a_ice(time, nod2)` and `m_ice(time, nod2)`;
    - `u(time, nz1, elem)` and `v(time, nz1, elem)`;
  - `r1x1_edges.nc`: a 1° grid with centres on integer degrees, to hit AR6 outlines exactly;
  - a fixture-local regions directory, passed via `CDORS_REGIONS`.

  Mesh directories are passed as `{fix}/fesom_pi` inside the arguments, not as fixture tokens.

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- update plan if implementation deviates from original scope
- keep plan in sync with actual work done

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): code, tests, scripts and documentation in this repository.
- **Post-Completion** (no checkboxes): later rounds, decisions for Nikolay, external checks.

## Implementation Steps

### Task 1: Day-1 gate: nereus baseline and cdors read probe on real FESOM2 data

**Files:**
- Create: `bench/unstructured/baseline.py` (nereus + xarray/dask)
- Create: `bench/unstructured/baseline.sh` (login-node subset; an `srun` variant submitted only after a yes)
- Create: `docs/unstructured-baseline.md`

- [x] **Find the dataset:** one native-mesh FESOM2 dataset with monthly 3D `temp` and `a_ice`, on dart or ng5, under
      `/work/bm1344/AWI/` (`Cycle3/IFS_28-FESOM_25-cycle3`, `IFS_9-FESOM_5-cycle3`, `EERIE/…`).
- [x] **Describe it** in `docs/unstructured-baseline.md`:
  - path, mesh name, years, size;
  - chunking and **dimension order** (`nod2` before or after `nz1`);
  - **how dry levels are stored** (0, `_FillValue` or NaN);
  - whether `autorotate_back_to_geo` is present.
- [x] baseline.py: Atlantic OHC 0–700 m monthly series (`nr.heat_content` with the nereus Atlantic basin mask) and NH
      ice extent (`nr.ice_extent_nh`) over N years. Record wall time, peak RSS and dask settings.
- [x] cdors read probe on the same files (`-fldmean -sellevel,…` and `-fldmean -selname,a_ice`). This is the lower
      bound for round 1, since reading dominates.
- [x] Run the login-node subset first. The node run (`srun -c 128 --hint=nomultithread --mem=0`, ≤ 0.1 node-h) happens
      only after Nikolay's yes, with a background `sacct` watcher.
- [x] Write the gate: speed pass marks for Task 13. If nereus is already within ~2× of the read lower bound, STOP and
      report before Task 2.
- [x] tests: none (measurement task); the scripts run end to end on the subset.
- ⚠️ Dataset: AWI-CM3 `TCO319L137-DART/hist1950d` raw FESOM2 output under `/work/ab0995/a270210/…/outdata/fesom/`
  (dart, 1950–2014). The `/work/bm1344/AWI` candidates are orca25 (Cycle3, post-processed) or ng5 (EERIE, GRIB
  names). There is no monthly `a_ice` in the run, so the ice extent uses the daily files.
- ⚠️ **Dimension order is `temp(time, nz1, nod2)`, with the horizontal dimension last** (FESOM ≥ 2.1). Only FESOM 2.0
  era files (`climatology_PHC/{pi,core2_old}`) have `(time, nod2, nz1)`.
  - This changes the Context line on `trailing_hdims()`, the Task 2 fixture order and the Task 5 executor item:
    the current order works today, and the old order only needs "accept both".
  - Dry levels are exactly 0.0 (no `_FillValue`, no NaN). There is no `calendar` attribute (cdors reads it as
    standard).
  - `FESOM_autorotate_back_to_geo = -1` (`.true.`) is present.
  - `mesh.diag` `nlevels_nod2D` differs from `nlvls.out` at one dart node (node 0). Details are in
    `docs/unstructured-baseline.md`.
- ⚠️ The user approved compute-node runs during this task, so login-node runs were cut to a smoke test. The node run
  was job 28228738: 17 min of a 45 min limit, 0.28 node-h (more than the 0.1 node-h first planned, for cold
  and warm sets of 3 years). Gate passed: nereus is 10.6× (OHC) and 21.7× (ice) slower than the cdors read bound
  when cold, and 2.3–3.7× when warm.

### Task 2: Harness: `ref:` references, `json:` assertions, FESOM planner variant, fixtures

**Files:**
- Modify: `tests/run_cases.sh`
- Create: `tests/ref/reference.py`
- Create: `tests/make_fesom_fixtures.py`
- Modify: `tests/make_fixtures.sh`
- Modify: `tests/cases.txt`

- [x] `ref:<lib>.<fn>[:abs=…][:rel=…]` tag as specified in Testing Strategy. Cache in `$CDORS_REF/ref-<key>`. A
      missing env or package FAILs.
- [x] `json:<jq-expr>` tag over `{"out", "stderr"}` (Testing Strategy).
- [x] `VARIANT_FIXTURE` becomes a list (`hpz2_noleap fesom_pi_data`), and `fesom_pi_data_tiny.zarr2` is added with
      chunks that split `nod2` and `nz1`.
- [x] `make_fesom_fixtures.py` (nereus env) writes the fixtures listed in Testing Strategy. `make_fixtures.sh` calls
      it, and the fixture list is documented in its header.
- [x] Write tests: self-test rows.
  - A `ref:numpy.fldmean` row equals cdo on r36x18_std, and a deliberately wrong reference fails.
  - A `json:` row that is true passes, and one that is false fails.
  - A missing env fails.
- [x] Run tests: the full harness passes, and no existing row changes.

- ➕ Row syntax: `ref:numpy.fldmean | -fldmean {in} | r36x18_std hpz2_noleap`, with tolerance
  `ref:numpy.fldmean_wrong:rel=1e-4 | …`, `…:abs=0.01`, `…:exact`; `json:any(.stderr[]; .warning == "equal_weights")
  | --json -fldmean -selname,a_ice {in} {out} | fesom_pi_data`. Arguments and fixtures are the last two `|` fields, so
  a jq expression may contain `|` (not `#`). New placeholder `{out}`: an output file in the row's run directory.
- ➕ Environments: `numpy` and `nereus` → nereus, `pyfesom2` → pyfesom2, `regionmask` → aqua;
  `CDORS_REF_ENV_<lib>=<env or prefix>` overrides. Values pass if `|cdors − ref| ≤ abs + rel·|ref|` everywhere, with
  missing values in the same places. This is checked by `reference.py compare`, because cdo's `diffn` counts a field
  as different when *either* abslim or rellim is exceeded, so it has no pure relative mode. Names and timestamps are
  compared with cdo as before. The planner variants of a `ref:` row compare with the base output exactly, as for cdo
  rows.
- ➕ Self-test: `tests/selftest_ref.sh` runs `tests/selftest_ref.txt` (16 rows, 10 of them expected to fail, each
  with its message checked: wrong reference, `exact` against float32 output, unknown modifier, missing env, missing
  package, unknown lib, unknown function, false and non-boolean `json:` results, a jq syntax error).
- ➕ `ulp` limits are now computed variable by variable, because cdo `outputf` aborts on a file with several grids
  (FESOM nodes and elements). For files with one grid the value is unchanged.
- ⚠️ **Real FESOM2 output puts `nz1` before `nod2`.** `/work/ab0995/a270088/fesom2_forca20_snap/vnod.fesom.1959.nc`
  (FESOM git 6b26861a) has `vnod(time, nz1, nod2)`. It is NetCDF-4 classic with chunks (1, 5, nod2/12) and deflate 1,
  `_FillValue` 9.96921e+36, no `calendar`, and time in `seconds since <year>-01-01 0:0:0`. FESOM 2.0 wrote
  `(time, nod2, nz1)`. The fixture follows this plan (`(time, nod2, nz1)`). The order is the constant `ORDER_NODE`
  / `ORDER_ELEM` in `make_fesom_fixtures.py`, and the dry value is `DRY`. Task 1 records which order the target
  dataset has; the loader probably needs both.
- ⚠️ **ng5's `mesh.diag` differs from pi's in more than `nod2`.** Its dimensions are `elem`, `nz` and `nz1` (not
  `elem_n`, `nl`, `nl1`), and its variables are `nz` and `nz1` (not `zbar` and `Z`) and `elements` (not `elem`). It
  adds `lon`/`lat`, and `gradient_sca_x/y` are `(elem, n3)`. `fesom_pi_deg/` reproduces all of this (nodes in
  degrees), so Task 5's loader must accept both layouts.
- ⚠️ cdors already reads `(time, nod2, nz1)` exactly as cdo does for the non-remap operators tried (`selname`,
  `seltimestep`, `sellevel`, `copy`, `timmean`, `yearmean`, `fldmean`, `vertmean`). That holds on NetCDF (both read
  paths), Zarr v2/v3 and the tiny chunks. cdo sees generic grids (3140 nodes, 5839 elements) on a generic 47-level
  z-axis. No disagreement to hand to Task 5, apart from remap's `trailing_hdims()`.
- ⚠️ **Planner variant on FESOM, two limits for later rows.**
  - Under `--mem 1M` a tile must stay below ~0.5 MB. The NetCDF fixture is therefore chunked (1 step, 1/8 of the
    cells, all levels); chunks of half the cells failed with `memory_limit`.
  - A chain whose intermediate holds the whole output fails with `intermediate_too_large`, for example
    `-selname,u -timmean` (6.8 MB). Keep selections innermost in rows on `fesom_pi_data`.
- ⚠️ The `regions/` directory is not made here. Task 6's export script fills `$CDORS_FIXTURES/regions`, and
  run_cases.sh exports `CDORS_REGIONS` with that default.

- ⚠️ Orchestrator, after Task 1 (2026-10-18): `fesom_pi_data` now uses current FESOM2's `(time, nz1, nod2)` and
  `(time, nz1, elem)`; `fesom_pi_data_v20` holds the same values in FESOM 2.0's `(time, nod2, nz1)` layout (two
  existing-operator rows on it).

### Task 3: `cdors-mesh` crate: vocabulary and geometry

**Files:**
- Create: `crates/cdors-mesh/Cargo.toml`
- Create: `crates/cdors-mesh/src/lib.rs`
- Create: `crates/cdors-mesh/src/mesh.rs`:
  - `Mesh` and `Place`;
  - `Csr` incidence tables;
  - `Vertical`: wet-level ranges, and `Thickness::Static` only, since variable and computed thickness are deferred;
  - manual `Debug` (sizes only).
- Create: `crates/cdors-mesh/src/geom.rs`:
  - unit vectors, great-circle distance and paths, for sections;
  - spherical polygon area, moved from `model/area.rs` (`huiliers_area*`) and re-exported;
  - region point-in-polygon in **planar lon/lat with straight edges**, with regionmask's edge, wrap, pole and hole
    rules.
- Create: `crates/cdors-mesh/src/locate.rs`: KD-tree on unit vectors (pure Rust, e.g. `kiddo`).
- Create: `crates/cdors-mesh/src/reader.rs`: an `ArrayReader` trait that reads a variable, attribute or dimension by
  name, plus a sub-range read.
- Modify:
  - `Cargo.toml` (workspace);
  - `crates/cdors-core/Cargo.toml`;
  - `crates/cdors-core/src/model/area.rs`;
  - `crates/cdors-core/src/model/grid.rs`: `mesh: Option<Arc<Mesh>>`.

- [x] Vocabulary types. Components that grow with the number of cells (per-level areas, topology) load lazily through
      the reader, and their bytes count against `--mem`. A grid without a mesh behaves exactly as today.
- [x] Built-in grids (regular, Gaussian, HEALPix, unstructured with areas) give a one-place `Mesh` view: centres and
      areas.
- [x] `geom.rs` primitives and the KD-tree locator.
- [x] Write tests (unit):
  - polygon area against today's `huiliers_area` results;
  - point-in-polygon cases: on an edge, at −180/180, at −90, inside a hole, on a long AR6 edge at high latitude
    (planar, not great circle).
- [x] Run tests: `cargo test` and the full harness.
- ⚠️ Lazy components (`Lazy<T>`, per-level areas as one `Lazy` per level) report their size
  (`Lazy::bytes`, `Mesh::resident_bytes`, `Mesh::lazy_bytes`) but are not yet counted by the planner:
  the operator that touches them (Task 5 `Weights::PerLevel`) adds them to its estimate, as
  `plan/explain.rs` does for remap weights.
- ⚠️ `Regions::new(regions, overlap)` takes the set's overlap flag: regionmask's `mask_3D` on a set
  without overlap (AR6 `all`, whose land and ocean outlines overlap in slivers) gives a cell to the
  last region containing it only. Checked against regionmask 0.13 on AR6 `all` and SREX: 1° grid on
  integer degrees (outlines, −180, −90) plus 20 000 random points, all memberships identical on both
  of its paths (rasterize and shapely).
- ⚠️ KD-tree: a small own implementation (`locate.rs`, ~100 lines) instead of `kiddo` (not in the
  cargo cache; no new dependency, and a fixed tie rule: on equal distances the smallest cell index).
- ➕ `Grid::cell_centres` (degrees, storage order) and `Grid::mesh_view` (one place `cells`, areas from
  `area::cell_areas`); a unit test checks r36x18 and hpz2 (areas sum to 4πR²).

### Task 4: Mesh cache and `cdors mesh info`

**Files:**
- Create: `crates/cdors-mesh/src/ugrid.rs`: mesh to UGRID 1.0 arrays and back. Vertical geometry and per-level areas go
  in as `cdors_*` attributes.
- Create: `crates/cdors-core/src/io/mesh_reader.rs`: `ArrayReader` for NetCDF and Zarr.
- Create: `crates/cdors-core/src/model/mesh_cache.rs`.
- Create: `crates/cdors/src/mesh_cmd.rs`.
- Modify: `crates/cdors/src/main.rs`.

- [x] **Cache entry** = UGRID-Zarr at `$CDORS_CACHE/meshes/<key>.zarr`.
  - The key comes from the realpath, size and mtime of each source file plus a loader-version salt; contents are not
    hashed on every run.
  - The entry stores a content fingerprint, the sha of the first and last MiB of each source file, which is checked
    on load.
- [x] **Concurrency.**
  - Build into `<key>.zarr.tmp-<pid>-<rand>`, write a completion marker, then rename; the first rename wins.
  - A loser removes only its own uniquely named temp directory.
  - An entry without its marker, or with the wrong fingerprint, is renamed aside to `<key>.zarr.bad-<time>` and
    rebuilt. It is never deleted.
- [x] `cdors mesh info <spec> [--json]` reports:
  - places, sizes and the area sum;
  - the vertical: levels, plus min and max wet levels;
  - topology tables;
  - how units were decided;
  - source files and the cache path.

  The cache entry is itself a UGRID store, and its path is printed.
- [x] Write tests:
  - UGRID round trip (unit);
  - `json:` rows for `mesh info` on hpz2 and r36x18;
  - two concurrent `mesh info` runs on a fresh cache both succeed and leave one entry.
- [x] Run tests.
- ⚠️ The content fingerprint is FNV-128 (the hash cdors already uses for its weight cache keys), not a
  sha: no new dependency, and it guards against changed files, not against an adversary.
- ⚠️ Temporary names carry the host as well: `<key>.zarr.tmp-<host>-<pid>-<random>` (a pid alone is
  not unique on a shared file system). Unusable entries go to `<key>.zarr.bad-<unix time>-<pid>`, with
  a `mesh_cache_rebuilt` warning.
- ➕ `crates/cdors-mesh/src/ugrid.rs`: `write(mesh, extra_attrs, &mut dyn UgridSink)` / `read(Arc<dyn
  ArrayReader>)`; cdors-core's `ZarrSink` (in `mesh_cache.rs`) writes Zarr v3 (blosc zstd 1). Lazy
  components are streamed through once without being kept (`Lazy::get_uncached`,
  `Levels::level_uncached`): building the ng5 entry holds one level of `nod_area` at a time.
- ➕ `io/mesh_reader.rs`: one `SourceReader` over any `ChunkSource` (both NetCDF paths, Zarr);
  hyperslabs are assembled from the chunks they touch (contiguous NetCDF: one record per chunk).
- ➕ Concurrency and set-aside are tested in `crates/cdors/tests/mesh_cache.rs` (two `cdors mesh info`
  processes on a fresh cache; a wrong fingerprint; per-level areas and `elem_nodes` read back lazily
  equal the source). Each test leaves its cache directory under the cargo target directory.
- ➕ ng5 (7.4M nodes, 69 levels) on the login node: cold `mesh info` (builds the entry, 2.9 GB) 37 s,
  peak RSS 1.6 GB; warm 1.6 s, 0.9 GB; without `CDORS_CACHE` 4.2 s, 1.0 GB. Per-level areas
  (4.1 GB) and `elem_nodes` (295 MB) stay unloaded.

### Task 5: FESOM2 loader, `-setgrid,fesom:`, per-level weights, FESOM dimension order, `tonodes`/`toelems`

**Files:**
- Create: `crates/cdors-mesh/src/models/mod.rs`: the registry, one table mapping model → loader, rules and native
  capabilities.
- Create: `crates/cdors-mesh/src/models/fesom2/mod.rs`, `loader.rs` and `rules.rs`.
- Modify: `crates/cdors-core/src/ops/files.rs`: mesh specs in `setgrid`.
- Modify: `crates/cdors-core/src/error.rs`: new code `grid_mismatch`.
- Modify: `crates/cdors-core/src/ops/fldstat.rs`: `Weights::PerLevel` and dry-level masking.
- Modify: `crates/cdors-core/src/ops/remap.rs`: non-trailing horizontal dimensions; `prune` reused.
- Modify: `crates/cdors-core/src/remap/weights.rs`: public `from_csr` and a public `MissingRule`.
- Create: `crates/cdors-core/src/ops/places.rs`.
- Modify: `crates/cdors-core/src/ops/mod.rs`, `tests/ref/reference.py`, `tests/cases.txt`.

- [x] **Loader from `fesom.mesh.diag.nc`:**
  - **Units:** radians if both |lat| ≤ π/2 + ε and |lon| ≤ 2π + ε, otherwise degrees. An override is available
    (`fesom:<dir>,units=deg|rad`), and the decision is shown in `mesh info`.
  - **Names:** both `nod_n` and `nod2` are accepted.
  - **Top levels:** `ulevels_nod2D` is optional; without it the top level is 1.
  - **Areas:** per-level `nod_area`.
  - **Layers:** full layers (FESOM2 default `use_partial_cell=.false.`).
- [x] **Rules:**
  - dimensions: `nod2`/`nod_n` → nodes, `elem`/`elem_n` → elements, `nz` → interfaces, `nz1` → mid-levels;
  - levels below a cell's bottom (or above its top) are missing, whatever value the file stores (0 in FESOM output).
- [x] **`-setgrid,fesom:<dir>`.** Sizes are checked against the data; a mismatch is error `grid_mismatch`, and its
      hint names both sizes and the mesh. Area-weighted operators (`fldmean`, `fldint`, `fldstd`, …) then use the mesh
      areas, at level k `nod_area[k]`, through `Weights::PerLevel`. A deviation from cdo's 2-D area goes into
      `docs/deviations.md`.
- ⚠️ Loader half done (Task 4 agent): loader, rules, registry, `-setgrid,fesom:<dir>[,units=deg|rad]`
  with `grid_mismatch`, 2-D field statistics with the mesh's 2-D areas (`AreaSource::Mesh`), and
  `fldint` (cdo's Σ area·value; it was not implemented). Open for the other half: the
  `Weights::PerLevel` part of the setgrid item, dry-level masking, the executor, `from_csr`,
  `tonodes`/`toelems`. Until then `fld*`/`zon*`/`vert*` refuse variables with levels on a mesh
  (`not_implemented`), whatever their dimension order.
- ⚠️ Both mesh.diag layouts are read: old (`nod_n`, `elem_n`, `nl`, `zbar`, `Z`, `elem(n3, elem_n)`)
  and new (`nod2`, `elem`, `nz`/`nz1` as variables, `elements` or `face_nodes` with `start_index`,
  `gradient_sca_*` as `(elem, n3)`). No mesh on /pool has `ulevels_nod2D`; it is read when present.
- ➕ What the other half needs: `Grid::mesh` + `Grid::mesh_place` (the place of the grid's cells);
  `cdors_mesh::models::dim_rule(&mesh, dim)` → `DimRule::{Place, Mid, Interfaces}`;
  `mesh.vertical.wet(place)` (0-based half-open mid-levels, `top..bottom` from `ulevels-1`,
  `nlevels-1`) and `models::wet_range(wet, rule, cell)` for interfaces; per-level node areas
  `place.area_per_level.level(k)` = `nod_area[k]` (the layer's top interface; FESOM2 `nod_area` has
  `nl` rows, the last one is zero). Elements have one area for all levels (`elem_area`).
- ➕ Element centres: normalised mean of the unit vectors of the three nodes. The grid's coordinates
  are `lon`/`lat` for the first place (nodes) and `lon_<dim>`/`lat_<dim>` for the others, so outputs
  with node and element variables keep both.
- [x] **Executor for FESOM 2.0's `(time, nod2, nz1)` order** (current FESOM2 writes `(time, nz1, nod2)`, which the
      executor already handles; both orders must give identical results, fixture `fesom_pi_data_v20`). `FieldKernel` lanes accept horizontal dimensions that are
      not last: they gather to `(…, nz1, nod2)` on input and scatter back on output. The output keeps the input's
      dimension order.
- [x] **`RemapWeights::from_csr`** takes a public rule: `Renormalise` for means, `Drop` for sums and integrals.
      Per-level weights are one membership CSR, scaled by `area[k][col]` at apply time; there is no matrix per level.
- [x] **`tonodes`/`toelems`** from `elem`:
  - toelems: the mean of the three nodes;
  - tonodes: the element-area-weighted mean;
  - NaN as in remap.
- [x] Write tests:
  - `fldmean`/`fldint` per level on `fesom_pi_data` vs `ref:nereus.surface_mean` (and a NumPy per-level reference);
  - the degrees and radians meshes give identical `mesh info` (`json:`); ✓ (loader half)
  - an `err:grid_mismatch` row; ✓ (loader half)
  - dry levels stored as 0 do not enter `fldmin`/`fldstd`;
  - `tonodes`/`toelems` vs NumPy;
  - all rows on `fesom_pi_data`, so the planner variant runs;
  - existing remap rows unchanged.
- [x] Run tests.
- ➕ Executor half (second agent): dry levels are masked on the leaves `-setgrid` puts on the mesh
  (`plan::WetMask` on `Leaf::mask`, applied to each gathered tile through the leaf's maps, so any
  later selection or dimension order is handled; the stored level maps to the mesh level), and
  carried to intermediates still on a mesh place (`-fldmean -toelems ...`). `fld*` on variables
  with levels use `Weights::PerLevel` for nodes (the mesh's own `nod_area[k]` arrays, loaded per
  lane on first use, so `--plan` loads nothing; normalised to sum 1 per level except for
  `fldint`, as cdo's 2-D weights) and `elem_area` for elements; `--plan` lists them in
  `kernel.mesh_weights` and `memory.parts.mesh_weights`. `zon*`/`vert*` on levels stay refused.
- ⚠️ Fix (after the merge; found by the Task 9 agent): the dry-level mask sat on the stored leaves
  below `-setgrid`, so arithmetic between them and `setgrid` undid it (`-setgrid,fesom:… -addc,1
  -mulc,0` gave 1 at dry levels: cdo's `mulc,0` turns missing into 0). The mask is now an
  expression node, `Expr::Mask(WetMask, …)`, wrapped around the value `setgrid` receives (stored
  variable, arithmetic, intermediate), with its own level and cell maps that follow selections and
  renumbering like a leaf's; `Leaf::mask` is gone. The mesh level of each index comes from a mask
  the value already has (setgrid twice), else from a leaf's stored level dimension (a `-sellevel`
  below `setgrid`), else from the depths of the level axis matched to the mesh's mid-levels or
  interfaces (`-setgrid -yearmean -sellevel,…`). Intermediates above a masked value keep the mask
  (also on a cell subset); `intlevel` drops it (`Expr::unmask`). Rows for each shape vs NumPy
  (`reference.py` applies the arithmetic under `setgrid` and the time means, with cdo's group
  timestamps).
- ⚠️ The plan's "one membership CSR scaled by `area[k][col]`" is not needed for `fld*`: their
  per-level weights are the area rows themselves (no matrix). `tonodes`/`toelems` weights do not
  depend on the level (1/3; `elem_area` share). The scaled-membership form is left to the region
  statistics of Task 6, which can build it on `from_csr` (rows: regions, weights 1) and scale by
  `place.area_per_level.level(k)` at apply time.
- ⚠️ `tonodes` on all 47 levels of the tiny-chunk copy does not fit the planner variant's
  `--mem 1M` (a lane holds all 5839 elements of 5 timesteps, next to two tiles in flight), so
  that row keeps 5 levels on `fesom_pi_data` (all levels on `fesom_pi_data_v20`).
- ⚠️ `hpdegrade`/`hpupgrade` and `remap*` accept non-trailing horizontal dimensions too (the
  same lanes); the existing rows are unchanged.

### Task 6: Region catalog, `regmask` and region statistics

**Files:**
- Create: `tools/export_regions.py` (aqua env). It turns regionmask sets into GeoJSON with names, abbreviations, the
  overlap flag, the source and the licence.
- Create: `crates/cdors-mesh/data/regions/*.geojson`: small embedded sets (ar6.*, srex, giorgi, prudence, nino).
- Create: `crates/cdors-mesh/src/regions.rs`: catalog, GeoJSON parsing, membership → CSR, overlap, `fill=nearest`.
- Create: `crates/cdors-core/src/ops/regstat.rs`: all six statistics through one CSR row reducer on the executor's
  field kernel, with fldstat's `Acc`.
- Modify: `crates/cdors-core/src/ops/mod.rs`, `tests/ref/reference.py`, `tests/cases.txt`.

- [x] **Licence gate (Nikolay decides before anything is embedded):** AR6 (CC-BY 4.0), Natural Earth (public domain),
      MPAS geometric_features / IHO via marineregions (licence to check). Record the outcome in this plan.
- [x] **Export script.**
  - Natural Earth (countries_50/110, land_*, ocean_basins_50 v5.1.2) and the nereus basins and moc sets are exported
    **unsimplified** to a regions directory. cdors finds it via `CDORS_REGIONS` or the install's `share/regions/`.
  - Tests use a fixture-local copy. The shared install is filled only after approval.
- [x] **`regions.rs`.**
  - Set names: `ar6.ocean`, `ar6.land`, `ar6.all`, `srex`, `giorgi`, `prudence`, `nino`, `basins`, `moc`,
    `ne.<set>`, or a GeoJSON path.
  - Membership follows regionmask's rules. Overlapping sets give a cell several rows.
  - `fill=nearest` hands wet cells that lie outside every polygon of an ocean set to the nearest region. The count is
    reported as a stderr JSON warning (`regions_filled`), in `--plan` `warnings[]`, and as an output attribute.
  - Weights are cached per (mesh key, place, selection after pruning, 2-D or per-level, region-data sha, set,
    options, algorithm version).
  - Attribution goes into an output attribute and into `help`.
- [x] **Operators.**
  - `regmask,regions=<set>` writes `mask(region, cell)` as int8, giving membership for overlapping sets too.
  - `regmin`, `regmax`, `regsum`, `regint`, `regmean` and `regstd` mirror `fld*`: sum is unweighted; int, mean and
    std are weighted by area, per level with a mesh.
  - Key: `regions=<set>[,fill=nearest][,select=<abbrev,…>]`.
  - Output dimension `region`, with `region_name` and `region_abbrev`.
  - An empty region gives an all-NaN row plus a warning.
- [x] Write tests:
  - `regmask` vs `ref:regionmask.mask_3D`, cell for cell, on r36x18_std, hpz2, unst_360, `fesom_pi_data` and
    r1x1_edges, for ar6.ocean, ar6.land, srex, prudence (overlap) and ne.ocean_basins_50;
  - `regmean` and `regint` vs `ref:nereus` (basins) and NumPy;
  - a `json:` row asserting the `regions_filled` count;
  - an empty-region row;
  - the planner variant.
- [x] Run tests.
- ➕ **Licence outcome (Nikolay, 2026-10-18): embed as planned.** Embedded (`include_bytes!`, 610 kB in all):
  AR6 (CC-BY 4.0, Iturbide et al. 2020) as one file `ar6.all.geojson` (596 kB; `ar6.land` and `ar6.ocean` are its
  subsets: same numbers and identical polygons in regionmask, three regions in both, so each feature lists its
  subsets), SREX, Giorgi, PRUDENCE (regionmask, MIT; coordinates from the papers) and `nino` (own NOAA CPC box
  definitions: Nino 1+2, 3, 3.4, 4). Regions directory only, unsimplified: Natural Earth v5.1.2 (public domain)
  `ne.countries_50/110`, `ne.land_50/110`, `ne.ocean_basins_50` (8.3 MB together) and nereus' `basins`, `moc`
  (MPAS geometric_features from IHO via marineregions, CC-BY 4.0; 3.9 MB; per-feature `history` dropped, `name`,
  `author`, `constituents` kept). Source, licence and attribution are attributes of the output (`regions_source`,
  `regions_license`, `regions_attribution`) and `cdors help regmask` lists the catalog and the attributions.
- ➕ Done in this task: `tools/export_regions.py` (aqua env; regionmask's pooch cache under
  `/work/ab0995/a270088/cdors-cache/regionmask`; `--no-embedded` for the fixtures), `cdors_mesh::regions`, the
  `regmask` operator, int8 data variables and string variables (`plan::Labels` on a `ZDesc`; NetCDF char arrays,
  Zarr vlen-utf8) in both writers, `OpSpec::origin` (`cdo`/`cdors`; `ops --json` shows it), `Sources::warnings` →
  `--plan` `warnings[]`. The statistics operators followed in a second pass (below).
- ⚠️ **Data model of `regmask`:** `mask(region, <the grid's own dims>)`, so regular grids keep `(region, lat, lon)`.
  `region` is a `ZDesc` (vertical role) with the region numbers as values and the labels `region_name`,
  `region_abbrev`; cdo reads the file as a field with one level per region. A region without cells is an all-zero row
  plus the warning `regions_empty` (regionmask's `drop=False`); `select=` masks with the selected regions only, as
  regionmask's `regions[[...]]`.
- ⚠️ **`fill=nearest` candidates:** on a grid with a mesh, the cells with at least one wet level (FESOM2: every
  node; pi: 1396 nodes outside the AR6 ocean regions, 22 coastal nodes outside nereus' five basins). A grid without a
  mesh has no wet mask: every cell counts, so on a grid with land the land is filled too (r36x18 with ar6.ocean: 314).
- ⚠️ **Membership cache not implemented:** membership is fast without it. HEALPix z10 (12.6M cells, built in memory,
  16 threads on a login node): ar6.ocean 0.09 s, ar6.all 0.09 s, srex 0.05 s, prudence 0.02 s, ne.ocean_basins_50
  0.15 s, basins 0.25 s (36.6M memberships); parsing ≤ 0.02 s. On hpz2 a whole `cdors regmask` run takes
  0.15–0.17 s. `fill_nearest` (one KD-tree over all member cells, queries in parallel, no per-cell allocations) on z10
  with 16 threads: 1.0 s for 207 filled cells (ar6.all; mostly the tree over 12.6M members), 1.0 / 1.4 / 1.5 s for
  3.7M / 5.1M / 6.6M filled cells (ne.ocean_basins_50, ar6.ocean, srex), 3.2 s for prudence (12.4M cells filled from
  207k members in Europe; 8.0 s on 6 threads). Before: 1.9 s, 5 / 23 / 75 s and 384 s.
- ➕ KD-tree (`locate.rs`): the far side of a split was pruned by the split plane only, so a query far from a cluster
  of points visited most of the tree. Now each sub-range is pruned by its distance to the query: the bounding box of
  its points for sub-ranges of more than 32 points (heap-numbered, 48 bytes each: 1.5–3 MB per million points), the
  cell cut out by the splits above (tracked per axis, Arya and Mount) below them. Same answers (the brute-force unit
  test gained a clustered case), tie rule unchanged (bounds compared with a 1e-12 relative slack).
- ⚠️ GeoJSON numbers are parsed with serde_json's `float_roundtrip` (enabled from cdors-mesh, so for the whole
  workspace): coordinates are read correctly rounded. The full harness passes unchanged with it.
- ➕ `fesom_pi_data` rows: `regmask` after `-setgrid,fesom:{fix}/fesom_pi` vs `ref:regionmask.mask_3D:exact` for
  ar6.ocean, ar6.land, srex, prudence and ne.ocean_basins_50 (identical; `reference.py` reads the nodes of
  fesom.mesh.diag.nc with the loader's radians rule), and two `json:` rows with the filled counts (ar6.ocean 1396,
  basins with five basins 22); all with the Zarr and planner variants.
- ➕ **Region statistics** `regmin`, `regmax`, `regsum`, `regint`, `regmean`, `regstd` (`ops/regstat.rs`, origin
  `cdors`, key as `regmask`): a `FieldMap` on the remap executor's `FieldKernel` (complete fields; non-trailing
  horizontal dimensions handled there). Per field and region the member cells are added in ascending order with
  fldstat's accumulators (`MeanAcc`, `SumAcc`, `MinMaxAcc`, `VarAcc`, made `pub(crate)`), so the numerics are those of
  `fld*`. Weights: 1 for sum, min, max; for int, mean, std the `fldint` cell areas (m²; mesh areas on a mesh) or, for a
  variable with the mesh's levels, the place's area at each mesh level (`area_per_level.level(k)`, loaded on first
  use, counted in `--plan` `mesh_weights`, as is the membership table). Output dimension `region`: a one-axis
  generic grid (coordinate `region` = the region numbers) with `region_name`/`region_abbrev` (new
  `FixedGrid::labels`, written by both writers); the set's attributes on every variable; `cell_methods`
  `area: <stat> where region`. cdo reads the outputs (generic grid of N points).
- ⚠️ **Not `RemapWeights::from_csr` for the linear statistics:** all six run through the one row reducer above. The
  membership CSR is scaled by the level's areas at apply time (no matrix per level), the mean renormalises around
  missing values exactly as `fldmean`, and min/max/std need the reducer anyway, so one code path keeps the fold
  order and the missing-value rules identical for all of them.
- ➕ Rows (all against NumPy on regionmask's own masks, `ref:regionmask.reg<stat>`): the six statistics on
  r36x18_std and hpz2_noleap (regmean with ar6.all: empty regions missing), regmean with prudence (overlap) on
  unst_360; on fesom_pi_data regmean/regint per level and 2-D (also on fesom_pi_data_v20), regmax, regstd, with the
  planner variant; regmean of nereus' basins vs `ref:nereus.nereus_regmean` (nereus' shapely masks and
  `surface_mean`, the same within 6e-8); `json:` rows for `regions_filled` (1396), `regions_empty` and the
  per-level `mesh_weights` bytes; `err:bad_arguments` without `regions=`. `regstd` rows use `abs=1e-5`: cdo's
  one-pass variance (as `fldstd`) leaves ~1e-7 instead of 0 for regions of one cell.

### Task 7: `selregion` and `maskregion` (cdo semantics)

**Files:**
- Modify: `crates/cdors-core/src/ops/select.rs`, `crates/cdors-core/src/ops/mod.rs`, `tests/cases.txt`

- [x] **`selregion` and `maskregion`** with cdo ASCII region files: cell centre inside, output grid unstructured for
      `selregion`. `maskregion,regions=<catalog set>` is accepted as a cdors extension (documented).
- [x] **`dcw:`** gives `not_implemented`, with a hint to use `ne.countries_50`.
- [x] Write tests:
  - `selregion` and `maskregion` vs cdo on r36x18_std, hpz2 and unst_360 with a two-polygon file;
  - an `err:not_implemented` row for `dcw:DE`.
- [x] Run tests.
- ➕ Implemented in `crates/cdors-core/src/ops/regions.rs` (not `select.rs`), with `regmask`. cdo's rules come from
  cdo 2.6.5's `Selregion.cc`, `Maskbox.cc` and `region.cc`: each polygon on its own, the crossing test
  `(y >= yi && y < yj) || (y > yj && y <= yi)`, longitudes shifted by 0, -360, +360 into the polygon's range (first
  fit), and the region file format (`#`, `&`, `>`, rings closed). `selregion` on 2-D grids makes one `ncells`
  dimension: each stored dimension follows it through its own index list (`Expr::retarget`), coordinates are a
  `FixedGrid`, `sel` holds the flat stored indices (so `fld*` weights pick the original cell areas).
  Rows: `selregion`/`maskregion` vs cdo with `cdo_one.txt` (date line, vertices on centres) and `maskregion` with
  `cdo_two.txt` on r36x18_std, hpz2_noleap, unst_360 and r1x1_edges; `maskregion,regions=` vs
  `ref:regionmask.maskregion`; empty selection `err:bad_arguments`; `dcw:DE` for both operators.
- ⚠️ **cdo 2.6.0 `selregion` with several polygons repeats cells:** after each polygon it appends all cells
  selected so far, so the first polygon's cells appear twice with two polygons (also in 2.6.5's source). cdors
  writes each cell once, ascending (`docs/deviations.md`); the two-polygon `selregion` row compares with
  `ref:numpy.selregion`, which runs cdo polygon by polygon on a cell-index field and takes the union.
- ⚠️ **cdo 2.6.0 `selregion` drops centres on a polygon's highest latitude** that its `maskregion` and 2.6.5's
  source keep (seen on r1x1_edges with `cdo_one.txt`): cdors uses an exclusive latitude range for `selregion`,
  inclusive for `maskregion` on cell grids, exclusive for `maskregion` on regular grids (as cdo).
- ⚠️ `fldmean` after `selregion` on a regular grid differs from cdo: cdo's new grid has no bounds and gets equal
  weights, cdors keeps the original cell areas (`docs/deviations.md`). No row.


### Task 8: Latitude bands and area-weighted histograms

**Files:**
- Modify: `crates/cdors-core/src/ops/fldstat.rs`: `zonal()` with `zonaldes=zonal_<DY>`.
- Create: `crates/cdors-core/src/ops/fldhist.rs`.
- Modify: `crates/cdors-core/src/ops/mod.rs`, `tests/ref/reference.py`, `tests/cases.txt`.

- [x] **`zonmean`, `zonsum`, `zonint` with `zonaldes=zonal_<DY>`** on unstructured and mesh grids, with cdo semantics
      (band membership and output latitudes as cdo writes them). The other `zon*` keep refusing, with today's hint.
- [x] **`fldhist,bins=<lo>/<hi>/<step>|<e0>/<e1>/…[,out=area|fraction]`.**
  - Bins are [e_i, e_{i+1}), except the last, which is closed.
  - Values outside all bins go to the variables `below` and `above`; nothing is dropped silently.
  - `fraction` divides by the total valid area, including below and above.
  - NaN cells are excluded. With a mesh, the histogram is per level with per-level areas.
  - Output dimension `bin`, with bounds.
- [x] Write tests:
  - `zonmean,zonaldes=zonal_10` vs cdo on unst_360 and hpz2;
  - `fldhist` vs NumPy on `fesom_pi_data`;
  - the ice-extent recipe vs `ref:nereus.ice_extent`;
  - a value exactly on an edge;
  - the planner variant.
- [x] Run tests (grids without a mesh).
- ➕ Done without a mesh (2026-10-18), the mesh parts wait for Task 5:
  - `zonmean,zonal_<DY>` (cdo's syntax: the description is the positional argument;
    `zonaldes=zonal_<DY>` is accepted too) on unstructured/curvilinear grids with bounds, HEALPix
    (cdo's corners) and regular grids: band membership, weights (`correct_weights`) and output
    grid as cdo's `remap_weights_zonal`; the part of a cell inside a band is integrated along its
    edges (`cdors_mesh::geom::band_area`, lon-lat cells `lonlat_band_area`) instead of yac's
    clipping, equal up to rounding (f32 ulp vs cdo on all three fixtures, zonal_7 and zonal_10).
    The cells → bands matrix is a `Csr` (`RowMap::Bands`) folded by fldstat's kernel in cell order.
  - `fldhist` (`ops/fldhist.rs`) with `<var>(…, bin)`, `<var>_below`, `<var>_above`, coordinate
    `bin` (centres) with `bin_bnds` (edges), a one-axis generic grid (writer: `exec/mod.rs`).
  - Mesh hooks: per-level areas go into `fldhist::hist_weights` (the only place deciding the
    weights; `Weights` per (level, cell) as for `fldmean`), and mesh cell polygons into
    `fldstat::cell_corners` (today fixed `nv` per cell; median-dual node polygons have a varying
    count, so `band_matrix` needs a CSR of corners there).
- ⚠️ cdo 2.6.0 (the harness reference) accepts `zonal_<DY>` with `zonmean` only and has no
  `zonint`; `zonsum`/`zonint` follow cdo 2.6.5's source (`Zonstat.cc`, `remap_conserv.cc:768-976`)
  and are checked against NumPy (`ref:numpy.zonsum`, `ref:numpy.zonint` on unst_360). `zonint`
  without bands (2.6.5: rows of regular grids) is refused (`bad_arguments`). cdo 2.6.0 segfaults on
  a HEALPix subset with `zonal_<DY>`, so that row runs on unst_360 only.
- ⚠️ `bins=` is always `<lo>/<hi>/<step>` (three numbers, as cdo's `first/last/inc` ranges);
  explicit edges are `edges=<e0>/<e1>/…/<en>`, because three explicit edges and lo/hi/step cannot
  be told apart.
- ⚠️ `out=area` gives m² with `fldint`'s cell areas and refuses a grid without areas
  (`unsupported_grid`/`no_coordinates`, as cdo's `fldint`); `out=fraction` uses `fldmean`'s weights,
  including the equal-weights fallback with its warning.
- ➕ `fldhist` reads its input once (2026-10-18): `FoldKernel::outputs(var)` names the output
  variables a fold's lanes emit tiles for (default: `var`), so one fold of each input variable
  writes `<var>`, `<var>_below` and `<var>_above`; `plan::build` (written bytes per stage variable)
  and `exec::layout` (lane-aligned output chunks) map outputs to their stage variable through it.
  2 `json:` rows check the decoded bytes. FESOM2 `sst.fesom.1959.nc` (2.1M nodes, 28 steps,
  238 MB): `--plan` bytes decoded 715 MB → 238 MB, warm wall time 1.10 s → 0.47 s (`-P 8`, login
  node; `fldmean` 0.27 s), output identical.
- ➕ Mesh parts (2026-10-18, after Task 5):
  - `fldhist` on a mesh: one histogram per level with `nod_area[k]` (`fldstat::level_weights`,
    normalised per level for fractions; elements `elem_area`), dry levels out; both FESOM layouts
    (the bin takes the place of `nod2`). `ref:numpy.mesh_fldhist`, and a `json:` row on the
    `mesh_weights` bytes in `--plan`.
  - `zon*,zonal_<DY>` on a mesh: nodes have no polygons in round 1, so every cell (node or
    element) is in the band of its centre ([lo, hi)) and weighs its whole (per-level) area:
    `zonmean` Σa·x/Σa, `zonint` Σa·x, `zonsum` Σx. Documented in `docs/deviations.md` ("Latitude
    bands on model meshes") and `help`. `ref:numpy.mesh_zonmean|zonint|zonsum`.
  - Recipes vs nereus on fesom_pi_data and fesom_pi_data_v20 (northern hemisphere, nod_area[0]):
    ice extent `-fldhist,edges=0.15/1 -sellonlatbox,-180,180,0,90 -setgrid,fesom:<dir>
    -selname,a_ice` = `nereus.ice_extent` (rel ≤ 6e-16); ice area `-fldint -sellonlatbox,…` =
    `nereus.ice_area` (rel 5e-8: fldint keeps a_ice's float32). No hemisphere region set exists
    in the catalog, so agents should use `-sellonlatbox,-180,180,0,90` (or `-90,0` for the
    south).
- ➕ `fldhist` has `origin: "cdors"` in the registry (the field came with Task 6).

### Task 9: Vertical geometry: `vert*` with bottoms, `vertint`, `volint`/`volmean`, `intlevel`, `bottomvalue`

**Files:**
- Modify: `crates/cdors-core/src/model/area.rs`: per-cell layer weights from `Mesh.vertical`.
- Modify: `crates/cdors-core/src/ops/fldstat.rs`: vertical statistics.
- Create: `crates/cdors-core/src/ops/vertical.rs`.
- Modify: `crates/cdors-core/src/ops/mod.rs`, `tests/ref/reference.py`, `tests/cases.txt`.

- [x] **`vertmean`, `vertsum`, `vertstd` on a mesh** skip each cell's dry levels. Without a mesh they behave exactly
      as now.
- [x] **`vertint[,z0/z1]`** = Σ value·dz, with layers cut at z0 and z1.
- [x] **`volint[,z0/z1]`** = Σ value·area(level)·dz, and **`volmean`** = volint / volume. This is deliberately not
      `fldmean -vertmean`. Folding order is fixed.
- [x] **`intlevel`** keeps cdo semantics and gives NaN below each cell's bottom. **`bottomvalue`** keeps cdo
      semantics and uses each cell's own bottom level.
- [x] Write tests:
  - `volmean`, `volint,0/700` and the OHC recipe vs `ref:nereus.volume_mean` and `ref:nereus.heat_content` on
    `fesom_pi_data`;
  - an analytic field with a known integral;
  - `vertmean` unchanged vs cdo on r36x18_2lev;
  - `intlevel` vs cdo on a regular grid, and NaN below the bottom on `fesom_pi_data`;
  - `bottomvalue` vs NumPy;
  - a synthetic cavity case (top level > 1);
  - the planner variant.
- [x] Run tests.
- ➕ `FoldKernel::folds(role)` (default: `fold_dim`): a kernel may fold several roles; `volint`
  folds levels and cells in one lane, tiles in C order over both. Its fold keeps one accumulator
  per (timestep, level, grid row), each fed its cells ascending, and adds them level by level at
  the end, so the planner variant is bit-identical. Off a mesh `volint` takes the grid's cell
  areas and the axis bounds (checked by hand on r36x18_2lev with `cdo genlevelbounds`: equal to
  NumPy to float32 precision; no fixture has level bounds).
- ➕ `bottomvalue` is a `vert*` fold whose weights rank the levels (`RankAcc`: the valid value
  of the largest rank): cdo's direction off a mesh (level order, `positive = down`), the mesh
  level on a mesh. `vertint` counts only levels of non-zero thickness in the range (`CutSumAcc`).
- ➕ Fixture `fesom_pi_cav/` + `fesom_pi_cav_data.nc` (tests/make_fesom_fixtures.py): the pi
  mesh with `ulevels_nod2D`/`ulevels` = 4 at the 354 nodes (549 elements) south of 65°S that
  are at least 10 interfaces deep; generated into this task's target fixtures (the shared
  fixtures directory needs `make_fixtures.sh` once more).
- ⚠️ nereus selects whole layers by mid-depth, so the nereus rows use 0/680 (an interface);
  `volint,0/700` (a cut layer) and the OHC recipe at 700 m are checked against NumPy. nereus is
  given the mesh's own `nod_area`, so its R = 6371 km does not enter.
- ⚠️ An analytic constant needs `-addc,1 -mulc,1e-30`: cdo's `mulc,0` of a missing value is 0
  (MULM), so `-mulc,0` would erase the dry levels.

### Task 10: Sampling of scalar fields: `selpoints` and `transect`

**Files:**
- Create: `crates/cdors-mesh/src/sample.rs`: great-circle paths, step, and weights. `nn` uses the KD-tree; `linear`
  uses barycentric weights on mesh triangles where the mesh has them, otherwise a local Delaunay.
- Create: `crates/cdors-core/src/ops/sample.rs`.
- Modify: `crates/cdors-core/src/ops/mod.rs`, `tests/ref/reference.py`, `tests/cases.txt`.

- [x] **`selpoints,file=<csv: name,lon,lat>|lon=…_lat=…[,method=nn|linear]`** gives dimension `station` with
      `station_name`.
- [x] **`transect,lon0,lat0,lon1,lat1[,…][,step=<km>][,method=nn|linear]`** or `file=<geojson LineString>`, for
      scalar fields.
  - The default step is the median cell spacing along the path.
  - It samples model levels; there is no vertical interpolation.
  - It gives NaN below the bottom and outputs `bottom_depth(distance)`, `lon(distance)` and `lat(distance)`.
  - It takes one path per call.
- [x] Write tests:
  - a linear analytic field is reproduced exactly by `method=linear`;
  - `transect` on `fesom_pi_data` vs `ref:nereus.transect` for `nn`;
  - `selpoints` vs `remapnn,lon=…_lat=…` on regular grids;
  - a NaN-below-bottom row;
  - a path along a meridian through the pole;
  - the planner variant.
- [x] Run tests.
- ➕ Library half done (Task 10 agent): `cdors-mesh/src/sample.rs` (stations from CSV or
  `lon=…_lat=…`, waypoints and GeoJSON LineStrings, equal-arc sampling per segment, the default
  step, `nn`/`linear` weights as `Csr`, wet levels and `bottom_depth` of sample points),
  `KdTree::nearest_k_xyz`, unit tests (`sample.rs`, `cdors-core/tests/sample_mesh.rs` on pi and
  hpz2), and the references `transect`, `selpoints`, `selpoints_cdo` in `tests/ref/reference.py`
  (checked by hand against the library: identical rows, steps and bottom depths).
- ➕ Operators (`ops/sample.rs`): weights computed while describing, one `from_csr` matrix per
  input place with `MissingRule::AllOrNothing` (a value is missing if any cell of its point is
  missing or dry there; the point's wet-level intersection masks the output as well), pruned to
  the cells the weights read, `mesh_weights` in `--plan`. Warnings `points_outside` (stderr JSON,
  `--plan` `warnings[]`, attribute) and `linear_as_nn`: a mesh place without triangles of its own
  (FESOM2 elements, a subset of the nodes) is sampled by `nn` (decision 2026-10-18; Delaunay stays
  for built-in grids). Layout: `station` with `lon`, `lat`, `station_name`; `distance(distance)`
  (km) with `lon`, `lat`, `bottom_depth` (the mesh's first place) and `bottom_depth_<dim>`
  (others), written through `FixedGrid::aux`/`labels` (new: variables along an operator's grid);
  attributes `sampling`, `sampling_method`, `transect_path`, `transect_step_km`,
  `transect_step_source`, `points_outside`. Fixtures `samples/stations.csv` and
  `samples/drake.geojson` (`make_fixtures.sh`). 17 rows (46 cases with their fixtures) in `cases.txt`.
- ⚠️ "A linear analytic field is reproduced exactly": on a sphere no weights of three cells that
  sum to 1 reproduce a field linear in x, y, z at the point itself. `linear` is barycentric in
  the plane of the triangle (as FESOM2's P1 elements) and exact there, at the point's central
  projection; at the point the error is O(h²) relative. The tests check that (1e-12).
- ⚠️ `linear` outside every triangle (land, beyond a regional grid) gives a missing value,
  counted for a warning; `nn` always has a cell.
- ⚠️ nereus' `transect` samples otherwise: `n_points` (default 100) on one segment by normalised
  chord interpolation (not equal arcs), a flat-earth distance, no dry-level masking, and it
  returns only a figure. `ref:nereus.transect` follows cdors' sampling with nereus' nearest-node
  algorithm (scipy cKDTree on unit vectors). By hand against `nereus.transect` itself on pi:
  Drake at 24 and 116 points identical (points ≤ 0.6 km apart); the pole path, 46 points, 2
  columns differ (points ≤ 4.4 km apart).
- ➕ Output order: the sample dimension replaces the horizontal one in place (the input's order,
  as Task 5's executor keeps it). cdo reads `(time, nz1, distance)` and `(time, distance, nz1)`
  alike, but only with the level coordinate variable (`nz1(nz1)`, axis Z) in the file: without it
  cdo 2.6.0 skips the first levelled variable ("Unsupported array structure").

### Task 11: Agent surface: `ops --json` origin, guide recipes, warnings, tables

**Files:**
- Modify: `crates/cdors-core/src/ops/mod.rs` (`OpSpec.origin`), `crates/cdors/src/ops_cmd.rs`
- Modify: `crates/cdors-core/src/ops/output.rs` (region, bin and station tables in `outputtab`)
- Modify: `docs/agent-guide.md`

- [x] **`ops --json` shows `"origin": "cdo" | "cdors"`.** `help <op>` for cdors operators prints their own help text.
- [x] **`outputtab`** gains keys for the new dimensions (`region`, `bin`, `station`, `distance`); the meaning of the
      existing keys does not change.
- [x] **Recipes in `cdors guide`** (≤ 1.5 kB added):
  - OHC: `-mulc,4.09e6 -volint,0/700 -setgrid,fesom:<dir>`;
  - ice area: `-regint`, or `fldint` with a hemisphere region;
  - ice extent via `fldhist`;
  - ice volume;
  - an Atlantic basin series;
  - a Drake transect of temperature.
- [x] Write tests: `json:` rows for `ops`, `help regmean` and each new warning.
- [x] Run tests.
- ➕ **Origin in one place:** `ops::build_registry` sets `origin = "cdors"` for every name that is not in cdo
  2.6.5's catalogue (`catalog::cdo_operator`, aliases included), `"cdo"` otherwise; the scattered settings
  (vertint, `vertical::operators`' third field, the regmask/reg*/selpoints/transect/fldhist list) are gone.
  This fixed `tonodes` and `toelems` (were `cdo`). `ops --json` gains `extensions` for cdo operators with cdors
  additions (`setgrid,fesom:`, `maskregion,regions=`, `zon*,zonal_<DY>` on meshes, `fld*`, `vert*`, `intlevel`,
  `bottomvalue` on meshes; `ops_cmd::extensions`) and `synopsis` for the cdors operators.
- ➕ **Help texts** of the 15 cdors operators in `crates/cdors/src/cdors_help.txt` (`%%% <op> [<op>...]`, cdo's
  NAME/SYNOPSIS/PARAMETERS/OUTPUT/WARNINGS/EXAMPLE layout, ≤ 25 lines each, one text for reg* and for
  volint/volmean and tonodes/toelems); `help <op>` prints it instead of the one-line description, `--json help`
  adds `origin`, `synopsis`, `help`, `extensions`. A unit test (`ops_cmd::tests`) checks that every cdors operator
  has one. Every EXAMPLE was run on the pi fixtures. The long "a cdors operator: …" notes of reg*, regmask,
  selpoints and transect moved into these texts (the region-set list stays in the notes).
- ➕ **`outputtab` keys** `region`, `region_name`, `region_abbrev`, `bin_lo`, `bin_hi`, `station` (1-based),
  `station_name`, `distance`; `bin` is the bin centre where a variable has a dimension `bin` (fldhist), else
  cdo's meaning (the level; text row vs cdo). A key is looked up on the variable's horizontal dimension (reg*,
  fldhist, selpoints, transect) or its vertical one (`regmask`); variables without it print `-` (JSON null), a key
  no variable has is `bad_arguments`.
- ⚠️ Names (`region_name`, `region_abbrev`, `station_name`) come from the chain that makes them; cdors reads no
  string variables from stored files, so on a stored file they fail with `no_coordinates` and a hint (numbers,
  bins and distances are read from the file's coordinates).
- ➕ Guide: a "Model meshes (FESOM2)" section (1.43 kB added) with the six recipes and the mesh/dry-level/units
  lines, and the new outputtab keys. Each recipe ran on `fesom_pi_data` + `fesom_pi` (the Atlantic one with `a_ice`
  in place of `sst`, which the fixture lacks).
- ➕ Warning rows: `regions_empty` and `points_outside` in `--plan` `warnings[]`, `linear_as_nn` on stderr (the
  other combinations had rows); `mesh_cache_rebuilt` is covered by `crates/cdors/tests/mesh_cache.rs`.
- ➕ **Units of the cdors integrals (decision 2026-10-19):** `vertint`, `volint`, `regint` write float64 with the
  units `<input units> m`, `m3`, `m2` (just that without units; `fldstat::integral_output`, the one helper);
  `volmean`, `regmean` keep the input's units and type; cdo-named operators keep cdo's behaviour (cdo 2.6.0
  `fldint` on r36x18_std: float32, units unchanged, checked with a `units=K` copy). `-mulc` keeps the units, so
  the OHC recipe reads "C m3" for a result in J (one line in the guide). Help texts, `docs/deviations.md` and 8
  `json:` rows (units and dtype through `outputtab --json` `variables[]`) updated; guide addition now 1482 bytes.

### Task 12: Verify acceptance criteria (local)

- [ ] Every requirement in the Overview is implemented; deviations are in `docs/deviations.md`.
- [ ] Full suite: `cargo test` and `tests/run_cases.sh` (both NetCDF read paths, all variants).
- [x] `bench/realdata_check.sh` is extended with the round-1 operators on the gate's dataset (login-node subset).
- ➕ 10 `dart_*` cases in `bench/realdata/run.py` (new mode `check`: a NumPy/nereus/regionmask check function gets the
  cdors output). The cases are mesh info, fldmean per level, regmask vs nereus' Atlantic mask and vs regionmask
  ar6.ocean, regmean, volint, fldhist extent, fldint ice area, a Drake transect and selpoints. All are identical or
  within 3.6e-8 (float32 outputs). Table in `docs/unstructured-results.md`.
- ⚠️ The login runs used cdors' default threads (≤ 16 outside Slurm), not 8; each case took ≤ 1.7 s.

### Task 13: Real-data validation and speed (compute run, needs Nikolay's yes)

**Files:**
- Modify: `bench/unstructured/baseline.sh`
- Create: `docs/unstructured-results.md`

- [x] Same diagnostics as the gate. cdors and nereus agree within `rel=1e-5` (float32 inputs, different summation
      order). Report wall time and peak RSS, and check that the `--mem` bound holds.
- [x] The pass marks from Task 1 are met, or reported as missed with the cause.
- [x] Node run ≤ 0.25 node-h (`--hint=nomultithread --mem=0`), with a background `sacct` watcher.
- ➕ `bench/unstructured/validate.sh` (new, instead of extending `baseline.sh`), `baseline.py --area nod_area` (the
  nereus reference with mesh.diag's per-level areas and nodes), `bench/unstructured/compare.py`. Jobs 28230544 and
  28230657: 8 min 24 s, 0.14 node-h. Results: `docs/unstructured-results.md`.
- ⚠️ **Values pass:** OHC agrees with nereus to 2.9e-8 (the float32 output) and the extent to 3e-14. Ice passes every
  speed mark: 7.2 s cold, 17.7× nereus. First use of the mesh takes 19.7 s; the cached mesh adds 2.5 s.
- ⚠️ **OHC misses rows 1, 3 and 5:**
  - 15.4 s cold and 13.9 s warm; peak RSS 3.0–3.3 GB, and 2.24 GB under `--mem 2G`, 12 % over the budget.
  - Cause: `volint` folds one lane per output step, so 36 lanes keep only 10–15 cores busy. `fldmean` with the same
    per-level areas takes 9.7 s on ~90 cores.
  - The `--mem` estimate leaves out the mesh's 0.26 GB of resident arrays.
- ➕ Follow-ups, not done here:
  - fold `volint` per (step, level) and sum the levels in fixed order;
  - make the region mask a CSR row instead of a whole-mesh leaf per tile;
  - count the mesh's resident bytes in the `--mem` estimate;
  - push `volint,z0/z1` down as a level selection. Today `-volint,0/700` without `-sellevel` reads all 79 levels
    (2.2× the bytes), so the guide's OHC recipe must keep `-sellevel`.

### Task 14: Agent check on FESOM2 (needs its own yes, ~$3–4)

**Files:**
- Modify: `bench/agent_check.sh`, `bench/agent/` tasks
- Modify: `docs/agent-check.md`

- [ ] Five tasks, two arms. Tasks: Atlantic OHC 0–700 m series, NH ice seasonal cycle, Drake T section, NINO3.4
      index, Arctic mean SST. Arms: cdors (`cdors guide` only) and Python + nereus.
- [ ] Score correctness against the Task 13 references. Report wall time, tokens and USD for each arm.

### Task 15: [Final] Criteria and documentation

- [ ] **`docs/criteria-unstructured.md`** with four criteria:
  1. correctness: regionmask masks identical, nereus/pyfesom2 within tolerance, no silent cell loss;
  2. speed against the gate's marks;
  3. agents 5/5, at a cost no higher than the Python arm;
  4. memory bounded under `--mem`.
- [ ] Update the README (operator list and a mesh section), `docs/levante.md` (FESOM2 examples; the shared install
      and guide page only with approved text), `docs/deviations.md` and `docs/STATUS.md`.
- [ ] Move this plan to `docs/plans/completed/`.

## Technical Details

**Mesh vocabulary** (`cdors-mesh::mesh`).
- **`Mesh`** has:
  - a list of `Place`s;
  - lazily loaded `topology`: named CSR incidence tables (`elem → nodes`, `edge → nodes`, `edge → elems`);
  - `vertical`;
  - `source`: what was read, and how units were decided.
- **`Place`** has `name`, `size`, `lon`/`lat` in degrees and a 2-D `area`. It optionally has `area_per_level`
  (levels × size, lazy) and `polygons` (lazy).
- **`vertical`** holds `interfaces` (zbar) and `mid` (Z), and for each place the top and bottom wet indices. Thickness
  is static full layers in round 1.

The enum variants for variable thickness (FESOM zstar `hnode`) and computed thickness (IFS Δp) are added when a
round implements them; they are not stubbed now.

**Rules against a dump.**
- Model code lives only in `cdors-mesh/src/models/<name>/`, and no model name appears outside it.
- There is one registry table, in `models/mod.rs`.
- Models do not depend on each other.
- An operator that seems model-specific is first expressed through the vocabulary.
- Adding a model = a directory plus one table line.

**Weights.**
- Generators return `Csr { row_ptr, col: u32, w: f64 }` plus a public `MissingRule`: `Renormalise` for means, `Drop`
  for sums and integrals.
- `RemapWeights::from_csr` wraps them, so `apply_batch`, `prune` and the lanes are reused.
- Per-level areas scale the membership weights at apply time.
- Non-linear reductions (min, max, std, histograms) walk the same CSR rows with fldstat's accumulators.
- The cache key is listed in Task 6. Files go to `$CDORS_CACHE/weights/gen-<key>.bin`.

**Output conventions.**
- New dimensions `region`, `bin`, `station` and `distance`, with CF-style coordinate variables and string variables
  for names.
- Attributes record the generator, its options and the data attribution.
- `history` and `cell_methods` as today.

**Region data.**
- Small sets are embedded with `include_bytes!` (well under 1 MB).
- Natural Earth and the nereus basins are stored unsimplified in a regions directory (`CDORS_REGIONS` or
  `share/regions/` of the install).
- ⚠️ Change from the brainstorm: there is no `cdors regions fetch` downloader in round 1. The export script fills the
  directory once, so compute nodes need no network access.

**Tolerances.**
- `rel=1e-6` by default against NumPy-based references on fixtures.
- `rel=1e-5` on real data in Task 13.
- `exact` where both sides compute the same thing in the same order (`regmask`, `selregion`, unchanged cdo rows).

**Deviations to record** (`docs/deviations.md`):
- with a mesh, field statistics use per-level areas;
- `maskregion,regions=<catalog set>` is an extension;
- `dcw:` is not implemented;
- full layers only (no partial cells).

**Cost (to be confirmed before the yes).**
- **Compute:** Task 1 ≤ 0.1 node-h; Task 13 ≤ 0.25 node-h; Task 14 ≈ $3–4.
- **Implementation:** Tasks 2–11 are 10 code tasks, at most 4 agents in parallel. The prototype's 16 tasks took one
  night.
- **Tokens:** no token figures were recorded for the prototype. Tasks 2–4 therefore run first, and their measured
  token use is extrapolated and reported to Nikolay before Tasks 5–11 start.

## Post-Completion

*Informational, no checkboxes.*

**Moved out of round 1 by the plan review** (YAGNI; they return with their first consumer):
- the FESOM2 ASCII mesh fallback (own areas and median-dual polygons);
- variable layer thickness (`hnode`, `thickness=`) and the partial-cell option;
- transect `vectors=`, the vector-basis rules (`autorotate_back_to_geo`, Euler angles) and the named-section catalog
  (CMIP6 `mfo` straits) → R3, together with transports;
- `cdors mesh export` and the `ugrid:` mesh spec. The cache entry is already UGRID, and `mesh info` prints its path.

**Later rounds** (brainstorm decisions, memory `cdors-nereus-brainstorm`).
- **R2: neighbour operators and the first plots.**
  - `smooth` with cdo semantics;
  - `grad`/`div`/`curl`/`laplace`: generic (Green-Gauss on polygons, least squares as fallback) and FESOM2-native
    (P1 via `gradient_sca_x/y`, model radius). A missing stencil neighbour gives NaN, counted in the output. The
    native gradient lives on elements, and `at=nodes` maps it back;
  - `components,<cond>`, which outputs a table of objects;
  - a `cdors-render` crate with `plotmap` and `plotline` first, then `plotsection` and `plothov`.
- **R3: FESOM2-native transports, MOC and vectors.**
  - transport through sections by Eq. 1 of Sidorenko et al. 2020 (GMD 13:3337), with `tracer=`; paths along mesh
    edges, with a deterministic tie-break;
  - MOC method A as in the paper, then method B and density MOC (checked against `ldiag_dMOC`);
  - the barotropic streamfunction by least squares over the dual-face network;
  - tests against the paper's Zenodo data (doi:10.5281/zenodo.3552456) and a closed-contour check.
- **Models:**
  - IFS: hybrid levels (Δp from ps; source in `~/IFS`), polygons and neighbours by formula;
  - FESOM3: read its code first;
  - then NEMO (ORCA basis, `e3t`), ICON, MITgcm and MPAS.
- **Python module (PyO3), after R2:** `cdors-mesh` and `cdors-render` only. nereus keeps its API and uses matplotlib
  for decoration.

**Decisions for Nikolay:**
- licences, at the gate in Task 6, plus code read from FESOM2 (GPL-3) and NEMO (CeCILL);
- whether `dcw:` should later map to Natural Earth countries (a different database from GMT's DCW).

**External.** Updates to the shared install (`/work/ab0995/a270088/cdors`), the guide page and any push need approval
of the exact text.
