"""Real-data check of cdors against cdo (and xarray/numpy), report only.

Run through bench/realdata_check.sh, which sets the environment. Every case runs one cdors chain on
a small slice of a real dataset on Levante and the same chain with cdo (or a numpy/xarray
computation), compares values, missing patterns, coordinates and timestamps, and prints one row
per case:

    identical         same values (bit for bit), missing pattern, coordinates and timestamps
    within tolerance  values within the case's tolerance (e.g. 2 float32 ulp of the largest value,
                      or one histogram bin for cdo's approximate percentiles)
    DIFFERENT         anything else (the note says what)
    error             a command failed (the note gives the log file)
    skipped           input not found (or --skip-remote)

All outputs, logs and summary.{txt,json} go to a fresh run directory; nothing is deleted or
overwritten.
"""

import argparse
import datetime
import glob
import json
import os
import subprocess
import sys
import time

import numpy as np
import netCDF4

CDORS = os.environ.get("CDORS", "cdors")
CDO = os.environ.get("CDO", "cdo")

VIEWS = "/scratch/a/a270088/cdors-bench/views"
HP_DAY = f"{VIEWS}/ngc4008_P1D_9_tas_30d.zarr"
HP_DAY3Y = f"{VIEWS}/ngc4008_P1D_9_tas_3y.zarr"
HP_3H = f"{VIEWS}/ngc4008_PT3H_9_tas_1y.zarr"
HP_3H_JAN = f"{VIEWS}/ngc4008_PT3H_9_tas_248.zarr"  # January 2020, one time chunk
W1 = "/work/kd1453/rechunked_ngc4008/ngc4008_P1D_9.zarr"
ICON_PP = "/work/bm1344/k202193/ICON/erc2002/postprocessing/interpolation/control_1950"
ATM = f"{ICON_PP}/atm_2d_1d_mean_remap025"
OCE = f"{ICON_PP}/oce_2d_1d_mean_remap025"
PQ = "/work/bm1344/k202193/Kerchunk/erc2002/control_1950/v20240618/atm_2d_1d_mean_remap025.parq"
CLOUD = ("https://eerie.cloud.dkrz.de/datasets/icon-esm-er.eerie-control-1950.v20240618."
         "atmos.gr025.2d_daily_mean/kerchunk")
ICON_RAW = ("/work/bm1344/k203123/experiments/erc2002/run_19910101T000000-19910131T235900/"
            "erc2002_atm_2d_1d_mean_19910101T000000Z.nc")
ICON_GRID = "/pool/data/ICON/grids/public/mpim/0033/icon_grid_0033_R02B08_G.nc"
FESOM = "/work/bm1344/AWI/Cycle3/IFS_28-FESOM_25-cycle3/daily_means/sst/sst_2020.nc"
FESOM_MESH = "/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/orca25/orca25_griddes_nodes.nc"
HADGEM = ("/work/ik1017/CMIP6/data/CMIP6/CMIP/MOHC/HadGEM3-GC31-LL/historical/r1i1p1f3/Omon/tos/"
          "gn/v20190624/tos_Omon_HadGEM3-GC31-LL_historical_r1i1p1f3_gn_195001-201412.nc")


def zarr(p):
    """A Zarr store as cdo (NCZarr) opens it."""
    return f"file://{p}#mode=zarr,file"


def files(pattern):
    return sorted(glob.glob(pattern))


def pctl3(chain, p=95):
    """cdo's three-input percentile form: timpctl,p chain -timmin chain -timmax chain."""
    return [f"-timpctl,{p}"] + chain + ["-timmin"] + chain + ["-timmax"] + chain


# Cases. Keys:
#   id, what       name and description
#   cdors          cdors arguments without the output file
#   cdo            cdo arguments without the output file (a list; glob patterns already expanded)
#   py             instead of cdo: name of a reference function below (text output of cdors)
#   mode           "ulp" (default: 2 float32 ulp of the largest |reference| value), "exact",
#                  "abs:<x>", "bin" (cdo's histogram percentiles: one bin of the min/max range,
#                  see "bin_chain"), "text" (stdout compared line by line), "num:<rel>"
#                  (numbers printed by cdors vs the py reference), "plan" (only times --plan),
#                  "mem" (the plan must fit --mem or be refused with memory_limit)
#   times          compare timestamps (default True)
#   needs          paths that must exist (otherwise skipped)
#   remote         needs network
CASES = [
    dict(id="hp_box_a", what="HEALPix sellonlatbox 5,15,50,56 (edge cells)",
         cdors=["-sellonlatbox,5,15,50,56", "-seltimestep,1", HP_DAY],
         cdo=["-sellonlatbox,5,15,50,56", "-seltimestep,1", zarr(HP_DAY)], needs=[HP_DAY]),
    dict(id="hp_box_b", what="HEALPix sellonlatbox 45,90,-30,30",
         cdors=["-sellonlatbox,45,90,-30,30", "-seltimestep,1", HP_DAY],
         cdo=["-sellonlatbox,45,90,-30,30", "-seltimestep,1", zarr(HP_DAY)], needs=[HP_DAY]),
    dict(id="hp_box_c", what="HEALPix sellonlatbox 9,11,53,54.5",
         cdors=["-sellonlatbox,9,11,53,54.5", "-seltimestep,1", HP_DAY],
         cdo=["-sellonlatbox,9,11,53,54.5", "-seltimestep,1", zarr(HP_DAY)], needs=[HP_DAY]),
    dict(id="hp_ymonmean", what="HEALPix fldmean ymonmean (January of 2 years)",
         cdors=["-fldmean", "-ymonmean", "-selmon,1", "-selyear,2020/2021", HP_DAY3Y],
         cdo=["-fldmean", "-ymonmean", "-selmon,1", "-selyear,2020/2021", zarr(HP_DAY3Y)],
         needs=[HP_DAY3Y]),
    dict(id="hp_yearmean_box", what="HEALPix yearmean of a box",
         cdors=["-yearmean", "-sellonlatbox,-10,40,35,70", "-selmon,1/3", "-selyear,2020", HP_DAY3Y],
         cdo=["-yearmean", "-sellonlatbox,-10,40,35,70", "-selmon,1/3", "-selyear,2020",
              zarr(HP_DAY3Y)], needs=[HP_DAY3Y]),
    dict(id="hp_timpctl_box", what="HEALPix timpctl,95 of a box (31 values: exact in cdo)",
         cdors=["-timpctl,95", "-sellonlatbox,5,15,50,56", "-selmon,1", "-selyear,2020", HP_DAY3Y],
         cdo=pctl3(["-sellonlatbox,5,15,50,56", "-selmon,1", "-selyear,2020", zarr(HP_DAY3Y)]),
         needs=[HP_DAY3Y]),
    dict(id="hp_ydaymean_box", what="HEALPix 3-hourly ydaymean of a box (January 2020)",
         cdors=["-ydaymean", "-sellonlatbox,0,30,40,60", HP_3H_JAN],
         cdo=["-ydaymean", "-sellonlatbox,0,30,40,60", zarr(HP_3H_JAN)], needs=[HP_3H_JAN]),
    dict(id="hp_pctl_point", what="HEALPix remapnn to a point, timpctl,95 of 248 values (cdo: bins)",
         cdors=["-timpctl,95", "-remapnn,lon=10/lat=53.55", HP_3H_JAN],
         cdo=pctl3(["-remapnn,lon=10/lat=53.55", zarr(HP_3H_JAN)]),
         bin_chain=["-remapnn,lon=10/lat=53.55", HP_3H_JAN],
         mode="bin", needs=[HP_3H_JAN]),
    dict(id="hp_timmean_xr", what="HEALPix fldmean timmean July 2020 vs numpy (W1 store)",
         cdors=["outputf,%.9g", "-fldmean", "-timmean", "-selmon,7", "-selyear,2020",
                "-selname,tas", W1],
         py="ref_w1_july", mode="num:1e-6", needs=[W1]),
    dict(id="icon_monmean_merge", what="ICON 0.25 deg pr monmean over two files (mergetime)",
         cdors=["-monmean", "-selname,pr", "-mergetime", f"{ATM}/run_1991010*/*.nc"],
         cdo=["-monmean", "-select,name=pr"] + files(f"{ATM}/run_1991010*/*.nc"),
         needs=[ATM]),
    dict(id="icon_kerchunk_box", what="ICON kerchunk (parquet) fldmean of a box vs raw files "
                                      "(dates differ: values only)",
         cdors=["-fldmean", "-sellonlatbox,-80,0,20,70", "-selmon,1", "-selyear,1950",
                "-selname,pr", PQ],
         cdo=["-fldmean", "-sellonlatbox,-80,0,20,70", "-seltimestep,1/31", "-select,name=pr"]
         + files(f"{ATM}/run_19910101*/*.nc"),
         times=False, needs=[PQ, ATM]),
    dict(id="icon_cloud_box", what="ICON kerchunk over https vs raw files (values only)",
         cdors=["-fldmean", "-sellonlatbox,-10,40,35,70", "-selmon,1", "-selyear,1950",
                "-selname,pr", CLOUD],
         cdo=["-fldmean", "-sellonlatbox,-10,40,35,70", "-seltimestep,1/31", "-select,name=pr"]
         + files(f"{ATM}/run_19910101*/*.nc"),
         times=False, remote=True, needs=[ATM]),
    dict(id="icon_oce_remapbil", what="ICON ocean 0.25 deg to r360x180 remapbil, timmean",
         cdors=["-timmean", "-remapbil,r360x180", "-selname,to", "-mergetime",
                f"{OCE}/run_19910101*/*.nc"],
         cdo=["-timmean", "-remapbil,r360x180", "-select,name=to"]
         + files(f"{OCE}/run_19910101*/*.nc"), needs=[OCE]),
    dict(id="icon_info_text", what="info text of the remapbil chain (param IDs)",
         cdors=["info", "-timmean", "-remapbil,r360x180", "-selmon,1", "-selname,to",
                files(f"{OCE}/run_19910101*/*.nc")[0] if files(f"{OCE}/run_19910101*/*.nc") else ""],
         cdo=["info", "-timmean", "-remapbil,r360x180", "-selmon,1", "-selname,to",
              files(f"{OCE}/run_19910101*/*.nc")[0] if files(f"{OCE}/run_19910101*/*.nc") else ""],
         mode="text", needs=[OCE]),
    dict(id="icon_outputtab_text", what="outputtab name,param,code,value (param attributes)",
         cdors=["outputtab,name,param,code,date,value", "-fldmean", "-seltimestep,1/3",
                files(f"{ATM}/run_19910101*/*.nc")[0] if files(f"{ATM}/run_19910101*/*.nc") else ""],
         cdo=["outputtab,name,param,code,date,value", "-fldmean", "-seltimestep,1/3",
              files(f"{ATM}/run_19910101*/*.nc")[0] if files(f"{ATM}/run_19910101*/*.nc") else ""],
         mode="text", needs=[ATM]),
    dict(id="icon_r2b8_fldmean", what="ICON R2B8 tas fldmean with setgrid (one month)",
         cdors=["-fldmean", "-selyear,1991", "-setgrid," + ICON_GRID, "-selname,tas", ICON_RAW],
         cdo=["-fldmean", "-selyear,1991", "-setgrid," + ICON_GRID, "-select,name=tas", ICON_RAW],
         needs=[ICON_RAW, ICON_GRID]),
    dict(id="icon_r2b8_plan", what="ICON R2B8 --plan time",
         cdors=["-fldmean", "-selyear,1991", "-setgrid," + ICON_GRID, "-selname,tas", ICON_RAW],
         mode="plan", needs=[ICON_RAW, ICON_GRID]),
    dict(id="fesom_fldmean", what="FESOM orca25 sst fldmean with setgrid (January)",
         cdors=["-fldmean", "-selmon,1", "-selyear,2020", "-setgrid," + FESOM_MESH, FESOM],
         cdo=["-fldmean", "-selmon,1", "-selyear,2020", "-setgrid," + FESOM_MESH, FESOM],
         needs=[FESOM, FESOM_MESH]),
    dict(id="fesom_plan", what="FESOM orca25 --plan time",
         cdors=["-fldmean", "-selyear,2020", "-setgrid," + FESOM_MESH, FESOM],
         mode="plan", needs=[FESOM, FESOM_MESH]),
    dict(id="hadgem_fldmean", what="HadGEM3 ORCA1 tos fldmean, 2000 (curvilinear)",
         cdors=["-fldmean", "-selyear,2000", HADGEM],
         cdo=["-fldmean", "-selyear,2000", HADGEM], needs=[HADGEM]),
    dict(id="hadgem_remapbil", what="HadGEM3 ORCA1 tos remapbil to r360x180, 2000",
         cdors=["-remapbil,r360x180", "-selyear,2000", HADGEM],
         cdo=["-remapbil,r360x180", "-selyear,2000", HADGEM], needs=[HADGEM]),
    dict(id="mem_remap", what="--mem 300M on a HEALPix remapnn (must fit or be refused)",
         cdors=["--mem", "300M", "-remapnn,r360x180", "-seltimestep,1/64", "-selname,tas", HP_3H],
         mode="mem", needs=[HP_3H]),
    dict(id="mem_timstat", what="--mem 300M on a HEALPix timpctl (must fit or be refused)",
         cdors=["--mem", "300M", "-timpctl,95", "-seltimestep,1/64", "-selname,tas", HP_3H],
         mode="mem", needs=[HP_3H]),
]


def ref_w1_july():
    """Area mean (HEALPix: plain mean) of the July 2020 time mean of tas, with numpy."""
    import xarray as xr
    ds = xr.open_zarr(W1, consolidated=None)
    t = ds["tas"].sel(time=slice("2020-07-01", "2020-07-31T23:59:59"))
    return [float(t.astype("f8").mean("time").mean().values)]


def timed(cmd, log, stdout=None):
    """Runs cmd; returns (returncode, wall seconds, max RSS in MB)."""
    tf = log + ".time"
    full = ["/usr/bin/time", "-f", "%e %M", "-o", tf] + cmd
    with open(log, "w") as lf:
        lf.write(" ".join(cmd) + "\n")
        lf.flush()
        r = subprocess.run(full, stdout=stdout or lf, stderr=lf)
    try:
        wall, rss = open(tf).read().split()[-2:]
        return r.returncode, float(wall), int(rss) / 1024
    except (OSError, ValueError):
        return r.returncode, float("nan"), float("nan")


def data_vars(ds):
    skip = set(ds.dimensions)
    for v in ds.variables.values():
        for a in ("bounds", "coordinates"):
            if a in v.ncattrs():
                skip.update(str(v.getncattr(a)).split())
    return [n for n, v in ds.variables.items() if n not in skip and v.ndim > 0 and n != "time_bnds"]


def coord_vars(ds):
    names = set()
    for n in data_vars(ds):
        v = ds.variables[n]
        if "coordinates" in v.ncattrs():
            names.update(str(v.getncattr("coordinates")).split())
        names.update(d for d in v.dimensions if d in ds.variables and d != "time")
    return sorted(n for n in names if n in ds.variables)


def values(v):
    x = v[:]
    return np.ma.filled(np.ma.asarray(x).astype("f8"), np.nan)


def timestamps(ds):
    if "time" not in ds.variables:
        return []
    t = ds.variables["time"]
    cal = getattr(t, "calendar", "standard")
    return [str(d) for d in netCDF4.num2date(t[:], t.units, cal)]


def compare_nc(ref, tst, mode, times, binfiles=None):
    """Returns (status, note)."""
    a, b = netCDF4.Dataset(ref), netCDF4.Dataset(tst)
    notes, worst = [], "identical"

    def degrade(s):
        nonlocal worst
        order = ["identical", "within tolerance", "DIFFERENT"]
        if order.index(s) > order.index(worst):
            worst = s

    names = data_vars(a)
    missing = [n for n in names if n not in b.variables]
    if missing:
        degrade("DIFFERENT")
        notes.append(f"missing variables {missing}")
    maxdiff = 0.0
    for n in [n for n in names if n in b.variables]:
        x, y = values(a.variables[n]), values(b.variables[n])
        if x.shape != y.shape:
            degrade("DIFFERENT")
            notes.append(f"{n}: shape {x.shape} vs {y.shape}")
            continue
        nan = np.isnan(x) != np.isnan(y)
        if nan.any():
            degrade("DIFFERENT")
            notes.append(f"{n}: missing pattern differs in {int(nan.sum())} values")
        ok = ~np.isnan(x) & ~np.isnan(y)
        d = np.abs(x[ok] - y[ok])
        if d.size == 0 or d.max() == 0:
            continue
        maxdiff = max(maxdiff, float(d.max()))
        if mode == "exact":
            lim = np.zeros_like(d)
        elif mode.startswith("abs:"):
            lim = np.full_like(d, float(mode[4:]))
        elif mode == "bin" and binfiles:
            lo = values(netCDF4.Dataset(binfiles[0]).variables[n])
            hi = values(netCDF4.Dataset(binfiles[1]).variables[n])
            lim = ((hi - lo) / 101.0 * 1.0001)[ok] if lo.shape == x.shape else np.full_like(d, np.inf)
        else:
            top = np.nanmax(np.abs(x)) if np.isfinite(x).any() else 0.0
            lim = np.full_like(d, 2 * 2.0**-24 * top)
        bad = d > lim
        if bad.any():
            degrade("DIFFERENT")
            notes.append(f"{n}: {int(bad.sum())} values beyond tolerance")
        else:
            degrade("within tolerance")
        notes.append(f"{n}: {int((d > 0).sum())} values differ, max|diff| {d.max():.3g}")
    for n in coord_vars(a):
        if n not in b.variables:
            degrade("DIFFERENT")
            notes.append(f"coordinate {n} missing")
            continue
        x, y = values(a.variables[n]), values(b.variables[n])
        if x.shape != y.shape:
            degrade("DIFFERENT")
            notes.append(f"coordinate {n}: shape {x.shape} vs {y.shape}")
        elif not np.array_equal(x, y, equal_nan=True):
            d = np.nanmax(np.abs(x - y))
            if d > 1e-6 * max(1.0, np.nanmax(np.abs(x))):
                degrade("DIFFERENT")
            else:
                degrade("within tolerance")
            notes.append(f"coordinate {n}: max|diff| {d:.3g}")
    if times:
        ta, tb = timestamps(a), timestamps(b)
        if ta != tb:
            degrade("DIFFERENT")
            notes.append(f"timestamps differ ({len(ta)} vs {len(tb)} steps)")
    if not notes:
        ncell = sum(values(a.variables[n]).size for n in names)
        notes.append(f"{len(names)} variable(s), {ncell} values")
    return worst, "; ".join(notes)


def compare_text(ref, tst):
    a = [l.rstrip() for l in open(ref).read().splitlines()]
    b = [l.rstrip() for l in open(tst).read().splitlines()]
    if a == b:
        return "identical", f"{len(a)} lines"
    diff = [i for i in range(max(len(a), len(b)))
            if i >= len(a) or i >= len(b) or a[i] != b[i]]
    i = diff[0]
    return "DIFFERENT", (f"{len(diff)} of {len(a)} lines differ; first (line {i + 1}): "
                         f"cdo '{a[i] if i < len(a) else ''}' cdors '{b[i] if i < len(b) else ''}'")


def run_case(c, d, cdo_threads):
    out = {"id": c["id"], "what": c["what"], "cdors_s": None, "ref_s": None, "rss_mb": None}
    mode = c.get("mode", "ulp")
    cdir = os.path.join(d, c["id"])
    os.makedirs(cdir)
    if mode == "plan":
        rc, wall, rss = timed([CDORS, "--plan"] + c["cdors"], f"{cdir}/cdors.log")
        out.update(cdors_s=wall, rss_mb=rss)
        out.update(status="ok" if rc == 0 else "error",
                   note="plan only" if rc == 0 else f"see {cdir}/cdors.log")
        return out
    if mode == "mem":
        pj = f"{cdir}/plan.json"
        with open(pj, "w") as f:
            rc = subprocess.run([CDORS, "--plan", "--json"] + c["cdors"], stdout=f,
                                stderr=subprocess.STDOUT).returncode
        txt = open(pj).read()
        if rc != 0:
            ok = "memory_limit" in txt
            out.update(status="ok" if ok else "error",
                       note="refused with memory_limit" if ok else f"see {pj}")
            return out
        m = json.loads(txt)["memory"]
        peak, budget = m["peak_bytes_estimate"], m["budget_bytes"]
        rc, wall, rss = timed([CDORS] + c["cdors"] + [f"{cdir}/out.nc"], f"{cdir}/cdors.log")
        out.update(cdors_s=wall, rss_mb=rss)
        if rc != 0:
            out.update(status="error", note=f"see {cdir}/cdors.log")
        elif peak > budget:
            out.update(status="DIFFERENT", note=f"estimate {peak / 1e6:.0f} MB over budget "
                                                f"{budget / 1e6:.0f} MB, run not refused")
        else:
            out.update(status="ok", note=f"estimate {peak / 1e6:.0f} MB of {budget / 1e6:.0f} MB; "
                                         f"RSS {rss:.0f} MB (also counts remap weights, libraries)")
        return out
    text = mode == "text" or mode.startswith("num:")
    tst = f"{cdir}/cdors.{'txt' if text else 'nc'}"
    if text:
        with open(tst, "w") as f:
            rc, wall, rss = timed([CDORS] + c["cdors"], f"{cdir}/cdors.log", stdout=f)
    else:
        rc, wall, rss = timed([CDORS] + c["cdors"] + [tst], f"{cdir}/cdors.log")
    out.update(cdors_s=wall, rss_mb=rss)
    if rc != 0:
        out.update(status="error", note=f"cdors failed, see {cdir}/cdors.log")
        return out
    if "py" in c:
        t0 = time.time()
        try:
            ref = globals()[c["py"]]()
        except Exception as e:  # report, do not stop the sweep
            out.update(status="error", note=f"reference failed: {e}")
            return out
        out["ref_s"] = time.time() - t0
        got = [float(x) for x in open(tst).read().split()]
        rel = float(mode[4:])
        if len(got) != len(ref):
            out.update(status="DIFFERENT", note=f"{len(got)} vs {len(ref)} numbers")
        else:
            d = max(abs(g - r) / max(abs(r), 1e-300) for g, r in zip(got, ref))
            st = "identical" if got == ref else ("within tolerance" if d <= rel else "DIFFERENT")
            out.update(status=st, note=f"cdors {got[:3]} numpy {[f'{r:.9g}' for r in ref[:3]]}; "
                                       f"max rel diff {d:.2g}")
        return out
    ref = f"{cdir}/cdo.{'txt' if text else 'nc'}"
    cdo = [CDO, "-s"] if text else [CDO, "-s", "-f", "nc4", "-P", str(cdo_threads), "--no_history"]
    if text:
        with open(ref, "w") as f:
            rc, rwall, _ = timed(cdo + c["cdo"], f"{cdir}/cdo.log", stdout=f)
    else:
        rc, rwall, _ = timed(cdo + c["cdo"] + [ref], f"{cdir}/cdo.log")
    out["ref_s"] = rwall
    if rc != 0:
        out.update(status="error", note=f"cdo failed, see {cdir}/cdo.log")
        return out
    if text:
        st, note = compare_text(ref, tst)
    else:
        binfiles = None
        if mode == "bin":
            binfiles = [f"{cdir}/min.nc", f"{cdir}/max.nc"]
            for op, f in zip(("-timmin", "-timmax"), binfiles):
                subprocess.run([CDORS, op] + c["bin_chain"] + [f], stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL)
        st, note = compare_nc(ref, tst, mode, c.get("times", True), binfiles)
    out.update(status=st, note=note)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--run-dir", required=True)
    ap.add_argument("--only", help="comma-separated case ids")
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--skip-remote", action="store_true")
    ap.add_argument("--cdo-threads", type=int, default=8)
    a = ap.parse_args()
    if a.list:
        for c in CASES:
            print(f"{c['id']:22} {c['what']}")
        return 0
    only = set(a.only.split(",")) if a.only else None
    os.makedirs(a.run_dir)  # fresh: fails if it exists
    rows = []
    print(f"run directory: {a.run_dir}")
    print(f"cdors: {CDORS}\ncdo:   {CDO}\n")
    hdr = f"{'case':22} {'status':17} {'cdors s':>8} {'ref s':>8} {'RSS MB':>7}  note"
    print(hdr)
    print("-" * len(hdr))
    for c in CASES:
        if only and c["id"] not in only:
            continue
        if (c.get("remote") and a.skip_remote) or not all(
                p and os.path.exists(p) for p in c.get("needs", [])):
            r = {"id": c["id"], "what": c["what"], "status": "skipped",
                 "note": "remote" if c.get("remote") and a.skip_remote else "input not found",
                 "cdors_s": None, "ref_s": None, "rss_mb": None}
        else:
            try:
                r = run_case(c, a.run_dir, a.cdo_threads)
            except Exception as e:  # report, keep going
                r = {"id": c["id"], "what": c["what"], "status": "error", "note": repr(e),
                     "cdors_s": None, "ref_s": None, "rss_mb": None}
        rows.append(r)
        f = lambda x, p: "-" if x is None or x != x else f"{x:.{p}f}"
        line = (f"{r['id']:22} {r['status']:17} {f(r['cdors_s'], 2):>8} {f(r['ref_s'], 2):>8} "
                f"{f(r['rss_mb'], 0):>7}  {r['note']}")
        print(line, flush=True)
        with open(os.path.join(a.run_dir, "summary.txt"), "a") as s:
            s.write(line + "\n")
    with open(os.path.join(a.run_dir, "summary.json"), "w") as s:
        json.dump({"date": datetime.datetime.now().isoformat(timespec="seconds"),
                   "cdors": CDORS, "cdo": CDO, "cases": rows}, s, indent=1)
    counts = {}
    for r in rows:
        counts[r["status"]] = counts.get(r["status"], 0) + 1
    print("\n" + ", ".join(f"{v} {k}" for k, v in counts.items()))
    return 0


if __name__ == "__main__":
    sys.exit(main())
