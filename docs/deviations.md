# Deliberate deviations from cdo

cdors follows cdo 2.6.0 (the reference build on Levante; source references are to CDO 2.6.5)
except for the points below. Each entry says what cdo does, what cdors does, and why. The notes
that `cdors help <op>` and `cdors ops --json` print name their entry here as `[section: entry]`.
Bugs of cdo that cdors does not reproduce are listed in the last section. The analysis on
model meshes (round 1, FESOM2) is in "Latitude bands and histograms", "Model meshes", "Vertical
geometry", "Regions", "Sampling" and "Units of integrals"; operators marked "(cdors)" there
have no cdo counterpart (`"origin": "cdors"` in `cdors ops --json`).

Bit-identical output is not a goal. cdors accumulates in double precision and in its own order
(chunk by chunk, on several threads, with the same result for any thread count); cdo accumulates
partly in float32 and uses OpenMP reductions whose order depends on its build and thread count.
`tests/run_cases.sh` therefore compares most results exactly and the others within a tolerance:
2 float32 units in the last place of the largest value (`ulp` rows), or one histogram bin where
cdo's percentiles are approximate (`bin` rows).

## Files and outputs

- **No silent overwrite, no append.** cdo overwrites an existing output without asking
  (`src/fileStream.cc:116`), and `cat` appends to an existing output unless `-O` is given
  (`src/operators/Cat.cc:128`). cdors refuses an existing output (`output_exists`, exit code 4)
  unless `-O` is given, and never appends.
- **`-O` never deletes an existing Zarr directory.** `-O` replaces files only; an existing store
  must be removed by the user (`output_exists`).
- **An output that is also an input is refused**, also with `-O` (`bad_arguments`): the same
  file (symlinks resolved), a path inside an input Zarr store, or a name matched by an input glob
  pattern. cdo has no such check (a NetCDF-4 input it reads happens to be protected by the HDF5
  file lock: `cdi error (cdf__create): Permission denied`).
- **`mergetime` and `cat` need time to increase from one input to the next.** cdo `mergetime`
  merges its inputs timestep by timestep, so overlapping inputs interleave, and keeps repeated
  timesteps unless `skip_same_time` (`src/operators/Mergetime.cc:256-287`); cdo `cat` appends in
  argument order whatever the dates (`src/operators/Cat.cc`). cdors reads the inputs as one
  virtual dataset without copying them: `mergetime` orders the inputs by their first timestep,
  `cat` keeps the argument order, and both refuse inputs whose times overlap, repeat or go
  backwards (`bad_data`), because the downstream operators assume a monotonic time axis. Their
  inputs must be files or stores (or one glob pattern), not the output of other operators
  (`not_implemented`).
- **The output format follows the output name.** Without `-f`, cdo writes the input's format
  whatever the output is called: a NetCDF-3 classic input gives classic output. cdors writes Zarr
  v3 to a name ending in `.zarr` and NetCDF-4 to any other name. `-f nc` writes the 64-bit
  offset format, as cdo does; `-f nc4c` writes NetCDF-4, not cdo's NetCDF-4 classic model.
- **Output data type.** cdo keeps the input's type: integer variables stay integers, also for
  statistics (it writes the time mean of an `int` variable rounded to an `int`), and a packed
  variable (`short` with `scale_factor` and `add_offset`) stays packed through selections and
  becomes float32 after a statistic. cdors writes float32 variables as float32 and all others
  (packed or plain integers, float64) as unpacked float64, so no result is rounded or re-packed;
  `-b F32` and `-b F64` set the type. cdo's other `-b` types (`I8`, `I16`, `U8`, ...) are
  refused (`bad_arguments`).
- **NetCDF compression.** As in cdo, NetCDF output is compressed only with `-z zip` (deflate
  level 1, `zip_N`: level N), grid coordinates of 32 values or more included. cdors also
  shuffles the bytes of the data variables (cdo only with `CDI_SHUFFLE=1`), and it compresses
  the chunks on all its threads, where cdo deflates on one: a 1.2 GB HEALPix z9 output became
  648 MB in 1.1 s, `cdo -z zip -copy` made 817 MB in 28 s. The values are the same. `-z zstd`
  is refused for NetCDF (`bad_arguments`): cdo writes it through an HDF5 plugin that readers
  (cdo, ncdump, viewers) load only with `HDF5_PLUGIN_PATH`, and cdo 2.6.0 left small fields
  uncompressed; it applies to Zarr outputs. With `-f nc` (NetCDF-3) cdo ignores `-z` with a
  warning; cdors refuses it. cdo's GRIB compressions (`szip`, `aec`, `jpeg`) are refused.
- **Zarr output** (cdo writes none) is always compressed: blosc with byte shuffle and zstd level
  1, or the compressor and level of `-z zip_N` (blosc's zlib) or `-z zstd_N`.
- **NetCDF chunks.** cdo by default splits large fields into several chunks (262144 cells per
  chunk on a HEALPix z9 grid). cdors writes one chunk per field (all horizontal points of one
  level and step), or, when a statistic runs in lane waves, chunks that end at wave boundaries;
  `--chunks dim=n,...` sets them. The values are the same.
- **Global attributes.** Both copy the input's global attributes and add the command to
  `history` (`--no_history` leaves it out). cdo also adds `CDI`, `CDO` and, on an input
  without one, `Conventions = "CF-1.6"`; cdors adds none of them.

## Command line

- **cdo options that cdors does not have are refused** (`bad_arguments`, naming the option): `-k`
  (`--chunks` instead), `-r`, `-a`, `-R`, `-t`, `-p` and the other single-letter options not
  listed by `cdors --help`. cdo's `-L` (serialised I/O) is accepted and has no effect.
- **Threads.** cdo computes on one thread unless `-P` says otherwise. cdors uses every core
  available to the process (in a Slurm job, those of the allocation), at most 16 outside Slurm
  jobs (also with a larger `-P`), and keeps 64–128 reads in flight (`--io-threads`).
- **Exit codes.** cdo exits with 1 on every error it reports. cdors separates usage errors
  (1), data errors (2), I/O errors worth retrying (3) and refusals (4: read limit, too many
  values, existing output), and with `--json` reports each error as one JSON object with a
  stable code (README, "For agents").

## Limits

cdo has none of these limits; cdors checks them on the plan, before any data is read or any
output is created.

- **Read limit on login nodes.** cdors refuses a run that would decode more than `--max-read`
  (default 64 GB outside Slurm jobs, no limit inside them) with `read_limit`, exit code 4;
  `--max-read none` removes the limit.
- **Memory budget.** cdo allocates what a run needs. cdors plans every run within `--mem`
  (default 60 % of the Slurm allocation, on login nodes a quarter of the available memory, at most
  4 GiB) and refuses a run that cannot fit: `memory_limit` when the state of a single lane or two
  tiles in flight do not fit, `intermediate_too_large` when the inner results of a chain alive at
  the same time exceed half of the budget (exit code 2). A model mesh's resident arrays (centres,
  areas, wet levels, the grids' copies) and the per-level areas the kernels load are held outside
  the stages: the stages are planned for the budget less them (at least a quarter of it; a mesh
  that leaves less gets the `mesh_memory` warning), and `--plan` counts them
  (`memory.parts.mesh_resident`, `mesh_weights`). HDF5 and netCDF-C buffers and the allocator's
  overhead are not modelled: on dart, a 0–700 m heat content under `--mem 2G` peaked 6–9 % over
  the budget.
- **Printed values** are limited by `--max-values` (see "Printing values: Flood guard").

## Statistics

- **Exact percentiles also above 50 values per group.** cdo computes exact percentiles only while
  a grid point holds at most 50 values and switches to a 101-bin histogram bounded by the min/max
  input files above that (`src/percentiles_hist.cc`), so its result is approximate. cdors is exact
  for any group size; above 50 values it differs from cdo's result by up to about one bin
  (`(max − min) / 101`). cdo's three-input form (`timpctl,p in -timmin in -timmax in`) is accepted,
  but the min/max inputs are never opened or computed; the one-input form `timpctl,p in` (an
  error in cdo) is accepted too.
- **Percentiles of double-precision input.** cdo stores the values of a group as float32 before
  computing percentiles (`percentiles_hist.cc:histAddValue`); cdors keeps double precision.
- **Variance and standard deviation** (`*var`, `*var1`, `*std`, `*std1`). cdo accumulates Σx and
  Σx² and computes `(Σx² − (Σx)²/n) / (n − d)` at the end (`src/field2.cc:field2_var`,
  `fieldc_var`), clamping results in (−1e-5, 0) to 0 and setting negative results to missing. This
  one-pass formula loses precision by cancellation when the mean is large compared with the spread
  (e.g. temperatures in K, pressures in Pa). cdors uses Welford's update (running mean and sum of
  squared deviations, folded one timestep at a time in time order, so no merge formula is needed);
  the result is never negative. On the test fixtures both agree to the float32 output in all but a
  few values, which differ by one unit in the last place.
- **`cell_methods`.** cdo (through CDI) writes `cell_methods = "time: <method>"` only for `mean`,
  `avg`, `sum`, `range`, `min` and `max` statistics, and only when the output has time bounds
  (`libcdi/src/cdf_write.c:cdfDefineCellMethods`), so not for `*std`, `*var` and `yseas*`. cdors
  writes it for every time statistic except percentiles and running statistics (for which cdo
  writes none either), with the CF names `standard_deviation` and `variance` for the spread
  statistics. Like cdo, it replaces an existing `cell_methods` attribute.
- **Climatology arithmetic pairs variables by name** (`ymonsub` & co.) when both inputs have the
  same variable names; cdo pairs them by position. Zarr stores have no variable order, so pairing
  by position would combine the wrong variables.

## Space statistics

- **`cell_methods` on space statistics.** cdors adds `area: mean` (and the analogous
  `longitude: …` and vertical-coordinate entries) to `fld*`, `zon*` and `vert*` output; cdo does
  not.
- **HEALPix `zon*` summation order.** cdors sums each ring in stored (nested) order, cdo in ring
  order, so the last bits can differ. The ring latitudes cdors writes differ from cdo's in the
  15th significant digit.
- **`vert*` accumulates in double precision.** cdo accumulates vertical statistics in float32;
  cdors accumulates in f64 and rounds once on output.
- **Equal area weights are announced loudly.** For a grid without cell areas or cell bounds
  (an unstructured grid without bounds, a grid without coordinates) cdo weights all cells equally
  and prints one line, "Grid cell bounds not available, using constant grid cell area weights!"
  (`Fldstat.cc:155`). cdors uses the same equal weights but prints a framed warning that names
  the operator, the variables, the number of cells and the grid, `{"warning": "equal_weights"}`
  under `--json`, and a `warnings[]` entry in `--plan`, whose `area weights of ...` lines name
  the source of the weights of every variable. Such a mean is only right if the cells have equal
  areas, and the one-line warning is easy to miss.
- **Reduced Gaussian grids are weighted by cell area.** cdo 2.6 finds no cell bounds for reduced
  Gaussian grids (`gridToUnstructuredGaussianReduced`, `mpim_grid.cc:1035`, reads the latitude
  bounds from an empty array) and weights all cells equally, with the warning above: on the N32
  test grid its field mean is 1.6 K below the area mean. cdors (for the reduced grids it restores
  from gribscan references) takes each cell as the rectangle between the midpoints of its row's
  longitudes and the latitude bounds generated as for a regular axis, with its area computed as
  for regular grids. The field mean of the EERIE IFS skin temperature of January 1950 on the
  O1280 grid (285.66658 K) agrees with that of the same field on the 0.25° grid (285.66653 K);
  equal weights give 287.31 K.
### Latitude bands and histograms

- **Latitude bands `zonal_<DY>`.** As in cdo, `zonmean,zonal_<DY>` takes the part of each cell
  inside a band (cdo clips the cells with yac, cdors integrates along their edges: equal up to
  rounding). cdors also accepts `zonaldes=zonal_<DY>`, and `zonsum,zonal_<DY>` and
  `zonint,zonal_<DY>` with cdo 2.6.5's meaning (cdo 2.6.0 has neither); `zonint` without bands
  (cdo 2.6.5: the rows of regular grids) is refused. Other grid descriptions as the band
  argument are refused. cdo 2.6.0 crashes on a HEALPix subset with `zonal_<DY>`; cdors does not.
- **Latitude bands on model meshes** (`-setgrid,fesom:…`; cdo cannot read meshes). Nodes have no
  cell polygons in round 1 (only `fesom.mesh.diag.nc` is read), so on a mesh every cell (node or
  element) belongs to the one band that contains its centre, [lo, hi), and counts with its whole
  area, not with the part inside the band: `zonmean` is Σ a·x / Σ a, `zonint` Σ a·x (m²),
  `zonsum` Σ x over the valid cells of the band. Variables with the mesh's levels use the area
  of each level (FESOM2 nodes: `nod_area[k]`; elements: `elem_area`), dry levels left out. With
  coarse bands the result differs from the area-exact rule of other grids by the cells cut by a
  band edge.
- **`fldhist` (cdors; cdo has no histogram operator).**
  `fldhist,bins=<lo>/<hi>/<step>` or `fldhist,edges=<e0>/<e1>/…/<en>`, `[,out=area|fraction]`:
  the area of each field's valid cells per bin. Bins are [e_i, e_i+1), the last one closed.
  Values below e0 and above en go to the variables `<var>_below` and `<var>_above`, so nothing
  is dropped. `out=area` (default) uses `fldint`'s cell areas and refuses a grid without them;
  `out=fraction` uses `fldmean`'s weights (with the `equal_weights` fallback and its warning)
  and divides by the whole valid area, below and above included. On a model mesh there is one
  histogram per level with that level's areas, dry levels left out. Three explicit edges would
  look like `lo/hi/step`, hence the two keys. At most 100000 bins (`bad_arguments`).

## Model meshes (`-setgrid,fesom:<dir>`)

cdo reads FESOM2 output as generic grids (no coordinates, equal weights, every stored value
valid). After `-setgrid,<mesh spec>` cdors uses the mesh. FESOM2 is the only model in round 1;
its mesh is read from `fesom.mesh.diag.nc` alone (no ASCII mesh files).

- **The mesh must match the data.** `setgrid` checks the sizes of the data's horizontal
  dimensions (`nod2`/`nod_n` → nodes, `elem`/`elem_n` → elements) against the mesh and fails
  with `grid_mismatch` otherwise, so horizontal selections belong above `setgrid`. Both FESOM
  dimension orders are read: `(time, nz1, nod2)` (FESOM 2.1 and later) and `(time, nod2, nz1)`
  (FESOM 2.0), with the same results; outputs keep the input's order. A FESOM 2.0 file without
  the `nz1` coordinate variable (the PHC climatologies in `/pool/data/AWICM/FESOM2`) is one
  generic grid of nodes × levels for cdo (147580 = 3140 × 47 on pi); `setgrid` separates it into
  the nodes and a level axis with the mesh's depths (|Z|, positive down, with the `zbar` layer
  bounds), in either order, when the model's rules name both dimensions with the mesh's sizes;
  the levels exist only from `setgrid` on, so `-sellevel` goes above it. Otherwise `grid_mismatch` names the grid and, when its size is a place times the levels, says
  so.
- **Units of the node coordinates** (mesh.diag `nodes` has no `units` attribute; core2 and dart
  store radians, ng5 degrees): radians if |lat| ≤ π/2 and |lon| ≤ 2π (within a small ε),
  otherwise degrees; `fesom:<dir>,units=deg|rad` overrides (other values: `bad_arguments`), and
  `cdors mesh info` shows the decision. Both mesh.diag layouts are read (older `nod_n`, `elem_n`, `nl`, `zbar`, `Z`; newer
  `nod2`, `elem`, `nz`, `nz1`, `elements`/`face_nodes`). Element centres are the normalised mean
  of their three nodes' unit vectors.
- **Mesh cache.** The mesh is converted once into a UGRID Zarr store in
  `$CDORS_CACHE/meshes/<key>.zarr` (key: path, size and mtime of each source file; a content
  fingerprint of the first and last MiB is checked on load). An entry that fails the check is
  renamed to `<key>.zarr.bad-<time>-<pid>` (warning `mesh_cache_rebuilt`), never deleted.
  `--plan` and refused runs never write the cache: they read a usable entry or the source files
  and warn `mesh_cache_pending` (entry path, bytes and estimated build time). Without
  `CDORS_CACHE` the mesh is read from mesh.diag on every run.
- **Dry levels are missing.** A value at a level outside its cell's wet levels (below the bottom
  from `nlevels_nod2D`/`nlevels`, above `ulevels` under cavities) is missing for every operator
  that follows, whatever the value `setgrid` receives holds there (the file's 0, or what
  arithmetic before `setgrid` made of it). FESOM2 writes 0 there, so outputs carry the
  missing value where the input had 0, and `fldmin`, `fldmean`, `timmean`, ... leave those cells
  out instead of counting zeros. 2-D variables are unchanged. The wet levels come from
  mesh.diag; on dart it disagrees with `nlvls.out` at one node (node 0, under the Ross ice
  shelf: 2 wet layers in mesh.diag, 4 in the data), and cdors follows mesh.diag.
- **Field statistics use the mesh's areas, per level.** `fldmean`, `fldavg`, `fldstd`, `fldvar`
  (and their `1` forms) and `fldint` weight node `i` at layer `k` by `nod_area[k, i]` (the area at
  the layer's top interface; FESOM2 `nod_area` shrinks with depth along the coasts), elements by
  `elem_area` at every level, and 2-D variables by the surface row. Variables on the interfaces
  (`nz`, e.g. `w`) weight interface `k` by `nod_area[k]` (all `nl` rows); a node's area there is
  0 at its sea floor (its deepest wet interface), which is right for an area at the floor, so a
  level where every wet node is at its floor has no area (mean missing, integral 0). After
  `intlevel` each target depth is weighted by the layer that contains it (`nod_area[k]`, with
  that layer's dry cells), so `-fldmean -intlevel,100 -setgrid,fesom:…` is the mean at 100 m. The
  same weights serve `fldhist`, `zon*,zonal_<DY>` and `reg*`. cdo, given the same file with a grid
  description, would weight every level by one 2-D area. `--plan` names the weights
  (`area_weights: "mesh_per_level"`) and counts the per-level areas in its memory estimate
  (`memory.parts.mesh_weights`). On dart, the surface area used for every layer gives a
  0.40 % higher Atlantic 0–700 m heat content than the per-level areas.
- **`tonodes`, `toelems` (cdors).** Map a variable between the places of the mesh: `toelems` is
  the mean of an element's three nodes, `tonodes` the `elem_area`-weighted mean of the elements
  around a node. Missing values are left out (weights renormalised), and levels outside the
  target cell's wet range are missing.

## Vertical geometry

- **`vert*` on a mesh use the mesh's layers.** `vertmean`, `vertavg`, `vertstd`, `vertvar` (and
  their `1` forms) weight level k by the thickness of the full layer between the mesh interfaces
  k and k + 1 (FESOM2 `zbar`), normalised as cdo normalises layer weights; `vertint` multiplies
  by it. cdo, without layer bounds in the file, weights all levels equally. Dry levels are
  missing and drop out, so each cell is averaged over its own wet levels. **Full layers only**:
  FESOM2's partial bottom cells (`use_partial_cell`) and zstar layer thickness are not modelled.
  The thickness-weighted operators refuse variables on the interfaces (`nz`). `setgrid` gives the
  level axis of mid-level variables the mesh's layers as bounds (`nz1_bnds` from `zbar`, in the
  sign of the axis values), so `vert*` after `transect`, `selpoints`, `timmean` or in a written
  file still weigh by the layer thicknesses (`cdo zaxisdes` shows the bounds).
- **`vertint` (cdors; cdo has none)** is Σ thickness·value over the valid values, the thickness
  from the layer bounds of the vertical axis (1 without bounds, with the
  `equal_vertical_weights` warning), on a mesh from its interfaces. `vertint,<z0>/<z1>` cuts the
  layers at the depths z0 and z1 (positive down: absolute values of the bounds; metres on a
  mesh) and counts the part of each layer inside the range; without layer bounds it is refused
  (`no_coordinates`). A cell without a valid value inside the range is missing.
- **`volint`, `volmean`, `volint,<z0>/<z1>`, `volmean,<z0>/<z1>` (cdors).** Per timestep
  Σ area·dz·value over all cells and levels with valid values, and that divided by their volume
  Σ area·dz; areas per level on a mesh place that has them (FESOM2 nodes: `nod_area[k]`),
  otherwise the cell areas of `fldint`. `volmean` is not `fldmean -vertmean`: every wet cell
  counts with its volume. Variables without levels are refused (`unsupported_dimension`). With a
  depth range (also `vertint,<z0>/<z1>`) only the levels whose layer reaches into it are read,
  and only their per-level areas are loaded: no `-sellevel` is needed.
- **`intlevel`** interpolates as cdo does (float32 weights, cdo's missing-value rule, targets
  outside the levels missing unless `extrapolate=true`), from positional targets or `level=`;
  `zdescription=` and `zvarname=` are not implemented. Every variable with more than one level
  is interpolated on its own vertical axis (cdo: the variables on the first z-axis with more than
  one level, or with as many levels). The output axis has the target values and no bounds. On a
  mesh a target above a cell's top interface or below its bottom interface is missing: cdo's
  rule alone (a missing neighbour level is replaced by the other one if that one's weight is at
  least 0.5) would carry the deepest wet value up to half a layer into the ground.
- **`bottomvalue`** reduces every variable with levels (cdo: only those on the first z-axis with
  more than one level, the others are copied) and copies a file without levels (cdo aborts with
  "No processable variable found!"). On a mesh it takes each cell's own bottom, the value of its
  deepest wet level (the deeper mesh level is the bottom, whatever the axis's `positive`
  attribute says); cdo takes the deepest stored value of the column, the 0 that FESOM2 writes
  into the ground.

## GRIB (gribscan references)

cdors reads GRIB through gribscan's references (`README.md`); cdo reads the GRIB files. Where
the two views differ, cdors follows the references:

- **Time stamps.** gribscan's EERIE references stamp monthly means in the middle of the month
  (1950-01-15 12:00); cdo stamps a GRIB message at the end of its period (1950-02-01 00:00), so
  that cdo's `yearmean` of monthly means puts December into the next year.
- **Levels.** gribscan sorts the levels (1, 5, ..., 1000 hPa) where cdo keeps the order of the
  file, and drops a single level (2 m of `2t`, mean sea level of `msl`) that cdo keeps as a
  z-axis. Pressure levels, stored by gribscan in hPa without units, are given in Pa, as cdo gives
  them (`-sellevel,85000`).
- **Grids.** gribscan flattens every grid to one dimension `value`. cdors restores regular
  lon-lat and Gaussian grids from the coordinates (`lat`, `lon` dimensions, as cdo reads the GRIB
  file) and gives reduced Gaussian grids their rows (cell areas, see "Space statistics"); they
  stay one dimension, where cdo has its `gaussian_reduced` grid type. Any other grid stays
  unstructured, with cell centres only.
- **cdo cannot read the EERIE messages** with ecCodes 2.40 or later (cdo 2.5.1, 2.5.3 and 2.6.0
  on Levante: "section_2 size" errors and no `shortName`); cdo 2.5.0, built with ecCodes 2.34,
  reads them. cdors decodes the messages itself (templates 5.0 and 5.42, bit-maps), with values
  identical to ecCodes'.

## Time axis

- **`yseas*` timestamps follow cdo 2.6.0.** 2.6.0's `Yseasstat` stamps each season with the latest
  member date, counting December as December of the previous year, and ignores
  `--timestat_date`; 2.6.5 moved `yseas*` into `Ymonstat.cc` and uses the `last` rule. cdors
  matches 2.6.0, the reference build (details in `model/timegroup.rs`).
- **Time statistics on inputs with `months since` or `years since` units.** CDI encodes timestamps
  in such units with month-length rules that do not round-trip (`taxis.c:datetime2rtimeval`;
  2001-07-01 00:00 in `months since 2001-01-16` reads back as 2001-07-01 11:36:46). cdors writes
  the output time axis of a time statistic as `days since <the same reference>` instead, so the
  stored timestamps are exactly the group timestamps; `showtimestamp` therefore differs from cdo's
  output for such inputs.
- **Real Julian calendar.** CDI treats `julian` like `proleptic_gregorian` (cdo warns and falls
  back); cdors uses the Julian calendar (a leap year every four years).
- **Timestep, year and month ranges are not expanded.** `seltimestep,1/400000000` selects the
  existing timesteps at once; cdo expands the range into a list first. Ranges of more than 1000
  members are reported as one "not found" warning instead of one per member.
- **Time values beyond about 100 million years** from the reference (typically an unmasked fill
  value such as 9.97e36) are `bad_data`, not dates.

## Reading values

- **`_Unsigned = "true"`.** CDI honours it for byte variables only (and a byte variable with
  `valid_range = 0, 255`) and does not mask the `_FillValue` of such variables (a fill value of -1
  is read as 255; `stream_cdf_i.c`). cdors reads every signed integer type with `_Unsigned =
  "true"` as unsigned, as netCDF-Java and xarray do, and compares the missing values in the stored
  bit pattern, so the fill value stays missing.
- **`valid_range`, `valid_min`, `valid_max`** are applied on read as in CDI
  (`cdf_read.c:cdfDoInputDataTransformationDP`): stored values outside the range become missing,
  before unpacking; only for variables with a missing value (`_FillValue` or `missing_value`);
  attributes whose type kind (integer or float) differs from the variable's are ignored;
  `valid_range` takes precedence. One difference: with `valid_max` alone, CDI uses `DBL_MIN` (the
  smallest positive double) as the lower bound and so also masks all values ≤ 0; cdors leaves the
  lower end open. cdors does not copy `valid_range`, `valid_min`, `valid_max` and `_Unsigned` to
  its outputs (they describe the stored input, and results such as `-addc` may lie outside the
  range); cdo copies them.
- **HEALPix grid mappings need an order.** CDI takes any `healpix_order` that does not start with
  `nest` as ring order, also a missing one; cdors accepts `nested`/`nest`/`ring`
  (`healpix_order`, or CF's `indexing_scheme`) and fails with `bad_data` otherwise, and also for
  an nside (`healpix_nside`, or CF's `refinement_level`) that is not positive, too large for the
  dimension, or not a power of two for nested order.
- **NaN is the internal missing value.** cdo carries the variable's missing value (default
  −9e33) through its computations; cdors uses NaN inside the engine and writes the variable's
  `_FillValue` on output. Results are the same unless an input contains NaN as a valid value.

## Remapping

- **Weights are generated for the unmasked grid.** cdo regenerates weights whenever the
  missing-value mask of a field changes; cdors generates them once for the unmasked grid and
  reproduces cdo's per-method missing-value rules by renormalising (see `remap/weights.rs`).
  Results agree with `cdo remap*` within 1 float32 ulp. The missing patterns are identical
  wherever cdors can tell from the unmasked weights which rule cdo applied. For `remapbil` from a
  2-D (regular or curvilinear) source grid, cdo replaces the bilinear weights of a destination
  whose search fails by a distance-weighted average of the valid neighbours (the "Bilinear
  interpolation failed" warning), which cdors reproduces by renormalising; it recognises such
  rows by a destination latitude outside the source latitudes, by four links that are not one
  quad of the source index space, or by weights that are not bilinear. Two cases remain
  undetected and give a missing value where cdo has one: a fallback whose four nearest source
  points form a quad with distances symmetric enough to look bilinear, and a destination whose
  search fails in cdo only because masked cells change the nearest source point. On the
  HadGEM3-GC31-LL ORCA1 `tos` (curvilinear, 1950–59 and 2010–14, to r360x180, r144x72, n32) the
  missing patterns are identical.
- **`hpdegrade,zoom=0`.** cdo 2.6.0 ignores `zoom=0` and keeps the input resolution; cdors
  follows cdo 2.6.5 and degrades to nside 1.
- **`--force` is accepted and ignored.** cdo needs it for `remapcon` from or to HEALPix grids;
  cdors always passes it to `cdo gencon` and `cdo genycon`.

## Regions

- **Region catalog (cdors; cdo knows only region files and GMT's DCW).** `regions=<set>` takes a
  set name or a GeoJSON file of polygons. Built into the binary: `ar6.all`, `ar6.land`,
  `ar6.ocean` (IPCC AR6, Iturbide et al. 2020, CC-BY 4.0), `srex`, `giorgi`, `prudence` (from
  regionmask, MIT; coordinates from the papers) and `nino` (NOAA CPC's Niño 1+2, 3, 3.4 and 4
  boxes). From a regions directory (`$CDORS_REGIONS`, else `share/regions/` of the install),
  unsimplified: Natural Earth v5.1.2 (public domain) `ne.countries_50`, `ne.countries_110`,
  `ne.land_50`, `ne.land_110`, `ne.ocean_basins_50`, and nereus' `basins` and `moc` (MPAS
  geometric_features, from the IHO seas via marineregions.org, CC-BY 4.0). Source, licence and
  attribution are attributes of every output (`regions_source`, `regions_license`,
  `regions_attribution`); `cdors help regmask` lists them. There is no hemisphere set: use
  `-sellonlatbox,-180,180,0,90`.
- **`regmask`.** A cdors operator (cdo has none): `regmask,regions=<set>[,select=<abbrev,...>][,fill=nearest]`
  writes `mask(region, <cells>)` as int8 (1: the cell centre lies in the region) with the
  coordinate `region` (the region numbers) and the string variables `region_name` and
  `region_abbrev` (NetCDF: character arrays over `<name>_strlen`; Zarr: variable-length UTF-8
  strings). The grid stays as it is; `region` is an axis of the vertical kind, so cdo reads the
  file as one field with a level per region (and skips the string variables with a warning).
  Membership follows regionmask 0.13 (`mask_3D`, identical cell for cell on the test grids and
  on dart's nodes), with one difference: every region is kept (regionmask's `drop=False`); an
  empty region is a zero row and the warning `regions_empty`. `select=` masks with the selected
  regions only, as regionmask's `regions[[...]]`. `fill=nearest` gives every cell outside all
  regions to the region of the nearest member cell: on a model mesh every cell with a wet level,
  on other grids every cell (there is no wet mask without a mesh), so on grids with land it
  fills the land too. The count is the warning `regions_filled` (stderr, `--plan` `warnings[]`)
  and the attribute `regions_filled`.
- **Region statistics** `regmin`, `regmax`, `regsum`, `regint`, `regmean`, `regstd`
  (`regions=<set>[,select=...][,fill=nearest]`), cdors operators: the `fld*` statistic over the
  cells of each region (membership as `regmask`; overlapping sets count a cell in each of its
  regions). `regsum`, `regmin`, `regmax` are unweighted; `regint` is Σ area·value (units: see
  "Units of integrals"); `regmean` and `regstd` are weighted by cell area (`regstd` with cdo's
  one-pass variance, as `fldstd`); on a model mesh per level with that level's areas. The
  horizontal dimensions become `region` (cdo reads it as a generic grid); a region without
  valid cells is missing (without any cell also the warning `regions_empty`). Region names are
  known only inside the chain that makes them: `outputtab,region_name` on a stored file fails
  with `no_coordinates` (cdors reads no string variables).
- **`maskregion,regions=<set>[,select=...][,fill=nearest]`.** A cdors extension: the union of the
  catalog regions (regionmask's rules) as the mask; cdo's region files work as in cdo.
- **`selregion` with several polygons.** cdo 2.6.0 (and 2.6.5's source) appends the selected
  cells after each polygon to the list of all cells selected so far, so the cells of the first
  polygon appear again for every further polygon. cdors writes every selected cell once, in
  ascending order.
- **`selregion` on the highest latitude of a polygon.** cdo 2.6.0's `selregion` drops cell
  centres on the polygon's highest latitude that its `maskregion` (and 2.6.5's `selregion`
  source) keep; cdors follows 2.6.0: the latitude range of a polygon is exclusive for
  `selregion` (inclusive for `maskregion` on unstructured, curvilinear and HEALPix grids,
  exclusive on regular grids, as cdo).
- **Field statistics after `selregion` on a regular grid.** cdo's unstructured grid of the
  selected cells has no cell bounds, so `fldmean` weights them equally; cdors keeps the cell
  areas of the original grid.
- **`dcw:<code>` is not implemented** (`not_implemented`): it needs GMT's DCW database. Natural
  Earth has the countries: `maskregion,regions=ne.countries_50,select=<country name>` (e.g. `select=Germany`).
- **`maskregion` and variables on other grids.** cdo writes only the variables on the masked
  grid (and needs exactly one); cdors masks every grid with coordinates and passes single
  points unchanged.

## Sampling: `selpoints` and `transect`

Both are cdors operators. cdo's nearest point is `remapnn,lon=<x>_lat=<y>` (weights from cdo,
one point per call), which cdors keeps unchanged.

- **`selpoints,file=<csv>|lon=<x>_lat=<y>[,...][,method=nn|linear]`** gives the dimension
  `station` with `lon`, `lat`, `station_name` and, on a mesh, `bottom_depth`. `nn` takes the
  nearest cell (KD-tree on unit vectors; on equal distances the smallest cell index); `linear`
  takes barycentric weights in the mesh's own triangles (FESOM2 nodes) or, on built-in grids,
  the local spherical Delaunay triangle of the nearest cells. The weights are planar in the
  triangle, as FESOM2's P1 elements, so a field linear in x, y, z is exact at the point's
  central projection, not at the point (O(h²)). A mesh place without triangles of its own
  (FESOM2 elements, a subset of the nodes) is sampled by `nn` with the warning `linear_as_nn`;
  a point in no triangle is missing (warning `points_outside`).
- **`transect,<lon0>,<lat0>,<lon1>,<lat1>[,...]|file=<GeoJSON LineString>[,step=<km>][,method=…]`**
  samples great-circle segments in equal arcs, at most `step` km apart (default: the median
  cell spacing along the path), on a sphere of 6371 km; one path per call. The output has the
  dimension `distance` (km) with `lon`, `lat` and `bottom_depth`.
- **Levels are kept, values below the bottom are missing.** Neither operator interpolates in
  the vertical. A value is missing if any cell its point uses is missing or dry there.
- **nereus' `transect` samples otherwise:** `n_points` (default 100) on one segment by
  normalised chord interpolation, a flat-earth distance, no dry-level masking, and it returns a
  figure. On cdors' points, nereus' nearest-node rule (scipy's cKDTree on unit vectors) picks
  the same nodes: Drake on pi (checked against nereus itself) and on dart.

## Overturning: `moc`

A cdors operator; cdo has none. The usual FESOM2 tool is pyfesom2's `xmoc_data`.

- **`moc[,dlat=<deg>|lat=<lat>[/<lat>...]][,regions=<set>,select=<abbrev,...>]`** takes the
  vertical velocity on the mesh's interfaces (FESOM2 `w` on `nz`). Each element gets the mean
  of `w` at its three nodes, at the interfaces that top its wet layers; the sea floor below them
  is left out, since no flow crosses it. At each interface, ψ(φ) = −Σ w_e·A_e over the basin's
  elements whose centre lies on or north of the line φ (`elem_area`; centres: the normalised
  mean of the nodes' unit vectors). This is minus the northward transport below the interface,
  so the streamfunction closes at the basin's northern end. It is written in Sv for `w` in m/s.
- **ψ at the surface is not 0.** At the top interface ψ is the net flow through the sea surface
  north of the line (FESOM2's linear free surface: `w` there is not 0). ψ equals the
  northward transport above the interface only where that flow is 0. On dart, AMOC at 26.5° N
  in January 1990, it is 0.216 Sv, and the maximum is 17.47 Sv. The transport above the interface (0 at the
  surface, 17.26 Sv) is the output minus its top interface:
  `cdors -sub moc.nc -sellevel,0 moc.nc above.nc`. pyfesom2 also leaves the surface term in.
- **The basin** is the elements whose centre lies in the selected regions, by `regmask`'s
  rules (`regions=moc,select=Atlantic_MOC`: MPAS/nereus, 34° S to the Nordic Seas and Baffin
  Bay, without the Arctic Ocean). Without `regions=` it is the whole mesh. `fill=nearest` is
  refused.
- **Lines:** every `dlat` degrees from −90 to 90 (default 1), or the latitudes of `lat=`. A
  line's value is missing below the deepest element of the basin that the line crosses (the
  latitudes of its vertices span the line).
- **Differences from pyfesom2's `xmoc_data`:**
  - pyfesom2 masks the nodes and then averages each element over all three, so an element on
    the basin's edge counts with zeros for its outside nodes; cdors takes an element whole or not
    at all.
  - pyfesom2 bins elements by the arithmetic mean of their nodes' latitudes, on lines
    `linspace(-90, 90, nlats)`, and masks the zeros.
  - Both leave out the elements' sea-floor interfaces.

## Section transport: `transport`

A cdors operator; cdo has none. The usual FESOM2 tool is pyfesom2's `cross_section_transport`.

- **`transport,<lon0>,<lat0>,<lon1>,<lat1>[,...]`** (or `file=<GeoJSON LineString>`) takes two
  inputs, the eastward and the northward velocity on the elements and layers of a mesh (FESOM2
  `u`, `v` on `elem`, `nz1`), one variable each. The path is great-circle arcs through the
  waypoints. In each element it crosses, the flux goes through the broken line from the midpoint of
  the edge where the path enters, through the centre, to the midpoint of the edge where it leaves.
  These are the faces of FESOM2's median-dual cells. With one velocity per element only the
  line's two ends matter, so each element keeps the chord between the two midpoints, as its east
  and north extent (dE, dN) at the element's centre. Per layer, T = Σ (u·(−dN) + v·dE)·dz over the
  crossed elements with water in the layer, in Sv for m/s.
- **Sign:** positive from the right of the path to its left: northward across a west-to-east
  path, eastward across a north-to-south one. pyfesom2 fixes the sign by the section's
  orientation (zonal: northward, meridional: eastward), which is the same for paths drawn west
  to east or north to south.
- **Lengths** are on a sphere of the model's radius (FESOM2 `r_earth`, 6367.5 km); pyfesom2
  uses the WGS84 ellipsoid (local east-north-up), which makes its lengths 0.2–0.4 % longer at
  high latitudes. Layer thicknesses are the |zbar| differences (full cells, no partial bottom
  cells).
- **Ends:** an element where the path starts or ends (an end in the ocean) has only one crossed
  edge and is left out, as in pyfesom2; the warning `section_open_ends` counts them.
- **Velocities are taken as stored.** FESOM2 writes them rotated back to geographic
  coordinates when `FESOM_autorotate_back_to_geo` is set (`-1` in the file's attributes). pyfesom2
  rotates them by default (`uvrotated=True`), which is wrong for such files.
- The output is one point (the path's middle) with the input's layers; `-vertsum` gives the
  total, `-sellevel` before the operator a depth range.

## Units of integrals

- **The integrals of cdors operators write float64 with the integral's units:** `vertint`
  `<input units> m`, `regint` `<input units> m2`, `volint` `<input units> m3` (just `m`, `m2`,
  `m3` without input units); `fldhist,out=area` writes m², `out=fraction` `1`; `moc` writes Sv
  for a velocity in m/s (else `<input units> m2`).
- **cdo-named operators keep cdo's behaviour:** `fldint` and `zonint` keep the input's units
  and type (cdo 2.6.0's `fldint`: float32, units unchanged). `volmean`, `regmean` and the other
  means keep the input's units and type.
- **`-mulc` keeps the units**, as in cdo: the heat content `-mulc,4.09e6 -volint,…` of a
  temperature in °C is in J but labelled `C m3`.

## Information operators

- **`sinfo` prints cdors' own summary.** cdo's `sinfo` is a table with institute, source, step
  type and parameter ID per variable and every timestamp of the input. cdors prints per variable
  the time flag, levels, points, data type, chunk shape and name, then the grids, the vertical
  axes and the time axis with its first and last timestamp; `--json` gives the same as one
  object (README, "For agents"). `showname`, `showtimestamp` and `griddes` print cdo's text.

## Printing values (`info`, `infon`, `output`, `outputf`, `outputtab`)

- **Flood guard.** cdo prints whatever it is asked to. cdors refuses, before reading, to print
  more than `--max-values` values (default 1,000,000; for `info`/`infon` the limit counts
  fields, one line each) with `too_many_values`, exit code 4; `--max-values none` removes it.
- **Any chain as input.** The printing operators take the output of any operator chain, as in
  cdo; the other information operators (`sinfo`, `showname`, `showtimestamp`, `griddes`) take
  files or stores only (`not_implemented` otherwise).
- **`--json`.** cdo has no structured form; cdors prints one JSON object (README, "Read values").
  Values of float32 variables appear there in their shortest float32 form, also the `info` mean
  (cdo's text shows 5 significant digits of the double mean).
- **Parameter IDs** (`info`, `outputtab` keys `param` and `code`). cdors derives them as CDI
  does: a `param` attribute (`"52.1.0"`), a numeric `code` (with `table`) attribute, or a name
  `var<N>`/`code<N>`/`param<N>` give the ID; the code of a GRIB2-style parameter
  (`num.cat.dis`) is −(position in the output + 1). Without these, cdo numbers NetCDF variables
  −1, −2, … in file order; cdors uses the position among the data variables of the first input,
  which matches cdo for NetCDF; Zarr stores have no variable order, so the IDs of Zarr inputs
  without a `param` attribute follow the order in which cdors lists their variables.
- **Several grids.** cdo's `output*` operators refuse a dataset whose variables are on different
  grids (`Output.cc`: "Too many different grids!"); cdors prints each variable on its own grid.
- **`outputf` formats** must hold exactly one floating-point conversion (`%[flags][width]
  [.precision](e|f|g|a)`, plus literal text and `%%`); cdo passes any string to `printf`.
- **HEALPix `x`, `y`, `xind`, `yind`** (`outputtab`): cdo 2.6.0 treats HEALPix as a projection
  grid; on a subset it prints the cell centres in degrees and `yind` 1, which cdors matches; on
  a complete HEALPix grid cdo 2.6.0 crashes (see below), cdors prints the same columns.

## cdo bugs observed (cdors does not reproduce them)

- **Percentiles near p = 100.** For the methods `hazen`, `weibull`, `median_unbiased` and
  `normal_unbiased`, cdo computes `j == n` and reads `x[n]`, one element past its buffer (zeroed
  memory), returning `(1−h)·x[n−1]`; cdors returns `x[n−1]`.
- **Partial last time chunk of a HEALPix Zarr array.** For a variable on a HEALPix grid
  (`grid_mapping_name = "healpix"`), cdo 2.6.0 reads a whole time chunk per `nc_get_vara_float`
  call and does not shorten the last call, where the array ends inside the chunk: on a 2928-step view
  of the W4 store (chunks of 248 steps) it asks for start 2728, count 248. netCDF-C 4.10.0 rejects
  that call. Asking cdo for one step there (`-seltimestep`, `outputf`) ends with `NetCDF:
  Start+count exceeds dimension bound`. Reading through the input (`copy`, `daymean`, `ydaymean`,
  `yearmean`) exits 0 without a message, but the steps of that chunk come out rotated in time: output
  step 2728 holds step 2856, and the 200 steps are rotated by 128. Results that do not depend on the
  order of the steps come out right, such as `timmean`, a whole-range `timpctl`, or a `yearmean`
  whose year contains the whole chunk. On generic grids cdo reads one step per call and is not
  affected. Seen on a 731-step view of the W1 store (11 steps wrong, up to 0.21 K in an earlier
  check) and on the benchmark's W4Y `ydaymean`: 26 of 366 days wrong, by up to 46 K
  (`docs/bench-results.md`). The full `ngc4008_PT3H_9.zarr` (87664 steps) ends inside a chunk too,
  and cdo cannot read its last 120 steps. cdors and zarr-python agree to float32 rounding.
- **Empty time selection in a chain.** A time selection that selects nothing ends cdo 2.6.0
  with "No timesteps selected!" (exit 1) and no output, as it ends cdors (`bad_arguments`, exit
  1, before any data is read). Inside a chain (`cdo -seltimestep,1 -selyear,3000 in out`) cdo
  crashes instead: `terminate called without an active exception`, SIGABRT, exit code 134.
- **Time bounds of a time selection from a CMIP6 file.** On
  `tos_Omon_HadGEM3-GC31-LL_historical_r1i1p1f3_gn_195001-201412.nc` (360_day calendar),
  `cdo -selyear,2000` and `cdo -seltimestep,601/603` write the time bounds of the file's first
  record (36000, 36030 days since 1850-01-01, i.e. January 1950) for every selected timestep; the
  time values themselves are right. cdors writes each step's own bounds (54000–54030, …).
- **`outputtab` with coordinate keys on a complete HEALPix grid.** cdo 2.6.0 ends with a
  segmentation fault on `outputtab,lon,lat,value -selname,tas hpz2_noleap.nc` (and with `x`,
  `y`); on a `sellonlatbox` subset it works. cdors prints the cell centres.
