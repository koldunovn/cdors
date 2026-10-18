#!/usr/bin/env python3
"""Agent check, task F4 reference: NINO3.4 SST anomalies of 2005, base period 2001-2005 (independent of cdors).

Data: AWI-CM3 dart hist1950d, temp.fesom.<year>.nc monthly, SST = the top layer of temp (nz1 = 2.5 m;
the 6-hourly sst files hold the same field: their January 1985 mean differs from temp's top layer by at
most 7e-4 degC at any node). Mesh: dart fesom.mesh.diag.nc, area weights nod_area[0].

Box mean B(y, m) = sum(T * a) / sum(a) over nodes with -170 <= lon <= -120 and -5 <= lat <= 5 (lon in
-180..180; no node lies within 1e-6 degrees of an edge, so edge rules do not matter).
Climatology C(m) = mean of B(y, m) over y = 2001..2005; reported: B(2005, m) - C(m), m = 1..12, degC.
Because the box mean is linear, this equals the box mean of the anomaly field.

Cross-check: the anomalies recomputed as box means of node-wise anomaly fields (T(2005, m) minus the
node-wise 5-year mean), which must agree to rounding.
Variants for the tolerance: unweighted box mean; the box mean itself (no anomaly); base period without
2005 (2001-2004).

Run: python -I ref_fesom_nino.py [--quick]   (--quick: 2004-2005 only)
"""
import json
import sys
import time

import netCDF4
import numpy as np

DATA = "/work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom"
MESH = "/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart"
YEARS = [2001, 2002, 2003, 2004, 2005]
BOX = (-170.0, -120.0, -5.0, 5.0)


def main():
    years = YEARS[-2:] if "--quick" in sys.argv else YEARS
    t0 = time.time()
    m = netCDF4.Dataset(f"{MESH}/fesom.mesh.diag.nc")
    m.set_auto_mask(False)
    nodes = m["nodes"][:]
    lon, lat = np.degrees(nodes[0]), np.degrees(nodes[1])
    w, e, s, n = BOX
    box = (lon >= w) & (lon <= e) & (lat >= s) & (lat <= n)
    edge_dist = min(np.abs(lon[box][:, None] - np.array([w, e])).min(),
                    np.abs(lat[box][:, None] - np.array([s, n])).min())
    near = (lon >= w - 1e-6) & (lon <= e + 1e-6) & (lat >= s - 1e-6) & (lat <= n + 1e-6)
    assert near.sum() == box.sum(), "nodes within 1e-6 deg of the box edges"
    a = m["nod_area"][0, :][box]
    B, U = {}, {}                       # weighted and unweighted box means per (year, month)
    fields = {}                         # node-wise top-layer fields in the box
    for y in years:
        f = netCDF4.Dataset(f"{DATA}/temp.fesom.{y}.nc")
        f.set_auto_mask(False)
        tv = f["time"]
        stamps = netCDF4.num2date(tv[:], tv.units, only_use_cftime_datetimes=False)
        for i in range(len(tv)):
            T = f["temp"][i, 0, :][box].astype(np.float64)
            key = (stamps[i].year, stamps[i].month)
            B[key] = float(np.sum(T * a) / np.sum(a))
            U[key] = float(T.mean())
            fields[key] = T
    last = years[-1]

    def anoms(means, base):
        return [means[(last, mo)] - np.mean([means[(y, mo)] for y in base]) for mo in range(1, 13)]

    vals = anoms(B, years)
    cross = []
    for mo in range(1, 13):
        anom_field = fields[(last, mo)] - np.mean([fields[(y, mo)] for y in years], axis=0)
        cross.append(float(np.sum(anom_field * a) / np.sum(a)))
    out = {"task": "F4", "units": "degC", "years": years, "box_nodes": int(box.sum()),
           "min_edge_distance_deg": float(edge_dist), "values": vals, "cross_check": cross,
           "cross_max_abs_diff": max(abs(x - y) for x, y in zip(vals, cross)),
           "variants": {"unweighted": anoms(U, years),
                        "box_mean_not_anomaly": [B[(last, mo)] for mo in range(1, 13)],
                        "base_without_last_year": anoms(B, years[:-1])},
           "seconds": round(time.time() - t0, 1)}
    print(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
