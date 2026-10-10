"""GRIB2 fixtures for tests/make_fixtures.sh, written with ecCodes and indexed with gribscan (run
with the Python of an environment that has eccodes and gribscan).

    python make_grib_fixtures.py OUTDIR

writes OUTDIR/<name>.grb, its gribscan index OUTDIR/<name>.index and gribscan's references to it,
OUTDIR/<name>.json (IFS magician, as for the EERIE IFS-FESOM data), each atomically (temporary
name, then rename) and never over an existing file, for
- grb_ll:  regular lon-lat 10 deg (36 x 19, poles included, north to south), on 2 pressure levels:
           t (simple packing), q (CCSDS packing), r (simple packing with a bit-map: missing cells);
- grb_gg:  regular Gaussian N16 (64 x 32): 2t and msl at the surface (CCSDS);
- grb_rgg: reduced Gaussian N32 (6114 cells in 64 rows): 2t at the surface (simple packing).
The fields are warm at the equator and cold at the poles, so area weights matter for field means.
Times: 6-hourly from 2001-01-01 00:00, instantaneous fields (12 steps).
"""
import json
import os
import sys

import eccodes as ec
import gribscan
import gribscan.gridutils as gu
import numpy as np
from gribscan.magician import IFSMagician

OUT = sys.argv[1]
NT = 12


def field(lat, lon, t, k):
    lat, lon = np.deg2rad(lat), np.deg2rad(lon)
    return 250 + 40 * np.cos(lat) ** 2 + 5 * np.sin(lon + 0.3 * t) * np.cos(lat) - 10 * k + 0.5 * t


def message(sample, keys, values, ccsds=False, bitmap=None):
    h = ec.codes_new_from_samples(sample, ec.CODES_PRODUCT_GRIB)
    for k, v in keys.items():
        if isinstance(v, (list, np.ndarray)):
            ec.codes_set_array(h, k, v)
        else:
            ec.codes_set(h, k, v)
    if bitmap is not None:
        ec.codes_set(h, "bitmapPresent", 1)
        ec.codes_set(h, "missingValue", 9999.0)
        values = np.where(bitmap, values, 9999.0)
    if ccsds:
        ec.codes_set(h, "packingType", "grid_ccsds")
    ec.codes_set(h, "bitsPerValue", 24 if ccsds else 16)
    ec.codes_set_values(h, values)
    msg = ec.codes_get_message(h)
    ec.codes_release(h)
    return msg


def time_keys(t):
    hours = 6 * t
    return {"dataDate": 20010101 + hours // 24, "dataTime": 100 * (hours % 24), "stepType": "instant", "step": 0}


def publish(path, write):
    """Writes `path` through `write(file)` under a temporary name, then renames it."""
    if os.path.exists(path):
        sys.exit(f"{path} exists")
    tmp = f"{path}.tmp{os.getpid()}"
    with open(tmp, "x" if write.__name__ == "text" else "xb") as f:
        write(f)
    os.rename(tmp, path)


def write(name, msgs):
    def binary(f):
        for m in msgs:
            f.write(m)
    publish(f"{OUT}/{name}.grb", binary)


# regular lon-lat, 10 degrees, poles included
lat1 = np.linspace(90, -90, 19)
lon1 = np.arange(36) * 10.0
lat2, lon2 = np.meshgrid(lat1, lon1, indexing="ij")
grid_ll = {
    "Ni": 36, "Nj": 19,
    "latitudeOfFirstGridPointInDegrees": 90, "longitudeOfFirstGridPointInDegrees": 0,
    "latitudeOfLastGridPointInDegrees": -90, "longitudeOfLastGridPointInDegrees": 350,
    "iDirectionIncrementInDegrees": 10, "jDirectionIncrementInDegrees": 10,
}
land = ~((lat2 > 20) & (lat2 < 60) & (lon2 > 60) & (lon2 < 150))
msgs = []
for t in range(NT):
    for k, lev in enumerate([850, 500]):
        base = {**grid_ll, **time_keys(t), "typeOfLevel": "isobaricInhPa", "level": lev}
        f = field(lat2, lon2, t, k).ravel()
        msgs.append(message("regular_ll_pl_grib2", {**base, "shortName": "t"}, f))
        msgs.append(message("regular_ll_pl_grib2", {**base, "shortName": "q"}, 1e-3 * (f - 200), ccsds=True))
        msgs.append(message("regular_ll_pl_grib2", {**base, "shortName": "r"}, f - 200, bitmap=land.ravel()))
write("grb_ll", msgs)

# regular Gaussian N16
glat = np.rad2deg(np.arcsin(np.polynomial.legendre.leggauss(32)[0]))[::-1]
grid_gg = {
    "N": 16, "Ni": 64, "Nj": 32, "iDirectionIncrementInDegrees": 5.625,
    "latitudeOfFirstGridPointInDegrees": round(glat[0], 6), "latitudeOfLastGridPointInDegrees": round(glat[-1], 6),
    "longitudeOfFirstGridPointInDegrees": 0, "longitudeOfLastGridPointInDegrees": 354.375,
}
glat2, glon2 = np.meshgrid(glat, np.arange(64) * 5.625, indexing="ij")
msgs = []
for t in range(NT):
    base = {**grid_gg, **time_keys(t), "typeOfLevel": "surface"}
    f = field(glat2, glon2, t, 0).ravel()
    msgs.append(message("regular_gg_sfc_grib2", {**base, "shortName": "2t"}, f, ccsds=True))
    msgs.append(message("regular_gg_sfc_grib2", {**base, "shortName": "msl"}, 1e5 + 100 * (f - 250), ccsds=True))
write("grb_gg", msgs)

# reduced Gaussian N32 (the sample's grid)
h = ec.codes_new_from_samples("reduced_gg_pl_32_grib2", ec.CODES_PRODUCT_GRIB)
rlat = ec.codes_get_array(h, "latitudes")
rlon = ec.codes_get_array(h, "longitudes")
ec.codes_release(h)
msgs = []
for t in range(NT):
    base = {**time_keys(t), "typeOfLevel": "surface", "level": 0, "shortName": "2t"}
    msgs.append(message("reduced_gg_pl_32_grib2", base, field(rlat, rlon, t, 0)))
write("grb_rgg", msgs)


# gribscan 0.0.7 computes coordinates for reduced grids only; newer versions (which made the EERIE
# references) also for regular ones, from these keys of the message
class RegularLatLon(gu.GribGrid):
    gridType = "regular_ll"
    params = ["Ni", "Nj", "latitudeOfFirstGridPointInDegrees", "longitudeOfFirstGridPointInDegrees",
              "iDirectionIncrementInDegrees", "jDirectionIncrementInDegrees", "iScansNegatively",
              "jScansPositively"]

    @classmethod
    def compute_coords(cls, Ni, Nj, latitudeOfFirstGridPointInDegrees, longitudeOfFirstGridPointInDegrees,
                       iDirectionIncrementInDegrees, jDirectionIncrementInDegrees, iScansNegatively,
                       jScansPositively):
        di = -iDirectionIncrementInDegrees if iScansNegatively else iDirectionIncrementInDegrees
        dj = jDirectionIncrementInDegrees if jScansPositively else -jDirectionIncrementInDegrees
        lats = latitudeOfFirstGridPointInDegrees + dj * np.arange(Nj)
        lons = longitudeOfFirstGridPointInDegrees + di * np.arange(Ni)
        lat, lon = np.meshgrid(lats, lons, indexing="ij")
        return {"lon": lon.ravel(), "lat": lat.ravel()}


class RegularGaussian(gu.GribGrid):
    gridType = "regular_gg"
    params = ["N", "Ni", "longitudeOfFirstGridPointInDegrees", "iDirectionIncrementInDegrees"]

    @classmethod
    def compute_coords(cls, N, Ni, longitudeOfFirstGridPointInDegrees, iDirectionIncrementInDegrees):
        lats = np.rad2deg(-np.arcsin(gu.roots_legendre(2 * N)[0]))
        lons = longitudeOfFirstGridPointInDegrees + iDirectionIncrementInDegrees * np.arange(Ni)
        lat, lon = np.meshgrid(lats, lons, indexing="ij")
        return {"lon": lon.ravel(), "lat": lat.ravel()}


gu.grids.update({g.gridType: g for g in (RegularLatLon, RegularGaussian)})
for name in ("grb_ll", "grb_gg", "grb_rgg"):
    index = f"{OUT}/{name}.index"
    if os.path.exists(index):
        sys.exit(f"{index} exists")
    gribscan.write_index(f"{OUT}/{name}.grb", f"{index}.tmp{os.getpid()}")
    os.rename(f"{index}.tmp{os.getpid()}", index)
    refs = gribscan.grib_magic([index], magician=IFSMagician(), global_prefix=OUT + "/")
    (ref,) = refs.values()

    def text(f):
        json.dump(ref, f)
    publish(f"{OUT}/{name}.json", text)
