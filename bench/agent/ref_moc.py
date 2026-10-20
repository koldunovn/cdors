#!/usr/bin/env python3
"""Agent check, long-script task H1: the Atlantic overturning at 26.5 N from w, reference and variants.

Data: AWI-CM3 dart hist1950d (FESOM2, 3.16M nodes, 6.26M elements, 80 interfaces nz, linfs), January 1990
(time step 1 of w.fesom.1990.nc; w(time, nz, nod2) in m/s on the interfaces). Basin: Atlantic_MOC of the
MPAS MOC basins (cdors' regions set `moc`, nereus' MOCBasins.geojson; the same polygon pyfesom2 uses).

Reference (cdors' definition, written independently): each element's mean of w at its three nodes, at the
interfaces that top its wet layers (k < nlevels - 1; the sea floor left out), times elem_area; psi(26.5, k)
= -sum over the Atlantic elements whose centre (normalised mean of the nodes' unit vectors) lies on or
north of 26.5 N (membership: shapely on the polygon), in Sv. The task's answer is max_k psi.

Variants (what other careful or careless methods give):
  pyfesom2_like    nodes masked by the basin, then each element averaged over its three nodes (outside
                   nodes count as 0), centres the arithmetic mean of the node latitudes (xmoc_data)
  arith_centres    the reference with arithmetic-mean centres (membership and lines)
  nodes            sum over the basin's nodes north of the line of w * nod_area[k] (no elements)
  with_floor       the reference with the elements' sea-floor interface included
  from_south       +sum over the basin south of the line (closes at 34 S, where the Atlantic is open)
  global           the whole ocean north of the line (no basin)

Run: python -I ref_moc.py [LAT]   (default 26.5); prints one JSON object.
"""
import json
import os
import sys
import time

import numpy as np

DATA = "/work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom"
MESH = "/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart"
REGIONS = os.environ.get("CDORS_REGIONS", "/work/ab0995/a270088/cdors-target/fixtures/regions")


def log(msg):
    print(f"{time.strftime('%T')} {msg}", file=sys.stderr, flush=True)


def basin_polygon(name="Atlantic_MOC"):
    from shapely.geometry import shape
    with open(f"{REGIONS}/moc.geojson") as f:
        feats = json.load(f)["features"]
    return shape(next(ft["geometry"] for ft in feats if ft["properties"]["abbrev"] == name))


def unit(lon, lat):
    lo, la = np.deg2rad(lon), np.deg2rad(lat)
    return np.stack([np.cos(la) * np.cos(lo), np.cos(la) * np.sin(lo), np.sin(la)], axis=-1)


def main():
    import netCDF4
    import shapely
    line = float(sys.argv[1]) if len(sys.argv) > 1 else 26.5
    t0 = time.time()
    with netCDF4.Dataset(f"{MESH}/fesom.mesh.diag.nc") as m:
        nodes = m["nodes"][:].astype("f8")
        lon, lat = np.degrees(nodes[0]), np.degrees(nodes[1])      # radians in dart's mesh.diag
        el = m["elem"][:].astype("i8").T - 1
        nl_e = m["nlevels"][:].astype("i8")                        # wet layers: k < nl_e - 1
        area_e = m["elem_area"][:].astype("f8")
        nl_n = m["nlevels_nod2D"][:].astype("i8")
        zbar = np.abs(m["zbar"][:].astype("f8"))
        nod_area = m["nod_area"]
        log(f"mesh: {len(lon)} nodes, {len(el)} elements, {len(zbar)} interfaces")
        with netCDF4.Dataset(f"{DATA}/w.fesom.1990.nc") as d:
            w = np.ma.filled(d["w"][0].astype("f8"), np.nan)       # (nz, nod2)
            depth = d["nz"][:].astype("f8")
        log("w read")
        # dry interfaces of the nodes are missing, as -setgrid,fesom does (interfaces k <= nl_n - 1)
        k = np.arange(len(depth))[:, None]
        w = np.where(k < nl_n[None, :], w, np.nan)

        p = unit(lon, lat)[el].sum(axis=1)
        p /= np.linalg.norm(p, axis=1)[:, None]
        elon, elat = np.degrees(np.arctan2(p[:, 1], p[:, 0])), np.degrees(np.arcsin(p[:, 2]))
        alat = lat[el].mean(axis=1)                                 # arithmetic-mean centres
        poly = basin_polygon()
        shapely.prepare(poly)
        in_e = shapely.contains_xy(poly, elon, elat)
        # pyfesom2's face_x: the arithmetic mean of the longitudes, cyclic elements shifted
        ax = lon[el].mean(axis=1)
        j = np.abs(lon[el][:, 0] - ax) > 100
        cyc = lon[el][j]
        ax[j] = np.where(cyc > 0, cyc, cyc + 360).mean(axis=1)
        ax[ax > 180] -= 360
        in_ea = shapely.contains_xy(poly, ax, alat)
        in_n = shapely.contains_xy(poly, lon, lat)
        log(f"membership: {in_e.sum()} elements (unit-vector centres), {in_ea.sum()} (arithmetic), "
            f"{in_n.sum()} nodes")

        nz = len(depth)
        prof = {v: np.full(nz, np.nan) for v in
                ("reference", "pyfesom2_like", "arith_centres", "nodes", "with_floor", "from_south", "global")}
        north, north_a = elat >= line, alat >= line
        for kk in range(nz):
            we3 = w[kk][el]                                         # (ne, 3)
            n = np.isfinite(we3).sum(axis=1)
            with np.errstate(invalid="ignore"):
                we = np.where(np.isfinite(we3), we3, 0).sum(axis=1) / n
            top = (kk < nl_e - 1) & (n > 0)                         # interfaces that top wet layers
            floor = (kk <= nl_e - 1) & (n > 0)
            c = np.where(top, we * area_e, 0.0)
            prof["reference"][kk] = -c[in_e & north].sum() * 1e-6
            prof["arith_centres"][kk] = -c[in_ea & north_a].sum() * 1e-6
            prof["from_south"][kk] = c[in_e & ~north].sum() * 1e-6
            prof["global"][kk] = -c[north].sum() * 1e-6
            cf = np.where(floor, we * area_e, 0.0)
            prof["with_floor"][kk] = -cf[in_e & north].sum() * 1e-6
            # pyfesom2: w * mask on the nodes, the mean over all three nodes, NaN below nlevels - 1
            wm = np.where(in_n, np.nan_to_num(w[kk]), 0.0)[el].sum(axis=1) / 3.0
            cp = np.where(kk < nl_e - 1, wm * area_e, 0.0)
            prof["pyfesom2_like"][kk] = -cp[north_a].sum() * 1e-6
            an = nod_area[kk, :].astype("f8")
            prof["nodes"][kk] = -np.nansum((w[kk] * an)[in_n & (lat >= line)]) * 1e-6
        log("profiles done")

    def summary(v):
        y = prof[v]
        i = int(np.nanargmax(y))
        return {"max_sv": float(y[i]), "depth_m": float(depth[i]), "level": i}

    ref = summary("reference")
    out = {"line": line, "reference": ref, "variants": {}, "profile_reference": prof["reference"].tolist(),
           "depth": depth.tolist(), "basin_elements": int(in_e.sum()), "wall_s": round(time.time() - t0, 1)}
    for v in prof:
        if v == "reference":
            continue
        s = summary(v)
        s["rel_err"] = (s["max_sv"] - ref["max_sv"]) / abs(ref["max_sv"])
        s["max_abs_profile_diff_sv"] = float(np.nanmax(np.abs(prof[v] - prof["reference"])))
        out["variants"][v] = s
    print(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
