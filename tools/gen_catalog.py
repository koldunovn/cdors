#!/usr/bin/env python3
"""Generate cdors' catalog of CDO operators from the CDO source tree.

Usage:
    python -I tools/gen_catalog.py <cdo-source-dir> [<output-dir>]

Reads, from a CDO release (tested with 2.6.5):
  OPERATORS              section, module, name and one-line description of every operator
  src/operator_help.cc   help texts (one CdoHelp per operator family)
  src/operators/*.cc     `CdoModule module = {...}` structs: which help text each operator uses,
                         number of input/output streams, aliases

Writes into <output-dir> (default: crates/cdors-core/data next to this script):
  cdo_operators.tsv      name, section, module, n_in, n_out, help, description
  cdo_aliases.tsv        alias, operator
  cdo_help.txt           help texts, already formatted as `cdo -h <operator>` prints them

Each file starts with CDO's copyright and licence notice (BSD-3-Clause), as '#' lines.
Standard library only.
"""

import re
import sys
from pathlib import Path

HELP_HEADERS = (
    "NAME", "SYNOPSIS", "DESCRIPTION", "OPERATORS", "NAMELIST", "PARAMETERS",
    "ENVIRONMENT", "NOTE", "OPTIONS", "EXAMPLE", "AUTHOR",
)
HELP_MARK = "%%% "  # starts the line that names a help text in cdo_help.txt


def c_unescape(s):
    return re.sub(r"\\(.)", lambda m: {"n": "\n", "t": "\t"}.get(m.group(1), m.group(1)), s)


def parse_operators(path):
    """OPERATORS catalog -> list of (section, module, name, description)."""
    lines = path.read_text(encoding="utf-8").splitlines()
    out, section = [], None
    for i, line in enumerate(lines):
        if not line.strip() or line.lstrip().startswith(("-", "=")):
            continue
        prev = lines[i - 1].strip() if i else ""
        nxt = lines[i + 1].strip() if i + 1 < len(lines) else ""
        if prev.startswith("---") and nxt.startswith("---") and len(line.split()) == 1:
            section = line.strip()
            continue
        if section is None:
            continue  # title and "Operator catalog:" lines
        fields = line.split(None, 2)
        if len(fields) < 2:
            raise SystemExit(f"OPERATORS:{i + 1}: cannot parse {line!r}")
        module, name = fields[0], fields[1]
        desc = fields[2].strip() if len(fields) > 2 else ""
        out.append((section, module, name, desc))
    return out


def parse_help(path):
    """operator_help.cc -> {HelpName: [raw lines]}."""
    text = path.read_text(encoding="utf-8")
    helps = {}
    for m in re.finditer(r"const CdoHelp (\w+) = \{(.*?)\n\};", text, re.S):
        lines = re.findall(r'^\s*"((?:[^"\\]|\\.)*)",?\s*$', m.group(2), re.M)
        helps[m.group(1)] = [c_unescape(s) for s in lines]
    return helps


def format_help(lines):
    """Same output as Modules::construct_help() in src/module_info.cc (without colour)."""
    out, section = [], HELP_HEADERS[0]
    for line in lines:
        if line in HELP_HEADERS:
            section = line
            if section not in ("NAME", "EXAMPLE", "AUTHOR"):
                out.append("")
        if section in ("EXAMPLE", "AUTHOR") or not line:
            continue
        out.append(line)
    return "\n".join(out) + "\n" if out else ""


def braced(text, start):
    """Return the text inside the brace pair whose '{' is at text[start]."""
    depth, i, in_str = 0, start, False
    while i < len(text):
        c = text[i]
        if in_str:
            if c == "\\":
                i += 1
            elif c == '"':
                in_str = False
        elif c == '"':
            in_str = True
        elif c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return text[start + 1:i]
        i += 1
    raise ValueError("unbalanced braces")


def top_level_entries(body):
    """Split '{ a }, { b }' into the inner texts of the top-level brace groups."""
    entries, i = [], 0
    while True:
        j = body.find("{", i)
        if j < 0:
            return entries
        inner = braced(body, j)
        entries.append(inner)
        i = j + len(inner) + 2


def field(module_body, key):
    m = re.search(r"\." + key + r"\s*=\s*", module_body)
    if not m:
        return None
    if module_body[m.end()] == "{":
        return braced(module_body, m.end())
    return module_body[m.end():].split(",", 1)[0].strip()


def parse_modules(src_dir):
    """src/operators/*.cc -> ({op: (module, n_in, n_out, help)}, {alias: op})."""
    ops, aliases = {}, {}
    for path in sorted((src_dir / "operators").glob("*.cc")):
        text = path.read_text(encoding="utf-8", errors="replace")
        for m in re.finditer(r"CdoModule\s+module\s*=\s*\{", text):
            body = braced(text, m.end() - 1)
            name = (field(body, "name") or "").strip('"')
            cons = [c.strip() for c in (field(body, "constraints") or "").split(",")]
            n_in, n_out = (-1 if c == "OBASE" else int(c) for c in cons[:2])
            for entry in top_level_entries(field(body, "operators") or ""):
                op = re.match(r'\s*"([^"]+)"', entry).group(1)
                hm = re.search(r"(\w+Help)\s*$", entry.strip())
                ops.setdefault(op, (name, n_in, n_out, hm.group(1) if hm else ""))
            for entry in top_level_entries(field(body, "aliases") or ""):
                alias, orig = re.findall(r'"([^"]+)"', entry)
                aliases.setdefault(alias, orig)
    return ops, aliases


def licence_header(src):
    lic = (src / "LICENSE").read_text(encoding="utf-8").rstrip().splitlines()
    head = [
        "Generated by tools/gen_catalog.py from the CDO source (OPERATORS, src/operator_help.cc,",
        "src/operators/*.cc). Do not edit; regenerate instead.",
        "",
        "CDO (Climate Data Operators), https://code.mpimet.mpg.de/projects/cdo",
        "",
    ]
    return "".join(("# " + s).rstrip() + "\n" for s in head + lic)


def main():
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    src = Path(sys.argv[1])
    out_dir = Path(sys.argv[2]) if len(sys.argv) > 2 else (
        Path(__file__).resolve().parent.parent / "crates" / "cdors-core" / "data")
    out_dir.mkdir(parents=True, exist_ok=True)

    catalog = parse_operators(src / "OPERATORS")
    helps = parse_help(src / "src" / "operator_help.cc")
    modules, aliases = parse_modules(src / "src")
    header = licence_header(src)

    # OPERATORS names the documentation module (operator family, e.g. `Copy` for `cat`); the code
    # module that runs the operator can differ and only provides arity and help key here.
    missing = [name for _, _, name, _ in catalog if name not in modules]
    if missing:
        raise SystemExit(f"operators without a CdoModule in src/operators: {missing}")
    rows, used_helps = [], set()
    for section, module, name, desc in catalog:
        _, n_in, n_out, help_name = modules[name]
        if not format_help(helps.get(help_name, [])):
            help_name = ""
        if help_name:
            used_helps.add(help_name)
        for s in (section, module, name, desc):
            assert "\t" not in s and "\n" not in s, s
        rows.append("\t".join((name, section, module, str(n_in), str(n_out), help_name, desc)))

    names = {r[2] for r in catalog}
    alias_rows = [f"{a}\t{o}" for a, o in sorted(aliases.items()) if o in names and a not in names]

    with open(out_dir / "cdo_operators.tsv", "w", encoding="utf-8") as f:
        f.write(header)
        f.write("# n_in/n_out: number of input/output streams; -1 = any number of inputs, or (outputs) files\n")
        f.write("# named from an output base name.\n")
        f.write("# help: key into cdo_help.txt, empty if cdo has no help text for the operator.\n")
        f.write("#name\tsection\tmodule\tn_in\tn_out\thelp\tdescription\n")
        f.write("\n".join(rows) + "\n")
    with open(out_dir / "cdo_aliases.tsv", "w", encoding="utf-8") as f:
        f.write(header)
        f.write("#alias\toperator\n")
        f.write("\n".join(alias_rows) + "\n")
    with open(out_dir / "cdo_help.txt", "w", encoding="utf-8") as f:
        f.write(header)
        f.write(f"# Each text starts after a line '{HELP_MARK}<HelpName>' and is exactly what `cdo -h <op>` prints.\n")
        for h in sorted(used_helps):
            text = format_help(helps[h])
            assert not any(l.startswith(HELP_MARK) for l in text.splitlines())
            f.write(f"{HELP_MARK}{h}\n{text}")

    print(f"operators parsed:     {len(catalog)} ({len(names)} unique) in "
          f"{len({r[0] for r in catalog})} sections")
    n_help = sum(1 for r in rows if r.split("\t")[5])
    print(f"arity found:          {len(rows)}")
    print(f"help texts mapped:    {n_help} operators -> {len(used_helps)} texts "
          f"(of {len(helps)} in operator_help.cc)")
    print(f"aliases:              {len(alias_rows)}")

if __name__ == "__main__":
    main()
