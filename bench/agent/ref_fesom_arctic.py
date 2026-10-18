#!/usr/bin/env python3
"""Agent check, task F5 reference: Arctic mean SST, annual means 2006-2010 (independent of cdors).

Data: AWI-CM3 dart hist1950d, temp.fesom.<year>.nc monthly, SST = the top layer of temp (nz1 = 2.5 m).
Mesh: dart fesom.mesh.diag.nc, area weights nod_area[0].

Monthly Arctic mean A(y, m) = sum(T * a) / sum(a) over nodes with lat > 66.5 (ice-covered nodes
included); reported: the annual mean of each year 2006..2010 = the plain mean of its 12 monthly values
(every month weighs the same), degC.

Cross-check: the annual means recomputed as the area mean of each year's 12-month mean field.
Variants for the tolerance: unweighted mean (dart is refined in the Arctic); months weighted by their
number of days; the cap from 66.5 N inclusive of 66 N (lat > 66, a looser circle); the nereus
Arctic_Basin region instead of the latitude cap.

Run: python -I ref_fesom_arctic.py [--quick]   (--quick: 2006 only)
"""
import calendar
import json
import sys
import time

import netCDF4
import numpy as np

DATA = "/work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom"
MESH = "/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart"
NEREUS_GEOJSON = "/home/a/a270088/PYTHON/nereus/src/nereus/diag/data/oceanBasins.geojson"
YEARS = [2006, 2007, 2008, 2009, 2010]


def basin(lon, lat, name):
    import shapely
    from shapely.geometry import shape
    from shapely.ops import unary_union
    g = json.load(open(NEREUS_GEOJSON))
    return shapely.contains_xy(unary_union([shape(f["geometry"]) for f in g["features"]
                                            if f["properties"].get("name") == name]), lon, lat)


def main():
    years = YEARS[:1] if "--quick" in sys.argv else YEARS
    t0 = time.time()
    m = netCDF4.Dataset(f"{MESH}/fesom.mesh.diag.nc")
    m.set_auto_mask(False)
    nodes = m["nodes"][:]
    lon, lat = np.degrees(nodes[0]), np.degrees(nodes[1])
    area = m["nod_area"][0, :]
    masks = {"cap": lat > 66.5, "cap66": lat > 66.0, "basin": basin(lon, lat, "Arctic_Basin")}
    near = np.abs(lat - 66.5) < 1e-6
    vals, cross, unw, dayw, cap66, bas = [], [], [], [], [], []
    for y in years:
        f = netCDF4.Dataset(f"{DATA}/temp.fesom.{y}.nc")
        f.set_auto_mask(False)
        tv = f["time"]
        stamps = netCDF4.num2date(tv[:], tv.units, only_use_cftime_datetimes=False)
        assert [s.month for s in stamps] == list(range(1, 13)) and all(s.year == y for s in stamps)
        mon = {k: [] for k in ("cap", "unw", "cap66", "basin")}
        acc = np.zeros(int(masks["cap"].sum()))
        for i in range(12):
            T = f["temp"][i, 0, :].astype(np.float64)
            for k in ("cap", "cap66", "basin"):
                mk = masks[k]
                mon[k].append(float(np.sum(T[mk] * area[mk]) / np.sum(area[mk])))
            mon["unw"].append(float(T[masks["cap"]].mean()))
            acc += T[masks["cap"]]
        ndays = np.array([calendar.monthrange(y, mo)[1] for mo in range(1, 13)], float)
        vals.append(float(np.mean(mon["cap"])))
        a = area[masks["cap"]]
        cross.append(float(np.sum(acc / 12 * a) / np.sum(a)))
        unw.append(float(np.mean(mon["unw"])))
        dayw.append(float(np.sum(np.array(mon["cap"]) * ndays) / ndays.sum()))
        cap66.append(float(np.mean(mon["cap66"])))
        bas.append(float(np.mean(mon["basin"])))
    out = {"task": "F5", "units": "degC", "years": years, "cap_nodes": int(masks["cap"].sum()),
           "nodes_within_1e-6_of_66.5": int(near.sum()), "basin_nodes": int(masks["basin"].sum()),
           "values": vals, "cross_check": cross,
           "cross_max_abs_diff": max(abs(x - y) for x, y in zip(vals, cross)),
           "variants": {"unweighted": unw, "days_weighted": dayw, "lat_gt_66": cap66,
                        "nereus_arctic_basin": bas},
           "seconds": round(time.time() - t0, 1)}
    print(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
