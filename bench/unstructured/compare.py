#!/usr/bin/env python3
"""cdors vs nereus series of the Task 13 node run (bench/unstructured/validate.sh).

    compare.py OUT_DIR [REF_DIR]    reads OUT_DIR/ohc_A_*.nc, OUT_DIR/ice_A_*.nc (cdors) and
                          OUT_DIR/nereus_ref_A/ (baseline.py --area nod_area); prints one JSON object
                          and writes it to OUT_DIR/compare.json

For each pair: the number of steps, whether the timestamps agree (to the second), and the largest
relative difference |cdors - nereus| / |nereus|. OHC is compared with nereus' per-level-area series
(the cdors definition) and, for information, with its surface-area series; ice extent is the
fldhist bin of a_ice (a_ice_above must be zero: no concentration above 1).
"""
import glob
import json
import os
import sys

import netCDF4
import numpy as np


def series(path, name):
    with netCDF4.Dataset(path) as d:
        v = np.ma.filled(d[name][:].astype("f8"), np.nan).reshape(len(d["time"]), -1)[:, 0]
        t = d["time"]
        when = netCDF4.num2date(t[:], t.units, getattr(t, "calendar", "standard"))
        extra = {}
        if name + "_above" in d.variables:
            extra["above_max"] = float(np.nanmax(d[name + "_above"][:]))
    return v, [w.strftime("%Y-%m-%dT%H:%M:%S") for w in when], extra


def pair(got, want):
    (g, tg, ex), (w, tw, _) = got, want
    r = {"steps": [len(g), len(w)], "times_equal": tg == tw}
    if len(g) == len(w):
        rel = np.abs(g - w) / np.abs(w)
        r.update(max_rel=float(np.nanmax(rel)), median_rel=float(np.nanmedian(rel)),
                 first=[float(g[0]), float(w[0])], nan=[int(np.isnan(g).sum()), int(np.isnan(w).sum())])
    r.update(ex)
    return r


def main():
    out = sys.argv[1]
    ref = sys.argv[2] if len(sys.argv) > 2 else os.path.join(out, "nereus_ref_A")
    res = {}
    for kind, var, files in (("ohc", "temp", ["nereus_ohc_atl_0_700.nc", "nereus_ohc_atl_0_700_area0.nc"]),
                             ("ice", "a_ice", ["nereus_siextent_nh.nc"])):
        want = []
        for f in files:
            p = os.path.join(ref, f)
            if os.path.exists(p):
                with netCDF4.Dataset(p) as d:
                    name = next(n for n in d.variables if n not in d.dimensions)
                want.append((f, series(p, name)))
        for g in sorted(glob.glob(os.path.join(out, f"{kind}_A_*.nc")) + glob.glob(os.path.join(out, f"{kind}2_A_*.nc"))):
            got = series(g, var)
            for f, w in want:
                res[f"{os.path.basename(g)} vs {f}"] = pair(got, w)
    print(json.dumps(res, indent=1))
    with open(os.path.join(out, "compare.json"), "w") as f:
        json.dump(res, f, indent=1)


if __name__ == "__main__":
    sys.exit(main())
