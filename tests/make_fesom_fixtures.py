"""FESOM2 and region-edge fixtures for tests/make_fixtures.sh (run with the Python of the nereus
environment: numpy, netCDF4 and xarray + zarr for the Zarr copies).

    python make_fesom_fixtures.py OUTDIR MESHDIR

writes into OUTDIR, each atomically (temporary name, then rename) and never over an existing file:
- fesom_pi/      copy of MESHDIR (the pi mesh: 3140 nodes, 5839 elements, 47 layers) with only
                 fesom.mesh.diag.nc and the cdo grid descriptions pi_griddes_{nodes,elements}.nc
                 (old layout: nodes in radians, dimensions nod_n, elem_n, nl, nl1);
- fesom_pi_deg/  the same mesh.diag in the layout of newer meshes such as ng5: nodes in degrees,
                 dimensions nod2, elem, nz, nz1, variables zbar -> nz, Z -> nz1, elem -> elements,
                 extra lon(nod2) and lat(nod2), gradient_sca_x/y transposed to (elem, n3);
- fesom_pi_data.nc  24 monthly means (2000-2001) in FESOM2's output layout: no lon/lat, time in
                 "seconds since 2000-01-01 0:0:0" (no calendar attribute, stamped one model step
                 before the end of each month), NetCDF-4 classic, deflate 1, chunks splitting the
                 cells; temp, salt (time, nz1, nod2), a_ice, m_ice (time, nod2), u, v (time, nz1,
                 elem), float32; levels below each node's / element's bottom stored as 0;
- fesom_pi_data_v20.nc  the same values in FESOM 2.0's layout: temp, salt (time, nod2, nz1),
                 u, v (time, elem, nz1) (horizontal dimension before the levels);
- r1x1_edges.nc  a 1 deg lon-lat grid with centres on integer degrees (lon -180..179, lat -90..90),
                 so cell centres fall exactly on AR6 outlines, at -180 and at the poles; 2 steps.
The Zarr copies of fesom_pi_data (.zarr2, .zarr3 and _tiny.zarr2, whose chunks split nod2/elem
and nz1) are written by make_fixtures.sh with ZARR_PYTHON.

The layout choices are the constants below (one line each), since real FESOM2 output differs
between versions: FESOM 2.0 writes (time, nod2, nz1), FESOM 2.1 and later (time, nz1, nod2) (AWI-CM3
dart output, docs/unstructured-baseline.md).
"""
import datetime
import os
import shutil
import sys

import netCDF4
import numpy as np

OUT, MESH = sys.argv[1], sys.argv[2]

ORDER_NODE = ("time", "nz1", "nod2")    # FESOM 2.1 and later
ORDER_ELEM = ("time", "nz1", "elem")
ORDER_NODE_V20 = ("time", "nod2", "nz1")  # FESOM 2.0 (fesom_pi_data_v20)
ORDER_ELEM_V20 = ("time", "elem", "nz1")
DRY = 0.0                               # value stored below the bottom (FESOM2 writes 0)
DTYPE = "f4"
FILL = netCDF4.default_fillvals["f4"]   # FESOM2 declares the default fill (9.96921e+36)
CALENDAR = None                         # FESOM2 writes no calendar attribute (standard)
DT = 3600                               # model step: FESOM stamps a mean one step before its end
NT = 24
NC_PARTS = 8                            # NetCDF chunks: one step, 1/8 of the cells, all levels (--mem 1M)
MESH_FILES = ("fesom.mesh.diag.nc", "pi_griddes_nodes.nc", "pi_griddes_elements.nc")


def done(path):
    print(f"made:   {path}")


def atomic_dir(name, fill):
    dst = os.path.join(OUT, name)
    if os.path.exists(dst):
        print(f"exists: {dst}")
        return
    tmp = f"{dst}.tmp{os.getpid()}"
    os.mkdir(tmp)
    fill(tmp)
    os.rename(tmp, dst)
    done(dst)


def atomic_file(name, write):
    dst = os.path.join(OUT, name)
    if os.path.exists(dst):
        print(f"exists: {dst}")
        return
    tmp = f"{dst}.tmp{os.getpid()}"
    write(tmp)
    os.rename(tmp, dst)
    done(dst)


# ---------------------------------------------------------------- meshes
def copy_mesh(tmp):
    for f in MESH_FILES:
        shutil.copy2(os.path.join(MESH, f), tmp)


NG5_DIMS = {"nod_n": "nod2", "elem_n": "elem", "nl": "nz", "nl1": "nz1"}
NG5_VARS = {"zbar": "nz", "Z": "nz1", "elem": "elements"}
NG5_TRANSPOSED = ("gradient_sca_x", "gradient_sca_y")   # (n3, elem) -> (elem, n3)


def deg_mesh(tmp):
    with netCDF4.Dataset(os.path.join(MESH, "fesom.mesh.diag.nc")) as src, \
         netCDF4.Dataset(os.path.join(tmp, "fesom.mesh.diag.nc"), "w", format=src.data_model) as dst:
        for d, dim in src.dimensions.items():
            dst.createDimension(NG5_DIMS.get(d, d), len(dim))
        for name, v in src.variables.items():
            dims = tuple(NG5_DIMS.get(d, d) for d in v.dimensions)
            data = v[:]
            if name == "nodes":
                data = np.rad2deg(data)
            if name in NG5_TRANSPOSED:
                dims, data = dims[::-1], data.T
            out = dst.createVariable(NG5_VARS.get(name, name), v.dtype, dims)
            out.setncatts({a: v.getncattr(a) for a in v.ncattrs()})
            out[:] = data
        lonlat = np.rad2deg(src["nodes"][:])
        for i, (name, long) in enumerate((("lon", "longitude"), ("lat", "latitude"))):
            out = dst.createVariable(name, "f8", ("nod2",))
            out.long_name = long
            out[:] = lonlat[i]


# ---------------------------------------------------------------- data
def month_ends():
    """Seconds since 2000-01-01 at the end of each of the NT months, minus one model step."""
    t = netCDF4.date2num([datetime.datetime(2000 + (m + 1) // 12, (m + 1) % 12 + 1, 1) for m in range(NT)],
                         "seconds since 2000-01-01", "standard")
    return np.asarray(t, dtype="f8") - DT


def lonlat_elem(lon, lat, tri):
    """Element centres: the normalised mean of the three nodes' unit vectors."""
    lo, la = np.deg2rad(lon), np.deg2rad(lat)
    xyz = np.stack([np.cos(la) * np.cos(lo), np.cos(la) * np.sin(lo), np.sin(la)])
    c = xyz[:, tri].mean(axis=1)
    return np.rad2deg(np.arctan2(c[1], c[0])), np.rad2deg(np.arctan2(c[2], np.hypot(c[0], c[1])))


# Analytic fields of lon, lat (degrees), depth z (m, positive down) and month m (0-based):
# smooth, without symmetry in lon or between the hemispheres, with a seasonal cycle.
def season(m, phase=0.0):
    return np.sin(2 * np.pi * (m + 0.5 - phase) / 12)


def temp(lon, lat, z, m):
    lo, la = np.deg2rad(lon), np.deg2rad(lat)
    return (1.5 + (26 * np.cos(la) ** 2 - 1.5 + 2 * np.sin(lo + 0.3) * np.cos(la)) * np.exp(-z / 800)
            + 3 * season(m) * np.sin(la) * np.exp(-z / 100) + 0.01 * m)


def salt(lon, lat, z, m):
    lo, la = np.deg2rad(lon), np.deg2rad(lat)
    return (34.7 + 0.9 * np.cos(2 * la) * np.exp(-z / 500) + 0.3 * np.cos(lo) * np.sin(la)
            + 0.1 * season(m, 2) * np.exp(-z / 50))


def a_ice(lon, lat, m):
    """Ice where |lat| > ~55: the edge moves with the season (NH maximum in March), opposite in
    the south, and with longitude; concentration ramps from 0 at the edge to 1 eight degrees
    poleward."""
    lo = np.deg2rad(lon)
    edge_n = 70 - 8 * np.cos(2 * np.pi * (m - 2) / 12) + 3 * np.sin(lo)
    edge_s = -66 - 6 * np.cos(2 * np.pi * (m - 8) / 12) + 2 * np.cos(lo)
    return np.clip(np.maximum((lat - edge_n) / 8, (edge_s - lat) / 8), 0, 1)


def m_ice(lon, lat, m):
    return a_ice(lon, lat, m) * (0.5 + 2.5 * (np.abs(lat) - 50) / 40)


def u_vel(lon, lat, z, m):
    lo, la = np.deg2rad(lon), np.deg2rad(lat)
    return (0.2 * np.cos(la) * np.cos(3 * la) + 0.05 * np.sin(lo)) * np.exp(-z / 1000) * (1 + 0.2 * season(m))


def v_vel(lon, lat, z, m):
    lo, la = np.deg2rad(lon), np.deg2rad(lat)
    return 0.05 * np.sin(2 * lo) * np.cos(la) * np.exp(-z / 1500) + 0.01 * np.sin(la) * season(m, 3)


def fesom_data(tmp, order_node=ORDER_NODE, order_elem=ORDER_ELEM):
    with netCDF4.Dataset(os.path.join(MESH, "fesom.mesh.diag.nc")) as m:
        lon, lat = np.rad2deg(m["nodes"][:])
        tri = m["elem"][:] - 1                      # (3, elem), 1-based in the file
        z = -np.asarray(m["Z"][:])                  # layer mid-depths, positive down
        nlev_n = np.asarray(m["nlevels_nod2D"][:])  # interfaces: wet layers are k < nlevels - 1
        nlev_e = np.asarray(m["nlevels"][:])
        area = np.asarray(m["nod_area"][:])
    nz1 = len(z)
    k = np.arange(nz1)
    # the mesh's own per-level areas are zero exactly below each node's bottom
    assert ((area[:nz1] > 0) == (k[:, None] < nlev_n[None, :] - 1)).all(), "nod_area vs nlevels_nod2D"
    wet_n = k[None, :] < nlev_n[:, None] - 1        # (nod2, nz1)
    wet_e = k[None, :] < nlev_e[:, None] - 1        # (elem, nz1)
    elon, elat = lonlat_elem(lon, lat, tri)
    t = month_ends()

    ds = netCDF4.Dataset(tmp, "w", format="NETCDF4_CLASSIC")
    ds.setncatts({"title": "FESOM2 output", "FESOM_model": "FESOM2", "FESOM_website": "fesom.de",
                  "FESOM_use_partial_cell": 0, "FESOM_autorotate_back_to_geo": 0,
                  "comment": "cdors test fixture: analytic fields on the pi mesh (tests/make_fesom_fixtures.py)"})
    sizes = {"time": None, "nod2": len(lon), "elem": tri.shape[1], "nz1": nz1}
    for d in ("nz1", "nod2", "elem", "time"):
        ds.createDimension(d, sizes[d])
    v = ds.createVariable("nz1", "f8", ("nz1",))
    v.setncatts({"long_name": "depth at layer midpoint", "units": "m", "positive": "down", "axis": "Z"})
    v[:] = z
    v = ds.createVariable("time", "f8", ("time",))
    v.setncatts({"long_name": "time", "standard_name": "time", "units": "seconds since 2000-01-01 0:0:0",
                 "axis": "T", "stored_direction": "increasing"})
    if CALENDAR:
        v.calendar = CALENDAR
    v[:] = t

    def var(name, desc, units, loc, order, fn, x, y, wet):
        """fn(lon, lat[, z], month) -> values on (cells[, nz1]), written in `order`."""
        canon = ("time", "elem" if loc == "elem" else "nod2") + (("nz1",) if wet is not None else ())
        dims = order if wet is not None else canon
        chunks = [1 if d == "time" else -(-sizes[d] // NC_PARTS) if d in ("nod2", "elem") else sizes[d] for d in dims]
        out = ds.createVariable(name, DTYPE, dims, zlib=True, complevel=1, chunksizes=chunks, fill_value=FILL)
        out.setncatts({"description": desc, "long_name": desc, "units": units, "location": loc,
                       "mesh": "fesom_mesh"})
        for m in range(len(t)):
            a = fn(x[:, None], y[:, None], z[None, :], m) if wet is not None else fn(x, y, m)
            if wet is not None:
                a = np.where(wet, a, DRY)
            a = a.astype(DTYPE)[None]
            out[m:m + 1] = np.transpose(a, [canon.index(d) for d in dims])

    var("temp", "temperature", "C", "node", order_node, temp, lon, lat, wet_n)
    var("salt", "salinity", "psu", "node", order_node, salt, lon, lat, wet_n)
    var("a_ice", "ice concentration [0 to 1]", "%",   # FESOM2's own units string for a 0..1 field
        "node", None, a_ice, lon, lat, None)
    var("m_ice", "ice height per unit area", "m", "node", None, m_ice, lon, lat, None)
    var("u", "zonal velocity", "m/s", "elem", order_elem, u_vel, elon, elat, wet_e)
    var("v", "meridional velocity", "m/s", "elem", order_elem, v_vel, elon, elat, wet_e)
    ds.close()


def edges_grid(tmp):
    lon = np.arange(-180.0, 180.0)
    lat = np.arange(-90.0, 91.0)
    with netCDF4.Dataset(tmp, "w", format="NETCDF4") as ds:
        ds.createDimension("time", None)
        ds.createDimension("lat", len(lat))
        ds.createDimension("lon", len(lon))
        v = ds.createVariable("time", "f8", ("time",))
        v.setncatts({"standard_name": "time", "units": "days since 2000-01-01 00:00:00",
                     "calendar": "standard", "axis": "T"})
        v[:] = [14.5, 45.0]
        for name, vals, units, axis in (("lat", lat, "degrees_north", "Y"), ("lon", lon, "degrees_east", "X")):
            v = ds.createVariable(name, "f8", (name,))
            v.setncatts({"standard_name": {"Y": "latitude", "X": "longitude"}[axis], "units": units, "axis": axis})
            v[:] = vals
        v = ds.createVariable("tas", "f4", ("time", "lat", "lon"))
        v.units = "K"
        la, lo = np.deg2rad(np.meshgrid(lat, lon, indexing="ij"))
        v[:] = np.stack([280 + 25 * np.cos(la) + 6 * np.sin(lo + 0.5 * m) * np.cos(la) for m in range(2)])


atomic_dir("fesom_pi", copy_mesh)
atomic_dir("fesom_pi_deg", deg_mesh)
atomic_file("fesom_pi_data.nc", fesom_data)
atomic_file("fesom_pi_data_v20.nc", lambda tmp: fesom_data(tmp, ORDER_NODE_V20, ORDER_ELEM_V20))
atomic_file("r1x1_edges.nc", edges_grid)
