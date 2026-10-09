#!/usr/bin/env python3
"""Markdown tables for docs/bench-results.md from one bench/bench.sh output directory.

    summarize.py OUTDIR [--md FILE]      (default: stdout; run with python -I)

Reads OUTDIR/runs.tsv, OUTDIR/compare.tsv and, where present, OUTDIR/<run>.plan.json (cdors
--plan --json: decoded bytes = "totals.bytes_decoded", or for older plans the sum of
"bytes_decoded" over all leaves; otherwise the nominal
decoded size from runs.tsv is used). Prints two tables:

1. every run: wall time, decoded GB/s, peak RSS, status, and the diffn result against cdo;
2. the pass marks (proposed in the plan, Task 13; to be revised with the Task 2 numbers):
   speed-up >= 5x on W1 and W2, >= 3x on W3 (cdo wall / cdors wall, same node, same input, and the
   values must agree with cdo); W4 within the 32 GB budget (peak RSS <= 32 GiB, run finished);
   remote vs local = cdors throughput from the EERIE cloud / from the same bytes on Lustre
   (proposed >= 0.5; the server caps one client at ~0.19 GB/s, see docs/baseline.md).
   When cdo did not finish within its timeout, the speed-up is a lower bound (">").
Works for FIXTURE=1 directories too (run ids fx*), as a plumbing check only.
"""
import csv
import json
import os
import sys

GIB32_KB = 32 * 1024 * 1024


def read_tsv(path):
    if not os.path.exists(path):
        return []
    with open(path, newline="") as f:
        return list(csv.DictReader(f, delimiter="\t"))


def num(x):
    try:
        return float(x)
    except (TypeError, ValueError):
        return None


def plan_bytes(outdir, run):
    p = os.path.join(outdir, run + ".plan.json")
    if not os.path.exists(p) or os.path.getsize(p) == 0:
        return None
    total = 0
    found = False
    try:
        with open(p) as f:
            text = f.read().strip()
        objs = [json.loads(line) for line in text.splitlines() if line.startswith("{")] or [json.loads(text)]
    except ValueError:
        return None

    def walk(o):
        nonlocal total, found
        if isinstance(o, dict):
            for k, v in o.items():
                if k == "bytes_decoded" and isinstance(v, (int, float)):
                    total += v
                    found = True
                else:
                    walk(v)
        elif isinstance(o, list):
            for v in o:
                walk(v)
    for o in objs:
        # schema 1 (cdors_plan): the total is given; per-stage and per-leaf counts repeat it
        tot = o.get("totals", {}).get("bytes_decoded") if isinstance(o, dict) else None
        if isinstance(tot, (int, float)):
            total += tot
            found = True
        else:
            walk(o)
    return total if found else None


def fmt(x, nd=2):
    if x is None:
        return "–"
    if abs(x) >= 100:
        return f"{x:.0f}"
    if abs(x) >= 10:
        return f"{x:.1f}"
    return f"{x:.{nd}f}"


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    outdir = sys.argv[1]
    md_out = sys.argv[sys.argv.index("--md") + 1] if "--md" in sys.argv else None
    runs = read_tsv(os.path.join(outdir, "runs.tsv"))
    cmps = read_tsv(os.path.join(outdir, "compare.tsv"))
    if not runs:
        sys.exit(f"no runs.tsv rows in {outdir}")
    by_id = {r["run"]: r for r in runs}
    cmp_by_test = {}
    for c in cmps:
        cmp_by_test.setdefault(c["test"], []).append(c)

    for r in runs:
        r["wall"] = num(r["wall_s"])
        r["rss_kb"] = num(r["max_rss_kb"])
        pb = plan_bytes(outdir, r["run"]) if r["tool"] == "cdors" else None
        r["gb"] = pb / 1e9 if pb else num(r["decoded_gb"])
        r["gb_src"] = "plan" if pb else "nominal"
        r["gbps"] = r["gb"] / r["wall"] if r["gb"] and r["wall"] and r["status"] == "ok" else None

    L = []
    L.append(f"Benchmark run `{outdir}`")
    env = os.path.join(outdir, "env.txt")
    if os.path.exists(env):
        head = [ln.strip() for ln in open(env).read().splitlines()[:2]]
        L.append(f"({', '.join(head)})")
    L.append("")
    L.append("| Run | W | Tool | Source | Wall s | Decoded GB | GB/s | Peak RSS GB | Status | vs cdo (diffn) |")
    L.append("|---|---|---|---|---|---|---|---|---|---|")
    for r in runs:
        vs = []
        for c in cmp_by_test.get(r["run"], []):
            s = c["values"]
            if c["values"] in ("PASS", "FAIL"):
                s += f" ({c['tag']} {c['abslim']})"
            if c["timestamps"] not in ("-", ""):
                s += f", time {c['timestamps']}"
            if c["values"] == "SKIP":
                s += f": {c['detail']}"
            vs.append(s)
        status = r["status"] + (f" ({r['notes']})" if r["status"] != "ok" and r["notes"] else "")
        gb = fmt(r["gb"]) + ("" if r["gb_src"] == "plan" or r["gb"] is None else "*")
        rss = fmt(r["rss_kb"] / 1048576) if r["rss_kb"] is not None else "–"
        L.append(f"| {r['run']} | {r['workload']} | {r['tool']} | {r['source']} | {fmt(r['wall'])} | {gb} | "
                 f"{fmt(r['gbps'])} | {rss} | {status} | {'; '.join(vs) or '–'} |")
    L.append("")
    L.append("\\* nominal decoded size of the selection (steps × field × 4 bytes); "
             "otherwise from `cdors --plan --json`. GB = 10⁹ bytes, RSS in GiB.")
    L.append("")

    def pick(*ids):
        for i in ids:
            if i in by_id:
                return by_id[i]
        return None

    def values_ok(test):
        cs = cmp_by_test.get(test["run"], []) if test else []
        if not cs:
            return None
        return all(c["values"] == "PASS" for c in cs)

    marks = []

    def speed(label, ref, test, target):
        if not ref or not test:
            return
        tgt = f"≥ {target}×" if target else "–"
        if test["status"] != "ok":
            marks.append((label, f"{test['tool']} {test['status']}", tgt, "–"))
            return
        if ref["status"] == "ok":
            s = ref["wall"] / test["wall"]
            meas, lower = f"{s:.1f}× ({fmt(ref['wall'])} s / {fmt(test['wall'])} s)", False
        elif ref["status"] == "did_not_finish":
            s = ref["wall"] / test["wall"]
            meas, lower = f"> {s:.1f}× (cdo did not finish in {fmt(ref['wall'])} s)", True
        else:
            why = f" ({ref['notes']})" if ref["status"] == "skipped" and ref["notes"] else ""
            marks.append((label, f"{ref['tool']} {ref['status']}{why}", tgt, "–"))
            return
        if target is None:
            marks.append((label, meas, "–", "info"))
            return
        ok = s >= target
        v = values_ok(test)
        res = "PASS" if ok else ("open (lower bound)" if lower else "FAIL")
        if v is False:
            res += ", values differ"
        elif v is None and not lower:
            res += ", values unchecked"
        marks.append((label, meas, f"≥ {target}×", res))

    def memory(label, test):
        if not test:
            return
        if test["status"] != "ok" or test["rss_kb"] is None:
            marks.append((label, f"cdors {test['status']}", "≤ 32 GiB", "FAIL" if test["status"] != "not_implemented" else "–"))
            return
        gib = test["rss_kb"] / 1048576
        res = "PASS" if test["rss_kb"] <= GIB32_KB else "FAIL"
        v = values_ok(test)
        if v is False:
            res += ", values differ"
        marks.append((label, f"{gib:.1f} GiB peak RSS, {fmt(test['wall'])} s", "≤ 32 GiB", res))

    speed("W1 yearmean: cdors (store) vs cdo (view)", pick("w1_cdo", "fx1_cdo"), pick("w1_cdors_store", "fx1_cdors_store"), 5)
    speed("W1 reads in flight, cold decades: cdors default vs --io-threads 64", pick("w1_cdors_io64"), pick("w1_cdors_io_default"), None)
    speed("W1 yearmean: cdors (view) vs cdo (view)", pick("w1_cdo"), pick("w1_cdors_view"), 5)
    speed("W1 yearmean: xarray+dask+flox vs cdo", pick("w1_cdo", "fx1_cdo"), pick("w1_xr_local", "fx1_xr_local"), None)
    speed("W2 fldmean box: cdors (Parquet refs) vs cdo (raw files)", pick("w2_cdo_raw"), pick("w2_cdors_parquet"), 5)
    speed("W2 fldmean box: cdors (raw files) vs cdo (raw files)", pick("w2_cdo_raw", "fx2_cdo"), pick("w2_cdors_raw", "fx2_cdors_local"), 5)
    speed("W3 remap (cdo weights): cdors vs cdo", pick("w3_cdo_remap", "fx3_cdo_remap"), pick("w3_cdors_remap", "fx3_cdors_remap"), 3)
    speed("W3 remapbil (own weight cache): cdors vs cdo remap", pick("w3_cdo_remap", "fx3_cdo_remap"), pick("w3_cdors_remapbil", "fx3_cdors_remapbil"), 3)
    speed("W4Y (2020 to 2021-01-07) timpctl,95: cdors vs cdo", pick("w4y_cdo_timpctl"), pick("w4y_cdors_timpctl"), None)
    speed("W4Y (2020 to 2021-01-07) ydaymean: cdors vs cdo", pick("w4y_cdo_ydaymean"), pick("w4y_cdors_ydaymean"), None)
    memory("W4 timpctl,95 (1.1 TB, 87544 steps) within --mem 32G", pick("w4_cdors_timpctl", "fx4_cdors_timpctl"))
    memory("W4 ydaymean (1.1 TB, 87544 steps) within --mem 32G", pick("w4_cdors_ydaymean", "fx4_cdors_ydaymean"))
    speed("W4 timpctl,95: cdors vs cdo", pick("w4_cdo_timpctl", "fx4_cdo_timpctl"), pick("w4_cdors_timpctl", "fx4_cdors_timpctl"), None)
    speed("W4 ydaymean: cdors vs cdo", pick("w4_cdo_ydaymean", "fx4_cdo_ydaymean"), pick("w4_cdors_ydaymean", "fx4_cdors_ydaymean"), None)

    loc, rem = pick("w2_cdors_parquet"), pick("w2_cdors_cloud")
    if loc and rem:
        if loc["status"] == "ok" and rem["status"] == "ok":
            ratio = loc["wall"] / rem["wall"]
            meas = f"{ratio:.2f} (cloud {fmt(rem['gbps'])} GB/s, Lustre {fmt(loc['gbps'])} GB/s)"
            res = ("PASS" if ratio >= 0.5 else "FAIL") + " (mark under revision)"
            marks.append(("W2 remote vs local: cdors cloud / cdors Parquet throughput", meas, "≥ 0.5 (proposed)", res))
        else:
            marks.append(("W2 remote vs local", f"cloud {rem['status']}, Lustre {loc['status']}", "≥ 0.5 (proposed)", "–"))
    speed("W2 cloud: cdors vs xarray+dask+flox (both from the EERIE cloud)", pick("w2_xr_cloud"), pick("w2_cdors_cloud"), None)
    speed("W2 Lustre: cdors vs xarray+dask+flox (both on the Parquet refs)", pick("w2_xr_parquet"), pick("w2_cdors_parquet"), None)

    L.append("| Pass mark | Measured | Target | Result |")
    L.append("|---|---|---|---|")
    for m in marks:
        L.append("| " + " | ".join(m) + " |")
    L.append("")
    nfail = sum(1 for c in cmps if c["values"] == "FAIL")
    nskip = sum(1 for c in cmps if c["values"] == "SKIP")
    npass = sum(1 for c in cmps if c["values"] == "PASS")
    L.append(f"Comparisons with `cdo --pedantic diffn`: {npass} passed, {nfail} failed, {nskip} skipped.")
    text = "\n".join(L) + "\n"
    if md_out:
        if os.path.exists(md_out):
            sys.exit(f"{md_out} exists; not overwriting")
        open(md_out, "w").write(text)
    else:
        sys.stdout.write(text)


if __name__ == "__main__":
    main()
