/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Download streaming: a file paced onto the link by the shared cap and the downloads'
//! adaptive allowance fed [`UplinkGauge`] verdicts, or handed to a plain
//! file response that negotiates `HEAD`, `Range`, and the conditional headers when nothing is
//! paced.
//!
//! Pacing applies to plain GETs only: `HEAD` must answer instantly, a `Range` request is a
//! resume or a seek that pacing a partial stream buys nothing for while the file response
//! negotiates the 206, and a conditional request exists to be answered with 304 or 412 from
//! validators only the file response computes. The operator's static cap is the shared
//! `transfer_cap` bucket, and congestion control is `download_pacer` fed one gauge verdict per
//! chunk over the downloader's own session socket, so the transfer takes whatever the downlink
//! has spare and backs off the moment the session's delay inflates; the gauge is end to end,
//! so a reverse proxy or a fat modem buffer absorbing this host's writes still shows as the
//! round trip it costs the session. Chunks are sized to the current rate
//! ([`gauged_chunk_size`]): each lands on the link as a burst, and a
//! fixed 256 KiB would read as congestion on any link that cannot swallow it inside the
//! gauge's delay budget. A file below `TRANSFER_MIN_GAUGED_BYTES` skips the gauge, as does a
//! request with no gauge-able session socket; with no cap either, nothing is paced.

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_encode};
use serde_json::json;
use tokio::io::AsyncReadExt;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use super::pacing::{TRANSFER_MIN_GAUGED_BYTES, TransferPacer, UplinkGauge, gauged_chunk_size};
use super::{AuditSink, FilesState, RequestAuth};

/// `urllib.parse.quote`'s default: `/` and the unreserved characters pass, everything else is
/// percent-encoded.
pub const QUOTE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'_')
    .remove(b'.')
    .remove(b'-')
    .remove(b'~');

/// The headers that make a request conditional.
const CONDITIONAL: [HeaderName; 4] = [
    header::IF_MODIFIED_SINCE,
    header::IF_NONE_MATCH,
    header::IF_RANGE,
    header::IF_UNMODIFIED_SINCE,
];

/// Whether `headers` carry a conditional header.
pub fn is_conditional(headers: &HeaderMap) -> bool {
    CONDITIONAL.iter().any(|h| headers.contains_key(h))
}

/// The `Content-Disposition` of an attachment named by the bytes `name`: the quoted-string
/// fallback takes printable ASCII alone (a control character, a legal Linux filename byte,
/// would break the header, and a quote or backslash would end or escape the string), and
/// `filename*` carries the name percent-encoded as UTF-8.
pub fn attachment_disposition(name: &[u8]) -> (String, HeaderValue) {
    let filename: String = String::from_utf8_lossy(name)
        .chars()
        .map(|c| {
            if (' '..='~').contains(&c) && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let quoted = percent_encode(name, QUOTE).to_string();
    let value = format!("attachment; filename=\"{filename}\"; filename*=UTF-8''{quoted}");
    (
        filename,
        HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    )
}

/// A file to hand to the page.
pub struct Download {
    /// The file on disk, for the plain file response.
    pub path: std::path::PathBuf,
    /// The file already open, for the paced stream.
    pub file: std::fs::File,
    pub size: u64,
    /// The name the audit trail records.
    pub audit_name: String,
    /// The name the attachment is offered under, as the filesystem spells it.
    pub download_name: Vec<u8>,
    /// Whether a request the paced stream cannot answer may go to the plain file response;
    /// `false` for a file that exists only for the length of the transfer, where such a
    /// request is refused with 405.
    pub allow_fallback: bool,
}

/// Stream `download` to the page as an attachment with the download pacing, or answer with
/// the plain audited file response when nothing is paced and `allow_fallback` allows it.
pub async fn serve_paced_download(
    state: &Arc<FilesState>,
    auth: &RequestAuth,
    method: &Method,
    headers: &HeaderMap,
    download: Download,
) -> Response {
    let (filename, disposition) = attachment_disposition(&download.download_name);
    let cap = state
        .transfer_cap
        .active()
        .then(|| state.transfer_cap.clone());
    let gauge = if download.size >= TRANSFER_MIN_GAUGED_BYTES {
        state.session_gauge(auth)
    } else {
        None
    };
    let paced = (cap.is_some() || gauge.is_some() || !download.allow_fallback)
        && method == Method::GET
        && !headers.contains_key(header::RANGE)
        && !is_conditional(headers);
    if !paced {
        if !download.allow_fallback {
            return (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "GET")]).into_response();
        }
        let serve = FileServe {
            path: &download.path,
            served_name: &download.audit_name,
            event: "file.download",
            remove: false,
            extra: vec![(header::CONTENT_DISPOSITION, disposition)],
        };
        return audited_file_response(state, method, headers, serve).await;
    }
    log::info!(
        target: "server",
        "Download '{filename}' ({} bytes): {}{}",
        download.size,
        if gauge.is_some() { "gauged" } else { "ungauged" },
        cap.as_ref().map(|c| format!(", capped at {:.1} Mbit/s", c.static_bps() as f64 / 125_000.0)).unwrap_or_default()
    );
    let content_type = mime_guess::from_path(Path::new(
        &String::from_utf8_lossy(&download.download_name).into_owned(),
    ))
    .first_raw()
    .unwrap_or("application/octet-stream");
    let stream = Streaming {
        file: tokio::fs::File::from_std(download.file),
        sent: 0,
        size: download.size,
        filename,
        audit_name: download.audit_name,
        cap,
        gauge,
        pacer: state.download_pacer.clone(),
        audit: state.hooks.lock().audit.clone(),
        done: false,
    };
    let body = Body::from_stream(futures::stream::unfold(stream, |mut s| async move {
        match s.step().await {
            Ok(Some(chunk)) => Some((Ok(chunk), s)),
            Ok(None) => None,
            Err(e) => Some((Err(e), s)),
        }
    }));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, download.size)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// One paced download in flight; dropped before the end, it logs the client's stop.
struct Streaming {
    file: tokio::fs::File,
    sent: u64,
    size: u64,
    filename: String,
    audit_name: String,
    cap: Option<Arc<TransferPacer>>,
    gauge: Option<UplinkGauge>,
    pacer: Arc<TransferPacer>,
    audit: Option<Arc<dyn AuditSink>>,
    done: bool,
}

impl Streaming {
    /// The next chunk, paced; `None` at the end, which records the download.
    async fn step(&mut self) -> std::io::Result<Option<Bytes>> {
        let chunk_size = if self.gauge.is_some() {
            gauged_chunk_size(&self.pacer)
        } else {
            1 << 20
        };
        let mut buf = vec![0u8; chunk_size];
        let n = self.file.read(&mut buf).await?;
        if n == 0 {
            self.done = true;
            if let Some(audit) = &self.audit
                && let serde_json::Value::Object(fields) =
                    json!({"filename": self.audit_name, "size_bytes": self.size, "partial": false})
            {
                audit.emit("file.download", fields);
            }
            return Ok(None);
        }
        buf.truncate(n);
        if let Some(cap) = &self.cap {
            cap.pace(n).await;
        }
        if let Some(gauge) = &mut self.gauge {
            let verdict = gauge.sample();
            if gauge.alive() {
                let inflation = gauge.inflation_us();
                self.pacer.pace_verdict(n, verdict, Some(inflation)).await;
            } else {
                self.gauge = None;
            }
        }
        self.sent += n as u64;
        Ok(Some(Bytes::from(buf)))
    }
}

impl Drop for Streaming {
    fn drop(&mut self) {
        if !self.done {
            log::info!(
                target: "server",
                "Download '{}' stopped by the client after {} of {} bytes: the connection closed",
                self.filename, self.sent, self.size
            );
        }
    }
}

/// A file the plain file response serves (`audited_file_response`).
pub struct FileServe<'a> {
    /// The file on disk.
    pub path: &'a Path,
    /// The name the audit trail records.
    pub served_name: &'a str,
    /// The audit event: `file.download`, or `print.document`.
    pub event: &'a str,
    /// Delete the file once it went out whole.
    pub remove: bool,
    /// Headers added to the response, the `Content-Disposition` among them.
    pub extra: Vec<(HeaderName, HeaderValue)>,
}

/// A file served whole or in part by the plain file response, recorded for the audit as
/// `event` once it goes out: the whole file or the range asked for, with the bytes served. A
/// `HEAD` serves headers alone and records nothing. With `remove`, a file that went out whole
/// is deleted; the response holds the open file, so the deletion takes nothing from it.
pub async fn audited_file_response(
    state: &Arc<FilesState>,
    method: &Method,
    headers: &HeaderMap,
    serve: FileServe<'_>,
) -> Response {
    let mut builder = Request::builder().method(method.clone()).uri("/");
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    let Ok(request) = builder.body(Body::empty()) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let response = match ServeFile::new(serve.path).oneshot(request).await {
        Ok(response) => response,
        Err(e) => {
            log::warn!(target: "server", "Serving {} failed: {e}", serve.path.display());
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let mut response = response.map(Body::new);
    for (name, value) in serve.extra {
        response.headers_mut().insert(name, value);
    }
    let status = response.status();
    if method == Method::GET && matches!(status, StatusCode::OK | StatusCode::PARTIAL_CONTENT) {
        let size = response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        state.audit(
            serve.event,
            json!({"filename": serve.served_name, "size_bytes": size, "partial": status == StatusCode::PARTIAL_CONTENT}),
        );
        if serve.remove && status == StatusCode::OK {
            let _ = std::fs::remove_file(serve.path);
        }
    }
    response
}
