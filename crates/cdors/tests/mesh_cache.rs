//! The mesh cache (`$CDORS_CACHE/meshes`) across processes, on the FESOM2 pi mesh
//! (`$CDORS_FIXTURES/fesom_pi`, else the pool copy): two concurrent `cdors mesh info` runs on a
//! fresh cache both succeed and leave one entry; an entry whose fingerprint does not match is
//! set aside and rebuilt; per-level areas and the element table read back lazily from the entry
//! equal the source file's. Each test uses its own new cache directory under the cargo target
//! directory (left in place).

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn pi_dir() -> Option<PathBuf> {
    let fx = std::env::var_os("CDORS_FIXTURES").map(|d| PathBuf::from(d).join("fesom_pi"));
    let pool = PathBuf::from("/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/pi");
    let d = fx
        .filter(|d| d.exists())
        .or(Some(pool).filter(|d| d.exists()));
    if d.is_none() {
        eprintln!("skipped: no FESOM2 pi mesh (CDORS_FIXTURES/fesom_pi or /pool)");
    }
    d
}

fn fresh_cache(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("mesh-cache-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn info(cache: &Path, spec: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_cdors"));
    c.args(["--json", "mesh", "info", spec])
        .env("CDORS_CACHE", cache)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}

fn json(o: &Output) -> serde_json::Value {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}

fn entries(cache: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(cache.join("meshes"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn concurrent_runs_leave_one_entry() {
    let Some(pi) = pi_dir() else { return };
    let cache = fresh_cache("race");
    let spec = format!("fesom:{}", pi.display());
    let kids: Vec<_> = (0..2)
        .map(|_| info(&cache, &spec).spawn().unwrap())
        .collect();
    let outs: Vec<serde_json::Value> = kids
        .into_iter()
        .map(|c| json(&c.wait_with_output().unwrap()))
        .collect();
    // exactly one rename wins; the other run used the winner's entry
    let built: Vec<bool> = outs.iter().map(|v| v["cache"]["built"] == true).collect();
    assert_eq!(built.iter().filter(|&&b| b).count(), 1, "{built:?}");
    assert_eq!(outs[0]["places"], outs[1]["places"]);
    assert!(outs.iter().all(|v| v["memory"]["lazy_loaded_bytes"] == 0));
    let e = entries(&cache);
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(e[0].ends_with(".zarr"), "{e:?}");
}

#[test]
fn bad_entry_is_set_aside() {
    let Some(pi) = pi_dir() else { return };
    let cache = fresh_cache("bad");
    let spec = format!("fesom:{}", pi.display());
    let first = json(&info(&cache, &spec).output().unwrap());
    let entry = PathBuf::from(first["cache"]["path"].as_str().unwrap());
    std::fs::write(entry.join("cdors_complete.json"), r#"{"fingerprint": "0"}"#).unwrap();
    let o = info(&cache, &spec).output().unwrap();
    let again = json(&o);
    assert_eq!(again["cache"]["built"], true);
    assert!(String::from_utf8_lossy(&o.stderr).contains("mesh_cache_rebuilt"));
    let e = entries(&cache);
    assert_eq!(e.len(), 2, "{e:?}");
    assert!(e.iter().any(|n| n.contains(".zarr.bad-")), "{e:?}");
}

#[test]
fn lazy_parts_read_back_from_the_entry() {
    let Some(pi) = pi_dir() else { return };
    let cache = fresh_cache("lazy");
    // SAFETY: the other tests of this binary pass CDORS_CACHE to their child processes
    // explicitly and read no environment themselves.
    unsafe { std::env::set_var("CDORS_CACHE", &cache) };
    let spec = cdors_core::model::mesh_cache::parse_spec(&format!("fesom:{}", pi.display()))
        .unwrap()
        .unwrap();
    let l = cdors_core::model::mesh_cache::load(&spec).unwrap();
    assert!(l.built && l.entry.is_some());
    let m = &l.mesh;
    assert_eq!(m.lazy_bytes().1, 0);
    let file = pi.join("fesom.mesh.diag.nc");
    let src = cdors_core::io::mesh_reader::SourceReader::open(file.to_str().unwrap()).unwrap();
    let nodes = m.place("nodes").unwrap();
    let levels = nodes.area_per_level.as_ref().unwrap();
    for k in [0, 10, levels.nlev() - 1] {
        let want = src
            .read_range("nod_area", &[k..k + 1, 0..nodes.size])
            .unwrap();
        assert_eq!(*levels.level(k).unwrap(), want, "level {k}");
    }
    assert_eq!(m.lazy_bytes().1, 3 * 8 * nodes.size as u64);
    let en = m.table("elem_nodes").unwrap().csr.get().unwrap();
    let elem = src.read("elem").unwrap();
    let ne = en.rows();
    for e in [0, ne / 2, ne - 1] {
        let want: Vec<u32> = (0..3).map(|k| elem[k * ne + e] as u32 - 1).collect();
        assert_eq!(en.row(e), want.as_slice(), "element {e}");
    }
}
