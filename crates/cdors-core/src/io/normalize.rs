//! Zarr v2 metadata rewrites shared by every store that serves kerchunk-style metadata (local
//! kerchunk references, the EERIE cloud and other HTTP/S3 stores).
//!
//! - Python's `json` writes `NaN`, `Infinity` and `-Infinity` as bare tokens, which are not JSON;
//!   they become the strings Zarr v2 uses for such fill values (`"NaN"`, ...).
//! - kerchunk describes HDF5 pipelines as `"compressor": null, "filters": [..., {"id": "blosc"}]`,
//!   which `zarrs` refuses (it converts blosc to a v3 codec only as the compressor, because it
//!   needs the element size). With no compressor the last filter is decoded first, exactly as a
//!   compressor would be, so it is moved into `compressor`.
//!
//! `fill_value: null` needs no rewrite: `zarrs` reads it as 0 for numeric types (as zarr-python
//! does), so a missing chunk of such an array reads as zeros, and no value is masked as missing
//! (`_FillValue`/`missing_value` attributes still are).

use serde_json::Value;
use zarrs::storage::Bytes;

/// Whether `key` names Zarr v2 JSON metadata (`.zarray`, `.zattrs`, `.zgroup`, `.zmetadata`).
pub(crate) fn is_metadata_key(key: &str) -> bool {
    key.rsplit('/').next().unwrap_or(key).starts_with(".z")
}

/// Applies the rewrites to the value of a metadata key (`.zarray`, `.zmetadata`, `.zattrs`,
/// `.zgroup`; other keys are returned unchanged). `key` may carry a path (`pr/.zarray`).
pub(crate) fn metadata(key: &str, bytes: &[u8]) -> Bytes {
    if !is_metadata_key(key) {
        return Bytes::copy_from_slice(bytes);
    }
    let name = key.rsplit('/').next().unwrap_or(key);
    let b = sanitize_json(bytes);
    match name {
        ".zarray" => normalize_zarray(&b),
        ".zmetadata" => normalize_zmetadata(&b),
        _ => b,
    }
}

/// Turns bare `NaN`, `Infinity` and `-Infinity` tokens outside strings into JSON strings.
pub(crate) fn sanitize_json(text: &[u8]) -> Bytes {
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

/// [`normalize_zarray`] applied to every array of consolidated metadata (`.zmetadata`).
pub(crate) fn normalize_zmetadata(bytes: &Bytes) -> Bytes {
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

/// Rewrites Zarr v2 array metadata that `zarrs` cannot open but that means the same thing: a
/// trailing blosc filter with no compressor becomes the compressor.
pub(crate) fn normalize_zarray(bytes: &Bytes) -> Bytes {
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
