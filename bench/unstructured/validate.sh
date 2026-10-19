#!/bin/bash
# cdors round 1, Task 13: real-data validation and speed of the round-1 mesh operators on raw FESOM2
# output (AWI-CM3 TCO319L137-DART hist1950d, dart mesh; dataset facts and pass marks in
# docs/unstructured-baseline.md, results in docs/unstructured-results.md).
#
#   sbatch bench/unstructured/validate.sh    compute node, 128 cores, limit 40 min (job 28230544: 6 min)
#   sbatch -t 25 bench/unstructured/validate.sh v2
#                                            the re-run after the Task 13 fixes (both OHC chains)
#   sbatch -t 15 bench/unstructured/validate.sh extra
#                                            first use of the mesh without a plan before it, and
#                                            timing splits of the OHC chain (job: see the results doc)
#   CDORS=<binary> sbatch -t 25 bench/unstructured/validate.sh v3
#                                            follow-up: the warm rows measured warm (cdors and
#                                            nereus), and where RSS goes under --mem 2G
#   CDORS=<binary> [CDORS2=<binary>] sbatch -t 10 bench/unstructured/validate.sh mem
#                                            the memory runs of v3 alone (and with a second binary)
#
# Same years and cold/warm sets as Task 1 (bench/unstructured/baseline.sh):
#   A = 1970-1972, B = 1980-1982 (cold for cdors on this node), C = 1990-1992 (read probe, cold).
# Runs, in this order (each record: /usr/bin/time -v, --plan --json, output):
#   1 mesh_first_*     a fresh CDORS_CACHE: first use of the dart mesh (mesh.diag -> cache) and of
#                      the Atlantic region, one step of OHC; then the same with the cache warm, and
#                      the read probe of the same step (the overhead of the mesh path)
#   2 *_cold           OHC and ice extent on A and on B (two cold sets; best is reported), the
#                      read probe (Task 1's fldmean chains, equal weights) on C
#   3 *_warm[12]       OHC, ice and the probe on A again, twice each; OHC without the region
#                      (timing split: what maskregion costs)
#   4 *_mem2G          OHC and ice on A under --mem 2G (peak RSS against the budget)
#   5 nereus_ref_A     nereus on A with the mesh's own nod_area (baseline.py --area nod_area):
#                      the reference series; compare.py writes compare.json
# Output (fresh directory, nothing is overwritten or deleted):
#   /scratch/a/a270088/cdors-bench/unst-validate-<jobid>/  runs.tsv (name, years, wall_s,
#   max_rss_gb (1e9 bytes, as --mem counts; jobs up to 28236588 wrote KiB / 1e6, 2.4 % less), data_gb (bytes of the data leaves in --plan), gb_per_s, cpu_pct, user_s, sys_s, rc)
#
#SBATCH --job-name=cdors-unst-validate
#SBATCH --partition=compute
#SBATCH --account=ab0995
#SBATCH --nodes=1
#SBATCH --exclusive
#SBATCH --hint=nomultithread
#SBATCH --mem=0
#SBATCH --time=00:40:00
#SBATCH --output=/scratch/a/a270088/cdors-bench/unst-validate-%j.out
set -uo pipefail          # no -e: one failing run must not stop the others

HERE=$(cd "$(dirname "$0")" && pwd)
[ -f "$HERE/baseline.py" ] || HERE=/home/a/a270088/cdo/bench/unstructured  # sbatch spool copy
CDORS=${CDORS:-/work/ab0995/a270088/cdors-target-r1-t13/release/cdors}
PY="/work/ab0995/a270088/mambaforge/envs/nereus/bin/python -I"
D=/work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom
M=/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart
export CDORS_REGIONS=${CDORS_REGIONS:-/work/ab0995/a270088/cdors-target/fixtures/regions}
export OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 MKL_NUM_THREADS=1
THREADS=128
# layer centres <= 700 m on dart (layers 1-36; interfaces 0..700 m)
LV=2.5,7.5,15,25,35,45,55,65,75,85,95,106,118.5,132.5,147.5,162.5,177.5,192.5,207.5,222.5,240,262.5,287.5,312.5,337.5,362.5,387.5,415,445,475,505,535,565,600,640,680
RHOCP=4089750          # nereus' rho x cp = 1025 x 3990
OHC="-mulc,$RHOCP -volint,0/700 -maskregion,regions=basins,select=Atlantic_Basin -setgrid,fesom:$M -sellevel,$LV -selname,temp"
# the natural chain (master after Task 13: volint,z0/z1 selects its levels itself)
OHC2="-mulc,$RHOCP -volint,0/700 -maskregion,regions=basins,select=Atlantic_Basin -setgrid,fesom:$M -selname,temp"
OHC_NOMASK="-mulc,$RHOCP -volint,0/700 -setgrid,fesom:$M -sellevel,$LV -selname,temp"
ICE="-fldhist,edges=0.15/1 -sellonlatbox,-180,180,0,90 -setgrid,fesom:$M -selname,a_ice"
PROBE_T="-fldmean -sellevel,$LV -selname,temp"
PROBE_I="-fldmean -selname,a_ice"

MODE=${1:-main}
[ -n "${SLURM_JOB_ID:-}" ] || { echo "runs inside Slurm only: sbatch $0 [extra]" >&2; exit 1; }
OUT=/scratch/a/a270088/cdors-bench/unst-validate-$SLURM_JOB_ID
[ -e "$OUT" ] && { echo "refusing to reuse $OUT" >&2; exit 1; }
mkdir -p "$OUT"
export CDORS_CACHE=$OUT/cache          # fresh: run 1 measures the first use of the mesh
TSV=$OUT/runs.tsv
printf "name\tyears\twall_s\tmax_rss_gb\tdata_gb\tgb_per_s\tcpu_pct\tuser_s\tsys_s\trc\n" > "$TSV"
{ echo "host $(hostname) threads $THREADS"; date; "$CDORS" --version; echo "CDORS_REGIONS $CDORS_REGIONS"; } > "$OUT/env.txt"

files() {  # files VAR Y1 Y2
    local y; for y in $(seq "$2" "$3"); do printf '%s ' "$D/$1.fesom.$y.nc"; done
}

timed() {  # timed NAME YEARS DATA_BYTES cmd...
    local name=$1 years=$2 bytes=$3; shift 3
    /usr/bin/time -v -o "$OUT/$name.time" "$@" > "$OUT/$name.stdout" 2> "$OUT/$name.stderr"
    local rc=$?
    awk -F': ' -v n="$name" -v y="$years" -v b="$bytes" -v rc="$rc" '
        /Elapsed \(wall clock\)/ {k=split($2,a,":"); w=0; for(i=1;i<=k;i++) w=w*60+a[i]}
        /Maximum resident/ {r=$2*1024/1e9}
        /Percent of CPU/ {c=$2+0}
        /User time/ {u=$2}
        /System time/ {s=$2}
        END {gb=b/1e9; printf "%s\t%s\t%.2f\t%.3f\t%.3f\t%.3f\t%d\t%.1f\t%.1f\t%d\n", n,y,w,r,gb,(w>0?gb/w:0),c,u,s,rc}' \
        "$OUT/$name.time" >> "$TSV"
    tail -1 "$TSV"
}

run_cdors() {  # run_cdors NAME YEARS "OPTS" "CHAIN" INPUT...   (PLAN_AFTER=1: time the run first)
    local name=$1 years=$2 opts=$3 chain=$4; shift 4
    if [ "${PLAN_AFTER:-0}" = 1 ]; then  # the plan would build the mesh cache before the timed run
        # shellcheck disable=SC2086
        timed "$name" "$years" 0 "$CDORS" -O -w $opts -P "$THREADS" $chain "$@" "$OUT/$name.nc"
        "$CDORS" -w $opts --plan --json -P "$THREADS" $chain "$@" "$OUT/$name.plan.nc" > "$OUT/$name.plan.json" 2> "$OUT/$name.plan.stderr"
        return
    fi
    # shellcheck disable=SC2086
    "$CDORS" -w $opts --plan --json -P "$THREADS" $chain "$@" "$OUT/$name.nc" > "$OUT/$name.plan.json" 2> "$OUT/$name.plan.stderr"
    # bytes of the data leaves (the region mask is an in-memory leaf, counted once per tile)
    local b
    b=$(/usr/bin/jq '[.stages[].variables[]?.leaves[]? | select(.source | startswith("regions:") | not) | .bytes_decoded] | add // 0' \
        "$OUT/$name.plan.json" 2>/dev/null || echo 0)
    # shellcheck disable=SC2086
    timed "$name" "$years" "${b:-0}" "$CDORS" -O -w $opts -P "$THREADS" $chain "$@" "$OUT/$name.nc"
}

lustre() {
    echo "$1 $(lctl get_param -n llite.project-*.max_cached_mb 2>/dev/null | \
        awk '/^used_mb/{u+=$2} /^max_cached_mb/{m=$2} END{print "used_mb", u, "max_mb", m}')" >> "$OUT/cache.txt"
}
rss_run() {  # rss_run NAME "ENV" "OPTS" "CHAIN" INPUT...  (the plan, then the run sampled)
    local name=$1 envs=$2 opts=$3 chain=$4; shift 4
    # shellcheck disable=SC2086
    env $envs "$CDORS" -w $opts --plan --json -P "$THREADS" $chain "$@" "$OUT/$name.nc" \
        > "$OUT/$name.plan.json" 2> "$OUT/$name.plan.stderr"
    local b comm sp
    b=$(/usr/bin/jq '.totals.bytes_decoded' "$OUT/$name.plan.json" 2>/dev/null || echo 0)
    comm=$(basename "$CDORS" | cut -c1-15)
    ( pid=""; for _ in $(seq 500); do pid=$(pgrep -n -x "$comm") && break; sleep 0.01; done
      [ -n "$pid" ] || exit 0
      while [ -r "/proc/$pid/status" ]; do
          awk -v t="$(date +%s.%N)" '/^(VmRSS|RssAnon|RssFile|RssShmem|Threads):/{s=s" "$1$2} END{print t s}' \
              "/proc/$pid/status" 2>/dev/null
          sleep 0.05
      done ) > "$OUT/$name.rss" &
    sp=$!
    # shellcheck disable=SC2086
    timed "$name" 1970-1972 "${b:-0}" env $envs "$CDORS" -O -w $opts -P "$THREADS" $chain "$@" "$OUT/$name.nc"
    wait "$sp"
    { printf '%s env=[%s] opts=[%s] P=%s estimate=%s ' "$name" "$envs" "$opts" "$THREADS" \
          "$(/usr/bin/jq '.memory.peak_bytes_estimate' "$OUT/$name.plan.json" 2>/dev/null)"
      awk '{for(i=2;i<=NF;i++){split($i,a,":"); if(a[2]+0>m[a[1]]) m[a[1]]=a[2]+0}}
           END{for(k in m) printf "%s_max=%d ", k, m[k]; print ""}' "$OUT/$name.rss"; } | tee -a "$OUT/rss_summary.txt"
}

S1="$D/temp.fesom.1975.nc"
if [ "$MODE" = v2 ]; then
    # re-run after the Task 13 fixes (two-fold volint, z0/z1 push-down, maskregion flag node, mesh
    # memory in --mem): ohc2_* = the natural chain without -sellevel, ohc_* = the old chain.
    # Cold sets: A (ohc2, ice), B (ohc, ice), C (ohc2), D = 2000-2002 (read probe).
    # Values are compared with job 28230544's nereus reference on A (same data and areas).
    PLAN_AFTER=1 run_cdors mesh_first_ohc2_1 1975 "" "${OHC2/-selname,temp/-seltimestep,1 -selname,temp}" "$S1"
    "$CDORS" --json mesh info "fesom:$M" > "$OUT/mesh_info.json" 2>&1
    /usr/bin/du -sb "$CDORS_CACHE"/* > "$OUT/cache_sizes.txt" 2>&1
    run_cdors mesh_cached_ohc2_1 1975 "" "${OHC2/-selname,temp/-seltimestep,1 -selname,temp}" "$S1"
    run_cdors probe_ohc1 1975 "" "${PROBE_T/-selname,temp/-seltimestep,1 -selname,temp}" "$S1"
    run_cdors ohc2_A_cold 1970-1972 "" "$OHC2" -cat $(files temp 1970 1972)
    run_cdors ice_A_cold 1970-1972 "" "$ICE" -cat $(files a_ice 1970 1972)
    run_cdors ohc_B_cold 1980-1982 "" "$OHC" -cat $(files temp 1980 1982)
    run_cdors ice_B_cold 1980-1982 "" "$ICE" -cat $(files a_ice 1980 1982)
    run_cdors ohc2_C_cold 1990-1992 "" "$OHC2" -cat $(files temp 1990 1992)
    run_cdors probe_temp_D_cold 2000-2002 "" "$PROBE_T" -cat $(files temp 2000 2002)
    run_cdors probe_ice_D_cold 2000-2002 "" "$PROBE_I" -cat $(files a_ice 2000 2002)
    for i in 1 2; do
        run_cdors ohc2_A_warm$i 1970-1972 "" "$OHC2" -cat $(files temp 1970 1972)
        run_cdors ohc_A_warm$i 1970-1972 "" "$OHC" -cat $(files temp 1970 1972)
        run_cdors ice_A_warm$i 1970-1972 "" "$ICE" -cat $(files a_ice 1970 1972)
        run_cdors probe_temp_A_warm$i 1970-1972 "" "$PROBE_T" -cat $(files temp 1970 1972)
        run_cdors probe_ice_A_warm$i 1970-1972 "" "$PROBE_I" -cat $(files a_ice 1970 1972)
    done
    run_cdors ohc2_A_mem2G 1970-1972 "--mem 2G" "$OHC2" -cat $(files temp 1970 1972)
    run_cdors ohc_A_mem2G 1970-1972 "--mem 2G" "$OHC" -cat $(files temp 1970 1972)
    run_cdors ice_A_mem2G 1970-1972 "--mem 2G" "$ICE" -cat $(files a_ice 1970 1972)
    $PY "$HERE/compare.py" "$OUT" /scratch/a/a270088/cdors-bench/unst-validate-28230544/nereus_ref_A \
        > "$OUT/compare.stdout" 2>&1
    grep -v -i -e pyproj -e ca_bundle "$OUT/compare.stdout"
    echo "done: $OUT"; date
    exit 0
fi
if [ "$MODE" = v3 ]; then
    # follow-up to job 28231368. (1) Warm rows measured warm: the Lustre client keeps at most 24 GB
    # of file data, so between the repeats only temp 1970-1972 is read (16.4 GB of 36 layers), by
    # cdors and by nereus (OHC only, file chunks, the Task 1 mesh cache: Task 1's warm row);
    # llite used_mb is logged around them. (2) Where RSS goes under --mem 2G: RssAnon/RssFile of
    # the cdors process sampled every 50 ms, in runs that vary the allocator, threads and read path.
    NPZ=/scratch/a/a270088/cdors-bench/unst-baseline-28228738/dart_mesh_cache.npz
    T_A=$(files temp 1970 1972)
    "$CDORS" --json mesh info "fesom:$M" > "$OUT/mesh_info.json" 2>&1     # builds the mesh cache, untimed
    lustre start
    run_cdors ohc2_A_cold 1970-1972 "" "$OHC2" -cat $T_A
    lustre after_cold
    for i in 1 2 3; do run_cdors ohc2_A_warm$i 1970-1972 "" "$OHC2" -cat $T_A; done
    for i in 1 2; do
        run_cdors ohc_A_warm$i 1970-1972 "" "$OHC" -cat $T_A
        run_cdors probe_temp_A_warm$i 1970-1972 "" "$PROBE_T" -cat $T_A
    done
    lustre after_cdors_warm
    for i in 1 2; do
        mkdir -p "$OUT/nereus_ohc_file_A_warm$i"
        timed nereus_ohc_file_A_warm$i 1970-1972 0 \
            $PY "$HERE/baseline.py" --years 1970-1972 --out "$OUT/nereus_ohc_file_A_warm$i" --workers "$THREADS" \
            --chunks file --mesh-cache "$NPZ" --what ohc
        /usr/bin/jq -c '{wall_s, decoded_gb, peak_rss_gb}' "$OUT/nereus_ohc_file_A_warm$i/nereus_timing.json" 2>/dev/null
    done
    lustre after_nereus
    run_cdors ohc2_A_warm4 1970-1972 "" "$OHC2" -cat $T_A
    lustre after_warm4
fi
if [ "$MODE" = v3 ] || [ "$MODE" = mem ]; then
    # memory under --mem 2G (data warm; `mem` alone: one untimed pass first, and CDORS2 = a second
    # binary run the same way, e.g. the build before a change)
    if [ "$MODE" = mem ]; then
        NPZ=; T_A=$(files temp 1970 1972)
        "$CDORS" --json mesh info "fesom:$M" > "$OUT/mesh_info.json" 2>&1
        run_cdors warmup_ohc2_A 1970-1972 "" "$OHC2" -cat $T_A
    fi
    rss_run mem_ohc2 "" "--mem 2G" "$OHC2" -cat $T_A
    rss_run mem_ohc2_arena1 "MALLOC_ARENA_MAX=1" "--mem 2G" "$OHC2" -cat $T_A
    rss_run mem_ohc2_mmap1M "MALLOC_MMAP_THRESHOLD_=1048576" "--mem 2G" "$OHC2" -cat $T_A
    THREADS=16 rss_run mem_ohc2_P16 "" "--mem 2G" "$OHC2" -cat $T_A
    rss_run mem_ohc2_netcdf "CDORS_NC4=netcdf" "--mem 2G" "$OHC2" -cat $T_A
    rss_run mem_ohc2_1step "" "--mem 2G" "${OHC2/-selname,temp/-seltimestep,1 -selname,temp}" "$D/temp.fesom.1970.nc"
    rss_run mem_probe "" "--mem 2G" "$PROBE_T" -cat $T_A
    rss_run mem_ohc2_default "" "" "$OHC2" -cat $T_A
    if [ -n "${CDORS2:-}" ]; then
        CDORS=$CDORS2 rss_run b2_mem_ohc2 "" "--mem 2G" "$OHC2" -cat $T_A
        CDORS=$CDORS2 rss_run b2_mem_ohc2_1step "" "--mem 2G" "${OHC2/-selname,temp/-seltimestep,1 -selname,temp}" "$D/temp.fesom.1970.nc"
        CDORS=$CDORS2 rss_run b2_mem_ohc2_default "" "" "$OHC2" -cat $T_A
    fi
    echo "done: $OUT"; date
    exit 0
fi
if [ "$MODE" = extra ]; then
    # second job: the first use of the mesh timed without a --plan before it (in job 28230544
    # the plan built the cache), the mesh build alone, and timing splits of the 3-D mesh path
    PLAN_AFTER=1 run_cdors mesh_first_ohc1 1975 "" "${OHC/-selname,temp/-seltimestep,1 -selname,temp}" "$S1"
    CDORS_CACHE=$OUT/cache_meshonly timed mesh_info_fresh - 0 "$CDORS" --json mesh info "fesom:$M"
    run_cdors mesh_cached_ohc1 1975 "" "${OHC/-selname,temp/-seltimestep,1 -selname,temp}" "$S1"
    run_cdors probe_ohc1 1975 "" "${PROBE_T/-selname,temp/-seltimestep,1 -selname,temp}" "$S1"
    run_cdors ohc_C_cold 1990-1992 "" "$OHC" -cat $(files temp 1990 1992)
    run_cdors probe_temp_A_cold 1970-1972 "" "$PROBE_T" -cat $(files temp 1970 1972)
    for i in 1 2; do
        run_cdors ohc_A_warm$i 1970-1972 "" "$OHC" -cat $(files temp 1970 1972)
        run_cdors fldmean3d_A_warm$i 1970-1972 "" "-fldmean -setgrid,fesom:$M -sellevel,$LV -selname,temp" -cat $(files temp 1970 1972)
        run_cdors volint1y_A_warm$i 1970 "" "$OHC" "$D/temp.fesom.1970.nc"
    done
    echo "done: $OUT"; date
    exit 0
fi

# 1 first use of the mesh and the region (fresh cache), then cached, then the probe of the same step
PLAN_AFTER=1 run_cdors mesh_first_ohc1 1975 "" "${OHC/-selname,temp/-seltimestep,1 -selname,temp}" "$S1"
"$CDORS" --json mesh info "fesom:$M" > "$OUT/mesh_info.json" 2>&1
/usr/bin/du -sb "$CDORS_CACHE"/* > "$OUT/cache_sizes.txt" 2>&1
run_cdors mesh_cached_ohc1 1975 "" "${OHC/-selname,temp/-seltimestep,1 -selname,temp}" "$S1"
run_cdors probe_ohc1 1975 "" "${PROBE_T/-selname,temp/-seltimestep,1 -selname,temp}" "$S1"

# 2 cold sets
run_cdors ohc_A_cold 1970-1972 "" "$OHC" -cat $(files temp 1970 1972)
run_cdors ice_A_cold 1970-1972 "" "$ICE" -cat $(files a_ice 1970 1972)
run_cdors ohc_B_cold 1980-1982 "" "$OHC" -cat $(files temp 1980 1982)
run_cdors ice_B_cold 1980-1982 "" "$ICE" -cat $(files a_ice 1980 1982)
run_cdors probe_temp_C_cold 1990-1992 "" "$PROBE_T" -cat $(files temp 1990 1992)
run_cdors probe_ice_C_cold 1990-1992 "" "$PROBE_I" -cat $(files a_ice 1990 1992)

# 3 warm
for i in 1 2; do
    run_cdors ohc_A_warm$i 1970-1972 "" "$OHC" -cat $(files temp 1970 1972)
    run_cdors ice_A_warm$i 1970-1972 "" "$ICE" -cat $(files a_ice 1970 1972)
    run_cdors probe_temp_A_warm$i 1970-1972 "" "$PROBE_T" -cat $(files temp 1970 1972)
    run_cdors probe_ice_A_warm$i 1970-1972 "" "$PROBE_I" -cat $(files a_ice 1970 1972)
done
run_cdors ohcnomask_A_warm 1970-1972 "" "$OHC_NOMASK" -cat $(files temp 1970 1972)

# 4 memory bound
run_cdors ohc_A_mem2G 1970-1972 "--mem 2G" "$OHC" -cat $(files temp 1970 1972)
run_cdors ice_A_mem2G 1970-1972 "--mem 2G" "$ICE" -cat $(files a_ice 1970 1972)

# 5 nereus reference with the mesh's areas, on A (pages cached by the runs above)
mkdir -p "$OUT/nereus_ref_A"
timed nereus_ref_A 1970-1972 0 \
    $PY "$HERE/baseline.py" --years 1970-1972 --out "$OUT/nereus_ref_A" --workers "$THREADS" \
    --chunks nod2 --area nod_area
cp "$OUT/nereus_ref_A/nereus_timing.json" "$OUT/nereus_ref_A.json" 2>/dev/null
$PY "$HERE/compare.py" "$OUT" > "$OUT/compare.stdout" 2>&1
grep -v -i -e pyproj -e ca_bundle "$OUT/compare.stdout"
echo "done: $OUT"; date
