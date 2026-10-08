#!/usr/bin/env python3
"""Build a symlink "view" of a Zarr v2 store: one variable and a time window.

No chunk data of the variable is copied. The view directory holds the group metadata, a
filtered .zmetadata, and per-array directories whose .zarray/.zattrs are rewritten (shape cut
to the window along time) and whose chunk files are symlinks into the source store (renumbered
when the window does not start at 0). Zarr v2 stores edge chunks full-sized, so ending the
window inside a chunk is valid. Small 1-D time-coordinate arrays whose chunking does not match
the window (e.g. `time` stored as one chunk) are decoded with numcodecs, sliced and written as
one uncompressed chunk.

Why: cdo (NCZarr) cannot open one variable or one time range of a store without a chained
operator, `cdo -T` timers need a single operator, and on a store with ~100 variables a
`-selname` chain makes cdo read far more than the selected variable (see docs/baseline.md).
A view gives cdo and cdors the same, smaller input.

Usage: make_view.py SRC.zarr OUT.zarr VAR [NTIME [START]]
       START must be a multiple of VAR's time-chunk length.
       Arrays kept: VAR, time, crs, and any 1-D coordinate named in VAR's dims.
Run with python -I.
"""
import json
import math
import os
import sys


def main():
    if len(sys.argv) not in (4, 5, 6):
        sys.exit(__doc__)
    src, out, var = os.path.abspath(sys.argv[1]), sys.argv[2], sys.argv[3]
    ntime = int(sys.argv[4]) if len(sys.argv) >= 5 else None
    start = int(sys.argv[5]) if len(sys.argv) == 6 else 0
    meta = json.load(open(os.path.join(src, ".zmetadata")))["metadata"]
    vza = meta[f"{var}/.zarray"]
    dims = meta[f"{var}/.zattrs"]["_ARRAY_DIMENSIONS"]
    if dims[0] != "time":
        sys.exit(f"{var}: first dimension is {dims[0]}, not time")
    ct = vza["chunks"][0]
    if start % ct:
        sys.exit(f"START={start} is not a multiple of the time chunk {ct}")
    total = vza["shape"][0]
    stop = total if ntime is None else min(total, start + ntime)
    keep = list(dict.fromkeys([var] + [a for a in ("time", "crs", *dims) if f"{a}/.zarray" in meta]))
    if os.path.exists(out):
        sys.exit(f"{out} exists; refusing to touch it")
    os.makedirs(out)
    newmeta = {}
    for k in (".zgroup", ".zattrs"):
        if k in meta:
            newmeta[k] = meta[k]
            json.dump(meta[k], open(os.path.join(out, k), "w"), indent=1)
    for a in keep:
        za = dict(meta[f"{a}/.zarray"])
        zt = meta.get(f"{a}/.zattrs", {})
        adims = zt.get("_ARRAY_DIMENSIONS", [])
        sep = za.get("dimension_separator", ".")
        os.makedirs(os.path.join(out, a))
        srca = os.path.join(src, a)
        along_time = adims[:1] == ["time"]
        if along_time and za["chunks"][0] != ct and (start or stop < total):
            # small coordinate array chunked differently: decode, slice, write one raw chunk
            if len(za["shape"]) != 1:
                sys.exit(f"{a}: cannot slice a multi-dimensional array with other time chunking")
            import numcodecs
            import numpy as np
            vals = []
            for i in range(math.ceil(za["shape"][0] / za["chunks"][0])):
                raw = open(os.path.join(srca, str(i)), "rb").read()
                if za.get("compressor"):
                    raw = numcodecs.get_codec(za["compressor"]).decode(raw)
                for f in reversed(za.get("filters") or []):
                    raw = numcodecs.get_codec(f).decode(raw)
                vals.append(np.frombuffer(raw, dtype=za["dtype"])[: za["chunks"][0]])
            data = np.concatenate(vals)[: za["shape"][0]][start:stop]
            za.update(shape=[len(data)], chunks=[len(data)], compressor=None, filters=None)
            open(os.path.join(out, a, "0"), "wb").write(np.ascontiguousarray(data).tobytes())
        else:
            k0 = start // ct if along_time else 0
            k1 = math.ceil(stop / ct) if along_time else None
            if along_time:
                za["shape"] = [stop - start] + za["shape"][1:]
            for name in os.listdir(srca):
                if name.startswith(".z"):
                    continue
                if along_time:
                    head, _, rest = name.partition(sep)
                    i = int(head)
                    if i < k0 or i >= k1:
                        continue
                    name_out = str(i - k0) + (sep + rest if rest else "")
                else:
                    name_out = name
                os.symlink(os.path.join(srca, name), os.path.join(out, a, name_out))
        json.dump(za, open(os.path.join(out, a, ".zarray"), "w"), indent=1)
        json.dump(zt, open(os.path.join(out, a, ".zattrs"), "w"), indent=1)
        newmeta[f"{a}/.zarray"] = za
        newmeta[f"{a}/.zattrs"] = zt
    json.dump({"zarr_consolidated_format": 1, "metadata": newmeta},
              open(os.path.join(out, ".zmetadata"), "w"), indent=1)
    print(out, "arrays:", keep, "time window:", start, stop)


if __name__ == "__main__":
    main()
