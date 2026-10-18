#!/usr/bin/env python3
"""nereus baseline for cdors round 1 (plan Task 1): Atlantic OHC 0-700 m and NH sea-ice extent.

    baseline.py --years 2001-2002 [--data DIR] [--mesh DIR] [--out DIR] [--workers 16]
                [--what ohc,ice] [--full-column] [--chunks file|nod2] [--mesh-cache F.npz]
                [--steps N]

Computes, with nereus' public API on xarray/dask, from raw FESOM2 output (one file per variable
and year, `temp.fesom.<year>.nc`, `a_ice.fesom.<year>.nc`):
  ohc  Atlantic ocean heat content 0-700 m, one value per monthly step, in J:
       nr.heat_content(temp, mesh.area, mesh.layer_thickness, mesh.depth, depth_max=700,
                       mask=nr.get_region_mask(lon, lat, "Atlantic_Basin", "oceanBasins"))
       temp is cut to the layers with centres <= 700 m before the call (as cdors reads only these
       layers); --full-column passes the whole column instead, so dask reads all 79 layers.
  ice  NH sea-ice extent (area where a_ice >= 0.15, lat > 0), one value per output step, in m2:
       nr.ice_extent_nh(a_ice, mesh.area, mesh.lat)

The mesh comes from nr.fesom.load_mesh(<mesh dir>) (ASCII nod2d/elem2d/aux3d; node areas computed
from the triangles, surface level only). On dart this takes minutes, so --mesh-cache F.npz stores
lon, lat, area, depth, layer thickness and the Atlantic mask after the first run (whose "mesh" and
"mask" phases are the cold cost) and later runs load them from there, wrapped as dask arrays with
one chunk exactly as load_mesh returns them (phase "mesh_cache"). Dry layers are stored as 0 in FESOM output, so with the
reference temperature 0 they add nothing to the sum.

Dask runs on its threaded scheduler with --workers threads; the input is opened with
xr.open_mfdataset through the netCDF4 engine, which serialises HDF5 reads behind xarray's lock.
--chunks file (default) keeps the files' own chunks (chunks={}); --chunks nod2 takes whole fields
per chunk along nod2 (fewer, larger dask tasks).

Writes to --out: nereus_ohc_atl_0_700.nc / .csv, nereus_siextent_nh.nc / .csv and
nereus_timing.json (one JSON object, also printed on stdout): per phase wall seconds (import, mesh,
mask, open, ohc, ice), peak RSS of this process, bytes the computation has to decode, the dask
settings, versions. Run with python -I.
"""
import argparse
import json
import os
import resource
import sys
import time

T0 = time.perf_counter()

DATA = "/work/ab0995/a270210/runtime/awicm3-v3.1_refactoring/TCO319L137-DART/hist1950d/outdata/fesom"
MESH = "/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/dart"


def parse_args():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--years", required=True, help="Y1-Y2 or Y")
    p.add_argument("--data", default=DATA)
    p.add_argument("--mesh", default=MESH)
    p.add_argument("--out", required=True)
    p.add_argument("--workers", type=int, default=16)
    p.add_argument("--what", default="ohc,ice")
    p.add_argument("--full-column", action="store_true")
    p.add_argument("--chunks", choices=["file", "nod2"], default="file")
    p.add_argument("--mesh-cache")
    p.add_argument("--steps", type=int, help="only the first N time steps (smoke tests)")
    return p.parse_args()


def peak_rss_gb():
    return resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1e6  # kB on Linux


def main():
    a = parse_args()
    y = [int(s) for s in a.years.split("-")]
    years = list(range(y[0], y[-1] + 1))
    what = a.what.split(",")
    os.makedirs(a.out, exist_ok=True)
    for v in ("OMP_NUM_THREADS", "OPENBLAS_NUM_THREADS", "MKL_NUM_THREADS"):
        os.environ.setdefault(v, "1")

    import dask
    import numpy as np
    import xarray as xr
    import nereus as nr

    dask.config.set(scheduler="threads", num_workers=a.workers)
    t = {"import": time.perf_counter() - T0}
    rec = {"tool": "nereus", "years": a.years, "data": a.data, "mesh": a.mesh,
           "workers": a.workers, "scheduler": "threads", "chunks": a.chunks,
           "engine": "netcdf4", "full_column": a.full_column, "steps": a.steps,
           "versions": {"nereus": nr.__version__, "xarray": xr.__version__,
                        "dask": dask.__version__, "numpy": np.__version__}}

    import dask.array as da
    s = time.perf_counter()
    if a.mesh_cache and os.path.exists(a.mesh_cache):
        z = np.load(a.mesh_cache)
        lon, lat, atl = z["lon"], z["lat"], z["atl"]
        area, lat_d = da.from_array(z["area"], chunks=-1), da.from_array(lat, chunks=-1)
        depth_all, thick_all = z["depth"], z["thick"]
        t["mesh_cache"] = time.perf_counter() - s
    else:
        mesh = nr.fesom.load_mesh(a.mesh)
        lon = np.asarray(mesh["lon"].values)
        lat = np.asarray(mesh["lat"].values)
        area, lat_d = mesh["area"], mesh["lat"]
        depth_all, thick_all = mesh["depth"].values, mesh["layer_thickness"].values
        t["mesh"] = time.perf_counter() - s
        s = time.perf_counter()
        atl = nr.get_region_mask(lon, lat, "Atlantic_Basin", "oceanBasins")
        t["mask"] = time.perf_counter() - s
        if a.mesh_cache:
            np.savez(a.mesh_cache, lon=lon, lat=lat, area=np.asarray(area.values), atl=atl,
                     depth=depth_all, thick=thick_all)
    rec["atlantic_nodes"] = int(atl.sum())
    chunks = {} if a.chunks == "file" else {"time": 1, "nz1": 4, "nod2": -1}
    decoded = 0

    if "ohc" in what:
        s = time.perf_counter()
        files = [f"{a.data}/temp.fesom.{yr}.nc" for yr in years]
        ds = xr.open_mfdataset(files, chunks=chunks, combine="by_coords", engine="netcdf4",
                               parallel=False)
        temp = ds["temp"] if a.steps is None else ds["temp"].isel(time=slice(0, a.steps))
        depth, thick = depth_all, thick_all
        if not a.full_column:
            nlev = int((depth <= 700).sum())
            temp = temp.isel(nz1=slice(0, nlev))
            depth, thick = depth[:nlev], thick[:nlev]
        t["open_temp"] = time.perf_counter() - s
        decoded += temp.size * temp.dtype.itemsize

        s = time.perf_counter()
        ohc = nr.heat_content(temp, area, thick, depth, depth_max=700.0,
                              mask=atl, as_xarray=True)
        ohc = ohc.compute()
        t["ohc"] = time.perf_counter() - s
        ohc.name = "ohc_atl_0_700"
        ohc.attrs.update(units="J", long_name="Atlantic heat content 0-700 m (nereus)")
        ohc.to_netcdf(f"{a.out}/nereus_ohc_atl_0_700.nc")
        ohc.to_dataframe().to_csv(f"{a.out}/nereus_ohc_atl_0_700.csv")
        rec["ohc_steps"] = int(ohc.size)
        rec["ohc_first_last"] = [float(ohc[0]), float(ohc[-1])]
        rec["peak_rss_gb_after_ohc"] = peak_rss_gb()

    if "ice" in what:
        s = time.perf_counter()
        files = [f"{a.data}/a_ice.fesom.{yr}.nc" for yr in years]
        ds = xr.open_mfdataset(files, chunks=chunks, combine="by_coords", engine="netcdf4",
                               parallel=False)
        sic = ds["a_ice"] if a.steps is None else ds["a_ice"].isel(time=slice(0, a.steps))
        t["open_ice"] = time.perf_counter() - s
        decoded += sic.size * sic.dtype.itemsize

        s = time.perf_counter()
        ext = nr.ice_extent_nh(sic, area, lat_d, as_xarray=True)
        ext = ext.compute()
        t["ice"] = time.perf_counter() - s
        ext.name = "siextent_nh"
        ext.attrs.update(units="m2", long_name="NH sea-ice extent, a_ice >= 0.15 (nereus)")
        ext.to_netcdf(f"{a.out}/nereus_siextent_nh.nc")
        ext.to_dataframe().to_csv(f"{a.out}/nereus_siextent_nh.csv")
        rec["ice_steps"] = int(ext.size)
        rec["ice_first_last"] = [float(ext[0]), float(ext[-1])]

    t["total"] = time.perf_counter() - T0
    rec["wall_s"] = {k: round(v, 2) for k, v in t.items()}
    rec["decoded_gb"] = round(decoded / 1e9, 3)
    compute = t.get("ohc", 0) + t.get("ice", 0)
    rec["compute_gb_per_s"] = round(decoded / 1e9 / compute, 3) if compute else None
    rec["peak_rss_gb"] = round(peak_rss_gb(), 3)
    with open(f"{a.out}/nereus_timing.json", "w") as f:
        json.dump(rec, f, indent=1)
    print(json.dumps(rec))


if __name__ == "__main__":
    sys.exit(main())
