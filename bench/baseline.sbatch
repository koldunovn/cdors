#!/bin/bash
# cdors Task 2 baseline: cdo 2.6.0 on workloads W1-W4, cdo read-time shares (-P 1 -T), and the
# zarrs read probe on W1 (Lustre) and W2 (EERIE cloud). Datasets: bench/datasets.md.
#
# DO NOT SUBMIT without Nikolay's explicit go-ahead. Estimate: one exclusive node, expected
# ~0.75 h, limit 1.5 h  =>  at most 1.5 node-hours (see docs/baseline.md).
#
#   sbatch /home/a/a270088/cdo/bench/baseline.sbatch
#
# Outputs: /scratch/a/a270088/cdors-bench/baseline-<jobid>/ (summary.tsv, *.log, *.time, *.nc)
#
#SBATCH --job-name=cdors-baseline
#SBATCH --partition=compute
#SBATCH --account=ab0995
#SBATCH --nodes=1
#SBATCH --exclusive
#SBATCH --mem=0
#SBATCH --time=01:30:00
#SBATCH --output=/scratch/a/a270088/cdors-bench/baseline-%j.out

source /home/a/a270088/cdo/env.sh
set -uo pipefail          # no -e: one failing workload must not stop the others

CDO=${CDO:-/sw/spack-levante/cdo-2.6.0-akkxhz/bin/cdo}
PY=/work/ab0995/a270088/mambaforge/bin/python
BENCH=/home/a/a270088/cdo/bench
ROOT=/scratch/a/a270088/cdors-bench
VIEWS=$ROOT/views
OUT=$ROOT/baseline-${SLURM_JOB_ID:-manual}
PROBE=$CARGO_TARGET_DIR/release/examples/read_probe
mkdir -p "$OUT" "$VIEWS"
SUMMARY=$OUT/summary.tsv
printf "name\trc\twall_s\tuser_s\tsys_s\tmaxrss_GB\tcdo_read_s\tcdo_total_s\n" > "$SUMMARY"

zarr() { echo "file://$1#mode=zarr,file"; }   # NCZarr URL for cdo

# run NAME CMD... : /usr/bin/time -v around CMD, stdout+stderr to NAME.log, one summary row.
run() {
    local name=$1; shift
    echo "== $(date +%T) $name: $*"
    /usr/bin/time -v -o "$OUT/$name.time" "$@" > "$OUT/$name.log" 2>&1
    local rc=$?
    local wall user sys rss rd tot
    wall=$(awk -F': ' '/Elapsed/{n=split($2,a,":"); s=0; for(i=1;i<=n;i++) s=s*60+a[i]; print s}' "$OUT/$name.time")
    user=$(awk -F': ' '/User time/{print $2}' "$OUT/$name.time")
    sys=$(awk -F': ' '/System time/{print $2}' "$OUT/$name.time")
    rss=$(awk -F': ' '/Maximum resident/{printf "%.2f", $2/1048576}' "$OUT/$name.time")
    # cdo -T table: "    read   <calls> <min> <avg> <max> <total>"
    rd=$(awk '$1=="read" && NF==6 {print $6}' "$OUT/$name.log" | tail -1)
    tot=$(awk '$1=="total" && NF==6 {print $6}' "$OUT/$name.log" | tail -1)
    printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$name" "$rc" "$wall" "$user" "$sys" "$rss" "${rd:--}" "${tot:--}" >> "$SUMMARY"
    tail -n 1 "$SUMMARY"
}

# ---------------------------------------------------------------- environment record
{
    date; hostname; echo "SLURM_JOB_ID=${SLURM_JOB_ID:-}"
    lscpu | grep -E 'Model name|^CPU\(s\)|Thread|Socket'
    free -g
    "$CDO" --version 2>&1 | head -3
    ls -l "$PROBE" 2>&1
} > "$OUT/env.txt" 2>&1

# ---------------------------------------------------------------- inputs
SRC1=/work/kd1453/rechunked_ngc4008/ngc4008_P1D_9.zarr
SRC4=/work/kd1453/rechunked_ngc4008/ngc4008_PT3H_9.zarr
W1D1=$VIEWS/ngc4008_P1D_9_tas_10y.zarr          # 2020-01-02 .. 2029-12-31, 3652 days
W1D2=$VIEWS/ngc4008_P1D_9_tas_10y_2030.zarr     # 2030-01-09 .. 2040-01-08, 3652 days (cold read for the probe)
W4V=$VIEWS/ngc4008_PT3H_9_tas_1y.zarr           # 2020, 2928 3-hourly steps (1/30 of W4)
[ -d "$W1D1" ] || $PY -I $BENCH/make_view.py $SRC1 $W1D1 tas 3652
[ -d "$W1D2" ] || $PY -I $BENCH/make_view.py $SRC1 $W1D2 tas 3652 3660
[ -d "$W4V" ]  || $PY -I $BENCH/make_view.py $SRC4 $W4V tas 2928

ICON=/work/bm1344/k202193/ICON/erc2002/postprocessing/interpolation/control_1950
W2FILES=( $ICON/atm_2d_1d_mean_remap025/run_199[1-9]*/*.nc $ICON/atm_2d_1d_mean_remap025/run_2000*/*.nc )  # 240 files, 3653 days
W3FILES=( $ICON/oce_2d_1d_mean_remap025/run_199[1-5]*/*.nc )                                            # 120 files, 1826 days
# for the -T runs: the 30-day file of the first months of a year outside the timed ranges (cold)
W2T=( $(for d in $ICON/atm_2d_1d_mean_remap025/run_2001*; do ls $d/*.nc | head -1; done | head -4) )
W3T=( $(for d in $ICON/oce_2d_1d_mean_remap025/run_1996*; do ls $d/*.nc | head -1; done | head -3) )
W2URL=https://eerie.cloud.dkrz.de/datasets/icon-esm-er.eerie-control-1950.v20240618.atmos.gr025.2d_daily_mean/kerchunk
BOX=-30,40,30,75

# ---------------------------------------------------------------- W1: yearmean, HEALPix z9 Zarr (46 GB decoded per decade)
# cold first: cdo on decade 1 (single operator, -P 1 -T), probe on decade 2; then warm repeats
run w1_cdo_yearmean_P1_T_cold    "$CDO" -O -f nc4 -P 1 -T yearmean "$(zarr $W1D1)" "$OUT/w1_yearmean.nc"
run w1_probe_D2_cold_t128        "$PROBE" "$W1D2" tas 128
run w1_cdo_yearmean_P16_warm     "$CDO" -O -f nc4 -P 16 yearmean "$(zarr $W1D1)" "$OUT/w1_yearmean_P16.nc"
run w1_cdo_fldmean_P1_T_warm     "$CDO" -O -f nc4 -P 1 -T fldmean "$(zarr $W1D1)" "$OUT/w1_fldmean.nc"
for t in 1 16 64 128; do
    run w1_probe_D1_warm_t$t     "$PROBE" "$W1D1" tas $t
done

# ---------------------------------------------------------------- W4 (reduced to 1 year of 3-hourly z9, 36.9 GB decoded)
run w4_cdo_ydaymean_P1_T_cold    "$CDO" -O -f nc4 -P 1 -T ydaymean "$(zarr $W4V)" "$OUT/w4_ydaymean.nc"
run w4_cdo_timpctl95_P16_warm    "$CDO" -O -f nc4 -P 16 timpctl,95 "$(zarr $W4V)" -timmin "$(zarr $W4V)" -timmax "$(zarr $W4V)" "$OUT/w4_timpctl95.nc"

# ---------------------------------------------------------------- W2: fldmean of a box, raw NetCDF-4 (blosc) behind kerchunk, one decade
run w2_cdo_fldmean_box_P16       "$CDO" -O -P 16 fldmean -sellonlatbox,$BOX -select,name=pr "${W2FILES[@]}" "$OUT/w2_fldmean_box.nc"
for f in "${W2T[@]}"; do         # read share: single operator on whole raw files (all 2D variables)
    run w2_cdo_T_fldmean_$(basename "$f" .nc | grep -o '[0-9]\{8\}T') "$CDO" -O -P 1 -T fldmean "$f" "$OUT/w2_T_$(basename "$f")"
done
run w2_probe_cloud_t64           timeout 30m "$PROBE" "$W2URL" pr 64

# ---------------------------------------------------------------- W3: remapbil of daily SST (0.25 deg regular -> r360x180), 5 years
run w3_cdo_genbil                "$CDO" -O -P 1 genbil,r360x180 -seltimestep,1 -select,name=to "${W3FILES[0]}" "$OUT/w3_weights_bil_r360x180.nc"
run w3_cdo_remapbil_P16          "$CDO" -O -P 16 remapbil,r360x180 -select,name=to "${W3FILES[@]}" "$OUT/w3_remapbil.nc"
run w3_cdo_remap_cached_P16_warm "$CDO" -O -P 16 remap,r360x180,"$OUT/w3_weights_bil_r360x180.nc" -select,name=to "${W3FILES[@]}" "$OUT/w3_remap_cached.nc"
for f in "${W3T[@]}"; do         # read share: single operator on whole raw files (all 2D variables)
    run w3_cdo_T_remapbil_$(basename "$f" .nc | grep -o '[0-9]\{8\}T') "$CDO" -O -P 1 -T remapbil,r360x180 "$f" "$OUT/w3_T_$(basename "$f")"
done

echo "== $(date +%T) done"
column -t -s $'\t' "$SUMMARY"
