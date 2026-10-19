#!/usr/bin/env python3
"""Agent check, trap candidates W1, W2, W4, W5: reference answers and the answers of naive methods.

A trap task is kept only if the naive answers an agent is likely to give fall outside the tolerance
and cdors' natural chain gives the reference. This script computes, independent of cdors, the
reference (careful NumPy, float64 accumulation) and each naive variant, and prints one JSON object.

Data: AWI-CM3 dart hist1950d (FESOM2, 3.16M nodes, 6.26M elements), January 1990 (time step 1 of the
monthly files), layers below the sea floor stored as 0. Mesh: dart fesom.mesh.diag.nc (nodes in
radians, nod_area(nl, nod_n): area of layer k at index k, 0 below the floor; elem(n3, elem_n) 1-based;
elem_area; zbar; nlevels_nod2D, layer k wet if k < nlevels_nod2D - 1). IFS: EERIE IFS-FESOM hist-1950,
monthly means on the native O1280 grid through gribscan references (6 599 680 points in 2560 rows;
W5 needs an eccodes that decodes them: env ecmwf (2.34) does, envs nereus (2.44) and hk25 (2.41) do not).
ICON: EERIE ICON-ESM-ER control (erc2002), raw daily means on the native R2B8 grid (5 242 880 triangular
cells; the files have no coordinates, the grid file holds clon/clat in radians and cell_area in m2).

  W1 level  global mean temperature of the 1600 m level (layer 1550-1650 m), area-weighted
  W2 layer  volume-mean temperature of the water between 1000 and 2000 m (2000 m cuts layer 1950-2050)
  W4 elem   mean surface zonal velocity u (on elements) over boxes, area-weighted by element area
  W5 ifs    global mean 2 m temperature (mean2t), January 1950, cell-area weighted on O1280
  W6 icon   mean 2 m temperature (tas) south of 60 S, daily mean of 1 January 1991, cell-area weighted

Run: python -I ref_trap.py W1|W2|W4|W5|W6
"""
import json
import sys

import numpy as np

DATA = "/work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom"
MESH = "/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart"
IFS = "/work/bm1344/a270228/phase1_hist/gribscan_1m_NATIVE/jsons.1950-1959/sfc.dir/atm2d_avg.json"
ICON = ("/work/bm1344/k203123/experiments/erc2002/run_19910101T000000-19910131T235900/"
        "erc2002_atm_2d_1d_mean_19910101T000000Z.nc")
ICON_GRID = "/pool/data/ICON/grids/public/mpim/0033/icon_grid_0033_R02B08_G.nc"
BOXES = {  # lon0, lon1, lat0, lat1
    "gulfstream": (-80, -40, 30, 50),
    "agulhas": (10, 40, -45, -30),
    "nordic": (-20, 20, 60, 80),
    "drake": (-70, -55, -62, -55),
    "nino34": (-170, -120, -5, 5),
}


def nc(path):
    import netCDF4
    return netCDF4.Dataset(path)


def mean(w, x):
    w = np.asarray(w, np.float64)
    return float(np.sum(w * x) / np.sum(w))


def rel(v, ref):
    return {"value": v, "rel_err": (v - ref) / abs(ref)}


def mesh():
    return nc(f"{MESH}/fesom.mesh.diag.nc")


def w1():
    k = 50
    m = mesh()
    zbar = -m["zbar"][:].astype(np.float64)
    nlev = m["nlevels_nod2D"][:]
    a = m["nod_area"][k, :].astype(np.float64)
    a0 = m["nod_area"][0, :].astype(np.float64)
    d = nc(f"{DATA}/temp.fesom.1990.nc")
    assert float(d["nz1"][k]) == 1600.0 and (zbar[k], zbar[k + 1]) == (1550.0, 1650.0)
    t = d["temp"][0, k, :].astype(np.float64)
    wet = k < nlev - 1
    assert np.array_equal(wet, a > 0)
    ref = mean(a[wet], t[wet])
    nz = t != 0
    return {
        "task": "W1", "level_m": 1600.0, "reference": ref,
        "nodes": int(t.size), "wet_nodes": int(wet.sum()), "zero_nodes": int((~nz).sum()),
        "zeros_outside_wet": int((~nz & ~wet).sum()), "zeros_inside_wet": int((~nz & wet).sum()),
        "naive": {
            "unweighted_all_nodes": rel(float(t.mean()), ref),
            "unweighted_nonzero": rel(float(t[nz].mean()), ref),
            "surface_area_all_nodes": rel(mean(a0, t), ref),
            "surface_area_nonzero": rel(mean(a0[nz], t[nz]), ref),
            "level_area_all_nodes": rel(mean(a, t), ref),  # = ref: area 0 below the floor
        },
    }


def w2(z0=1000.0, z1=2000.0):
    m = mesh()
    zbar = -m["zbar"][:].astype(np.float64)
    nlev = m["nlevels_nod2D"][:]
    top, bot = zbar[:-1], zbar[1:]
    cut = np.clip(np.minimum(bot, z1) - np.maximum(top, z0), 0, None)
    ks = np.nonzero(cut > 0)[0]
    k0, k1 = int(ks[0]), int(ks[-1]) + 1
    a = m["nod_area"][k0:k1, :].astype(np.float64)
    a0 = m["nod_area"][0, :].astype(np.float64)
    d = nc(f"{DATA}/temp.fesom.1990.nc")
    t = d["temp"][0, k0:k1, :].astype(np.float64)
    mid = 0.5 * (top + bot)[k0:k1]
    dz = (bot - top)[k0:k1]
    wet = (np.arange(k0, k1)[:, None] < (nlev - 1)[None, :])
    assert np.array_equal(wet, a > 0)

    def vmean(area, thick, sel):  # sel: (nlev, npoints) bool
        v = area * thick[:, None] * sel
        return float(np.sum(v * t) / np.sum(v))

    ref = vmean(a, cut[k0:k1], wet)
    mid_in = (mid >= z0) & (mid <= z1)  # nereus' depth_min/depth_max: whole layers by mid-depth
    inside = (top[k0:k1] >= z0) & (bot[k0:k1] <= z1)
    all_pts = np.ones_like(wet)
    out = {
        "task": "W2", "z0": z0, "z1": z1, "layers": [k0, k1], "reference": ref,
        "layer_cut_at_z1": [float(top[k1 - 1]), float(bot[k1 - 1]), float(cut[k1 - 1])],
        "naive": {
            "whole_layers_mid_in_range": rel(vmean(a, dz * mid_in, wet), ref),
            "whole_layers_mid_in_range_surface_area_zeros": rel(vmean(a0[None, :], dz * mid_in, all_pts), ref),
            "whole_layers_inside_range": rel(vmean(a, dz * inside, wet), ref),
            "level_mean_no_thickness": rel(vmean(a, (cut > 0)[k0:k1].astype(float), wet), ref),
            "cut_surface_area_zeros": rel(vmean(a0[None, :], cut[k0:k1], all_pts), ref),
            "cut_surface_area_wet": rel(vmean(a0[None, :], cut[k0:k1], wet), ref),
        },
    }
    try:  # nereus itself, as an arm-B agent would call it
        import nereus as nr
        for name, area in (("nereus_volume_mean_area1d", a0), ("nereus_volume_mean_area2d", a)):
            v = float(nr.volume_mean(t, area, dz, depth=mid, depth_min=z0, depth_max=z1))
            out["naive"][name] = rel(v, ref)
    except Exception as e:  # noqa: BLE001
        out["nereus_error"] = repr(e)
    return out


def w4():
    m = mesh()
    nodes = m["nodes"][:].astype(np.float64)
    if np.abs(nodes).max() > 2 * np.pi + 1e-6:
        nodes = np.radians(nodes)
    elem = m["elem"][:].astype(np.int64)
    elem -= elem.min()
    ea = m["elem_area"][:].astype(np.float64)
    lon, lat = nodes[0][elem], nodes[1][elem]  # (3, nelem), radians
    xyz = np.stack([np.cos(lat) * np.cos(lon), np.cos(lat) * np.sin(lon), np.sin(lat)]).mean(axis=1)
    clon = np.degrees(np.arctan2(xyz[1], xyz[0]))
    clat = np.degrees(np.arctan2(xyz[2], np.hypot(xyz[0], xyz[1])))
    vlon = np.degrees(lon)
    plon = vlon.mean(axis=0)  # planar mean of the vertex longitudes (wrong across the dateline)
    plat = np.degrees(lat).mean(axis=0)
    d = nc(f"{DATA}/u.fesom.1990.nc")
    assert float(d["nz1"][0]) == 2.5
    u = d["u"][0, 0, :].astype(np.float64)
    out = {"task": "W4", "elements": int(u.size), "boxes": {}}
    for name, (x0, x1, y0, y1) in BOXES.items():
        inb = (clon >= x0) & (clon <= x1) & (clat >= y0) & (clat <= y1)
        ref = mean(ea[inb], u[inb])
        pin = (plon >= x0) & (plon <= x1) & (plat >= y0) & (plat <= y1)
        allv = np.all((vlon >= x0) & (vlon <= x1) & (np.degrees(lat) >= y0) & (np.degrees(lat) <= y1), axis=0)
        out["boxes"][name] = {
            "box": [x0, x1, y0, y1], "reference": ref, "elements": int(inb.sum()),
            "elem_area_km2_min_median_max": [float(np.min(ea[inb]) / 1e6), float(np.median(ea[inb]) / 1e6),
                                             float(np.max(ea[inb]) / 1e6)],
            "naive": {
                "unweighted": rel(float(u[inb].mean()), ref),
                "coslat_weights": rel(mean(np.cos(np.radians(clat[inb])), u[inb]), ref),
                "planar_centres": rel(mean(ea[pin], u[pin]), ref),
                "all_vertices_in_box": rel(mean(ea[allv], u[allv]), ref),
            },
        }
    return out


def w5():
    import fsspec
    import gribscan  # noqa: F401  (registers the GRIB codec)
    import xarray as xr
    fs = fsspec.filesystem("reference", fo=IFS)
    ds = xr.open_zarr(fs.get_mapper(""), consolidated=False)
    t = ds["mean2t"].isel(time=0).values.astype(np.float64)
    lat = ds["lat"].values.astype(np.float64)
    rows, inv, counts = np.unique(-lat, return_inverse=True, return_counts=True)  # north to south
    x, w = np.polynomial.legendre.leggauss(rows.size)
    glat = np.degrees(np.arcsin(x))[::-1]  # north to south
    w = w[::-1]
    out = {"task": "W5", "time": str(ds["time"].values[0]), "points": int(t.size), "rows": int(rows.size),
           "max_row_lat_minus_gauss_deg": float(np.max(np.abs(-rows - glat))),
           "points_per_row_first_last_equator": [int(counts[0]), int(counts[-1]), int(counts.max())]}
    pw = (w / counts)[inv]
    ref = mean(pw, t)
    edges = np.concatenate([[90.0], 0.5 * (glat[1:] + glat[:-1]), [-90.0]])  # row bounds midway between rows
    band = -np.diff(np.sin(np.radians(edges)))
    out["reference"] = ref
    out["accepted"] = {"band_area_per_point": rel(mean((band / counts)[inv], t), ref)}
    out["naive"] = {
        "unweighted": rel(float(t.mean()), ref),
        "coslat_per_point": rel(mean(np.cos(np.radians(lat)), t), ref),
        "gauss_weight_per_point": rel(mean(w[inv], t), ref),  # row weight without dividing by its points
    }
    return out


def w6(lat1=-60.0):
    g = nc(ICON_GRID)
    assert g["clat"].units == "radian"
    lat = np.degrees(g["clat"][:].astype(np.float64))
    a = g["cell_area"][:].astype(np.float64)
    d = nc(ICON)
    assert str(d["time"].units).startswith("minutes since 1950-1-1") and float(d["time"][0]) == 21565440.0
    t = d["tas"][0, 0, :].astype(np.float64)  # stamped 1991-01-02 00:00: the mean of 1 January
    s = lat <= lat1
    ref = mean(a[s], t[s])
    # spherical triangle areas from the vertices (another accepted way to get the cell areas)
    vlon = g["clon_vertices"][s, :].astype(np.float64)
    vlat = g["clat_vertices"][s, :].astype(np.float64)
    p = np.stack([np.cos(vlat) * np.cos(vlon), np.cos(vlat) * np.sin(vlon), np.sin(vlat)], axis=-1)
    p0, p1, p2 = p[:, 0], p[:, 1], p[:, 2]
    tri = 2 * np.arctan2(np.abs(np.einsum("ij,ij->i", p0, np.cross(p1, p2))),
                         1 + np.einsum("ij,ij->i", p0, p1) + np.einsum("ij,ij->i", p1, p2)
                         + np.einsum("ij,ij->i", p2, p0))
    blat = g["lat_cell_barycenter"][:].astype(np.float64)
    blat = np.degrees(blat) if np.abs(blat).max() < 2 else blat
    sb = blat <= lat1
    return {
        "task": "W6", "cells": int(t.size), "cells_south": int(s.sum()), "reference": ref,
        "cell_area_ratio_max_min": float(a[s].max() / a[s].min()),
        "accepted": {"vertex_triangle_area": rel(mean(tri, t[s]), ref),
                     "barycenter_membership": rel(mean(a[sb], t[sb]), ref)},
        "naive": {
            "unweighted": rel(float(t[s].mean()), ref),
            "coslat_weights": rel(mean(np.cos(np.radians(lat[s])), t[s]), ref),
            "time_step_2": rel(mean(a[s], d["tas"][1, 0, :].astype(np.float64)[s]), ref),
        },
    }


if __name__ == "__main__":
    task = sys.argv[1]
    print(json.dumps({"W1": w1, "W2": w2, "W4": w4, "W5": w5, "W6": w6}[task](), indent=1))
