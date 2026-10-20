#!/usr/bin/env python3
"""Agent check, long-script task H2: the volume transport through Drake Passage from u and v, reference
and variants.

Data: AWI-CM3 dart hist1950d (FESOM2, 3.16M nodes, 6.26M elements, 79 layers), January 1990 (time step 1 of
u.fesom.1990.nc and v.fesom.1990.nc: (time, nz1, elem) in m/s, already geographic: the files carry
FESOM_autorotate_back_to_geo = -1). Section: the great circle from 68 W 54 S (Tierra del Fuego) to 63 W 65.5 S
(Antarctic Peninsula), both ends on land.

Reference (cdors' definition, written independently): the great circle cuts the element edges whose nodes lie
on opposite sides of its plane; each crossed element gets the chord from the midpoint of its entry edge to the
midpoint of its exit edge (the broken line through its centre: FESOM2's dual-cell faces), projected on east and
north at the element's centre, on a sphere of FESOM2's r_earth = 6367.5 km; T = sum over the element's wet
layers (k < nlevels - 1) of (u * -dN + v * dE) * dz_k, dz from |zbar|; positive eastward (the left of the
north-to-south path), in Sv.

Variants (what other careful or careless methods give):
  exact_chord       the chord between the actual cut points instead of the edge midpoints
  radius_6371       lengths on a 6371 km sphere
  pyfesom2_like     pyfesom2's cross_section_transport method: a straight line in lon/lat, arithmetic edge
                    midpoints and centres, local east-north lengths on the WGS84 ellipsoid
  sampled_2km       points every 2 km along the great circle, each segment with the velocity of the element
                    that holds its midpoint (as -transect values integrated by hand)
  dz_from_nz1       layer thickness from the spacing of the layer midpoints nz1 instead of zbar
  u_only            the zonal component alone
  rotated           u, v rotated "back" once more with pyfesom2's vec_rotate_r2g(50, 15, -90) (its default
                    uvrotated=True), although the files are geographic already
  sign_reversed     positive westward

Run: python -I ref_transport.py   (prints one JSON object)
"""
import importlib.util
import json
import sys
import time

import numpy as np

DATA = "/work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom"
MESH = "/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart"
PYFESOM2_UT = "/home/a/a270088/PYTHON/pyfesom2/pyfesom2/ut.py"
P0, P1 = (-68.0, -54.0), (-63.0, -65.5)
R = 6367500.0


def log(msg):
    print(f"{time.strftime('%T')} {msg}", file=sys.stderr, flush=True)


def unit(lon, lat):
    lo, la = np.deg2rad(lon), np.deg2rad(lat)
    return np.stack([np.cos(la) * np.cos(lo), np.cos(la) * np.sin(lo), np.sin(la)], axis=-1)


def normed(x):
    return x / np.linalg.norm(x, axis=-1, keepdims=True)


def east_north(c):
    """Local east and north unit vectors at unit vectors c (n, 3)."""
    lo, la = np.arctan2(c[:, 1], c[:, 0]), np.arcsin(np.clip(c[:, 2], -1, 1))
    e = np.stack([-np.sin(lo), np.cos(lo), np.zeros_like(lo)], axis=1)
    n = np.stack([-np.sin(la) * np.cos(lo), -np.sin(la) * np.sin(lo), np.cos(la)], axis=1)
    return e, n


def gc_cuts(P, el):
    """Cuts of the great circle P0 -> P1 with the element edges: element, position, edge midpoint, cut point."""
    a, b = unit(*P0), unit(*P1)
    n = np.cross(a, b)
    nn = n / np.linalg.norm(n)
    arc = np.arctan2(np.linalg.norm(n), a @ b)
    left = P @ n >= 0
    out = []
    for i, j in ((0, 1), (1, 2), (2, 0)):
        e = np.nonzero(left[el[:, i]] != left[el[:, j]])[0]
        pi, pj = P[el[e, i]], P[el[e, j]]
        di, dj = pi @ n, pj @ n
        x = normed(pi + (di / (di - dj))[:, None] * (pj - pi))
        pos = np.arctan2(np.cross(a, x) @ nn, x @ a)
        ok = (pos >= 0) & (pos <= arc)
        out.append((e[ok], pos[ok], normed(pi + pj)[ok], x[ok]))
    return [np.concatenate(z) for z in zip(*out)], arc


def pair(elem, pos, *pts):
    """Per element with two cuts (entry, exit by position): the element and the exit-minus-entry vector of
    each point array; elements with another number of cuts are reported."""
    o = np.lexsort((pos, elem))
    elem, pos = elem[o], pos[o]
    pts = [p[o] for p in pts]
    u, start, count = np.unique(elem, return_index=True, return_counts=True)
    two = count == 2
    s = start[two]
    return u[two], [p[s + 1] - p[s] for p in pts], int((count != 2).sum()), pos[s]


def main():
    import netCDF4
    from scipy.spatial import cKDTree
    t0 = time.time()
    with netCDF4.Dataset(f"{MESH}/fesom.mesh.diag.nc") as m:
        nodes = m["nodes"][:].astype("f8")
        lon, lat = np.degrees(nodes[0]), np.degrees(nodes[1])          # radians in dart's mesh.diag
        el = m["elem"][:].astype("i8").T - 1
        nl_e = m["nlevels"][:].astype("i8")                            # wet layers: k < nl_e - 1
        zbar = np.abs(m["zbar"][:].astype("f8"))
    dz = np.diff(zbar)
    P = unit(lon, lat)
    C = normed(P[el].sum(axis=1))                                      # element centres
    log(f"mesh: {len(lon)} nodes, {len(el)} elements")

    (ce, cpos, cmid, cx), arc = gc_cuts(P, el)
    ee, (dmid, dx), odd, first = pair(ce, cpos, cmid, cx)
    order = np.argsort(first, kind="stable")
    ee, dmid, dx = ee[order], dmid[order], dx[order]
    log(f"great circle: {len(ee)} elements crossed, {odd} with another number of cuts, {arc * R / 1e3:.1f} km")

    # pyfesom2-like: a straight line in lon/lat, arithmetic midpoints and centres, WGS84 local lengths
    (x0, y0), (x1, y1) = P0, P1
    # the elements of the section's box widened by 1 degree (pyfesom2's add_extent=1), so that no element
    # across the date line cuts the line in lon/lat
    box = ((lon[el] >= min(x0, x1) - 1) & (lon[el] <= max(x0, x1) + 1)
           & (lat[el] >= min(y0, y1) - 1) & (lat[el] <= max(y0, y1) + 1)).all(axis=1)
    ex, ey = lon[el], lat[el]
    def side(px, py):
        return (x1 - x0) * (py - y0) - (y1 - y0) * (px - x0)
    sl = side(ex, ey) >= 0
    pe, ppos, pmx, pmy = [], [], [], []
    for i, j in ((0, 1), (1, 2), (2, 0)):
        e = np.nonzero(box & (sl[:, i] != sl[:, j]))[0]
        xi, yi, xj, yj = ex[e, i], ey[e, i], ex[e, j], ey[e, j]
        # parameter of the cut along the line (0 at P0, 1 at P1)
        den = (x1 - x0) * (yj - yi) - (y1 - y0) * (xj - xi)
        t = ((xi - x0) * (yj - yi) - (yi - y0) * (xj - xi)) / den
        ok = (t >= 0) & (t <= 1)
        pe.append(e[ok]); ppos.append(t[ok])
        pmx.append(((xi + xj) / 2)[ok]); pmy.append(((yi + yj) / 2)[ok])
    pe, ppos, pmx, pmy = map(np.concatenate, (pe, ppos, pmx, pmy))
    pee, (ddx, ddy), podd, _ = pair(pe, ppos, pmx, pmy)
    a_, f_ = 6378137.0, 1 / 298.257223563
    e2 = f_ * (2 - f_)
    phi = np.deg2rad(lat[el[pee]].mean(axis=1))
    w = 1 - e2 * np.sin(phi) ** 2
    Nr, Mr = a_ / np.sqrt(w), a_ * (1 - e2) / w ** 1.5
    pdE, pdN = np.deg2rad(ddx) * Nr * np.cos(phi), np.deg2rad(ddy) * Mr
    log(f"lon/lat line: {len(pee)} elements, {podd} odd")

    # sampled: 2 km steps on the great circle, the element holding each segment's midpoint
    a, b = unit(*P0), unit(*P1)
    ns = int(np.ceil(arc * R / 2000.0))
    tt = np.linspace(0, 1, ns + 1)
    pts = normed(np.sin((1 - tt) * arc)[:, None] * a + np.sin(tt * arc)[:, None] * b)
    mids = normed(pts[:-1] + pts[1:])
    tree = cKDTree(C)
    _, cand = tree.query(mids, k=8)
    hold = np.full(len(mids), -1)
    for q in range(8):
        e = cand[:, q]
        tri = P[el[e]]                                                 # (n, 3, 3)
        s = [np.einsum("ij,ij->i", np.cross(tri[:, i], tri[:, (i + 1) % 3]), mids) >= 0 for i in range(3)]
        inside = (s[0] == s[1]) & (s[1] == s[2])
        hold = np.where((hold < 0) & inside, e, hold)
    sm = hold >= 0
    seg = pts[1:] - pts[:-1]
    se, sn = east_north(mids)
    sdE, sdN = np.einsum("ij,ij->i", seg, se) * R, np.einsum("ij,ij->i", seg, sn) * R
    log(f"sampled: {ns} segments, {sm.sum()} in the ocean")

    # velocities, first time step
    need = np.unique(np.concatenate([ee, pee, hold[sm]]))
    with netCDF4.Dataset(f"{DATA}/u.fesom.1990.nc") as d:
        uu = np.ma.filled(d["u"][0].astype("f8"), np.nan)[:, need]
        nz1 = d["nz1"][:].astype("f8")
    with netCDF4.Dataset(f"{DATA}/v.fesom.1990.nc") as d:
        vv = np.ma.filled(d["v"][0].astype("f8"), np.nan)[:, need]
    log("u, v read")
    col = {e: i for i, e in enumerate(need)}

    def flux(elems, dE, dN, uu=uu, vv=vv, thick=dz, use_v=True):
        idx = np.array([col[e] for e in elems])
        k = np.arange(len(thick))[:, None]
        wet = k < (nl_e[elems] - 1)[None, :]
        f = uu[:, idx] * (-dN)[None, :] + (vv[:, idx] * dE[None, :] if use_v else 0.0)
        return float(np.nansum(np.where(wet, f, 0.0) * thick[:, None]) * 1e-6)

    e1, n1 = east_north(C[ee])
    dE, dN = np.einsum("ij,ij->i", dmid, e1) * R, np.einsum("ij,ij->i", dmid, n1) * R
    xE, xN = np.einsum("ij,ij->i", dx, e1) * R, np.einsum("ij,ij->i", dx, n1) * R
    ref = flux(ee, dE, dN)
    per_layer = []
    for k in range(len(dz)):
        t1 = dz * 0.0
        t1[k] = dz[k]
        per_layer.append(flux(ee, dE, dN, thick=t1))

    spec = importlib.util.spec_from_file_location("pyfesom2_ut", PYFESOM2_UT)
    ut = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(ut)
    clon, clat = np.degrees(np.arctan2(C[need, 1], C[need, 0])), np.degrees(np.arcsin(C[need, 2]))
    ru, rv = ut.vec_rotate_r2g(50, 15, -90, np.broadcast_to(clon, uu.shape).ravel(),
                               np.broadcast_to(clat, uu.shape).ravel(), uu.ravel(), vv.ravel(), 1)
    ru, rv = ru.reshape(uu.shape), rv.reshape(vv.shape)

    mid_nz1 = np.concatenate([[nz1[0] * 2], (nz1[2:] - nz1[:-2]) / 2, [nz1[-1] - nz1[-2]]])
    variants = {
        "exact_chord": flux(ee, xE, xN),
        "radius_6371": flux(ee, dE * 6371000.0 / R, dN * 6371000.0 / R),
        "pyfesom2_like": flux(pee, pdE, pdN),
        "sampled_2km": flux(hold[sm], sdE[sm], sdN[sm]),
        "dz_from_nz1": flux(ee, dE, dN, thick=mid_nz1),
        "u_only": flux(ee, dE, dN, use_v=False),
        "rotated": flux(ee, dE, dN, uu=ru, vv=rv),
        "sign_reversed": -ref,
    }
    out = {"section": [P0, P1], "length_km": arc * R / 1e3, "elements": int(len(ee)), "odd_cut_elements": odd,
           "reference_sv": ref, "per_layer_sv": per_layer, "variants": {}, "wall_s": round(time.time() - t0, 1)}
    for k, v in variants.items():
        out["variants"][k] = {"sv": v, "rel_err": (v - ref) / abs(ref)}
    print(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
