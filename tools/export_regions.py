"""Export region sets to the GeoJSON files cdors reads (`regmask`, `maskregion,regions=`).

    <aqua env>/bin/python -I tools/export_regions.py REGIONS_DIR [--embedded DIR | --no-embedded]
        [--cache-dir DIR] [--nereus-data DIR]

- the small sets go to --embedded (default: crates/cdors-mesh/data/regions of this repository),
  which cdors compiles into the binary: ar6.all (with its subsets ar6.land and ar6.ocean: features
  list the subsets they belong to in `subsets`, the collection gives each subset's overlap flag),
  srex, giorgi, prudence (regionmask 0.13) and nino (the four Nino boxes, defined here);
- the large sets go to REGIONS_DIR, unsimplified, for CDORS_REGIONS or the install's
  share/regions/ (never into the binary): Natural Earth v5.1.2 countries_50, countries_110,
  land_50, land_110, ocean_basins_50 (as ne.<set>), and nereus' basins and moc (MPAS
  geometric_features, IHO seas via marineregions.org).

One FeatureCollection per set. Every feature has the properties `number`, `name` and `abbrev`
(nereus sets: `name`, `abbrev` = name, plus `author` and `constituents`; their `history` is
dropped) and a Polygon or MultiPolygon geometry with the coordinates exactly as the source has
them. The collection carries a foreign member `cdors` with the set's name, its overlap flag
(regionmask's `overlap`; nereus and nino: true, every region on its own), source, licence and
attribution text.

regionmask downloads AR6 and Natural Earth through pooch into --cache-dir (default
/work/ab0995/a270088/cdors-cache/regionmask), never into $HOME. Files are written to a temporary
name and renamed; existing files are replaced.
"""
import argparse
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
EMBEDDED = os.path.join(HERE, "..", "crates", "cdors-mesh", "data", "regions")
NEREUS = os.path.expanduser("~/PYTHON/nereus/src/nereus/diag/data")

CC_BY_AR6 = ("CC-BY 4.0", "AR6 reference regions: Iturbide et al. (2020), An update of IPCC climate "
             "reference regions for subcontinental analysis of climate model data, Earth Syst. Sci. "
             "Data 12, 2959-2970, doi:10.5194/essd-12-2959-2020 (CC-BY 4.0); via regionmask (MIT)")
REGIONMASK = ("MIT (regionmask); coordinates from the cited publication",
              "via regionmask (Hauser et al., MIT licence)")
NE = ("public domain", "Made with Natural Earth (naturalearthdata.com), v5.1.2; via regionmask")
MPAS = ("CC-BY 4.0", "MPAS geometric_features (github.com/MPAS-Dev/geometric_features), from the "
        "IHO Limits of Oceans and Seas via marineregions.org (CC-BY 4.0); via nereus")

# the Nino boxes (NOAA CPC definitions), in -180..180 like nereus' NinoRegions
NINO = [
    ("Nino 1+2", "nino12", [(-90.0, -10.0), (-80.0, -10.0), (-80.0, 0.0), (-90.0, 0.0)]),
    ("Nino 3", "nino3", [(-150.0, -5.0), (-90.0, -5.0), (-90.0, 5.0), (-150.0, 5.0)]),
    ("Nino 3.4", "nino34", [(-170.0, -5.0), (-120.0, -5.0), (-120.0, 5.0), (-170.0, 5.0)]),
    ("Nino 4", "nino4", [[(160.0, -5.0), (180.0, -5.0), (180.0, 5.0), (160.0, 5.0)],
                         [(-180.0, -5.0), (-150.0, -5.0), (-150.0, 5.0), (-180.0, 5.0)]]),
]


def write(path, name, overlap, source, licence, attribution, features, subsets=None):
    meta = {"name": name, "overlap": bool(overlap), "source": source, "license": licence,
            "attribution": attribution}
    if subsets:
        meta["subsets"] = subsets
    fc = {"type": "FeatureCollection", "cdors": meta, "features": features}
    tmp = f"{path}.tmp{os.getpid()}"
    with open(tmp, "w") as f:
        # repr floats: the shortest text that reads back to the same double
        json.dump(fc, f, separators=(",", ":"), allow_nan=False)
        f.write("\n")
    os.replace(tmp, path)
    print(f"{path}: {len(features)} regions, {os.path.getsize(path)} bytes, overlap={bool(overlap)}")


def from_regionmask(rs):
    from shapely.geometry import mapping
    out = []
    for r in rs.regions.values():
        g = mapping(r.polygon)
        out.append({"type": "Feature",
                    "properties": {"number": int(r.number), "name": str(r.name), "abbrev": str(r.abbrev)},
                    "geometry": {"type": g["type"], "coordinates": json.loads(json.dumps(g["coordinates"]))}})
    return out


def box(ring):
    return [[list(p) for p in ring] + [list(ring[0])]]


def nino_features():
    out = []
    for i, (name, abbrev, rings) in enumerate(NINO):
        if isinstance(rings[0], tuple):
            geom = {"type": "Polygon", "coordinates": box(rings)}
        else:
            geom = {"type": "MultiPolygon", "coordinates": [box(r) for r in rings]}
        out.append({"type": "Feature", "properties": {"number": i, "name": name, "abbrev": abbrev},
                    "geometry": geom})
    return out


def nereus_features(path):
    with open(path) as f:
        d = json.load(f)
    out = []
    for i, ft in enumerate(d["features"]):
        p = ft["properties"]
        props = {"number": i, "name": p["name"], "abbrev": p["name"]}
        for k in ("author", "constituents"):   # `history` holds a host name and paths: dropped
            if k in p:
                props[k] = p[k]
        out.append({"type": "Feature", "properties": props, "geometry": ft["geometry"]})
    return out


def embedded(out, dr, rm_ver):
    """The sets compiled into cdors."""
    # ar6.land and ar6.ocean are subsets of ar6.all (same numbers, identical polygons; three
    # regions are in both): one file, each feature listing the subsets it belongs to
    ar6 = dr.ar6
    feats = from_regionmask(ar6.all)
    subsets = {}
    for key, rs in (("ar6.land", ar6.land), ("ar6.ocean", ar6.ocean)):
        nums = {int(n) for n in rs.numbers}
        for f in feats:
            n = f["properties"]["number"]
            if n in nums:
                assert rs[n].polygon.equals_exact(ar6.all[n].polygon, 0)
                f["properties"].setdefault("subsets", []).append(key)
        subsets[key] = {"overlap": bool(rs.overlap)}
    write(os.path.join(out, "ar6.all.geojson"), "ar6.all", ar6.all.overlap,
          f"{ar6.all.source} ({rm_ver})", *CC_BY_AR6, feats, subsets)
    for key in ("srex", "giorgi", "prudence"):
        rs = getattr(dr, key)
        write(os.path.join(out, f"{key}.geojson"), key, rs.overlap, f"{rs.source} ({rm_ver})",
              REGIONMASK[0], f"{rs.source}; {REGIONMASK[1]}", from_regionmask(rs))
    write(os.path.join(out, "nino.geojson"), "nino", True,
          "NOAA CPC Nino index regions (box definitions)", "public domain (box definitions)",
          "Nino regions: NOAA Climate Prediction Center box definitions", nino_features())


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("regions_dir")
    ap.add_argument("--embedded", default=EMBEDDED)
    ap.add_argument("--no-embedded", action="store_true", help="write only REGIONS_DIR")
    ap.add_argument("--cache-dir", default="/work/ab0995/a270088/cdors-cache/regionmask")
    ap.add_argument("--nereus-data", default=NEREUS)
    a = ap.parse_args()
    # pyproj (geopandas) finds its database only through PROJ_DATA
    os.environ.setdefault("PROJ_DATA", os.path.join(sys.prefix, "share", "proj"))
    import regionmask
    os.makedirs(a.cache_dir, exist_ok=True)
    regionmask.set_options(cache_dir=a.cache_dir)
    dr = regionmask.defined_regions
    os.makedirs(a.regions_dir, exist_ok=True)
    rm_ver = f"regionmask {regionmask.__version__}"
    if not a.no_embedded:
        os.makedirs(a.embedded, exist_ok=True)
        embedded(a.embedded, dr, rm_ver)

    ne = dr.natural_earth_v5_1_2
    for key in ("countries_50", "countries_110", "land_50", "land_110", "ocean_basins_50"):
        rs = getattr(ne, key)
        write(os.path.join(a.regions_dir, f"ne.{key}.geojson"), f"ne.{key}", rs.overlap,
              f"{rs.source} ({rm_ver})", *NE, from_regionmask(rs))
    for key, fname in (("basins", "oceanBasins"), ("moc", "MOCBasins")):
        write(os.path.join(a.regions_dir, f"{key}.geojson"), key, True,
              f"nereus diag/data/{fname}.geojson (MPAS geometric_features)", *MPAS,
              nereus_features(os.path.join(a.nereus_data, f"{fname}.geojson")))


if __name__ == "__main__":
    sys.exit(main())
