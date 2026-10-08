#!/usr/bin/env bash
# Stand-in for cdors in the harness self-test: runs cdo, then perturbs the result.
#   CDORS=tests/fake_cdors.sh FAKE_MODE=<mode> tests/run_cases.sh tests/selftest.txt
# FAKE_MODE: none (default) | mulc (x1.0001) | ulp1 (~1 float32 ulp) | ulp5 (~5 ulps)
#            | shift (time axis +1 hour, values unchanged) | drop (first timestep dropped)
#            | alt (yearmean computed as divc,365 -yearsum) | text (extra stdout line)
#            | tiny (~1 ulp, only when the input is the tiny-chunk Zarr copy: planner check must fail)
# Zarr inputs (<fixture>.zarr2/.zarr3/_tiny.zarr2) are mapped back to <fixture>.nc and "--mem X" is
# dropped, so the harness's variant plumbing can be checked without a Zarr reader.
# With --json first: prints a JSON object for sinfo, otherwise a {"error": "$FAKE_ERR"} and exit 4.
CDO=${CDO:-/sw/spack-levante/cdo-2.6.0-akkxhz/bin/cdo}
mode=${FAKE_MODE:-none}
args=() tiny=0
while (($#)); do
  case $1 in
    --mem) shift 2; continue ;;
    *_tiny.zarr2) tiny=1; args+=("${1%_tiny.zarr2}.nc") ;;
    *.zarr2|*.zarr3) args+=("${1%.zarr?}.nc") ;;
    *) args+=("$1") ;;
  esac
  shift
done
set -- "${args[@]}"
if [[ $1 == --json ]]; then
  [[ " $* " == *" sinfo "* ]] && { echo '{"variables": []}'; exit 0; }
  echo "{\"error\": \"${FAKE_ERR:-read_limit}\", \"hint\": \"fake\"}" >&2; exit 4
fi
if [[ $1 != --no_history ]]; then           # text rows: stdout only
  "$CDO" "$@"; rc=$?; [[ $mode == text ]] && echo "extra line"; exit $rc
fi
shift; out=${!#}; set -- "${@:1:$#-1}"     # drop --no_history and the output file
case $mode in
  mulc)  pre=(-mulc,1.0001) ;;
  ulp1)  pre=(-mulc,1.00000006) ;;
  ulp5)  pre=(-mulc,1.0000003) ;;
  shift) pre=(-shifttime,1hour) ;;
  drop)  pre=(-delete,timestep=1) ;;
  alt)   set -- "${@/-yearmean/-divc,365 -yearsum}"; set -- $* ; pre=() ;;
  tiny)  pre=(); ((tiny)) && pre=(-mulc,1.00000006) ;;
  *)     pre=() ;;
esac
exec "$CDO" -s --no_history "${pre[@]}" "$@" "$out"
