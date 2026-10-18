#!/usr/bin/env bash
# Generate the tiny test fixtures used by tests/run_cases.sh.
# Idempotent: a fixture that already exists is skipped; nothing is ever deleted or overwritten.
#
#   r36x18_std    36x18 regular grid, standard calendar, 1096 daily steps (2000-2002), tas + pr
#   r36x18_2lev   same, tas on two pressure levels (1000 and 850 hPa)
#   hpz2_noleap   HEALPix zoom 2 (nside 4, 192 cells), noleap (cdo writes "365_day") calendar, 1095 steps, with missing values
#   unst_360      20x18 regular grid turned unstructured (360 cells), 360_day calendar, 1080 steps
#   r36x18_std_y2000 / _y2001 / _y2002   the years of r36x18_std (cdo splityear), for mergetime / cat
#   hpz2_noleap.zarr2 / .zarr3   xarray copies of hpz2_noleap (Zarr v2 / v3), time chunks of 73
#   hpz2_noleap_tiny.zarr2       tiny chunks (10 steps x 16 cells) for the planner check
#   <f>_ymonmean / _ydaymean / _yseasmean   climatologies of r36x18_std and hpz2_noleap (cdo)
#   weights_con_r36x18_r18x9.nc  SCRIP weights of cdo gencon,r18x9 for r36x18_std (remap,<grid>,<weights> rows)
#   nocoord_360   unst_360 without horizontal coordinates (FESOM-like; written by xarray)
#   fesom_pi/ fesom_pi_deg/ fesom_pi_data fesom_pi_cav/ fesom_pi_cav_data fesom_pi_w r1x1_edges   tests/make_fesom_fixtures.py (FESOM_PYTHON; details
#                 in its header): the pi mesh directory (radians, nod_n), the same mesh.diag in ng5's
#                 layout (degrees, nod2, ...), 24 monthly FESOM2-layout fields on pi (temp, salt (time,
#                 nod2, nz1), a_ice, m_ice (time, nod2), u, v (time, elem, nz1); 0 below the bottom),
#                 and a 1 deg grid with centres on integer degrees (AR6 outlines, -180, poles)
#   fesom_pi_data.zarr2 / .zarr3   xarray copies of fesom_pi_data, time chunks of 6
#   fesom_pi_data_tiny.zarr2       tiny chunks (5 steps x 400 nodes / 700 elements x 10 levels)
#   regions/      fixture-local region catalog for CDORS_REGIONS (tools/export_regions.py:
#                 ne.<set>, basins, moc GeoJSON) and the cdo region files cdo_{two,one,none}.txt
#   samples/      stations.csv (name,lon,lat, for selpoints) and drake.geojson (a LineString in a
#                 FeatureCollection, for transect,file=)
#   grb_ll / grb_gg / grb_rgg   GRIB2 files written with ecCodes (tests/make_grib_fixtures.py: regular
#                 lon-lat 10 deg on 2 pressure levels with CCSDS packing and a bit-map, regular Gaussian
#                 N16, reduced Gaussian N32), gribscan's references to them (<name>.json, read by cdors in
#                 the "grib" variant of tests/run_cases.sh) and cdo's NetCDF copies (<name>.nc, -b F64)
#
# All values are float32 (-b F32), vary in space and time, and come from cdo `expr` on a
# `for` time series, so they are reproducible. Writes are atomic (tmp name, then mv).
set -euo pipefail
shopt -s inherit_errexit 2>/dev/null || true

CDO=${CDO:-/sw/spack-levante/cdo-2.6.0-akkxhz/bin/cdo}
TARGET=${CDORS_TARGET:-${CARGO_TARGET_DIR:-/work/ab0995/a270088/cdors-target}}
FIX=${CDORS_FIXTURES:-$TARGET/fixtures}
# zarr-python >= 3 writes both Zarr v2 and v3; the base mambaforge has zarr 2.14 (v2 only).
ZARR_PYTHON=${ZARR_PYTHON:-/work/ab0995/a270088/mambaforge/envs/aimip-virt/bin/python}
# eccodes and gribscan (0.0.7) for the GRIB fixtures
GRIBSCAN_PYTHON=${GRIBSCAN_PYTHON:-/work/ab0995/a270088/mambaforge/envs/gribscan/bin/python}
# numpy + netCDF4 for the FESOM fixtures, and the pi mesh they are made on
FESOM_PYTHON=${FESOM_PYTHON:-/work/ab0995/a270088/mambaforge/envs/nereus/bin/python}
FESOM_PI_MESH=${FESOM_PI_MESH:-/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/pi}
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

mkdir -p "$FIX"
cd "$FIX"
set -f   # expr strings contain * and ?: no globbing

# Fields: _la/_lo are latitude/longitude in radians, c is the timestep number (1..N).
TAS='280+25*cos(_la)+6*sin(_lo)*sin(6.2831853*c/365)+3*sin(12.9898*c+78.233*_lo+37.719*_la)+0.002*c'
PR='6+4*sin(3.1*c+5.3*_lo-2.7*_la)+1.5*cos(_la)*cos(6.2831853*c/365)'

# chain GRID CALENDAR NSTEPS EXTRA_PRE_OPS COORD_UNITS(deg|rad) [MASK]
#   EXTRA_PRE_OPS is applied to the grid before expr (e.g. -setgridtype,unstructured).
#   cdo returns HEALPix clon/clat in radians, other grids in degrees.
#   MASK=1 marks cells north of 1 rad (~57N) and a band of tas values as missing.
chain() {   # prints the cdo chain (without output) for one variable set
  local grid=$1 cal=$2 n=$3 pre=$4 units=$5 mask=${6:-0} expr la lo
  if [[ $units == deg ]]; then la='rad(clat(c))'; lo='rad(clon(c))'; else la='clat(c)'; lo='clon(c)'; fi
  # "+0*c" keeps the result time-varying: cdo 2.6.0 expr gives a ?: result the time type of its condition.
  if [[ $mask == 1 ]]; then
    expr="_la=$la;_lo=$lo;tas=(_la+0*c>1.0)?-999:($TAS);pr=(_la+0*c>1.0)?-999:($PR);"
    printf '%s ' -setrtomiss,300.0,300.4 -setrtomiss,-1000,-998
  else
    expr="_la=$la;_lo=$lo;tas=$TAS;pr=$PR;"
  fi
  printf '%s ' "-expr,$expr" -settaxis,2000-01-01,12:00:00,1day "-setcalendar,$cal" \
    $pre -setname,c "-enlarge,$grid" "-for,1,$n"
}

nc() {   # nc NAME CHAIN...: write NAME.nc atomically unless it exists
  local name=$1; shift
  if [[ -e $name.nc ]]; then echo "exists: $FIX/$name.nc"; return; fi
  # shellcheck disable=SC2068
  "$CDO" -s --no_history -f nc4 -b F32 $@ "$name.nc.tmp$$"
  mv "$name.nc.tmp$$" "$name.nc"
  echo "made:   $FIX/$name.nc"
}

nc r36x18_std  $(chain r36x18 standard 1096 '' deg)
nc hpz2_noleap $(chain hpz2   365_day  1095 '' rad 1)
nc unst_360    $(chain r20x18 360_day  1080 -setgridtype,unstructured deg)

# Yearly pieces of r36x18_std (split under a temporary prefix, then renamed one by one).
if [[ -e r36x18_std_y2000.nc && -e r36x18_std_y2001.nc && -e r36x18_std_y2002.nc ]]; then
  echo "exists: $FIX/r36x18_std_y{2000,2001,2002}.nc"
else
  "$CDO" -s --no_history -f nc4 -b F32 splityear r36x18_std.nc "r36x18_std_y.tmp$$."
  for y in 2000 2001 2002; do
    [[ -e r36x18_std_y$y.nc ]] || { mv "r36x18_std_y.tmp$$.$y.nc" "r36x18_std_y$y.nc"; echo "made:   $FIX/r36x18_std_y$y.nc"; }
  done
fi

# Two pressure levels of tas: merge two single-level chains with different z-axes.
printf 'zaxistype = pressure\nsize = 1\nlevels = 100000\n' > zaxis_p100000.txt
printf 'zaxistype = pressure\nsize = 1\nlevels = 85000\n'  > zaxis_p85000.txt
nc r36x18_2lev -merge \
  -setzaxis,zaxis_p100000.txt -selname,tas $(chain r36x18 standard 1096 '' deg) \
  -setzaxis,zaxis_p85000.txt -subc,15 -selname,tas $(chain r36x18 standard 1096 '' deg)

# Climatologies for the ymon*/yday*/yseas* arithmetic rows (second input "a:a_ymonmean" etc.).
for f in r36x18_std hpz2_noleap; do
  for s in ymonmean ydaymean yseasmean; do nc "${f}_$s" "-$s" "$f.nc"; done
done

# Zarr copies of hpz2_noleap and fesom_pi_data, written by xarray (real third-party Zarr, not our
# own writer). CHUNKS maps dimension names to chunk sizes (other dimensions: one chunk); it applies
# to every data variable with two or more dimensions.
zarr() {  # zarr NAME SOURCE FORMAT CHUNKS      e.g. CHUNKS=time=10,healpix_index=16
  local name=$1
  if [[ -e $name ]]; then echo "exists: $FIX/$name"; return; fi
  if [[ ! -x $ZARR_PYTHON ]]; then echo "skip:   $name (no ZARR_PYTHON=$ZARR_PYTHON)"; return; fi
  if [[ ! -e $2.nc ]]; then echo "skip:   $name (no $FIX/$2.nc)"; return; fi
  "$ZARR_PYTHON" -I - "$FIX/$2.nc" "$FIX/$name.tmp$$" "$3" "$4" <<'EOF'
import sys, xarray as xr, zarr
src, dst, fmt = sys.argv[1], sys.argv[2], int(sys.argv[3])
chunks = {k: int(v) for k, v in (kv.split("=") for kv in sys.argv[4].split(","))}
ds = xr.open_dataset(src, decode_times=False)   # keep time as numbers + units/calendar attrs
enc = {}
for v in ds.data_vars:
    if ds[v].ndim >= 2:
        ds[v].encoding.pop("chunksizes", None); ds[v].encoding.pop("contiguous", None)
        enc[v] = {"chunks": tuple(min(chunks.get(d, n), n) for d, n in ds[v].sizes.items())}
ds.to_zarr(dst, mode="w-", zarr_format=fmt, encoding=enc, consolidated=(fmt == 2))
EOF
  mv "$FIX/$name.tmp$$" "$FIX/$name"
  echo "made:   $FIX/$name"
}
zarr hpz2_noleap.zarr2      hpz2_noleap 2 time=73
zarr hpz2_noleap.zarr3      hpz2_noleap 3 time=73
zarr hpz2_noleap_tiny.zarr2 hpz2_noleap 2 time=10,healpix_index=16

# FESOM2 fixtures on the pi mesh and the 1 deg region-edge grid, then Zarr copies of the FESOM data.
if [[ ! -x $FESOM_PYTHON ]]; then
  echo "skip:   fesom_pi* r1x1_edges (no FESOM_PYTHON=$FESOM_PYTHON)"
else
  "$FESOM_PYTHON" -I "$HERE/make_fesom_fixtures.py" "$FIX" "$FESOM_PI_MESH"
fi
zarr fesom_pi_data.zarr2      fesom_pi_data 2 time=6
zarr fesom_pi_data.zarr3      fesom_pi_data 3 time=6
zarr fesom_pi_data_tiny.zarr2 fesom_pi_data 2 time=5,nod2=400,elem=700,nz1=10

# FESOM-like file without horizontal coordinates: unst_360 with lon/lat, their bounds and the
# `coordinates`/`CDI_grid_type` attributes removed, cells renamed to nod2 (err:no_coordinates rows).
if [[ -e nocoord_360.nc ]]; then
  echo "exists: $FIX/nocoord_360.nc"
elif [[ ! -x $ZARR_PYTHON ]]; then
  echo "skip:   nocoord_360.nc (no ZARR_PYTHON=$ZARR_PYTHON)"
else
  "$ZARR_PYTHON" -I - "$FIX/unst_360.nc" "$FIX/nocoord_360.nc.tmp$$" <<'EOF'
import sys, xarray as xr
ds = xr.open_dataset(sys.argv[1], decode_times=False)
ds = ds.drop_vars([v for v in ds.variables if v.startswith(("clon", "clat", "lon", "lat"))])
ds = ds.rename_dims({d: "nod2" for d in ds["tas"].dims if d != "time"})
for v in ds.variables.values():
    v.attrs.pop("coordinates", None); v.attrs.pop("CDI_grid_type", None)
    v.encoding.pop("coordinates", None)
ds.to_netcdf(sys.argv[2], format="NETCDF4")
EOF
  mv "$FIX/nocoord_360.nc.tmp$$" "$FIX/nocoord_360.nc"
  echo "made:   $FIX/nocoord_360.nc"
fi

# SCRIP weights for the remap,<grid>,<weights.nc> rows.
if [[ -e weights_con_r36x18_r18x9.nc ]]; then
  echo "exists: $FIX/weights_con_r36x18_r18x9.nc"
else
  "$CDO" -s --no_history gencon,r18x9 r36x18_std.nc "weights_con_r36x18_r18x9.nc.tmp$$"
  mv "weights_con_r36x18_r18x9.nc.tmp$$" weights_con_r36x18_r18x9.nc
  echo "made:   $FIX/weights_con_r36x18_r18x9.nc"
fi

# GRIB fixtures: the GRIB files and gribscan's references to them (paths to $FIX in the
# references), then cdo's NetCDF copy of each, which cdo reads in the rows.
if [[ -e grb_ll.json && -e grb_gg.json && -e grb_rgg.json ]]; then
  echo "exists: $FIX/grb_{ll,gg,rgg}.{grb,json}"
elif [[ ! -x $GRIBSCAN_PYTHON ]]; then
  echo "skip:   grb_* (no GRIBSCAN_PYTHON=$GRIBSCAN_PYTHON)"
else
  "$GRIBSCAN_PYTHON" -I "$HERE/make_grib_fixtures.py" "$FIX"
  echo "made:   $FIX/grb_{ll,gg,rgg}.{grb,index,json}"
fi
for g in grb_ll grb_gg grb_rgg; do
  [[ -e $g.grb ]] && nc "$g" -b F64 copy "$g.grb"
done

# Region catalog for CDORS_REGIONS (Natural Earth v5.1.2 and nereus' basins/moc, unsimplified;
# tools/export_regions.py in the aqua environment, regionmask's downloads cached under /work),
# and cdo region files for the selregion/maskregion rows: cdo_two.txt (a box across 0 deg with
# negative longitudes, then an open triangle), cdo_one.txt (a polygon across the date line with
# vertices on cell centres), cdo_none.txt (a triangle without any cell centre of the fixtures).
REGIONS_PYTHON=${REGIONS_PYTHON:-/work/ab0995/a270088/mambaforge/envs/aqua/bin/python}
if [[ -e regions/ne.ocean_basins_50.geojson && -e regions/moc.geojson ]]; then
  echo "exists: $FIX/regions/*.geojson"
elif [[ ! -x $REGIONS_PYTHON ]]; then
  echo "skip:   regions/ (no REGIONS_PYTHON=$REGIONS_PYTHON)"
else
  "$REGIONS_PYTHON" -I "$HERE/../tools/export_regions.py" --no-embedded "$FIX/regions"
  echo "made:   $FIX/regions/*.geojson"
fi
mkdir -p regions
region_file() {  # region_file NAME: the lines on stdin, written atomically unless it exists
  if [[ -e regions/$1 ]]; then echo "exists: $FIX/regions/$1"; cat > /dev/null; return; fi
  cat > "regions/$1.tmp$$" && mv "regions/$1.tmp$$" "regions/$1" && echo "made:   $FIX/regions/$1"
}
region_file cdo_two.txt <<'TXT'
# two polygons
-30 -20
40 -20
40 30
-30 30
&
100 0
150 0
150 50
TXT
region_file cdo_one.txt <<'TXT'
# one polygon across the date line, with vertices on cell centres
150 -40
200 -40
220 10
170 30
TXT
region_file cdo_none.txt <<'TXT'
0.1 0.1
0.2 0.1
0.2 0.2
TXT

# sampling: stations (a header line, a comment, points away from cell boundaries of r36x18 and
# from equal distances on hpz2; one near the north pole, one west of the date line) and a path
mkdir -p samples
sample_file() {  # sample_file NAME: the lines on stdin, written atomically unless it exists
  if [[ -e samples/$1 ]]; then echo "exists: $FIX/samples/$1"; cat > /dev/null; return; fi
  cat > "samples/$1.tmp$$" && mv "samples/$1.tmp$$" "samples/$1" && echo "made:   $FIX/samples/$1"
}
sample_file stations.csv <<'TXT'
name,lon,lat
# a comment
Drake,-63.3,-58.6
South Atlantic,-21.7,-38.2
Fram Strait,2.6,78.9
Near pole,123.4,88.7
Date line,-178.9,12.3
TXT
sample_file drake.geojson <<'TXT'
{"type": "FeatureCollection", "features": [{"type": "Feature", "properties": {"name": "Drake"},
 "geometry": {"type": "LineString", "coordinates": [[-65.0, -55.0], [-62.0, -60.0], [-60.0, -65.0]]}}]}
TXT
