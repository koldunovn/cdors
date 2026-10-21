#!/bin/bash
#SBATCH --job-name=moc-refs
#SBATCH --partition=shared
#SBATCH --account=ab0995
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=16
#SBATCH --mem=14G
#SBATCH --hint=nomultithread
#SBATCH --time=00:40:00
#SBATCH --output=/scratch/a/a270088/cdors-bench/moc-refs-%j.log
# --mem: the shared partition grants one core per 940 MB, so more memory than the cores need bills more cores.
#
# Agent check, long-script task H1 (AMOC at 26.5 N from w on dart): the reference and its variants
# (bench/agent/ref_moc.py, independent of cdors), then cdors' chain for the same number, and cdors' full
# Atlantic and global streamfunctions (1 degree) with their run times.
#   CDORS_BIN=<binary> sbatch bench/agent/ref_moc.sh      (from the repository root)
# Outputs: /scratch/a/a270088/cdors-bench/moc-refs-<jobid>/{H1.json, cdors_H1.json, moc_atl.nc, moc_glob.nc,
# times.tsv}. Nothing is deleted or overwritten.
set -uo pipefail
HERE=${SLURM_SUBMIT_DIR:-$(pwd)}/bench/agent
OUT=/scratch/a/a270088/cdors-bench/moc-refs-${SLURM_JOB_ID:-local-$(date +%s)}
[ -e "$OUT" ] && { echo "$OUT exists" >&2; exit 1; }
mkdir -p "$OUT/cache"
PY=/work/ab0995/a270088/mambaforge/envs/nereus/bin/python
C=${CDORS_BIN:?set CDORS_BIN to the cdors binary}
M=/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart
D=/work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom
export CDORS_CACHE=$OUT/cache
export CDORS_REGIONS=${CDORS_REGIONS:-/work/ab0995/a270088/cdors-target/fixtures/regions}
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
T="-setgrid,fesom:$M -seltimestep,1 -selname,w $D/w.fesom.1990.nc"

run cdors_H1 cdors_H1.json "$C" --json outputtab,lev,value -moc,lat=26.5,regions=moc,select=Atlantic_MOC $T
run cdors_atl atl.out "$C" -moc,regions=moc,select=Atlantic_MOC $T moc_atl.nc
run cdors_glob glob.out "$C" -moc $T moc_glob.nc
run ref_H1 H1.json "$PY" -I "$HERE/ref_moc.py" 26.5
echo "done $(date -Is)"
