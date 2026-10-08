//! Read-only `zarrs` store over kerchunk references.
//!
//! A kerchunk reference set maps Zarr v2 keys (`.zgroup`, `var/.zarray`, `var/0.0.0`, ...) to
//! either inline bytes or a byte range in some file. Two on-disk formats are supported:
//!
//! - JSON, version 0 (the whole object is the reference map) and version 1
//!   (`{"version": 1, "refs": {...}, "templates": {...}}`). A reference is `[url]` (whole file),
//!   `[url, offset, length]`, a `"base64:..."` string or an inline text/JSON string.
//!   `{{name}}` in URLs is replaced from `templates`; `gen` is not supported.
//! - Parquet (fsspec `LazyReferenceMapper`): a directory with `.zmetadata`
//!   (`{"metadata": {...}, "record_size": N}`) and per array `var/refs.<k>.parq` with columns
//!   `path, offset, size, raw`; row `i` of file `k` is chunk `k * N + i` in C order of the chunk
//!   grid. Both `path` and `raw` null means the chunk is missing (fill value).
//!
//! Chunk reads are positioned reads (`pread`) of local files, so the store can be used from many
//! threads at once. `file://` URLs and plain paths are local; `http(s)://`, `s3://` and other
//! schemes return a "remote not yet supported" error (Task 11 adds remote reads).
//!
//! ```ignore
//! let store = Arc::new(KerchunkStore::open(Path::new("refs.json"))?);
//! let array = zarrs::array::Array::open(store, "/tas")?;
//! ```

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::Value;
use zarrs::storage::byte_range::{ByteRange, ByteRangeIterator, InvalidByteRangeError};
use zarrs::storage::{
    Bytes, ListableStorageTraits, MaybeBytes, MaybeBytesIterator, ReadableStorageTraits,
    StorageError, StoreKey, StoreKeys, StoreKeysPrefixes, StorePrefix, StorePrefixes,
};

/// Errors from opening or resolving kerchunk references.
#[derive(Debug, thiserror::Error)]
pub enum KerchunkError {
    #[error("{path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("{path}: invalid JSON: {source}")]
    Json {
        path: String,
        source: serde_json::Error,
    },
    #[error("{path}: {source}")]
    Parquet {
        path: String,
        source: parquet::errors::ParquetError,
    },
    #[error("invalid kerchunk reference: {0}")]
    Format(String),
    #[error("remote references are not yet supported (Task 11): {0}")]
    Remote(String),
}

impl From<KerchunkError> for StorageError {
    fn from(e: KerchunkError) -> Self {
        StorageError::Other(e.to_string())
    }
}

/// One resolved reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ref {
    /// Bytes stored in the reference set itself.
    Inline(Bytes),
    /// A byte range in a file; `length == None` means the whole file from `offset`.
    Range {
        url: Arc<str>,
        offset: u64,
        length: Option<u64>,
    },
}

/// Read-only store over a kerchunk reference set; implements `zarrs` storage traits.
pub struct KerchunkStore {
    /// Every key that is resolved without Parquet lookups (all keys for JSON; metadata for Parquet).
    refs: HashMap<String, Ref>,
    parquet: Option<ParquetRefs>,
    files: FileCache,
}

impl std::fmt::Debug for KerchunkStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KerchunkStore")
            .field("keys", &self.refs.len())
            .field("parquet", &self.parquet.as_ref().map(|p| &p.root))
            .finish()
    }
}

impl KerchunkStore {
    /// Open a reference set: a Parquet reference directory (contains `.zmetadata`) or a JSON file.
    pub fn open(path: &Path) -> Result<Self, KerchunkError> {
        if path.is_dir() {
            return Self::open_parquet(path);
        }
        let text = std::fs::read(path).map_err(|source| KerchunkError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let json: Value = serde_json::from_slice(&sanitize_json(&text)).map_err(|source| {
            KerchunkError::Json {
                path: path.display().to_string(),
                source,
            }
        })?;
        Self::from_json(&json)
    }

    /// Build from a parsed JSON reference set (version 0 or 1).
    pub fn from_json(json: &Value) -> Result<Self, KerchunkError> {
        let obj = json
            .as_object()
            .ok_or_else(|| KerchunkError::Format("top level is not an object".into()))?;
        let (refs, templates) = match obj.get("version").and_then(Value::as_u64) {
            Some(1) => {
                if obj
                    .get("gen")
                    .is_some_and(|g| g.as_array().is_some_and(|a| !a.is_empty()))
                {
                    return Err(KerchunkError::Format(
                        "`gen` references are not supported".into(),
                    ));
                }
                let refs = obj
                    .get("refs")
                    .and_then(Value::as_object)
                    .ok_or_else(|| KerchunkError::Format("version 1 without `refs`".into()))?;
                let templates: HashMap<String, String> = obj
                    .get("templates")
                    .and_then(Value::as_object)
                    .map(|t| {
                        t.iter()
                            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                            .collect()
                    })
                    .unwrap_or_default();
                (refs, templates)
            }
            Some(v) => return Err(KerchunkError::Format(format!("unknown version {v}"))),
            None => (obj, HashMap::new()),
        };
        let mut urls = UrlInterner::default();
        let mut out = HashMap::with_capacity(refs.len());
        for (key, value) in refs {
            out.insert(
                key.clone(),
                parse_json_ref(key, value, &templates, &mut urls)?,
            );
        }
        Ok(Self::from_refs(out))
    }

    /// Build from an already resolved reference map (used for NetCDF-4 chunk indexes).
    pub fn from_refs(mut refs: HashMap<String, Ref>) -> Self {
        for (key, r) in refs.iter_mut() {
            let name = key.rsplit('/').next().unwrap_or(key);
            if let (true, Ref::Inline(b)) = (name.starts_with(".z"), &*r) {
                let b = sanitize_json(b);
                *r = Ref::Inline(match name {
                    ".zarray" => normalize_zarray(&b),
                    ".zmetadata" => normalize_zmetadata(&b),
                    _ => b,
                });
            }
        }
        Self {
            refs,
            parquet: None,
            files: FileCache::default(),
        }
    }

    fn open_parquet(root: &Path) -> Result<Self, KerchunkError> {
        let zpath = root.join(".zmetadata");
        let text = std::fs::read(&zpath).map_err(|source| KerchunkError::Io {
            path: zpath.display().to_string(),
            source,
        })?;
        let zmeta: Value = serde_json::from_slice(&sanitize_json(&text)).map_err(|source| {
            KerchunkError::Json {
                path: zpath.display().to_string(),
                source,
            }
        })?;
        let record_size = zmeta
            .get("record_size")
            .and_then(Value::as_u64)
            .filter(|&n| n > 0)
            .ok_or_else(|| KerchunkError::Format(format!("{}: no record_size", zpath.display())))?;
        let metadata = zmeta
            .get("metadata")
            .and_then(Value::as_object)
            .ok_or_else(|| KerchunkError::Format(format!("{}: no metadata", zpath.display())))?;
        let mut refs = HashMap::new();
        let mut arrays = HashMap::new();
        // the consolidated metadata with the same `.zarray` normalisation as the single keys
        let mut consolidated = serde_json::Map::new();
        for (key, value) in metadata {
            let bytes = match value {
                Value::String(s) => Bytes::from(s.clone().into_bytes()),
                other => Bytes::from(other.to_string().into_bytes()),
            };
            if let Some(name) = key.strip_suffix("/.zarray") {
                let bytes = normalize_zarray(&bytes);
                let meta: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| KerchunkError::Format(format!("{key}: {e}")))?;
                arrays.insert(name.to_owned(), ChunkGrid::from_zarray(key, &meta)?);
                consolidated.insert(key.clone(), meta);
                refs.insert(key.clone(), Ref::Inline(bytes));
            } else {
                // fsspec may store each entry as a JSON string
                let parsed = match value {
                    Value::String(s) => serde_json::from_slice(&sanitize_json(s.as_bytes()))
                        .unwrap_or_else(|_| value.clone()),
                    other => other.clone(),
                };
                consolidated.insert(key.clone(), parsed);
                refs.insert(key.clone(), Ref::Inline(bytes));
            }
        }
        // fsspec presents the consolidated metadata itself as a key, as zarr-python expects.
        refs.insert(
            ".zmetadata".into(),
            Ref::Inline(Bytes::from(
                serde_json::json!({"metadata": consolidated, "zarr_consolidated_format": 1})
                    .to_string()
                    .into_bytes(),
            )),
        );
        Ok(Self {
            refs,
            parquet: Some(ParquetRefs {
                root: root.to_path_buf(),
                record_size,
                arrays,
                records: Mutex::new(HashMap::new()),
            }),
            files: FileCache::default(),
        })
    }

    /// Names of all arrays in the reference set (keys ending in `/.zarray`), sorted.
    pub fn arrays(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .refs
            .keys()
            .filter_map(|k| k.strip_suffix("/.zarray").map(str::to_owned))
            .collect();
        names.sort();
        names
    }

    /// Resolve a key to its reference (`None`: key not present, e.g. a missing chunk).
    pub fn reference(&self, key: &str) -> Result<Option<Ref>, KerchunkError> {
        if let Some(r) = self.refs.get(key) {
            return Ok(Some(r.clone()));
        }
        match &self.parquet {
            Some(p) => p.lookup(key),
            None => Ok(None),
        }
    }

    /// Read byte ranges of a resolved reference.
    fn read_ranges(
        &self,
        r: &Ref,
        ranges: &mut dyn Iterator<Item = ByteRange>,
    ) -> Result<Vec<Bytes>, StorageError> {
        match r {
            Ref::Inline(data) => ranges
                .map(|br| {
                    let len = data.len() as u64;
                    let (s, e) = (br.start(len), br.end(len));
                    if e > len || s > e {
                        Err(InvalidByteRangeError::new(br, len).into())
                    } else {
                        Ok(data.slice(s as usize..e as usize))
                    }
                })
                .collect(),
            Ref::Range {
                url,
                offset,
                length,
            } => {
                let path = local_path(url)?;
                let file = self.files.get(&path)?;
                let len = match length {
                    Some(l) => *l,
                    None => file_len(&file, &path)?.saturating_sub(*offset),
                };
                ranges
                    .map(|br| {
                        let (s, e) = (br.start(len), br.end(len));
                        if e > len || s > e {
                            return Err(InvalidByteRangeError::new(br, len).into());
                        }
                        let mut buf = vec![0u8; (e - s) as usize];
                        file.read_exact_at(&mut buf, offset + s).map_err(|err| {
                            StorageError::Other(format!(
                                "{}: read {} bytes at {}: {err}",
                                path.display(),
                                e - s,
                                offset + s
                            ))
                        })?;
                        Ok(Bytes::from(buf))
                    })
                    .collect()
            }
        }
    }
}

impl ReadableStorageTraits for KerchunkStore {
    fn get_partial_many<'a>(
        &'a self,
        key: &StoreKey,
        byte_ranges: ByteRangeIterator<'a>,
    ) -> Result<MaybeBytesIterator<'a>, StorageError> {
        let Some(r) = self.reference(key.as_str())? else {
            return Ok(None);
        };
        let mut ranges = byte_ranges;
        let out = self.read_ranges(&r, &mut ranges)?;
        Ok(Some(Box::new(out.into_iter().map(Ok))))
    }

    fn get(&self, key: &StoreKey) -> Result<MaybeBytes, StorageError> {
        let Some(r) = self.reference(key.as_str())? else {
            return Ok(None);
        };
        let mut one = std::iter::once(ByteRange::FromStart(0, None));
        Ok(self.read_ranges(&r, &mut one)?.pop())
    }

    fn size_key(&self, key: &StoreKey) -> Result<Option<u64>, StorageError> {
        Ok(match self.reference(key.as_str())? {
            None => None,
            Some(Ref::Inline(b)) => Some(b.len() as u64),
            Some(Ref::Range {
                length: Some(l), ..
            }) => Some(l),
            Some(Ref::Range { url, offset, .. }) => {
                let path = local_path(&url)?;
                let file = self.files.get(&path)?;
                Some(file_len(&file, &path)?.saturating_sub(offset))
            }
        })
    }

    fn supports_get_partial(&self) -> bool {
        true
    }
}

/// Listing covers the keys held in memory: every key of a JSON reference set, and the metadata
/// keys of a Parquet reference set (chunk keys of Parquet sets are not enumerated).
impl ListableStorageTraits for KerchunkStore {
    fn list(&self) -> Result<StoreKeys, StorageError> {
        self.list_prefix(&StorePrefix::root())
    }

    fn list_prefix(&self, prefix: &StorePrefix) -> Result<StoreKeys, StorageError> {
        let mut keys: Vec<&String> = self
            .refs
            .keys()
            .filter(|k| k.starts_with(prefix.as_str()))
            .collect();
        keys.sort();
        keys.into_iter()
            .map(|k| StoreKey::new(k.as_str()).map_err(StorageError::from))
            .collect()
    }

    fn list_dir(&self, prefix: &StorePrefix) -> Result<StoreKeysPrefixes, StorageError> {
        let mut keys = BTreeSet::new();
        let mut prefixes = BTreeSet::new();
        for k in self.refs.keys() {
            if let Some(rest) = k.strip_prefix(prefix.as_str()) {
                match rest.split_once('/') {
                    Some((dir, _)) => {
                        prefixes.insert(format!("{}{dir}/", prefix.as_str()));
                    }
                    None => {
                        keys.insert(k.clone());
                    }
                }
            }
        }
        let keys: StoreKeys = keys
            .into_iter()
            .map(StoreKey::new)
            .collect::<Result<_, _>>()?;
        let prefixes: StorePrefixes = prefixes
            .into_iter()
            .map(StorePrefix::new)
            .collect::<Result<_, _>>()?;
        Ok(StoreKeysPrefixes::new(keys, prefixes))
    }

    fn size_prefix(&self, prefix: &StorePrefix) -> Result<u64, StorageError> {
        let mut total = 0;
        for key in self.list_prefix(prefix)? {
            total += self.size_key(&key)?.unwrap_or(0);
        }
        Ok(total)
    }
}

/// Python's `json` writes `NaN`, `Infinity` and `-Infinity` as bare tokens, which are not JSON;
/// turn them into the strings Zarr v2 uses for such fill values (`"NaN"`, ...).
fn sanitize_json(text: &[u8]) -> Bytes {
    if !text.windows(3).any(|w| w == b"NaN") && !text.windows(8).any(|w| w == b"Infinity") {
        return Bytes::copy_from_slice(text);
    }
    let mut out = Vec::with_capacity(text.len() + 16);
    let (mut in_str, mut escaped, mut i) = (false, false, 0);
    while i < text.len() {
        let c = text[i];
        if in_str {
            out.push(c);
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_str = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        let token = [&b"-Infinity"[..], b"Infinity", b"NaN"]
            .into_iter()
            .find(|t| text[i..].starts_with(t));
        match (c, token) {
            (b'"', _) => {
                in_str = true;
                out.push(c);
                i += 1;
            }
            (_, Some(t)) => {
                out.push(b'"');
                out.extend_from_slice(t);
                out.push(b'"');
                i += t.len();
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Bytes::from(out)
}

/// Rewrite Zarr v2 array metadata that `zarrs` cannot open but that means the same thing.
///
/// kerchunk describes HDF5 pipelines as `"compressor": null, "filters": [..., {"id": "blosc"}]`,
/// and zarrs refuses blosc as a v2 filter. With no compressor, the last filter is decoded first,
/// exactly as a compressor would be, so it is moved into `compressor`.
/// [`normalize_zarray`] applied to every array of consolidated metadata (`.zmetadata`).
fn normalize_zmetadata(bytes: &Bytes) -> Bytes {
    let Ok(mut zm) = serde_json::from_slice::<Value>(bytes) else {
        return bytes.clone();
    };
    let Some(meta) = zm.get_mut("metadata").and_then(Value::as_object_mut) else {
        return bytes.clone();
    };
    for (key, value) in meta.iter_mut() {
        if key.ends_with("/.zarray") {
            let raw = match &*value {
                Value::String(s) => Bytes::from(s.clone().into_bytes()),
                other => Bytes::from(other.to_string().into_bytes()),
            };
            if let Ok(v) = serde_json::from_slice(&normalize_zarray(&raw)) {
                *value = v;
            }
        }
    }
    Bytes::from(zm.to_string().into_bytes())
}

fn normalize_zarray(bytes: &Bytes) -> Bytes {
    let Ok(Value::Object(mut meta)) = serde_json::from_slice::<Value>(bytes) else {
        return bytes.clone();
    };
    let compressor_null = meta.get("compressor").is_none_or(Value::is_null);
    let last_is_blosc = meta
        .get("filters")
        .and_then(Value::as_array)
        .and_then(|f| f.last())
        .and_then(|f| f.get("id"))
        .and_then(Value::as_str)
        == Some("blosc");
    if !(compressor_null && last_is_blosc) {
        return bytes.clone();
    }
    if let Some(Value::Array(filters)) = meta.get_mut("filters") {
        let blosc = filters.pop().unwrap_or(Value::Null);
        let empty = filters.is_empty();
        meta.insert("compressor".into(), blosc);
        if empty {
            meta.insert("filters".into(), Value::Null);
        }
    }
    Bytes::from(Value::Object(meta).to_string().into_bytes())
}

/// Turn one JSON reference value into a [`Ref`].
fn parse_json_ref(
    key: &str,
    value: &Value,
    templates: &HashMap<String, String>,
    urls: &mut UrlInterner,
) -> Result<Ref, KerchunkError> {
    let bad = || KerchunkError::Format(format!("{key}: {value}"));
    match value {
        Value::String(s) => match s.strip_prefix("base64:") {
            Some(b64) => base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map(|b| Ref::Inline(Bytes::from(b)))
                .map_err(|e| KerchunkError::Format(format!("{key}: base64: {e}"))),
            None => Ok(Ref::Inline(Bytes::from(s.clone().into_bytes()))),
        },
        Value::Array(a) => {
            let url = a.first().and_then(Value::as_str).ok_or_else(bad)?;
            let url = urls.intern(&apply_templates(url, templates));
            match a.len() {
                1 => Ok(Ref::Range {
                    url,
                    offset: 0,
                    length: None,
                }),
                3 => Ok(Ref::Range {
                    url,
                    offset: a[1].as_u64().ok_or_else(bad)?,
                    length: Some(a[2].as_u64().ok_or_else(bad)?),
                }),
                _ => Err(bad()),
            }
        }
        // Some writers store metadata as JSON objects instead of strings.
        Value::Object(_) => Ok(Ref::Inline(Bytes::from(value.to_string().into_bytes()))),
        _ => Err(bad()),
    }
}

fn apply_templates(url: &str, templates: &HashMap<String, String>) -> String {
    if !url.contains("{{") {
        return url.to_owned();
    }
    let mut out = url.to_owned();
    for (name, value) in templates {
        out = out.replace(&format!("{{{{{name}}}}}"), value);
    }
    out
}

/// Map a reference URL to a local path, or fail for remote schemes.
fn local_path(url: &str) -> Result<PathBuf, KerchunkError> {
    if let Some(p) = url.strip_prefix("file://") {
        return Ok(PathBuf::from(p));
    }
    if url.contains("://") {
        return Err(KerchunkError::Remote(url.to_owned()));
    }
    Ok(PathBuf::from(url))
}

fn file_len(file: &File, path: &Path) -> Result<u64, StorageError> {
    file.metadata()
        .map(|m| m.len())
        .map_err(|e| StorageError::Other(format!("{}: {e}", path.display())))
}

#[derive(Default)]
struct UrlInterner(HashMap<String, Arc<str>>);

impl UrlInterner {
    fn intern(&mut self, url: &str) -> Arc<str> {
        if let Some(u) = self.0.get(url) {
            return u.clone();
        }
        let u: Arc<str> = Arc::from(url);
        self.0.insert(url.to_owned(), u.clone());
        u
    }
}

/// Open file handles shared by all readers; positioned reads need no seek, so one handle per
/// file serves every thread.
#[derive(Default)]
struct FileCache(Mutex<HashMap<PathBuf, Arc<File>>>);

impl FileCache {
    /// Upper bound on cached handles; beyond it the map is reset (in-flight readers keep theirs).
    /// Reference sets over thousands of files (EERIE: 2424 monthly files) are read in parallel
    /// across all of them, so a small bound makes every chunk read reopen its file.
    const MAX_OPEN: usize = 4096;

    fn get(&self, path: &Path) -> Result<Arc<File>, StorageError> {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = map.get(path) {
            return Ok(f.clone());
        }
        let f = Arc::new(
            File::open(path)
                .map_err(|e| StorageError::Other(format!("{}: {e}", path.display())))?,
        );
        if map.len() >= Self::MAX_OPEN {
            map.clear();
        }
        map.insert(path.to_path_buf(), f.clone());
        Ok(f)
    }
}

/// Chunk grid of one array, for mapping chunk keys to Parquet rows.
#[derive(Debug)]
struct ChunkGrid {
    /// Number of chunks along each dimension.
    counts: Vec<u64>,
    separator: char,
}

impl ChunkGrid {
    fn from_zarray(key: &str, meta: &Value) -> Result<Self, KerchunkError> {
        let dims = |name: &str| -> Result<Vec<u64>, KerchunkError> {
            meta.get(name)
                .and_then(Value::as_array)
                .and_then(|a| a.iter().map(Value::as_u64).collect::<Option<Vec<_>>>())
                .ok_or_else(|| KerchunkError::Format(format!("{key}: bad `{name}`")))
        };
        let shape = dims("shape")?;
        let chunks = dims("chunks")?;
        if shape.len() != chunks.len() || chunks.contains(&0) {
            return Err(KerchunkError::Format(format!(
                "{key}: shape/chunks mismatch"
            )));
        }
        let separator = match meta.get("dimension_separator").and_then(Value::as_str) {
            Some("/") => '/',
            _ => '.',
        };
        Ok(Self {
            counts: shape
                .iter()
                .zip(&chunks)
                .map(|(s, c)| s.div_ceil(*c))
                .collect(),
            separator,
        })
    }

    /// C-order linear chunk index of a chunk key suffix like `3.0.1`; `None` if not a chunk key.
    fn linear_index(&self, suffix: &str) -> Option<u64> {
        if self.counts.is_empty() {
            return (suffix == "0").then_some(0);
        }
        let mut idx = 0u64;
        let mut n = 0;
        for (part, count) in suffix.split(self.separator).zip(&self.counts) {
            let i: u64 = part.parse().ok()?;
            if i >= *count {
                return None;
            }
            idx = idx * count + i;
            n += 1;
        }
        (n == self.counts.len() && suffix.split(self.separator).count() == n).then_some(idx)
    }
}

/// Rows of one decoded `refs.<k>.parq` file; `None` is a missing chunk.
type Records = Arc<Vec<Option<Ref>>>;

struct ParquetRefs {
    root: PathBuf,
    record_size: u64,
    arrays: HashMap<String, ChunkGrid>,
    /// Decoded `refs.<k>.parq` files, keyed by (array, k).
    records: Mutex<HashMap<(String, u64), Records>>,
}

impl ParquetRefs {
    fn lookup(&self, key: &str) -> Result<Option<Ref>, KerchunkError> {
        let Some((array, suffix)) = key.rsplit_once('/') else {
            return Ok(None);
        };
        let Some(grid) = self.arrays.get(array) else {
            return Ok(None);
        };
        let Some(idx) = grid.linear_index(suffix) else {
            return Ok(None);
        };
        let (file_no, row) = (idx / self.record_size, (idx % self.record_size) as usize);
        let records = self.records(array, file_no)?;
        Ok(records.get(row).cloned().flatten())
    }

    fn records(&self, array: &str, file_no: u64) -> Result<Records, KerchunkError> {
        let cache_key = (array.to_owned(), file_no);
        // Decoded under the lock: many threads asking for chunks of the same array at once
        // would otherwise each decode the file (a few ms, but megabytes of rows each).
        let mut records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = records.get(&cache_key) {
            return Ok(r.clone());
        }
        let path = self.root.join(array).join(format!("refs.{file_no}.parq"));
        let rows = if path.exists() {
            Arc::new(read_parquet_refs(&path)?)
        } else {
            Arc::new(Vec::new())
        };
        records.insert(cache_key, rows.clone());
        Ok(rows)
    }
}

/// Read one kerchunk Parquet reference file (columns `path, offset, size, raw`).
fn read_parquet_refs(path: &Path) -> Result<Vec<Option<Ref>>, KerchunkError> {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::Field;
    let perr = |source| KerchunkError::Parquet {
        path: path.display().to_string(),
        source,
    };
    let file = File::open(path).map_err(|source| KerchunkError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let reader = SerializedFileReader::new(file).map_err(perr)?;
    let mut urls = UrlInterner::default();
    let mut out = Vec::with_capacity(reader.metadata().file_metadata().num_rows() as usize);
    for row in reader.get_row_iter(None).map_err(perr)? {
        let row = row.map_err(perr)?;
        let (mut url, mut offset, mut size, mut raw) = (None, 0i64, 0i64, None);
        for (name, field) in row.get_column_iter() {
            match (name.as_str(), field) {
                ("path", Field::Str(s)) => url = Some(s.as_str()),
                ("offset", Field::Long(v)) => offset = *v,
                ("size", Field::Long(v)) => size = *v,
                ("raw", Field::Bytes(b)) => raw = Some(b.data()),
                _ => {}
            }
        }
        out.push(match (raw, url) {
            (Some(b), _) => Some(Ref::Inline(Bytes::copy_from_slice(b))),
            (None, Some(u)) => Some(Ref::Range {
                url: urls.intern(u),
                offset: offset as u64,
                // fsspec stores a whole-file reference `[url]` as offset 0, size 0.
                length: (size > 0 || offset > 0).then_some(size as u64),
            }),
            (None, None) => None,
        });
    }
    Ok(out)
}

/// Whether `path` is a kerchunk reference set: a directory whose `.zmetadata` has a
/// `record_size` (Parquet references), or a file holding a JSON object (JSON references).
pub fn is_kerchunk(path: &Path) -> bool {
    if path.is_dir() {
        return std::fs::read(path.join(".zmetadata"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&sanitize_json(&b)).ok())
            .is_some_and(|v| v.get("record_size").is_some());
    }
    let mut head = [0u8; 64];
    let n = File::open(path)
        .and_then(|f| f.read_at(&mut head, 0))
        .unwrap_or(0);
    head[..n]
        .iter()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|&b| b == b'{')
}

/// Opens a kerchunk reference set as a Zarr dataset: the description comes out exactly as for
/// the equivalent Zarr v2 store.
pub fn open_source(path: &str) -> crate::error::Result<super::zarr::ZarrSource> {
    use crate::error::{Error, ErrorCode};
    let store = KerchunkStore::open(Path::new(path)).map_err(|e| match e {
        KerchunkError::Remote(_) => Error::new(ErrorCode::NotImplemented, e.to_string())
            .with("path", path)
            .with_hint("remote references come with a later version; use local references"),
        _ => Error::bad_data(format!("cannot open kerchunk references '{path}': {e}"))
            .with("path", path),
    })?;
    let names: Vec<String> = store
        .arrays()
        .into_iter()
        .filter(|n| !n.contains('/'))
        .collect();
    let root = path.trim_end_matches('/');
    super::zarr::ZarrSource::open_store(root, Arc::new(store), &move || names.clone())
}
