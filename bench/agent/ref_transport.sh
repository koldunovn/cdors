#!/bin/bash
#SBATCH --job-name=transport-refs
#SBATCH --partition=shared
#SBATCH --account=ab0995
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=16
#SBATCH --mem=14G
#SBATCH --hint=nomultithread
#SBATCH --time=00:40:00
#SBATCH --output=/scratch/a/a270088/cdors-bench/transport-refs-%j.log
# --mem: the shared partition grants one core per 940 MB, so more memory than the cores need bills more cores.
#
# Agent check, long-script task H2 (volume transport through Drake Passage from u and v on dart): the
# reference and its variants (bench/agent/ref_transport.py, independent of cdors), then cdors' chain for the
# same number and its per-layer transport, with their run times.
#   CDORS_BIN=<binary> sbatch bench/agent/ref_transport.sh      (from the repository root)
# Outputs: /scratch/a/a270088/cdors-bench/transport-refs-<jobid>/{H2.json, cdors_H2.json, cdors_layers.json,
# times.tsv}. Nothing is deleted or overwritten.
set -uo pipefail
HERE=${SLURM_SUBMIT_DIR:-$(pwd)}/bench/agent
OUT=/scratch/a/a270088/cdors-bench/transport-refs-${SLURM_JOB_ID:-local-$(date +%s)}
[ -e "$OUT" ] && { echo "$OUT exists" >&2; exit 1; }
mkdir -p "$OUT/cache"
PY=/work/ab0995/a270088/mambaforge/envs/nereus/bin/python
C=${CDORS_BIN:?set CDORS_BIN to the cdors binary}
M=/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart
D=/work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom
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
S=-68,-54,-63,-65.5
U="-setgrid,fesom:$M -seltimestep,1 -selname,u $D/u.fesom.1990.nc"
V="-setgrid,fesom:$M -seltimestep,1 -selname,v $D/v.fesom.1990.nc"

run cdors_H2 cdors_H2.json "$C" --json outputtab,value -vertsum -transport,$S $U $V
run cdors_layers cdors_layers.json "$C" --json outputtab,lev,value -transport,$S $U $V
run ref_H2 H2.json "$PY" -I "$HERE/ref_transport.py"
echo "done $(date -Is)"
