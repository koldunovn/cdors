#!/usr/bin/env python3
"""Agent check, task T3 reference (xarray/numpy, independent of cdors).

EERIE ICON-ESM-ER control-1950 ocean, variable `to` at depth 1 m (degC), regular 0.25 deg grid,
January 1950 of the reference time axis (31 daily means = raw directory run_19910101*).
Bilinear remapping to the regular 1 deg grid with cell centres 0.5..359.5 E, 89.5S..89.5N
(not cdo's r360x180, whose first longitude is 0), time mean, global area-weighted mean over the valid (ocean) target cells
(weights cos(lat), proportional to the exact cell area on a regular grid).

The 1 deg centres coincide with 0.25 deg source nodes, so bilinear interpolation returns the
source node value; the methods differ only in which target cells next to land become missing.
Variants printed (to size the tolerance):
  interp_lowerleft : xarray .interp(method="linear") (scipy; a node is interpolated in the cell
                     below/left of it; any NaN corner gives NaN), the primary reference
  cell_upperright  : the same rule with the cell above/right of the node
  node_only        : the source node value wherever it is ocean (no corner rule)
  source_grid      : area-weighted mean on the 0.25 deg source grid (no remapping)
Time mean and remapping commute here (the land mask is the same every day).

Run: python -I ref_t3_remap.py   (31 chunks, ~64 MB on disk)
"""
import time

import fsspec
import numpy as np
import xarray as xr

PARQ = "/work/bm1344/k202193/Kerchunk/erc2002/control_1950/v20240618/oce_2d_1d_mean_remap025.parq"

t0 = time.time()
fs = fsspec.filesystem("reference", fo=PARQ, remote_protocol="file", lazy=True)
ds = xr.open_zarr(fs.get_mapper(""), consolidated=False, chunks={})
t = ds["time"].to_index()
steps = np.flatnonzero((t.year == 1950) & (t.month == 1))
assert len(steps) == 31, len(steps)
to = ds["to"].isel(time=steps).squeeze("depth", drop=True)
print("to attrs:", {k: v for k, v in to.attrs.items() if k in ("units", "long_name")},
      "encoding missing/fill:", to.encoding.get("missing_value"), to.encoding.get("_FillValue"))
src = to.astype("f8").mean("time").compute(scheduler="threads", num_workers=16)  # NaN on land
v = src.values
lat, lon = src["lat"].values, src["lon"].values
assert np.allclose(np.diff(lat), 0.25) and np.allclose(np.diff(lon), 0.25) and lon[0] == 0.0
print(f"source: {np.isfinite(v).sum()} ocean of {v.size} cells, range {np.nanmin(v):.3f}..{np.nanmax(v):.3f}")

tlat = np.arange(-89.5, 90, 1.0)
tlon = np.arange(0.5, 360, 1.0)


def wmean(field, lats):
    w = np.cos(np.deg2rad(lats))[:, None] * np.ones(field.shape)
    ok = np.isfinite(field)
    return (field[ok] * w[ok]).sum() / w[ok].sum(), ok.sum()


res = {}
res["source_grid"] = wmean(v, lat)
remapped = src.interp(lat=tlat, lon=tlon, method="linear").values
res["interp_lowerleft"] = wmean(remapped, tlat)
# node indices of the target centres on the source grid
ilat = np.rint((tlat - lat[0]) / 0.25).astype(int)
ilon = np.rint((tlon - lon[0]) / 0.25).astype(int)
node = v[np.ix_(ilat, ilon)]
res["node_only"] = wmean(node, tlat)
ur = node.copy()
for dj, di in ((0, 1), (1, 0), (1, 1)):
    nb = v[np.ix_(ilat + dj, (ilon + di) % len(lon))]
    ur[~np.isfinite(nb)] = np.nan
res["cell_upperright"] = wmean(ur, tlat)
for k, (m, n) in res.items():
    print(f"T3 {k}: {m:.6f} degC ({n} valid cells)")
print(f"wall {time.time() - t0:.1f} s")
