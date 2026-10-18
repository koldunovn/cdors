//! The mesh cache: model meshes loaded once, kept as UGRID-Zarr stores in
//! `$CDORS_CACHE/meshes/<key>.zarr`.
//!
//! - **Key.** FNV-128 over a loader-version salt, the model, the spec options and, for each source
//!   file, its real path, size and mtime: nothing is hashed on a cache hit but these.
//! - **Fingerprint.** The entry stores FNV-128 of the first and last MiB of each source file in its
//!   completion marker (`cdors_complete.json`) and as the global attribute `cdors_fingerprint`; it
//!   is checked on every load (2 MiB read per file), so a file rewritten with the same size and
//!   mtime is still noticed.
//! - **Concurrency.** An entry is built in `<key>.zarr.tmp-<host>-<pid>-<random>`, gets its
//!   marker, and is renamed to `<key>.zarr`; the first rename wins. A process that loses removes
//!   only its own temporary directory (the one thing this module ever deletes) and opens the
//!   winner's entry. An entry without a marker or with another fingerprint is renamed aside to
//!   `<key>.zarr.bad-<unix time>-<pid>` (never deleted) and rebuilt.
//! - **Laziness.** The mesh is read back from the entry: centres, 2-D areas and wet levels now;
//!   per-level areas (one level per read) and tables on first use. Building streams the
//!   per-level areas through one level at a time.
//!
//! Without `CDORS_CACHE` meshes are loaded from their source files on every run. Loaded meshes
//! are also kept for the rest of the process (keyed like the cache), so planning passes share
//! one mesh.

use crate::error::{Error, ErrorCode, Result};
use crate::io::mesh_reader::SourceReader;
use crate::remap::generate::Fnv128;
use cdors_mesh::models::MeshSpec;
use cdors_mesh::ugrid::{self, ArraySpec, Kind, UgridSink};
use cdors_mesh::{ArrayReader, Attr, Mesh, MeshError};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use zarrs::array::{Array, ArrayMetadata, ArrayMetadataV3};
use zarrs::filesystem::FilesystemStore;
use zarrs::storage::{ReadableWritableStorageTraits, StoreKey, WritableStorageTraits};

/// Raised when the cache layout (not a loader) changes.
const CACHE_SALT: &str = "cdors-mesh-cache-v1";
const MARKER: &str = "cdors_complete.json";
const MIB: u64 = 1 << 20;

/// A loaded mesh and where it came from.
#[derive(Clone)]
pub struct Loaded {
    pub mesh: Arc<Mesh>,
    pub spec: String,
    /// The cache entry (`None` without `CDORS_CACHE`).
    pub entry: Option<PathBuf>,
    /// Whether this process built the entry.
    pub built: bool,
}

static MEMO: Mutex<Option<HashMap<String, Loaded>>> = Mutex::new(None);

/// A cdors error for a mesh loading or geometry failure.
pub fn mesh_error(e: MeshError, what: &str) -> Error {
    Error::new(ErrorCode::BadData, format!("{what}: {e}"))
}

/// Parses a mesh spec (`fesom:<dir>[,units=deg|rad]`); `None` if `s` names no model.
pub fn parse_spec(s: &str) -> Option<Result<MeshSpec>> {
    cdors_mesh::models::parse_spec(s).map(|r| {
        r.map_err(|e| {
            Error::bad_arguments(e.to_string()).with_hint(
                "a mesh spec is <model>:<mesh directory>[,<option>=<value>], e.g. \
                 fesom:/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/pi or fesom:<dir>,units=deg",
            )
        })
    })
}

/// The source files of a spec, checked to exist.
fn source_files(spec: &MeshSpec) -> Result<Vec<PathBuf>> {
    let files = spec.files();
    for f in &files {
        if !f.is_file() {
            return Err(Error::new(
                ErrorCode::MissingInput,
                format!("mesh {}: '{}' does not exist", spec.text(), f.display()),
            )
            .with("path", f.display().to_string())
            .with_hint(format!(
                "a {} mesh directory holds {}",
                spec.model.name,
                spec.model.files.join(", ")
            )));
        }
    }
    Ok(files)
}

fn io_err(path: &Path, e: std::io::Error) -> Error {
    Error::from(e).with("path", path.display().to_string())
}

/// Cache key: salt, loader version, model, options, and per file real path, size, mtime.
fn key(spec: &MeshSpec, files: &[PathBuf]) -> Result<String> {
    let mut h = Fnv128::new();
    h.update(CACHE_SALT.as_bytes());
    h.update(format!("\0{}\0{}\0", spec.model.name, spec.model.version).as_bytes());
    for (k, v) in &spec.options {
        h.update(format!("{k}={v}\0").as_bytes());
    }
    for f in files {
        let real = std::fs::canonicalize(f).map_err(|e| io_err(f, e))?;
        let md = std::fs::metadata(&real).map_err(|e| io_err(f, e))?;
        h.update(real.as_os_str().as_encoded_bytes());
        h.update(format!("\0{}\0{}.{}\0", md.len(), md.mtime(), md.mtime_nsec()).as_bytes());
    }
    Ok(format!("{}-{:032x}", spec.model.name, h.finish()))
}

/// FNV-128 of the first and last MiB of each file.
fn fingerprint(files: &[PathBuf]) -> Result<String> {
    let mut h = Fnv128::new();
    for f in files {
        let file = std::fs::File::open(f).map_err(|e| io_err(f, e))?;
        let len = file.metadata().map_err(|e| io_err(f, e))?.len();
        let mut buf = vec![0u8; len.min(MIB) as usize];
        file.read_exact_at(&mut buf, 0).map_err(|e| io_err(f, e))?;
        h.update(&buf);
        let tail = len.saturating_sub(MIB).max(MIB.min(len));
        let mut buf = vec![0u8; (len - tail) as usize];
        file.read_exact_at(&mut buf, tail)
            .map_err(|e| io_err(f, e))?;
        h.update(&buf);
        h.update(format!("\0{len}\0").as_bytes());
    }
    Ok(format!("{:032x}", h.finish()))
}

/// Loads the mesh of `spec`: from this process's memo, from the cache entry, or built from the
/// source files (and cached).
pub fn load(spec: &MeshSpec) -> Result<Loaded> {
    let files = source_files(spec)?;
    let key = key(spec, &files)?;
    if let Some(l) = MEMO
        .lock()
        .ok()
        .and_then(|m| m.as_ref()?.get(&key).cloned())
    {
        return Ok(l);
    }
    let dir = std::env::var_os("CDORS_CACHE")
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join("meshes"));
    let loaded = match dir {
        None => Loaded {
            mesh: Arc::new(load_source(spec, &files)?),
            spec: spec.text(),
            entry: None,
            built: false,
        },
        Some(dir) => {
            let (mesh, entry, built) = cached(spec, &files, &dir, &key)?;
            Loaded {
                mesh,
                spec: spec.text(),
                entry: Some(entry),
                built,
            }
        }
    };
    if let Ok(mut m) = MEMO.lock() {
        m.get_or_insert_with(HashMap::new)
            .insert(key, loaded.clone());
    }
    Ok(loaded)
}

/// The mesh read by the model's loader from the source files.
fn load_source(spec: &MeshSpec, files: &[PathBuf]) -> Result<Mesh> {
    let readers = files
        .iter()
        .map(|f| SourceReader::open(&f.to_string_lossy()))
        .collect::<Result<Vec<_>>>()?;
    (spec.model.load)(&readers, spec).map_err(|e| {
        mesh_error(e, &format!("mesh {}", spec.text())).with_hint(format!(
            "check that the directory holds a {} mesh ({})",
            spec.model.name,
            spec.model.files.join(", ")
        ))
    })
}

/// Why an entry cannot be used (`None`: it can).
fn entry_problem(entry: &Path, fp: &str) -> Option<String> {
    let marker = match std::fs::read(entry.join(MARKER)) {
        Ok(m) => m,
        Err(_) => return Some("no completion marker".into()),
    };
    let v: Value = match serde_json::from_slice(&marker) {
        Ok(v) => v,
        Err(e) => return Some(format!("unreadable completion marker: {e}")),
    };
    match v.get("fingerprint").and_then(Value::as_str) {
        Some(f) if f == fp => None,
        Some(f) => Some(format!(
            "fingerprint {f} differs from the source files' {fp} (the files changed)"
        )),
        None => Some("completion marker without fingerprint".into()),
    }
}

fn open_entry(entry: &Path) -> Result<Mesh> {
    let r: Arc<dyn ArrayReader> = SourceReader::open(&entry.to_string_lossy())?;
    ugrid::read(r).map_err(|e| mesh_error(e, &format!("mesh cache entry {}", entry.display())))
}

fn cached(
    spec: &MeshSpec,
    files: &[PathBuf],
    dir: &Path,
    key: &str,
) -> Result<(Arc<Mesh>, PathBuf, bool)> {
    let fp = fingerprint(files)?;
    let entry = dir.join(format!("{key}.zarr"));
    if entry.exists() {
        match entry_problem(&entry, &fp) {
            None => return Ok((Arc::new(open_entry(&entry)?), entry, false)),
            Some(why) => set_aside(&entry, &why),
        }
    }
    std::fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
    let tmp = dir.join(format!(
        "{key}.zarr.tmp-{}-{}-{:08x}",
        crate::exec::publish::hostname(),
        std::process::id(),
        crate::exec::publish::random_u32()
    ));
    // exclusive: a name nobody else uses, created by this process
    std::fs::create_dir(&tmp).map_err(|e| io_err(&tmp, e))?;
    let built = build(spec, files, &tmp, &fp);
    let built = built.and_then(|()| match std::fs::rename(&tmp, &entry) {
        Ok(()) => Ok(true),
        // another process renamed its entry first: use that one
        Err(_) if entry.join(MARKER).exists() => Ok(false),
        Err(e) => Err(io_err(&entry, e)),
    });
    let won = match built {
        Ok(w) => w,
        Err(e) => {
            remove_own_tmp(&tmp);
            return Err(e);
        }
    };
    if !won {
        remove_own_tmp(&tmp);
        if let Some(why) = entry_problem(&entry, &fp) {
            return Err(Error::new(
                ErrorCode::BadData,
                format!("mesh cache entry {} is not usable: {why}", entry.display()),
            ));
        }
    }
    Ok((Arc::new(open_entry(&entry)?), entry, won))
}

/// Removes the temporary directory this process created for an entry it did not publish.
fn remove_own_tmp(tmp: &Path) {
    let _ = std::fs::remove_dir_all(tmp);
}

/// Renames an unusable entry aside (it is never deleted) and says so.
fn set_aside(entry: &Path, why: &str) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let bad = PathBuf::from(format!(
        "{}.bad-{secs}-{}",
        entry.display(),
        std::process::id()
    ));
    // a concurrent process may have moved it already
    if std::fs::rename(entry, &bad).is_ok() {
        crate::exec::threads::warn(
            "mesh_cache_rebuilt",
            &format!(
                "mesh cache entry {} was not usable ({why}); moved to {} and rebuilt",
                entry.display(),
                bad.display()
            ),
        );
    }
}

/// Loads the mesh from its sources and writes it into the (new, empty) directory `tmp`, with
/// the completion marker last.
fn build(spec: &MeshSpec, files: &[PathBuf], tmp: &Path, fp: &str) -> Result<()> {
    let mesh = load_source(spec, files)?;
    let mut sink = ZarrSink::new(tmp)?;
    let extra = [
        ("cdors_fingerprint".to_owned(), Attr::Text(fp.to_owned())),
        ("cdors_spec".to_owned(), Attr::Text(spec.text())),
    ];
    ugrid::write(&mesh, &extra, &mut sink).map_err(|e| {
        mesh_error(
            e,
            &format!("writing the mesh cache entry {}", tmp.display()),
        )
    })?;
    let marker = serde_json::to_vec_pretty(&json!({
        "fingerprint": fp,
        "spec": spec.text(),
        "files": files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>(),
    }))
    .map_err(|e| Error::internal(e.to_string()))?;
    let m = tmp.join(MARKER);
    std::fs::write(&m, marker).map_err(|e| io_err(&m, e))
}

type Store = dyn ReadableWritableStorageTraits;

/// Writes UGRID arrays as a Zarr v3 store (blosc zstd level 1 with byte shuffle).
struct ZarrSink {
    store: Arc<Store>,
    arrays: HashMap<String, (Array<Store>, ArraySpec)>,
}

fn zerr(e: impl std::fmt::Display) -> MeshError {
    MeshError::Read(format!("zarr: {e}"))
}

fn attr_json(a: &Attr) -> Value {
    match a {
        Attr::Text(s) => json!(s),
        Attr::Num(v) if v.len() == 1 => num_json(v[0]),
        Attr::Num(v) => Value::Array(v.iter().map(|&x| num_json(x)).collect()),
    }
}

fn num_json(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() < 9e15 {
        json!(x as i64)
    } else {
        json!(x)
    }
}

fn attrs_json(attrs: &[(String, Attr)]) -> Map<String, Value> {
    attrs
        .iter()
        .map(|(k, v)| (k.clone(), attr_json(v)))
        .collect()
}

impl ZarrSink {
    fn new(dir: &Path) -> Result<Self> {
        let fs = FilesystemStore::new(dir)
            .map_err(|e| Error::io(format!("zarr: cannot create {}: {e}", dir.display())))?;
        Ok(Self {
            store: Arc::new(fs),
            arrays: HashMap::new(),
        })
    }
}

impl UgridSink for ZarrSink {
    fn global(&mut self, attrs: &[(String, Attr)]) -> cdors_mesh::Result<()> {
        let v = json!({"zarr_format": 3, "node_type": "group", "attributes": attrs_json(attrs)});
        let k = StoreKey::new("zarr.json").map_err(zerr)?;
        let bytes = serde_json::to_vec_pretty(&v).map_err(zerr)?;
        self.store.set(&k, bytes.into()).map_err(zerr)
    }

    fn create(&mut self, s: &ArraySpec) -> cdors_mesh::Result<()> {
        let (dtype, size, fill) = match s.kind {
            Kind::F64 => ("float64", 8, json!("NaN")),
            Kind::I32 => ("int32", 4, json!(-1)),
        };
        let mut codecs = vec![json!({"name": "bytes", "configuration": {"endian": "little"}})];
        if !s.dims.is_empty() {
            codecs.push(json!({"name": "blosc", "configuration": {
                "cname": "zstd", "clevel": 1, "shuffle": "shuffle", "typesize": size,
                "blocksize": 0,
            }}));
        }
        let meta = json!({
            "zarr_format": 3,
            "node_type": "array",
            "shape": s.dims.iter().map(|d| d.1).collect::<Vec<_>>(),
            "data_type": dtype,
            "chunk_grid": {"name": "regular", "configuration": {"chunk_shape": s.chunks}},
            "chunk_key_encoding": {"name": "default", "configuration": {"separator": "/"}},
            "fill_value": fill,
            "codecs": codecs,
            "attributes": attrs_json(&s.attrs),
            "dimension_names": s.dims.iter().map(|d| d.0.clone()).collect::<Vec<_>>(),
        });
        let md: ArrayMetadataV3 = serde_json::from_value(meta).map_err(zerr)?;
        let a = Array::new_with_metadata(
            self.store.clone(),
            &format!("/{}", s.name),
            ArrayMetadata::V3(md),
        )
        .map_err(zerr)?;
        a.store_metadata().map_err(zerr)?;
        self.arrays.insert(s.name.clone(), (a, s.clone()));
        Ok(())
    }

    fn write(&mut self, name: &str, first: usize, values: &[f64]) -> cdors_mesh::Result<()> {
        let (a, s) = self
            .arrays
            .get(name)
            .ok_or_else(|| MeshError::Invalid(format!("array '{name}' not created")))?;
        let mut idx = vec![0u64; s.dims.len()];
        if let Some(i) = idx.first_mut() {
            *i = (first / s.chunks[0].max(1)) as u64;
        }
        // whole chunks; the last one along the first dimension padded to the chunk shape
        let full: usize = s.chunks.iter().product();
        match s.kind {
            Kind::F64 => {
                let mut v = values.to_vec();
                v.resize(full.max(values.len()), f64::NAN);
                a.store_chunk(&idx, v).map_err(zerr)
            }
            Kind::I32 => {
                let mut v: Vec<i32> = values.iter().map(|&x| x as i32).collect();
                v.resize(full.max(values.len()), -1);
                a.store_chunk(&idx, v).map_err(zerr)
            }
        }
    }
}
