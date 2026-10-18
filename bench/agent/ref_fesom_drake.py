#!/usr/bin/env python3
"""Agent check, task F3 reference: Drake Passage temperature section, January 2000 (independent of cdors).

Data: AWI-CM3 dart hist1950d, temp.fesom.2000.nc step 1 (stamped 2000-01-31T23:56), temp(time, nz1,
nod2), dry layers stored as 0. Mesh: dart fesom.mesh.diag.nc.

Section: 81 points on the meridian 65 W at 55.0 S, 55.1 S, ..., 63.0 S. Each point takes the values of
its nearest mesh node (great-circle distance; a KD-tree on unit vectors, whose chord distance orders
like the great-circle distance). A sample (point i, layer k) is wet if k < nlevels_nod2D[node] - 1.
Reported: (1) the section-mean temperature, sum of T * dz_k over wet samples / sum of dz_k over wet
samples (every point weighs the same); (2) the number of wet samples.

Cross-check: the nearest nodes are recomputed by brute force (haversine over all nodes) and the wet
count from the stored zeros (T != 0), which must agree with the mesh's levels.
Variants for the tolerance: barycentric (linear) interpolation in the mesh triangles, a sample
missing if any vertex is dry; nearest node by Euclidean distance in lon/lat degrees; the dry zeros
averaged in; the plain mean of wet samples (no dz weights).

Run: python -I ref_fesom_drake.py
"""
import json
import time

import netCDF4
import numpy as np
from scipy.spatial import cKDTree

DATA = "/work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom"
MESH = "/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart"
PLAT = -55.0 - 0.1 * np.arange(81)
PLON = np.full(81, -65.0)


def xyz(lon, lat):
    lo, la = np.radians(lon), np.radians(lat)
    return np.stack([np.cos(la) * np.cos(lo), np.cos(la) * np.sin(lo), np.sin(la)], axis=-1)


def section_stats(T, dz, wet):
    """T, wet: (levels, points); returns (thickness-weighted mean, wet count, plain mean, with zeros)."""
    w = dz[:, None] * wet
    return (float(np.sum(T * w) / np.sum(w)), int(wet.sum()), float(T[wet].mean()),
            float(np.sum(T * dz[:, None]) / (dz.sum() * T.shape[1])))


def main():
    t0 = time.time()
    m = netCDF4.Dataset(f"{MESH}/fesom.mesh.diag.nc")
    m.set_auto_mask(False)
    nodes = m["nodes"][:]
    lon, lat = np.degrees(nodes[0]), np.degrees(nodes[1])
    zbar = -m["zbar"][:]
    dz = zbar[1:] - zbar[:-1]
    nl1 = dz.size
    nlev = m["nlevels_nod2D"][:]
    elem = m["elem"][:] - 1                                  # (3, elem), 1-based in the file
    tree = cKDTree(xyz(lon, lat))
    _, nn = tree.query(xyz(PLON, PLAT))
    # brute-force check of the nearest nodes (haversine)
    brute = []
    for plo, pla in zip(PLON, PLAT):
        p1, p2 = np.radians(pla), np.radians(lat)
        h = np.sin((p2 - p1) / 2) ** 2 + np.cos(p1) * np.cos(p2) * np.sin(np.radians(lon - plo) / 2) ** 2
        brute.append(int(np.argmin(h)))
    assert list(nn) == brute, "KD-tree and brute-force nearest nodes differ"
    # nearest by Euclidean distance in degrees (a plausible mistake)
    nn_deg = [int(np.argmin((lon - plo) ** 2 + (lat - pla) ** 2)) for plo, pla in zip(PLON, PLAT)]

    f = netCDF4.Dataset(f"{DATA}/temp.fesom.2000.nc")
    f.set_auto_mask(False)
    tv = f["time"]
    stamp = netCDF4.num2date(tv[0], tv.units, only_use_cftime_datetimes=False)
    T = f["temp"][0, :, :].astype(np.float64)                # (79, nod), 1 GB in float64
    lev = np.arange(nl1)[:, None]

    def at(nodes_):
        Ts = T[:, nodes_]
        return Ts, lev < (nlev[nodes_][None, :] - 1)

    Ts, wet = at(nn)
    assert np.array_equal(wet, Ts != 0), "mesh levels and stored zeros disagree on the section"
    mean, count, plain, zeros = section_stats(Ts, dz, wet)
    Td, wd = at(np.array(nn_deg))
    mean_deg, count_deg, _, _ = section_stats(Td, dz, wd)

    # linear: barycentric weights in the triangle holding each point (planar in lon/lat around it)
    cand = cKDTree(xyz(lon[elem].mean(0), lat[elem].mean(0)))
    Tl = np.full((nl1, PLAT.size), np.nan)
    for i, (plo, pla) in enumerate(zip(PLON, PLAT)):
        _, ce = cand.query(xyz(plo, pla), k=16)
        for e in ce:
            v = elem[:, e]
            x = (lon[v] - plo) * np.cos(np.radians(pla))
            y = lat[v] - pla
            M = np.array([[x[0] - x[2], x[1] - x[2]], [y[0] - y[2], y[1] - y[2]]])
            l12 = np.linalg.solve(M, -np.array([x[2], y[2]]))
            b = np.array([l12[0], l12[1], 1 - l12.sum()])
            if (b >= -1e-12).all():
                ok = lev[:, 0] < (nlev[v].min() - 1)          # missing if any vertex is dry
                Tl[ok, i] = T[np.ix_(ok, v)] @ b
                break
    wl = ~np.isnan(Tl)
    lin = float(np.nansum(Tl * dz[:, None]) / np.sum(dz[:, None] * wl))

    out = {"task": "F3", "date": stamp.strftime("%Y-%m-%dT%H:%M"), "units": ["degC", "count"],
           "values": [mean, count],
           "nearest_nodes_distinct": int(np.unique(nn).size),
           "bottom_depths_m": [float(zbar[nlev[n] - 1]) for n in nn[::10]],
           "variants": {"linear": [lin, int(wl.sum())],
                        "nearest_in_degrees": [mean_deg, count_deg],
                        "plain_mean_of_wet": [plain, count],
                        "zeros_included": [zeros, nl1 * PLAT.size]},
           "seconds": round(time.time() - t0, 1)}
    print(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
