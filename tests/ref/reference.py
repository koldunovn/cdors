"""Non-cdo references for the ref:<lib>.<fn> rows of tests/run_cases.sh (run with the Python of the
environment that <lib> selects, always with -I).

    reference.py run LIB FN OUT IN1 IN2 [ARG ...]   write the reference of one row to OUT
    reference.py compare WANT GOT REL ABS NAME ...  compare variables NAME of two NetCDF files
    reference.py version LIB                        versions of LIB and the modules it loads

run imports LIB first (a missing package exits with 3), then calls FN(args, inputs, out):
- args:   the row's arguments after placeholder expansion ({in}, {fix}, ...), cdors options
          included ({cdors:OPT} unwrapped), without the output file;
- inputs: [IN1, IN2], the row's fixture files ({in} and {in2});
- out:    the NetCDF file to write: the same variable names and time axis (values, units, calendar)
          as the cdors output, so that the harness compares names, values and timestamps.
          Horizontal and level dimensions of size 1 are free (compare squeezes them).
compare passes when, for every value, both are missing (masked or NaN) or
|got - want| <= ABS + REL * |want|.
"""
import importlib
import os
import sys

import netCDF4
import numpy as np

# pyproj (geopandas, regionmask) finds its database only through PROJ_DATA
os.environ.setdefault("PROJ_DATA", os.path.join(sys.prefix, "share", "proj"))


# ---------------------------------------------------------------- helpers
def data_vars(ds):
    """Variables that hold data: not coordinates, bounds, grid mappings or scalars."""
    aux = set()
    for v in ds.variables.values():
        for a in ("bounds", "coordinates", "grid_mapping"):
            aux.update(str(getattr(v, a, "")).split())
    return [n for n, v in ds.variables.items() if n not in ds.dimensions and n not in aux and v.ndim > 0]


def copy_time(src, dst):
    """Unlimited time dimension and the time variable of src, values and attributes unchanged."""
    dst.createDimension("time", None)
    t = src["time"]
    out = dst.createVariable("time", "f8", ("time",))
    out.setncatts({a: t.getncattr(a) for a in t.ncattrs()})
    out[:] = t[:]


def axis_bounds(c, lat):
    """Cell bounds from centres as cdo makes them: midpoints, half a step past the ends,
    latitudes clipped to [-90, 90]."""
    mid = (c[1:] + c[:-1]) / 2
    b = np.concatenate([[c[0] - (mid[0] - c[0])], mid, [c[-1] + (c[-1] - mid[-1])]])
    return np.clip(b, -90, 90) if lat else b


def unit(lon, lat):
    lo, la = np.deg2rad(lon), np.deg2rad(lat)
    return np.stack([np.cos(la) * np.cos(lo), np.cos(la) * np.sin(lo), np.sin(la)], axis=-1)


def triangle_area(a, b, c):
    """Spherical excess of the great-circle triangle abc (unit vectors), Van Oosterom-Strackee."""
    num = np.abs(np.einsum("...i,...i", a, np.cross(b, c)))
    den = 1 + np.einsum("...i,...i", a, b) + np.einsum("...i,...i", b, c) + np.einsum("...i,...i", c, a)
    return 2 * np.arctan2(num, den)


def cell_areas(ds, name):
    """Cell areas (unit sphere) of the horizontal grid of variable `name`, as cdo weights cells:
    lon-lat cells are quadrilaterals with great-circle edges; HEALPix cells are all equal."""
    v = ds[name]
    if getattr(v, "grid_mapping", "") and "healpix" in ds[v.grid_mapping].grid_mapping_name:
        return np.ones(v.shape[-1])
    ylat, xlon = v.dimensions[-2:]
    if not (ylat in ds.variables and xlon in ds.variables
            and "north" in getattr(ds[ylat], "units", "") and "east" in getattr(ds[xlon], "units", "")):
        raise SystemExit(f"reference: no lon-lat grid for {name}")
    lb, yb = axis_bounds(ds[xlon][:].astype("f8"), False), axis_bounds(ds[ylat][:].astype("f8"), True)
    lo, la = np.meshgrid(lb, yb)                     # corner grid (ny+1, nx+1)
    p = unit(lo, la)
    sw, se, ne, nw = p[:-1, :-1], p[:-1, 1:], p[1:, 1:], p[1:, :-1]
    return triangle_area(sw, se, ne) + triangle_area(sw, ne, nw)


def masked(x):
    return np.ma.masked_invalid(np.ma.asarray(x, dtype="f8"))


# ---------------------------------------------------------------- references
def fldmean(args, inputs, out, scale=1.0):
    """Area-weighted field mean of every variable of IN1 (time, [level,] horizontal), weighted by
    the cell areas of cell_areas(); missing cells drop out of the weights."""
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst)
        for n in data_vars(src):
            v = src[n]
            w = cell_areas(src, n)
            nh = w.ndim
            x = masked(v[:])
            ww = np.broadcast_to(w, x.shape)
            axes = tuple(range(x.ndim - nh, x.ndim))
            wsum = np.ma.array(ww, mask=np.ma.getmaskarray(x)).sum(axis=axes)
            mean = (x * ww).sum(axis=axes) / wsum * scale
            dims = v.dimensions[: x.ndim - nh]
            o = dst.createVariable(n, "f8", dims, fill_value=netCDF4.default_fillvals["f8"])
            o[:] = mean


def fldmean_wrong(args, inputs, out):
    """Harness self-test only: fldmean off by a relative 1e-5 (must fail at the default rel=1e-6)."""
    fldmean(args, inputs, out, scale=1 + 1e-5)


REGIONMASK_CACHE = "/work/ab0995/a270088/cdors-cache/regionmask"


def regionmask_set(name):
    """A cdors catalog name as regionmask's own region set (its data, not cdors' export)."""
    import regionmask
    regionmask.set_options(cache_dir=os.environ.get("CDORS_REGIONMASK_CACHE", REGIONMASK_CACHE))
    dr = regionmask.defined_regions
    if name.startswith("ar6."):
        return getattr(dr.ar6, name[4:])
    if name.startswith("ne."):
        return getattr(dr.natural_earth_v5_1_2, name[3:])
    return getattr(dr, name)


def cell_centres(ds, name):
    """Cell centres (degrees, float64) of the grid of variable `name`, and its horizontal
    dimensions: lon-lat axes (2-D grid), unstructured lon/lat, or HEALPix (healpy, nested)."""
    v = ds[name]
    if getattr(v, "grid_mapping", "") and "healpix" in ds[v.grid_mapping].grid_mapping_name:
        import healpy
        gm = ds[v.grid_mapping]
        nest = str(getattr(gm, "healpix_order", "nested")).startswith("nest")
        n = v.shape[-1]
        lon, lat = healpy.pix2ang(int(gm.healpix_nside), np.arange(n), nest=nest, lonlat=True)
        return lon, lat, v.dimensions[-1:]
    last = v.dimensions[-1]
    if last in ds.variables and "east" in getattr(ds[last], "units", ""):   # regular
        ylat = v.dimensions[-2]
        return ds[last][:].astype("f8"), ds[ylat][:].astype("f8"), v.dimensions[-2:]
    co = str(getattr(v, "coordinates", "")).split()
    xn = next(c for c in co if "east" in getattr(ds[c], "units", ""))
    yn = next(c for c in co if "north" in getattr(ds[c], "units", ""))
    return ds[xn][:].astype("f8"), ds[yn][:].astype("f8"), v.dimensions[-1:]


def _mask_3D(args, inputs, opname):
    """regionmask's mask_3D (drop=False) for `<opname>,regions=<set>[,select=...]` on the grid of
    the first data variable of IN1: (mask (region, <horizontal>) bool, horizontal dims)."""
    import xarray as xr
    op = next(a for a in args if a.lstrip("-").startswith(opname + ","))
    parts = op.lstrip("-").split(",")[1:]
    name = next(p.split("=", 1)[1] for p in parts if p.startswith("regions="))
    sel, on = [], False
    for p in parts:
        if p.startswith("select="):
            sel.append(p.split("=", 1)[1])
            on = True
        elif "=" in p:
            on = False
        elif on:
            sel.append(p)
    rs = regionmask_set(name)
    if sel:
        rs = rs[sel]
    with netCDF4.Dataset(inputs[0]) as src:
        var = data_vars(src)[0]
        lon, lat, hdims = cell_centres(src, var)
    if len(hdims) == 2:
        m = rs.mask_3D(np.asarray(lon), np.asarray(lat), drop=False)   # 1-D axes: a lon-lat grid
        m = m.transpose("region", "lat", "lon")
    else:
        m = rs.mask_3D(xr.DataArray(lon, dims="cell"), xr.DataArray(lat, dims="cell"), drop=False)
        m = m.transpose("region", "cell")
    return m.values, hdims


def mask_3D(args, inputs, out):
    """regionmask's mask_3D (drop=False: every region, empty ones all zero) of the grid of the
    first data variable of IN1 for `regmask,regions=<set>[,select=<abbrev>,...]`, written as
    mask(region, <horizontal dims>) int8."""
    m, hdims = _mask_3D(args, inputs, "regmask")
    with netCDF4.Dataset(out, "w") as dst:
        dst.createDimension("region", m.shape[0])
        for d, n in zip(hdims, m.shape[1:]):
            dst.createDimension(d, n)
        o = dst.createVariable("mask", "i1", ("region",) + tuple(hdims))
        o[:] = m.astype("i1")


def maskregion(args, inputs, out):
    """`maskregion,regions=<set>[,select=...]`: every variable of IN1 (time, <horizontal>) missing
    outside the union of regionmask's regions."""
    m, hdims = _mask_3D(args, inputs, "maskregion")
    keep = m.any(axis=0)
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst)
        for d in hdims:
            dst.createDimension(d, len(src.dimensions[d]))
        for n in data_vars(src):
            v = src[n]
            o = dst.createVariable(n, v.dtype, v.dimensions,
                                   fill_value=getattr(v, "_FillValue", None))
            x = masked(v[:])
            o[:] = np.ma.masked_where(np.broadcast_to(~keep, x.shape) | np.ma.getmaskarray(x), x)


def selregion(args, inputs, out):
    """cdo's selregion with every cell once, in ascending order: cdo ($CDO) selects each polygon
    of the region files on its own, from a field that holds the cell index, and the union of the
    selected cells is taken from IN1 (cdo 2.6.0 repeats the cells of earlier polygons for every
    further polygon). Data variables are (time, <horizontal>); the result is (time, ncells).
    cdo's inputs and outputs stay in OUT.d/."""
    import subprocess
    cdo = os.environ.get("CDO", "cdo")
    op = next(a for a in args if a.lstrip("-").startswith("selregion,"))
    polys = []
    for fname in op.lstrip("-").split(",")[1:]:
        block = []
        with open(fname) as f:
            for line in f:
                if line.startswith("&"):
                    polys.append(block)
                    block = []
                else:
                    block.append(line)
        polys.append(block)
    polys = [p for p in polys if any(x.strip() and not x.startswith("#") for x in p)]
    work = out + ".d"
    os.makedirs(work, exist_ok=True)
    with netCDF4.Dataset(inputs[0]) as src:
        names = data_vars(src)
        idx = os.path.join(work, "index.nc")
        subprocess.run([cdo, "-s", "-O", f"-seltimestep,1", f"-selname,{names[0]}", inputs[0], idx], check=True)
        with netCDF4.Dataset(idx, "a") as d:
            v = d[names[0]]
            v[:] = np.arange(v[:].size, dtype="f8").reshape(v.shape)
        cells = set()
        for k, p in enumerate(polys):
            pf, sf = os.path.join(work, f"poly{k}.txt"), os.path.join(work, f"sel{k}.nc")
            with open(pf, "w") as f:
                f.writelines(p)
            subprocess.run([cdo, "-s", "-O", f"selregion,{pf}", idx, sf], check=True)
            with netCDF4.Dataset(sf) as d:
                cells.update(int(round(x)) for x in np.ravel(d[names[0]][:]))
        cells = np.array(sorted(cells))
        with netCDF4.Dataset(out, "w") as dst:
            copy_time(src, dst)
            dst.createDimension("ncells", len(cells))
            for n in names:
                x = src[n][:]
                x = x.reshape(x.shape[0], -1)[:, cells]
                o = dst.createVariable(n, src[n].dtype, ("time", "ncells"),
                                       fill_value=getattr(src[n], "_FillValue", None))
                o[:] = x


# the functions a row can name (ref:<lib>.<fn>)
# 2-D field statistics on a FESOM2 mesh, weighted by the surface row of nod_area (nodes) or
# elem_area (elements).
def mesh_dir(args):
    """The mesh directory of the row's -setgrid,fesom:<dir>[,...] argument."""
    for a in args:
        if a.startswith("-setgrid,fesom:"):
            return a[len("-setgrid,fesom:"):].split(",")[0]
    raise SystemExit("reference: no -setgrid,fesom:<dir> in the arguments")


def mesh_fld(args, inputs, out, integral=False):
    """fldmean (or fldint) of every 2-D (time, nod2|elem) variable of IN1 on the mesh of the row."""
    with netCDF4.Dataset(os.path.join(mesh_dir(args), "fesom.mesh.diag.nc")) as m:
        areas = {m["nod_area"].shape[1]: m["nod_area"][0].astype("f8"),
                 m["elem_area"].shape[0]: m["elem_area"][:].astype("f8")}
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst)
        for n in data_vars(src):
            v = src[n]
            if v.ndim != 2:
                continue
            x = masked(v[:])
            w = np.broadcast_to(areas[v.shape[1]], x.shape)
            s = (x * w).sum(axis=1)
            res = s if integral else s / np.ma.array(w, mask=np.ma.getmaskarray(x)).sum(axis=1)
            o = dst.createVariable(n, "f8", ("time",), fill_value=netCDF4.default_fillvals["f8"])
            o[:] = res


def mesh_fldint(args, inputs, out):
    mesh_fld(args, inputs, out, integral=True)


REFERENCES = {
    "fldmean": fldmean,
    "fldmean_wrong": fldmean_wrong,
    "mesh_fldmean": mesh_fld,
    "mesh_fldint": mesh_fldint,
    "mask_3D": mask_3D,
    "maskregion": maskregion,
    "selregion": selregion,
}


# ---------------------------------------------------------------- commands
def compare(want, got, rel, abs_, names):
    bad = 0
    with netCDF4.Dataset(want) as w, netCDF4.Dataset(got) as g:
        for n in names:
            a, b = masked(w[n][:]).squeeze(), masked(g[n][:]).squeeze()
            if a.shape != b.shape:
                print(f"{n}: shape {b.shape}, reference {a.shape}")
                bad += 1
                continue
            ma, mb = np.ma.getmaskarray(a), np.ma.getmaskarray(b)
            if (ma != mb).any():
                print(f"{n}: {int((ma != mb).sum())} values missing in one file only")
                bad += 1
                continue
            d = np.abs(b.filled(0) - a.filled(0))
            lim = abs_ + rel * np.abs(a.filled(0))
            nbad = int((d > lim).sum())
            r = np.where(a.filled(0) != 0, d / np.abs(np.where(a.filled(0) != 0, a.filled(0), 1)), 0)
            print(f"{n}: {nbad} of {d.size} values beyond abs={abs_:g} + rel={rel:g}*|ref|; "
                  f"max abs diff {d.max(initial=0):.3g}, max rel diff {r.max(initial=0):.3g}")
            bad += nbad > 0
    return 1 if bad else 0


def version(lib):
    """Python and the versions of all modules LIB loads, plus the newest file time of LIB's
    package (editable installs change without a new version)."""
    print(sys.version.split()[0], sys.prefix)
    try:
        m = importlib.import_module(lib)
    except ImportError as e:
        print(f"missing {lib}: {e}")
        return
    for name in sorted(k for k in sys.modules if "." not in k):
        ver = getattr(sys.modules[name], "__version__", None)
        if isinstance(ver, str):
            print(name, ver)
    root = os.path.dirname(getattr(m, "__file__", "") or "")
    newest = max((os.stat(os.path.join(d, f)).st_mtime for d, _, fs in os.walk(root) for f in fs
                  if f.endswith(".py")), default=0)
    print(lib, root, newest)


def main():
    cmd = sys.argv[1]
    if cmd == "run":
        lib, fn, out, in1, in2, *args = sys.argv[2:]
        try:
            importlib.import_module(lib)
        except ImportError as e:
            print(f"reference: package '{lib}' is missing in {sys.prefix}: {e}", file=sys.stderr)
            sys.exit(3)
        f = REFERENCES.get(fn)
        if f is None:
            print(f"reference: no reference function '{fn}' in {__file__}", file=sys.stderr)
            sys.exit(2)
        f(args, [in1, in2], out)
    elif cmd == "compare":
        want, got, rel, abs_, *names = sys.argv[2:]
        sys.exit(compare(want, got, float(rel), float(abs_), names))
    elif cmd == "version":
        version(sys.argv[2])
    else:
        sys.exit(f"reference: unknown command {cmd}")


if __name__ == "__main__":
    main()
