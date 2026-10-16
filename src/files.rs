//! File transfer over plain HTTP, next to the stream: list a directory, download a
//! file, upload a file. Everything is confined to one root directory (the desktop
//! user's home by default). Transfers use their own connections, so a large upload
//! or download never queues behind (or in front of) video frames.

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct Files {
    pub root: PathBuf,
    pub upload: bool,
    pub download: bool,
    /// Directory (relative to root) the client opens first and uploads into by default.
    /// Created on the first upload if it does not exist yet.
    pub default_dir: String,
    /// 0 = unlimited.
    pub max_upload_bytes: u64,
}

/// Uploads must carry this header. Browsers only send custom headers cross-origin
/// after a CORS preflight, which we never answer, so other sites can't upload.
pub const UPLOAD_HEADER: &str = "x-voxl-upload";

type Q = Query<HashMap<String, String>>;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, [(header::CONTENT_TYPE, "application/json")], serde_json::json!({ "error": msg }).to_string()).into_response()
}

impl Files {
    /// Resolves a client path ("Downloads/report.pdf") under the root. Absolute paths and
    /// `..` are rejected; symlinks are followed only if they stay inside the root.
    fn resolve(&self, rel: &str, must_exist: bool) -> Result<PathBuf, Response> {
        let bad = || err(StatusCode::BAD_REQUEST, "invalid path");
        let mut p = self.root.clone();
        for c in Path::new(rel).components() {
            match c {
                Component::Normal(s) => p.push(s),
                Component::CurDir => {}
                _ => return Err(bad()),
            }
        }
        if rel.contains('\0') {
            return Err(bad());
        }
        match p.canonicalize() {
            Ok(c) if c.starts_with(&self.root) => Ok(c),
            Ok(_) => Err(err(StatusCode::FORBIDDEN, "outside the shared folder")),
            Err(_) if !must_exist => Ok(p),
            Err(_) => Err(err(StatusCode::NOT_FOUND, "not found")),
        }
    }

    fn rel(&self, p: &Path) -> String {
        p.strip_prefix(&self.root).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

pub async fn list(State(f): State<Arc<Files>>, Query(q): Q) -> Response {
    if !f.download && !f.upload {
        return err(StatusCode::FORBIDDEN, "file transfer is disabled");
    }
    let rel = q.get("path").map(String::as_str).unwrap_or("");
    let dir = match f.resolve(rel, rel != f.default_dir) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let mut entries = Vec::new();
    match tokio::fs::read_dir(&dir).await {
        Ok(mut rd) => {
            while let Ok(Some(e)) = rd.next_entry().await {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                // Follow symlinks for type and size, as a file manager would, but
                // hide links that lead outside the shared folder.
                let Ok(md) = tokio::fs::metadata(e.path()).await else { continue };
                if e.file_type().await.is_ok_and(|t| t.is_symlink()) && !tokio::fs::canonicalize(e.path()).await.is_ok_and(|c| c.starts_with(&f.root)) {
                    continue;
                }
                if !md.is_dir() && !md.is_file() {
                    continue;
                }
                let mtime = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0);
                entries.push(serde_json::json!({ "name": name, "dir": md.is_dir(), "size": md.len(), "mtime": mtime }));
                if entries.len() >= 5000 {
                    break;
                }
            }
        }
        // The default upload folder may not exist until something is uploaded.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && rel == f.default_dir => {}
        Err(_) => return err(StatusCode::NOT_FOUND, "not a folder"),
    }
    let body = serde_json::json!({ "path": f.rel(&dir).trim_start_matches('/'), "entries": entries });
    ([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], body.to_string()).into_response()
}

pub async fn download(State(f): State<Arc<Files>>, Query(q): Q) -> Response {
    if !f.download {
        return err(StatusCode::FORBIDDEN, "downloads are disabled");
    }
    let path = match f.resolve(q.get("path").map(String::as_str).unwrap_or(""), true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Ok(file) = tokio::fs::File::open(&path).await else { return err(StatusCode::NOT_FOUND, "not found") };
    let Ok(md) = file.metadata().await else { return err(StatusCode::NOT_FOUND, "not found") };
    if !md.is_file() {
        return err(StatusCode::BAD_REQUEST, "not a file");
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "download".into());
    let stream = futures_util::stream::unfold(Some(file), |file| async move {
        let mut file = file?;
        let mut buf = Vec::with_capacity(256 << 10);
        match file.read_buf(&mut buf).await {
            Ok(0) => None,
            Ok(_) => Some((Ok::<_, std::io::Error>(bytes::Bytes::from(buf)), Some(file))),
            Err(e) => Some((Err(e), None)),
        }
    });
    let ascii: String = name.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect();
    let disposition = format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{}", pct_encode(&name));
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, "application/octet-stream".parse().unwrap());
    h.insert(header::CONTENT_LENGTH, md.len().into());
    h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    if let Ok(v) = disposition.parse() {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    (h, Body::from_stream(stream)).into_response()
}

fn pct_encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// A plain file name: no separators, no `.`/`..`, no control characters.
fn clean_name(name: &str) -> Option<&str> {
    let name = name.trim();
    let ok = !name.is_empty() && name != "." && name != ".." && name.len() <= 255 && !name.chars().any(|c| c == '/' || c == '\\' || c.is_control());
    ok.then_some(name)
}

/// `name`, or `name (1).ext`, `name (2).ext`, ... if taken.
fn unique(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).unwrap()
}

static UPLOADS: AtomicU64 = AtomicU64::new(0);

pub async fn upload(State(f): State<Arc<Files>>, Query(q): Q, headers: HeaderMap, body: Body) -> Response {
    if !f.upload {
        return err(StatusCode::FORBIDDEN, "uploads are disabled");
    }
    if !headers.contains_key(UPLOAD_HEADER) {
        return err(StatusCode::FORBIDDEN, "missing upload header");
    }
    let Some(name) = q.get("name").and_then(|n| clean_name(n)) else { return err(StatusCode::BAD_REQUEST, "invalid file name") };
    let rel = q.get("path").cloned().unwrap_or_else(|| f.default_dir.clone());
    let dir = match f.resolve(&rel, rel != f.default_dir) {
        Ok(d) => d,
        Err(e) => return e,
    };
    if rel == f.default_dir && tokio::fs::create_dir_all(&dir).await.is_err() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "cannot create the upload folder");
    }
    if !dir.is_dir() {
        return err(StatusCode::NOT_FOUND, "not a folder");
    }
    // Write to a hidden temporary file and rename once complete, so nothing in the
    // VM sees (or opens) a half-uploaded file.
    let part = dir.join(format!(".{name}.{}-{}.part", std::process::id(), UPLOADS.fetch_add(1, Ordering::Relaxed)));
    let mut out = match tokio::fs::OpenOptions::new().write(true).create_new(true).open(&part).await {
        Ok(o) => o,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("cannot write: {e}")),
    };
    let mut written: u64 = 0;
    let mut stream = body.into_data_stream();
    let result: Result<(), Response> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| err(StatusCode::BAD_REQUEST, "upload interrupted"))?;
            written += chunk.len() as u64;
            if f.max_upload_bytes > 0 && written > f.max_upload_bytes {
                return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "file is too large"));
            }
            out.write_all(&chunk).await.map_err(|e| err(StatusCode::INSUFFICIENT_STORAGE, &format!("cannot write: {e}")))?;
        }
        out.flush().await.map_err(|e| err(StatusCode::INSUFFICIENT_STORAGE, &format!("cannot write: {e}")))?;
        Ok(())
    }
    .await;
    drop(out);
    if let Err(e) = result {
        let _ = tokio::fs::remove_file(&part).await;
        return e;
    }
    let dest = unique(&dir, name);
    if let Err(e) = tokio::fs::rename(&part, &dest).await {
        let _ = tokio::fs::remove_file(&part).await;
        return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("cannot save: {e}"));
    }
    log::info!("uploaded {} ({written} bytes)", dest.display());
    let body = serde_json::json!({ "path": f.rel(&dest).trim_start_matches('/'), "size": written });
    ([(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}
