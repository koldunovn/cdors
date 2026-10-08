#!/bin/bash
# cdors Task 13: benchmarks W1-W4 (bench/bench.sh) on one exclusive compute node.
#
# DO NOT SUBMIT without Nikolay's explicit go-ahead.
#
# Cost estimate (one exclusive node = 1 node-hour per hour):
#   expected ~5-6 h  =>  ~5-6 node-hours; time limit 8 h  =>  at most 8 node-hours
#   (the plan budgets 10-20 node-hours for Task 13).
#   Where the time goes (cold Lustre; rates from docs/baseline.md):
#     pre-flight on the fixtures                                     ~1 min
#     W1  cdors x2, cdo, xarray on 46 GB                              ~5-10 min
#     W2  cdors x3 (Parquet, raw, cloud), cdo, xarray x2 (Lustre, cloud); the cloud is capped at
#         ~0.19 GB/s, so each cloud run reads 13.2 GB in ~70 s or more  ~10-20 min
#     W3  genbil, cdors x2, cdo on 7.6 GB                             ~5 min
#     W4Y (2020 only, 37 GB) cdors x3, cdo timpctl (3 reads) and ydaymean  ~20-30 min
#     W4  (1.1 TB) cdors timpctl and ydaymean under --mem 32G        ~10-60 min
#         cdo ydaymean and timpctl, 2 h timeout each (cdo reads ~0.18 GB/s => ydaymean ~1.7 h,
#         timpctl ~5 h: expected "did_not_finish")                   up to 4 h
#   Cheaper split, if wanted: WORKLOADS="W1 W2 W3 W4Y" (~1-1.5 node-hours), W4 later on its own.
#   Shorter cdo timeouts: TMO_CDO_W4=3600 (W4 at most ~2.5 h).
#
# Before submitting (login node):
#   1. build cdors:   source /home/a/a270088/cdo/env.sh && cargo build --release
#   2. pre-flight:    FIXTURE=1 bench/bench.sh   (< 1 min; cdors runs must not say not_implemented)
#                     PLAN_ONLY=1 bench/bench.sh (~20 s, metadata only: cdors plans every real chain)
#   3. read the plan: DRY=1 bench/bench.sh
#   4. submit:        sbatch /home/a/a270088/cdo/bench/bench.sbatch
#      (from an unmerged worktree: sbatch --export=ALL,BENCH=<worktree>/bench <worktree>/bench/bench.sbatch)
#
# The job repeats both pre-flights and stops at once (no node time beyond ~1-2 min) if the cdors
# binary is missing, or cdors cannot run a fixture stand-in or plan a real chain (REQUIRE_OPS=0
# runs anyway and records not_implemented).
#
# Outputs: /scratch/a/a270088/cdors-bench/bench-<jobid>/ (runs.tsv, compare.tsv, summary.md, env.txt,
#          per run .cmd/.log/.time/.plan.json and the output .nc); pre-flight in bench-<jobid>-preflight/.
#          Nothing is deleted; every run writes into these fresh directories.
#
#SBATCH --job-name=cdors-bench
#SBATCH --partition=compute
#SBATCH --account=ab0995
#SBATCH --nodes=1
#SBATCH --exclusive
#SBATCH --mem=0
#SBATCH --time=08:00:00
#SBATCH --output=/scratch/a/a270088/cdors-bench/bench-%j.out

source /home/a/a270088/cdo/env.sh
set -uo pipefail

BENCH=${BENCH:-/home/a/a270088/cdo/bench}
ROOT=/scratch/a/a270088/cdors-bench
JOB=${SLURM_JOB_ID:-manual-$(date +%Y%m%d-%H%M%S)}
CDORS=${CDORS:-$CARGO_TARGET_DIR/release/cdors}
export CDORS

if [ ! -f "$BENCH/bench.sh" ]; then
    echo "bench.sh not found in $BENCH (merge the Task 13 branch, or pass BENCH=<worktree>/bench)" >&2
    exit 1
fi
if [ ! -x "$CDORS" ]; then
    echo "cdors not found at $CDORS; build it on the login node first:" >&2
    echo "  source /home/a/a270088/cdo/env.sh && cargo build --release" >&2
    exit 1
fi
"$CDORS" --version || exit 1

# ---------------------------------------------------------------- pre-flight on the fixtures (< 1 min)
PRE=$ROOT/bench-$JOB-preflight
FIXTURE=1 OUT=$PRE bash "$BENCH/bench.sh"
# (the bin-width helpers *_range/_timmax/_timmin have fallbacks and do not count)
missing=$(awk -F'\t' '$3=="cdors" && $8!="ok" && $1 !~ /_(range|timmax|timmin)$/ {print $1 " (" $8 ")"}' "$PRE/runs.tsv" 2>/dev/null)
if [ -n "$missing" ] && [ "${REQUIRE_OPS:-1}" = 1 ]; then
    echo "pre-flight: cdors cannot run these fixture stand-ins yet; stopping before the real workloads:" >&2
    echo "$missing" >&2
    echo "(see $PRE; REQUIRE_OPS=0 runs anyway and records not_implemented)" >&2
    exit 2
fi
# metadata-only plans on the real inputs (Zarr stores, Parquet refs, raw-file globs, EERIE cloud)
PLN=$ROOT/bench-$JOB-plans
PLAN_ONLY=1 OUT=$PLN bash "$BENCH/bench.sh" > /dev/null
missing=$(awk -F'\t' '$3=="cdors-plan" && $8!="ok" && $1 !~ /_(range|timmax|timmin)$/ {print $1 " (" $8 ": " $11 ")"}' "$PLN/runs.tsv" 2>/dev/null)
if [ -n "$missing" ] && [ "${REQUIRE_OPS:-1}" = 1 ]; then
    echo "pre-flight: cdors cannot plan these real workloads; stopping:" >&2
    echo "$missing" >&2
    exit 2
fi

# ---------------------------------------------------------------- the benchmarks
OUT=$ROOT/bench-$JOB bash "$BENCH/bench.sh"
