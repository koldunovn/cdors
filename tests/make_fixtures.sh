#!/usr/bin/env bash
# Generate the tiny test fixtures used by tests/run_cases.sh.
# Idempotent: a fixture that already exists is skipped; nothing is ever deleted or overwritten.
#
#   r36x18_std    36x18 regular grid, standard calendar, 1096 daily steps (2000-2002), tas + pr
#   r36x18_2lev   same, tas on two pressure levels (1000 and 850 hPa)
#   hpz2_noleap   HEALPix zoom 2 (nside 4, 192 cells), noleap (cdo writes "365_day") calendar, 1095 steps, with missing values
#   unst_360      20x18 regular grid turned unstructured (360 cells), 360_day calendar, 1080 steps
#   hpz2_noleap.zarr2 / .zarr3   xarray copies of hpz2_noleap (Zarr v2 / v3), time chunks of 73
#   hpz2_noleap_tiny.zarr2       tiny chunks (10 steps x 16 cells) for the planner check
#   weights_con_r36x18_r18x9.nc  SCRIP weights of cdo gencon,r18x9 for r36x18_std (remap,<grid>,<weights> rows)
#   nocoord_360   unst_360 without horizontal coordinates (FESOM-like; written by xarray)
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

# Two pressure levels of tas: merge two single-level chains with different z-axes.
printf 'zaxistype = pressure\nsize = 1\nlevels = 100000\n' > zaxis_p100000.txt
printf 'zaxistype = pressure\nsize = 1\nlevels = 85000\n'  > zaxis_p85000.txt
nc r36x18_2lev -merge \
  -setzaxis,zaxis_p100000.txt -selname,tas $(chain r36x18 standard 1096 '' deg) \
  -setzaxis,zaxis_p85000.txt -subc,15 -selname,tas $(chain r36x18 standard 1096 '' deg)

# Zarr copies of hpz2_noleap, written by xarray (real third-party Zarr, not our own writer).
zarr() {  # zarr NAME FORMAT TIMECHUNK CELLCHUNK
  local name=$1
  if [[ -e $name ]]; then echo "exists: $FIX/$name"; return; fi
  if [[ ! -x $ZARR_PYTHON ]]; then echo "skip:   $name (no ZARR_PYTHON=$ZARR_PYTHON)"; return; fi
  "$ZARR_PYTHON" -I - "$FIX/hpz2_noleap.nc" "$FIX/$name.tmp$$" "$2" "$3" "$4" <<'EOF'
import sys, xarray as xr, zarr
src, dst, fmt, tch, cch = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
ds = xr.open_dataset(src, decode_times=False)   # keep time as numbers + units/calendar attrs
enc = {}
for v in ds.data_vars:
    if ds[v].ndim == 2:
        ds[v].encoding.pop("chunksizes", None); ds[v].encoding.pop("contiguous", None)
        enc[v] = {"chunks": (tch, cch)}
ds.to_zarr(dst, mode="w-", zarr_format=fmt, encoding=enc, consolidated=(fmt == 2))
EOF
  mv "$FIX/$name.tmp$$" "$FIX/$name"
  echo "made:   $FIX/$name"
}
zarr hpz2_noleap.zarr2      2 73 192
zarr hpz2_noleap.zarr3      3 73 192
zarr hpz2_noleap_tiny.zarr2 2 10 16

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
