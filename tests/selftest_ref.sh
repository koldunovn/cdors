#!/usr/bin/env bash
# Self-test of the json: and ref: tags of tests/run_cases.sh with the real cdors: runs
# tests/selftest_ref.txt and checks that exactly the rows marked "# expect FAIL: <text>" fail, each
# with <text> in its message, and that nothing is skipped. The library 'missingenv' is pointed at
# an environment that does not exist, and 'regionmask' at nereus, which has no regionmask.
#
#   tests/selftest_ref.sh            (same environment variables as tests/run_cases.sh)
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
CASES=$HERE/selftest_ref.txt
out=$(CDORS_REF_ENV_missingenv=cdors-no-such-env CDORS_REF_ENV_regionmask=nereus "$HERE/run_cases.sh" "$CASES")
summary=$(tail -1 <<< "$out")
echo "$summary"
# run_cases.sh prints each failure as "FAIL line <n>: <row>" followed by "     <message> -> <log>"
got=$(awk '/^FAIL line/ {n = $3; sub(/:$/, "", n); getline; print n "\t" $0}' <<< "$out")
want=$(grep -n 'expect FAIL:' "$CASES" | grep -v '^[0-9]*:#' | sed -E 's/^([0-9]+):.*expect FAIL: *(.*)$/\1\t\2/')
bad=0
[[ $summary == *" 0 skipped"* ]] || { echo "selftest: rows were skipped"; bad=1; }
while IFS=$'\t' read -r n text; do
  msg=$(awk -F'\t' -v n="$n" '$1 == n {print $2}' <<< "$got")
  if [[ -z $msg ]]; then echo "selftest: line $n passed, expected FAIL: $text"; bad=1
  elif [[ $msg != *"$text"* ]]; then echo "selftest: line $n failed with '$msg', expected '$text'"; bad=1
  fi
done <<< "$want"
while IFS=$'\t' read -r n msg; do
  [[ -z $n ]] || awk -F'\t' -v n="$n" '$1 == n {f = 1} END {exit !f}' <<< "$want" \
    || { echo "selftest: line $n failed unexpectedly: $msg"; bad=1; }
done <<< "$got"
((bad)) && echo "selftest_ref: FAILED" || echo "selftest_ref: ok ($(wc -l <<< "$want") expected failures)"
exit $bad
