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
        if v.shape[-1] != len(fes[0]):
            raise SystemExit(f"reference: {name} is not on the mesh nodes")
        return fes[0], fes[1], v.dimensions[-1:]
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


# the functions a row can name (ref:<lib>.<fn>)
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


# the functions a row can name (ref:<lib>.<fn>)
# Field statistics per level and node <-> element mapping on a FESOM2 mesh, in both dimension
# orders: (time, nz1, nod2|elem) and FESOM 2.0's (time, nod2|elem, nz1). Levels outside a cell's
# wet range (nlevels - 1 layers from the top) are missing whatever the file stores (0).
def read_mesh(args):
    """Per-level node areas (nl, nod), element areas, elem -> nodes (ne, 3, 0-based) and the
    number of wet layers of nodes and elements."""
    with netCDF4.Dataset(os.path.join(mesh_dir(args), "fesom.mesh.diag.nc")) as m:
        el = m["elem" if "elem" in m.variables else "elements"][:].astype("i8")
        el = el.T if el.shape[0] == 3 else el
        return {"nod_area": m["nod_area"][:].astype("f8"), "elem_area": m["elem_area"][:].astype("f8"),
                "elem": el - el.min(), "wet_nod": m["nlevels_nod2D"][:].astype("i8") - 1,
                "wet_elem": m["nlevels"][:].astype("i8") - 1}


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
    x = np.ma.filled(masked(v[:]), np.nan)
    if v.ndim == 2:
        return x[:, None, :], np.arange(1), False
    last = v.dimensions[2].startswith("nz")
    x = np.swapaxes(x, 1, 2) if last else x
    lev = np.arange(x.shape[1])
    for a in args:
        if a.startswith("-sellevel,"):
            want = [float(z) for z in a.split(",")[1:]]
            lev = np.array([k for k, z in enumerate(v.group()["nz1"][:]) if z in want])
    return x[:, lev, :], lev, last


def wet(mesh, ncell, lev, surface):
    """(levels, ncell) wet mask at the mesh levels lev; 2-D variables are wet everywhere."""
    w = mesh["wet_nod"] if ncell == len(mesh["wet_nod"]) else mesh["wet_elem"]
    return np.ones((1, ncell), bool) if surface else lev[:, None] < w[None, :]


def mesh_level_stat(args, inputs, out, stat):
    """fld<stat> per level: nodes weigh nod_area[k] at layer k, elements elem_area; dry levels
    excluded; mean, int (sum of area times value), min, std (population, weighted); nereus: the
    mean of nereus.surface_mean per level with these areas."""
    mesh = read_mesh(args)
    with netCDF4.Dataset(inputs[0]) as src, netCDF4.Dataset(out, "w") as dst:
        copy_time(src, dst)
        for n in selected(args, data_vars(src)):
            v = src[n]
            x, lev, _ = to_tlc(v, args)
            nt, nl, nc = x.shape
            nodes = nc == mesh["nod_area"].shape[1]
            a = mesh["nod_area"][lev] if nodes else np.broadcast_to(mesh["elem_area"], (nl, nc))
            x = np.where(wet(mesh, nc, lev, v.ndim == 2)[None], x, np.nan)
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
        copy_time(src, dst)
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



REFERENCES.update({f"mesh_level_{s}": (lambda s: lambda a, i, o: mesh_level_stat(a, i, o, s))(s)
                   for s in ("mean", "int", "min", "std")})
REFERENCES["nereus_level_mean"] = lambda a, i, o: mesh_level_stat(a, i, o, "nereus")
REFERENCES.update({f"to{t}": (lambda t: lambda a, i, o: mesh_place_map(a, i, o, t))(t)
                   for t in ("nodes", "elems")})


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
