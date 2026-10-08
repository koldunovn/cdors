#!/usr/bin/env python3
"""xarray + dask + flox baseline for the cdors benchmarks W1 and W2 (Task 13).

    xarray_baseline.py w1 SRC --var tas [--start ISO] [--end ISO] [--ntime N] [--out F.nc]
    xarray_baseline.py w2 SRC --var pr --box LON1,LON2,LAT1,LAT2 [--weights AREA.nc] [...]

  w1  yearmean: mean per calendar year over the time axis (flox groupby "time.year"),
      the same statistic as `cdo yearmean` (missing values skipped).
  w2  fldmean of a lon-lat box: `cdo fldmean -sellonlatbox,LON1,LON2,LAT1,LAT2`, area-weighted.
      With --weights, the weights are the `cell_area` of `cdo gridarea` (any file whose grid
      contains the box; matched by latitude, since on a regular lon-lat grid the area depends on
      latitude only). Without it, the exact band area sin(lat+dlat/2) - sin(lat-dlat/2) of each
      row is used, clipped at the poles, which is cdo's weighting up to rounding.
      Box rule (as cdo's sellonlatbox on a regular grid): centres with LAT1 <= lat <= LAT2 and
      lon inside [LON1, LON2] modulo 360, both ends included.

SRC: an https URL (Zarr v2 with consolidated metadata, e.g. the EERIE cloud /kerchunk endpoint),
a Parquet kerchunk reference directory (*.parq, read through fsspec "reference://"), a Zarr
directory, or a NetCDF file (fixtures).

Time window: --start/--end select by date (inclusive, ISO strings, as cdo seldate);
--ntime N then keeps the first N steps (for tiny tests). Dask runs on its threaded scheduler
with --workers threads; blosc's own threads are switched off to avoid oversubscription.

Prints exactly one JSON line on stdout: workload, source, nsteps, ncells, decoded_gb (whole
chunks fetched, steps x full field x itemsize), open_s, compute_s, write_s, total_s, workers,
out. The result is written to --out as NetCDF that cdo can read (time axis plus the original
spatial dims for w1; time x lat(1) x lon(1) for w2), so `cdo diffn` can compare it with cdo's.
Run with python -I.
"""
import argparse
import json
import os
import sys
import time


def parse_args():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("workload", choices=["w1", "w2"])
    p.add_argument("src")
    p.add_argument("--var", required=True)
    p.add_argument("--start")
    p.add_argument("--end")
    p.add_argument("--ntime", type=int)
    p.add_argument("--box", help="LON1,LON2,LAT1,LAT2 (w2)")
    p.add_argument("--weights", help="cdo gridarea output (w2)")
    p.add_argument("--workers", type=int, default=os.cpu_count())
    p.add_argument("--out")
    return p.parse_args()


def open_source(src, var):
    import xarray as xr
    if src.startswith(("http://", "https://")):
        ds = xr.open_dataset(src, engine="zarr", consolidated=True, chunks={})
    elif src.rstrip("/").endswith((".parq", ".parquet")):
        ds = xr.open_dataset("reference://", engine="zarr", chunks={},
                             backend_kwargs={"consolidated": False,
                                             "storage_options": {"fo": src, "remote_protocol": "file",
                                                                 "lazy": True}})
    elif os.path.isdir(src):
        ds = xr.open_dataset(src, engine="zarr", chunks={})
    else:
        ds = xr.open_dataset(src, chunks={})
    return ds[var]


def select_time(da, a):
    if a.start or a.end:
        da = da.sel(time=slice(a.start, a.end))
    if a.ntime:
        da = da.isel(time=slice(0, a.ntime))
    return da


def box_weights(da, a):
    import numpy as np
    import xarray as xr
    lon1, lon2, lat1, lat2 = (float(x) for x in a.box.split(","))
    lat, lon = da["lat"], da["lon"]
    in_lat = (lat >= lat1) & (lat <= lat2)
    width = (lon2 - lon1) % 360.0 if lon2 - lon1 < 360.0 else 360.0
    in_lon = ((lon - lon1) % 360.0) <= width
    sub = da.isel(lat=np.flatnonzero(in_lat.values), lon=np.flatnonzero(in_lon.values))
    if a.weights:
        area = xr.open_dataset(a.weights)["cell_area"]
        alat = area["lat"] if "lat" in area.coords else area[area.dims[0]]
        row = area.isel({d: 0 for d in area.dims if d != alat.dims[0]})
        w = row.assign_coords({alat.dims[0]: alat.values}).rename({alat.dims[0]: "lat"})
        w = w.sel(lat=sub["lat"].values, method="nearest", tolerance=1e-6)
        w = xr.DataArray(w.values.astype("f8"), dims=["lat"], coords={"lat": sub["lat"].values})
        how = "cdo gridarea"
    else:
        la = np.deg2rad(da["lat"].values.astype("f8"))
        edges = np.concatenate([[-np.pi / 2], (la[1:] + la[:-1]) / 2, [np.pi / 2]])
        band = np.sin(edges[1:]) - np.sin(edges[:-1])
        w = xr.DataArray(band, dims=["lat"], coords={"lat": da["lat"].values}).sel(lat=sub["lat"].values)
        how = "band area"
    return sub, w, how


def main():
    a = parse_args()
    t0 = time.perf_counter()
    import dask
    import numpy as np
    import xarray as xr
    try:
        import numcodecs.blosc
        numcodecs.blosc.use_threads = False
    except Exception:
        pass
    import flox  # noqa: F401  (xarray uses it for groupby when installed)
    xr.set_options(use_flox=True)
    dask.config.set(scheduler="threads", num_workers=a.workers)

    da = select_time(open_source(a.src, a.var), a)
    nsteps = int(da.sizes["time"])
    field = int(np.prod([n for d, n in da.sizes.items() if d != "time"]))
    decoded_gb = nsteps * field * da.dtype.itemsize / 1e9
    t_open = time.perf_counter()

    extra = {}
    if a.workload == "w1":
        first = da["time"].groupby("time.year").min()
        # accumulate in float64, as cdo does (a float32 mean is off by several ulps)
        res = da.astype("f8").groupby("time.year").mean(skipna=True)
        res = res.rename(year="time").assign_coords(time=first.values)
    else:
        if not a.box:
            sys.exit("w2 needs --box")
        sub, w, how = box_weights(da, a)
        valid = sub.notnull()
        num = (sub * w).sum(("lat", "lon"))
        den = (valid * w).sum(("lat", "lon"))
        res = (num / den).astype(da.dtype)
        res = res.expand_dims(lat=[float(sub["lat"].mean())], lon=[float(sub["lon"].mean())], axis=(1, 2))
        extra = {"weights": how, "box_cells": int(sub.sizes["lat"] * sub.sizes["lon"])}
    res = res.astype(da.dtype).compute()
    t_comp = time.perf_counter()

    if a.out:
        res.name = a.var
        res.attrs = {k: v for k, v in da.attrs.items() if k not in ("grid_mapping", "coordinates")}
        # cdo's default missing value; cdo does not treat a NaN _FillValue as missing
        enc = {a.var: {"_FillValue": np.array(-9e33, dtype=da.dtype)[()]}}
        tenc = da["time"].encoding
        if "units" in tenc:
            enc["time"] = {"units": tenc["units"], "calendar": tenc.get("calendar", "standard")}
        res.to_dataset().to_netcdf(a.out, encoding=enc)
    t_end = time.perf_counter()

    print(json.dumps({
        "workload": a.workload, "source": a.src, "var": a.var, "nsteps": nsteps, "ncells": field,
        "decoded_gb": round(decoded_gb, 4), "open_s": round(t_open - t0, 3),
        "compute_s": round(t_comp - t_open, 3), "write_s": round(t_end - t_comp, 3),
        "total_s": round(t_end - t0, 3), "workers": a.workers, "out": a.out, **extra}))


if __name__ == "__main__":
    main()
