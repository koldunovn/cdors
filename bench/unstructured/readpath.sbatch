#!/bin/bash
# Where cdors' read time goes on uncompressed NetCDF-4 (AWI-CM3 dart temp, chunks 1x4x197522, 3.2 MB):
# the read probe `-fldmean -sellevel,<36 levels> -selname,temp` on 3 years (16.4 GB), for the binaries in
# $BIN (cdors-base = master, cdors-new* = candidates), plus a plain-C pread probe (prbench) that reads the
# same files with N threads, one shared descriptor or one per thread.
#   sbatch bench/unstructured/readpath.sbatch [MODE]     MODE: diag (default, job 28231623) | modes
# Record of jobs 28231623 (diag) and 28231676 (modes). cdors-new2 had experiment knobs that the final code
# dropped (CDORS_NC4_READ=uninit|zeroed|buffer, CDORS_NC4_FD=pool); the final code is the `uninit` variant.
# Every cold run uses its own never-read year set; the chunk indexes are prebuilt in $CDORS_CACHE.
# Output: /scratch/a/a270088/cdors-bench/readpath-<jobid>/runs.tsv (name, wall_s, rss_gb, gb, gb_per_s,
# cpu_pct, user_s, sys_s, minflt, rc) and per-run .time/.stderr; prbench lines in prbench.txt.
#
#SBATCH --job-name=cdors-readpath
#SBATCH --partition=compute
#SBATCH --account=ab0995
#SBATCH --nodes=1
#SBATCH --exclusive
#SBATCH --hint=nomultithread
#SBATCH --mem=0
#SBATCH --time=00:20:00
#SBATCH --output=/scratch/a/a270088/cdors-bench/readpath-%j.out
set -uo pipefail

BIN=${BIN:-/scratch/a/a270088/cdors-bench/readpath-bin}
D=/work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom
export CDORS_CACHE=${CDORS_CACHE:-/work/ab0995/a270088/cdors-target-r1-readpath/cache}
export OMP_NUM_THREADS=1
LV=2.5,7.5,15,25,35,45,55,65,75,85,95,106,118.5,132.5,147.5,162.5,177.5,192.5,207.5,222.5,240,262.5,287.5,312.5,337.5,362.5,387.5,415,445,475,505,535,565,600,640,680
PROBE="-fldmean -sellevel,$LV -selname,temp"
GB=16.383            # bytes the probe reads for 3 years: 36 steps x 9 level chunks x 16 x 3160352 B
MODE=${1:-diag}
[ -n "${SLURM_JOB_ID:-}" ] || { echo "runs inside Slurm only" >&2; exit 1; }
OUT=/scratch/a/a270088/cdors-bench/readpath-$SLURM_JOB_ID
[ -e "$OUT" ] && { echo "refusing to reuse $OUT" >&2; exit 1; }
mkdir -p "$OUT"
TSV=$OUT/runs.tsv
printf "name\twall_s\trss_gb\tgb\tgb_per_s\tcpu_pct\tuser_s\tsys_s\tminflt\trc\n" > "$TSV"
{ hostname; date; ls -la "$BIN"; } > "$OUT/env.txt"

files() { local y; for y in $(seq "$1" "$2"); do printf '%s ' "$D/temp.fesom.$y.nc"; done; }

timed() {  # timed NAME GB cmd...
    local name=$1 gb=$2; shift 2
    /usr/bin/time -v -o "$OUT/$name.time" "$@" > "$OUT/$name.stdout" 2> "$OUT/$name.stderr"
    local rc=$?
    awk -F': ' -v n="$name" -v gb="$gb" -v rc="$rc" '
        /Elapsed \(wall clock\)/ {k=split($2,a,":"); w=0; for(i=1;i<=k;i++) w=w*60+a[i]}
        /Maximum resident/ {r=$2/1e6}
        /Percent of CPU/ {c=$2+0}
        /User time/ {u=$2}
        /System time/ {s=$2}
        /Minor/ {m=$2}
        END {printf "%s\t%.2f\t%.3f\t%.2f\t%.2f\t%d\t%.1f\t%.1f\t%d\t%d\n", n,w,r,gb,(w>0?gb/w:0),c,u,s,m,rc}' \
        "$OUT/$name.time" >> "$TSV"
    tail -1 "$TSV"
}

probe() {  # probe NAME BINARY Y1 Y2 [cdors options...]
    local name=$1 b=$2 y1=$3 y2=$4; shift 4
    # shellcheck disable=SC2046,SC2086
    timed "$name" $GB "$BIN/$b" -O -w -P 128 "$@" $PROBE -cat $(files "$y1" "$y2") "$OUT/$name.nc"
}

pr() {  # pr NAME ARGS...  (prbench)
    echo "$1: $("$BIN/prbench" "${@:2}")" | tee -a "$OUT/prbench.txt"
}

if [ "$MODE" = diag ]; then
    # 1. cdors, master vs candidate: cold on their own sets, then warm twice
    probe base_cold   cdors-base 1950 1952
    probe base_warm1  cdors-base 1950 1952
    probe base_warm2  cdors-base 1950 1952
    probe new1_cold   cdors-new1 1953 1955
    probe new1_warm1  cdors-new1 1953 1955
    probe new1_warm2  cdors-new1 1953 1955
    probe new1_warmB  cdors-new1 1950 1952
    CDORS_NC4_FD=pool probe new1pool_cold  cdors-new1 1956 1958
    CDORS_NC4_FD=pool probe new1pool_warm1 cdors-new1 1956 1958
    CDORS_NC4_FD=pool probe new1pool_warm2 cdors-new1 1956 1958
    probe new1_io32_warm  cdors-new1 1953 1955 --io-threads 32
    probe new1_io16_warm  cdors-new1 1953 1955 --io-threads 16
    probe new1_io64_warm  cdors-new1 1953 1955 --io-threads 64
    # 2. plain pread of whole-file prefixes (16 GB over 3 files), 3.16 MB blocks
    S4=$(files 1959 1961); S5=$(files 1962 1964); S6=$(files 1983 1985)
    # shellcheck disable=SC2086
    {
    pr pr_shared128_cold     128 shared reuse 3.16 16 $S4
    pr pr_shared128_warm     128 shared reuse 3.16 16 $S4
    pr pr_perthread128_warm  128 perthread reuse 3.16 16 $S4
    pr pr_shared64_warm       64 shared reuse 3.16 16 $S4
    pr pr_shared32_warm       32 shared reuse 3.16 16 $S4
    pr pr_shared16_warm       16 shared reuse 3.16 16 $S4
    pr pr_shared8_warm         8 shared reuse 3.16 16 $S4
    pr pr_shared128_12MB_warm 128 shared reuse 12.64 16 $S4
    pr pr_shared128_calloc_warm 128 shared calloc 3.16 16 $S4
    pr pr_perthread128_cold  128 perthread reuse 3.16 16 $S5
    pr pr_shared32_cold       32 shared reuse 3.16 16 $S6
    }
else
    # modes: one group per read variant, each from an evicted cache on its own fresh set (cold, then
    # warm twice); then two cold runs with the cache left full (the Lustre client cache cap)
    lctl get_param llite.*.max_cached_mb > "$OUT/lustre_cache.txt" 2>&1
    cachestat() {
        echo "$1 $(lctl get_param -n llite.project-*.max_cached_mb 2>/dev/null | \
            awk '/^used_mb/{u+=$2} /^max_cached_mb/{m=$2} END{print "used_mb", u, "max_mb", m}')" >> "$OUT/cache.txt"
    }
    ALL=$(files 1965 1969; files 1973 1974; files 1977 1979; files 1986 1988; files 1993 1998; files 2003 2008)
    group() {  # group NAME BINARY Y1 Y2: evict, cold, warm x2
        # shellcheck disable=SC2086
        "$BIN/evict" $ALL
        cachestat "$1 evicted"
        probe "$1_cold" "$2" "$3" "$4"
        cachestat "$1 after cold"
        probe "$1_warm1" "$2" "$3" "$4"
        probe "$1_warm2" "$2" "$3" "$4"
        cachestat "$1 after warm"
    }
    group base cdors-base 1986 1988
    CDORS_NC4_READ=uninit group uninit cdors-new2 1993 1995
    CDORS_NC4_READ=zeroed group zeroed cdors-new2 1996 1998
    CDORS_NC4_READ=buffer group buffer cdors-new2 2003 2005
    CDORS_NC4_READ=buffer CDORS_NC4_FD=pool group bufpool cdors-new2 2006 2008
    # cache full (2006-2008 still in it): cold on fresh sets without evicting
    cachestat "full before"
    probe base_full_cold cdors-base 1965 1967
    cachestat "full after base"
    CDORS_NC4_READ=buffer probe buffer_full_cold cdors-new2 1977 1979
    cachestat "full after buffer"
    # plain pread (1973-1974 prefixes, 16 GB): cache full, then after evicting, then warm
    S=$(files 1973 1974)
    # shellcheck disable=SC2086
    {
    pr pr_full_cold128 128 shared reuse 3.16 16 $S
    "$BIN/evict" $ALL; cachestat "pr evicted"
    pr pr_after_evict128 128 shared reuse 3.16 16 $S
    pr pr_warm128 128 shared reuse 3.16 16 $S
    }
    cachestat "end"
fi
date >> "$OUT/env.txt"
