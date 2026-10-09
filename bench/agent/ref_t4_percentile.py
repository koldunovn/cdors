#!/usr/bin/env python3
"""Agent check, task T4 reference (numpy/healpy, independent of cdors and of cdo's histogram).

95th percentile (nearest rank: sort the n values, take rank ceil(0.95 n)) of 3-hourly tas in 2020
at the HEALPix zoom-9 (nside 512, nested) cell of ngc4008 that contains Hamburg (53.55 N, 10.0 E);
the script checks that this cell is also the one with the nearest centre.
"Year 2020" = stored timestamps in 2020. The stamps mark the END of each 3-hour mean
(2020-01-01T03:00 .. 2021-01-01T00:00), so this gives 2927 values; the variant "intervals in 2020"
(stamps 2020-01-01T03:00 .. 2021-01-01T00:00, 2928 values) and neighbouring ranks are printed to
size the tolerance.

Run: python -I ref_t4_percentile.py   (reads one spatial chunk column: 12 x 16.25 MB decoded)
"""
import math
import time

import healpy as hp
import numpy as np
import xarray as xr

STORE = "/work/kd1453/rechunked_ngc4008/ngc4008_PT3H_9.zarr"
LAT, LON = 53.55, 10.0
NSIDE = 512

t0 = time.time()
cell = int(hp.ang2pix(NSIDE, LON, LAT, nest=True, lonlat=True))
# nearest centre among the containing cell and its 8 neighbours
cand = np.concatenate([[cell], hp.get_all_neighbours(NSIDE, cell, nest=True)])
cand = cand[cand >= 0]
clon, clat = hp.pix2ang(NSIDE, cand, nest=True, lonlat=True)
p = np.deg2rad
d = np.arccos(np.clip(np.sin(p(LAT)) * np.sin(p(clat)) + np.cos(p(LAT)) * np.cos(p(clat)) * np.cos(p(clon - LON)), -1, 1))
nearest = int(cand[np.argmin(d)])
print(f"containing cell {cell}, nearest-centre cell {nearest}; centre {clon[0]:.4f}E {clat[0]:.4f}N, "
      f"distance {np.rad2deg(d[0]) * 111.2:.2f} km (next {np.sort(np.rad2deg(d))[1] * 111.2:.2f} km)")
assert cell == nearest

ds = xr.open_zarr(STORE, consolidated=True)
t = ds["time"].to_index()
x = ds["tas"].isel(cell=cell)


def nrank(v, q):
    v = np.sort(v)
    n = len(v)
    k = min(max(math.ceil(n * q), 1), n)
    return v, k, v[k - 1]


for label, sel in (("stamps in 2020", t.year == 2020),
                   ("intervals in 2020", ((t - np.timedelta64(3, "h")).year == 2020))):
    idx = np.flatnonzero(sel)
    v = x.isel(time=idx).values
    assert np.isfinite(v).all()
    vs, k, val = nrank(v, 0.95)
    print(f"T4 {label}: n={len(v)} rank={k} p95={val:.6f} K  (ranks k-1, k+1: {vs[k - 2]:.6f}, {vs[k]:.6f}; "
          f"numpy linear: {np.percentile(v, 95):.6f}; min {vs[0]:.3f} max {vs[-1]:.3f})")
print(f"wall {time.time() - t0:.1f} s")
