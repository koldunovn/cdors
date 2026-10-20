"""Non-cdo references for the ref:<lib>.<fn> rows of tests/run_cases.sh (run with the Python of the
environment that <lib> selects, always with -I).

    reference.py run LIB FN OUT IN1 IN2 [ARG ...]   write the reference of one row to OUT
    reference.py compare WANT GOT REL ABS NAME ...  compare variables NAME of two NetCDF files
    reference.py compare-many WANT REL ABS LABEL=GOT ... -- NAME ...
                                                    the same for several GOT files in one process;
                                                    a failure ends with "first failure: LABEL"
    reference.py compare-batch < REQUESTS           compare-many for many rows; one request per line,
                                                    tab-separated: ID LOG WANT REL ABS NAMES LABEL=GOT...
                                                    (NAMES space-separated), output appended to LOG;
                                                    prints "ID<TAB><first failing LABEL or empty>"
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

# reference functions by name (`ref:<lib>.<name>` rows): `def f(args, inputs, out)`, registered with
# @reference() under its own name or @reference("<name>")
REFERENCES = {}


def reference(name=None):
    def add(f):
        REFERENCES[name or f.__name__] = f
        return f
    return add


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


def copy_time(src, dst, args=()):
    """Unlimited time dimension and the time variable of src, attributes unchanged; values
    unchanged, or those of the -timmean/-yearmean in args (time_groups)."""
    dst.createDimension("time", None)
    t = src["time"]
    out = dst.createVariable("time", "f8", ("time",))
    out.setncatts({a: t.getncattr(a) for a in t.ncattrs()})
    groups = time_groups(args, t)
    out[:] = t[:] if groups is None else cdo_group_times(args, src, t)


def cdo_group_times(args, src, t):
    """The times cdo stamps the groups of the -timmean/-yearmean in args with (cdo's own date
    averaging, read from `cdo showtimestamp`; $CDO as the harness sets it)."""
    import datetime
    import subprocess
    op = next(a for a in args if a in ("-timmean", "-yearmean"))
    two_d = next(n for n in data_vars(src) if src[n].ndim == 2)
    r = subprocess.run([os.environ.get("CDO", "cdo"), "-s", "showtimestamp", op, f"-selname,{two_d}",
                        src.filepath()], capture_output=True, text=True, check=True)
    dates = [datetime.datetime.fromisoformat(x) for x in r.stdout.split()]
    return netCDF4.date2num(dates, t.units, getattr(t, "calendar", "standard"))


def time_groups(args, t):
    """The timesteps of each group of a -timmean (all) or -yearmean (per year) in args, None
    without."""
    ops = [a for a in args if a in ("-timmean", "-yearmean")]
    if not ops:
        return None
    if ops[0] == "-timmean":
        return [np.arange(len(t))]
    years = np.array([d.year for d in netCDF4.num2date(t[:], t.units, getattr(t, "calendar", "standard"))])
    return [np.flatnonzero(years == y) for y in np.unique(years)]


def under_setgrid(args, x, t):
    """x (time, ...) after what the chain does to it besides selections: -addc/-mulc below the
    first -setgrid (innermost first, cdo's rule: 0 times anything is 0), and a -timmean or
    -yearmean anywhere (they commute with the static dry-level mask)."""
    first = next((i for i, a in enumerate(args) if a.startswith("-setgrid,")), len(args))
    for a in reversed(args[first + 1:]):
        op, _, c = a.lstrip("-").partition(",")
        if op == "mulc":
            x = np.where((float(c) == 0) | (x == 0), 0.0, x * float(c))
        elif op == "addc":
            x = x + float(c)
    groups = time_groups(args, t)
    if groups is not None:
        x = np.stack([x[g].mean(axis=0) for g in groups])
    return x


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


def cell_areas(ds, name, equal_ok=False):
    """Cell areas (unit sphere) of the horizontal grid of variable `name`, as cdo weights cells:
    lon-lat cells are quadrilaterals with great-circle edges; HEALPix cells are all 4 pi / N;
    unstructured cells with bounds are fans of great-circle triangles from their first vertex.
    equal_ok: a grid without coordinates gets equal weights (ones), as cdo's fldmean."""
    v = ds[name]
    if getattr(v, "grid_mapping", "") and "healpix" in ds[v.grid_mapping].grid_mapping_name:
        return np.full(v.shape[-1], 4 * np.pi / v.shape[-1])
    coords = str(getattr(v, "coordinates", "")).split()
    bnds = [str(getattr(ds[c], "bounds", "")) for c in coords if c in ds.variables]
    if len(coords) == 2 and all(b in ds.variables for b in bnds):
        lat, lon = sorted(coords, key=lambda c: "east" in getattr(ds[c], "units", ""))
        p = unit(ds[ds[lon].bounds][:].astype("f8"), ds[ds[lat].bounds][:].astype("f8"))
        return sum(triangle_area(p[:, 0], p[:, k], p[:, k + 1]) for k in range(1, p.shape[1] - 1))
    if equal_ok and not coords and not any(d in ds.variables for d in v.dimensions[1:]):
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
@reference()
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


@reference()
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


def fesom_nodes(args):
    """Node centres (degrees) of the mesh of a `-setgrid,fesom:<dir>[,units=deg|rad]` argument,
    None without one: `nodes` of fesom.mesh.diag.nc, radians if |lat| <= pi/2 and |lon| <= 2 pi
    (as cdors' loader decides)."""
    spec = next((a.split("fesom:", 1)[1] for a in args if "-setgrid,fesom:" in a), None)
    if spec is None:
        return None
    d, *opts = spec.split(",")
    with netCDF4.Dataset(os.path.join(d, "fesom.mesh.diag.nc")) as m:
        nodes = m["nodes"][:].astype("f8")
    lon, lat = (nodes[0], nodes[1]) if nodes.shape[0] == 2 else (nodes[:, 0], nodes[:, 1])
    eps = 1e-6
    rad = np.abs(lat).max() <= np.pi / 2 + eps and np.abs(lon).max() <= 2 * np.pi + eps
    if "units=deg" in opts or "units=rad" in opts:
        rad = "units=rad" in opts
    return (np.degrees(lon), np.degrees(lat)) if rad else (lon, lat)


def cell_centres(ds, name, args=()):
    """Cell centres (degrees, float64) of the grid of variable `name`, and its horizontal
    dimensions: FESOM2 mesh nodes (`-setgrid,fesom:<dir>` in args; the variable is on nodes),
    lon-lat axes (2-D grid), unstructured lon/lat, or HEALPix (healpy, nested)."""
    v = ds[name]
    fes = fesom_nodes(args)
    if fes is not None:
        hd = [d for d, k in zip(v.dimensions, v.shape) if k == len(fes[0])]
        if not hd:
            raise SystemExit(f"reference: {name} is not on the mesh nodes")
        return fes[0], fes[1], hd[:1]
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
        lon, lat, hdims = cell_centres(src, var, args)
    if len(hdims) == 2:
        m = rs.mask_3D(np.asarray(lon), np.asarray(lat), drop=False)   # 1-D axes: a lon-lat grid
        m = m.transpose("region", "lat", "lon")
    else:
        m = rs.mask_3D(xr.DataArray(lon, dims="cell"), xr.DataArray(lat, dims="cell"), drop=False)
        m = m.transpose("region", "cell")
    return m.values, hdims


@reference()
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


@reference()
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


@reference()
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


# 2-D field statistics on a FESOM2 mesh, weighted by the surface row of nod_area (nodes) or
# elem_area (elements).
def mesh_dir(args):
    """The mesh directory of the row's -setgrid,fesom:<dir>[,...] argument."""
    for a in args:
        if a.startswith("-setgrid,fesom:"):
            return a[len("-setgrid,fesom:"):].split(",")[0]
    raise SystemExit("reference: no -setgrid,fesom:<dir> in the arguments")


@reference("mesh_fldmean")
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


@reference()
def mesh_fldint(args, inputs, out):
    mesh_fld(args, inputs, out, integral=True)


R = 6371000.0  # cdo's planet radius (m)


def op_args(args, name):
    """Arguments of operator `name` in the row's arguments ('-fldhist,a=1,b=2' -> {a: 1, b: 2},
    a positional argument under its index) and the variables of a -selname in the chain."""
    opts, names = None, None
    for a in args:
        head, _, rest = a.lstrip("-").partition(",")
        if head == name:
            opts = {}
            for i, x in enumerate(rest.split(",") if rest else []):
                k, eq, val = x.partition("=")
                opts[k if eq else i] = val if eq else x
        elif head == "selname":
            names = rest.split(",")
    return opts, names


@reference()
def fldhist(args, inputs, out):
    """Area-weighted histogram of every variable (or the -selname ones): <var>(time, [level,] bin)
    with [e_i, e_i+1), the last bin closed, <var>_below and <var>_above; out=area in m2 (areas of
    cell_areas() x R^2), out=fraction divided by the weight of all valid cells (equal weights on a
    grid without coordinates, as fldmean)."""
    opts, names = op_args(args, "fldhist")
    if "bins" in opts:
        lo, hi, step = map(float, opts["bins"].split("/"))
        n = int(round((hi - lo) / step))
        edges = np.append(lo + step * np.arange(n), hi)
    else:
        edges = np.array([float(x) for x in opts["edges"].split("/")])
    frac = opts.get("out", "area") == "fraction"
    nb = len(edges) - 1
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst)
        dst.createDimension("bin", nb)
        for n in names or data_vars(src):
            v = src[n]
            w = cell_areas(src, n, equal_ok=frac) * (1 if frac else R * R)
            x = masked(v[:])
            lead = x.shape[: x.ndim - w.ndim]
            xf = x.reshape(-1, w.size)
            res = np.zeros((xf.shape[0], nb + 2))     # below, bins, above
            for r, row in enumerate(xf):
                ok = ~np.ma.getmaskarray(row)
                val, ww = row.data[ok], w.reshape(-1)[ok]
                slot = np.where(val < edges[0], 0, np.where(
                    val > edges[-1], nb + 1, np.minimum(np.searchsorted(edges, val, side="right"), nb)))
                res[r] = np.bincount(slot, weights=ww, minlength=nb + 2)
                if frac:
                    res[r] = res[r] / ww.sum() if ww.sum() > 0 else np.nan
            res = res.reshape(*lead, nb + 2)
            dims = v.dimensions[: len(lead)]
            for d in dims[1:]:
                if d not in dst.dimensions:
                    dst.createDimension(d, src.dimensions[d].size)
            for suffix, vals, dd in [("", res[..., 1:nb + 1], dims + ("bin",)),
                                     ("_below", res[..., 0], dims), ("_above", res[..., nb + 1], dims)]:
                o = dst.createVariable(n + suffix, "f8", dd, fill_value=netCDF4.default_fillvals["f8"])
                o[:] = np.ma.masked_invalid(vals)


def zonal_bands(args, name):
    """Band bounds (degrees) of the row's <name>,zonal_<DY> as cdo makes them."""
    opts, _ = op_args(args, name)
    inc = float(opts[0].split("zonal_")[1])
    n = int(180 / inc + 0.5)
    c = -90 + inc / 2 + inc * np.arange(n)
    b = axis_bounds(c, False) if n > 1 else np.array([-90.0, 90.0])
    return np.stack([b[:-1], b[1:]], axis=1)


def zonal_ref(args, inputs, out, name):
    """zonsum / zonint with zonal_<DY> on a grid of lon-lat cells stored as unstructured bounds
    (unst_360): zonsum sums the cells whose centre latitude is in [lo, hi); zonint sums value x
    the cell's area inside the band, Delta lon (sin(min(top, hi)) - sin(max(bottom, lo))) R^2. A
    band without valid cells is missing."""
    bands = zonal_bands(args, name)
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst)
        dst.createDimension("lat", len(bands))
        dst.createDimension("lon", 1)
        for n in data_vars(src):
            v = src[n]
            lat, lon = "lat", "lon"
            yb = np.deg2rad(src[src[lat].bounds][:].astype("f8"))
            xb = np.deg2rad(src[src[lon].bounds][:].astype("f8"))
            yc = src[lat][:].astype("f8")
            x = masked(v[:])
            w = np.zeros((len(bands), v.shape[-1]))
            for b, (lo, hi) in enumerate(bands):
                if name == "zonsum":
                    w[b] = (yc >= lo) & (yc < hi)
                else:
                    s0, s1 = np.sin(np.deg2rad(lo)), np.sin(np.deg2rad(hi))
                    top = np.clip(np.sin(yb.max(axis=1)), s0, s1)
                    bot = np.clip(np.sin(yb.min(axis=1)), s0, s1)
                    dlon = np.abs((xb[:, 1] - xb[:, 0] + np.pi) % (2 * np.pi) - np.pi)
                    w[b] = dlon * (top - bot) * R * R
            valid = (~np.ma.getmaskarray(x)).astype("f8")
            s = np.einsum("tc,bc->tb", x.filled(0), w)
            cnt = np.einsum("tc,bc->tb", valid, (w > 0).astype("f8"))
            o = dst.createVariable(n, "f8", ("time", "lat", "lon"), fill_value=netCDF4.default_fillvals["f8"])
            o[:] = np.ma.masked_where(cnt == 0, s)[..., None]


@reference()
def zonsum(args, inputs, out):
    zonal_ref(args, inputs, out, "zonsum")


@reference()
def zonint(args, inputs, out):
    zonal_ref(args, inputs, out, "zonint")


# ---------------------------------------------------------------- sampling (selpoints, transect)
# cdors' documented sampling (crates/cdors-mesh/src/sample.rs), written independently in NumPy:
# great-circle paths cut into equal arcs per segment, the default step, nn by a KD-tree on unit
# vectors (nereus' transect: scipy cKDTree on lonlat_to_cartesian), linear by brute force over all
# mesh triangles, levels valid only where every cell of a point's row is wet.
R_KM = 6371.0


def wrap_lon(x):
    w = np.mod(x, 360.0)
    return np.where(w > 180.0, w - 360.0, w)


def sample_path(wp, step_km):
    """(distance km, lon, lat) of the path through the waypoints wp [(lon, lat), ...]."""
    dist, lon, lat, cum = [0.0], [float(wrap_lon(wp[0][0]))], [wp[0][1]], 0.0
    for (a, b) in zip(wp[:-1], wp[1:]):
        u, v = unit(*a), unit(*b)
        d = np.arctan2(np.linalg.norm(np.cross(u, v)), u @ v)
        if d == 0:
            continue
        n = max(int(np.ceil(d / (step_km / R_KM))), 1)
        for k in range(1, n + 1):
            if k == n:
                x, y = b
            else:
                f = k / n
                p = (np.sin((1 - f) * d) * u + np.sin(f * d) * v) / np.sin(d)
                x, y = np.rad2deg(np.arctan2(p[1], p[0])), np.rad2deg(np.arctan2(p[2], np.hypot(p[0], p[1])))
            dist.append((cum + d * k / n) * R_KM)
            lon.append(float(wrap_lon(x)))
            lat.append(y)
        cum += d
    return np.array(dist), np.array(lon), np.array(lat)


def nearest(clon, clat, lon, lat, k=1):
    """Indices (and squared chords) of the k cells nearest to each point (scipy cKDTree)."""
    from scipy.spatial import cKDTree
    tree = cKDTree(unit(clon, clat))
    d, i = tree.query(unit(np.asarray(lon), np.asarray(lat)), k=k)
    return i, d * d


def default_step(clon, clat, wp):
    """Median cell spacing along the path: the distinct cells nearest to 1024 equal arcs of the
    path, each cell's distance to its nearest other cell, the lower median (km)."""
    length = sample_path(wp, 1e9)[0][-1]
    _, lon, lat = sample_path(wp, length / 1024)
    cells = np.unique(nearest(clon, clat, lon, lat)[0])
    i, d2 = nearest(clon, clat, clon[cells], clat[cells], k=2)
    d = np.sort(2 * np.arcsin(np.minimum(0.5 * np.sqrt(d2[:, 1]), 1)) * R_KM)
    return d[(len(d) - 1) // 2]


def linear_rows(clon, clat, tri, lon, lat):
    """Barycentric rows [(cells, weights)] of each point in the first mesh triangle (ascending
    index) containing it, by brute force; weights <= 1e-12 dropped and renormalised; [] outside."""
    p = unit(clon, clat)
    a, b, c = p[tri[:, 0]], p[tri[:, 1]], p[tri[:, 2]]
    bc, ca, ab = np.cross(b, c), np.cross(c, a), np.cross(a, b)
    det = np.einsum("ij,ij->i", a, bc)
    rows = []
    for q in unit(np.asarray(lon), np.asarray(lat)):
        m = np.stack([bc @ q, ca @ q, ab @ q], axis=1)
        s = m.sum(axis=1)
        w = m / s[:, None]
        ok = np.nonzero((s * det > 0) & (w >= -1e-12).all(axis=1))[0]
        if len(ok) == 0:
            rows.append(([], []))
            continue
        e = ok[0]
        keep = w[e] > 1e-12
        cells, ww = tri[e][keep], w[e][keep] / w[e][keep].sum()
        o = np.argsort(cells)
        rows.append((list(cells[o]), list(ww[o])))
    return rows


def fesom_mesh(mdir):
    """Places of a FESOM2 mesh.diag: {size: (name, lon, lat, top, bottom)} and the triangles of
    the nodes, zbar; nodes in radians or degrees as the cdors loader decides."""
    with netCDF4.Dataset(os.path.join(mdir, "fesom.mesh.diag.nc")) as m:
        nodes = m["nodes"][:].astype("f8")
        if nodes.shape[0] != 2:
            nodes = nodes.T
        x, y = nodes
        if np.abs(y).max() <= np.pi / 2 + 1e-6 and np.abs(x).max() <= 2 * np.pi + 1e-6:
            x, y = np.rad2deg(x), np.rad2deg(y)
        ev = next(n for n in ("elem", "elements", "face_nodes") if n in m.variables and m[n].ndim == 2)
        el = m[ev][:].astype("i8")
        if el.shape[0] == 3 and el.shape[1] != 3:
            el = el.T
        el = el - int(getattr(m[ev], "start_index", 1))
        zbar = m["zbar"][:].astype("f8") if "zbar" in m.variables else m["nz"][:].astype("f8")
        nl = {k: m[k][:].astype("i8") for k in ("nlevels_nod2D", "nlevels")}
        ul = {k: (m[k][:].astype("i8") if k in m.variables else None) for k in ("ulevels_nod2D", "ulevels")}
    e = unit(x[el], y[el]).sum(axis=1)
    ex, ey = np.rad2deg(np.arctan2(e[:, 1], e[:, 0])), np.rad2deg(np.arctan2(e[:, 2], np.hypot(e[:, 0], e[:, 1])))
    top = lambda u, n: (u - 1) if u is not None else np.zeros_like(n)
    places = {len(x): ("nodes", x, y, top(ul["ulevels_nod2D"], nl["nlevels_nod2D"]), nl["nlevels_nod2D"] - 1),
              len(ex): ("elements", ex, ey, top(ul["ulevels"], nl["nlevels"]), nl["nlevels"] - 1)}
    return places, el, zbar


def sample_rows(method, place, tri, lon, lat):
    """Rows [(cells, weights)] of the points on one place: linear in the mesh triangles of the
    nodes; the elements have no triangles of their own and are sampled by nn (cdors'
    linear_as_nn)."""
    _, clon, clat, _, _ = place
    if method == "linear" and place[0] == "nodes":
        return linear_rows(clon, clat, tri, lon, lat)
    i = nearest(clon, clat, lon, lat)[0]
    return [([int(c)], [1.0]) for c in i]


def write_sampled(src, dst, dim, rows_of, coords, wet_of=None, zbar=None, extra=None, first=None,
                  names=None):
    """Every data variable of src (or `names`) with a horizontal dimension sampled by its rows:
    the horizontal dimension replaced by `dim` in place (the input's order), values sum w * x,
    missing where any cell of the row is missing or dry at that level (wet_of[size] = (top, bottom) per cell); bottom_depth for
    the place of size `first` (nodes), bottom_depth_<dim> for the others."""
    copy_time(src, dst)
    n = len(coords["lon"])
    dst.createDimension(dim, n)
    if dim == "distance":
        o = dst.createVariable("distance", "f8", ("distance",))
        o.units, o.long_name = "km", "distance along the path"
        o[:] = coords["distance"]
    for c, units in (("lon", "degrees_east"), ("lat", "degrees_north")):
        o = dst.createVariable(c, "f8", (dim,))
        o.units = units
        o[:] = coords[c]
    for k, v in (extra or {}).items():
        v(dst)
    done = set()
    for name in names or data_vars(src):
        v = src[name]
        hd = next((d for d in v.dimensions if src.dimensions[d].size in rows_of), None)
        if hd is None:
            continue
        size = src.dimensions[hd].size
        rows = rows_of[size]
        ax = v.dimensions.index(hd)
        x = np.moveaxis(masked(v[:]), ax, -1)
        lev = next((i for i, d in enumerate(v.dimensions) if d not in ("time", hd)), None)
        out = np.ma.masked_all(x.shape[:-1] + (n,))
        for r, (cells, w) in enumerate(rows):
            if not cells:
                continue
            val = (x[..., cells] * np.array(w)).sum(axis=-1)
            bad = np.ma.getmaskarray(x[..., cells]).any(axis=-1)
            if wet_of is not None and lev is not None:
                top, bot = wet_of[size]
                t, b = max(top[c] for c in cells), min(bot[c] for c in cells)
                k = np.arange(src.dimensions[v.dimensions[lev]].size)
                dry = (k < t) | (k >= b)
                shape = [1] * (x.ndim - 1)
                shape[lev - (1 if lev > ax else 0)] = len(k)
                bad = bad | dry.reshape(shape)
            out[..., r] = np.ma.masked_where(bad, val)
        for d in v.dimensions:
            if d != hd and d not in dst.dimensions:
                dst.createDimension(d, src.dimensions[d].size)
                if d in src.variables:      # the level coordinate (cdo needs it to see a z-axis)
                    c = dst.createVariable(d, src[d].dtype, (d,))
                    c.setncatts({a: src[d].getncattr(a) for a in src[d].ncattrs()})
                    c[:] = src[d][:]
        dims = tuple(dim if d == hd else d for d in v.dimensions)
        o = dst.createVariable(name, "f8", dims, fill_value=netCDF4.default_fillvals["f8"])
        o.coordinates = "lat lon"
        o[:] = np.moveaxis(out, -1, ax)
        if wet_of is not None and size not in done:
            done.add(size)
            top, bot = wet_of[size]
            dep = [abs(zbar[min(bot[c] for c in cells)]) if cells and max(top[c] for c in cells) < min(bot[c] for c in cells)
                   else np.nan for cells, _ in rows]
            bn = "bottom_depth" if size == first else f"bottom_depth_{hd}"
            o = dst.createVariable(bn, "f8", (dim,), fill_value=netCDF4.default_fillvals["f8"])
            o.units, o.standard_name, o.coordinates = "m", "sea_floor_depth_below_sea_surface", "lat lon"
            o[:] = np.ma.masked_invalid(dep)


@reference()
def transect(args, inputs, out):
    """transect,lon0,lat0,lon1,lat1[,...][,step=<km>][,method=nn|linear] or file=<geojson> on a
    FESOM2 mesh (-setgrid,fesom:<dir>): every node and element variable of IN1 along the path."""
    opts, _ = op_args(args, "transect")
    method = opts.get("method", "nn")
    if "file" in opts:
        import json
        with open(opts["file"]) as f:
            g = json.load(f)
        if g["type"] == "FeatureCollection":
            g = g["features"][0]
        g = g.get("geometry", g)
        wp = [tuple(p[:2]) for p in g["coordinates"]]
    else:
        nums = [float(opts[i]) for i in sorted(k for k in opts if isinstance(k, int))]
        wp = list(zip(nums[0::2], nums[1::2]))
    places, tri, zbar = fesom_mesh(mesh_dir(args))
    nodes = next(p for p in places.values() if p[0] == "nodes")
    step = float(opts["step"]) if "step" in opts else default_step(nodes[1], nodes[2], wp)
    dist, lon, lat = sample_path(wp, step)
    rows_of = {s: sample_rows(method, p, tri, lon, lat) for s, p in places.items()}
    wet_of = {s: (p[3], p[4]) for s, p in places.items()}
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        write_sampled(src, dst, "distance", rows_of, {"distance": dist, "lon": lon, "lat": lat},
                      wet_of, zbar, first=len(nodes[1]), names=op_args(args, "transect")[1])


def stations(args, opname):
    """Stations of <opname>,file=<csv>|lon=<x>_lat=<y>[,...]: [(name, lon, lat)], method."""
    import re
    op = next(a for a in args if a.lstrip("-").startswith(opname + ","))
    st, method = [], "nn"
    for t in op.lstrip("-").split(",")[1:]:
        if t.startswith("method="):
            method = t[7:]
        elif t.startswith("file="):
            first = True
            with open(t[5:]) as f:
                for line in f:
                    line = line.strip()
                    if not line or line.startswith("#"):
                        continue
                    n, x, y = [s.strip() for s in line.split(",")]
                    try:
                        st.append((n, float(x), float(y)))
                    except ValueError:
                        if not first:
                            raise
                    first = False
        else:
            m = re.fullmatch(r"lon=([-+0-9.eE]+)[_/x]lat=([-+0-9.eE]+)", t.lower())
            st.append((t, float(m.group(1)), float(m.group(2))))
    return st, method


@reference()
def selpoints(args, inputs, out):
    """selpoints on a grid (cell centres of cell_centres(); nn by brute force, ties to the smallest
    index) or a FESOM2 mesh (-setgrid,fesom:<dir>; nn or linear): (time, [level,] station)."""
    st, method = stations(args, "selpoints")
    lon, lat = np.array([s[1] for s in st]), np.array([s[2] for s in st])
    names = np.array([s[0] for s in st], dtype=object)

    def name_var(dst):
        o = dst.createVariable("station_name", str, ("station",))
        o[:] = names
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        if any(a.startswith("-setgrid,fesom:") for a in args):
            places, tri, zbar = fesom_mesh(mesh_dir(args))
            rows_of = {s: sample_rows(method, p, tri, lon, lat) for s, p in places.items()}
            wet_of = {s: (p[3], p[4]) for s, p in places.items()}
            first = next(s for s, p in places.items() if p[0] == "nodes")
        else:
            clon, clat, hdims = cell_centres(src, data_vars(src)[0])
            if len(hdims) == 2:
                raise SystemExit("reference: selpoints on lon-lat grids: use ref:numpy.selpoints_cdo")
            q, p = unit(lon, lat), unit(clon, clat)
            d = ((p[None, :, :] - q[:, None, :]) ** 2).sum(axis=2)
            rows = [([int(c)], [1.0]) for c in d.argmin(axis=1)]
            rows_of, wet_of, zbar, first = {len(clon): rows}, None, None, None
        write_sampled(src, dst, "station", rows_of, {"lon": lon, "lat": lat}, wet_of, zbar,
                      {"station_name": name_var}, first, op_args(args, "selpoints")[1])


@reference()
def sampled_vert(args, inputs, out):
    """vertint[,<z0>/<z1>] or vertmean of transect or selpoints on a FESOM2 mesh, in either order
    (-vert* -transect ... or -transect ... -vert* ...; with nn the same numbers): the sampled
    columns of transect()/selpoints(), each level weighted by its layer of the mesh's zbar (cut to
    the depth range), missing levels left out; a point without valid levels is missing. Variables
    without levels are left out (rows select those with levels). A -seltimestep,<a>/<b> in the
    chain (keeping the planner variant's intermediate small) selects those steps."""
    import tempfile
    steps = None
    for a in args:
        if a.startswith("-seltimestep,"):
            lo, _, hi = a.split(",", 1)[1].partition("/")
            steps = np.arange(int(lo) - 1, int(hi or lo))
    vop = next(a.lstrip("-") for a in args if a.lstrip("-").split(",")[0] in ("vertint", "vertmean"))
    name, _, rng = vop.partition(",")
    sampler = transect if any(a.lstrip("-").startswith("transect,") for a in args) else selpoints
    with netCDF4.Dataset(os.path.join(mesh_dir(args), "fesom.mesh.diag.nc")) as m:
        zbar = np.abs(m["zbar"][:] if "zbar" in m.variables else m["nz"][:]).astype("f8")
    top, bot = zbar[:-1], zbar[1:]
    if rng:
        z0, z1 = (abs(float(x)) for x in rng.split("/"))
        dz = np.clip(np.minimum(bot, z1) - np.maximum(top, z0), 0, None)
    else:
        dz = bot - top
    with tempfile.TemporaryDirectory() as tmp:
        mid = os.path.join(tmp, "sampled.nc")
        sampler(args, inputs, mid)
        with netCDF4.Dataset(mid) as src, netCDF4.Dataset(out, "w") as dst:
            tsel = steps if steps is not None else np.arange(src.dimensions["time"].size)
            dst.createDimension("time", None)
            t = dst.createVariable("time", "f8", ("time",))
            t.setncatts({a: src["time"].getncattr(a) for a in src["time"].ncattrs()})
            t[:] = src["time"][tsel]
            for d in ("distance", "station"):
                if d in src.dimensions:
                    dst.createDimension(d, src.dimensions[d].size)
            for n, v in src.variables.items():
                if n.startswith("bottom_depth"):
                    o = dst.createVariable(n, "f8", v.dimensions, fill_value=netCDF4.default_fillvals["f8"])
                    o.coordinates = "lat lon"
                    o[:] = v[:]
                elif n in ("lon", "lat", "distance"):
                    o = dst.createVariable(n, "f8", v.dimensions)
                    o.setncatts({a: v.getncattr(a) for a in v.ncattrs()})
                    o[:] = v[:]
            for n in data_vars(src):
                v = src[n]
                if "nz1" not in v.dimensions:
                    continue
                x = np.moveaxis(masked(v[:])[tsel], v.dimensions.index("nz1"), -1)
                ok = ~np.ma.getmaskarray(x)
                s = (x.filled(0) * dz).sum(axis=-1)
                w = (ok * dz).sum(axis=-1)
                res = s if name == "vertint" else s / np.where(w > 0, w, 1)
                dims = tuple(d for d in v.dimensions if d != "nz1")
                o = dst.createVariable(n, "f8", dims, fill_value=netCDF4.default_fillvals["f8"])
                o.coordinates = "lat lon"
                o[:] = np.ma.masked_where(~ok.any(axis=-1), res)


@reference()
def selpoints_cdo(args, inputs, out):
    """cdo's remapnn,lon=<x>_lat=<y> ($CDO) for each station of the row's selpoints, the station
    replacing cdo's 1 x 1 grid: (time, [level,] station). cdo's outputs stay in OUT.d/."""
    import subprocess
    cdo = os.environ.get("CDO", "cdo")
    st, _ = stations(args, "selpoints")
    work = out + ".d"
    os.makedirs(work, exist_ok=True)
    i = next(k for k, a in enumerate(args) if a.lstrip("-").startswith("selpoints,"))
    parts = []
    for k, (n, x, y) in enumerate(st):
        f = os.path.join(work, f"st{k}.nc")
        cmd = [a for a in args[:i] if not a.startswith("--")] + [f"-remapnn,lon={x!r}_lat={y!r}"] + args[i + 1:]
        subprocess.run([cdo, "-s", "-O"] + cmd + [f], check=True)
        parts.append(f)
    with netCDF4.Dataset(parts[0]) as p0, netCDF4.Dataset(out, "w") as dst:
        copy_time(p0, dst)
        dst.createDimension("station", len(st))
        for c, units, j in (("lon", "degrees_east", 1), ("lat", "degrees_north", 2)):
            o = dst.createVariable(c, "f8", ("station",))
            o.units = units
            o[:] = [s[j] for s in st]
        o = dst.createVariable("station_name", str, ("station",))
        o[:] = np.array([s[0] for s in st], dtype=object)
        for name in data_vars(p0):
            if name in ("lon", "lat"):
                continue
            v = p0[name]
            lead = [d for d in v.dimensions if d not in ("lat", "lon")]
            for d in lead[1:]:
                if d not in dst.dimensions:
                    dst.createDimension(d, p0.dimensions[d].size)
            vals = []
            for f in parts:
                with netCDF4.Dataset(f) as p:
                    vals.append(masked(p[name][:]).reshape(p[name].shape[: len(lead)]))
            o = dst.createVariable(name, "f8", tuple(lead) + ("station",),
                                   fill_value=netCDF4.default_fillvals["f8"])
            o.coordinates = "lat lon"
            o[:] = np.ma.stack(vals, axis=-1)


# Field statistics per level and node <-> element mapping on a FESOM2 mesh, in both dimension
# orders: (time, nz1, nod2|elem) and FESOM 2.0's (time, nod2|elem, nz1). Levels outside a cell's
# wet range (nlevels - 1 layers from the top) are missing whatever the file stores (0).
def read_mesh(args):
    """Per-level node areas (nl, nod), element areas, elem -> nodes (ne, 3, 0-based), the end
    (number of wet layers from the surface) and the first wet layer (0-based; 0 without
    ulevels, i.e. without cavities) of nodes and elements, and the interface depths |zbar|."""
    with netCDF4.Dataset(os.path.join(mesh_dir(args), "fesom.mesh.diag.nc")) as m:
        el = m["elem" if "elem" in m.variables else "elements"][:].astype("i8")
        el = el.T if el.shape[0] == 3 else el
        top = {k: (m[v][:].astype("i8") - 1 if v in m.variables else np.zeros(len(m[n][:]), "i8"))
               for k, v, n in (("top_nod", "ulevels_nod2D", "nlevels_nod2D"), ("top_elem", "ulevels", "nlevels"))}
        return {"nod_area": m["nod_area"][:].astype("f8"), "elem_area": m["elem_area"][:].astype("f8"),
                "elem": el - el.min(), "wet_nod": m["nlevels_nod2D"][:].astype("i8") - 1,
                "wet_elem": m["nlevels"][:].astype("i8") - 1,
                "zbar": np.abs(m["zbar" if "zbar" in m.variables else "nz"][:].astype("f8")), **top}


def selected(args, names):
    """The variables of -selname,<a>,<b>,... in the arguments (all if there is none)."""
    for a in args:
        if a.startswith("-selname,"):
            return [n for n in a.split(",")[1:] if n in names]
    return names


def to_tlc(v, args):
    """Values of variable v as (time, level, cell), the levels kept by -sellevel,<values> (by
    the values of nz1; all without), and whether its levels come last (FESOM 2.0); 2-D variables
    get one level."""
    x = under_setgrid(args, np.ma.filled(masked(v[:]), np.nan), v.group()["time"])
    if v.ndim == 2:
        return x[:, None, :], np.arange(1), False
    last = v.dimensions[2].startswith("nz")
    x = np.swapaxes(x, 1, 2) if last else x
    lev = np.arange(x.shape[1])
    zdim = v.dimensions[2] if last else v.dimensions[1]
    for a in args:
        if a.startswith("-sellevel,"):
            want = [float(z) for z in a.split(",")[1:]]
            lev = np.array([k for k, z in enumerate(v.group()[zdim][:]) if z in want])
    return x[:, lev, :], lev, last


def wet(mesh, ncell, lev, surface, interfaces=False):
    """(levels, ncell) wet mask at the mesh levels lev (layers, or interfaces: one more at the
    bottom); 2-D variables are wet everywhere."""
    nodes = ncell == len(mesh["wet_nod"])
    w = (mesh["wet_nod"] if nodes else mesh["wet_elem"]) + (1 if interfaces else 0)
    t = mesh["top_nod"] if nodes else mesh["top_elem"]
    return np.ones((1, ncell), bool) if surface else (lev[:, None] < w[None, :]) & (lev[:, None] >= t[None, :])


def is_interfaces(v):
    """Whether v's levels are the interfaces (FESOM2 dimension nz)."""
    return "nz" in v.dimensions


def mesh_field(args, v, mesh):
    """Values (time, level, cell) of v as the operator after the chain sees them: to_tlc, dry
    levels missing, and an -intlevel,<depths> applied (intlevel_values); with the mesh level of
    each level, whose areas weigh it (after intlevel: the layer that contains the depth)."""
    x, lev, last = to_tlc(v, args)
    nc = x.shape[2]
    x = np.where(wet(mesh, nc, lev, v.ndim == 2, is_interfaces(v))[None], x, np.nan)
    il = next((a for a in args if a.startswith("-intlevel,")), None)
    if il is not None and v.ndim == 3:
        t = [float(z) for z in il.split(",")[1:]]
        x = intlevel_values(x, lev, t, mesh, nc, np.abs(v.group()["nz1"][:].astype("f8")))
        z = mesh["zbar"]
        lev = np.array([min(np.searchsorted(z, d, side="right") - 1, len(z) - 2) for d in t])
        x = np.where(wet(mesh, nc, lev, False)[None], x, np.nan)
    return x, lev, last


def mesh_level_stat(args, inputs, out, stat):
    """fld<stat> per level: nodes weigh nod_area[k] at layer k, elements elem_area; dry levels
    excluded; mean, int (sum of area times value), min, std (population, weighted); nereus: the
    mean of nereus.surface_mean per level with these areas."""
    mesh = read_mesh(args)
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst, args)
        for n in selected(args, data_vars(src)):
            v = src[n]
            x, lev, _ = mesh_field(args, v, mesh)
            nt, nl, nc = x.shape
            nodes = nc == mesh["nod_area"].shape[1]
            a = mesh["nod_area"][lev] if nodes else np.broadcast_to(mesh["elem_area"], (nl, nc))
            ok = ~np.isnan(x)
            w = np.where(ok, a[None], 0.0)
            xs = np.where(ok, x, 0.0)
            with np.errstate(invalid="ignore", divide="ignore"):
                mean = (w * xs).sum(axis=2) / w.sum(axis=2)
                res = {"mean": mean, "int": (w * xs).sum(axis=2),
                       "min": np.where(ok.any(axis=2), np.nanmin(np.where(ok, x, np.inf), axis=2), np.nan),
                       "std": np.sqrt((w * (xs - mean[..., None]) ** 2).sum(axis=2) / w.sum(axis=2))
                       }.get(stat)
            if stat == "nereus":
                import nereus
                res = np.stack([nereus.surface_mean(x[:, k, :], a[k]) for k in range(nl)], axis=1)
            res = np.where(ok.any(axis=2), res, np.nan)
            if v.ndim == 2:
                o = dst.createVariable(n, "f8", ("time",), fill_value=netCDF4.default_fillvals["f8"])
                o[:] = np.ma.masked_invalid(res[:, 0])
            else:
                if "lev" not in dst.dimensions:
                    dst.createDimension("lev", nl)
                o = dst.createVariable(n, "f8", ("time", "lev"), fill_value=netCDF4.default_fillvals["f8"])
                o[:] = np.ma.masked_invalid(res)


def mesh_place_map(args, inputs, out, to):
    """toelems (mean of the three nodes) or tonodes (mean of the elements around a node weighted
    by elem_area), over the valid values only; dry levels of the target place missing. Written
    in the input's dimension order."""
    mesh = read_mesh(args)
    el, ea = mesh["elem"], mesh["elem_area"]
    nn, ne = mesh["nod_area"].shape[1], len(ea)
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst, args)
        for n in selected(args, data_vars(src)):
            v = src[n]
            x, lev, last = to_tlc(v, args)
            nt, nl, nc = x.shape
            x = np.where(wet(mesh, nc, lev, v.ndim == 2)[None], x, np.nan)
            ok = ~np.isnan(x)
            xs = np.where(ok, x, 0.0)
            if to == "elems":
                num = xs[..., el].sum(axis=3)
                den = ok[..., el].sum(axis=3).astype("f8")
                ncell, dim = ne, "elem"
            else:
                num, den = np.zeros((nt, nl, nn)), np.zeros((nt, nl, nn))
                for k in range(3):
                    np.add.at(num, (slice(None), slice(None), el[:, k]), xs * ea)
                    np.add.at(den, (slice(None), slice(None), el[:, k]), ok * ea)
                ncell, dim = nn, "nod2"
            with np.errstate(invalid="ignore", divide="ignore"):
                y = np.where(den > 0, num / den, np.nan)
            y = np.where(wet(mesh, ncell, lev, v.ndim == 2)[None], y, np.nan)
            if dim not in dst.dimensions:
                dst.createDimension(dim, ncell)
            if v.ndim == 2:
                dims, y = ("time", dim), y[:, 0]
            else:
                if "nz1" not in dst.dimensions:
                    dst.createDimension("nz1", nl)
                dims = ("time", dim, "nz1") if last else ("time", "nz1", dim)
                y = np.swapaxes(y, 1, 2) if last else y
            o = dst.createVariable(n, "f8", dims, fill_value=netCDF4.default_fillvals["f8"])
            o[:] = np.ma.masked_invalid(y)



# Vertical geometry on a FESOM2 mesh (Task 9): full layers between the interfaces (depth |zbar|),
# cut at the depth range z0/z1 of the operator (the part of each layer inside it), dry levels
# (below each cell's bottom and above its top) left out.
def op_range(args, names):
    """(operator, (z0, z1) or None) of the first of the operators `names` in the arguments."""
    for a in args:
        head, _, rest = a.lstrip("-").partition(",")
        if head in names:
            return head, (tuple(float(x) for x in rest.split("/")) if rest else None)
    raise SystemExit(f"reference: none of {names} in the arguments")


def layer_dz(mesh, lev, rng):
    """Thickness of the layers lev inside the depth range rng (all of them without)."""
    top, bot = mesh["zbar"][lev], mesh["zbar"][lev + 1]
    if rng is None:
        return bot - top
    return np.clip(np.minimum(bot, rng[1]) - np.maximum(top, rng[0]), 0, None)


def outer_mulc(args):
    """The constant of a -mulc,<c> applied last (the first argument), 1 without."""
    return float(args[0].split(",")[1]) if args and args[0].startswith("-mulc,") else 1.0


def mesh_columns(args, inputs):
    """Per 3-D variable of the row: name, values (time, level, cell) with the dry levels
    missing (and the nodes outside the regions of a -maskregion,regions=<set>, regionmask's
    mask: rows ref:regionmask.*), levels, areas per level (level, cell), and the mesh."""
    mesh = read_mesh(args)
    keep = None
    if any(a.startswith("-maskregion,") for a in args):
        keep = _mask_3D(args, inputs, "maskregion")[0].any(axis=0)
    with netCDF4.Dataset(inputs[0]) as src:
        for n in selected(args, data_vars(src)):
            v = src[n]
            if v.ndim != 3:
                continue
            x, lev, _ = to_tlc(v, args)
            nc = x.shape[2]
            nodes = nc == mesh["nod_area"].shape[1]
            a = mesh["nod_area"][lev] if nodes else np.broadcast_to(mesh["elem_area"], (len(lev), nc))
            x = np.where(wet(mesh, nc, lev, False)[None], x, np.nan)
            if keep is not None:
                x = np.where(keep[None, None, :], x, np.nan)
            yield n, x, lev, a, mesh


def write_time_series(inputs, out, series, cells=None):
    """Variables (time,) or (time, cell) of {name: values}, time axis of IN1."""
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst)
        for n, y in series.items():
            cell = f"cell{y.shape[-1]}"
            dims = ("time",) if y.ndim == 1 else ("time", cell)
            if y.ndim == 2 and cell not in dst.dimensions:
                dst.createDimension(cell, y.shape[1])
            o = dst.createVariable(n, "f8", dims, fill_value=netCDF4.default_fillvals["f8"])
            o[:] = np.ma.masked_invalid(y)


@reference()
def mesh_vol(args, inputs, out):
    """volint (sum of area(level) * dz * value) and volmean (that over the volume of the valid
    cells and levels) per timestep, times an outer -mulc."""
    op, rng = op_range(args, ("volint", "volmean"))
    res = {}
    for n, x, lev, a, mesh in mesh_columns(args, inputs):
        w = a * layer_dz(mesh, lev, rng)[:, None]
        ok = ~np.isnan(x) & (w > 0)[None]
        s = np.where(ok, x * w, 0).sum(axis=(1, 2))
        vol = np.where(ok, w, 0).sum(axis=(1, 2))
        with np.errstate(invalid="ignore", divide="ignore"):
            y = s if op == "volint" else s / vol
        res[n] = np.where(ok.any(axis=(1, 2)), y, np.nan) * outer_mulc(args)
    write_time_series(inputs, out, res)


def nereus_args(mesh, lev, rng):
    """thickness, depth and depth_min/max for nereus: it takes whole layers by their mid-depth,
    so a range must lie on the interfaces to mean the same as the operator's cut."""
    z = mesh["zbar"]
    kw = {}
    if rng is not None:
        if not all(np.isclose(z, r).any() for r in rng):
            raise SystemExit(f"reference: nereus needs a depth range on the interfaces, not {rng}")
        kw = {"depth_min": rng[0], "depth_max": rng[1]}
    return z[lev + 1] - z[lev], (z[lev] + z[lev + 1]) / 2, kw


@reference()
def nereus_volume_mean(args, inputs, out):
    """nereus.volume_mean with the mesh's per-level node areas (nl rows: nereus drops the last)."""
    import warnings
    import nereus
    _, rng = op_range(args, ("volmean",))
    res = {}
    for n, x, lev, a, mesh in mesh_columns(args, inputs):
        dz, depth, kw = nereus_args(mesh, lev, rng)
        area = mesh["nod_area"] if a.shape[1] == mesh["nod_area"].shape[1] else a[0]
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", UserWarning)
            res[n] = np.asarray(nereus.volume_mean(x, area, dz, depth, **kw))
    write_time_series(inputs, out, res)


@reference()
def nereus_heat_content(args, inputs, out):
    """nereus.heat_content (total) for -mulc,<rho*cp> -volint[,z0/z1]: rho = 1000, cp = c / 1000."""
    import warnings
    import nereus
    _, rng = op_range(args, ("volint",))
    c = outer_mulc(args)
    res = {}
    for n, x, lev, a, mesh in mesh_columns(args, inputs):
        dz, depth, kw = nereus_args(mesh, lev, rng)
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", UserWarning)
            res[n] = np.asarray(nereus.heat_content(x, mesh["nod_area"], dz, depth, rho=1000.0,
                                                    cp=c / 1000.0, **kw))
    write_time_series(inputs, out, res)


@reference()
def mesh_vert(args, inputs, out):
    """vertmean, vertstd (layer-thickness weights), vertsum, vertint[,z0/z1] (sum of dz * value,
    the layers cut) per cell over its wet levels."""
    op, rng = op_range(args, ("vertmean", "vertstd", "vertsum", "vertint"))
    res = {}
    for n, x, lev, a, mesh in mesh_columns(args, inputs):
        w = layer_dz(mesh, lev, rng if op == "vertint" else None)[None, :, None]
        w = np.ones_like(w) if op == "vertsum" else w
        ok = ~np.isnan(x) & (w > 0)
        xs = np.where(ok, x, 0)
        sw = np.where(ok, w, 0).sum(axis=1)
        with np.errstate(invalid="ignore", divide="ignore"):
            mean = (xs * w).sum(axis=1) / sw
            y = {"vertmean": mean, "vertsum": xs.sum(axis=1), "vertint": (xs * w).sum(axis=1),
                 "vertstd": np.sqrt((np.where(ok, w, 0) * (xs - mean[:, None]) ** 2).sum(axis=1) / sw)}[op]
        res[n] = np.where(ok.any(axis=1), y, np.nan)
    write_time_series(inputs, out, res)


@reference()
def mesh_bottomvalue(args, inputs, out):
    """The value of each cell's deepest wet level (its bottom)."""
    res = {}
    for n, x, lev, a, mesh in mesh_columns(args, inputs):
        ok = ~np.isnan(x)
        last = x.shape[1] - 1 - np.argmax(ok[:, ::-1], axis=1)
        y = np.take_along_axis(x, last[:, None], axis=1)[:, 0]
        res[n] = np.where(ok.any(axis=1), y, np.nan)
    write_time_series(inputs, out, res)


@reference()
def analytic_vertint(args, inputs, out):
    """vertint,z0/z1 of a field that is 1 in the water (-addc,1 -mulc,1e-30): the depth of water
    inside [z0, z1] in each cell's column, min(z1, bottom) - max(z0, top); missing without any."""
    _, (z0, z1) = op_range(args, ("vertint",))
    res = {}
    for n, x, lev, a, mesh in mesh_columns(args, inputs):
        nodes = x.shape[2] == mesh["nod_area"].shape[1]
        top = mesh["zbar"][mesh["top_nod" if nodes else "top_elem"]]
        bot = mesh["zbar"][mesh["wet_nod" if nodes else "wet_elem"]]
        d = np.minimum(bot, z1) - np.maximum(top, z0)
        res[n] = np.broadcast_to(np.where(d > 0, d, np.nan), (x.shape[0], x.shape[2]))
    write_time_series(inputs, out, res)


def intlevel_values(x, lev, t, mesh, ncell, zc):
    """intlevel of x (time, level, cell; dry levels missing) at the depths t: cdo's rule (see
    mesh_intlevel), then missing where the depth is outside the cell's wet column. Returns
    (time, len(t), cell)."""
    nodes = ncell == mesh["nod_area"].shape[1]
    top = mesh["zbar"][mesh["top_nod" if nodes else "top_elem"]]
    bot = mesh["zbar"][mesh["wet_nod" if nodes else "wet_elem"]]
    z = zc[lev]
    ys = []
    for tz in t:
        if tz == z[0]:
            y = x[:, 0]
        elif tz < z[0] or tz > z[-1]:
            y = np.full_like(x[:, 0], np.nan)
        else:
            k = np.searchsorted(z, tz) - 1          # z[k] < tz <= z[k + 1]
            w1 = np.float32((z[k + 1] - tz) / (z[k + 1] - z[k]))
            w2 = np.float32(1) - w1
            p, q = x[:, k], x[:, k + 1]
            y = np.where(np.isnan(p), np.where(w2 >= 0.5, q, np.nan),
                         np.where(np.isnan(q), np.where(w1 >= 0.5, p, np.nan),
                                  p * np.float64(w1) + q * np.float64(w2)))
        ys.append(np.where((top <= tz) & (tz <= bot), y, np.nan))
    return np.stack(ys, axis=1)


@reference()
def mesh_intlevel(args, inputs, out):
    """intlevel,<depths> with cdo's rule (float32 weights between the two levels around the
    target; with one of them missing the other if its weight is >= 0.5; outside the levels
    missing), then missing where the target lies above the cell's top or below its bottom
    interface. Levels: the depths of nz1 (ascending)."""
    t = [float(z) for z in next(a for a in args if a.startswith("-intlevel,")).split(",")[1:]]
    with netCDF4.Dataset(inputs[0]) as src:
        zc = np.abs(src["nz1"][:].astype("f8"))
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst)
        dst.createDimension("lev", len(t))
        for n, x, lev, a, mesh in mesh_columns(args, inputs):
            y = intlevel_values(x, lev, t, mesh, x.shape[2], zc)
            cell = f"cell{x.shape[2]}"
            if cell not in dst.dimensions:
                dst.createDimension(cell, x.shape[2])
            last = src[n].dimensions[2].startswith("nz")       # FESOM 2.0: (time, cell, level)
            dims = ("time", cell, "lev") if last else ("time", "lev", cell)
            o = dst.createVariable(n, "f8", dims, fill_value=netCDF4.default_fillvals["f8"])
            o[:] = np.ma.masked_invalid(np.swapaxes(y, 1, 2) if last else y)


REFERENCES.update({f"mesh_level_{s}": (lambda s: lambda a, i, o: mesh_level_stat(a, i, o, s))(s)
                   for s in ("mean", "int", "min", "std")})
REFERENCES["nereus_level_mean"] = lambda a, i, o: mesh_level_stat(a, i, o, "nereus")
REFERENCES.update({f"to{t}": (lambda t: lambda a, i, o: mesh_place_map(a, i, o, t))(t)
                   for t in ("nodes", "elems")})


# Region statistics: regmin, regmax, regsum (unweighted), regint (sum of area times value), regmean,
# regstd (area-weighted, population) over the cells of each region of regionmask's masks
# (mask_3D, drop=False). Lon-lat and HEALPix grids weigh cdo's cell areas times R^2; on a FESOM2
# mesh (-setgrid,fesom:) nodes weigh nod_area[k] at layer k (2-D: nod_area[0]), dry levels left
# out. Output (time, [lev,] region), levels after the regions for FESOM 2.0's (time, nod2, nz1).
def regstat(args, inputs, out, stat):
    m, _ = _mask_3D(args, inputs, "reg" + stat)
    nr = m.shape[0]
    m = m.reshape(nr, -1)
    mesh = read_mesh(args) if fesom_nodes(args) is not None else None
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst)
        dst.createDimension("region", nr)
        for n in selected(args, data_vars(src)):
            v = src[n]
            if mesh is not None:
                x, lev, last = mesh_field(args, v, mesh)
                a = mesh["nod_area"][lev] if v.ndim == 3 else mesh["nod_area"][:1]
            else:
                x = np.ma.filled(masked(v[:]), np.nan).reshape(v.shape[0], 1, -1)
                a = (cell_areas(src, n) * R * R).reshape(1, -1)
                last = False
            nt, nl, _ = x.shape
            res = np.full((nt, nl, nr), np.nan)
            for r in range(nr):
                xr_ = x[:, :, m[r]]
                ok = ~np.isnan(xr_)
                w = np.where(ok, np.broadcast_to(a[None][:, :, m[r]], xr_.shape), 0.0)
                xs = np.where(ok, xr_, 0.0)
                with np.errstate(invalid="ignore", divide="ignore"):
                    mean = (w * xs).sum(axis=2) / w.sum(axis=2)
                    val = {
                        "min": lambda: np.where(ok, xr_, np.inf).min(axis=2, initial=np.inf),
                        "max": lambda: np.where(ok, xr_, -np.inf).max(axis=2, initial=-np.inf),
                        "sum": lambda: xs.sum(axis=2),
                        "int": lambda: (w * xs).sum(axis=2),
                        "mean": lambda: mean,
                        "std": lambda: np.sqrt((w * (xs - mean[..., None]) ** 2).sum(axis=2)
                                               / w.sum(axis=2)),
                    }[stat]()
                res[:, :, r] = np.where(ok.any(axis=2), val, np.nan)
            if v.ndim == 2 or mesh is None:
                dims, y = ("time", "region"), res[:, 0, :]
            else:
                if "lev" not in dst.dimensions:
                    dst.createDimension("lev", nl)
                dims = ("time", "region", "lev") if last else ("time", "lev", "region")
                y = np.swapaxes(res, 1, 2) if last else res
            o = dst.createVariable(n, "f8", dims, fill_value=netCDF4.default_fillvals["f8"])
            o[:] = np.ma.masked_invalid(y)


REFERENCES.update({f"reg{s}": (lambda s: lambda a, i, o: regstat(a, i, o, s))(s)
                   for s in ("min", "max", "sum", "int", "mean", "std")})


@reference()
def nereus_regmean(args, inputs, out, stat="mean"):
    """regmean,regions=basins,select=<names> of the 2-D node variables on a FESOM2 mesh with
    nereus: its own basin masks (nereus.diag.regions.get_region_mask on oceanBasins, shapely's
    contains) and nereus.surface_mean with the mesh's nod_area[0]; regint (stat="int") of an ice
    concentration: nereus.ice_area (sum of concentration times nod_area[0])."""
    from nereus.diag.regions import get_region_mask
    import nereus
    fn = nereus.surface_mean if stat == "mean" else nereus.ice_area
    op = next(a for a in args if a.lstrip("-").startswith(f"reg{stat},"))
    names = [p for p in op.split(",")[1:] if "=" not in p or p.startswith("select=")]
    names = [p.split("=", 1)[-1] for p in names]
    lon, lat = fesom_nodes(args)
    mesh = read_mesh(args)
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst)
        dst.createDimension("region", len(names))
        for n in selected(args, data_vars(src)):
            v = src[n]
            x = np.ma.filled(masked(v[:]), np.nan)
            res = np.stack([fn(x, mesh["nod_area"][0], mask=get_region_mask(lon, lat, b, "oceanBasins"))
                            for b in names], axis=1)
            o = dst.createVariable(n, "f8", ("time", "region"), fill_value=netCDF4.default_fillvals["f8"])
            o[:] = np.ma.masked_invalid(res)


@reference()
def nereus_regint(args, inputs, out):
    nereus_regmean(args, inputs, out, "int")


def hist_edges(args):
    """Bin edges and out=fraction of the row's fldhist."""
    opts, _ = op_args(args, "fldhist")
    if "bins" in opts:
        lo, hi, step = map(float, opts["bins"].split("/"))
        n = int(round((hi - lo) / step))
        edges = np.append(lo + step * np.arange(n), hi)
    else:
        edges = np.array([float(x) for x in opts["edges"].split("/")])
    return edges, opts.get("out", "area") == "fraction"


def hist_row(val, w, edges):
    """(below, bins..., above) sums of the weights w of the values val (NaN excluded)."""
    nb = len(edges) - 1
    ok = ~np.isnan(val)
    val, w = val[ok], w[ok]
    slot = np.where(val < edges[0], 0, np.where(
        val > edges[-1], nb + 1, np.minimum(np.searchsorted(edges, val, side="right"), nb)))
    return np.bincount(slot, weights=w, minlength=nb + 2), w.sum()


@reference()
def mesh_fldhist(args, inputs, out):
    """fldhist on a FESOM2 mesh, per level: nodes weigh nod_area[k] at layer k, elements
    elem_area; dry levels excluded; out=fraction divides each level by its valid area. Output in
    the input's dimension order, the cell dimension replaced by bin."""
    edges, frac = hist_edges(args)
    nb = len(edges) - 1
    mesh = read_mesh(args)
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst)
        dst.createDimension("bin", nb)
        for n in selected(args, data_vars(src)):
            v = src[n]
            x, lev, last = to_tlc(v, args)
            nt, nl, nc = x.shape
            nodes = nc == mesh["nod_area"].shape[1]
            a = mesh["nod_area"][lev] if nodes else np.broadcast_to(mesh["elem_area"], (nl, nc))
            x = np.where(wet(mesh, nc, lev, v.ndim == 2)[None], x, np.nan)
            res = np.zeros((nt, nl, nb + 2))
            for t in range(nt):
                for k in range(nl):
                    h, tot = hist_row(x[t, k], a[k], edges)
                    res[t, k] = h / tot if frac and tot > 0 else (np.nan if frac else h)
            parts = [("", res[..., 1:nb + 1]), ("_below", res[..., 0]), ("_above", res[..., nb + 1])]
            if v.ndim == 3 and "lev" not in dst.dimensions:
                dst.createDimension("lev", nl)
            for suffix, vals in parts:
                if v.ndim == 2:
                    vals, dims = vals[:, 0], ("time",) + (("bin",) if suffix == "" else ())
                elif suffix == "":
                    vals, dims = (np.swapaxes(vals, 1, 2), ("time", "bin", "lev")) if last \
                        else (vals, ("time", "lev", "bin"))
                else:
                    dims = ("time", "lev")
                o = dst.createVariable(n + suffix, "f8", dims, fill_value=netCDF4.default_fillvals["f8"])
                o[:] = np.ma.masked_invalid(vals)


def mesh_zonal(args, inputs, out, name):
    """zon<stat>,zonal_<DY> on a FESOM2 mesh: a node (element) is in the band containing its
    centre, [lo, hi); zonmean and zonint weigh it with nod_area[k] at layer k (elements:
    elem_area), zonsum sums the values; dry levels excluded; a band without valid cells is
    missing. Element centres: the normalised mean of their nodes' unit vectors."""
    bands = zonal_bands(args, name)
    mesh = read_mesh(args)
    lon, lat = fesom_nodes(args)
    p = unit(lon, lat)[mesh["elem"]].sum(axis=1)
    elat = np.degrees(np.arcsin(p[:, 2] / np.linalg.norm(p, axis=1)))
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst)
        dst.createDimension("lat", len(bands))
        for n in selected(args, data_vars(src)):
            v = src[n]
            x, lev, last = to_tlc(v, args)
            nt, nl, nc = x.shape
            nodes = nc == mesh["nod_area"].shape[1]
            a = mesh["nod_area"][lev] if nodes else np.broadcast_to(mesh["elem_area"], (nl, nc))
            if name == "zonsum":
                a = np.ones_like(a)
            c = lat if nodes else elat
            x = np.where(wet(mesh, nc, lev, v.ndim == 2)[None], x, np.nan)
            ok = ~np.isnan(x)
            res = np.full((nt, nl, len(bands)), np.nan)
            for b, (lo, hi) in enumerate(bands):
                m = (c >= lo) & (c < hi)
                w = np.where(ok & m[None, None], a[None], 0.0)
                s = (w * np.where(ok, x, 0)).sum(axis=2)
                cnt = (ok & m[None, None]).sum(axis=2)
                with np.errstate(invalid="ignore", divide="ignore"):
                    r = s / w.sum(axis=2) if name == "zonmean" else s
                res[..., b] = np.where(cnt > 0, r, np.nan)
            if v.ndim == 2:
                vals, dims = res[:, 0], ("time", "lat")
            else:
                if "lev" not in dst.dimensions:
                    dst.createDimension("lev", nl)
                vals, dims = (np.swapaxes(res, 1, 2), ("time", "lat", "lev")) if last \
                    else (res, ("time", "lev", "lat"))
            o = dst.createVariable(n, "f8", dims, fill_value=netCDF4.default_fillvals["f8"])
            o[:] = np.ma.masked_invalid(vals)


for _op in ("zonmean", "zonint", "zonsum"):
    REFERENCES["mesh_" + _op] = (lambda op: lambda a, i, o: mesh_zonal(a, i, o, op))(_op)


@reference()
def mesh_moc(args, inputs, out):
    """moc[,dlat=<d>|lat=<a>/<b>...][,regions=<set>,select=<name>...] of w on the interfaces of
    a FESOM2 mesh: each element's mean of w at its valid nodes at the interfaces top_e <= k <
    wet_e (the tops of its wet layers) times elem_area; psi(line) = -sum over the elements whose
    centre latitude is >= the line, times 1e-6 (Sv); missing at k where no element whose vertex
    latitudes span the line has k among its interfaces (top_e <= k <= wet_e). The basin: element
    centres in the regions' polygons of $CDORS_REGIONS/<set>.geojson (shapely, longitudes wrapped
    into the polygons' range); centres: normalised mean of the nodes' unit vectors."""
    import json
    import shapely
    from shapely.geometry import shape
    op = next(a for a in args if a.startswith("-moc")).split(",")[1:]
    lines = np.arange(-90.0, 90.0 + 1e-9, 1.0)
    rset, sel = None, []
    for p in op:
        k, _, v = p.partition("=")
        if k == "dlat":
            d = float(v)
            lines = -90.0 + d * np.arange(int(np.floor(180.0 / d + 1e-9)) + 1)
        elif k == "lat":
            lines = np.unique([float(x) for x in v.split("/")])
        elif k == "regions":
            rset = v
        elif k == "select" or (not v and rset):
            sel.append(v or k)
    mesh = read_mesh(args)
    lon, lat = fesom_nodes(args)
    el = mesh["elem"]
    p = unit(lon, lat)[el].sum(axis=1)
    p /= np.linalg.norm(p, axis=1)[:, None]
    elon, elat = np.degrees(np.arctan2(p[:, 1], p[:, 0])), np.degrees(np.arcsin(p[:, 2]))
    basin = np.ones(len(el), bool)
    if rset:
        with open(os.path.join(os.environ["CDORS_REGIONS"], rset + ".geojson")) as f:
            feats = json.load(f)["features"]
        basin[:] = False
        for ft in feats:
            pr = ft["properties"]
            if sel and pr.get("abbrev") not in sel and pr.get("name") not in sel:
                continue
            g = shape(ft["geometry"])
            x = elon if g.bounds[2] <= 180 else np.where(elon < g.bounds[0], elon + 360.0, elon)
            basin |= shapely.contains_xy(g, x, elat)
    top, end = mesh["top_elem"], mesh["wet_elem"]       # wet layers top..end
    vlo, vhi = lat[el].min(axis=1), lat[el].max(axis=1)
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst)
        v = src[selected(args, data_vars(src))[0]]
        x, lev, last = to_tlc(v, args)
        nt, nl, _ = x.shape
        x = np.where(wet(mesh, x.shape[2], lev, False, True)[None], x, np.nan)
        res = np.full((nt, nl, len(lines)), np.nan)
        for j, k in enumerate(lev):
            on = basin & (top <= k) & (k < end)
            has = basin & (top <= k) & (k <= end)
            crossed = [(has & (vlo <= y) & (y <= vhi)).any() for y in lines]
            for t in range(nt):
                xe = x[t, j][el]
                n = np.isfinite(xe).sum(axis=1)
                with np.errstate(invalid="ignore"):
                    we = np.where(np.isfinite(xe), xe, 0).sum(axis=1) / n
                ok = on & (n > 0)
                c = we[ok] * mesh["elem_area"][ok]
                for i, y in enumerate(lines):
                    if crossed[i]:
                        res[t, j, i] = -c[elat[ok] >= y].sum() * 1e-6
        dst.createDimension("lev", nl)
        dst.createDimension("lat", len(lines))
        dims, res = (("time", "lat", "lev"), np.swapaxes(res, 1, 2)) if last else (("time", "lev", "lat"), res)
        o = dst.createVariable("moc", "f8", dims, fill_value=netCDF4.default_fillvals["f8"])
        o[:] = np.ma.masked_invalid(res)


@reference()
def mesh_transport(args, inputs, out):
    """transport,<lon0>,<lat0>,<lon1>,<lat1>[,...] of the two components the row's -selname
    pick (eastward, then northward; both from {in}) on the elements and layers of a FESOM2 mesh.
    Each great-circle arc a -> b cuts the edges whose nodes lie on opposite sides of its plane
    (n . p >= 0 counts as the left), where the edge's chord meets the plane (normalised), if that
    point's angle from a about n lies in [0, |ab|]; each element's cuts in path order pair up
    (entry, exit), an unpaired last one is left out; the chord between the edges' midpoints
    (normalised node sums) projected on east and north at the element's centre (normalised mean
    of the nodes' unit vectors), times r_earth = 6367500 m; T(k) = sum of (u * -dN + v * dE) *
    dz_k over the crossed elements wet at k (top_e <= k < wet_e) with finite values, times 1e-6
    (Sv); missing where there is none."""
    op = next(a for a in args if a.startswith("-transport,"))
    nums = [float(x) for x in op.split(",")[1:]]
    pts = list(zip(nums[0::2], nums[1::2]))
    names = [a.split(",")[1] for a in args if a.startswith("-selname,")]
    mesh = read_mesh(args)
    lon, lat = fesom_nodes(args)
    el = mesh["elem"]
    P = unit(lon, lat)
    c = P[el].sum(axis=1)
    c /= np.linalg.norm(c, axis=1)[:, None]
    clon, clat = np.arctan2(c[:, 1], c[:, 0]), np.arcsin(c[:, 2])
    cuts, done = [], 0.0
    for p0, p1 in zip(pts[:-1], pts[1:]):
        a, b = unit(*p0), unit(*p1)
        n = np.cross(a, b)
        nn = n / np.linalg.norm(n)
        arc = np.arctan2(np.linalg.norm(n), a @ b)
        left = P @ n >= 0
        for i, j in ((0, 1), (1, 2), (2, 0)):
            e = np.nonzero(left[el[:, i]] != left[el[:, j]])[0]
            pi, pj = P[el[e, i]], P[el[e, j]]
            di, dj = pi @ n, pj @ n
            x = pi + (di / (di - dj))[:, None] * (pj - pi)
            x /= np.linalg.norm(x, axis=1)[:, None]
            pos = np.arctan2(np.cross(a, x) @ nn, x @ a)
            ok = (pos >= 0) & (pos <= arc)
            mid = pi + pj
            mid /= np.linalg.norm(mid, axis=1)[:, None]
            cuts += [(int(k), done + q, m) for k, q, m in zip(e[ok], pos[ok], mid[ok])]
        done += arc
    cuts.sort(key=lambda h: (h[0], h[1]))
    cross = {}                                          # element -> [first cut, dE, dN]
    for k in sorted({h[0] for h in cuts}):
        hs = [h for h in cuts if h[0] == k]
        if len(hs) < 2:
            continue
        lo, la = clon[k], clat[k]
        east = np.array([-np.sin(lo), np.cos(lo), 0.0])
        north = np.array([-np.sin(la) * np.cos(lo), -np.sin(la) * np.sin(lo), np.cos(la)])
        d = sum(hs[q + 1][2] - hs[q][2] for q in range(0, len(hs) - 1, 2))
        cross[k] = (hs[0][1], d @ east * 6367500.0, d @ north * 6367500.0)
    order = sorted(cross, key=lambda k: (cross[k][0], k))
    ee = np.array(order)
    dE = np.array([cross[k][1] for k in order])
    dN = np.array([cross[k][2] for k in order])
    dz = np.diff(mesh["zbar"])
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst)
        xu, lev, _ = to_tlc(src[names[0]], args)
        xv, _, _ = to_tlc(src[names[1]], args)
        w = wet(mesh, xu.shape[2], lev, False)[:, ee]          # (levels, crossed)
        fu = np.where(w[None], xu[:, :, ee], np.nan)
        fv = np.where(w[None], xv[:, :, ee], np.nan)
        f = np.nan_to_num(fu) * -dN + np.nan_to_num(fv) * dE
        has = np.isfinite(fu) | np.isfinite(fv)
        res = np.where(has.any(axis=2), f.sum(axis=2) * dz[lev][None, :] * 1e-6, np.nan)
        dst.createDimension("lev", len(lev))
        o = dst.createVariable("transport", "f8", ("time", "lev"), fill_value=netCDF4.default_fillvals["f8"])
        o[:] = np.ma.masked_invalid(res)


def nh_nodes(args):
    """Nodes of the row's -sellonlatbox,-180,180,0,90 (latitude >= 0, both edges included)."""
    if not any(a.startswith("-sellonlatbox,-180,180,0,90") for a in args):
        raise SystemExit("reference: the row must select the northern hemisphere with "
                         "-sellonlatbox,-180,180,0,90")
    return fesom_nodes(args)[1] >= 0


@reference()
def ice_extent(args, inputs, out):
    """Ice-extent recipe -fldhist,edges=<t>/<hi> -sellonlatbox,-180,180,0,90 on a FESOM2 mesh:
    the bin is nereus.ice_extent (area of the nodes with a_ice >= t, nod_area[0], northern
    hemisphere) less the area above hi; below and above from NumPy."""
    import nereus
    edges, frac = hist_edges(args)
    assert len(edges) == 2 and not frac, "ice_extent: one bin, out=area"
    nh = nh_nodes(args)
    area = read_mesh(args)["nod_area"][0]
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst)
        dst.createDimension("bin", 1)
        for n in selected(args, data_vars(src)):
            x = np.ma.filled(masked(src[n][:]), np.nan)
            above = np.where(nh & (x > edges[1]), area, 0).sum(axis=1)
            ext = nereus.ice_extent(x, area, threshold=edges[0], mask=nh) - above
            below = np.where(nh & (x < edges[0]), area, 0).sum(axis=1)
            for suffix, vals, dims in [("", ext[:, None], ("time", "bin")), ("_below", below, ("time",)),
                                       ("_above", above, ("time",))]:
                o = dst.createVariable(n + suffix, "f8", dims, fill_value=netCDF4.default_fillvals["f8"])
                o[:] = vals


@reference()
def ice_area(args, inputs, out):
    """Ice-area recipe -fldint -sellonlatbox,-180,180,0,90 on a FESOM2 mesh: nereus.ice_area
    with nod_area[0] over the northern-hemisphere nodes."""
    import nereus
    nh = nh_nodes(args)
    area = read_mesh(args)["nod_area"][0]
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst)
        for n in selected(args, data_vars(src)):
            x = np.ma.filled(masked(src[n][:]), np.nan)
            o = dst.createVariable(n, "f8", ("time",), fill_value=netCDF4.default_fillvals["f8"])
            o[:] = nereus.ice_area(x, area, mask=nh)


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


def compare_many(want, rel, abs_, gots, names):
    """Label of the first failing GOT, or None."""
    first = None
    for g in gots:
        label, got = g.split("=", 1)
        print(f"== compare {label}")
        try:
            bad = compare(want, got, rel, abs_, names)
        except Exception as e:  # unreadable file or missing variable: a failure of that output
            print(f"{label}: {type(e).__name__}: {e}")
            bad = 1
        if bad and first is None:
            first = label
    if first is not None:
        print(f"first failure: {first}")
    return first


def compare_batch():
    out = sys.stdout
    for line in sys.stdin:
        id_, log, want, rel, abs_, names, *gots = line.rstrip("\n").split("\t")
        with open(log, "a") as sys.stdout:
            first = compare_many(want, float(rel), float(abs_), gots, names.split())
        sys.stdout = out
        print(f"{id_}\t{first or ''}", flush=True)


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
    elif cmd == "compare-many":
        want, rel, abs_, *rest = sys.argv[2:]
        k = rest.index("--")
        sys.exit(compare_many(want, float(rel), float(abs_), rest[:k], rest[k + 1:]) is not None)
    elif cmd == "compare-batch":
        compare_batch()
    elif cmd == "version":
        version(sys.argv[2])
    else:
        sys.exit(f"reference: unknown command {cmd}")


if __name__ == "__main__":
    main()
