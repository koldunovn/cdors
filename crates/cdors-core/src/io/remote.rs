//! Remote inputs: Zarr stores and kerchunk reference targets over `http(s)://` and `s3://`.
//!
//! - **One client per origin** (`scheme://host[:port]`, or the S3 bucket), shared by every store
//!   and every kerchunk reference that points there, all driven by one small tokio runtime
//!   (`runtime()`). Calls are blocking: each [`RemoteStore::get`] blocks its thread on one async
//!   GET, so an executor I/O pool of N threads keeps N requests in flight.
//! - **HTTP/1.1** by default, one connection per request in flight: on the EERIE cloud, 64
//!   requests over HTTP/1.1 reached 0.19 GB/s, over one HTTP/2 connection 0.14 GB/s (at 16 in
//!   flight HTTP/2 was the faster one). `CDORS_HTTP2=1` lets the server choose HTTP/2.
//! - **Byte ranges** of one call (kerchunk targets, partial reads) go out as one batch with
//!   nearby ranges merged (object_store `get_ranges`). Servers whose 206 answers carry neither
//!   Content-Length nor Content-Range (the EERIE cloud) get whole-object reads instead.
//! - **Retries** (object_store's retry layer): timeouts, connection errors, incomplete bodies,
//!   HTTP 5xx and 429 are retried with exponential backoff, at most [`MAX_RETRIES`] times
//!   (4 attempts, 0.5 s, 1 s, 2 s apart) and never longer than [`RETRY_TIMEOUT`]; 4xx answers are
//!   not retried. Callers only ever ask for keys inside an array's chunk grid
//!   ([`super::ChunkGrid::check`]), because some servers (the EERIE cloud) answer 503, not 404,
//!   for keys outside it.
//! - **Metadata**: `.zmetadata` is fetched first; when it exists the store is opened from it
//!   alone (one request, `zarr.json` is then assumed absent). Every metadata key goes through the
//!   same v2 rewrites as kerchunk references ([`super::normalize`]), so the EERIE cloud's
//!   `"compressor": null, "filters": [blosc]` arrays open.
//! - **S3**: credentials, region and endpoint come from the usual `AWS_*` environment variables
//!   (`AWS_ENDPOINT_URL`, `AWS_SKIP_SIGNATURE=true` for public buckets, ...).
//! - A URL ending in `.json` is a kerchunk JSON reference set; its targets may be local or remote.
//!
//! Errors: 404 is "absent" (a missing chunk holds the fill value); a store with no metadata at all
//! is `bad_data` (exit 2); 401/403 is `missing_input` (exit 1); everything that survives the
//! retries is `io_error` (exit 3) with the URL and key in the message.

use super::kerchunk::KerchunkStore;
use super::normalize;
use super::zarr::ZarrSource;
use crate::error::{Error, ErrorCode, Result};
use object_store::path::{Path as ObjPath, PathPart};
use object_store::{BackoffConfig, ClientOptions, ObjectStore, ObjectStoreExt, RetryConfig};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use zarrs::storage::byte_range::{ByteRange, ByteRangeIterator, InvalidByteRangeError};
use zarrs::storage::{
    Bytes, MaybeBytes, MaybeBytesIterator, ReadableStorageTraits, StorageError, StoreKey,
};

/// Retries after the first attempt.
pub const MAX_RETRIES: usize = 3;
/// No retry starts later than this after the first attempt.
pub const RETRY_TIMEOUT: Duration = Duration::from_secs(60);
/// Per-request timeout (a 4 MB chunk at the EERIE cloud's rate shared by 64 requests takes ~1 s).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether `s` is a URL that cdors reads remotely.
pub fn is_url(s: &str) -> bool {
    ["http://", "https://", "s3://"]
        .iter()
        .any(|p| s.starts_with(p))
}

/// The runtime that drives all remote I/O. Its few worker threads only poll sockets; the
/// callers' threads block in [`block_on`].
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        // fewer workers if the system refuses threads; without any, a current-thread runtime
        // (driven by whichever caller blocks on it)
        let multi = crate::exec::threads::with_fallback("the network runtime", 4, |k| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(k)
                .max_blocking_threads(k)
                .thread_name("cdors-net")
                .enable_all()
                .build()
        });
        multi.unwrap_or_else(|e| {
            crate::exec::threads::warn("threads_reduced", &e.to_text());
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("cannot start the I/O runtime")
        })
    })
}

/// Runs `fut` to completion on the I/O runtime, blocking the calling thread (which must not be
/// a runtime thread itself; cdors calls this from its own I/O and compute pools).
fn block_on<F: Future + Send + 'static>(fut: F) -> F::Output
where
    F::Output: Send + 'static,
{
    let rt = runtime();
    rt.block_on(async move { rt.spawn(fut).await.expect("I/O task panicked") })
}

/// A parsed remote location: the shared client of its origin and the object path in it.
#[derive(Clone)]
struct Location {
    store: Arc<dyn ObjectStore>,
    /// `scheme://authority`, for messages.
    origin: String,
    path: ObjPath,
}

impl Location {
    fn child(&self, key: &str) -> ObjPath {
        ObjPath::from_iter(
            self.path
                .parts()
                .chain(key.split('/').filter(|p| !p.is_empty()).map(PathPart::from)),
        )
    }

    fn url(&self, path: &ObjPath) -> String {
        format!("{}/{path}", self.origin)
    }
}

fn client_options() -> ClientOptions {
    let opts = ClientOptions::new()
        .with_timeout(REQUEST_TIMEOUT)
        .with_connect_timeout(CONNECT_TIMEOUT)
        .with_pool_max_idle_per_host(128)
        .with_allow_http(true);
    if std::env::var_os("CDORS_HTTP2").is_some_and(|v| v == "1") {
        opts.with_allow_http2()
    } else {
        opts
    }
}

fn retry_config() -> RetryConfig {
    RetryConfig {
        backoff: BackoffConfig {
            init_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(4),
            base: 2.0,
        },
        max_retries: MAX_RETRIES,
        retry_timeout: RETRY_TIMEOUT,
    }
}

/// Splits a URL and returns the (cached) client of its origin.
fn locate(url: &str) -> std::result::Result<Location, String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("'{url}' is not a URL (no host)"))?;
    if rest.contains(['?', '#']) {
        return Err(format!(
            "'{url}': query strings and fragments are not supported"
        ));
    }
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.is_empty() {
        return Err(format!("'{url}' has no host"));
    }
    let origin = format!("{scheme}://{authority}");
    static CLIENTS: OnceLock<Mutex<HashMap<String, Arc<dyn ObjectStore>>>> = OnceLock::new();
    let mut clients = CLIENTS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let store = match clients.get(&origin) {
        Some(s) => s.clone(),
        None => {
            // reqwest wants a runtime context when its client is built
            let _guard = runtime().enter();
            let store: Arc<dyn ObjectStore> = match scheme {
                "http" | "https" => Arc::new(
                    object_store::http::HttpBuilder::new()
                        .with_url(&origin)
                        .with_client_options(client_options())
                        .with_retry(retry_config())
                        .build()
                        .map_err(|e| format!("'{url}': {e}"))?,
                ),
                "s3" => Arc::new(
                    object_store::aws::AmazonS3Builder::from_env()
                        .with_bucket_name(authority)
                        .with_client_options(client_options())
                        .with_retry(retry_config())
                        .build()
                        .map_err(|e| format!("'{url}': {e}"))?,
                ),
                _ => return Err(format!("'{url}': unsupported URL scheme '{scheme}'")),
            };
            clients.insert(origin.clone(), store.clone());
            store
        }
    };
    let path = ObjPath::from_url_path(path).map_err(|e| format!("'{url}': {e}"))?;
    Ok(Location {
        store,
        origin,
        path,
    })
}

fn other(url: &str, e: impl std::fmt::Display) -> StorageError {
    StorageError::Other(format!("GET {url}: {}", tidy(&e.to_string())))
}

/// Drops the HTML error page that some servers send with an error status.
fn tidy(msg: &str) -> &str {
    msg.find("<html")
        .or_else(|| msg.find("<!DOCTYPE"))
        .map_or(msg, |i| msg[..i].trim_end_matches([' ', ':', '\r', '\n']))
}

/// GETs a whole object; `None` if it does not exist (404).
fn get_object(loc: &Location, path: ObjPath) -> std::result::Result<Option<Bytes>, StorageError> {
    let store = loc.store.clone();
    let p = path.clone();
    match block_on(async move { store.get(&p).await?.bytes().await }) {
        Ok(b) => Ok(Some(b)),
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(other(&loc.url(&path), e)),
    }
}

/// GETs byte ranges of one object; adjacent and nearby ranges are merged into one request.
fn get_ranges(
    loc: &Location,
    path: ObjPath,
    ranges: Vec<Range<u64>>,
) -> std::result::Result<Option<Vec<Bytes>>, StorageError> {
    let store = loc.store.clone();
    let p = path.clone();
    let rs = ranges.clone();
    match block_on(async move { store.get_ranges(&p, &rs).await }) {
        Ok(b) => Ok(Some(b)),
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        // Some servers answer a range request with 206 but neither Content-Length nor
        // Content-Range (the EERIE cloud's nginx sends a chunked body), which object_store
        // refuses because it cannot check the range. Read the whole object and slice it.
        Err(e)
            if ["Content-Length Header missing", "Content-Range"]
                .iter()
                .any(|s| e.to_string().contains(s)) =>
        {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                crate::exec::threads::warn(
                    "no_range_requests",
                    &format!(
                        "{} does not describe partial responses; reading whole objects",
                        loc.origin
                    ),
                );
            });
            let Some(b) = get_object(loc, path.clone())? else {
                return Ok(None);
            };
            let len = b.len() as u64;
            ranges
                .iter()
                .map(|r| {
                    if r.end > len {
                        return Err(other(
                            &loc.url(&path),
                            format!("range {r:?} beyond the object size {len}"),
                        ));
                    }
                    Ok(b.slice(r.start as usize..r.end as usize))
                })
                .collect::<std::result::Result<Vec<_>, _>>()
                .map(Some)
        }
        Err(e) => Err(other(&loc.url(&path), e)),
    }
}

fn head_size(loc: &Location, path: ObjPath) -> std::result::Result<Option<u64>, StorageError> {
    let store = loc.store.clone();
    let p = path.clone();
    match block_on(async move { store.head(&p).await }) {
        Ok(m) => Ok(Some(m.size)),
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(StorageError::Other(format!(
            "HEAD {}: {}",
            loc.url(&path),
            tidy(&e.to_string())
        ))),
    }
}

/// Size of the object at `url` (a kerchunk reference with no length).
pub(crate) fn object_size(url: &str) -> std::result::Result<u64, StorageError> {
    let loc = locate(url).map_err(StorageError::Other)?;
    let path = loc.path.clone();
    head_size(&loc, path)?.ok_or_else(|| StorageError::Other(format!("{url}: not found (404)")))
}

/// Reads `ranges` (relative to `offset`) of the object at `url`, which is a kerchunk reference
/// target `[url, offset, length]` (`length == None`: to the end of the object). All ranges of one
/// call go out as one batch, with adjacent ranges merged.
pub(crate) fn read_ref_ranges(
    url: &str,
    offset: u64,
    length: Option<u64>,
    ranges: &mut dyn Iterator<Item = ByteRange>,
) -> std::result::Result<Vec<Bytes>, StorageError> {
    let loc = locate(url).map_err(StorageError::Other)?;
    let ranges: Vec<ByteRange> = ranges.collect();
    let len = match length {
        Some(l) => l,
        None => {
            if let [ByteRange::FromStart(0, None)] = ranges[..]
                && offset == 0
            {
                let b = get_object(&loc, loc.path.clone())?
                    .ok_or_else(|| other(url, "not found (404)"))?;
                return Ok(vec![b]);
            }
            object_size(url)?.saturating_sub(offset)
        }
    };
    let mut abs = Vec::with_capacity(ranges.len());
    for br in ranges {
        let (s, e) = (br.start(len), br.end(len));
        if e > len || s > e {
            return Err(InvalidByteRangeError::new(br, len).into());
        }
        abs.push(offset + s..offset + e);
    }
    get_ranges(&loc, loc.path.clone(), abs)?.ok_or_else(|| other(url, "not found (404)"))
}

/// A Zarr store at a remote URL, readable through `zarrs`. Metadata keys are cached and
/// normalised; chunk keys are fetched on every call.
pub struct RemoteStore {
    loc: Location,
    url: String,
    meta: Mutex<HashMap<String, Option<Bytes>>>,
}

impl RemoteStore {
    fn new(raw: &str) -> Result<Self> {
        let trimmed = raw.trim_end_matches('/');
        let url = if trimmed.contains("://") {
            trimmed
        } else {
            raw
        }
        .to_owned();
        let loc = locate(&url).map_err(|m| Error::bad_arguments(m).with("url", raw))?;
        Ok(Self {
            loc,
            url,
            meta: Mutex::new(HashMap::new()),
        })
    }

    fn fetch(&self, key: &str) -> std::result::Result<Option<Bytes>, StorageError> {
        get_object(&self.loc, self.loc.child(key))
    }

    /// Fetches a metadata key once (normalised) and remembers the answer, including "absent".
    fn metadata(&self, key: &str) -> std::result::Result<Option<Bytes>, StorageError> {
        if let Some(v) = self.meta.lock().unwrap_or_else(|e| e.into_inner()).get(key) {
            return Ok(v.clone());
        }
        let v = self.fetch(key)?.map(|b| normalize::metadata(key, &b));
        self.meta
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_owned(), v.clone());
        Ok(v)
    }

    /// [`Self::metadata`] with errors classified for the user.
    fn probe(&self, key: &str) -> Result<Option<Bytes>> {
        self.metadata(key)
            .map_err(|e| self.open_error(&e.to_string()))
    }

    fn open_error(&self, msg: &str) -> Error {
        let denied = ["status: 401", "status: 403", "Unauthorized", "Forbidden"]
            .iter()
            .any(|s| msg.contains(s));
        if denied {
            Error::new(
                ErrorCode::MissingInput,
                format!("access to '{}' denied: {msg}", self.url),
            )
            .with("url", self.url.as_str())
            .with_hint(
                "check the URL and credentials; EERIE cloud datasets are readable at \
                 .../datasets/<id>/kerchunk",
            )
        } else {
            Error::io(format!("cannot open '{}': {msg}", self.url)).with("url", self.url.as_str())
        }
    }
}

impl ReadableStorageTraits for RemoteStore {
    fn get(&self, key: &StoreKey) -> std::result::Result<MaybeBytes, StorageError> {
        let k = key.as_str();
        if normalize::is_metadata_key(k) || k.ends_with("zarr.json") {
            self.metadata(k)
        } else {
            self.fetch(k)
        }
    }

    fn get_partial_many<'a>(
        &'a self,
        key: &StoreKey,
        byte_ranges: ByteRangeIterator<'a>,
    ) -> std::result::Result<MaybeBytesIterator<'a>, StorageError> {
        let ranges: Vec<ByteRange> = byte_ranges.collect();
        let bounded: Option<Vec<Range<u64>>> = ranges
            .iter()
            .map(|r| match r {
                ByteRange::FromStart(o, Some(l)) => Some(*o..o + l),
                _ => None,
            })
            .collect();
        let out = match bounded {
            Some(abs) => get_ranges(&self.loc, self.loc.child(key.as_str()), abs)?,
            // open-ended or suffix ranges: fetch the object once and slice it
            None => self.fetch(key.as_str())?.map(|b| {
                let len = b.len() as u64;
                ranges
                    .iter()
                    .map(|r| b.slice(r.start(len) as usize..r.end(len).min(len) as usize))
                    .collect()
            }),
        };
        Ok(out.map(|v| Box::new(v.into_iter().map(Ok)) as _))
    }

    fn size_key(&self, key: &StoreKey) -> std::result::Result<Option<u64>, StorageError> {
        head_size(&self.loc, self.loc.child(key.as_str()))
    }

    fn supports_get_partial(&self) -> bool {
        true
    }
}

/// Lists the root group's members of an S3 store (HTTP servers cannot list).
fn list_dirs(loc: &Location) -> Vec<String> {
    if !loc.origin.starts_with("s3://") {
        return Vec::new();
    }
    let store = loc.store.clone();
    let prefix = loc.path.clone();
    let mut names: Vec<String> =
        match block_on(async move { store.list_with_delimiter(Some(&prefix)).await }) {
            Ok(l) => l
                .common_prefixes
                .iter()
                .filter_map(|p| p.filename().map(str::to_owned))
                .filter(|n| !n.starts_with('.'))
                .collect(),
            Err(_) => Vec::new(),
        };
    names.sort();
    names
}

/// Opens a remote input (lazily: metadata and coordinates only): a Zarr v2/v3 store, or a
/// kerchunk JSON reference set when the URL ends in `.json`.
pub fn open(url: &str) -> Result<ZarrSource> {
    let store = RemoteStore::new(url)?;
    let url = store.url.clone();
    if url.ends_with(".json") {
        let text = get_object(&store.loc, store.loc.path.clone())
            .map_err(|e| store.open_error(&e.to_string()))?
            .ok_or_else(|| missing(&url))?;
        let json: serde_json::Value = serde_json::from_slice(&normalize::sanitize_json(&text))
            .map_err(|e| Error::bad_data(format!("'{url}': invalid JSON: {e}")))?;
        let refs = KerchunkStore::from_json(&json).map_err(|e| {
            Error::bad_data(format!("cannot open kerchunk references '{url}': {e}"))
        })?;
        let names: Vec<String> = refs
            .arrays()
            .into_iter()
            .filter(|n| !n.contains('/'))
            .collect();
        return ZarrSource::open_store(&url, Arc::new(refs), &move || names.clone());
    }
    // Consolidated v2 metadata first: with it, opening is a single request.
    if store.probe(".zmetadata")?.is_some() {
        store
            .meta
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("zarr.json".into(), None);
    } else if store.probe("zarr.json")?.is_none()
        && store.probe(".zgroup")?.is_none()
        && store.probe(".zarray")?.is_none()
    {
        return Err(missing(&url));
    }
    let loc = store.loc.clone();
    let store = Arc::new(store);
    ZarrSource::open_store(&url, store, &move || list_dirs(&loc))
}

fn missing(url: &str) -> Error {
    Error::bad_data(format!(
        "no Zarr store at '{url}' (.zmetadata, zarr.json, .zgroup and .zarray not found)"
    ))
    .with("url", url)
    .with_hint("check the URL; EERIE cloud datasets are at https://eerie.cloud.dkrz.de/datasets/<id>/kerchunk")
}
