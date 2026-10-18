#!/bin/bash
#SBATCH --job-name=t14-refs
#SBATCH --partition=compute
#SBATCH --account=ab0995
#SBATCH --nodes=1
#SBATCH --exclusive
#SBATCH --mem=0
#SBATCH --hint=nomultithread
#SBATCH --time=00:45:00
#SBATCH --output=/scratch/a/a270088/cdors-bench/t14-refs-%j.log
#
# Agent check, FESOM2 tasks F1-F5: the reference answers (bench/agent/ref_fesom_*.py, NumPy/nereus,
# independent of cdors) and, as a cross-check, the command an expert would run with cdors.
# One compute node, so the references do not warm the login node's page cache for the sessions.
#   sbatch bench/agent/ref_fesom.sh            (from the repository root; CDORS_BIN overrides the binary)
# Job 28230786 (2026-10-19) ran it with CDORS_BIN=/work/ab0995/a270088/cdors-target-r1-t14/release/cdors (b3285c3).
# Outputs: /scratch/a/a270088/cdors-bench/t14-refs-<jobid>/{F1..F5}.json (references),
# cdors_F*.json (cdors records), times.tsv. Nothing is deleted or overwritten.
set -uo pipefail
HERE=${SLURM_SUBMIT_DIR:-$(pwd)}/bench/agent
OUT=/scratch/a/a270088/cdors-bench/t14-refs-${SLURM_JOB_ID:-local-$(date +%s)}
[ -e "$OUT" ] && { echo "$OUT exists" >&2; exit 1; }
mkdir -p "$OUT/cache"
PY=/work/ab0995/a270088/mambaforge/envs/nereus/bin/python
C=${CDORS_BIN:-/work/ab0995/a270088/cdors-target/release/cdors}
M=/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart
D=/work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom
JQ=/usr/bin/jq
export CDORS_CACHE=$OUT/cache CDORS_REGIONS=${CDORS_REGIONS:-/work/ab0995/a270088/cdors-target/fixtures/regions}
export HDF5_PLUGIN_PATH=/sw/spack-levante/netcdf-c-main-bdxvs5/plugins:/sw/spack-levante/netcdf-c-main-k4lh4v/plugins
export CDO=/sw/spack-levante/cdo-2.6.0-akkxhz/bin/cdo
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

# --- cdors, the expert's commands (cold: before the Python references read the same files)
run cdors_mesh mesh_info.txt "$C" mesh info "fesom:$M"
run cdors_F1 cdors_F1.json "$C" --json outputtab,date,value -mulc,4.08975e6 -volint,0/700 \
  -maskregion,regions=basins,select=Atlantic_Basin -setgrid,fesom:$M -selname,temp "$D/temp.fesom.199[01].nc"
run cdors_F2 cdors_F2.json "$C" --json outputtab,name,date,value -ymonmean -fldhist,edges=0.15/1 \
  -sellonlatbox,-180,180,0,90 -setgrid,fesom:$M -selname,a_ice "$D/a_ice.fesom.199[01].nc"
TR="-transect,-65,-55,-65,-63,step=11.12"
IN3="-setgrid,fesom:$M -selname,temp -seltimestep,1 $D/temp.fesom.2000.nc"
run cdors_F3_section drake.nc.log "$C" $TR $IN3 drake.nc
run cdors_F3_sumTdz cdors_F3_sumTdz.json "$C" --json outputtab,value -fldsum $TR -vertint $IN3
run cdors_F3_sumH cdors_F3_sumH.json "$C" --json outputtab,value -fldsum -selname,bottom_depth drake.nc
run cdors_F3_samples cdors_F3_samples.json "$C" --json outputtab,distance,lev,value -selname,temp drake.nc
run cdors_F4_box nino_box.log "$C" -fldmean -sellonlatbox,-170,-120,-5,5 -sellevel,2.5 -setgrid,fesom:$M \
  -selname,temp "$D/temp.fesom.200[1-5].nc" nino_box.nc
run cdors_F4 cdors_F4.json "$C" --json outputtab,date,value -selyear,2005 -ymonsub nino_box.nc -ymonmean nino_box.nc
F5IN=(); for y in 2006 2007 2008 2009 2010; do F5IN+=("$D/temp.fesom.$y.nc"); done
run cdors_F5 cdors_F5.json "$C" --json outputtab,date,value -yearmean -fldmean -sellonlatbox,-180,180,66.5,90 \
  -sellevel,2.5 -setgrid,fesom:$M -selname,temp -mergetime "${F5IN[@]}"

# --- references (NumPy; nereus as the second method for F1, F2)
run ref_F1 F1.json "$PY" -I "$HERE/ref_fesom_ohc.py" --nereus
run ref_F2 F2.json "$PY" -I "$HERE/ref_fesom_ice.py" --nereus
run ref_F3 F3.json "$PY" -I "$HERE/ref_fesom_drake.py"
run ref_F4 F4.json "$PY" -I "$HERE/ref_fesom_nino.py"
run ref_F5 F5.json "$PY" -I "$HERE/ref_fesom_arctic.py"

# --- cdors vs reference
cmp_vals() {  # cmp_vals TASK "jq expr for the cdors values"
  local c r
  c=$($JQ -c "$2" "cdors_$1.json" 2>/dev/null)
  r=$($JQ -c '.values' "$1.json" 2>/dev/null)
  echo "$1 cdors     $c"
  echo "$1 reference $r"
}
cmp_vals F1 '[.records[].value]'
cmp_vals F2 '[.records[] | select(.name == "a_ice") | .value / 1e12]'
s=$($JQ '.records[0].value' cdors_F3_sumTdz.json); h=$($JQ '.records[0].value' cdors_F3_sumH.json)
n=$($JQ '[.records[] | select(.value != null)] | length' cdors_F3_samples.json)
echo "F3 cdors     [$(awk -v s="$s" -v h="$h" 'BEGIN{printf "%.10g", s / h}'),$n]"
echo "F3 reference $($JQ -c '.values' F3.json)"
cmp_vals F4 '[.records[].value]'
cmp_vals F5 '[.records[].value]'
echo "done $(date +%T); outputs in $OUT"
