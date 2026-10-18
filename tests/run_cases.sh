#!/usr/bin/env bash
# Compare cdors against cdo, row by row, as described in the plan's "Testing Strategy".
#
#   tests/run_cases.sh [cases-file]          default: tests/cases.txt
#
# Row format (tests/cases.txt):   tag | arguments | fixtures      (# starts a comment)
#   tag        exact | ulp | bin | text | json | err:<code> | json:<jq-expr> | ref:<lib>.<fn>[:<tol>...]
#              (arguments and fixtures are the last two |-fields: a jq expression may contain |,
#              but no #)
#   arguments  cdo arguments without the output file; {in} and {in2} are replaced by fixture paths,
#              {fix} and {fx} are replaced by the fixtures directory (weight files and other files
#              made by make_fixtures.sh; rows on several files, e.g. -mergetime).
#              Split on whitespace, no quoting, no globbing (a glob pattern reaches both tools).
#   fixtures   space-separated fixture names (files $CDORS_FIXTURES/<name>.nc); a token "a:b"
#              sets {in}=a and {in2}=b, otherwise {in2}={in}.
#
# json:<jq-expr>  cdors runs (must succeed); /usr/bin/jq evaluates the expression on
#              {"out": <stdout JSON or null>, "stderr": [<stderr lines that are JSON>]}, and every
#              value it yields must be true. E.g.  json:.out.variables | length == 2
#              (the bare tag json only checks that stdout is JSON). {out} in the arguments is an
#              output file in the row's run directory (rows about warnings of file-writing chains).
# ref:<lib>.<fn>  reference from tests/ref/reference.py instead of cdo: function <fn>, run with
#              -I by the Python of the conda environment of <lib> (numpy -> nereus, nereus -> nereus,
#              pyfesom2 -> pyfesom2, regionmask -> aqua; CDORS_REF_ENV_<lib>=<env name or prefix>
#              overrides). Unknown <lib>, missing environment or missing package: FAIL. The output
#              must have the reference's variable names and timestamps (cdo showtimestamp), and
#              every value |cdors - ref| <= abs + rel*|ref| (missing where the reference is
#              missing). Tolerance modifiers: default rel=1e-6, :rel=<x>, :abs=<x> (sets abs, rel
#              stays unless given), :exact (rel=abs=0); loosening needs a reason in a comment.
#              The reference is cached as $CDORS_REF/ref-<key>.nc, key over reference.py, the
#              environment's package list and the versions reference.py loads, lib, fn, arguments,
#              fixture files and every fixture path inside the arguments (mesh directories: name,
#              size and mtime of each file).
#
# Environment (all optional):
#   CDO               reference cdo          (default: spack cdo 2.6.0)
#   CDORS             binary under test      (default: $CDORS_TARGET/release/cdors)
#   CDORS_TARGET      work dir               (default: $CARGO_TARGET_DIR or /work/ab0995/a270088/cdors-target)
#   CDORS_FIXTURES    fixtures               (default: $CDORS_TARGET/fixtures; made by tests/make_fixtures.sh)
#   CDORS_REF         cached cdo outputs     (default: $CDORS_TARGET/cdo-ref)
#   CDORS_JOBS        parallel rows          (default: 8, capped at 16: shared login node)
#   CDORS_ZARR=1      (default) also run rows on a variant fixture (hpz2_noleap, fesom_pi_data) on its
#                     Zarr v2/v3 copies (vs the reference); 0 switches the variant off
#   CDORS_PLANNER=1   (default) also run rows on a variant fixture on the tiny-chunk Zarr copy, and on the
#                     NetCDF fixture itself (one field per chunk), both with "--mem $CDORS_PLANNER_MEM"
#                     (default 1M: finer tiles, lane waves, several passes); each must equal the first
#                     cdors output exactly. CDORS_PLANNER_MEM= drops --mem and the second run.
#   (always)          rows on a fixture <f> with gribscan references <f>.json next to it (the grb_*
#                     fixtures, NetCDF copies cdo made of GRIB files) also run cdors on the references
#                     (variant "grib", vs the cdo reference on the NetCDF copy; compared without
#                     --pedantic, since gribscan drops single levels such as 2 m or mean sea level that
#                     cdo keeps as z-axes, and cdo's pedantic diffn aborts on "different levels"; grid
#                     types are checked by the griddes text rows)
#   CDORS_THREADS     thread options of every cdors run   (default: -P 2 --io-threads 2)
#   CDORS_THREADS_TINY  ... of the tiny-chunk run         (default: -P 3 --io-threads 3; different
#                     from the base run, so the planner check also checks thread-count invariance)
#   CDORS_REGIONS     region catalog for cdors (default: $CDORS_FIXTURES/regions, filled by Task 6)
#   CDORS_REF_ENVS    conda environments of the ref: rows (default: /work/ab0995/a270088/mambaforge/envs)
#
# err rows: the output "$dir/out.nc" is appended unless the arguments contain {noout} (removed).
# {cdors:OPT} passes OPT to cdors only (options cdo does not have, e.g. {cdors:--lonlat}); such rows
# compare values on cdo's grid (cdo -setgrid,<cdo output>), since the option may change how cdo
# reads the grid.
# text rows of showname compare the names order-insensitively on the Zarr and tiny variants
# (Zarr stores have no variable order); values of these variants are compared variable by variable.
#
# Self-tests of the harness: tests/selftest.txt and tests/selftest_fake.txt (commands in their
# headers), tests/selftest_ref.sh (json: and ref: tags).
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
export CDORS_ZARR=${CDORS_ZARR:-1} CDORS_PLANNER=${CDORS_PLANNER:-1} CDORS_PLANNER_MEM=${CDORS_PLANNER_MEM-1M}
# modest threads: rows run in parallel on a login node with a per-user thread limit
export CDORS_THREADS=${CDORS_THREADS--P 2 --io-threads 2} CDORS_THREADS_TINY=${CDORS_THREADS_TINY--P 3 --io-threads 3}
export CDORS_REGIONS=${CDORS_REGIONS:-$CDORS_FIXTURES/regions}
export CDORS_REF_ENVS=${CDORS_REF_ENVS:-/work/ab0995/a270088/mambaforge/envs}
VARIANT_FIXTURES="hpz2_noleap fesom_pi_data"   # rows on these also run the Zarr and planner variants
# reference Pythons: one thread each (NumPy's BLAS would start one per core)
PYENV=(env OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 MKL_NUM_THREADS=1 PYTHONWARNINGS=ignore)
REFPY=$HERE/ref/reference.py

# ---------------------------------------------------------------- one row x fixture
have() { [[ -x $1 ]] || command -v "$1" >/dev/null 2>&1; }
sig() { stat -c '%n %s %Y' "$1" 2>/dev/null; }   # file identity for cache keys
maxabs() { awk '{for(i=1;i<=NF;i++){v=$i<0?-$i:$i; if(v<1e30 && v>m)m=v}} END{printf "%.9g\n", m+0}'; }
ref_env() {  # ref_env LIB: prefix of the conda environment of a ref: library
  [[ $1 =~ ^[A-Za-z0-9_]+$ ]] || return 1
  local v=CDORS_REF_ENV_$1 e
  e=${!v:-}
  if [[ -z $e ]]; then
    case $1 in numpy|nereus) e=nereus ;; pyfesom2) e=pyfesom2 ;; regionmask) e=aqua ;; *) return 1 ;; esac
  fi
  [[ $e == /* ]] || e=$CDORS_REF_ENVS/$e
  echo "$e"
}
env_sig() {  # env_sig LIB: what the cache keys of LIB's references know about its environment
  local e; e=$(ref_env "$1") || { echo unknown; return; }
  [[ -x $e/bin/python ]] || { echo "missing $e"; return; }
  ls "$e/conda-meta" 2>/dev/null
  "${PYENV[@]}" "$e/bin/python" -I "$REFPY" version "$1" 2>/dev/null
}
path_sig() {  # path_sig WORD...: identity of every fixture path in the words (directories: each file)
  local p
  for p in $(grep -oE "$CDORS_FIXTURES/[^ ,:=]+" <<< "$*" | sort -u); do
    if [[ -d $p ]]; then (cd "$p" && find . -printf '%P %s %T@\n' | sort); else sig "$p"; fi
  done
}

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
  [[ $tag == err:* || $tag == json* ]] || have "$CDO" || skip "cdo not found ($CDO)"

  args_for() {  # args_for IN1 IN2 -> fills arrays A (cdo) and R (cdors: with the {cdors:...} options)
    local a=${args//\{in\}/$1}; a=${a//\{in2\}/$2}; a=${a//\{fix\}/$CDORS_FIXTURES}; a=${a//\{fx\}/$CDORS_FIXTURES}
    a=${a//\{out\}/$dir/out.nc}
    local -a all; read -ra all <<< "$a"
    A=(); R=()
    local t
    for t in "${all[@]}"; do
      if [[ $t == '{cdors:'*'}' ]]; then t=${t#\{cdors:}; R+=("${t%\}}"); else A+=("$t"); R+=("$t"); fi
    done
  }
  local -a A R; args_for "$in1" "$in2"
  local -a T; read -ra T <<< "$CDORS_THREADS"

  case $tag in
  err:*)
    local code=${tag#err:} noout=0 a
    local -a O=()
    for a in "${R[@]}"; do [[ $a == '{noout}' ]] && noout=1 || O+=("$a"); done
    ((noout)) || O+=("$dir/out.nc")
    "$CDORS" "${T[@]}" --json "${O[@]}" > "$dir/out.json" 2> "$dir/err.json"
    local rc=$?
    { echo "exit $rc"; cat "$dir/out.json" "$dir/err.json"; } >> "$log"
    ((rc != 0)) || fail "cdors succeeded, expected error $code"
    grep -qE "\"error\" *: *\"$code\"" "$dir/out.json" "$dir/err.json" || fail "no \"error\": \"$code\" in JSON output"
    pass ;;
  json)
    "$CDORS" "${T[@]}" "${R[@]}" > "$dir/out.json" 2>> "$log" || fail "cdors failed"
    python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$dir/out.json" 2>> "$log" || fail "output is not valid JSON"
    pass ;;
  json:*)
    local expr=${tag#json:} got
    "$CDORS" "${T[@]}" "${R[@]}" > "$dir/out.json" 2> "$dir/err.json"
    local rc=$?
    { echo "exit $rc"; cat "$dir/err.json"; echo "jq: [$expr]"; } >> "$log"
    ((rc == 0)) || fail "cdors failed"
    /usr/bin/jq -n --rawfile o "$dir/out.json" --rawfile e "$dir/err.json" \
      '{out: (if ($o | test("^\\s*$")) then null else ($o | fromjson) end),
        stderr: [$e | split("\n")[] | fromjson?]}' > "$dir/in.json" 2>> "$log" || fail "stdout is not JSON"
    got=$(/usr/bin/jq -c "[$expr]" "$dir/in.json" 2>> "$log") || fail "jq expression failed"
    echo "values: $got" >> "$log"
    [[ $(/usr/bin/jq '. != [] and all(. == true)' <<< "$got") == true ]] || fail "not true: ${got:0:200}"
    pass ;;
  ref:*)
    local -a mods; IFS=: read -ra mods <<< "${tag#ref:}"
    local lib=${mods[0]%%.*} fn=${mods[0]#*.} rel=1e-6 abs=0 m
    [[ -n $lib && -n $fn && $fn != "${mods[0]}" ]] || fail "bad tag $tag (ref:<lib>.<fn>)"
    for m in "${mods[@]:1}"; do
      case $m in
        exact) rel=0 abs=0 ;; rel=*) rel=${m#rel=} ;; abs=*) abs=${m#abs=} ;;
        *) fail "unknown ref modifier '$m' (exact, rel=<x>, abs=<x>)" ;;
      esac
    done
    local env; env=$(ref_env "$lib") || fail "unknown reference library '$lib' (numpy, nereus, pyfesom2, regionmask)"
    local py=$env/bin/python
    [[ -x $py ]] || fail "reference environment of '$lib' missing: $env"
    ;;
  exact|ulp|bin|text) ;;
  *) fail "unknown tag $tag" ;;
  esac

  mkdir -p "$CDORS_REF"
  local kind=nc; [[ $tag == text ]] && kind=txt
  local key ref refts
  if [[ $tag == ref:* ]]; then
    # cached reference.py output, keyed by reference.py, environment, row and fixture identity
    key=$(printf '%s|' ref "$(sha1sum < "$REFPY")" "$(cat "$RUN/envsig-$lib" 2>/dev/null)" "$lib" "$fn" \
      "$args" "$(sig "$in1")" "$(sig "$in2")" "$(path_sig "${R[@]}")" | sha1sum | cut -c1-16)
    ref=$CDORS_REF/ref-$key.nc refts=$CDORS_REF/ref-$key.ts
    if [[ ! -e $ref ]]; then
      printf 'ref:%s.%s\n%s | %s\n' "$lib" "$fn" "$args" "$fx" > "$CDORS_REF/ref-$key.row"
      "${PYENV[@]}" "$py" -I "$REFPY" run "$lib" "$fn" "$ref.tmp$$.nc" "$in1" "$in2" "${R[@]}" >> "$log" 2>&1
      local rc=$?
      ((rc == 3)) && fail "package '$lib' missing in $env"
      ((rc == 0)) || fail "reference failed ($lib.$fn)"
      "$CDO" -s showtimestamp "$ref.tmp$$.nc" > "$refts.tmp$$" 2>> "$log" || fail "cdo cannot read the reference"
      mv "$refts.tmp$$" "$refts"; mv "$ref.tmp$$.nc" "$ref"   # the .nc last: complete when it exists
    fi
  else
    # cached cdo reference, keyed by cdo version, row and fixture identity
    key=$(printf '%s|%s|%s|%s|%s' "$CDOVER" "$kind" "$args" "$(sig "$in1")" "$(sig "$in2")" | sha1sum | cut -c1-16)
    ref=$CDORS_REF/$key.$kind refts=$CDORS_REF/$key.ts
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
  fi

  # cdors run(s): base, then the variants on the variant fixtures
  local -a runs=("base|$in1|$in2|$CDORS_THREADS")
  if [[ " $VARIANT_FIXTURES " == *" $f1 "* ]]; then
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
      # the NetCDF fixture (one field per chunk) under the small budget: finer tiles, lane waves
      # and several passes over the input
      [[ -n $CDORS_PLANNER_MEM ]] && runs+=("mem|$in1|$in2|$CDORS_THREADS_TINY --mem $CDORS_PLANNER_MEM")
    fi
  fi
  # GRIB fixtures: cdors also reads gribscan's references to the GRIB file the fixture was made from
  local gp=$CDORS_FIXTURES/$f1.json
  if [[ -e $gp ]]; then
    local g2=$in2; [[ $f2 == "$f1" ]] && g2=$gp
    runs+=("grib|$gp|$g2|$CDORS_THREADS")
  fi

  local abslim=0
  case $tag in
  ulp)  # 2 float32 ulps at the reference's largest magnitude (all variables and levels)
    if [[ ! -e $CDORS_REF/$key.ulp ]]; then
      # variable by variable: outputf aborts on files with several grids (FESOM nodes + elements)
      for v in $("$CDO" -s showname "$ref" 2>> "$log"); do
        "$CDO" -s outputf,%.9g -timmax -fldmax -abs -selname,"$v" "$ref" 2>> "$log"
      done | maxabs \
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
    echo "== $name: $CDORS ${X[*]} ${R[*]}" >> "$log"
    if [[ $tag == text ]]; then
      local out=$dir/out_$name.txt
      "$CDORS" "${X[@]}" "${R[@]}" > "$out" 2>> "$log" || fail "cdors failed ($name)"
      local want=$ref; [[ $name == tiny || $name == mem ]] && want=$base_out
      if [[ $name != base && ${A[0]} == showname ]]; then
        diff -u <(tr -s ' \n' '\n\n' < "$want" | sort) <(tr -s ' \n' '\n\n' < "$out" | sort) >> "$log" || fail "names differ ($name)"
      else
        diff -u "$want" "$out" >> "$log" || fail "text differs ($name)"
      fi
    else
      local out=$dir/out_$name.nc
      "$CDORS" --no_history "${X[@]}" "${R[@]}" "$out" >> "$log" 2>&1 || fail "cdors failed ($name)"
      local want=$ref lim=$abslim wantts=$refts
      if [[ $name == tiny || $name == mem ]]; then
        want=$base_out lim=0 wantts=$dir/ts_base
      fi
      local -a ped=(--pedantic); [[ $name == grib ]] && ped=()
      local -a og=()  # rows with cdors-only options: values on cdo's grid
      [[ $args == *'{cdors:'* && $name != tiny && $name != mem ]] && og=(-setgrid,"$want")
      if [[ $tag == ref:* && $want == "$ref" ]]; then  # same names, then values within the tolerance
        local wn on
        wn=$("$CDO" -s showname "$want" 2>> "$log" | tr -s ' ' '\n' | sed '/^$/d' | sort)
        on=$("$CDO" -s showname "$out" 2>> "$log" | tr -s ' ' '\n' | sed '/^$/d' | sort)
        [[ $wn == "$on" ]] || { echo "names: want [$wn] got [$on]" >> "$log"; fail "variables differ ($name)"; }
        # shellcheck disable=SC2086
        "${PYENV[@]}" "$py" -I "$REFPY" compare "$want" "$out" "$rel" "$abs" $wn >> "$log" 2>&1 \
          || fail "values differ from the reference ($name, rel=$rel abs=$abs)"
      elif [[ $name == base ]]; then
        "$CDO" --pedantic diffn,abslim="$lim" "$want" "${og[@]}" "$out" >> "$log" 2>&1 || fail "values differ ($name, abslim=$lim)"
      else  # Zarr inputs have no variable order: same names, then each variable by name
        local wn on v
        wn=$("$CDO" -s showname "$want" 2>> "$log" | tr -s ' ' '\n' | sed '/^$/d' | sort)
        on=$("$CDO" -s showname "$out" 2>> "$log" | tr -s ' ' '\n' | sed '/^$/d' | sort)
        [[ $wn == "$on" ]] || { echo "names: want [$wn] got [$on]" >> "$log"; fail "variables differ ($name)"; }
        for v in $wn; do
          "$CDO" "${ped[@]}" diffn,abslim="$lim" -selname,"$v" "$want" -selname,"$v" "${og[@]}" "$out" >> "$log" 2>&1 || fail "values differ ($name, $v, abslim=$lim)"
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
  # tag | arguments | fixtures: the last two fields are |-free, the tag (json:<jq>) may hold |
  [[ $line == *'|'*'|'* ]] || { echo "line $n: not 'tag | arguments | fixtures', ignored" >&2; continue; }
  fixtures=${line##*|}; line=${line%|*}; args=${line##*|}; tag=${line%|*}
  tag=${tag#"${tag%%[![:space:]]*}"}; tag=${tag%"${tag##*[![:space:]]}"}; args=$(echo $args)
  for fx in $fixtures; do printf '%s\t%s\t%s\t%s\n' "$n" "$tag" "$args" "$fx"; done
done < "$CASES" > "$RUN/jobs.tsv"
njobs=$(wc -l < "$RUN/jobs.tsv")
# environment signatures of the ref: libraries (part of their cache keys), once per run
for lib in $(awk -F'\t' '$2 ~ /^ref:/ {s = substr($2, 5); sub(/[.:].*/, "", s); print s}' "$RUN/jobs.tsv" | sort -u); do
  env_sig "$lib" > "$RUN/envsig-$lib" &
done
wait

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
