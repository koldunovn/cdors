#!/usr/bin/env python3
"""Agent check, task F1 reference: Atlantic heat content 0-700 m, monthly, 1990-1991 (independent of cdors).

Data: AWI-CM3 dart hist1950d, temp.fesom.<year>.nc, temp(time, nz1, nod2) monthly, float32, layers
below the sea floor stored as 0. Mesh: dart fesom.mesh.diag.nc (nodes in radians, nod_area(nl, nod_n),
zbar, nlevels_nod2D).

OHC(t) = rho0 * cp * sum over Atlantic nodes n and layers k with zbar[k+1] >= -700 m (the 36 layers
0-700 m; 700 m is the interface zbar[36]) and k < nlevels_nod2D[n] - 1 (wet) of
T[t, k, n] * nod_area[k, n] * (zbar[k] - zbar[k+1]), with rho0 = 1025 kg m-3, cp = 3990 J kg-1 K-1.
Atlantic: nodes inside the Atlantic_Basin polygons of nereus' oceanBasins.geojson (shapely,
vectorised contains_xy on the union of the features of that name).

Method 1 (the reference): netCDF4 + NumPy, float64 accumulation, one month at a time.
Method 2 (cross-check, `--nereus`): nereus.heat_content with nereus.get_region_mask (its own point
loop) on the same data, 2-D nod_area and the mesh's layer thicknesses.
Variants for the tolerance: surface area at every level (nod_area[0]); nereus' triangle areas
(Earth radius 6371 km instead of FESOM's 6367.5 km: x (6371/6367.5)^2); rho*cp = 4.09e6.

Run: python -I ref_fesom_ohc.py [--quick] [--nereus]   (--quick: first month only)
"""
import json
import sys
import time

import netCDF4
import numpy as np

DATA = "/work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom"
MESH = "/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart"
NEREUS_GEOJSON = "/home/a/a270088/PYTHON/nereus/src/nereus/diag/data/oceanBasins.geojson"
YEARS = [1990, 1991]
RHO_CP = 1025.0 * 3990.0
ZMAX = 700.0


def atlantic_mask(lon, lat):
    import shapely
    from shapely.geometry import shape
    from shapely.ops import unary_union
    g = json.load(open(NEREUS_GEOJSON))
    geoms = [shape(f["geometry"]) for f in g["features"] if f["properties"].get("name") == "Atlantic_Basin"]
    return shapely.contains_xy(unary_union(geoms), lon, lat)


def main():
    quick = "--quick" in sys.argv
    t0 = time.time()
    m = netCDF4.Dataset(f"{MESH}/fesom.mesh.diag.nc")
    m.set_auto_mask(False)
    nodes = m["nodes"][:]
    lon, lat = np.degrees(nodes[0]), np.degrees(nodes[1])
    zbar = -m["zbar"][:]                       # depth of the interfaces, m, positive down
    nk = int(np.searchsorted(zbar, ZMAX, side="right") - 1)
    assert abs(zbar[nk] - ZMAX) < 1e-9, zbar[nk]   # 700 m is an interface: no cut layer
    dz = zbar[1:nk + 1] - zbar[:nk]
    nlev = m["nlevels_nod2D"][:]
    area = m["nod_area"][:nk, :]               # (nk, nod): layer k uses its top interface
    atl = atlantic_mask(lon, lat)
    wet = np.arange(nk)[:, None] < (nlev[None, :] - 1)
    w = area * dz[:, None] * wet * atl[None, :]          # m3 per (layer, node)
    w_surf = m["nod_area"][0, :][None, :] * dz[:, None] * wet * atl[None, :]
    t_mesh = time.time() - t0

    vals, surf, dates = [], [], []
    t1 = time.time()
    for y in YEARS:
        f = netCDF4.Dataset(f"{DATA}/temp.fesom.{y}.nc")
        f.set_auto_mask(False)
        tv = f["time"]
        stamps = netCDF4.num2date(tv[:], tv.units, only_use_cftime_datetimes=False)
        for i in range(len(tv)):
            T = f["temp"][i, :nk, :].astype(np.float64)
            vals.append(RHO_CP * float(np.sum(T * w)))
            surf.append(RHO_CP * float(np.sum(T * w_surf)))
            dates.append(stamps[i].strftime("%Y-%m-%d"))
            if quick:
                break
        if quick:
            break
    out = {"task": "F1", "units": "J", "dates": dates, "values": vals,
           "atlantic_nodes": int(atl.sum()), "layers": nk,
           "variants": {"surface_area_all_levels": surf,
                        "nereus_areas_r6371": [v * (6371.0 / 6367.5) ** 2 for v in vals],
                        "rho_cp_4.09e6": [v * 4.09e6 / RHO_CP for v in vals]},
           "seconds": {"mesh_and_mask": round(t_mesh, 1), "data": round(time.time() - t1, 1)}}

    if "--nereus" in sys.argv:          # second method: nereus' own mask and heat_content
        import nereus as nr
        t2 = time.time()
        atl2 = nr.get_region_mask(lon, lat, "Atlantic_Basin", "oceanBasins")
        out["nereus_mask_nodes"] = int(atl2.sum())
        out["nereus_mask_differs_at"] = int((atl2 != atl).sum())
        thick = np.where(wet, dz[:, None], 0.0)          # dry layers contribute nothing
        nv = []
        for y in YEARS:
            f = netCDF4.Dataset(f"{DATA}/temp.fesom.{y}.nc")
            f.set_auto_mask(False)
            for i in range(len(f["time"])):
                T = f["temp"][i, :nk, :]
                nv.append(float(nr.heat_content(T, area, thick, mask=atl2, rho=1025.0, cp=3990.0)))
                if quick:
                    break
            if quick:
                break
        out["nereus"] = nv
        out["nereus_max_rel_diff"] = max(abs(a - b) / abs(b) for a, b in zip(nv, vals))
        out["seconds"]["nereus"] = round(time.time() - t2, 1)
    print(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
