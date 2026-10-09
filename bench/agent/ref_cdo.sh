#!/bin/bash
# Agent check: cdo 2.6.0 cross-checks of the reference answers (independent of cdors).
# Usage: bench/agent/ref_cdo.sh T1|T2|T3|T5 [workdir]
# Login node, each part < 5 min, -P 8. Writes only into the work directory
# (default /scratch/a/a270088/cdors-agentcheck/prep); deletes nothing.
set -euo pipefail
CDO=${CDO:-/sw/spack-levante/cdo-2.6.0-akkxhz/bin/cdo}
WORK=${2:-/scratch/a/a270088/cdors-agentcheck/prep}
VIEWS=/scratch/a/a270088/cdors-bench/views
ATM=/work/bm1344/k202193/ICON/erc2002/postprocessing/interpolation/control_1950/atm_2d_1d_mean_remap025
OCE=/work/bm1344/k202193/ICON/erc2002/postprocessing/interpolation/control_1950/oce_2d_1d_mean_remap025
mkdir -p "$WORK"
cd "$WORK"
c() { local a="$*"; echo "+ cdo ${a:0:240}$([ ${#a} -gt 240 ] && echo " ...")" >&2; /usr/bin/time -f "  [wall %e s, max rss %M kB]" "$CDO" -s --no_history "$@"; }

case "$1" in
T1)
  # decade view 2020-01-02..2029-12-31 (single-variable symlink view, bench/make_view.py);
  # July grouped by stored timestamp; HEALPix cells are equal-area, cdo's fldmean weights equally
  c -P 8 -outputf,%.6f -fldmean -timmean -selmon,7 -selyear,2020/2024 \
    "file://$VIEWS/ngc4008_P1D_9_tas_10y.zarr#mode=zarr,file"
  ;;
T2)
  # Raw files of model years 1991-1995 hold steps 0..1825 of the reference axis (1950-01-01 ..
  # 1954-12-31), step for step; the raw directories (model years, 1992 leap) do NOT coincide with
  # the reference years (1952 leap), so years are cut by step index: 1950 = steps 1-365,
  # 1951 = 366-730, 1952 = 731-1096, 1953 = 1097-1461, 1954 = 1462-1826 (cdo counts from 1).
  # Daily box means first (one pass over 6.6 GB), then yearly means of those in awk (no missing
  # values in pr, so the mean of daily box means equals the box mean of the time mean).
  # Box edges included (cdo sellonlatbox includes boundary points).
  files=$(ls $ATM/run_199[1-5]??01T000000-*/*.nc | sort)
  c -P 8 -outputf,%.10g -mulc,86400 -fldmean -sellonlatbox,-80,0,20,70 -select,name=pr [ $files ] > t2_daily_boxmean.txt
  awk 'BEGIN{split("365 365 366 365 365", n, " ")} {v[NR]=$1}
       END{ if (NR != 1826) { print "expected 1826 steps, got " NR; exit 1 }
            i=0; for (y=1; y<=5; y++) { s=0; for (k=0; k<n[y]; k++) s+=v[++i];
            printf "%d %.6f mm/day\n", 1949+y, s/n[y] } }' t2_daily_boxmean.txt
  ;;
T3)
  # model January 1991 = reference January 1950: directory run_19910101T000000-* (31 daily means)
  files=$(ls $OCE/run_19910101T000000-*/*.nc | sort)
  c -P 8 -f nc4 -O -timmean -select,name=to [ $files ] t3_to_timmean_src.nc
  c -P 8 -O -remapbil,r360x180 t3_to_timmean_src.nc t3_to_timmean_r360x180.nc
  echo "remapbil then fldmean:"; c -outputf,%.6f -fldmean t3_to_timmean_r360x180.nc
  echo "fldmean on the source grid (for comparison):"; c -outputf,%.6f -fldmean t3_to_timmean_src.nc
  # order check: remap each day first, then the time mean (same weights, same mask -> same result)
  c -P 8 -outputf,%.6f -fldmean -timmean -remapbil,r360x180 -select,name=to [ $files ]
  ;;
T5)
  # same dataset as T2, raw files, reference January 1950 = model January 1991
  files=$(ls $ATM/run_19910101T000000-*/*.nc | sort)
  c -P 8 -outputf,%.6f -mulc,86400 -fldmean -timmean -sellonlatbox,-10,40,35,70 -select,name=pr [ $files ]
  ;;
*) echo "usage: $0 T1|T2|T3|T5 [workdir]" >&2; exit 2 ;;
esac
