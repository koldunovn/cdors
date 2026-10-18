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
    - ⚠️ `trailing_hdims()` (`:254`) requires the horizontal dimensions last, but FESOM writes `temp(time, nod2, nz1)`.
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
    - `temp(time, nod2, nz1)`, analytic in lon, lat and z;
    - `salt(time, nod2, nz1)`;
    - `a_ice(time, nod2)` and `m_ice(time, nod2)`;
    - `u(time, elem, nz1)` and `v(time, elem, nz1)`;
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

- [ ] **Find the dataset:** one native-mesh FESOM2 dataset with monthly 3D `temp` and `a_ice`, on dart or ng5, under
      `/work/bm1344/AWI/` (`Cycle3/IFS_28-FESOM_25-cycle3`, `IFS_9-FESOM_5-cycle3`, `EERIE/…`).
- [ ] **Describe it** in `docs/unstructured-baseline.md`:
  - path, mesh name, years, size;
  - chunking and **dimension order** (`nod2` before or after `nz1`);
  - **how dry levels are stored** (0, `_FillValue` or NaN);
  - whether `autorotate_back_to_geo` is present.
- [ ] baseline.py: Atlantic OHC 0–700 m monthly series (`nr.heat_content` with the nereus Atlantic basin mask) and NH
      ice extent (`nr.ice_extent_nh`) over N years. Record wall time, peak RSS and dask settings.
- [ ] cdors read probe on the same files (`-fldmean -sellevel,…` and `-fldmean -selname,a_ice`). This is the lower
      bound for round 1, since reading dominates.
- [ ] Run the login-node subset first. The node run (`srun -c 128 --hint=nomultithread --mem=0`, ≤ 0.1 node-h) happens
      only after Nikolay's yes, with a background `sacct` watcher.
- [ ] Write the gate: speed pass marks for Task 13. If nereus is already within ~2× of the read lower bound, STOP and
      report before Task 2.
- [ ] tests: none (measurement task); the scripts run end to end on the subset.

### Task 2: Harness: `ref:` references, `json:` assertions, FESOM planner variant, fixtures

**Files:**
- Modify: `tests/run_cases.sh`
- Create: `tests/ref/reference.py`
- Create: `tests/make_fesom_fixtures.py`
- Modify: `tests/make_fixtures.sh`
- Modify: `tests/cases.txt`

- [ ] `ref:<lib>.<fn>[:abs=…][:rel=…]` tag as specified in Testing Strategy. Cache in `$CDORS_REF/ref-<key>`. A
      missing env or package FAILs.
- [ ] `json:<jq-expr>` tag over `{"out", "stderr"}` (Testing Strategy).
- [ ] `VARIANT_FIXTURE` becomes a list (`hpz2_noleap fesom_pi_data`), and `fesom_pi_data_tiny.zarr2` is added with
      chunks that split `nod2` and `nz1`.
- [ ] `make_fesom_fixtures.py` (nereus env) writes the fixtures listed in Testing Strategy. `make_fixtures.sh` calls
      it, and the fixture list is documented in its header.
- [ ] Write tests: self-test rows.
  - A `ref:numpy.fldmean` row equals cdo on r36x18_std, and a deliberately wrong reference fails.
  - A `json:` row that is true passes, and one that is false fails.
  - A missing env fails.
- [ ] Run tests: the full harness passes, and no existing row changes.

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

- [ ] Vocabulary types. Components that grow with the number of cells (per-level areas, topology) load lazily through
      the reader, and their bytes count against `--mem`. A grid without a mesh behaves exactly as today.
- [ ] Built-in grids (regular, Gaussian, HEALPix, unstructured with areas) give a one-place `Mesh` view: centres and
      areas.
- [ ] `geom.rs` primitives and the KD-tree locator.
- [ ] Write tests (unit):
  - polygon area against today's `huiliers_area` results;
  - point-in-polygon cases: on an edge, at −180/180, at −90, inside a hole, on a long AR6 edge at high latitude
    (planar, not great circle).
- [ ] Run tests: `cargo test` and the full harness.

### Task 4: Mesh cache and `cdors mesh info`

**Files:**
- Create: `crates/cdors-mesh/src/ugrid.rs`: mesh to UGRID 1.0 arrays and back. Vertical geometry and per-level areas go
  in as `cdors_*` attributes.
- Create: `crates/cdors-core/src/io/mesh_reader.rs`: `ArrayReader` for NetCDF and Zarr.
- Create: `crates/cdors-core/src/model/mesh_cache.rs`.
- Create: `crates/cdors/src/mesh_cmd.rs`.
- Modify: `crates/cdors/src/main.rs`.

- [ ] **Cache entry** = UGRID-Zarr at `$CDORS_CACHE/meshes/<key>.zarr`.
  - The key comes from the realpath, size and mtime of each source file plus a loader-version salt; contents are not
    hashed on every run.
  - The entry stores a content fingerprint, the sha of the first and last MiB of each source file, which is checked
    on load.
- [ ] **Concurrency.**
  - Build into `<key>.zarr.tmp-<pid>-<rand>`, write a completion marker, then rename; the first rename wins.
  - A loser removes only its own uniquely named temp directory.
  - An entry without its marker, or with the wrong fingerprint, is renamed aside to `<key>.zarr.bad-<time>` and
    rebuilt. It is never deleted.
- [ ] `cdors mesh info <spec> [--json]` reports:
  - places, sizes and the area sum;
  - the vertical: levels, plus min and max wet levels;
  - topology tables;
  - how units were decided;
  - source files and the cache path.

  The cache entry is itself a UGRID store, and its path is printed.
- [ ] Write tests:
  - UGRID round trip (unit);
  - `json:` rows for `mesh info` on hpz2 and r36x18;
  - two concurrent `mesh info` runs on a fresh cache both succeed and leave one entry.
- [ ] Run tests.

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

- [ ] **Loader from `fesom.mesh.diag.nc`:**
  - **Units:** radians if both |lat| ≤ π/2 + ε and |lon| ≤ 2π + ε, otherwise degrees. An override is available
    (`fesom:<dir>,units=deg|rad`), and the decision is shown in `mesh info`.
  - **Names:** both `nod_n` and `nod2` are accepted.
  - **Top levels:** `ulevels_nod2D` is optional; without it the top level is 1.
  - **Areas:** per-level `nod_area`.
  - **Layers:** full layers (FESOM2 default `use_partial_cell=.false.`).
- [ ] **Rules:**
  - dimensions: `nod2`/`nod_n` → nodes, `elem`/`elem_n` → elements, `nz` → interfaces, `nz1` → mid-levels;
  - levels below a cell's bottom (or above its top) are missing, whatever value the file stores (0 in FESOM output).
- [ ] **`-setgrid,fesom:<dir>`.** Sizes are checked against the data; a mismatch is error `grid_mismatch`, and its
      hint names both sizes and the mesh. Area-weighted operators (`fldmean`, `fldint`, `fldstd`, …) then use the mesh
      areas, at level k `nod_area[k]`, through `Weights::PerLevel`. A deviation from cdo's 2-D area goes into
      `docs/deviations.md`.
- [ ] **Executor for FESOM's `(time, nod2, nz1)` order.** `FieldKernel` lanes accept horizontal dimensions that are
      not last: they gather to `(…, nz1, nod2)` on input and scatter back on output. The output keeps the input's
      dimension order.
- [ ] **`RemapWeights::from_csr`** takes a public rule: `Renormalise` for means, `Drop` for sums and integrals.
      Per-level weights are one membership CSR, scaled by `area[k][col]` at apply time; there is no matrix per level.
- [ ] **`tonodes`/`toelems`** from `elem`:
  - toelems: the mean of the three nodes;
  - tonodes: the element-area-weighted mean;
  - NaN as in remap.
- [ ] Write tests:
  - `fldmean`/`fldint` per level on `fesom_pi_data` vs `ref:nereus.surface_mean` (and a NumPy per-level reference);
  - the degrees and radians meshes give identical `mesh info` (`json:`);
  - an `err:grid_mismatch` row;
  - dry levels stored as 0 do not enter `fldmin`/`fldstd`;
  - `tonodes`/`toelems` vs NumPy;
  - all rows on `fesom_pi_data`, so the planner variant runs;
  - existing remap rows unchanged.
- [ ] Run tests.

### Task 6: Region catalog, `regmask` and region statistics

**Files:**
- Create: `tools/export_regions.py` (aqua env). It turns regionmask sets into GeoJSON with names, abbreviations, the
  overlap flag, the source and the licence.
- Create: `crates/cdors-mesh/data/regions/*.geojson`: small embedded sets (ar6.*, srex, giorgi, prudence, nino).
- Create: `crates/cdors-mesh/src/regions.rs`: catalog, GeoJSON parsing, membership → CSR, overlap, `fill=nearest`.
- Create: `crates/cdors-core/src/ops/regstat.rs`: linear operators via the executor; min, max and std via a CSR row
  reducer that reuses fldstat's `Acc`.
- Modify: `crates/cdors-core/src/ops/mod.rs`, `tests/ref/reference.py`, `tests/cases.txt`.

- [ ] **Licence gate (Nikolay decides before anything is embedded):** AR6 (CC-BY 4.0), Natural Earth (public domain),
      MPAS geometric_features / IHO via marineregions (licence to check). Record the outcome in this plan.
- [ ] **Export script.**
  - Natural Earth (countries_50/110, land_*, ocean_basins_50 v5.1.2) and the nereus basins and moc sets are exported
    **unsimplified** to a regions directory. cdors finds it via `CDORS_REGIONS` or the install's `share/regions/`.
  - Tests use a fixture-local copy. The shared install is filled only after approval.
- [ ] **`regions.rs`.**
  - Set names: `ar6.ocean`, `ar6.land`, `ar6.all`, `srex`, `giorgi`, `prudence`, `nino`, `basins`, `moc`,
    `ne.<set>`, or a GeoJSON path.
  - Membership follows regionmask's rules. Overlapping sets give a cell several rows.
  - `fill=nearest` hands wet cells that lie outside every polygon of an ocean set to the nearest region. The count is
    reported as a stderr JSON warning (`regions_filled`), in `--plan` `warnings[]`, and as an output attribute.
  - Weights are cached per (mesh key, place, selection after pruning, 2-D or per-level, region-data sha, set,
    options, algorithm version).
  - Attribution goes into an output attribute and into `help`.
- [ ] **Operators.**
  - `regmask,regions=<set>` writes `mask(region, cell)` as int8, giving membership for overlapping sets too.
  - `regmin`, `regmax`, `regsum`, `regint`, `regmean` and `regstd` mirror `fld*`: sum is unweighted; int, mean and
    std are weighted by area, per level with a mesh.
  - Key: `regions=<set>[,fill=nearest][,select=<abbrev,…>]`.
  - Output dimension `region`, with `region_name` and `region_abbrev`.
  - An empty region gives an all-NaN row plus a warning.
- [ ] Write tests:
  - `regmask` vs `ref:regionmask.mask_3D`, cell for cell, on r36x18_std, hpz2, unst_360, `fesom_pi_data` and
    r1x1_edges, for ar6.ocean, ar6.land, srex, prudence (overlap) and ne.ocean_basins_50;
  - `regmean` and `regint` vs `ref:nereus` (basins) and NumPy;
  - a `json:` row asserting the `regions_filled` count;
  - an empty-region row;
  - the planner variant.
- [ ] Run tests.

### Task 7: `selregion` and `maskregion` (cdo semantics)

**Files:**
- Modify: `crates/cdors-core/src/ops/select.rs`, `crates/cdors-core/src/ops/mod.rs`, `tests/cases.txt`

- [ ] **`selregion` and `maskregion`** with cdo ASCII region files: cell centre inside, output grid unstructured for
      `selregion`. `maskregion,regions=<catalog set>` is accepted as a cdors extension (documented).
- [ ] **`dcw:`** gives `not_implemented`, with a hint to use `ne.countries_50`.
- [ ] Write tests:
  - `selregion` and `maskregion` vs cdo on r36x18_std, hpz2 and unst_360 with a two-polygon file;
  - an `err:not_implemented` row for `dcw:DE`.
- [ ] Run tests.

### Task 8: Latitude bands and area-weighted histograms

**Files:**
- Modify: `crates/cdors-core/src/ops/fldstat.rs`: `zonal()` with `zonaldes=zonal_<DY>`.
- Create: `crates/cdors-core/src/ops/fldhist.rs`.
- Modify: `crates/cdors-core/src/ops/mod.rs`, `tests/ref/reference.py`, `tests/cases.txt`.

- [ ] **`zonmean`, `zonsum`, `zonint` with `zonaldes=zonal_<DY>`** on unstructured and mesh grids, with cdo semantics
      (band membership and output latitudes as cdo writes them). The other `zon*` keep refusing, with today's hint.
- [ ] **`fldhist,bins=<lo>/<hi>/<step>|<e0>/<e1>/…[,out=area|fraction]`.**
  - Bins are [e_i, e_{i+1}), except the last, which is closed.
  - Values outside all bins go to the variables `below` and `above`; nothing is dropped silently.
  - `fraction` divides by the total valid area, including below and above.
  - NaN cells are excluded. With a mesh, the histogram is per level with per-level areas.
  - Output dimension `bin`, with bounds.
- [ ] Write tests:
  - `zonmean,zonaldes=zonal_10` vs cdo on unst_360 and hpz2;
  - `fldhist` vs NumPy on `fesom_pi_data`;
  - the ice-extent recipe vs `ref:nereus.ice_extent`;
  - a value exactly on an edge;
  - the planner variant.
- [ ] Run tests.

### Task 9: Vertical geometry: `vert*` with bottoms, `vertint`, `volint`/`volmean`, `intlevel`, `bottomvalue`

**Files:**
- Modify: `crates/cdors-core/src/model/area.rs`: per-cell layer weights from `Mesh.vertical`.
- Modify: `crates/cdors-core/src/ops/fldstat.rs`: vertical statistics.
- Create: `crates/cdors-core/src/ops/vertical.rs`.
- Modify: `crates/cdors-core/src/ops/mod.rs`, `tests/ref/reference.py`, `tests/cases.txt`.

- [ ] **`vertmean`, `vertsum`, `vertstd` on a mesh** skip each cell's dry levels. Without a mesh they behave exactly
      as now.
- [ ] **`vertint[,z0/z1]`** = Σ value·dz, with layers cut at z0 and z1.
- [ ] **`volint[,z0/z1]`** = Σ value·area(level)·dz, and **`volmean`** = volint / volume. This is deliberately not
      `fldmean -vertmean`. Folding order is fixed.
- [ ] **`intlevel`** keeps cdo semantics and gives NaN below each cell's bottom. **`bottomvalue`** keeps cdo
      semantics and uses each cell's own bottom level.
- [ ] Write tests:
  - `volmean`, `volint,0/700` and the OHC recipe vs `ref:nereus.volume_mean` and `ref:nereus.heat_content` on
    `fesom_pi_data`;
  - an analytic field with a known integral;
  - `vertmean` unchanged vs cdo on r36x18_2lev;
  - `intlevel` vs cdo on a regular grid, and NaN below the bottom on `fesom_pi_data`;
  - `bottomvalue` vs NumPy;
  - a synthetic cavity case (top level > 1);
  - the planner variant.
- [ ] Run tests.

### Task 10: Sampling of scalar fields: `selpoints` and `transect`

**Files:**
- Create: `crates/cdors-mesh/src/sample.rs`: great-circle paths, step, and weights. `nn` uses the KD-tree; `linear`
  uses barycentric weights on mesh triangles where the mesh has them, otherwise a local Delaunay.
- Create: `crates/cdors-core/src/ops/sample.rs`.
- Modify: `crates/cdors-core/src/ops/mod.rs`, `tests/ref/reference.py`, `tests/cases.txt`.

- [ ] **`selpoints,file=<csv: name,lon,lat>|lon=…_lat=…[,method=nn|linear]`** gives dimension `station` with
      `station_name`.
- [ ] **`transect,lon0,lat0,lon1,lat1[,…][,step=<km>][,method=nn|linear]`** or `file=<geojson LineString>`, for
      scalar fields.
  - The default step is the median cell spacing along the path.
  - It samples model levels; there is no vertical interpolation.
  - It gives NaN below the bottom and outputs `bottom_depth(distance)`, `lon(distance)` and `lat(distance)`.
  - It takes one path per call.
- [ ] Write tests:
  - a linear analytic field is reproduced exactly by `method=linear`;
  - `transect` on `fesom_pi_data` vs `ref:nereus.transect` for `nn`;
  - `selpoints` vs `remapnn,lon=…_lat=…` on regular grids;
  - a NaN-below-bottom row;
  - a path along a meridian through the pole;
  - the planner variant.
- [ ] Run tests.

### Task 11: Agent surface: `ops --json` origin, guide recipes, warnings, tables

**Files:**
- Modify: `crates/cdors-core/src/ops/mod.rs` (`OpSpec.origin`), `crates/cdors/src/ops_cmd.rs`
- Modify: `crates/cdors-core/src/ops/output.rs` (region, bin and station tables in `outputtab`)
- Modify: `docs/agent-guide.md`

- [ ] **`ops --json` shows `"origin": "cdo" | "cdors"`.** `help <op>` for cdors operators prints their own help text.
- [ ] **`outputtab`** gains keys for the new dimensions (`region`, `bin`, `station`, `distance`); the meaning of the
      existing keys does not change.
- [ ] **Recipes in `cdors guide`** (≤ 1.5 kB added):
  - OHC: `-mulc,4.09e6 -volint,0/700 -setgrid,fesom:<dir>`;
  - ice area: `-regint`, or `fldint` with a hemisphere region;
  - ice extent via `fldhist`;
  - ice volume;
  - an Atlantic basin series;
  - a Drake transect of temperature.
- [ ] Write tests: `json:` rows for `ops`, `help regmean` and each new warning.
- [ ] Run tests.

### Task 12: Verify acceptance criteria (local)

- [ ] Every requirement in the Overview is implemented; deviations are in `docs/deviations.md`.
- [ ] Full suite: `cargo test` and `tests/run_cases.sh` (both NetCDF read paths, all variants).
- [ ] `bench/realdata_check.sh` is extended with the round-1 operators on the gate's dataset (login-node subset).

### Task 13: Real-data validation and speed (compute run, needs Nikolay's yes)

**Files:**
- Modify: `bench/unstructured/baseline.sh`
- Create: `docs/unstructured-results.md`

- [ ] Same diagnostics as the gate. cdors and nereus agree within `rel=1e-5` (float32 inputs, different summation
      order). Report wall time and peak RSS, and check that the `--mem` bound holds.
- [ ] The pass marks from Task 1 are met, or reported as missed with the cause.
- [ ] Node run ≤ 0.25 node-h (`--hint=nomultithread --mem=0`), with a background `sacct` watcher.

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
