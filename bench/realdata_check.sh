#!/usr/bin/env bash
# Real-data check of cdors against cdo (and numpy/xarray) on small slices of Levante datasets.
# Report only: prints one status row per case (identical / within tolerance / DIFFERENT / error /
# skipped) with timings, and never deletes or overwrites anything.
#
#   bench/realdata_check.sh [--list] [--only id1,id2] [--skip-remote] [--cdo-threads N]
#
# Environment (all optional):
#   CDORS           binary under test   (default: $CARGO_TARGET_DIR/release/cdors, after env.sh)
#   CDO             reference cdo       (default: spack cdo 2.6.0, as in env.sh)
#   PYTHON          python with numpy, netCDF4, xarray, zarr (default: the hk25 mamba env)
#   REALDATA_ROOT   parent of the run directories (default: /scratch/a/a270088/cdors-realdata)
#   REALDATA_CACHE  CDORS_CACHE for the cdors runs (default: <run dir>/cache, so remap weights are
#                   generated afresh; point it at an existing cache to reuse weights)
#
# Every run writes into a fresh directory $REALDATA_ROOT/<YYYYmmdd-HHMMSS>-<pid>/: one
# subdirectory per case (cdors and cdo outputs, logs, /usr/bin/time records), summary.txt and
# summary.json. The cases and their tolerances are listed in bench/realdata/run.py.
# Login node: every case reads a small slice (seconds to a minute); the whole sweep takes a few
# minutes.
set -uo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(dirname "$HERE")
if [[ -z ${CARGO_TARGET_DIR:-} && -f $ROOT/env.sh ]]; then
  # shellcheck source=/dev/null
  source "$ROOT/env.sh"
fi
export CDORS=${CDORS:-${CARGO_TARGET_DIR:-/work/ab0995/a270088/cdors-target}/release/cdors}
export CDO=${CDO:-/sw/spack-levante/cdo-2.6.0-akkxhz/bin/cdo}
PYTHON=${PYTHON:-/work/ab0995/a270088/mambaforge/envs/hk25/bin/python}
REALDATA_ROOT=${REALDATA_ROOT:-/scratch/a/a270088/cdors-realdata}

for a in "$@"; do
  if [[ $a == --list ]]; then
    exec "$PYTHON" -I "$HERE/realdata/run.py" --run-dir /nonexistent --list
  fi
done

if [[ ! -x $CDORS ]]; then
  echo "realdata_check: cdors binary not found: $CDORS (build it, or set CDORS)" >&2
  exit 1
fi
mkdir -p "$REALDATA_ROOT" || exit 1
RUN="$REALDATA_ROOT/$(date +%Y%m%d-%H%M%S)-$$"
if [[ -e $RUN ]]; then
  echo "realdata_check: $RUN exists; not touching it" >&2
  exit 1
fi
export CDORS_CACHE=${REALDATA_CACHE:-$RUN/cache}
# pyproj in the python env warns about its data directory on import; harmless
exec "$PYTHON" -I "$HERE/realdata/run.py" --run-dir "$RUN" "$@" 2> >(grep -v -i -e pyproj -e ca_bundle >&2)
