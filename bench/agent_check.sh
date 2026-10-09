#!/bin/bash
# =====================================================================================================
# DO NOT RUN without Nikolay's explicit go-ahead; estimated ~0.3–0.8M tokens for 10 sessions
# (fresh input + output tokens; in addition roughly 3–8M cache-read tokens, which are cheaper but count
# towards usage limits). Run a ONE-SESSION PILOT first and re-estimate from its result record:
#     RUN=1 TASKS=T5 ARMS=A bench/agent_check.sh
# =====================================================================================================
#
# Agent check (plan Task 14): five analysis tasks (bench/agent_tasks.md), each given to a headless
# `claude -p` session twice: arm A with cdors, arm B with cdo + Python/xarray. Sessions run strictly one
# after another, each in its own fresh scratch directory, and are scored against the reference answers.
#
# Usage:
#   bench/agent_check.sh               DRY RUN (default): checks the tools, prints every prompt and the
#                                      exact claude command; calls no claude, writes nothing
#   SELFTEST=1 bench/agent_check.sh    scores the synthetic transcripts in bench/agent/selftest/ (no claude)
#   RUN=1 bench/agent_check.sh         runs the sessions (needs Nikolay's go-ahead, see above)
#
# Environment (defaults in brackets):
#   TASKS ["T1 T2 T3 T4 T5"]  ARMS ["A B"]  RUN_ID [YYYYmmdd-HHMMSS]
#   TIME_LIMIT [1800] wall seconds per session (timeout; the transcript so far is kept and scored)
#   MAX_TURNS [50] passed as --max-turns (empty: not passed)
#   MAX_BUDGET_USD [] passed as --max-budget-usd when set (only enforced for API-key billing)
#   MODEL [] / EFFORT []  passed as --model / --effort when set; use the same for both arms
#   CDORS_BIN [/work/ab0995/a270088/cdors-target/release/cdors]  REPO [/home/a/a270088/cdo]
#   DOCS [full]  documentation offered to arm A: full = README and docs/deviations.md copied into the
#                session; guide = only `cdors guide` (the 5 kB guide for agents) besides help and ops
#   BASE [/scratch/a/a270088/cdors-agentcheck]   sessions go to $BASE/<run-id>/<task>-<arm>/
#   RESULTS_DIR [bench/agent]   CLAUDE_BIN [claude on PATH; a stub for testing the RUN path without claude]
#
# Outputs: per session prompt.txt, command.txt, transcript.jsonl (stream-json), stderr.txt, exit.txt in the
# session directory; bench/agent/results-<run-id>.tsv (one row per session) and
# bench/agent/results-<run-id>.md (summary). The script never deletes or overwrites anything: it refuses
# to reuse an existing session directory or results file. No session ids or links are written to the
# results files.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
TASKS_MD=$HERE/agent_tasks.md
SCORE=$HERE/agent/agent_score.py
RUN=${RUN:-0}
SELFTEST=${SELFTEST:-0}
TASKS=${TASKS:-T1 T2 T3 T4 T5}
ARMS=${ARMS:-A B}
RUN_ID=${RUN_ID:-$(date +%Y%m%d-%H%M%S)}
TIME_LIMIT=${TIME_LIMIT:-1800}
MAX_TURNS=${MAX_TURNS-50}
MAX_BUDGET_USD=${MAX_BUDGET_USD:-}
MODEL=${MODEL:-}
EFFORT=${EFFORT:-}
CDORS_BIN=${CDORS_BIN:-/work/ab0995/a270088/cdors-target/release/cdors}
REPO=${REPO:-/home/a/a270088/cdo}
BASE=${BASE:-/scratch/a/a270088/cdors-agentcheck}
CDO_BIN=/sw/spack-levante/cdo-2.6.0-akkxhz/bin/cdo
PY_BIN=/work/ab0995/a270088/mambaforge/envs/hk25/bin/python
NCDUMP_BIN=/sw/spack-levante/netcdf-c-main-k4lh4v/bin/ncdump
HDF5_PLUGINS=/sw/spack-levante/netcdf-c-main-bdxvs5/plugins:/sw/spack-levante/netcdf-c-main-k4lh4v/plugins
SYS_PATH=/usr/local/bin:/usr/bin:/bin

die() { echo "agent_check: $*" >&2; exit 1; }
py() { python3 -I "$SCORE" "$@"; }

# ---------------------------------------------------------------------------------------------- selftest
if [ "$SELFTEST" = 1 ]; then
  out=${SELFTEST_DIR:-/scratch/a/a270088/cdors-agentcheck/prep/selftest-$RUN_ID}
  [ -e "$out" ] && die "$out exists; refusing to touch it"
  mkdir -p "$out"
  tsv=$out/results-selftest.tsv
  for f in "$HERE"/agent/selftest/*.jsonl; do
    name=$(basename "$f" .jsonl)                  # <task>-<arm>-<what>
    task=${name%%-*}; rest=${name#*-}; arm=${rest%%-*}
    py score "$TASKS_MD" "$task" "$arm" "$f" "$tsv" selftest 12.3 0 || die "scoring $f failed"
  done
  py summary "$tsv" "$out/results-selftest.md"
  echo "selftest outputs in $out"
  exit 0
fi

# ---------------------------------------------------------------------------------------------- checks
CLAUDE_BIN=${CLAUDE_BIN:-$(command -v claude)} || die "claude CLI not found"   # override: tests with a stub
[ -x "$CDORS_BIN" ] || die "cdors binary not found: $CDORS_BIN"
[ -x "$CDO_BIN" ] || die "cdo 2.6.0 not found: $CDO_BIN"
[ -x "$PY_BIN" ] || die "python (hk25) not found: $PY_BIN"
[ -x "$NCDUMP_BIN" ] || die "ncdump not found: $NCDUMP_BIN"
py extract "$TASKS_MD" reference | python3 -I -c 'import json,sys; json.load(sys.stdin)' \
  || die "reference JSON in $TASKS_MD does not parse"
for t in $TASKS; do py extract "$TASKS_MD" "prompt $t" >/dev/null || die "no prompt for $t"; done
for a in $ARMS; do case $a in A|B) ;; *) die "unknown arm $a (A or B)";; esac; done

# documentation offered to arm A: only what exists in this build
CDORS_DOCS="\`$CDORS_BIN help\` (usage and options) and \`$CDORS_BIN help <operator>\` (cdo's help text plus cdors notes)"
if "$CDORS_BIN" ops --json >/dev/null 2>&1; then
  # `ops` is ~20 kB, `ops --json` ~170 kB (~45k tokens if read whole), so the plain listing comes first
  CDORS_DOCS+=", \`$CDORS_BIN ops\` (operator list) and \`$CDORS_BIN ops --json\` (the same, machine-readable, with arguments)"
fi
# The repository itself is NOT shown to the agent: it holds bench/agent_tasks.md with the reference
# answers. Its README and docs/deviations.md are copied into the session directory instead (docs/), and
# the scorer flags any transcript that touches agent_tasks.md or the reference scripts.
DOC_FILES=()
DOCS=${DOCS:-full}
case $DOCS in full|guide) ;; *) die "unknown DOCS=$DOCS (full or guide)";; esac
if [ "$DOCS" = guide ]; then
  "$CDORS_BIN" guide >/dev/null 2>&1 || die "DOCS=guide but $CDORS_BIN has no 'guide' subcommand"
  CDORS_DOCS="\`$CDORS_BIN guide\` (a short guide for agents: workflow, syntax, recipes, errors; start here), $CDORS_DOCS"
elif [ -f "$REPO/README.md" ]; then
  CDORS_DOCS+=", the README in docs/README.md (in the current directory)"; DOC_FILES+=("$REPO/README.md")
fi
if [ "$DOCS" = full ] && [ -f "$REPO/docs/deviations.md" ]; then
  CDORS_DOCS+=", and docs/deviations.md (known differences from cdo)"; DOC_FILES+=("$REPO/docs/deviations.md")
fi

prompt_for() {  # task arm
  local note
  note=$(py extract "$TASKS_MD" "arm $2")
  note=${note//\{CDORS\}/$CDORS_BIN}
  note=${note//\{CDORS_DOCS\}/$CDORS_DOCS}
  note=${note//\{CDO\}/$CDO_BIN}
  note=${note//\{PYTHON\}/$PY_BIN}
  note=${note//\{NCDUMP\}/$NCDUMP_BIN}
  printf '%s\n\n%s\n\n%s\n' "$(py extract "$TASKS_MD" "prompt $1")" "$note" "$(py extract "$TASKS_MD" footer)"
}

claude_args() {  # arm -> fills the array CLAUDE_ARGS. The prompt goes first, right after -p, because
                 # --allowedTools, --disallowedTools and --add-dir take a variable number of values.
  CLAUDE_ARGS=(--output-format stream-json --verbose
    --no-session-persistence            # nothing written to ~/.claude (home quota)
    --strict-mcp-config                 # no MCP servers (no connector tools, fewer tokens)
    --disable-slash-commands            # no skills in the system prompt
    --tools "Bash,Read,Write,Edit,Glob,Grep"
    --permission-mode dontAsk
    --allowedTools "Bash" "Read" "Write" "Edit" "Glob" "Grep"
    --disallowedTools "Bash(rm *)" "Bash(rmdir *)" "Bash(git *)" "Bash(sbatch *)" "Bash(srun *)"
                      "Bash(salloc *)" "Bash(scancel *)")
  [ -n "$MAX_TURNS" ] && CLAUDE_ARGS+=(--max-turns "$MAX_TURNS")
  [ -n "$MAX_BUDGET_USD" ] && CLAUDE_ARGS+=(--max-budget-usd "$MAX_BUDGET_USD")
  [ -n "$MODEL" ] && CLAUDE_ARGS+=(--model "$MODEL")
  [ -n "$EFFORT" ] && CLAUDE_ARGS+=(--effort "$EFFORT")
  return 0
}

arm_path() {  # arm A: cdors only (it prints values itself: info/outputtab --json); arm B: cdo, Python, ncdump
  if [ "$1" = A ]; then echo "$(dirname "$CDORS_BIN"):$SYS_PATH"
  else echo "$(dirname "$CDO_BIN"):$(dirname "$PY_BIN"):$(dirname "$NCDUMP_BIN"):$SYS_PATH"; fi
}

n=0; for t in $TASKS; do for a in $ARMS; do n=$((n + 1)); done; done
echo "agent_check: run $RUN_ID, $n session(s): tasks [$TASKS] x arms [$ARMS], time limit ${TIME_LIMIT}s," \
     "max turns ${MAX_TURNS:-none}, model ${MODEL:-default}; claude $("$CLAUDE_BIN" --version 2>/dev/null | head -1)"
echo "agent_check: cdors: $("$CDORS_BIN" --version 2>/dev/null | sed -n 1p) ($CDORS_BIN)"
echo "agent_check: docs copied into arm-A session directories: ${DOC_FILES[*]:-none}"

# ---------------------------------------------------------------------------------------------- dry run
if [ "$RUN" != 1 ]; then
  for t in $TASKS; do for a in $ARMS; do
    claude_args "$a"
    echo
    echo "=================== $t-$a  ->  $BASE/$RUN_ID/$t-$a/"
    echo "--- PATH=$(arm_path "$a")"
    printf -- '--- command: claude -p "$(cat prompt.txt)"'; printf ' %q' "${CLAUDE_ARGS[@]}"; echo
    echo "--- prompt.txt:"
    prompt_for "$t" "$a"
  done; done
  echo
  echo "DRY RUN: no claude session started, nothing written. Set RUN=1 (after Nikolay's go-ahead) to run."
  exit 0
fi

# ---------------------------------------------------------------------------------------------- run
RESULTS=${RESULTS_DIR:-$HERE/agent}/results-$RUN_ID.tsv
SUMMARY=${RESULTS_DIR:-$HERE/agent}/results-$RUN_ID.md
[ -e "$RESULTS" ] && die "$RESULTS exists; choose another RUN_ID"
mkdir -p "$BASE/$RUN_ID" || die "cannot create $BASE/$RUN_ID"
exec 9>"$BASE/agent_check.lock"
flock -n 9 || die "another agent_check run holds $BASE/agent_check.lock (sessions must not run in parallel)"

for t in $TASKS; do for a in $ARMS; do
  dir=$BASE/$RUN_ID/$t-$a
  [ -e "$dir" ] && die "$dir exists; refusing to reuse it"
  mkdir -p "$dir"
  if [ "$a" = A ]; then
    mkdir -p "$dir/cdors-cache" "$dir/docs"
    [ ${#DOC_FILES[@]} -gt 0 ] && cp -p "${DOC_FILES[@]}" "$dir/docs/"
  fi
  prompt_for "$t" "$a" > "$dir/prompt.txt"
  claude_args "$a"
  { printf 'claude -p "$(cat prompt.txt)"'; printf ' %q' "${CLAUDE_ARGS[@]}"; echo; } > "$dir/command.txt"
  echo "agent_check: $(date '+%F %T') start $t-$a in $dir"
  t0=$(date +%s.%N)
  (
    cd "$dir" || exit 97
    export PATH="$(arm_path "$a")"
    if [ "$a" = A ]; then
      # fresh weights/index cache per session; cdors calls cdo (gen*) internally to make remap weights
      export CDORS_CACHE="$dir/cdors-cache" HDF5_PLUGIN_PATH="$HDF5_PLUGINS" CDO="$CDO_BIN"
    fi
    exec timeout -k 30 "$TIME_LIMIT" "$CLAUDE_BIN" -p "$(cat prompt.txt)" "${CLAUDE_ARGS[@]}" \
      < /dev/null > transcript.jsonl 2> stderr.txt
  )
  rc=$?
  t1=$(date +%s.%N)
  wall=$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.1f", b - a}')
  echo "$rc" > "$dir/exit.txt"
  py score "$TASKS_MD" "$t" "$a" "$dir/transcript.jsonl" "$RESULTS" "$RUN_ID" "$wall" "$rc" \
    || echo "agent_check: scoring $t-$a failed (transcript kept in $dir)" >&2
done; done

py summary "$RESULTS" "$SUMMARY"
echo "agent_check: results $RESULTS, summary $SUMMARY, transcripts under $BASE/$RUN_ID"
