/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The `xclip` rung: the X11 fallback while the XFixes monitor cannot be built.
//!
//! Every fork is bounded: a deadline covers the stdin write and the exit together, and a
//! process past it is killed and reaped, so a payload larger than the pipe buffer cannot
//! wedge on a stalled `xclip`. A missing binary warns once, not per poll.

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use log::warn;
use parking_lot::Mutex;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::{IMAGE_MIMES, Payload, SECRET_HINTS, flavors, secret_hint, x11};

/// Why an `xclip` run failed.
enum Failure {
    /// The binary is not installed.
    Missing,
    /// Anything else: a spawn error, a timeout, a broken pipe.
    Other(String),
}

/// The `xclip` fallback rung and its once-only missing warning.
pub struct Xclip {
    /// The program run; a test points it at a name that does not exist.
    pub(crate) program: Mutex<String>,
    missing_warned: AtomicBool,
}

impl Default for Xclip {
    fn default() -> Self {
        Xclip {
            program: Mutex::new("xclip".into()),
            missing_warned: AtomicBool::new(false),
        }
    }
}

impl Xclip {
    /// Run `xclip` with `args`, feeding `stdin`, within `timeout`: the output on exit 0,
    /// None on another exit code.
    async fn run(
        &self,
        display: Option<&str>,
        args: &[&str],
        stdin: Option<Vec<u8>>,
        timeout: Duration,
        what: &str,
    ) -> Result<Option<Vec<u8>>, Failure> {
        let program = self.program.lock().clone();
        let mut command = Command::new(program);
        command.args(args).stderr(Stdio::null()).kill_on_drop(true);
        // `xclip -i` forks a daemon to serve the selection, which inherits stdout and would
        // hold a pipe open until another owner takes over.
        command.stdout(if stdin.is_some() {
            Stdio::null()
        } else {
            Stdio::piped()
        });
        if let Some(display) = display {
            command.env("DISPLAY", display);
        }
        command.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let lang_ok = std::env::var("LANG").is_ok_and(|l| l != "C");
        if !lang_ok {
            command.env("LANG", "C.UTF-8");
        }
        let mut child = command.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Failure::Missing
            } else {
                Failure::Other(e.to_string())
            }
        })?;
        let pid = child.id();
        if let Some(bytes) = stdin
            && let Some(mut pipe) = child.stdin.take()
        {
            tokio::spawn(async move {
                let _ = pipe.write_all(&bytes).await;
                let _ = pipe.shutdown().await;
            });
        }
        let mut stdout = child.stdout.take();
        let output = async {
            let mut buf = Vec::new();
            if let Some(out) = stdout.as_mut() {
                let _ = tokio::io::AsyncReadExt::read_to_end(out, &mut buf).await;
            }
            let status = child.wait().await;
            (status, buf)
        };
        match tokio::time::timeout(timeout, output).await {
            Ok((Ok(status), buf)) => Ok(status.success().then_some(buf)),
            Ok((Err(e), _)) => Err(Failure::Other(e.to_string())),
            Err(_) => {
                warn!(
                    target: "input",
                    "Timed out waiting for clipboard command '{what}' pid={}; killing it.",
                    pid.map_or("unknown".to_string(), |p| p.to_string())
                );
                Err(Failure::Other(format!("timed out waiting for {what}")))
            }
        }
    }

    /// Say once that `xclip` is not installed, naming what the rung could not do.
    fn warn_missing(&self, verb: &str) {
        if !self.missing_warned.swap(true, Ordering::Relaxed) {
            warn!(target: "input", "xclip is not installed; the clipboard polling rung has nothing to {verb} with.");
        }
    }

    /// Read the CLIPBOARD selection through `xclip`: the targets first, then the same
    /// precedence as the native rungs (a secret as its text alone, a copy whose markup
    /// carries text as its plain text, else the first picture, a file manager's uri-list, and
    /// the text). None when nothing is readable.
    pub async fn read(&self, use_binary: bool, display: Option<&str>) -> Option<Payload> {
        match self.read_inner(use_binary, display).await {
            Ok(payload) => payload,
            Err(Failure::Missing) => {
                self.warn_missing("read");
                None
            }
            Err(Failure::Other(e)) => {
                warn!(target: "input", "Error reading clipboard with xclip: {e}");
                None
            }
        }
    }

    /// The read `read` describes, its failures left for it to report.
    async fn read_inner(
        &self,
        use_binary: bool,
        display: Option<&str>,
    ) -> Result<Option<Payload>, Failure> {
        let out =
            |target: &str| ["-selection", "clipboard", "-o", "-t", target].map(str::to_string);
        let targets_out = self
            .run(
                display,
                &out("TARGETS").each_ref().map(String::as_str),
                None,
                Duration::from_secs(1),
                "xclip TARGETS",
            )
            .await?;
        let Some(targets_out) = targets_out else {
            return Ok(None);
        };
        let targets: Vec<String> = String::from_utf8_lossy(&targets_out)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        let has = |m: &str| targets.iter().any(|t| t == m);
        let fetch = |target: &str, secs: u64, what: String| {
            let args = out(target);
            async move {
                self.run(
                    display,
                    &args.each_ref().map(String::as_str),
                    None,
                    Duration::from_secs(secs),
                    &what,
                )
                .await
            }
        };
        let mut secret = false;
        if let Some(hint) = SECRET_HINTS.iter().find(|h| has(h)) {
            secret = fetch(hint, 1, "xclip password-manager hint".into())
                .await?
                .is_some_and(|v| secret_hint(&v));
        }
        let mut text_copy = false;
        if use_binary
            && !secret
            && has("text/html")
            && targets.iter().any(|t| t.starts_with("image/"))
        {
            text_copy = fetch("text/html", 1, "xclip text/html".into())
                .await?
                .is_some_and(|h| flavors::markup_has_text(&h));
        }
        if use_binary && !secret && !text_copy {
            for mime in IMAGE_MIMES {
                if has(mime)
                    && let Some(data) = fetch(mime, 3, format!("xclip {mime}")).await?
                    && !data.is_empty()
                {
                    return Ok(Some(Payload::binary(mime, data)));
                }
            }
            if has("text/uri-list")
                && let Some(data) = fetch("text/uri-list", 1, "xclip text/uri-list".into()).await?
                && !data.is_empty()
            {
                let resolved =
                    tokio::task::spawn_blocking(move || x11::resolve_uri_list_image(&data))
                        .await
                        .ok()
                        .flatten();
                if let Some((bytes, mime)) = resolved {
                    return Ok(Some(Payload::binary(mime, bytes)));
                }
            }
        }
        if has("UTF8_STRING")
            && let Some(text) = fetch("UTF8_STRING", 1, "xclip UTF8_STRING".into()).await?
        {
            let text = String::from_utf8_lossy(&text).into_owned();
            return Ok(Some(if secret {
                Payload::secret(text)
            } else {
                Payload::text(text)
            }));
        }
        Ok(secret.then(|| Payload::secret("")))
    }

    /// Offer `payload` on the CLIPBOARD selection through `xclip -i`; true when it took it.
    pub async fn write(&self, payload: &Payload, display: Option<&str>) -> bool {
        let target = if payload.is_text() {
            "UTF8_STRING".to_string()
        } else {
            payload.mime.clone()
        };
        let args = ["-selection", "clipboard", "-i", "-t", &target];
        match self
            .run(
                display,
                &args,
                Some(payload.bytes.to_vec()),
                Duration::from_secs(2),
                &format!("xclip -i {target}"),
            )
            .await
        {
            Ok(Some(_)) => true,
            Ok(None) => {
                warn!(target: "input", "xclip process exited with non-zero code.");
                false
            }
            Err(Failure::Missing) => {
                self.warn_missing("write");
                false
            }
            Err(Failure::Other(e)) if e.starts_with("timed out") => {
                warn!(target: "input", "Timeout waiting for xclip process to terminate.");
                false
            }
            Err(Failure::Other(e)) => {
                warn!(target: "input", "Error writing to clipboard with xclip: {e}");
                false
            }
        }
    }
}
