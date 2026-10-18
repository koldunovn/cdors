//! `cdors mesh info <spec> [--json]`: what cdors sees of a mesh.
//!
//! `<spec>` is a mesh spec (`fesom:<dir>[,units=deg|rad]`, loaded through the mesh cache), a
//! data file or store (the one-place view of the grid of its first data variable), or a grid
//! name (`r36x18`, `hpz2`, ...). Lazy components (per-level areas, tables) are reported with
//! their sizes but not loaded.

use cdors_core::error::{Error, ErrorCode, Result};
use cdors_core::io::ChunkSource;
use cdors_core::model::mesh_cache::{self, Loaded};
use cdors_mesh::Mesh;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::sync::Arc;

const USAGE: &str = "usage: cdors mesh info <spec> [--json]
  <spec>: fesom:<mesh dir>[,units=deg|rad], a data file or store, or a grid name (r36x18, hpz2)";

pub fn run(args: &[String], json: bool) -> Result<String> {
    let words: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with("--"))
        .collect();
    match words.as_slice() {
        ["info", spec] => info(spec, json),
        _ => Err(
            Error::bad_arguments(format!("mesh: expected `info <spec>`, got {words:?}"))
                .with_hint(USAGE),
        ),
    }
}

/// The mesh of a spec, file or grid name, and its cache entry (meshes of grids have none).
fn resolve(spec: &str) -> Result<Loaded> {
    if let Some(s) = mesh_cache::parse_spec(spec) {
        return mesh_cache::load(&s?);
    }
    let src: Arc<dyn ChunkSource> = if std::path::Path::new(spec).exists() {
        cdors_core::io::open(spec)?
    } else if let Some(g) = cdors_core::remap::target::from_name(spec) {
        Arc::new(g)
    } else {
        return Err(Error::new(
            ErrorCode::MissingInput,
            format!("mesh info: '{spec}' is no mesh spec, file or grid name"),
        )
        .with_hint(USAGE));
    };
    let ds = src.dataset();
    let var = ds.data_vars().find(|v| v.grid.is_some()).ok_or_else(|| {
        Error::new(
            ErrorCode::NoCoordinates,
            format!("'{spec}' has no data variable on a horizontal grid"),
        )
    })?;
    let grid = &ds.grids[var.grid.expect("grid")];
    let mesh = grid.mesh_view(ds, var, &|v| src.read_var(v))?;
    Ok(Loaded {
        mesh,
        spec: spec.to_owned(),
        entry: None,
        built: false,
        pending: false,
    })
}

fn range(v: &[f64]) -> Value {
    let (lo, hi) = v
        .iter()
        .filter(|x| x.is_finite())
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &x| {
            (a.min(x), b.max(x))
        });
    if lo <= hi {
        json!([lo, hi])
    } else {
        Value::Null
    }
}

fn describe(l: &Loaded) -> Value {
    let m: &Mesh = &l.mesh;
    let vert = m.vertical.as_ref();
    let places: Vec<Value> = m
        .places
        .iter()
        .map(|p| {
            let wet = vert
                .and_then(|v| v.wet(&p.name))
                .and_then(|w| w.depth_range());
            json!({
                "name": p.name,
                "size": p.size,
                "area_sum": p.area.iter().sum::<f64>(),
                "lon_range": range(&p.lon),
                "lat_range": range(&p.lat),
                "area_per_level": p.area_per_level.as_ref().map(|a| json!({
                    "levels": a.nlev(),
                    "bytes": a.bytes(),
                    "loaded_bytes": a.loaded_bytes(),
                })),
                "polygons": p.polygons.as_ref().map(|q| json!({"bytes": q.bytes()})),
                "wet_levels": wet.map(|(lo, hi)| json!({"min": lo, "max": hi})),
            })
        })
        .collect();
    let topology: Vec<Value> = m
        .topology
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "rows": t.rows_place,
                "cols": t.cols_place,
                "bytes": t.csr.bytes(),
                "loaded": t.csr.is_loaded(),
            })
        })
        .collect();
    let vertical = vert.map(|v| {
        json!({
            "levels": v.nlev(),
            "interfaces": range(&v.interfaces),
            "mid": range(&v.mid),
            "thickness": "static",
        })
    });
    let (lazy, lazy_loaded) = m.lazy_bytes();
    let native = cdors_mesh::models::model(&m.source.model).map(|md| md.native.to_vec());
    json!({
        "spec": l.spec,
        "mesh": m.name,
        "model": m.source.model,
        "places": places,
        "topology": topology,
        "vertical": vertical,
        "units": m.source.units,
        "notes": m.source.notes,
        "native": native,
        "source_files": m.source.files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>(),
        "cache": l.entry.as_ref().map(|e| json!({"path": e.display().to_string(), "built": l.built})),
        "memory": {
            "resident_bytes": m.resident_bytes(),
            "lazy_bytes": lazy,
            "lazy_loaded_bytes": lazy_loaded,
        },
    })
}

fn info(spec: &str, json: bool) -> Result<String> {
    let l = resolve(spec)?;
    let v = describe(&l);
    if json {
        return Ok(format!("{v}\n"));
    }
    let mut s = String::new();
    let _ = writeln!(
        s,
        "mesh {} ({})",
        v["mesh"].as_str().unwrap_or(""),
        v["model"].as_str().unwrap_or("")
    );
    for p in v["places"].as_array().into_iter().flatten() {
        let _ = write!(
            s,
            "  place {:<9} {:>10} cells, area sum {:.6e}",
            p["name"].as_str().unwrap_or(""),
            p["size"],
            p["area_sum"].as_f64().unwrap_or(f64::NAN)
        );
        if let Some(w) = p["wet_levels"].as_object() {
            let _ = write!(s, ", wet levels {}..{}", w["min"], w["max"]);
        }
        if let Some(a) = p["area_per_level"].as_object() {
            let _ = write!(
                s,
                ", areas per level ({} levels, {} B, lazy)",
                a["levels"], a["bytes"]
            );
        }
        s.push('\n');
    }
    for t in v["topology"].as_array().into_iter().flatten() {
        let _ = writeln!(
            s,
            "  table {} ({} -> {}), {} B, lazy",
            t["name"].as_str().unwrap_or(""),
            t["rows"].as_str().unwrap_or(""),
            t["cols"].as_str().unwrap_or(""),
            t["bytes"]
        );
    }
    if let Some(vt) = v["vertical"].as_object() {
        let _ = writeln!(
            s,
            "  vertical: {} levels, interfaces {}, full layers",
            vt["levels"], vt["interfaces"]
        );
    }
    if let Some(u) = v["units"].as_str() {
        let _ = writeln!(s, "  units: {u}");
    }
    for n in v["notes"].as_array().into_iter().flatten() {
        let _ = writeln!(s, "  note: {}", n.as_str().unwrap_or(""));
    }
    for f in v["source_files"].as_array().into_iter().flatten() {
        let _ = writeln!(s, "  source: {}", f.as_str().unwrap_or(""));
    }
    if let Some(c) = v["cache"].as_object() {
        let _ = writeln!(
            s,
            "  cache: {}{}",
            c["path"].as_str().unwrap_or(""),
            if c["built"] == json!(true) {
                " (built now)"
            } else {
                ""
            }
        );
    }
    let mem = &v["memory"];
    let _ = writeln!(
        s,
        "  memory: {} B resident, {} B lazy ({} B loaded)",
        mem["resident_bytes"], mem["lazy_bytes"], mem["lazy_loaded_bytes"]
    );
    Ok(s)
}
