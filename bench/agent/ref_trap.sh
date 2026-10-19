#!/bin/bash
#SBATCH --job-name=trap-refs
#SBATCH --partition=shared
#SBATCH --account=ab0995
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=32
#SBATCH --mem=64G
#SBATCH --hint=nomultithread
#SBATCH --time=00:40:00
#SBATCH --output=/scratch/a/a270088/cdors-bench/trap-refs-%j.log
#
# Agent check, trap candidates (bench/agent/ref_trap.py): references and naive answers, independent of
# cdors, then the chain an agent would naturally write with cdors, to check that it gives the reference.
#   sbatch bench/agent/ref_trap.sh          (from the repository root; CDORS_BIN, IFS_PY override)
# Outputs: /scratch/a/a270088/cdors-bench/trap-refs-<jobid>/{W1,W2,W4,W5}.json, cdors_*.json, times.tsv.
# Job 28250598 (2026-10-19) ran it with IFS_PY = env nereus, whose eccodes cannot decode W5's messages; W5 was
# then run on the login node in env ecmwf, now the default. Nothing is deleted or overwritten.
set -uo pipefail
HERE=${SLURM_SUBMIT_DIR:-$(pwd)}/bench/agent
OUT=/scratch/a/a270088/cdors-bench/trap-refs-${SLURM_JOB_ID:-local-$(date +%s)}
[ -e "$OUT" ] && { echo "$OUT exists" >&2; exit 1; }
mkdir -p "$OUT/cache"
PY=/work/ab0995/a270088/mambaforge/envs/nereus/bin/python
IFS_PY=${IFS_PY:-/work/ab0995/a270088/mambaforge/envs/ecmwf/bin/python}
C=${CDORS_BIN:-/work/ab0995/a270088/cdors-target/frozen/cdors-32b1dbd}
M=/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart
D=/work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom
G=/work/bm1344/a270228/phase1_hist/gribscan_1m_NATIVE/jsons.1950-1959/sfc.dir/atm2d_avg.json
export CDORS_CACHE=$OUT/cache
cd "$OUT" || exit 1
echo -e "what\twall_s\tmaxrss_kB\texit" > times.tsv
run() {  # run NAME OUTFILE cmd...
  local name=$1 out=$2; shift 2
  /usr/bin/time -o "$name.time" -f "%e %M" "$@" > "$out" 2> "$name.err"
  local rc=$?
  echo -e "$name\t$(cut -d' ' -f1 "$name.time")\t$(cut -d' ' -f2 "$name.time")\t$rc" >> times.tsv
  echo "$(date +%T) $name exit $rc"
}
echo "node $(hostname), cdors $($C --version 2>/dev/null | sed -n 1p), binary $C"
T="-seltimestep,1 -setgrid,fesom:$M"

# --- cdors: the chains an agent would write
run cdors_mesh mesh_info.txt "$C" mesh info "fesom:$M"
run cdors_W1 cdors_W1.json "$C" --json outputtab,value -fldmean -sellevel,1600 $T -selname,temp "$D/temp.fesom.1990.nc"
run cdors_W2 cdors_W2.json "$C" --json outputtab,value -volmean,1000/2000 $T -selname,temp "$D/temp.fesom.1990.nc"
for b in gulfstream:-80,-40,30,50 agulhas:10,40,-45,-30 nordic:-20,20,60,80 drake:-70,-55,-62,-55 \
         nino34:-170,-120,-5,5; do
  run "cdors_W4_${b%%:*}" "cdors_W4_${b%%:*}.json" "$C" --json outputtab,value -fldmean -sellonlatbox,${b#*:} \
    -sellevel,2.5 $T -selname,u "$D/u.fesom.1990.nc"
done
run cdors_W5 cdors_W5.json "$C" --json outputtab,date,value -fldmean -seltimestep,1 -selname,mean2t "$G"

# --- references and naive answers
for w in W1 W2 W4; do run "ref_$w" "$w.json" "$PY" -I "$HERE/ref_trap.py" "$w"; done
run ref_W5 W5.json "$IFS_PY" -I "$HERE/ref_trap.py" W5
echo "done $(date -Is)"
