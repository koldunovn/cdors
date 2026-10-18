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


# the functions a row can name (ref:<lib>.<fn>)
REFERENCES = {"fldmean": fldmean, "fldmean_wrong": fldmean_wrong}


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
