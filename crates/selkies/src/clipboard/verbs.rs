/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The clipboard verbs a client sends: `cw`/`cb` for one text or binary payload, the
//! `cws`/`cwd`/`cwe` and `cbs`/`cbd`/`cbe` multipart transfers, `cr` for a fetch, and
//! `REQUEST_CLIPBOARD` for the copy a keyboard shortcut asks the session for.
//!
//! Policy is enforced here, so a modified client cannot move content a direction the
//! operator closed: inbound verbs need `in`, outbound ones `out`, and a picture needs the
//! binary switch while the flavor envelope, being text, does not. A multipart transfer is
//! bounded by its declared size and by what it accumulates, one transfer at a time, a new
//! start aborting the previous one and any malformed frame closing it, so a client cannot
//! balloon memory or leave the state half-open.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use log::{debug, error, warn};

use super::{ClientId, Clipboard, FLAVORS_MIME, Payload, flavors};

/// Bound on one multipart clipboard transfer; the declared size and the accumulated chunks
/// are both checked.
pub const MULTIPART_MAX_BYTES: usize = 64 * 1024 * 1024;
/// `REQUEST_CLIPBOARD` (Ctrl/Cmd+C) debounce, per requesting connection, so a keypress storm
/// cannot stack clipboard reads and one client's copy cannot suppress another's.
const REQUEST_DEBOUNCE: Duration = Duration::from_millis(250);
/// How long a `REQUEST_CLIPBOARD` read that still matches the baseline waits for the owner
/// change the injected Ctrl+C raises before re-reading.
const REQUEST_SETTLE: Duration = Duration::from_millis(150);

/// Which kind of multipart transfer is in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Text,
    Binary,
}

/// The one multipart receive in progress.
#[derive(Default)]
pub(crate) struct Multipart {
    transfer: Option<Transfer>,
}

impl Multipart {
    /// Whether a transfer is open.
    #[cfg(test)]
    pub(super) fn in_progress(&self) -> bool {
        self.transfer.is_some()
    }
}

struct Transfer {
    id: String,
    kind: Kind,
    mime: String,
    declared: usize,
    buffer: Vec<u8>,
}

/// Per-client fetch bookkeeping: the `REQUEST_CLIPBOARD` debounce and the `cr` reads in
/// flight, since a repeat from a client whose read still waits on the selection owner would
/// only queue another blocking thread behind the same owner.
#[derive(Default)]
pub(crate) struct Requests {
    last_request: HashMap<ClientId, Instant>,
    reads: HashSet<ClientId>,
}

impl Clipboard {
    /// Handle one client message; false when it is not a clipboard verb.
    ///
    /// Writes are awaited in-line: a paste keystroke right behind a transfer must find the
    /// clipboard set. Reads are answered as tasks, so an owner that never converts the
    /// selection cannot stall the dispatch.
    pub async fn on_message(self: &Arc<Self>, client: ClientId, msg: &str) -> bool {
        let toks: Vec<&str> = msg.split(',').collect();
        let policy = self.policy();
        match toks[0] {
            "cws" | "cbs" => {
                let binary = toks[0] == "cbs";
                let mime = if binary {
                    toks.get(2).copied()
                } else {
                    Some("text/plain")
                };
                let allowed =
                    policy.inbound() && (!binary || mime == Some(FLAVORS_MIME) || policy.binary);
                if !allowed {
                    warn!(target: "input", "Rejecting multi-part clipboard write: inbound clipboard disabled.");
                    return true;
                }
                let size_tok = if binary { toks.get(3) } else { toks.get(2) };
                let parsed = match (
                    toks.get(1),
                    mime,
                    size_tok.and_then(|s| s.parse::<i64>().ok()),
                ) {
                    (Some(id), Some(mime), Some(size)) => Some((id, mime, size)),
                    _ => None,
                };
                let Some((id, mime, declared)) = parsed else {
                    error!(target: "input", "Invalid {} message: {msg}", toks[0]);
                    return true;
                };
                if declared < 0 || declared as usize > MULTIPART_MAX_BYTES {
                    error!(
                        target: "input",
                        "Rejecting multi-part clipboard write: declared size {declared} out of bounds (max {MULTIPART_MAX_BYTES})."
                    );
                    return true;
                }
                let mut inbound = self.inbound.lock();
                if let Some(previous) = &inbound.transfer
                    && previous.id != *id
                {
                    warn!(
                        target: "input",
                        "Aborting previous in-progress clipboard transfer {} for new transfer {id}.",
                        previous.id
                    );
                }
                inbound.transfer = Some(Transfer {
                    id: id.to_string(),
                    kind: if binary { Kind::Binary } else { Kind::Text },
                    mime: mime.to_string(),
                    declared: declared as usize,
                    buffer: Vec::new(),
                });
                if binary {
                    debug!(target: "input", "Starting multi-part binary clipboard receive ({mime}), total size: {declared}");
                } else {
                    debug!(target: "input", "Starting multi-part text clipboard receive, total size: {declared}");
                }
            }
            "cwd" | "cbd" => {
                let expected = if toks[0] == "cwd" {
                    Kind::Text
                } else {
                    Kind::Binary
                };
                let mut inbound = self.inbound.lock();
                if toks.len() < 3 {
                    warn!(target: "input", "Malformed clipboard chunk ({}): missing fields; aborting transfer.", toks[0]);
                    inbound.transfer = None;
                    return true;
                }
                let Some(transfer) = inbound
                    .transfer
                    .as_mut()
                    .filter(|t| t.id == toks[1] && t.kind == expected)
                else {
                    warn!(target: "input", "Ignoring mismatched clipboard chunk ({}): id/kind does not match active transfer.", toks[0]);
                    return true;
                };
                match BASE64.decode(toks[2]) {
                    Ok(chunk) => {
                        if transfer.buffer.len() + chunk.len() > transfer.declared {
                            error!(target: "input", "Multi-part clipboard exceeded its declared size; aborting transfer.");
                            inbound.transfer = None;
                            return true;
                        }
                        let _ = transfer.buffer.write_all(&chunk);
                    }
                    Err(e) => {
                        error!(target: "input", "Failed to process clipboard data chunk: {e}");
                        inbound.transfer = None;
                    }
                }
            }
            "cwe" | "cbe" => {
                let expected = if toks[0] == "cwe" {
                    Kind::Text
                } else {
                    Kind::Binary
                };
                let finished = {
                    let mut inbound = self.inbound.lock();
                    if toks.len() < 2 {
                        warn!(target: "input", "Malformed clipboard end ({}): missing id; aborting transfer.", toks[0]);
                        inbound.transfer = None;
                        return true;
                    }
                    let matches = inbound
                        .transfer
                        .as_ref()
                        .is_some_and(|t| t.id == toks[1] && t.kind == expected);
                    if !matches {
                        warn!(target: "input", "Ignoring mismatched clipboard end ({}): id/kind does not match active transfer.", toks[0]);
                        return true;
                    }
                    inbound.transfer.take()
                };
                let Some(transfer) = finished else {
                    return true;
                };
                let received = transfer.buffer.len();
                if received != transfer.declared {
                    error!(
                        target: "input",
                        "Multi-part clipboard size mismatch. Expected {}, got {received}. Aborting.",
                        transfer.declared
                    );
                    return true;
                }
                debug!(target: "input", "Finished multi-part clipboard receive. Total size: {received}");
                self.on_incoming(&transfer.mime, Bytes::from(transfer.buffer), true)
                    .await;
            }
            "cr" => {
                if !policy.outbound() {
                    warn!(target: "input", "Rejecting clipboard read: outbound clipboard disabled.");
                    return true;
                }
                if !self.requests.lock().reads.insert(client) {
                    debug!(target: "input", "Clipboard read-back already in flight; dropping the repeat");
                    return true;
                }
                let me = self.clone();
                tokio::spawn(async move { me.answer_read(client, policy.binary).await });
            }
            "REQUEST_CLIPBOARD" => {
                if !policy.outbound() {
                    warn!(target: "input", "Rejecting REQUEST_CLIPBOARD: outbound clipboard disabled.");
                    return true;
                }
                let now = Instant::now();
                let debounced = {
                    let mut requests = self.requests.lock();
                    if requests.last_request.len() > 64 {
                        requests
                            .last_request
                            .retain(|_, at| now.duration_since(*at) < REQUEST_DEBOUNCE);
                    }
                    let recent = requests
                        .last_request
                        .get(&client)
                        .is_some_and(|at| now.duration_since(*at) < REQUEST_DEBOUNCE);
                    if !recent {
                        requests.last_request.insert(client, now);
                    }
                    recent
                };
                if debounced {
                    debug!(target: "input", "Debouncing REQUEST_CLIPBOARD (too frequent).");
                    return true;
                }
                let me = self.clone();
                tokio::spawn(async move { me.send_requested(client, policy.binary).await });
            }
            "cb" => {
                let mime = toks.get(1).copied();
                if !(policy.inbound() && (mime == Some(FLAVORS_MIME) || policy.binary)) {
                    warn!(target: "input", "Rejecting binary clipboard write: inbound binary clipboard disabled.");
                    return true;
                }
                if toks.len() != 3 {
                    error!(target: "input", "Binary clipboard write error: expected mime and payload");
                    return true;
                }
                match BASE64.decode(toks[2]) {
                    Ok(data) => {
                        self.on_incoming(toks[1], Bytes::from(data), false).await;
                    }
                    Err(e) => error!(target: "input", "Binary clipboard write error: {e}"),
                }
            }
            "cw" => {
                if !policy.inbound() {
                    warn!(target: "input", "Rejecting clipboard write: inbound clipboard disabled.");
                    return true;
                }
                match toks.get(1).map(|t| BASE64.decode(t)) {
                    Some(Ok(data)) => {
                        // Invalid sequences are dropped, as the browser's own decoder would.
                        let text: String = data.utf8_chunks().map(|c| c.valid()).collect();
                        self.on_incoming("text/plain", Bytes::from(text), false)
                            .await;
                    }
                    Some(Err(e)) => error!(target: "input", "Clipboard decode error: {e}"),
                    None => error!(target: "input", "Clipboard decode error: missing payload"),
                }
            }
            _ => return false,
        }
        true
    }

    /// Abandon the multipart transfer in progress, so nothing of it bleeds into the next one:
    /// the input handler's reset, on a session's teardown.
    pub fn reset_inbound(&self) {
        self.inbound.lock().transfer = None;
    }

    /// Content a client pushed, decoded: a flavor envelope is unpacked and written as its
    /// markup with every flavor offered, anything else as it is. True when the clipboard took
    /// it; the audit collector hears of a transfer that did.
    pub async fn on_incoming(self: &Arc<Self>, mime: &str, data: Bytes, multipart: bool) -> bool {
        let (payload, flavors) = if mime == FLAVORS_MIME {
            match flavors::flavors(&data) {
                Ok(entries) => {
                    let (mime, bytes) = entries[0].clone();
                    (Payload::binary(mime, bytes), Some(entries))
                }
                Err(e) => {
                    if multipart {
                        error!(target: "input", "Multi-part clipboard flavors rejected: {e}");
                    } else {
                        error!(target: "input", "Binary clipboard write error: {e}");
                    }
                    return false;
                }
            }
        } else {
            (Payload::binary(mime, data), None)
        };
        let size = payload.bytes.len();
        let mime = payload.mime.clone();
        if !self.write(payload, flavors).await {
            return false;
        }
        self.host
            .audit("clipboard.receive", &mime, size, Some(multipart));
        match (multipart, mime.as_str()) {
            (true, "text/plain") => {
                debug!(target: "input", "Set multi-part clipboard content, length: {size}")
            }
            (true, m) => {
                debug!(target: "input", "Set multi-part binary clipboard content ({m}), size: {size} bytes")
            }
            (false, "text/plain") => {
                debug!(target: "input", "Set clipboard content, length: {size}")
            }
            (false, m) => {
                debug!(target: "input", "Set binary clipboard content ({m}), size: {size} bytes")
            }
        }
        true
    }

    /// Answer a `cr`: the content tagged `clipboard_reply,cr` to the asking client alone, so
    /// it treats the payload cache-only without its connect-time heuristic; an empty tagged
    /// reply when there is none, since the tag settles the client's connect-time fetch and a
    /// real change seconds later is then not taken for the snapshot.
    async fn answer_read(&self, client: ClientId, use_binary: bool) {
        let payload = self.read(use_binary).await;
        match payload {
            Some(p) if !p.is_empty() => self.send(p, Some("cr"), Some(client)).await,
            _ => {
                debug!(target: "input", "No clipboard content; sending empty tagged reply");
                self.send(Payload::text(""), Some("cr"), Some(client)).await;
            }
        }
        self.requests.lock().reads.remove(&client);
    }

    /// Read and push the clipboard for a `REQUEST_CLIPBOARD`. A read that still matches the
    /// baseline races the injected Ctrl+C (the app has not published the new selection yet),
    /// so it waits briefly for the owner change, without consuming the edge the monitor loop
    /// broadcasts on, and re-reads.
    async fn send_requested(&self, client: ClientId, use_binary: bool) {
        let mut payload = self.read(use_binary).await;
        if let Some(p) = &payload
            && self.baseline().as_ref() == Some(&p.bytes)
        {
            let monitor = self.x11.monitor.lock().clone().filter(|m| m.alive());
            match monitor {
                Some(m) => {
                    m.changed().wait(REQUEST_SETTLE).await;
                }
                None => tokio::time::sleep(REQUEST_SETTLE).await,
            }
            payload = self.read(use_binary).await;
        }
        match payload {
            Some(p) if !p.is_empty() => self.send(p, None, Some(client)).await,
            _ => debug!(target: "input", "No clipboard content to send on REQUEST_CLIPBOARD."),
        }
    }
}
