#!/bin/bash
# cdors round 1, Task 1 (day-1 gate): nereus baseline vs cdors read probe on raw FESOM2 output
# (AWI-CM3 TCO319L137-DART hist1950d, dart mesh; facts in docs/unstructured-baseline.md).
#
#   bench/unstructured/baseline.sh smoke    login node: two steps of temp, 30 steps of a_ice
#                                           (~1 GB read per tool, 8 threads, < 1 min with a mesh cache)
#   sbatch bench/unstructured/baseline.sh   compute node: the measurement (below), 17 min (job 28228738), limit 45 min
#   # the same as a foreground run:
#   # srun -A ab0995 -p compute -N 1 --exclusive -c 128 --hint=nomultithread --mem=0 -t 00:45:00 \
#   #     bench/unstructured/baseline.sh node
#
# Node run, N=3 years per set, cold sets disjoint so that no tool reads the other's cached pages:
#   1 nereus_mesh       nr.fesom.load_mesh(dart) + Atlantic mask, cold; writes the mesh cache
#   2 cdors_*_A_cold    cdors read probe on 1970-1972 (temp 0-700 m, a_ice)
#   3 nereus_file_B     nereus, file chunks (chunks={}), 1980-1982, cold
#   4 nereus_nod2_C     nereus, whole-field chunks along nod2, 1990-1992, cold
#   5 warm reruns:      cdors on A, cdors on C (pages cached by 4), nereus nod2 on C, nereus file on B
# The cdors probe is `fldmean` (equal weights: today's cdors has no FESOM mesh, so the values are
# meaningless) over the same bytes nereus decodes: temp layers 0-700 m (36 of 79) and all of a_ice.
#
# Output (a fresh directory, nothing is overwritten or deleted):
#   node:  /scratch/a/a270088/cdors-bench/unst-baseline-<jobid>/
#   smoke: /work/ab0995/a270088/cdors-target-r1-t1/smoke-<date>/
#   runs.tsv (name, tool, years, wall_s, max_rss_gb, decoded_gb, gb_per_s, rc), <name>.time
#   (/usr/bin/time -v), <name>.json (nereus phases), <name>.plan.json (cdors --plan), result files.
#
#SBATCH --job-name=cdors-unst-baseline
#SBATCH --partition=compute
#SBATCH --account=ab0995
#SBATCH --nodes=1
#SBATCH --exclusive
#SBATCH --hint=nomultithread
#SBATCH --mem=0
#SBATCH --time=00:45:00
#SBATCH --output=/scratch/a/a270088/cdors-bench/unst-baseline-%j.out
set -uo pipefail          # no -e: one failing run must not stop the others

MODE=${1:-node}
HERE=$(cd "$(dirname "$0")" && pwd)
[ -f "$HERE/baseline.py" ] || HERE=/work/ab0995/a270088/cdors-wt/t1-gate/bench/unstructured  # sbatch spool copy
CDORS=${CDORS:-/work/ab0995/a270088/cdors-target/release/cdors}
PY="/work/ab0995/a270088/mambaforge/envs/nereus/bin/python -I"
D=/work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom
export CDORS_CACHE=${CDORS_CACHE:-/work/ab0995/a270088/cdors-target-r1-t1/cache}
export OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 MKL_NUM_THREADS=1
# layer centres <= 700 m on dart (layers 1-36; interfaces 0..700 m)
LV=2.5,7.5,15,25,35,45,55,65,75,85,95,106,118.5,132.5,147.5,162.5,177.5,192.5,207.5,222.5,240,262.5,287.5,312.5,337.5,362.5,387.5,415,445,475,505,535,565,600,640,680

if [ "$MODE" = smoke ]; then
    OUT=/work/ab0995/a270088/cdors-target-r1-t1/smoke-$(date +%Y%m%d-%H%M%S)
    THREADS=8; NOPT="--steps 2"; IOPT="--steps 30"
    CACHE=${MESH_CACHE:-/work/ab0995/a270088/cdors-target-r1-t1/dart_mesh_cache.npz}
else
    [ -n "${SLURM_JOB_ID:-}" ] || { echo "node mode runs inside Slurm only (use: $0 smoke)" >&2; exit 1; }
    OUT=/scratch/a/a270088/cdors-bench/unst-baseline-$SLURM_JOB_ID
    THREADS=128; NOPT=""; IOPT=""
    CACHE=$OUT/dart_mesh_cache.npz
fi
[ -e "$OUT" ] && { echo "refusing to reuse $OUT" >&2; exit 1; }
mkdir -p "$OUT"
TSV=$OUT/runs.tsv
printf "name\ttool\tyears\twall_s\tmax_rss_gb\tdecoded_gb\tgb_per_s\trc\n" > "$TSV"
{ echo "host $(hostname) mode $MODE threads $THREADS"; date; "$CDORS" --version; } > "$OUT/env.txt"

files() {  # files VAR Y1 Y2
    local y; for y in $(seq "$2" "$3"); do printf '%s ' "$D/$1.fesom.$y.nc"; done
}

timed() {  # timed NAME TOOL YEARS DECODED_BYTES cmd...
    local name=$1 tool=$2 years=$3 bytes=$4; shift 4
    /usr/bin/time -v -o "$OUT/$name.time" "$@" > "$OUT/$name.stdout" 2> "$OUT/$name.stderr"
    local rc=$?
    local wall rss
    wall=$(awk -F': ' '/Elapsed \(wall clock\)/{n=split($2,a,":"); s=0; for(i=1;i<=n;i++) s=s*60+a[i]; print s}' "$OUT/$name.time")
    rss=$(awk -F': ' '/Maximum resident/{printf "%.3f", $2/1e6}' "$OUT/$name.time")
    awk -v n="$name" -v t="$tool" -v y="$years" -v w="$wall" -v r="$rss" -v b="$bytes" -v rc="$rc" \
        'BEGIN{gb=b/1e9; printf "%s\t%s\t%s\t%.2f\t%s\t%.3f\t%.3f\t%d\n", n,t,y,w,r,gb,(w>0?gb/w:0),rc}' >> "$TSV"
    tail -1 "$TSV"
}

cdors_probe() {  # cdors_probe NAME Y1 Y2
    local name=$1 y1=$2 y2=$3 sel_t="" sel_i="" b
    if [ "$MODE" = smoke ]; then sel_t="-seltimestep,1/2"; sel_i="-seltimestep,1/30"; fi
    # shellcheck disable=SC2046
    for v in temp a_ice; do
        if [ $v = temp ]; then chain="-fldmean -sellevel,$LV $sel_t -selname,temp"
        else chain="-fldmean $sel_i -selname,a_ice"; fi
        "$CDORS" -w --plan --json -P "$THREADS" $chain -cat $(files $v "$y1" "$y2") \
            "$OUT/${name}_$v.nc" > "$OUT/${name}_$v.plan.json" 2>&1
        b=$(/usr/bin/jq -r '.totals.bytes_decoded' "$OUT/${name}_$v.plan.json")
        timed "${name}_$v" cdors "$y1-$y2" "$b" \
            "$CDORS" -O -w -P "$THREADS" $chain -cat $(files $v "$y1" "$y2") "$OUT/${name}_$v.nc"
    done
}

nereus_run() {  # nereus_run NAME Y1 Y2 CHUNKS [extra args]
    local name=$1 y1=$2 y2=$3 ch=$4; shift 4
    mkdir -p "$OUT/$name"
    timed "$name" nereus "$y1-$y2" 0 \
        $PY "$HERE/baseline.py" --years "$y1-$y2" --out "$OUT/$name" --workers "$THREADS" \
        --chunks "$ch" --mesh-cache "$CACHE" "$@"
    cp "$OUT/$name/nereus_timing.json" "$OUT/$name.json" 2>/dev/null
    # decoded bytes come from the script's own record: fill them into the last runs.tsv row
    local gb
    gb=$(/usr/bin/jq -r '.decoded_gb' "$OUT/$name.json" 2>/dev/null || echo 0)
    awk -F'\t' -v OFS='\t' -v gb="${gb:-0}" -v last="$(wc -l < "$TSV")" \
        'NR==last{$6=sprintf("%.3f",gb); $7=sprintf("%.3f",($4>0?gb/$4:0))} {print}' "$TSV" > "$TSV.tmp" \
        && mv "$TSV.tmp" "$TSV"
    /usr/bin/jq -c '{wall_s, decoded_gb, compute_gb_per_s, peak_rss_gb}' "$OUT/$name.json" 2>/dev/null
}

if [ "$MODE" = smoke ]; then
    cdors_probe cdors_smoke 1975 1975
    nereus_run nereus_smoke_file 1975 1975 file $NOPT --what ohc
    nereus_run nereus_smoke_ice 1975 1975 nod2 $IOPT --what ice
else
    nereus_run nereus_mesh 1975 1975 file --what ""          # cold mesh load + mask, writes $CACHE
    cdors_probe cdors_A_cold 1970 1972
    nereus_run nereus_file_B_cold 1980 1982 file
    nereus_run nereus_nod2_C_cold 1990 1992 nod2
    cdors_probe cdors_A_warm 1970 1972
    cdors_probe cdors_C_warm 1990 1992
    nereus_run nereus_nod2_C_warm 1990 1992 nod2
    nereus_run nereus_file_B_warm 1980 1982 file
fi
echo "done: $OUT"; date
