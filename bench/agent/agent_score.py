#!/usr/bin/env python3
"""Helpers for the agent check (Task 14): prompt extraction, scoring, summary. Stdlib only.

  agent_score.py extract TASKS_MD KEY
      print the fenced block that follows `<!-- KEY -->` in TASKS_MD
      (KEY: "prompt T1" .. "prompt T5", "arm A", "arm B", "footer", "reference")
  agent_score.py score TASKS_MD TASK ARM TRANSCRIPT RESULTS_TSV RUN_ID WALL_S EXIT_CODE
      parse a `claude -p --output-format stream-json` (or `json`) transcript, score the ANSWER line
      against the reference, append one row to RESULTS_TSV (header written if the file is new) and
      print the row as key=value pairs
  agent_score.py summary RESULTS_TSV OUT_MD
      write a markdown summary of all rows of RESULTS_TSV

Nothing identifying a session (session id, links) is written to the TSV or the summary.
Run with python3 -I.
"""
import json
import math
import os
import re
import sys

COLUMNS = [
    "run_id", "task", "arm", "verdict", "variant", "n_expected", "answer", "reference", "max_rel_err",
    "exit_code", "wall_s", "duration_ms", "num_turns", "tool_calls", "bash_calls", "cdors_calls",
    "cdo_calls", "python_calls", "input_tokens", "output_tokens", "cache_creation_tokens",
    "cache_read_tokens", "cost_usd", "stop", "model", "peeked", "note",
]

NUM = re.compile(r"[-+]?(?:\d+\.\d*|\.\d+|\d+)(?:[eE][-+]?\d+)?")


def extract(md_path, key):
    text = open(md_path, encoding="utf-8").read()
    marker = f"<!-- {key} -->"
    found = re.search(r"^" + re.escape(marker) + r"[ \t]*$", text, re.M)  # on a line of its own
    if not found:
        sys.exit(f"marker {marker!r} not found in {md_path}")
    m = re.compile(r"^```[a-z]*\n(.*?)\n```", re.S | re.M).search(text, found.end())
    if not m:
        sys.exit(f"no fenced block after {marker!r}")
    return m.group(1)


def read_transcript(path):
    """Return (events, result) from a stream-json (one JSON per line) or json (one object) file."""
    events, result = [], None
    try:
        raw = open(path, encoding="utf-8", errors="replace").read()
    except OSError:
        return [], None
    stripped = raw.strip()
    if stripped.startswith("{") and "\n{" not in stripped:
        try:  # --output-format json: a single result object
            obj = json.loads(stripped)
            return [obj], obj if obj.get("type") == "result" else None
        except json.JSONDecodeError:
            pass
    for line in raw.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:  # a line cut off by the time limit
            continue
        events.append(ev)
        if ev.get("type") == "result":
            result = ev
    return events, result


def assistant_blocks(events):
    for ev in events:
        if ev.get("type") == "assistant":
            msg = ev.get("message") or {}
            for block in msg.get("content") or []:
                if isinstance(block, dict):
                    yield msg, block


def find_answer(texts):
    """Last `ANSWER:` line over the texts, scanned from the last text backwards."""
    pat = re.compile(r"^[#>\-\s]*answer\s*:\s*(.*)$", re.I)
    for text in reversed(texts):
        for line in reversed(text.splitlines()):
            cleaned = line.replace("*", "").replace("`", "").strip()
            m = pat.match(cleaned)
            if m:
                return cleaned, m.group(1)
    return None, None


def parse_numbers(s, n_expected):
    vals = [float(x) for x in NUM.findall(s.replace(",", " "))]
    note = ""
    if len(vals) != n_expected:
        # "1950: 2.91 1951: 2.88 ..." -> drop integers that look like years
        tokens = NUM.findall(s.replace(",", " "))
        kept = [float(t) for t in tokens if not re.fullmatch(r"(19|20)\d\d", t)]
        if len(kept) == n_expected:
            vals, note = kept, "year labels dropped"
    return vals, note


def within(vals, ref, spec):
    if len(vals) != len(ref):
        return False
    for v, r in zip(vals, ref):
        if "tol_abs" in spec:
            if not abs(v - r) <= spec["tol_abs"]:
                return False
        elif not abs(v - r) <= spec["tol_rel"] * abs(r):
            return False
    return True


def score(md, task, arm, transcript, tsv, run_id, wall_s, exit_code):
    refs = json.loads(extract(md, "reference"))
    spec = refs[task]
    ref = spec["values"]
    events, result = read_transcript(transcript)
    row = dict.fromkeys(COLUMNS, "")
    row.update(run_id=run_id, task=task, arm=arm, n_expected=len(ref), exit_code=exit_code,
               wall_s=wall_s, reference=" ".join(f"{x:.9g}" for x in ref))
    notes = []
    for ev in events:
        if ev.get("type") == "system" and ev.get("subtype") == "init":
            row["model"] = ev.get("model", "")
            break

    # tool calls (unique ids; stream-json may repeat a message per content block)
    seen, bash_cmds = set(), []
    texts = []
    def peeks(s):
        """the reference answers, the reference scripts/logs or another session's directory"""
        if re.search(r"agent_tasks|bench/agent|ref_t\d|ref_cdo|results-|/prep\b", s):
            return True
        if re.search(r"(^|[\s'\"=;(])\.\.($|[\s/'\";)])", s):  # parent directory of the session
            return True
        # the path ends at whitespace, a quote or a shell separator (`cd <session dir>; ...`)
        for m in re.finditer(r"cdors-agentcheck/([^\s'\";&|)\\]*)", s):
            if not re.match(rf"{re.escape(run_id)}/{task}-{arm}(/|$)", m.group(1)):
                return True
        return False

    for msg, block in assistant_blocks(events):
        if block.get("type") == "tool_use" and block.get("id") not in seen:
            seen.add(block.get("id"))
            if peeks(json.dumps(block.get("input"))):
                row["peeked"] = "CHECK"
            if block.get("name") == "Bash":
                bash_cmds.append(str((block.get("input") or {}).get("command", "")))
        elif block.get("type") == "text":
            texts.append(block.get("text", ""))
    have_stream = any(ev.get("type") == "assistant" for ev in events)
    if have_stream:
        row["tool_calls"] = len(seen)
        row["bash_calls"] = len(bash_cmds)
        # the cdors binary, not paths such as cdors-agentcheck/ or cdors-bin/
        row["cdors_calls"] = sum(bool(re.search(r"(?<![\w.-])cdors(?![\w.-])", c)) for c in bash_cmds)
        row["cdo_calls"] = sum(bool(re.search(r"(?<![\w/.-])cdo\b(?!rs)|/cdo\b(?!rs)", c)) for c in bash_cmds)
        row["python_calls"] = sum(bool(re.search(r"\bpython\d?(\.\d+)?\b", c)) for c in bash_cmds)
    else:
        for k in ("tool_calls", "bash_calls", "cdors_calls", "cdo_calls", "python_calls"):
            row[k] = "NA"

    # tokens, turns, cost
    if result is not None:
        u = result.get("usage") or {}
        row.update(duration_ms=result.get("duration_ms", ""), num_turns=result.get("num_turns", ""),
                   cost_usd=result.get("total_cost_usd", ""), stop=result.get("subtype", ""))
        if result.get("is_error"):
            notes.append("result is_error")
        if isinstance(result.get("result"), str):
            texts.append(result["result"])
    else:
        # cut off (time limit) or crashed: sum the last usage seen per assistant message id
        per_msg = {}
        for ev in events:
            if ev.get("type") == "assistant":
                msg = ev.get("message") or {}
                if msg.get("usage"):
                    per_msg[msg.get("id", id(msg))] = msg["usage"]
        u = {}
        for mu in per_msg.values():
            for k, v in mu.items():
                if isinstance(v, (int, float)):
                    u[k] = u.get(k, 0) + v
        row.update(num_turns=len(per_msg) if per_msg else "", stop="no_result_record")
        notes.append("no result record (time limit or crash); tokens summed from assistant messages")
    row.update(input_tokens=u.get("input_tokens", ""), output_tokens=u.get("output_tokens", ""),
               cache_creation_tokens=u.get("cache_creation_input_tokens", ""),
               cache_read_tokens=u.get("cache_read_input_tokens", ""))

    # answer
    line, payload = find_answer(texts)
    if line is None:
        row["verdict"] = "no_answer"
    else:
        vals, note = parse_numbers(payload, len(ref))
        if note:
            notes.append(note)
        row["answer"] = " ".join(f"{v:.9g}" for v in vals)
        if len(vals) != len(ref):
            row["verdict"] = "wrong_count"
        else:
            errs = [abs(v - r) / abs(r) if r else abs(v) for v, r in zip(vals, ref)]
            row["max_rel_err"] = f"{max(errs):.3g}"
            row["verdict"] = "correct" if within(vals, ref, spec) else "fail"
            matched = [] if row["verdict"] == "correct" else [
                name for name, vref in spec.get("variants", {}).items() if within(vals, vref, spec)]
            row["variant"] = ",".join(matched)
    if str(exit_code) not in ("0", ""):
        notes.append(f"claude exit {exit_code}" + (" (timeout)" if str(exit_code) == "124" else ""))
    row["note"] = "; ".join(notes)

    new = not os.path.exists(tsv) or os.path.getsize(tsv) == 0
    with open(tsv, "a", encoding="utf-8") as f:
        if new:
            f.write("\t".join(COLUMNS) + "\n")
        f.write("\t".join(str(row[c]).replace("\t", " ").replace("\n", " ") for c in COLUMNS) + "\n")
    print(" ".join(f"{k}={row[k]}" for k in ("task", "arm", "verdict", "variant", "answer", "reference",
                                            "num_turns", "tool_calls", "wall_s", "cost_usd") if row[k] != ""))


def num(x):
    try:
        v = float(x)
        return v if math.isfinite(v) else None
    except (TypeError, ValueError):
        return None


def summary(tsv, out):
    rows = []
    with open(tsv, encoding="utf-8") as f:
        header = f.readline().rstrip("\n").split("\t")
        for line in f:
            if line.strip():
                rows.append(dict(zip(header, line.rstrip("\n").split("\t"))))
    run_ids = sorted({r["run_id"] for r in rows})
    lines = [f"# Agent check results ({', '.join(run_ids)})", "",
             "Generated by `bench/agent/agent_score.py summary` from "
             f"`{os.path.basename(tsv)}`. Tokens: input + cache creation + cache read + output.", "",
             "| Task | Arm | Verdict | Answer | Reference | Turns | Tools (bash: cdors/cdo/py) | Wall s | Tokens (out) | USD | Note |",
             "|---|---|---|---|---|---|---|---|---|---|---|"]

    def tok(r):
        parts = [num(r.get(k)) for k in ("input_tokens", "cache_creation_tokens", "cache_read_tokens",
                                         "output_tokens")]
        return sum(p for p in parts if p is not None) if any(p is not None for p in parts) else None

    for r in sorted(rows, key=lambda r: (r["task"], r["arm"], r["run_id"])):
        verdict = r["verdict"] + (f" ({r['variant']})" if r.get("variant") else "")
        if r.get("peeked"):
            verdict += " **peeked? check transcript**"
        tools = f"{r['tool_calls']} ({r['cdors_calls']}/{r['cdo_calls']}/{r['python_calls']})"
        t = tok(r)
        lines.append(f"| {r['task']} | {r['arm']} | {verdict} | {r['answer']} | {r['reference']} | "
                     f"{r['num_turns']} | {tools} | {r['wall_s']} | "
                     f"{'' if t is None else f'{t:,.0f}'} ({r['output_tokens']}) | {r['cost_usd']} | {r['note']} |")
    lines += ["", "| Arm | Sessions | Correct | Tool calls | Wall s | Tokens | Output tokens | USD |",
              "|---|---|---|---|---|---|---|---|"]
    for arm in sorted({r["arm"] for r in rows}):
        rs = [r for r in rows if r["arm"] == arm]

        def tot(key):
            vals = [num(r.get(key)) for r in rs]
            return sum(v for v in vals if v is not None)
        toks = sum(t for t in (tok(r) for r in rs) if t is not None)
        lines.append(f"| {arm} | {len(rs)} | {sum(r['verdict'] == 'correct' for r in rs)} | "
                     f"{tot('tool_calls'):.0f} | {tot('wall_s'):.0f} | {toks:,.0f} | "
                     f"{tot('output_tokens'):,.0f} | {tot('cost_usd'):.2f} |")
    open(out, "w", encoding="utf-8").write("\n".join(lines) + "\n")
    print(f"wrote {out}")


def main():
    a = sys.argv[1:]
    if a[:1] == ["extract"] and len(a) == 3:
        print(extract(a[1], a[2]))
    elif a[:1] == ["score"] and len(a) == 9:
        score(*a[1:])
    elif a[:1] == ["summary"] and len(a) == 3:
        summary(a[1], a[2])
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
