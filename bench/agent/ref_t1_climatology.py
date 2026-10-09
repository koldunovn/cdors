#!/usr/bin/env python3
"""Agent check, task T1 reference (xarray, independent of cdors).

Global-mean July temperature of the 2020-2024 monthly climatology of nextGEMS ngc4008 daily tas
(HEALPix zoom 9, equal-area cells, so the area-weighted mean is the plain mean over cells).
Days are grouped by the calendar month of their stored timestamp (each daily mean is stamped
00:00 at the END of its day, so stamp 2020-07-01 00:00 is the mean of 30 June). The variant
"shifted" groups by the day the mean actually covers (stamp minus one day) and is printed only to
size the tolerance.

Run: python -I ref_t1_climatology.py   (hk25 env; reads ~3.8 GB decoded, ~2.1 GB on disk)
"""
import time

import numpy as np
import xarray as xr

STORE = "/work/kd1453/rechunked_ngc4008/ngc4008_P1D_9.zarr"

t0 = time.time()
ds = xr.open_zarr(STORE, consolidated=True)
tas = ds["tas"]
t = ds["time"].to_index()
res = {}
for label, tt in (("stored", t), ("shifted", t - np.timedelta64(1, "D"))):
    sel = np.flatnonzero((tt.month == 7) & (tt.year >= 2020) & (tt.year <= 2024))
    assert len(sel) == 155, len(sel)
    # per-day global means in float64, then the mean over the 155 July days
    # (equal-length Julys: mean of daily values == mean of the five monthly means)
    daily = tas.isel(time=sel).astype("f8").mean("cell").compute(scheduler="threads", num_workers=16)
    res[label] = float(daily.mean())
    print(label, "first/last stamp", tt[sel[0]], tt[sel[-1]], "n", len(sel))
for k, v in res.items():
    print(f"T1 {k}: {v:.6f} K")
print(f"relative difference stored vs shifted: {abs(res['stored'] - res['shifted']) / res['stored']:.2e}")
print(f"wall {time.time() - t0:.1f} s")
