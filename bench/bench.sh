#!/usr/bin/env bash
# cdors Task 13 benchmarks W1-W4: each workload once per tool, then cdors (and xarray) vs cdo with
# `cdo --pedantic diffn`. Datasets: bench/datasets.md. Run on a compute node via bench/bench.sbatch.
#
#   bench/bench.sh                    real workloads (compute node only: reads ~1.3 TB)
#   DRY=1 bench/bench.sh              print every command, run nothing, write nothing
#   PLAN_ONLY=1 bench/bench.sh        only `cdors --plan --json` per cdors run (metadata; login node OK):
#                                     checks that cdors opens every real input and accepts every chain
#   FIXTURE=1 bench/bench.sh          tiny stand-ins on $CDORS_FIXTURES (login node, < 1 min)
#   FIXTURE=1 CDORS=tests/fake_cdors.sh bench/bench.sh    plumbing check with cdo posing as cdors
#   python3 bench/summarize.py <OUT>  markdown table for docs/bench-results.md
#
# Output: one fresh directory $OUT (refuses to reuse one; nothing is ever deleted or overwritten):
#   runs.tsv     run, workload, tool, source, wall_s, max_rss_kb, rc, status, decoded_gb, output, notes
#                status: ok | not_implemented | failed | no_output | did_not_finish (timeout)
#                        | skipped (not run on purpose; the note says why)
#   compare.tsv  workload, ref, test, tag, abslim, values (PASS/FAIL/SKIP), timestamps, detail
#   <run>.cmd/.log/.time per run, <run>.plan.json (cdors --plan --json, for bytes decoded), env.txt
#
# Comparison rule (as tests/run_cases.sh): values with `cdo --pedantic diffn,abslim=<x>`, rellim at
# its default; ulp: x = 2 * 2^-24 * max|ref|; bin (percentiles; cdo uses 101-bin histograms):
# x = max(timmax - timmin) / 101; exact: x = 0. Timestamps (`cdo -s showtimestamp`) are compared
# where both sides read the same time axis (not across W2's raw files vs kerchunk/cloud).
#
# Cache order: in every workload cdors runs first (cold Lustre cache), cdo after it. Runs that
# read the same bytes later (cdo, xarray, a second cdors source) may find them in the page cache;
# this favours cdo, never cdors. W4's 1.1 TB does not fit in memory, so W4 is cold for all tools.
#
# Settings (environment; defaults for one exclusive 128-core Levante compute node):
#   CDO=/sw/spack-levante/cdo-2.6.0-akkxhz/bin/cdo   CDO_P=16 (cdo -P; -P 8 did not help in Task 2)
#   CDORS=$CARGO_TARGET_DIR/release/cdors (copied into $OUT/bin first, so rebuilds cannot interfere)
#   CDORS_P=128                 cdors -P (compute threads)
#   CDORS_IO_THREADS            reads in flight (--io-threads); unset: not passed, so cdors uses its own
#                               default (128 inside a Slurm job, 64 for URLs). W1 adds two cold runs on
#                               other decades, one at --io-threads 64 and one at the default.
#   CDORS_MEM=32G               W4 memory budget (--mem)
#   XR_PY=.../envs/hk25/bin/python  xarray 2025.4, dask 2025.4, flox 0.10.1, zarr 2.18.7
#   XR_WORKERS=128, XR_CLOUD_WORKERS=64   dask threads (local / EERIE cloud)
#   TMO_DEFAULT=3600            timeout per run (s); TMO_CDO_W4=7200 for cdo on the full W4 (then
#                               "did_not_finish" is the result), TMO_CDORS_W4=5400
#   CDO_W4_TIMPCTL=0            skip cdo timpctl,95 on the full W4: by the Task 2 baseline it needs
#                               >= 30 x 632 s = 5.3 h (docs/baseline.md); 1 runs it under TMO_CDO_W4
#   WORKLOADS="W1 W2 W3 W4Y W4" subset to run (W4Y = W4 on its first year, 2020: cdo finishes there)
#   OUT=/scratch/a/a270088/cdors-bench/bench-<jobid>   (fixture: .../fixture-<date>-<pid>)
set -uo pipefail          # no -e: one failing run must not stop the others

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
DRY=${DRY:-0}
PLAN_ONLY=${PLAN_ONLY:-0}
FIXTURE=${FIXTURE:-0}
CDO=${CDO:-/sw/spack-levante/cdo-2.6.0-akkxhz/bin/cdo}
CDORS_SRC=${CDORS:-${CARGO_TARGET_DIR:-/work/ab0995/a270088/cdors-target}/release/cdors}
CDORS_FIXTURES=${CDORS_FIXTURES:-${CARGO_TARGET_DIR:-/work/ab0995/a270088/cdors-target}/fixtures}
XR_PY=${XR_PY:-/work/ab0995/a270088/mambaforge/envs/hk25/bin/python}
VIEW_PY=${VIEW_PY:-/work/ab0995/a270088/mambaforge/bin/python}
CDO_P=${CDO_P:-16}
CDORS_P=${CDORS_P:-128}
CDORS_IO_THREADS=${CDORS_IO_THREADS:-}
CDORS_MEM=${CDORS_MEM:-32G}
CDO_W4_TIMPCTL=${CDO_W4_TIMPCTL:-0}
XR_WORKERS=${XR_WORKERS:-128}
XR_CLOUD_WORKERS=${XR_CLOUD_WORKERS:-64}
TMO_DEFAULT=${TMO_DEFAULT:-3600}
TMO_CDO_W4=${TMO_CDO_W4:-7200}
TMO_CDORS_W4=${TMO_CDORS_W4:-5400}
WORKLOADS=${WORKLOADS:-W1 W2 W3 W4Y W4}
export CDO CDORS_CACHE=${CDORS_CACHE:-/work/ab0995/a270088/cdors-cache}   # cdors: weight generation, index cache
ROOT=/scratch/a/a270088/cdors-bench
VIEWS=$ROOT/views

if ((FIXTURE)); then
    OUT=${OUT:-$ROOT/fixture-$(date +%Y%m%d-%H%M%S)-$$}
    CDORS_P=${FIX_CDORS_P:-4} CDO_P=${FIX_CDO_P:-4} CDORS_IO_THREADS=${FIX_IO_THREADS:-8}
    CDORS_MEM=${FIX_MEM:-64M} XR_WORKERS=4 XR_CLOUD_WORKERS=4
    TMO_DEFAULT=120 TMO_CDO_W4=120 TMO_CDORS_W4=120
else
    if ((PLAN_ONLY)); then OUT=${OUT:-$ROOT/plan-$(date +%Y%m%d-%H%M%S)-$$}; fi
    OUT=${OUT:-$ROOT/bench-${SLURM_JOB_ID:-manual-$(date +%Y%m%d-%H%M%S)-$$}}
fi

want() { [[ " $WORKLOADS " == *" $1 "* ]]; }

# ---------------------------------------------------------------- printing (DRY) and setup
# show CMD... : one line; runs of > 4 absolute *.nc arguments (not the last argument, the output)
# are abbreviated as "first ... last (N files)"; arguments with shell characters are single-quoted.
show() {
    local -a o=() run=()
    local x i n=$#
    q() { if [[ $1 =~ ^[A-Za-z0-9_./:=,+@%-]+$ ]]; then printf '%s' "$1"; else printf "'%s'" "$1"; fi; }
    flush() {
        if ((${#run[@]} > 4)); then o+=("${run[0]}" "..." "${run[-1]}" "(${#run[@]} files)"); else o+=("${run[@]}"); fi
        run=()
    }
    for ((i = 1; i <= n; i++)); do
        x=${!i}
        if ((i < n)) && [[ $x == /*.nc && $x != *[*?[]* ]]; then run+=("$x"); else flush; o+=("$(q "$x")"); fi
    done
    flush
    echo "${o[*]}"
}

if ((DRY)); then
    CDORS=$CDORS_SRC
    echo "# DRY run: nothing is executed or written. OUT would be $OUT"
else
    if [[ -e $OUT ]]; then echo "$OUT exists; refusing to reuse it (set OUT=...)" >&2; exit 1; fi
    mkdir -p "$OUT/bin" || exit 1
    if [[ ! -x $CDORS_SRC ]]; then echo "cdors not found: $CDORS_SRC" >&2; exit 1; fi
    CDORS=$OUT/bin/$(basename "$CDORS_SRC")
    cp -p "$CDORS_SRC" "$CDORS" || exit 1
    printf "run\tworkload\ttool\tsource\twall_s\tmax_rss_kb\trc\tstatus\tdecoded_gb\toutput\tnotes\n" > "$OUT/runs.tsv"
    printf "workload\tref\ttest\ttag\tabslim\tvalues\ttimestamps\tdetail\n" > "$OUT/compare.tsv"
fi

# reads in flight: --io-threads only when CDORS_IO_THREADS (or a run's IO=) is set (see header)
CDORS_OPT=(-P "$CDORS_P" -f nc4)
CDO_OPT=(-P "$CDO_P" -f nc4)

if ! ((DRY)); then
    {
        date; hostname; echo "SLURM_JOB_ID=${SLURM_JOB_ID:-}"
        lscpu | grep -E 'Model name|^CPU\(s\)|Thread|Socket'
        free -g
        "$CDO" --version 2>&1 | head -2
        echo "cdors: $CDORS_SRC (copied to $CDORS)"; ls -l "$CDORS_SRC"; sha256sum "$CDORS"
        "$CDORS" --version 2>&1
        echo "CDORS_OPT=${CDORS_OPT[*]}  CDORS_IO_THREADS=${CDORS_IO_THREADS:-default}  CDO_OPT=${CDO_OPT[*]}  CDORS_MEM=$CDORS_MEM  WORKLOADS=$WORKLOADS"
        echo "CDO_W4_TIMPCTL=$CDO_W4_TIMPCTL"
        echo "XR_PY=$XR_PY XR_WORKERS=$XR_WORKERS XR_CLOUD_WORKERS=$XR_CLOUD_WORKERS"
        echo "TMO_DEFAULT=$TMO_DEFAULT TMO_CDO_W4=$TMO_CDO_W4 TMO_CDORS_W4=$TMO_CDORS_W4 FIXTURE=$FIXTURE"
    } > "$OUT/env.txt" 2>&1
fi

# ---------------------------------------------------------------- one run
declare -A STATUS OUTF
# [TMO=s] [GB=nominal decoded GB] [NOTE=text] run ID WORKLOAD TOOL SOURCE OUTFILE CMD...
#   OUTFILE "-" = no output file. One runs.tsv line per call.
run() {
    local id=$1 wl=$2 tool=$3 src=$4 outf=$5; shift 5
    local tmo=${TMO:-$TMO_DEFAULT} gb=${GB:--} note=${NOTE:-}
    if ((PLAN_ONLY)) && [[ $tool != cdors-plan ]]; then STATUS[$id]=skipped; OUTF[$id]=$outf; return 0; fi
    if ((DRY)); then
        printf '[%s] %s/%s/%s (timeout %ss)\n    %s\n' "$id" "$wl" "$tool" "$src" "$tmo" "$(show "$@")"
        STATUS[$id]=dry; OUTF[$id]=$outf
        return 0
    fi
    echo "== $(date +%T) $id: $(show "$@")"
    printf '%q ' "$@" > "$OUT/$id.cmd"; echo >> "$OUT/$id.cmd"
    /usr/bin/time -v -o "$OUT/$id.time" timeout --kill-after=60 "$tmo" "$@" > "$OUT/$id.log" 2>&1
    local rc=$? wall rss status
    wall=$(awk -F': ' '/Elapsed \(wall clock\)/{n=split($2,a,":"); s=0; for(i=1;i<=n;i++) s=s*60+a[i]; printf "%.2f", s}' "$OUT/$id.time")
    rss=$(awk -F': ' '/Maximum resident/{print $2}' "$OUT/$id.time")
    if ((rc == 0)); then
        status=ok
        [[ $outf == - || -s $outf ]] || status=no_output
    elif ((rc == 124 || rc == 137)); then      # timeout: TERM after $tmo s, KILL 60 s later
        status=did_not_finish; note="${note:+$note; }timeout ${tmo}s"
    elif grep -qiE 'not_implemented|not implemented' "$OUT/$id.log"; then
        status=not_implemented
    else
        status=failed
    fi
    if [[ $status != ok ]]; then   # first error line of the log, else its last line
        local msg; msg=$( { grep -m1 -iE 'error|abort' "$OUT/$id.log" || grep -v '^\s*$' "$OUT/$id.log" | tail -1; } | cut -c1-200 | tr '\t' ' ')
        [[ -n $msg ]] && note="${note:+$note; }$msg"
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$id" "$wl" "$tool" "$src" "${wall:--}" "${rss:--}" \
        "$rc" "$status" "$gb" "$outf" "$note" >> "$OUT/runs.tsv"
    tail -n 1 "$OUT/runs.tsv"
    STATUS[$id]=$status; OUTF[$id]=$outf
}
# skip ID WORKLOAD TOOL SOURCE NOTE : a runs.tsv line (status skipped) for a run not made on purpose
skip() {
    local id=$1 wl=$2 tool=$3 src=$4 note=$5
    STATUS[$id]=skipped; OUTF[$id]=-
    if ((DRY)); then printf '[%s] %s/%s/%s skipped: %s\n' "$id" "$wl" "$tool" "$src" "$note"; return 0; fi
    ((PLAN_ONLY)) && return 0
    printf '%s\t%s\t%s\t%s\t-\t-\t-\tskipped\t-\t-\t%s\n' "$id" "$wl" "$tool" "$src" "$note" >> "$OUT/runs.tsv"
    tail -n 1 "$OUT/runs.tsv"
}

# cdors with the common options; also saves `--plan --json` (what will be read) beside the run.
# [TMO=] [GB=] [NOTE=] [MEM=size] [IO=reads in flight] run_cdors ID WORKLOAD SOURCE OUTFILE CHAIN...
run_cdors() {
    local id=$1 wl=$2 src=$3 outf=$4; shift 4
    local io=${IO:-$CDORS_IO_THREADS}
    local -a cmd=("$CDORS" "${CDORS_OPT[@]}")
    [[ -n $io ]] && cmd+=(--io-threads "$io")
    [[ -n ${MEM:-} ]] && cmd+=(--mem "$MEM")
    cmd+=("$@" "$outf")
    if ((PLAN_ONLY)); then     # metadata only: can cdors open the inputs and plan the chain?
        TMO=300 run "$id" "$wl" cdors-plan "$src" - "${cmd[0]}" --plan --json "${cmd[@]:1}"
        return 0
    fi
    if ! ((DRY)); then
        timeout 300 "${cmd[0]}" --plan --json "${cmd[@]:1}" > "$OUT/$id.plan.json" 2> "$OUT/$id.plan.err"
    fi
    NOTE="${NOTE:+$NOTE; }P=$CDORS_P io=${io:-default}${MEM:+ mem=$MEM}" run "$id" "$wl" cdors "$src" "$outf" "${cmd[@]}"
}
# [TMO=] [GB=] [NOTE=] run_cdo ID WORKLOAD SOURCE OUTFILE ARGS...
run_cdo() {
    local id=$1 wl=$2 src=$3 outf=$4; shift 4
    NOTE="${NOTE:+$NOTE; }P=$CDO_P" run "$id" "$wl" cdo "$src" "$outf" "$CDO" "${CDO_OPT[@]}" "$@" "$outf"
}
# [TMO=] [GB=] [NOTE=] run_xr ID WORKLOAD SOURCE OUTFILE ARGS...   (bench/xarray_baseline.py)
run_xr() {
    local id=$1 wl=$2 src=$3 outf=$4; shift 4
    run "$id" "$wl" xarray "$src" "$outf" "$XR_PY" -I "$HERE/xarray_baseline.py" "$@" --out "$outf"
    ((DRY)) || grep -h '^{' "$OUT/$id.log" >> "$OUT/xarray.jsonl" 2>/dev/null
}

# ---------------------------------------------------------------- comparison
maxabs() { awk '{for(i=1;i<=NF;i++){v=$i<0?-$i:$i; if(v<1e30 && v>m)m=v}} END{printf "%.9g\n", m+0}'; }
# compare WORKLOAD REF_ID TEST_ID TAG [TS]    TAG: exact | ulp | bin:<abslim> | abs:<abslim>
#   TS=1 also compares the timestamps. One compare.tsv line per call.
compare() {
    local wl=$1 ref=$2 test=$3 tag=$4 ts=${5:-0}
    local rf=${OUTF[$ref]:-} tf=${OUTF[$test]:-} lim values tsres=- detail=""
    ((PLAN_ONLY)) && return 0
    if ((DRY)); then
        printf '[compare %s] %s vs %s: %s --pedantic diffn,abslim=<%s> %s %s%s\n' "$wl" "$test" "$ref" "$CDO" "$tag" "$rf" "$tf" \
            "$( ((ts)) && echo "; showtimestamp compared")"
        return 0
    fi
    local log=$OUT/cmp_${test}.log
    if [[ ${STATUS[$ref]:-} != ok || ${STATUS[$test]:-} != ok ]]; then
        printf '%s\t%s\t%s\t%s\t-\tSKIP\t-\t%s\n' "$wl" "$ref" "$test" "$tag" \
            "ref ${STATUS[$ref]:-missing}, test ${STATUS[$test]:-missing}" >> "$OUT/compare.tsv"
        return 0
    fi
    case $tag in
        exact) lim=0 ;;
        ulp)   lim=$("$CDO" -s outputf,%.9g -timmax -fldmax -abs "$rf" 2>> "$log" | maxabs | awk '{printf "%.9g", 2*2^-24*$1}') ;;
        bin:*|abs:*) lim=${tag#*:} ;;
        *) lim=0 ;;
    esac
    if [[ -z $lim || $lim == - ]]; then
        printf '%s\t%s\t%s\t%s\t-\tSKIP\t-\t%s\n' "$wl" "$ref" "$test" "$tag" "no abslim (bin width unknown)" >> "$OUT/compare.tsv"
        return 0
    fi
    if "$CDO" --pedantic diffn,abslim="$lim" "$rf" "$tf" >> "$log" 2>&1; then values=PASS; else values=FAIL; fi
    detail=$(grep -E 'differ|Warning|Error|error' "$log" | tail -1 | cut -c1-160 | tr '\t' ' ')
    if ((ts)); then
        if diff <("$CDO" -s showtimestamp "$rf" 2>/dev/null) <("$CDO" -s showtimestamp "$tf" 2>/dev/null) >> "$log"; then
            tsres=same; else tsres=differ; fi
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$wl" "$ref" "$test" "${tag%%:*}" "$lim" "$values" "$tsres" "$detail" >> "$OUT/compare.tsv"
    tail -n 1 "$OUT/compare.tsv"
}
# bin width for percentile comparisons from a (timmax - timmin) file: max over the grid / 101
binwidth() {
    local id=$1
    ((DRY)) && { echo "<max(${OUTF[$id]:-range})/101>"; return; }
    [[ ${STATUS[$id]:-} == ok ]] || { echo -; return; }
    "$CDO" -s outputf,%.9g -fldmax "${OUTF[$id]}" 2>/dev/null | maxabs | awk '{printf "%.9g", $1/101}'
}

# [TMO=] [GB=] [MEM=] cdors_range PREFIX WORKLOAD SOURCE SELECTION... : timmax - timmin for the bin
# width. First the one-pass chain `sub -timmax SEL -timmin SEL`; if cdors cannot run it, timmax and
# timmin as two cdors runs and their difference with cdo (small files). Sets RANGE to the run id.
cdors_range() {
    local p=$1 wl=$2 src=$3; shift 3
    NOTE="for the bin width" run_cdors "${p}_range" "$wl" "$src" "$O/${p}_range.nc" sub -timmax "$@" -timmin "$@"
    RANGE=${p}_range
    [[ ${STATUS[$RANGE]} == ok || ${STATUS[$RANGE]} == dry ]] && return 0
    NOTE="for the bin width" run_cdors "${p}_timmax" "$wl" "$src" "$O/${p}_timmax.nc" timmax "$@"
    NOTE="for the bin width" run_cdors "${p}_timmin" "$wl" "$src" "$O/${p}_timmin.nc" timmin "$@"
    if [[ ${STATUS[${p}_timmax]} == ok && ${STATUS[${p}_timmin]} == ok ]]; then
        GB=- run "${p}_range2" "$wl" cdo nc "$O/${p}_range2.nc" "$CDO" -f nc4 sub "$O/${p}_timmax.nc" "$O/${p}_timmin.nc" "$O/${p}_range2.nc"
        RANGE=${p}_range2
    fi
}

# make_view SRC OUT VAR [NTIME]: the single-variable symlink view cdo reads (bench/make_view.py)
make_view() {
    [[ -d $2 ]] && return 0
    ((PLAN_ONLY)) && return 0
    if ((DRY)); then echo "[view] $VIEW_PY -I $HERE/make_view.py $*"; return 0; fi
    "$VIEW_PY" -I "$HERE/make_view.py" "$@" >> "$OUT/views.log" 2>&1 || echo "make_view failed: $*" >&2
}

O=$OUT
zarr() { echo "file://$1#mode=zarr,file"; }   # NCZarr URL for cdo

# =================================================================== FIXTURE stand-ins
if ((FIXTURE)); then
    HP=$CDORS_FIXTURES/hpz2_noleap.nc          # HEALPix z2 (cdo: projection grid), 365_day, missing values
    HPZ=$CDORS_FIXTURES/hpz2_noleap.zarr2      # its Zarr v2 copy (cdors reads the "store", cdo the file)
    RG=$CDORS_FIXTURES/r36x18_std.nc           # regular 10 deg grid (sellonlatbox, remap)
    BOX=-30,40,30,75
    D1=2000-01-01T00:00:00 D2=2001-12-31T23:59:59
    if want W1; then
        run_cdors fx1_cdors_store W1 zarr "$O/fx1_cdors_store.nc" yearmean -seldate,$D1,$D2 -selname,tas "$HPZ"
        run_cdo   fx1_cdo         W1 nc   "$O/fx1_cdo.nc"         yearmean -seldate,$D1,$D2 -selname,tas "$HP"
        run_xr    fx1_xr_local    W1 zarr "$O/fx1_xr_local.nc"    w1 "$HPZ" --var tas --start $D1 --end $D2 --workers "$XR_WORKERS"
        compare W1 fx1_cdo fx1_cdors_store ulp 1
        compare W1 fx1_cdo fx1_xr_local ulp
    fi
    if want W2; then
        run "fx2_cdo_gridarea" W2 cdo nc "$O/fx2_gridarea.nc" "$CDO" -f nc4 gridarea -sellonlatbox,$BOX -seltimestep,1 -selname,pr "$RG" "$O/fx2_gridarea.nc"
        run_cdors fx2_cdors_local W2 nc "$O/fx2_cdors_local.nc" fldmean -sellonlatbox,$BOX -selname,pr "$RG"
        run_cdo   fx2_cdo         W2 nc "$O/fx2_cdo.nc"         fldmean -sellonlatbox,$BOX -selname,pr "$RG"
        run_xr    fx2_xr          W2 nc "$O/fx2_xr.nc"          w2 "$RG" --var pr --box=$BOX --weights "$O/fx2_gridarea.nc" --workers "$XR_WORKERS"
        compare W2 fx2_cdo fx2_cdors_local ulp 1
        compare W2 fx2_cdo fx2_xr ulp
    fi
    if want W3; then
        W=$O/fx3_weights_bil_r18x9.nc
        run_cdo   fx3_cdo_genbil  W3 nc "$W" genbil,r18x9 -seltimestep,1 -selname,tas "$RG"
        run_cdors fx3_cdors_remap W3 nc "$O/fx3_cdors_remap.nc" remap,r18x9,"$W" -selname,tas "$RG"
        run_cdors fx3_cdors_remapbil W3 nc "$O/fx3_cdors_remapbil.nc" remapbil,r18x9 -selname,tas "$RG"
        run_cdo   fx3_cdo_remap   W3 nc "$O/fx3_cdo_remap.nc"   remap,r18x9,"$W" -selname,tas "$RG"
        compare W3 fx3_cdo_remap fx3_cdors_remap ulp 1
        compare W3 fx3_cdo_remap fx3_cdors_remapbil ulp 1
    fi
    if want W4Y || want W4; then
        S="-selname,tas $HPZ"
        TMO=$TMO_CDORS_W4 MEM=$CDORS_MEM run_cdors fx4_cdors_timpctl  W4 zarr "$O/fx4_cdors_timpctl.nc" timpctl,95 $S -timmin $S -timmax $S
        TMO=$TMO_CDORS_W4 MEM=$CDORS_MEM run_cdors fx4_cdors_ydaymean W4 zarr "$O/fx4_cdors_ydaymean.nc" ydaymean $S
        TMO=$TMO_CDORS_W4 MEM=$CDORS_MEM cdors_range fx4_cdors W4 zarr $S
        S="-selname,tas $HP"
        TMO=$TMO_CDO_W4 run_cdo fx4_cdo_timpctl  W4 nc "$O/fx4_cdo_timpctl.nc" timpctl,95 $S -timmin $S -timmax $S
        TMO=$TMO_CDO_W4 run_cdo fx4_cdo_ydaymean W4 nc "$O/fx4_cdo_ydaymean.nc" ydaymean $S
        if [[ ${STATUS[$RANGE]:-} != ok && ${STATUS[$RANGE]:-} != dry ]]; then
            NOTE="bin width for the comparison (cdors range not available)" \
                run_cdo fx4_cdo_range W4 nc "$O/fx4_cdo_range.nc" sub -timmax $S -timmin $S
            RANGE=fx4_cdo_range
        fi
        compare W4 fx4_cdo_timpctl fx4_cdors_timpctl "bin:$(binwidth $RANGE)" 1
        compare W4 fx4_cdo_ydaymean fx4_cdors_ydaymean ulp 1
        # the timeout path: must be recorded as did_not_finish
        TMO=1 NOTE="self-test of the timeout path" run fx_timeout_selftest W4 cdo - - sleep 5
    fi
    if ! ((DRY)); then
        echo "== done: $OUT"; column -t -s $'\t' "$OUT/runs.tsv"; column -t -s $'\t' "$OUT/compare.tsv"
        "$VIEW_PY" -I "$HERE/summarize.py" "$OUT" > "$OUT/summary.md" 2> "$OUT/summary.err" && cat "$OUT/summary.md"
    fi
    exit 0
fi

# =================================================================== real workloads
SRC1=/work/kd1453/rechunked_ngc4008/ngc4008_P1D_9.zarr
SRC4=/work/kd1453/rechunked_ngc4008/ngc4008_PT3H_9.zarr
ICON=/work/bm1344/k202193/ICON/erc2002/postprocessing/interpolation/control_1950
REFS=/work/bm1344/k202193/Kerchunk/erc2002/control_1950/v20240618/atm_2d_1d_mean_remap025.parq
W2URL=https://eerie.cloud.dkrz.de/datasets/icon-esm-er.eerie-control-1950.v20240618.atmos.gr025.2d_daily_mean/kerchunk
BOX=-30,40,30,75          # North Atlantic / Europe; the same box as the Task 2 baseline

# ---------------------------------------------------------------- W1: yearmean, HEALPix z9 Zarr, decade 2020-2029
# 3652 daily steps (2020-01-02 .. 2029-12-31, stamped at the end of each day), 45.9 GB decoded.
# cdo reads the single-variable view (bench/datasets.md); cdors and xarray read the 111-variable store.
if want W1; then
    V1=$VIEWS/ngc4008_P1D_9_tas_10y.zarr
    make_view "$SRC1" "$V1" tas 3652
    D1=2020-01-01T00:00:00 D2=2029-12-31T23:59:59
    GB=45.95 NOTE="cold" run_cdors w1_cdors_store W1 local "$O/w1_cdors_store.nc" yearmean -seldate,$D1,$D2 -selname,tas "$SRC1"
    # reads in flight on cold data: the next two decades of the store (each read once before, by the
    # Task 2 probe, as decade 1 was by cdo), at --io-threads 64 (login-node default) and at the default
    GB=46.2 NOTE="cold; reads-in-flight check" IO=64 run_cdors w1_cdors_io64 W1 local "$O/w1_cdors_io64.nc" \
        yearmean -seldate,2030-01-01T00:00:00,2039-12-31T23:59:59 -selname,tas "$SRC1"
    GB=46.2 NOTE="cold; reads-in-flight check" run_cdors w1_cdors_io_default W1 local "$O/w1_cdors_io_default.nc" \
        yearmean -seldate,2040-01-01T00:00:00,2049-12-31T23:59:59 -selname,tas "$SRC1"
    GB=45.95 NOTE="view (cdo's input)" run_cdors w1_cdors_view W1 local "$O/w1_cdors_view.nc" yearmean "$V1"
    GB=45.95 run_cdo w1_cdo W1 local "$O/w1_cdo.nc" yearmean "$(zarr "$V1")"
    GB=45.95 run_xr  w1_xr_local W1 local "$O/w1_xr_local.nc" w1 "$SRC1" --var tas --start $D1 --end $D2 --workers "$XR_WORKERS"
    compare W1 w1_cdo w1_cdors_store ulp 1
    compare W1 w1_cdo w1_cdors_view ulp 1
    compare W1 w1_cdo w1_xr_local ulp
fi

# ---------------------------------------------------------------- W2: fldmean of a box, ICON-ESM-ER daily pr 0.25 deg, decade
# Raw files: model years 1991-2000, 240 files, 3653 steps (15.2 GB decoded, 13.2 GB on disk).
# Kerchunk (Parquet) and cloud relabel the same 3653 chunks as 1950-01-01T12 .. 1960-01-01T12, so
# timestamps are only compared between the two raw-file runs.
if want W2; then
    W2FILES=( "$ICON"/atm_2d_1d_mean_remap025/run_199[1-9]*/*.nc "$ICON"/atm_2d_1d_mean_remap025/run_2000*/*.nc )
    G2A="$ICON/atm_2d_1d_mean_remap025/run_199[1-9]*/*.nc"     # globs passed unexpanded to cdors
    G2B="$ICON/atm_2d_1d_mean_remap025/run_2000*/*.nc"
    D1=1950-01-01T00:00:00 D2=1960-01-01T23:59:59
    run w2_cdo_gridarea W2 cdo local "$O/w2_gridarea.nc" "$CDO" -f nc4 gridarea -sellonlatbox,$BOX -seltimestep,1 -select,name=pr "${W2FILES[0]}" "$O/w2_gridarea.nc"
    GB=15.17 NOTE="cold" run_cdors w2_cdors_parquet W2 local "$O/w2_cdors_parquet.nc" fldmean -sellonlatbox,$BOX -seldate,$D1,$D2 -selname,pr "$REFS"
    GB=15.17 NOTE="same bytes as the Parquet run; includes building the NetCDF-4 chunk index unless cached" \
        run_cdors w2_cdors_raw W2 local "$O/w2_cdors_raw.nc" fldmean -sellonlatbox,$BOX -selname,pr -mergetime "$G2A" "$G2B"
    GB=15.17 run_cdo w2_cdo_raw W2 local "$O/w2_cdo_raw.nc" fldmean -sellonlatbox,$BOX -select,name=pr "${W2FILES[@]}"
    GB=15.17 run_xr  w2_xr_parquet W2 local "$O/w2_xr_parquet.nc" w2 "$REFS" --var pr --box=$BOX --weights "$O/w2_gridarea.nc" \
        --start $D1 --end $D2 --workers "$XR_WORKERS"
    TMO=2400 GB=15.17 run_cdors w2_cdors_cloud W2 cloud "$O/w2_cdors_cloud.nc" fldmean -sellonlatbox,$BOX -seldate,$D1,$D2 -selname,pr "$W2URL"
    TMO=2400 GB=15.17 run_xr w2_xr_cloud W2 cloud "$O/w2_xr_cloud.nc" w2 "$W2URL" --var pr --box=$BOX --weights "$O/w2_gridarea.nc" \
        --start $D1 --end $D2 --workers "$XR_CLOUD_WORKERS"
    compare W2 w2_cdo_raw w2_cdors_raw ulp 1
    compare W2 w2_cdo_raw w2_cdors_parquet ulp
    compare W2 w2_cdo_raw w2_cdors_cloud ulp
    compare W2 w2_cdo_raw w2_xr_parquet ulp
    compare W2 w2_cdo_raw w2_xr_cloud ulp
fi

# ---------------------------------------------------------------- W3: remapbil r360x180 of daily SST, 5 years
# ICON-ESM-ER ocean `to` at 1 m, 0.25 deg regular, run_1991..1995: 120 files, 1826 steps, 7.6 GB decoded.
# Weights generated once by cdo (timed on their own); both tools then apply the same file.
if want W3; then
    W3FILES=( "$ICON"/oce_2d_1d_mean_remap025/run_199[1-5]*/*.nc )
    G3="$ICON/oce_2d_1d_mean_remap025/run_199[1-5]*/*.nc"
    W=$O/w3_weights_bil_r360x180.nc
    run_cdo w3_cdo_genbil W3 local "$W" genbil,r360x180 -seltimestep,1 -select,name=to "${W3FILES[0]}"
    if ((PLAN_ONLY)); then     # run() skips cdo here, but the plan of w3_cdors_remap needs the file (~1 s)
        "$CDO" -s -f nc4 genbil,r360x180 -seltimestep,1 -select,name=to "${W3FILES[0]}" "$W" > "$O/w3_genbil.log" 2>&1
    fi
    GB=7.58 NOTE="cold; cdo's weights" run_cdors w3_cdors_remap W3 local "$O/w3_cdors_remap.nc" remap,r360x180,"$W" -selname,to -mergetime "$G3"
    GB=7.58 NOTE="own weight cache (\$CDORS_CACHE/weights, made by cdo genbil on first use)" \
        run_cdors w3_cdors_remapbil W3 local "$O/w3_cdors_remapbil.nc" remapbil,r360x180 -selname,to -mergetime "$G3"
    GB=7.58 run_cdo w3_cdo_remap W3 local "$O/w3_cdo_remap.nc" remap,r360x180,"$W" -select,name=to "${W3FILES[@]}"
    compare W3 w3_cdo_remap w3_cdors_remap ulp 1
    compare W3 w3_cdo_remap w3_cdors_remapbil ulp 1
fi

# ---------------------------------------------------------------- W4Y: W4 on its first year (2020, 2928 3-hourly steps, 36.8 GB)
# cdo finishes here, so this is where values are checked and a cdo speed-up is measured.
if want W4Y; then
    V4Y=$VIEWS/ngc4008_PT3H_9_tas_1y.zarr
    make_view "$SRC4" "$V4Y" tas 2928
    S="-seltimestep,1/2928 -selname,tas $SRC4"
    GB=36.8 MEM=$CDORS_MEM TMO=$TMO_CDORS_W4 run_cdors w4y_cdors_timpctl  W4Y local "$O/w4y_cdors_timpctl.nc" timpctl,95 $S -timmin $S -timmax $S
    GB=36.8 MEM=$CDORS_MEM TMO=$TMO_CDORS_W4 run_cdors w4y_cdors_ydaymean W4Y local "$O/w4y_cdors_ydaymean.nc" ydaymean $S
    GB=36.8 MEM=$CDORS_MEM TMO=$TMO_CDORS_W4 cdors_range w4y_cdors W4Y local $S
    GB=36.8 TMO=$TMO_CDO_W4 run_cdo w4y_cdo_timpctl  W4Y local "$O/w4y_cdo_timpctl.nc"  timpctl,95 "$(zarr "$V4Y")" -timmin "$(zarr "$V4Y")" -timmax "$(zarr "$V4Y")"
    GB=36.8 TMO=$TMO_CDO_W4 run_cdo w4y_cdo_ydaymean W4Y local "$O/w4y_cdo_ydaymean.nc" ydaymean "$(zarr "$V4Y")"
    if [[ ${STATUS[$RANGE]:-} != ok && ${STATUS[$RANGE]:-} != dry && ${STATUS[w4y_cdo_timpctl]:-} == ok ]]; then
        NOTE="bin width for the comparison (cdors range not available)" \
            run_cdo w4y_cdo_range W4Y local "$O/w4y_cdo_range.nc" sub -timmax "$(zarr "$V4Y")" -timmin "$(zarr "$V4Y")"
        RANGE=w4y_cdo_range
    fi
    compare W4Y w4y_cdo_timpctl w4y_cdors_timpctl "bin:$(binwidth $RANGE)" 1
    compare W4Y w4y_cdo_ydaymean w4y_cdors_ydaymean ulp 1
fi

# ---------------------------------------------------------------- W4: timpctl,95 and ydaymean, PT3H tas, 87664 steps, 1103 GB
# cdors under --mem 32G (peak RSS from /usr/bin/time is the check); cdo through the full
# single-variable view with a timeout: "did_not_finish" is a result. cdo runs last.
if want W4; then
    V4=$VIEWS/ngc4008_PT3H_9_tas_full.zarr
    make_view "$SRC4" "$V4" tas
    S="-selname,tas $SRC4"
    GB=1103 MEM=$CDORS_MEM TMO=$TMO_CDORS_W4 run_cdors w4_cdors_timpctl  W4 local "$O/w4_cdors_timpctl.nc" timpctl,95 $S -timmin $S -timmax $S
    GB=1103 MEM=$CDORS_MEM TMO=$TMO_CDORS_W4 run_cdors w4_cdors_ydaymean W4 local "$O/w4_cdors_ydaymean.nc" ydaymean $S
    GB=1103 TMO=$TMO_CDO_W4 run_cdo w4_cdo_ydaymean W4 local "$O/w4_cdo_ydaymean.nc" ydaymean "$(zarr "$V4")"
    if ((CDO_W4_TIMPCTL)); then
        GB=1103 TMO=$TMO_CDO_W4 run_cdo w4_cdo_timpctl  W4 local "$O/w4_cdo_timpctl.nc"  timpctl,95 "$(zarr "$V4")" -timmin "$(zarr "$V4")" -timmax "$(zarr "$V4")"
    else
        skip w4_cdo_timpctl W4 cdo local "not run: >= 5.3 h extrapolated (30 x 632 s for 2020 in the Task 2 baseline); CDO_W4_TIMPCTL=1 runs it"
    fi
    RANGE=none   # the bin width costs another pass over 1.1 TB: only when there is a cdo result to compare with
    if [[ ${STATUS[w4_cdo_timpctl]:-} == ok || ${STATUS[w4_cdo_timpctl]:-} == dry ]]; then
        GB=1103 MEM=$CDORS_MEM TMO=$TMO_CDORS_W4 cdors_range w4_cdors W4 local $S
    fi
    compare W4 w4_cdo_timpctl w4_cdors_timpctl "bin:$(binwidth $RANGE)" 1
    compare W4 w4_cdo_ydaymean w4_cdors_ydaymean ulp 1
fi

if ! ((DRY)); then
    echo "== $(date +%T) done: $OUT"
    column -t -s $'\t' "$OUT/runs.tsv"
    column -t -s $'\t' "$OUT/compare.tsv"
    "$VIEW_PY" -I "$HERE/summarize.py" "$OUT" > "$OUT/summary.md" 2> "$OUT/summary.err" && cat "$OUT/summary.md"
fi
