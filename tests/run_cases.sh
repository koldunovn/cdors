#!/usr/bin/env bash
# Compare cdors against cdo, row by row, as described in the plan's "Testing Strategy".
#
#   tests/run_cases.sh [cases-file]          default: tests/cases.txt
#
# Row format (tests/cases.txt):   tag | arguments | fixtures      (# starts a comment)
#   tag        exact | ulp | bin | text | json | err:<code>
#   arguments  cdo arguments without the output file; {in} and {in2} are replaced by fixture paths,
#              {fx} by the fixture directory (for rows on several files, e.g. -mergetime).
#              Split on whitespace, no quoting, no globbing (a glob pattern reaches both tools).
#   fixtures   space-separated fixture names (files $CDORS_FIXTURES/<name>.nc); a token "a:b"
#              sets {in}=a and {in2}=b, otherwise {in2}={in}.
#
# Environment (all optional):
#   CDO               reference cdo          (default: spack cdo 2.6.0)
#   CDORS             binary under test      (default: $CDORS_TARGET/release/cdors)
#   CDORS_TARGET      work dir               (default: $CARGO_TARGET_DIR or /work/ab0995/a270088/cdors-target)
#   CDORS_FIXTURES    fixtures               (default: $CDORS_TARGET/fixtures; made by tests/make_fixtures.sh)
#   CDORS_REF         cached cdo outputs     (default: $CDORS_TARGET/cdo-ref)
#   CDORS_JOBS        parallel rows          (default: 8, capped at 16: shared login node)
#   CDORS_ZARR=1      (default) also run rows on hpz2_noleap on its Zarr v2/v3 copies (vs the cdo
#                     reference); 0 switches the variant off
#   CDORS_PLANNER=1   (default) also run rows on hpz2_noleap on the tiny-chunk Zarr copy; must equal
#                     the first cdors output exactly. CDORS_PLANNER_MEM=1M adds "--mem 1M" to that run.
#   CDORS_THREADS     thread options of every cdors run   (default: -P 2 --io-threads 2)
#   CDORS_THREADS_TINY  ... of the tiny-chunk run         (default: -P 3 --io-threads 3; different
#                     from the base run, so the planner check also checks thread-count invariance)
#
# err rows: the output "$dir/out.nc" is appended unless the arguments contain {noout} (removed).
# text rows of showname compare the names order-insensitively on the Zarr and tiny variants
# (Zarr stores have no variable order); values of these variants are compared variable by variable.
#
# Every run writes into a fresh directory $CDORS_TARGET/runs/<timestamp>-<pid>; nothing is deleted.
set -uo pipefail
set -f

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
export CDO=${CDO:-/sw/spack-levante/cdo-2.6.0-akkxhz/bin/cdo}
export CDORS_TARGET=${CDORS_TARGET:-${CARGO_TARGET_DIR:-/work/ab0995/a270088/cdors-target}}
export CDORS_FIXTURES=${CDORS_FIXTURES:-$CDORS_TARGET/fixtures}
export CDORS=${CDORS:-$CDORS_TARGET/release/cdors}
export CDORS_REF=${CDORS_REF:-$CDORS_TARGET/cdo-ref}
export CDORS_ZARR=${CDORS_ZARR:-1} CDORS_PLANNER=${CDORS_PLANNER:-1} CDORS_PLANNER_MEM=${CDORS_PLANNER_MEM:-}
# modest threads: rows run in parallel on a login node with a per-user thread limit
export CDORS_THREADS=${CDORS_THREADS--P 2 --io-threads 2} CDORS_THREADS_TINY=${CDORS_THREADS_TINY--P 3 --io-threads 3}
VARIANT_FIXTURE=hpz2_noleap

# ---------------------------------------------------------------- one row x fixture
have() { [[ -x $1 ]] || command -v "$1" >/dev/null 2>&1; }
sig() { stat -c '%n %s %Y' "$1" 2>/dev/null; }   # file identity for cache keys
maxabs() { awk '{for(i=1;i<=NF;i++){v=$i<0?-$i:$i; if(v<1e30 && v>m)m=v}} END{printf "%.9g\n", m+0}'; }

run_job() {
  local i=$1 lineno tag args fx
  IFS=$'\t' read -r lineno tag args fx < <(sed -n "${i}p" "$RUN/jobs.tsv")
  local dir; dir=$RUN/$(printf '%03d' "$i")_${fx//:/+}
  mkdir -p "$dir"
  local log=$dir/log
  echo "line $lineno: $tag | $args | $fx" > "$dir/row"
  result() { printf '%s\t%s\t%s\n' "$1" "line $lineno: $tag | $args | $fx" "$2" > "$dir/result"; }
  pass() { result PASS ""; exit 0; }
  fail() { result FAIL "$1 -> $log"; exit 0; }
  skip() { result SKIP "$1"; exit 0; }

  local f1=${fx%%:*} f2=${fx#*:}
  local in1=$CDORS_FIXTURES/$f1.nc in2=$CDORS_FIXTURES/$f2.nc
  [[ -e $in1 && -e $in2 ]] || skip "fixture $f1/$f2 missing (run tests/make_fixtures.sh)"
  have "$CDORS" || skip "cdors not found ($CDORS)"
  [[ $tag == err:* || $tag == json ]] || have "$CDO" || skip "cdo not found ($CDO)"

  args_for() {  # args_for IN1 IN2 -> fills array A
    local a=${args//\{in\}/$1}; a=${a//\{in2\}/$2}; a=${a//\{fx\}/$CDORS_FIXTURES}
    read -ra A <<< "$a"
  }
  local A; args_for "$in1" "$in2"
  local -a T; read -ra T <<< "$CDORS_THREADS"

  case $tag in
  err:*)
    local code=${tag#err:} noout=0 a
    local -a O=()
    for a in "${A[@]}"; do [[ $a == '{noout}' ]] && noout=1 || O+=("$a"); done
    ((noout)) || O+=("$dir/out.nc")
    "$CDORS" "${T[@]}" --json "${O[@]}" > "$dir/out.json" 2> "$dir/err.json"
    local rc=$?
    { echo "exit $rc"; cat "$dir/out.json" "$dir/err.json"; } >> "$log"
    ((rc != 0)) || fail "cdors succeeded, expected error $code"
    grep -qE "\"error\" *: *\"$code\"" "$dir/out.json" "$dir/err.json" || fail "no \"error\": \"$code\" in JSON output"
    pass ;;
  json)
    "$CDORS" "${T[@]}" "${A[@]}" > "$dir/out.json" 2>> "$log" || fail "cdors failed"
    python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$dir/out.json" 2>> "$log" || fail "output is not valid JSON"
    pass ;;
  exact|ulp|bin|text) ;;
  *) fail "unknown tag $tag" ;;
  esac

  # cached cdo reference, keyed by cdo version, row and fixture identity
  mkdir -p "$CDORS_REF"
  local kind=nc; [[ $tag == text ]] && kind=txt
  local key; key=$(printf '%s|%s|%s|%s|%s' "$CDOVER" "$kind" "$args" "$(sig "$in1")" "$(sig "$in2")" | sha1sum | cut -c1-16)
  local ref=$CDORS_REF/$key.$kind
  if [[ ! -e $ref ]]; then
    printf '%s\n%s | %s\n' "$CDOVER" "$args" "$fx" > "$CDORS_REF/$key.row"
    if [[ $kind == txt ]]; then
      "$CDO" "${A[@]}" > "$ref.tmp$$" 2>> "$log" || fail "cdo failed (reference)"
    else
      "$CDO" -s --no_history "${A[@]}" "$ref.tmp$$.nc" >> "$log" 2>&1 || fail "cdo failed (reference)"
      "$CDO" -s showtimestamp "$ref.tmp$$.nc" > "$CDORS_REF/$key.ts" 2>> "$log"
      mv "$ref.tmp$$.nc" "$ref.tmp$$"
    fi
    mv "$ref.tmp$$" "$ref"   # atomic: a reference exists only when complete
  fi

  # cdors run(s): base, then the variants on the variant fixture
  local -a runs=("base|$in1|$in2|$CDORS_THREADS")
  if [[ $f1 == "$VARIANT_FIXTURE" ]]; then
    local z2=$in2
    if [[ $CDORS_ZARR == 1 ]]; then
      for z in zarr2 zarr3; do
        local zp=$CDORS_FIXTURES/$f1.$z
        [[ $f2 == "$f1" ]] && z2=$zp
        [[ -e $zp ]] && runs+=("$z|$zp|$z2|$CDORS_THREADS") || echo "variant $z skipped: $zp missing" >> "$log"
      done
    fi
    if [[ $CDORS_PLANNER == 1 ]]; then
      local tp=$CDORS_FIXTURES/${f1}_tiny.zarr2; [[ $f2 == "$f1" ]] && z2=$tp
      [[ -e $tp ]] && runs+=("tiny|$tp|$z2|$CDORS_THREADS_TINY${CDORS_PLANNER_MEM:+ --mem $CDORS_PLANNER_MEM}") || echo "planner variant skipped: $tp missing" >> "$log"
    fi
  fi

  local abslim=0
  case $tag in
  ulp)  # 2 float32 ulps at the reference's largest magnitude (all variables and levels)
    if [[ ! -e $CDORS_REF/$key.ulp ]]; then
      "$CDO" -s outputf,%.9g -timmax -fldmax -abs "$ref" 2>> "$log" | maxabs \
        | awk '{printf "%.9g\n", 2*2^-24*$1}' > "$CDORS_REF/$key.ulp.tmp$$" && mv "$CDORS_REF/$key.ulp.tmp$$" "$CDORS_REF/$key.ulp"
    fi
    abslim=$(cat "$CDORS_REF/$key.ulp") ;;
  bin)  # largest histogram bin width of the input: max over grid of (timmax - timmin) / 101
    local bkey; bkey=$(printf '%s|bin|%s' "$CDOVER" "$(sig "$in1")" | sha1sum | cut -c1-16)
    if [[ ! -e $CDORS_REF/$bkey.bin ]]; then
      "$CDO" -s outputf,%.9g -fldmax -sub -timmax "$in1" -timmin "$in1" 2>> "$log" | maxabs \
        | awk '{printf "%.9g\n", $1/101}' > "$CDORS_REF/$bkey.bin.tmp$$" && mv "$CDORS_REF/$bkey.bin.tmp$$" "$CDORS_REF/$bkey.bin"
    fi
    abslim=$(cat "$CDORS_REF/$bkey.bin") ;;
  esac
  echo "abslim=$abslim ref=$ref" >> "$log"

  local r name ri1 ri2 extra base_out=""
  for r in "${runs[@]}"; do
    IFS='|' read -r name ri1 ri2 extra <<< "$r"
    args_for "$ri1" "$ri2"
    local -a X=(); [[ -n $extra ]] && read -ra X <<< "$extra"
    echo "== $name: $CDORS ${X[*]} ${A[*]}" >> "$log"
    if [[ $tag == text ]]; then
      local out=$dir/out_$name.txt
      "$CDORS" "${X[@]}" "${A[@]}" > "$out" 2>> "$log" || fail "cdors failed ($name)"
      local want=$ref; [[ $name == tiny ]] && want=$base_out
      if [[ $name != base && ${A[0]} == showname ]]; then
        diff -u <(tr -s ' \n' '\n\n' < "$want" | sort) <(tr -s ' \n' '\n\n' < "$out" | sort) >> "$log" || fail "names differ ($name)"
      else
        diff -u "$want" "$out" >> "$log" || fail "text differs ($name)"
      fi
    else
      local out=$dir/out_$name.nc
      "$CDORS" --no_history "${X[@]}" "${A[@]}" "$out" >> "$log" 2>&1 || fail "cdors failed ($name)"
      local want=$ref lim=$abslim wantts=$CDORS_REF/$key.ts
      if [[ $name == tiny ]]; then
        want=$base_out lim=0 wantts=$dir/ts_base
      fi
      if [[ $name == base ]]; then
        "$CDO" --pedantic diffn,abslim="$lim" "$want" "$out" >> "$log" 2>&1 || fail "values differ ($name, abslim=$lim)"
      else  # Zarr inputs have no variable order: same names, then each variable by name
        local wn on v
        wn=$("$CDO" -s showname "$want" 2>> "$log" | tr -s ' ' '\n' | sed '/^$/d' | sort)
        on=$("$CDO" -s showname "$out" 2>> "$log" | tr -s ' ' '\n' | sed '/^$/d' | sort)
        [[ $wn == "$on" ]] || { echo "names: want [$wn] got [$on]" >> "$log"; fail "variables differ ($name)"; }
        for v in $wn; do
          "$CDO" --pedantic diffn,abslim="$lim" -selname,"$v" "$want" -selname,"$v" "$out" >> "$log" 2>&1 || fail "values differ ($name, $v, abslim=$lim)"
        done
      fi
      "$CDO" -s showtimestamp "$out" > "$dir/ts_$name" 2>> "$log"
      diff "$wantts" "$dir/ts_$name" >> "$log" || fail "timestamps differ ($name)"
    fi
    [[ $name == base ]] && base_out=$out
  done
  pass
}

if [[ ${1:-} == __job ]]; then RUN=$2 CDOVER=$3; run_job "$4"; exit 0; fi

# ---------------------------------------------------------------- driver
CASES=${1:-$HERE/cases.txt}
JOBS=${CDORS_JOBS:-8}; ((JOBS > 16)) && JOBS=16
t0=$(date +%s.%N)
RUN=$CDORS_TARGET/runs/$(date +%Y%m%d-%H%M%S)-$$
mkdir -p "$RUN"
CDOVER=$("$CDO" --version 2>&1 | head -1) || CDOVER=none

# expand rows x fixtures into jobs.tsv: lineno, tag, args, fixture
n=0
while IFS= read -r line || [[ -n $line ]]; do
  ((n++))
  line=${line%%#*}
  [[ $line =~ [^[:space:]] ]] || continue
  IFS='|' read -r tag args fixtures <<< "$line"
  tag=$(echo $tag); args=$(echo $args)
  for fx in $fixtures; do printf '%s\t%s\t%s\t%s\n' "$n" "$tag" "$args" "$fx"; done
done < "$CASES" > "$RUN/jobs.tsv"
njobs=$(wc -l < "$RUN/jobs.tsv")

seq 1 "$njobs" | xargs -P "$JOBS" -I{} "$0" __job "$RUN" "$CDOVER" {}

set +f; cat "$RUN"/*/result > "$RUN/results.tsv" 2>/dev/null; set -f
npass=$(grep -c '^PASS' "$RUN/results.tsv"); nfail=$(grep -c '^FAIL' "$RUN/results.tsv"); nskip=$(grep -c '^SKIP' "$RUN/results.tsv")
nmissing=$((njobs - npass - nfail - nskip))
awk -F'\t' '$1=="SKIP"{print "skip: " $3}' "$RUN/results.tsv" | sort | uniq -c | sed 's/^ */  /'
awk -F'\t' '$1=="FAIL"{print "FAIL " $2 "\n     " $3}' "$RUN/results.tsv"
((nmissing > 0)) && echo "ERROR: $nmissing job(s) left no result (see $RUN)"
printf 'cases: %d passed, %d failed, %d skipped in %.1fs (%s, %s)\n' "$npass" "$nfail" "$nskip" \
  "$(awk -v a="$t0" -v b="$(date +%s.%N)" 'BEGIN{print b-a}')" "$(basename "$CASES")" "$RUN"
((nfail == 0 && nmissing == 0))
