#!/usr/bin/env python3
"""Agent check, task F2 reference: NH sea-ice extent seasonal cycle, 1990-1991 (independent of cdors).

Data: AWI-CM3 dart hist1950d, a_ice.fesom.<year>.nc, a_ice(time, nod2) daily, float32 fraction 0-1
(units attribute "%" is wrong), stamped <day>T23:56. Mesh: dart fesom.mesh.diag.nc.

Daily extent E(d) = sum of nod_area[0, n] over nodes n with lat > 0 and a_ice(d, n) >= 0.15.
Seasonal cycle: for each calendar month (by stored timestamp), the mean of E(d) over all days of that
month in 1990 and 1991. Reported in million km2 (1e12 m2).

Method 1 (the reference): netCDF4 + NumPy, float64 sums, 30 days at a time.
Method 2 (cross-check, `--nereus`): nereus.ice_extent_nh(a_ice, nod_area[0], lat) per block.
Variants for the tolerance: threshold > 0.15; the threshold applied to monthly-mean concentration;
ice area (sum of a_ice x area) instead of extent; nereus' triangle areas (x (6371/6367.5)^2);
latitude >= 0 is identical (no node at exactly 0 holds ice).

Run: python -I ref_fesom_ice.py [--quick] [--nereus]   (--quick: first 31 days only)
"""
import json
import sys
import time

import netCDF4
import numpy as np

DATA = "/work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom"
MESH = "/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart"
YEARS = [1990, 1991]
BLOCK = 30


def main():
    quick = "--quick" in sys.argv
    use_nereus = "--nereus" in sys.argv
    if use_nereus:
        import nereus as nr
    t0 = time.time()
    m = netCDF4.Dataset(f"{MESH}/fesom.mesh.diag.nc")
    m.set_auto_mask(False)
    lat = np.degrees(m["nodes"][1, :])
    nh = lat > 0
    area = m["nod_area"][0, :]
    a_nh = area[nh]
    ext, ext_gt, iarea, nrs = [], [], [], []      # per day
    months = []
    conc_sum = {}                                 # (year, month) -> [sum of a_ice over days, ndays]
    for y in YEARS:
        f = netCDF4.Dataset(f"{DATA}/a_ice.fesom.{y}.nc")
        f.set_auto_mask(False)
        tv = f["time"]
        stamps = netCDF4.num2date(tv[:], tv.units, only_use_cftime_datetimes=False)
        n = 31 if quick else len(tv)
        for i0 in range(0, n, BLOCK):
            blk = f["a_ice"][i0:min(i0 + BLOCK, n), :]
            for j in range(blk.shape[0]):
                a = blk[j, nh]
                ext.append(float(np.sum(a_nh * (a >= 0.15))))
                ext_gt.append(float(np.sum(a_nh * (a > 0.15))))
                iarea.append(float(np.sum(a_nh * a.astype(np.float64))))
                s = stamps[i0 + j]
                months.append(s.month)
                key = (s.year, s.month)
                acc = conc_sum.setdefault(key, [np.zeros(a.shape), 0])
                acc[0] += a
                acc[1] += 1
            if use_nereus:
                nrs.extend(float(v) for v in nr.ice_extent_nh(blk, area, lat))
        if quick:
            break
    months = np.array(months)

    def clim(daily):
        d = np.array(daily)
        return [float(d[months == mo].mean()) / 1e12 for mo in range(1, 13) if (months == mo).any()]

    monthly_conc = {}
    for (y, mo), (s, k) in conc_sum.items():
        monthly_conc.setdefault(mo, []).append(float(np.sum(a_nh * (s / k >= 0.15))))
    out = {"task": "F2", "units": "1e6 km2", "months": sorted(set(months.tolist())),
           "days": int(len(months)), "values": clim(ext),
           "variants": {"threshold_gt": clim(ext_gt),
                        "threshold_on_monthly_mean_conc": [float(np.mean(monthly_conc[mo])) / 1e12
                                                           for mo in sorted(monthly_conc)],
                        "ice_area": clim(iarea),
                        "nereus_areas_r6371": [v * (6371.0 / 6367.5) ** 2 for v in clim(ext)]},
           "seconds": round(time.time() - t0, 1)}
    if use_nereus:
        out["nereus"] = clim(nrs)
        out["nereus_max_rel_diff"] = max(abs(a - b) / abs(b) for a, b in zip(out["nereus"], out["values"]))
    print(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
