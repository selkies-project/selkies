/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Sound-server control operations over one connection, with a `pactl` fallback.
//!
//! [`AudioControl`] runs the provisioning operations both transports share over a
//! [`ControlBackend`]: the libpulse connection of `pulse.rs` by default, else `pactl`
//! subprocesses parsed by [`parse_pactl_list`]. It opens lazily on first use (or explicitly with
//! `open`), releases with `close`, and every public operation reports failure through its return
//! value and a log line rather than an error, so a missing or wedged sound server degrades audio
//! instead of aborting the caller. A lost connection is reopened once on the next operation; a
//! timeout abandons the connection and answers the operation's default.
//!
//! The backend behind a control is injectable (a [`Connector`] for the connection, a
//! [`PactlRunner`] for the subprocesses) so the provisioning logic and the never-cancel
//! discipline are testable against a scripted server.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::future::BoxFuture;
use log::{debug, error, info, warn};
use tokio::process::Command;

use super::pulse::PulseBackend;
use super::{
    PCMFLUX_APP_NAME, PIPEWIRE_NULL_MONITOR, VIRTUAL_MIC_MASTER, VIRTUAL_MIC_SINK,
    VIRTUAL_MIC_SOURCE, VIRTUAL_MIC_SOURCE_NAMES, capture_channels, capture_sink_name,
    surround_sink_map,
};

/// How long a freshly loaded module's object is polled for before it is reported missing:
/// PipeWire creates sinks and sources asynchronously.
const APPEAR_TIMEOUT: Duration = Duration::from_secs(2);
const APPEAR_POLL: Duration = Duration::from_millis(100);

/// One sink, source, or source-output as either backend reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PulseNode {
    /// Server object index.
    pub index: u32,
    /// Object name (a source-output's media name).
    pub name: String,
    /// Index of the module that created it, when known.
    pub owner_module: Option<u32>,
    /// Property list; `application.name` identifies a stream's client, `device.master_device`
    /// a virtual source's master.
    pub proplist: HashMap<String, String>,
    /// For a source-output, the index of the source it records from.
    pub source: Option<u32>,
    /// For a source-output, whether the stream is suspended, as one an application opened but
    /// is not recording from is.
    pub corked: bool,
}

/// Why a control operation failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AudioControlError {
    /// No connection, or the connection closed under the operation.
    #[error("not connected to the sound server")]
    NotConnected,
    /// The server did not answer within the operation timeout; the connection was abandoned.
    #[error("the sound server did not answer in time")]
    Timeout,
    /// The server refused the operation, or `pactl` failed.
    #[error("{0}")]
    Failed(String),
}

/// The operation set of a backend; one method per server call.
#[async_trait]
pub trait ControlBackend: Send {
    /// Whether the connection is still up; `pactl` is always up.
    fn connected(&self) -> bool;
    async fn sink_list(&mut self) -> Result<Vec<PulseNode>, AudioControlError>;
    async fn source_list(&mut self) -> Result<Vec<PulseNode>, AudioControlError>;
    async fn source_output_list(&mut self) -> Result<Vec<PulseNode>, AudioControlError>;
    /// `(default sink name, default source name)`.
    async fn server_defaults(
        &mut self,
    ) -> Result<(Option<String>, Option<String>), AudioControlError>;
    async fn module_load(&mut self, name: &str, args: &str) -> Result<u32, AudioControlError>;
    async fn module_unload(&mut self, index: u32) -> Result<(), AudioControlError>;
    async fn sink_default_set(&mut self, name: &str) -> Result<(), AudioControlError>;
    async fn source_default_set(&mut self, name: &str) -> Result<(), AudioControlError>;
    async fn source_output_move(
        &mut self,
        output_index: u32,
        source_index: u32,
    ) -> Result<(), AudioControlError>;
    /// Let in-flight operations finish, then release the connection.
    async fn close(&mut self);
}

/// Opens a connection backend: the libpulse thread by default, a stub in tests. Takes the
/// client name, the connect timeout, and the operation timeout; the error is the reason the
/// fallback is announced with.
pub type Connector = Arc<
    dyn Fn(
            String,
            Duration,
            Duration,
        ) -> BoxFuture<'static, Result<Box<dyn ControlBackend>, String>>
        + Send
        + Sync,
>;

/// Runs one `pactl` command (C locale) and returns its stdout.
#[async_trait]
pub trait PactlRunner: Send + Sync {
    async fn run(&self, args: &[String], timeout: Duration) -> Result<String, AudioControlError>;
}

/// The real `pactl`: a subprocess killed when it outlives the timeout rather than left blocked
/// on a stuck server.
struct PactlProcess;

#[async_trait]
impl PactlRunner for PactlProcess {
    async fn run(&self, args: &[String], timeout: Duration) -> Result<String, AudioControlError> {
        let command = format!("pactl {}", args.join(" "));
        let child = Command::new("pactl")
            .args(args)
            .env("LC_ALL", "C")
            .env("LANGUAGE", "C")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| AudioControlError::Failed(format!("pactl unavailable: {e}")))?;
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Err(_) => return Err(AudioControlError::Failed(format!("{command} timed out"))),
            Ok(Err(e)) => return Err(AudioControlError::Failed(format!("{command} failed: {e}"))),
            Ok(Ok(output)) => output,
        };
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(AudioControlError::Failed(format!(
                "{command} failed: {err}"
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// The object kinds `pactl list` prints blocks for.
const PACTL_BLOCK_KINDS: [&str; 6] = [
    "Sink",
    "Source",
    "Source Output",
    "Sink Input",
    "Module",
    "Client",
];

/// The index of a `pactl list` block header such as `Source Output #12`.
fn pactl_block_index(line: &str) -> Option<u32> {
    let line = line.trim_end();
    let (kind, index) = line.rsplit_once(" #")?;
    if !PACTL_BLOCK_KINDS.contains(&kind) || index.is_empty() {
        return None;
    }
    index.parse().ok()
}

/// Parse `pactl list <kind>` output (C locale) into [`PulseNode`] records.
///
/// A block's fields are the single-tab `Key: value` lines; its properties the double-tab
/// `key = "value"` lines under `Properties:`; other nested lists (`Formats:`) are left out.
pub fn parse_pactl_list(text: &str) -> Vec<PulseNode> {
    let mut nodes: Vec<PulseNode> = Vec::new();
    let mut fields: HashMap<String, String> = HashMap::new();
    let mut in_props = false;
    for line in text.lines() {
        if !line.starts_with('\t') {
            if let Some(index) = pactl_block_index(line) {
                nodes.push(PulseNode {
                    index,
                    ..PulseNode::default()
                });
                fields.clear();
                in_props = false;
            }
            continue;
        }
        let Some(node) = nodes.last_mut() else {
            continue;
        };
        if line.starts_with("\t\t") {
            if in_props && let Some((key, value)) = line.trim().split_once(" = ") {
                node.proplist
                    .insert(key.to_string(), value.trim().trim_matches('"').to_string());
            }
            continue;
        }
        let trimmed = line.trim();
        let (key, value) = match trimmed.split_once(':') {
            Some((k, v)) => (k, Some(v.trim())),
            None => (trimmed, None),
        };
        in_props = key == "Properties";
        if in_props {
            continue;
        }
        if let Some(value) = value {
            fields.insert(key.to_string(), value.to_string());
            match key {
                "Name" => node.name = value.to_string(),
                "Owner Module" => node.owner_module = value.parse().ok(),
                "Source" => node.source = value.parse().ok(),
                "Corked" => node.corked = value.eq_ignore_ascii_case("yes"),
                _ => {}
            }
        }
    }
    nodes
}

/// `pactl` subprocesses with the same operation set as the bindings.
struct PactlBackend {
    runner: Arc<dyn PactlRunner>,
    timeout: Duration,
}

impl PactlBackend {
    async fn run(&self, args: &[&str]) -> Result<String, AudioControlError> {
        debug!(target: "audio", "pactl fallback: pactl {}", args.join(" "));
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        self.runner.run(&args, self.timeout).await
    }
}

#[async_trait]
impl ControlBackend for PactlBackend {
    fn connected(&self) -> bool {
        true
    }

    async fn sink_list(&mut self) -> Result<Vec<PulseNode>, AudioControlError> {
        Ok(parse_pactl_list(&self.run(&["list", "sinks"]).await?))
    }

    async fn source_list(&mut self) -> Result<Vec<PulseNode>, AudioControlError> {
        Ok(parse_pactl_list(&self.run(&["list", "sources"]).await?))
    }

    async fn source_output_list(&mut self) -> Result<Vec<PulseNode>, AudioControlError> {
        Ok(parse_pactl_list(
            &self.run(&["list", "source-outputs"]).await?,
        ))
    }

    async fn server_defaults(
        &mut self,
    ) -> Result<(Option<String>, Option<String>), AudioControlError> {
        let mut sink = None;
        let mut source = None;
        for line in self.run(&["info"]).await?.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            let value = (!value.is_empty()).then(|| value.to_string());
            match key.trim() {
                "Default Sink" => sink = value,
                "Default Source" => source = value,
                _ => {}
            }
        }
        Ok((sink, source))
    }

    async fn module_load(&mut self, name: &str, args: &str) -> Result<u32, AudioControlError> {
        let mut argv = vec!["load-module", name];
        argv.extend(args.split_whitespace());
        let out = self.run(&argv).await?;
        out.trim().parse().map_err(|_| {
            AudioControlError::Failed(format!(
                "load-module {name} returned no index: {:?}",
                out.trim()
            ))
        })
    }

    async fn module_unload(&mut self, index: u32) -> Result<(), AudioControlError> {
        self.run(&["unload-module", &index.to_string()])
            .await
            .map(drop)
    }

    async fn sink_default_set(&mut self, name: &str) -> Result<(), AudioControlError> {
        self.run(&["set-default-sink", name]).await.map(drop)
    }

    async fn source_default_set(&mut self, name: &str) -> Result<(), AudioControlError> {
        self.run(&["set-default-source", name]).await.map(drop)
    }

    async fn source_output_move(
        &mut self,
        output_index: u32,
        source_index: u32,
    ) -> Result<(), AudioControlError> {
        self.run(&[
            "move-source-output",
            &output_index.to_string(),
            &source_index.to_string(),
        ])
        .await
        .map(drop)
    }

    async fn close(&mut self) {}
}

/// Which backend a control settled on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    /// The in-process libpulse connection.
    Pulse,
    /// `pactl` subprocesses.
    Pactl,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Pulse => "libpulse",
            BackendKind::Pactl => "pactl",
        }
    }
}

/// Whether any control of this process announced the fallback at WARNING yet; later controls
/// announce at DEBUG, so a server without a reachable control plane costs one warning.
static FALLBACK_ANNOUNCED: AtomicBool = AtomicBool::new(false);

/// Sound-server control operations over one connection, with a `pactl` fallback.
///
/// Open lazily on first use (or explicitly with [`open`](Self::open)) and released with
/// [`close`](Self::close). Every public operation reports failure through its return value and a
/// log line rather than an error, so a missing or wedged sound server degrades audio instead of
/// aborting the caller; a lost connection is reopened on the next operation.
pub struct AudioControl {
    client_name: String,
    connect_timeout: Duration,
    op_timeout: Duration,
    connector: Connector,
    pactl: Arc<dyn PactlRunner>,
    backend: Option<Box<dyn ControlBackend>>,
    kind: Option<BackendKind>,
    fallback_logged: bool,
}

impl AudioControl {
    /// A control shown as `client_name` in the server's client list, with a two-second
    /// handshake and five seconds per operation.
    pub fn new(client_name: &str) -> Self {
        Self::with_timeouts(client_name, Duration::from_secs(2), Duration::from_secs(5))
    }

    /// A control with its own bounds: `connect_timeout` on the server handshake, `op_timeout`
    /// on each operation before the connection is abandoned (or a `pactl` process killed).
    pub fn with_timeouts(
        client_name: &str,
        connect_timeout: Duration,
        op_timeout: Duration,
    ) -> Self {
        let connector: Connector = Arc::new(|name, connect_timeout, op_timeout| {
            Box::pin(async move {
                PulseBackend::connect(name, connect_timeout, op_timeout)
                    .await
                    .map(|b| Box::new(b) as Box<dyn ControlBackend>)
            })
        });
        AudioControl {
            client_name: client_name.to_string(),
            connect_timeout,
            op_timeout,
            connector,
            pactl: Arc::new(PactlProcess),
            backend: None,
            kind: None,
            fallback_logged: false,
        }
    }

    /// Replace how the connection backend is opened (a scripted server in tests).
    pub fn with_connector(mut self, connector: Connector) -> Self {
        self.connector = connector;
        self
    }

    /// Replace how `pactl` commands run (scripted output in tests).
    pub fn with_pactl_runner(mut self, runner: Arc<dyn PactlRunner>) -> Self {
        self.pactl = runner;
        self
    }

    /// The name the server lists this control under.
    pub fn client_name(&self) -> &str {
        &self.client_name
    }

    /// The backend once settled, else `None`.
    pub fn backend(&self) -> Option<BackendKind> {
        self.kind
    }

    fn announce_fallback(&mut self, reason: &str) {
        if self.fallback_logged {
            return;
        }
        self.fallback_logged = true;
        let message = format!("Sound server control: {reason}; using pactl subprocesses.");
        if FALLBACK_ANNOUNCED.swap(true, Ordering::AcqRel) {
            debug!(target: "audio", "{message}");
        } else {
            warn!(target: "audio", "{message}");
        }
    }

    /// Settle the backend: connect over libpulse, else engage the `pactl` fallback.
    ///
    /// Returns true when a backend is ready; false when neither libpulse nor `pactl` can reach
    /// a server. The fallback, once engaged, stays until [`close`](Self::close).
    pub async fn open(&mut self) -> bool {
        match self.kind {
            Some(BackendKind::Pactl) => return true,
            Some(BackendKind::Pulse) if self.backend.as_ref().is_some_and(|b| b.connected()) => {
                return true;
            }
            _ => {}
        }
        if let Some(mut stale) = self.backend.take() {
            stale.close().await;
        }
        let connect = (self.connector)(
            self.client_name.clone(),
            self.connect_timeout,
            self.op_timeout,
        );
        match connect.await {
            Ok(backend) => {
                self.backend = Some(backend);
                self.kind = Some(BackendKind::Pulse);
                return true;
            }
            Err(e) => self.announce_fallback(&format!("connection failed ({e})")),
        }
        let mut pactl = PactlBackend {
            runner: self.pactl.clone(),
            timeout: self.op_timeout,
        };
        let probe = pactl.server_defaults().await;
        self.backend = Some(Box::new(pactl));
        self.kind = Some(BackendKind::Pactl);
        match probe {
            Ok(_) => true,
            Err(e) => {
                warn!(target: "audio", "Sound server control unavailable: {e}");
                false
            }
        }
    }

    /// Release the connection; the next operation reopens it.
    pub async fn close(&mut self) {
        self.kind = None;
        if let Some(mut backend) = self.backend.take() {
            backend.close().await;
        }
    }

    fn connection_lost(&self) -> bool {
        match (self.kind, &self.backend) {
            (Some(BackendKind::Pulse), Some(b)) => !b.connected(),
            (Some(BackendKind::Pactl), Some(_)) => false,
            _ => true,
        }
    }

    /// Run `f` on the backend, reconnecting once after a lost connection.
    ///
    /// Returns `default` (after one warning) when the operation fails, times out, or no backend
    /// is usable, so callers degrade rather than fail.
    async fn op<T, F>(&mut self, what: &str, f: F, default: T) -> T
    where
        F: for<'a> Fn(
                &'a mut (dyn ControlBackend + 'static),
            ) -> BoxFuture<'a, Result<T, AudioControlError>>
            + Send,
        T: Send,
    {
        for attempt in 0..2 {
            if self.connection_lost() && !self.open().await {
                return default;
            }
            let Some(backend) = self.backend.as_mut() else {
                return default;
            };
            match f(backend.as_mut()).await {
                Ok(value) => return value,
                Err(AudioControlError::Timeout) => {
                    warn!(target: "audio", "Sound server did not answer ({what}); connection abandoned.");
                    return default;
                }
                Err(e) => {
                    if attempt == 0 && self.connection_lost() {
                        info!(target: "audio", "Sound server connection lost during {what}; reconnecting.");
                        continue;
                    }
                    warn!(target: "audio", "Sound server control failed ({what}): {e}");
                    return default;
                }
            }
        }
        default
    }

    pub async fn sinks(&mut self) -> Vec<PulseNode> {
        self.op("sink list", |b| b.sink_list(), Vec::new()).await
    }

    pub async fn sources(&mut self) -> Vec<PulseNode> {
        self.op("source list", |b| b.source_list(), Vec::new())
            .await
    }

    /// `(default sink name, default source name)`; `None` when unknown.
    pub async fn default_devices(&mut self) -> (Option<String>, Option<String>) {
        self.op("server info", |b| b.server_defaults(), (None, None))
            .await
    }

    /// Load a server module; its index, or `None` when the load failed.
    pub async fn load_module(&mut self, name: &str, args: &str) -> Option<u32> {
        let what = format!("load {name}");
        self.op(
            &what,
            |b| {
                let (name, args) = (name.to_string(), args.to_string());
                Box::pin(async move { b.module_load(&name, &args).await.map(Some) })
            },
            None,
        )
        .await
    }

    pub async fn unload_module(&mut self, index: u32) -> bool {
        let what = format!("unload module {index}");
        self.op(
            &what,
            move |b| Box::pin(async move { b.module_unload(index).await.map(|_| true) }),
            false,
        )
        .await
    }

    pub async fn set_default_sink(&mut self, name: &str) -> bool {
        let what = format!("default sink {name}");
        self.op(
            &what,
            |b| {
                let name = name.to_string();
                Box::pin(async move { b.sink_default_set(&name).await.map(|_| true) })
            },
            false,
        )
        .await
    }

    pub async fn set_default_source(&mut self, name: &str) -> bool {
        let what = format!("default source {name}");
        self.op(
            &what,
            |b| {
                let name = name.to_string();
                Box::pin(async move { b.source_default_set(&name).await.map(|_| true) })
            },
            false,
        )
        .await
    }

    /// Poll the sinks or the sources until an object named in `names` appears, for up to
    /// `APPEAR_TIMEOUT`: PipeWire creates a freshly loaded module's object asynchronously.
    async fn wait_for(&mut self, sinks: bool, names: &[&str]) -> Option<PulseNode> {
        let deadline = Instant::now() + APPEAR_TIMEOUT;
        loop {
            let listing = if sinks {
                self.sinks().await
            } else {
                self.sources().await
            };
            if let Some(node) = listing
                .into_iter()
                .find(|n| names.contains(&n.name.as_str()))
            {
                return Some(node);
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(APPEAR_POLL).await;
        }
    }

    /// Make sure a null sink called `name` exists, loading it if needed.
    ///
    /// A surround `channels` count creates it at 48 kHz with that many channels in the
    /// positions of `SURROUND_SINK_MAPS`; any other count leaves the server's default layout. An
    /// existing sink is left as it is, since recreating it would move the streams playing into
    /// it.
    pub async fn ensure_null_sink(&mut self, name: &str, channels: i32) -> bool {
        if self.sinks().await.iter().any(|s| s.name == name) {
            return true;
        }
        info!(target: "audio", "Sink '{name}' not found. Creating it...");
        let mut args = format!("sink_name={name}");
        if let Some(map) = surround_sink_map(channels) {
            args.push_str(&format!(
                " rate=48000 channels={channels} channel_map={map}"
            ));
        }
        if self.load_module("module-null-sink", &args).await.is_none() {
            return false;
        }
        if self.wait_for(true, &[name]).await.is_some() {
            info!(target: "audio", "Created sink '{name}'.");
            return true;
        }
        error!(target: "audio", "Loaded module-null-sink for '{name}' but it never appeared.");
        false
    }

    /// Make sure the sink whose monitor the server captures exists.
    ///
    /// A container's PipeWire or PulseAudio comes up with no sink at all when the host exposes
    /// no sound card, so the monitor source named by the configured audio device is missing and
    /// pcmflux gives up after its retry budget. The microphone control plane creates the same
    /// sink, but only once a client sends mic data, which server-to-client audio must not wait
    /// for. A surround session creates it with its channel count (`capture_channels`), so the
    /// capture records the channels applications play rather than an upmix of stereo.
    ///
    /// Best effort: false only means the capture will fail for the usual reasons, so callers
    /// proceed and let pcmflux report.
    pub async fn ensure_capture_sink(&mut self, audio_device_name: Option<&str>) -> bool {
        let sink = capture_sink_name(audio_device_name);
        self.ensure_null_sink(&sink, capture_channels()).await
    }

    /// The source to capture: the configured one if it exists, else the default sink's monitor,
    /// else PipeWire's `auto_null.monitor`; `None` when no usable source exists (or the server
    /// could not be queried).
    pub async fn resolve_capture_source(
        &mut self,
        audio_device_name: Option<&str>,
    ) -> Option<String> {
        let (default_sink, _) = self.default_devices().await;
        let default_monitor = default_sink.as_ref().map(|s| format!("{s}.monitor"));
        match &default_sink {
            Some(sink) => debug!(target: "audio", "Default sink: '{sink}'"),
            None => warn!(target: "audio", "Could not determine the default sink."),
        }
        let available: HashSet<String> = self.sources().await.into_iter().map(|s| s.name).collect();
        if available.is_empty() {
            error!(target: "audio", "Failed to enumerate audio sources.");
            return None;
        }
        if let Some(device) = audio_device_name.filter(|d| !d.is_empty()) {
            if available.contains(device) {
                info!(target: "audio", "Configured audio device '{device}' is valid.");
                return Some(device.to_string());
            }
            warn!(target: "audio", "Configured audio device '{device}' not found in available sources.");
        }
        if let Some(monitor) = default_monitor.filter(|m| available.contains(m)) {
            info!(target: "audio", "Falling back to the default sink's monitor: '{monitor}'");
            return Some(monitor);
        }
        if available.contains(PIPEWIRE_NULL_MONITOR) {
            info!(target: "audio", "Default sink monitor not available; falling back to '{PIPEWIRE_NULL_MONITOR}'");
            return Some(PIPEWIRE_NULL_MONITOR.to_string());
        }
        let mut names: Vec<&String> = available.iter().collect();
        names.sort();
        error!(target: "audio", "No valid audio source found. Audio capture will likely fail. Available sources: {names:?}");
        None
    }

    /// Put the pcmflux record stream on one of `targets` if it strayed.
    ///
    /// PipeWire often ignores a recording app's requested device and attaches it to the default
    /// source, particularly across streaming-mode switches. Returns the name of the source
    /// pcmflux records from afterwards, or `None` when no pcmflux stream (or no target source)
    /// exists.
    pub async fn route_pcmflux(&mut self, targets: &[&str]) -> Option<String> {
        let wanted: Vec<String> = targets
            .iter()
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string())
            .collect();
        self.op(
            "pcmflux routing",
            |b| {
                let wanted = wanted.clone();
                Box::pin(async move {
                    let sources = b.source_list().await?;
                    let outputs = b.source_output_list().await?;
                    let Some(stream) = outputs.iter().find(|o| {
                        o.proplist.get("application.name").map(String::as_str) == Some(PCMFLUX_APP_NAME)
                    }) else {
                        debug!(target: "audio", "pcmflux has no record stream to route.");
                        return Ok(None);
                    };
                    let current = stream.source.and_then(|idx| sources.iter().find(|s| s.index == idx));
                    if let Some(current) = current.filter(|c| wanted.contains(&c.name)) {
                        info!(target: "audio", "pcmflux correctly connected to '{}'", current.name);
                        return Ok(Some(current.name.clone()));
                    }
                    let Some(target) = sources.iter().find(|s| wanted.contains(&s.name)) else {
                        warn!(target: "audio", "Routing enforcement: no target source among {wanted:?} exists.");
                        return Ok(current.map(|c| c.name.clone()));
                    };
                    let from = match (current, stream.source) {
                        (Some(c), _) => c.name.clone(),
                        (None, Some(idx)) => idx.to_string(),
                        (None, None) => "nothing".to_string(),
                    };
                    warn!(target: "audio", "pcmflux connected to '{from}', moving it to '{}'", target.name);
                    b.source_output_move(stream.index, target.index).await?;
                    Ok(Some(target.name.clone()))
                })
            },
            None,
        )
        .await
    }

    /// The applications recording from one of `source_names` right now, by name.
    ///
    /// Filtering by source index leaves out the virtual source's own bridge stream, which records
    /// from the master, and a corked stream is left out too: an application that opened the
    /// microphone without recording holds one. Empty when the server cannot say.
    pub async fn recorders(&mut self, source_names: &[&str]) -> Vec<String> {
        let wanted: Vec<String> = source_names.iter().map(|n| n.to_lowercase()).collect();
        self.op(
            "microphone readers",
            |b| {
                let wanted = wanted.clone();
                Box::pin(async move {
                    let indices: HashSet<u32> = b
                        .source_list()
                        .await?
                        .into_iter()
                        .filter(|s| wanted.contains(&s.name.to_lowercase()))
                        .map(|s| s.index)
                        .collect();
                    if indices.is_empty() {
                        return Ok(Vec::new());
                    }
                    Ok(b.source_output_list()
                        .await?
                        .into_iter()
                        .filter(|o| o.source.is_some_and(|idx| indices.contains(&idx)) && !o.corked)
                        .map(|o| {
                            o.proplist
                                .get("application.name")
                                .filter(|n| !n.is_empty())
                                .cloned()
                                .or_else(|| (!o.name.is_empty()).then(|| o.name.clone()))
                                .unwrap_or_else(|| "an application".to_string())
                        })
                        .collect())
                })
            },
            Vec::new(),
        )
        .await
    }

    /// Provision the `SelkiesVirtualMic` control plane shared by both transports.
    ///
    /// Creates the `input` and capture null sinks, loads `module-virtual-source` bridging
    /// `input.monitor` to a recordable source, and makes them the system default sink and source
    /// so an app recording the default source hears the client's forwarded mic. The PCM data
    /// plane (pcmflux `AudioPlayback` into the `input` sink) belongs to the caller.
    ///
    /// Idempotent: an existing `SelkiesVirtualMic` is reused, so the websockets mic path and the
    /// WebRTC mic playback never double-load the module when both are live. With
    /// `is_pcmflux_capturing`, pcmflux's record stream is verified to sit on a valid capture
    /// target and moved if not.
    ///
    /// Returns `(module index, owns module)`: `owns module` is true only when this call loaded
    /// the module, so a caller that merely reused an existing source never unloads it out from
    /// under the other transport on teardown; `(None, false)` when the load could not be
    /// verified.
    pub async fn ensure_virtual_microphone(
        &mut self,
        audio_device_name: Option<&str>,
        is_pcmflux_capturing: bool,
    ) -> (Option<u32>, bool) {
        let output_sink = capture_sink_name(audio_device_name);
        self.ensure_null_sink(VIRTUAL_MIC_SINK, 2).await;
        self.ensure_null_sink(&output_sink, capture_channels())
            .await;
        if self.set_default_sink(&output_sink).await {
            debug!(target: "audio", "Set system default sink to '{output_sink}'.");
        }
        let existing = self
            .sources()
            .await
            .into_iter()
            .find(|s| VIRTUAL_MIC_SOURCE_NAMES.contains(&s.name.as_str()));
        let (module_index, owns_module) = match existing {
            Some(existing) => {
                debug!(target: "audio", "Virtual source '{}' (index {}) already exists.", existing.name, existing.index);
                if let Some(master) = existing.proplist.get("device.master_device")
                    && master != VIRTUAL_MIC_MASTER
                {
                    warn!(target: "audio", "Existing source '{}' is linked to '{master}', not '{VIRTUAL_MIC_MASTER}'.", existing.name);
                }
                self.set_default_source(&existing.name).await;
                (existing.owner_module, false)
            }
            None => {
                debug!(target: "audio", "Virtual source '{VIRTUAL_MIC_SOURCE}' not found. Loading module...");
                let Some(module_index) = self
                    .load_module(
                        "module-virtual-source",
                        &format!("source_name={VIRTUAL_MIC_SOURCE} master={VIRTUAL_MIC_MASTER}"),
                    )
                    .await
                else {
                    return (None, false);
                };
                let Some(created) = self.wait_for(false, &VIRTUAL_MIC_SOURCE_NAMES).await else {
                    error!(target: "audio", "Loaded module {module_index} but source '{VIRTUAL_MIC_SOURCE}' never appeared.");
                    self.unload_module(module_index).await;
                    return (None, false);
                };
                debug!(target: "audio", "Created source '{}' (index {}).", created.name, created.index);
                if self.set_default_source(&created.name).await {
                    debug!(target: "audio", "Set system default source to '{}'.", created.name);
                }
                (Some(module_index), true)
            }
        };
        if is_pcmflux_capturing {
            let device = audio_device_name.unwrap_or("");
            self.route_pcmflux(&[device, PIPEWIRE_NULL_MONITOR]).await;
        }
        info!(target: "audio", "Virtual microphone '{VIRTUAL_MIC_SOURCE}' is ready for microphone forwarding.");
        (module_index, owns_module)
    }
}

/// One-shot [`AudioControl::ensure_capture_sink`] over a short-lived connection.
pub async fn ensure_capture_sink(audio_device_name: Option<&str>, client_name: &str) -> bool {
    let mut control = AudioControl::new(client_name);
    let ok = control.ensure_capture_sink(audio_device_name).await;
    control.close().await;
    ok
}

#[cfg(test)]
pub(super) mod tests {
    //! The control plane against a scripted server: the
    //! provisioning operations for what they ask the server to do, and the `pactl` fallback
    //! against scripted command output. The never-cancel discipline belongs to the libpulse
    //! thread and is proven in `pulse.rs` against the real server.

    use super::*;
    use crate::audio::install_test_settings;
    use parking_lot::Mutex;
    use std::sync::atomic::AtomicUsize;

    /// Sink/source state shared by every stub client of one scenario.
    #[derive(Default)]
    pub struct StubServer {
        pub sinks: Vec<PulseNode>,
        pub sources: Vec<PulseNode>,
        pub source_outputs: Vec<PulseNode>,
        pub default_sink: Option<String>,
        pub default_source: Option<String>,
        pub modules: HashMap<u32, String>,
        pub next_index: u32,
        pub refuse_connect: bool,
        pub log: Vec<String>,
        pub virtual_source_appears: bool,
    }

    impl StubServer {
        pub fn add_sink(&mut self, name: &str, module: Option<u32>) -> u32 {
            let idx = self.next_index;
            self.next_index += 1;
            self.sinks.push(PulseNode {
                index: idx,
                name: name.to_string(),
                owner_module: module,
                ..PulseNode::default()
            });
            self.add_source(&format!("{name}.monitor"), module, HashMap::new());
            idx
        }

        pub fn add_source(
            &mut self,
            name: &str,
            module: Option<u32>,
            proplist: HashMap<String, String>,
        ) -> u32 {
            let idx = self.next_index;
            self.next_index += 1;
            self.sources.push(PulseNode {
                index: idx,
                name: name.to_string(),
                owner_module: module,
                proplist,
                ..PulseNode::default()
            });
            idx
        }

        pub fn add_source_output(&mut self, app: &str, source_index: u32, corked: bool) -> u32 {
            let idx = self.next_index;
            self.next_index += 1;
            self.source_outputs.push(PulseNode {
                index: idx,
                name: format!("{app} stream"),
                owner_module: None,
                proplist: HashMap::from([("application.name".to_string(), app.to_string())]),
                source: Some(source_index),
                corked,
            });
            idx
        }

        pub fn source_index(&self, name: &str) -> u32 {
            self.sources.iter().find(|s| s.name == name).unwrap().index
        }

        pub fn output_source(&self, index: u32) -> Option<u32> {
            self.source_outputs
                .iter()
                .find(|o| o.index == index)
                .and_then(|o| o.source)
        }
    }

    /// The scenario's server and how many clients it saw open and close.
    pub struct Scenario {
        pub server: Mutex<StubServer>,
        pub clients: AtomicUsize,
        pub closed: AtomicUsize,
    }

    impl Scenario {
        pub fn new() -> Arc<Self> {
            Arc::new(Scenario {
                server: Mutex::new(StubServer {
                    next_index: 100,
                    virtual_source_appears: true,
                    ..StubServer::default()
                }),
                clients: AtomicUsize::new(0),
                closed: AtomicUsize::new(0),
            })
        }

        pub fn connector(self: &Arc<Self>) -> Connector {
            let scenario = self.clone();
            Arc::new(move |name, _connect, _op| {
                let scenario = scenario.clone();
                Box::pin(async move {
                    if scenario.server.lock().refuse_connect {
                        return Err("Failed to connect to pulseaudio server".to_string());
                    }
                    scenario.clients.fetch_add(1, Ordering::SeqCst);
                    Ok(Box::new(StubClient {
                        name,
                        scenario,
                        connected: Arc::new(AtomicBool::new(true)),
                    }) as Box<dyn ControlBackend>)
                })
            })
        }
    }

    /// Stands in for a connected client: async ops over the scenario's state.
    pub struct StubClient {
        name: String,
        scenario: Arc<Scenario>,
        connected: Arc<AtomicBool>,
    }

    impl StubClient {
        async fn op(&self, what: &str) -> Result<(), AudioControlError> {
            if !self.connected.load(Ordering::SeqCst) {
                return Err(AudioControlError::NotConnected);
            }
            self.scenario
                .server
                .lock()
                .log
                .push(format!("{}:{what}", self.name));
            Ok(())
        }
    }

    #[async_trait]
    impl ControlBackend for StubClient {
        fn connected(&self) -> bool {
            self.connected.load(Ordering::SeqCst)
        }

        async fn sink_list(&mut self) -> Result<Vec<PulseNode>, AudioControlError> {
            self.op("sink_list").await?;
            Ok(self.scenario.server.lock().sinks.clone())
        }

        async fn source_list(&mut self) -> Result<Vec<PulseNode>, AudioControlError> {
            self.op("source_list").await?;
            Ok(self.scenario.server.lock().sources.clone())
        }

        async fn source_output_list(&mut self) -> Result<Vec<PulseNode>, AudioControlError> {
            self.op("source_output_list").await?;
            Ok(self.scenario.server.lock().source_outputs.clone())
        }

        async fn server_defaults(
            &mut self,
        ) -> Result<(Option<String>, Option<String>), AudioControlError> {
            self.op("server_info").await?;
            let server = self.scenario.server.lock();
            Ok((server.default_sink.clone(), server.default_source.clone()))
        }

        async fn module_load(&mut self, name: &str, args: &str) -> Result<u32, AudioControlError> {
            self.op(&format!("module_load {name} {args}")).await?;
            let mut server = self.scenario.server.lock();
            let idx = server.next_index;
            server.next_index += 1;
            server.modules.insert(idx, format!("{name} {args}"));
            let params: HashMap<&str, &str> = args
                .split_whitespace()
                .filter_map(|kv| kv.split_once('='))
                .collect();
            if name == "module-null-sink" {
                server.add_sink(params["sink_name"], Some(idx));
            } else if name == "module-virtual-source" && server.virtual_source_appears {
                let master = params.get("master").unwrap_or(&"").to_string();
                server.add_source(
                    &format!("output.{}", params["source_name"]),
                    Some(idx),
                    HashMap::from([("device.master_device".to_string(), master)]),
                );
            }
            Ok(idx)
        }

        async fn module_unload(&mut self, index: u32) -> Result<(), AudioControlError> {
            self.op(&format!("module_unload {index}")).await?;
            let mut server = self.scenario.server.lock();
            server.modules.remove(&index);
            server.sinks.retain(|s| s.owner_module != Some(index));
            server.sources.retain(|s| s.owner_module != Some(index));
            Ok(())
        }

        async fn sink_default_set(&mut self, name: &str) -> Result<(), AudioControlError> {
            self.op(&format!("sink_default_set {name}")).await?;
            self.scenario.server.lock().default_sink = Some(name.to_string());
            Ok(())
        }

        async fn source_default_set(&mut self, name: &str) -> Result<(), AudioControlError> {
            self.op(&format!("source_default_set {name}")).await?;
            self.scenario.server.lock().default_source = Some(name.to_string());
            Ok(())
        }

        async fn source_output_move(
            &mut self,
            output_index: u32,
            source_index: u32,
        ) -> Result<(), AudioControlError> {
            self.op(&format!("source_output_move {output_index} {source_index}"))
                .await?;
            for o in &mut self.scenario.server.lock().source_outputs {
                if o.index == output_index {
                    o.source = Some(source_index);
                }
            }
            Ok(())
        }

        async fn close(&mut self) {
            self.connected.store(false, Ordering::SeqCst);
            self.scenario.closed.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn control(scenario: &Arc<Scenario>, name: &str) -> AudioControl {
        AudioControl::with_timeouts(name, Duration::from_millis(300), Duration::from_millis(300))
            .with_connector(scenario.connector())
    }

    #[tokio::test]
    async fn capture_sink_is_created_from_the_monitor_name_and_left_alone_after() {
        install_test_settings();
        let scenario = Scenario::new();
        let mut ctl = control(&scenario, "t-sink");
        assert!(ctl.ensure_capture_sink(Some("output.monitor")).await);
        {
            let server = scenario.server.lock();
            assert_eq!(
                server
                    .sinks
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>(),
                ["output"]
            );
            assert!(
                server
                    .modules
                    .values()
                    .any(|m| m == "module-null-sink sink_name=output")
            );
        }
        let calls_before = scenario.server.lock().log.len();
        assert!(ctl.ensure_capture_sink(Some("output.monitor")).await);
        {
            let server = scenario.server.lock();
            assert_eq!(server.log.len(), calls_before + 1, "one listing, no load");
            assert_eq!(server.modules.len(), 1);
        }
        assert!(ctl.ensure_capture_sink(None).await);
        assert_eq!(
            scenario.server.lock().modules.len(),
            1,
            "default name when unset"
        );
        assert!(ctl.ensure_null_sink("surround", 6).await);
        assert!(scenario.server.lock().modules.values().any(|m| m
            == "module-null-sink sink_name=surround rate=48000 channels=6 channel_map=front-left,front-right,front-center,lfe,rear-left,rear-right"));
        assert_eq!(ctl.backend(), Some(BackendKind::Pulse));
        ctl.close().await;
        assert_eq!(
            scenario.closed.load(Ordering::SeqCst),
            1,
            "close closes the client"
        );
    }

    #[tokio::test]
    async fn capture_source_resolution_falls_back_in_order() {
        install_test_settings();
        let scenario = Scenario::new();
        {
            let mut server = scenario.server.lock();
            server.add_sink("output", None);
            server.default_sink = Some("output".to_string());
            server.add_source("auto_null.monitor", None, HashMap::new());
        }
        let mut ctl = control(&scenario, "t-resolve");
        assert_eq!(
            ctl.resolve_capture_source(Some("output.monitor"))
                .await
                .as_deref(),
            Some("output.monitor")
        );
        assert_eq!(
            ctl.resolve_capture_source(Some("gone.monitor"))
                .await
                .as_deref(),
            Some("output.monitor")
        );
        scenario.server.lock().default_sink = Some("vanished".to_string());
        assert_eq!(
            ctl.resolve_capture_source(Some("gone.monitor"))
                .await
                .as_deref(),
            Some("auto_null.monitor")
        );
        scenario.server.lock().sources.clear();
        assert_eq!(ctl.resolve_capture_source(Some("gone.monitor")).await, None);
        ctl.close().await;
    }

    #[tokio::test]
    async fn strayed_pcmflux_stream_is_moved_once() {
        install_test_settings();
        let scenario = Scenario::new();
        let (stream, out_mon) = {
            let mut server = scenario.server.lock();
            server.add_sink("output", None);
            server.add_sink("input", None);
            let in_mon = server.source_index("input.monitor");
            let out_mon = server.source_index("output.monitor");
            (server.add_source_output("pcmflux", in_mon, false), out_mon)
        };
        let mut ctl = control(&scenario, "t-route");
        assert_eq!(
            ctl.route_pcmflux(&["output.monitor"]).await.as_deref(),
            Some("output.monitor")
        );
        assert_eq!(scenario.server.lock().output_source(stream), Some(out_mon));
        let moves = || {
            scenario
                .server
                .lock()
                .log
                .iter()
                .filter(|c| c.contains("source_output_move"))
                .count()
        };
        let before = moves();
        assert_eq!(
            ctl.route_pcmflux(&["output.monitor"]).await.as_deref(),
            Some("output.monitor")
        );
        assert_eq!(
            moves(),
            before,
            "correctly attached stream is not moved again"
        );
        scenario.server.lock().source_outputs.clear();
        assert_eq!(ctl.route_pcmflux(&["output.monitor"]).await, None);
        ctl.close().await;
    }

    #[tokio::test]
    async fn virtual_microphone_is_provisioned_once_and_reused() {
        install_test_settings();
        let scenario = Scenario::new();
        let mut ctl = control(&scenario, "t-mic");
        let (idx, owned) = ctl
            .ensure_virtual_microphone(Some("output.monitor"), false)
            .await;
        let idx = idx.expect("module loaded");
        assert!(owned);
        {
            let server = scenario.server.lock();
            let mut names: Vec<&str> = server.sinks.iter().map(|s| s.name.as_str()).collect();
            names.sort();
            assert_eq!(names, ["input", "output"]);
            assert!(server.modules[&idx].starts_with(
                "module-virtual-source source_name=SelkiesVirtualMic master=input.monitor"
            ));
            assert_eq!(
                (
                    server.default_sink.as_deref(),
                    server.default_source.as_deref()
                ),
                (Some("output"), Some("output.SelkiesVirtualMic"))
            );
        }
        let (idx2, owned2) = ctl
            .ensure_virtual_microphone(Some("output.monitor"), false)
            .await;
        assert_eq!(
            (idx2, owned2),
            (Some(idx), false),
            "second provisioning reuses the source"
        );
        assert_eq!(scenario.server.lock().modules.len(), 3);
        let (stream, target) = {
            let mut server = scenario.server.lock();
            let wrong = server.add_source("SelkiesVirtualMic.monitor", None, HashMap::new());
            let stream = server.add_source_output("pcmflux", wrong, false);
            (stream, server.source_index("output.monitor"))
        };
        ctl.ensure_virtual_microphone(Some("output.monitor"), true)
            .await;
        assert_eq!(
            scenario.server.lock().output_source(stream),
            Some(target),
            "capturing pcmflux moved onto the capture monitor"
        );
        assert!(ctl.unload_module(idx).await);
        assert!(!scenario.server.lock().modules.contains_key(&idx));
        scenario.server.lock().virtual_source_appears = false;
        let (idx3, owned3) = ctl
            .ensure_virtual_microphone(Some("output.monitor"), false)
            .await;
        assert_eq!((idx3, owned3), (None, false));
        assert!(
            !scenario
                .server
                .lock()
                .modules
                .values()
                .any(|m| m.starts_with("module-virtual-source"))
        );
        ctl.close().await;
    }

    #[tokio::test]
    async fn recorders_lists_uncorked_streams_of_the_virtual_source_only() {
        install_test_settings();
        let scenario = Scenario::new();
        {
            let mut server = scenario.server.lock();
            server.add_sink("input", None);
            let mic = server.add_source("output.SelkiesVirtualMic", Some(5), HashMap::new());
            let master = server.source_index("input.monitor");
            server.add_source_output("Firefox", mic, false);
            server.add_source_output("Zoom", mic, true);
            server.add_source_output("bridge", master, false);
        }
        let mut ctl = control(&scenario, "t-readers");
        assert_eq!(ctl.recorders(&VIRTUAL_MIC_SOURCE_NAMES).await, ["Firefox"]);
        assert!(ctl.recorders(&["nothing"]).await.is_empty());
        ctl.close().await;
    }

    #[tokio::test]
    async fn pactl_fallback_engages_when_the_connection_fails() {
        install_test_settings();
        let scenario = Scenario::new();
        scenario.server.lock().refuse_connect = true;

        struct FakePactl(Mutex<Vec<Vec<String>>>);

        #[async_trait]
        impl PactlRunner for FakePactl {
            async fn run(
                &self,
                args: &[String],
                _timeout: Duration,
            ) -> Result<String, AudioControlError> {
                self.0.lock().push(args.to_vec());
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                Ok(match args.as_slice() {
                    ["info"] => "Default Sink: output\nDefault Source: output.monitor\n".to_string(),
                    ["list", "sinks"] => "Sink #7\n\tState: IDLE\n\tName: output\n\tOwner Module: 3\n\tProperties:\n\t\tnode.name = \"output\"\n".to_string(),
                    ["list", "sources"] => "Source #8\n\tName: output.monitor\n\tOwner Module: 3\n\tMonitor of Sink: output\n\tProperties:\n\t\tdevice.class = \"monitor\"\n\nSource #9\n\tName: output.SelkiesVirtualMic\n\tOwner Module: 5\n\tProperties:\n\t\tdevice.master_device = \"input.monitor\"\n".to_string(),
                    ["list", "source-outputs"] => "Source Output #12\n\tDriver: PipeWire\n\tOwner Module: n/a\n\tSource: 9\n\tProperties:\n\t\tapplication.name = \"pcmflux\"\n\t\tmedia.name = \"Record Stream\"\n".to_string(),
                    ["load-module", ..] => "77\n".to_string(),
                    _ => String::new(),
                })
            }
        }

        let runner = Arc::new(FakePactl(Mutex::new(Vec::new())));
        let calls = |needle: &[&str]| {
            runner
                .0
                .lock()
                .iter()
                .any(|c| c.iter().map(String::as_str).eq(needle.iter().copied()))
        };
        let mut ctl = control(&scenario, "t-fallback").with_pactl_runner(runner.clone());
        assert!(ctl.open().await, "usable when the bindings cannot connect");
        assert_eq!(ctl.backend(), Some(BackendKind::Pactl));
        assert!(ctl.ensure_capture_sink(Some("output.monitor")).await);
        assert!(
            calls(&["list", "sinks"]),
            "capture sink resolved over pactl"
        );
        assert_eq!(
            ctl.route_pcmflux(&["output.monitor"]).await.as_deref(),
            Some("output.monitor")
        );
        assert!(
            calls(&["move-source-output", "12", "8"]),
            "parsed source-outputs and moved pcmflux by index"
        );
        assert_eq!(
            ctl.ensure_virtual_microphone(Some("output.monitor"), false)
                .await,
            (Some(5), false),
            "existing virtual source reused"
        );
        assert!(
            calls(&["set-default-sink", "output"])
                && calls(&["set-default-source", "output.SelkiesVirtualMic"])
        );
        assert_eq!(
            ctl.load_module("module-null-sink", "sink_name=x").await,
            Some(77),
            "load-module index parsed"
        );
        ctl.close().await;
    }

    #[test]
    fn pactl_listing_parses_index_name_owner_and_properties() {
        let sample = "Source #149\n\tState: IDLE\n\tName: output.monitor\n\tDescription: Monitor of output\n\
                      \tOwner Module: 536870913\n\tMonitor of Sink: output\n\tProperties:\n\
                      \t\tnode.name = \"output\"\n\t\tdevice.class = \"monitor\"\n\tFormats:\n\t\tpcm\n\n\
                      Source #41285\n\tState: SUSPENDED\n\tName: output.SelkiesVirtualMic\n\tOwner Module: 536870917\n\
                      \tProperties:\n\t\tmedia.class = \"Audio/Source\"\n\t\tnode.virtual = \"true\"\n";
        let nodes = parse_pactl_list(sample);
        let summary: Vec<(u32, &str, Option<u32>)> = nodes
            .iter()
            .map(|n| (n.index, n.name.as_str(), n.owner_module))
            .collect();
        assert_eq!(
            summary,
            [
                (149, "output.monitor", Some(536870913)),
                (41285, "output.SelkiesVirtualMic", Some(536870917))
            ]
        );
        assert_eq!(
            nodes[0].proplist.get("device.class").map(String::as_str),
            Some("monitor")
        );
        assert_eq!(
            nodes[1].proplist.get("node.virtual").map(String::as_str),
            Some("true")
        );
        assert!(!nodes[0].proplist.contains_key("pcm"));
        let outputs = parse_pactl_list(
            "Source Output #12\n\tSource: 9\n\tCorked: yes\n\tOwner Module: n/a\n",
        );
        assert_eq!(
            (
                outputs[0].source,
                outputs[0].corked,
                outputs[0].owner_module
            ),
            (Some(9), true, None)
        );
    }
}
