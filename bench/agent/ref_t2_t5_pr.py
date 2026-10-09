#!/usr/bin/env python3
"""Agent check, tasks T2 and T5 references (xarray, independent of cdors).

Dataset: EERIE ICON-ESM-ER control-1950, atmos 0.25 deg daily mean, variable pr (kg m-2 s-1).
Dates follow the dataset's reference time axis (kerchunk refs / EERIE cloud: 1950-01-01T12:00,
daily, gregorian). Box edges are included; longitudes are 0..359.75, so a box crossing 0 E is
lon >= 360+W or lon <= E.

T2: box 80W-0, 20N-70N, annual means 1950..1954, mm/day (x 86400; 1 kg m-2 = 1 mm water).
T5: box 10W-40E, 35N-70N, January 1950 mean, mm/day.
Area weights: cos(lat) of the cell centre (a regular lon-lat grid; equals the exact spherical
cell area up to a factor that is constant to < 1e-5 within the box). The script also prints the
variants used to size the tolerance (unweighted, edges excluded).

Run: python -I ref_t2_t5_pr.py T2|T5 [local|cloud]
  local: the Parquet kerchunk refs on Lustre (reads the raw NetCDF chunks); cloud: EERIE cloud.
"""
import sys
import time

import fsspec
import numpy as np
import xarray as xr

PARQ = "/work/bm1344/k202193/Kerchunk/erc2002/control_1950/v20240618/atm_2d_1d_mean_remap025.parq"
CLOUD = ("https://eerie.cloud.dkrz.de/datasets/"
         "icon-esm-er.eerie-control-1950.v20240618.atmos.gr025.2d_daily_mean/kerchunk")


def open_ds(src):
    if src == "local":
        fs = fsspec.filesystem("reference", fo=PARQ, remote_protocol="file", lazy=True)
        return xr.open_zarr(fs.get_mapper(""), consolidated=False, chunks={})
    return xr.open_zarr(CLOUD, consolidated=True, chunks={})


def box_index(lon, lat, w, e, s, n, inclusive=True):
    lon360 = np.mod(lon, 360.0)
    w360, e360 = w % 360.0, e % 360.0
    if inclusive:
        inlon = (lon360 >= w360) | (lon360 <= e360) if w360 > e360 else (lon360 >= w360) & (lon360 <= e360)
        inlat = (lat >= s) & (lat <= n)
    else:
        inlon = (lon360 > w360) | (lon360 < e360) if w360 > e360 else (lon360 > w360) & (lon360 < e360)
        inlat = (lat > s) & (lat < n)
    return np.flatnonzero(inlon), np.flatnonzero(inlat)


def box_means(pr, lon, lat, box, steps):
    """daily box means (f64) for the given steps, three variants"""
    out = {}
    ilon, ilat = box_index(lon, lat, *box)
    sub = pr.isel(time=steps, lat=ilat, lon=ilon).astype("f8").compute(scheduler="threads", num_workers=16)
    w = np.cos(np.deg2rad(lat[ilat]))
    a = sub.values  # time, lat, lon
    out["weighted"] = (a.mean(axis=2) * w).sum(axis=1) / w.sum()
    out["unweighted"] = a.mean(axis=(1, 2))
    jlon, jlat = box_index(lon, lat, *box, inclusive=False)
    b = a[:, np.isin(ilat, jlat)][:, :, np.isin(ilon, jlon)]
    wb = np.cos(np.deg2rad(lat[jlat]))
    out["weighted_edges_excluded"] = (b.mean(axis=2) * wb).sum(axis=1) / wb.sum()
    print(f"box {box}: {len(ilat)} lat x {len(ilon)} lon points (edges excluded: {len(jlat)} x {len(jlon)})")
    return out


def main():
    task = sys.argv[1]
    src = sys.argv[2] if len(sys.argv) > 2 else "local"
    t0 = time.time()
    ds = open_ds(src)
    lon, lat = ds["lon"].values, ds["lat"].values
    t = ds["time"].to_index()
    print("time axis", t[0], "...", t[-1], len(t), "| units", ds["pr"].attrs.get("units"))
    if task == "T2":
        steps = np.flatnonzero((t.year >= 1950) & (t.year <= 1954))
        m = box_means(ds["pr"], lon, lat, (-80, 0, 20, 70), steps)
        years = t.year[steps]
        for k, v in m.items():
            vals = [86400 * v[years == y].mean() for y in range(1950, 1955)]
            print(f"T2 {k}:", " ".join(f"{x:.6f}" for x in vals), "mm/day",
                  "| days per year", [int((years == y).sum()) for y in range(1950, 1955)])
    elif task == "T5":
        steps = np.flatnonzero((t.year == 1950) & (t.month == 1))
        assert len(steps) == 31
        m = box_means(ds["pr"], lon, lat, (-10, 40, 35, 70), steps)
        for k, v in m.items():
            print(f"T5 {k}: {86400 * v.mean():.6f} mm/day")
    print(f"wall {time.time() - t0:.1f} s ({src})")


if __name__ == "__main__":
    main()
