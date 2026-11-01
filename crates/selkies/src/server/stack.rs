/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The adapters that join the modules into a server: what each module asks of the others
//! through a trait, answered here, and the late bindings that break the construction cycles
//! (the supervisor needs its services, a service needs the token table, the table needs the
//! state the supervisor is part of; the clipboard needs the dispatcher, which needs the clipboard).

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use futures::future::BoxFuture;
use log::{error, warn};
use serde_json::Value as Json;
use tokio::sync::Notify;

use crate::audio::{AudioControl, VIRTUAL_MIC_SOURCE_NAMES};
use crate::clipboard::Clipboard;
use crate::core::Role;
use crate::files::{CaptureSinkProvisioner, FilesState, SessionDirectory};
use crate::gamepad::virtual_device::VirtualInputDevice;
use crate::gamepad::{ConnId, GamepadHost, GamepadSlots, SlotAuthority};
use crate::http::MetricsSource;
use crate::http::supervisor::{ModeSupervisor, StreamingService};
use crate::input::{Dispatcher, PointerSink, VirtualDeviceSink};
use crate::settings::settings;
use crate::stats::metrics::Metrics as StatsMetrics;
use crate::webcam::demand::Recorders;
use crate::ws::WsService;
use crate::ws::host;

/// A service bound after the supervisor it reports to has been built.
#[derive(Default)]
pub struct LateService(OnceLock<Arc<dyn StreamingService>>);

impl LateService {
    pub fn bind(&self, service: Arc<dyn StreamingService>) {
        let _ = self.0.set(service);
    }
}

#[async_trait]
impl StreamingService for LateService {
    async fn start(&self) -> Result<(), String> {
        match self.0.get() {
            Some(service) => service.start().await,
            None => Err("service not built".into()),
        }
    }

    async fn stop(&self) {
        if let Some(service) = self.0.get() {
            service.stop().await;
        }
    }

    async fn sessions(&self) -> Vec<Json> {
        match self.0.get() {
            Some(service) => service.sessions().await,
            None => Vec::new(),
        }
    }

    async fn disconnect_session(&self, id: &str) -> bool {
        match self.0.get() {
            Some(service) => service.disconnect_session(id).await,
            None => false,
        }
    }
}

/// The WebSockets transport as the supervisor drives it: the input stack comes up with the
/// service and goes down with it, the persistent gamepads and captures staying.
pub struct WsStreaming {
    pub service: Arc<WsService>,
    pub dispatcher: Arc<Dispatcher>,
    pub clipboard: Arc<Clipboard>,
    pub gamepads: Arc<GamepadSlots>,
    pub devices: Arc<PublishedDevices>,
    pub screens: Arc<crate::session_screens::SessionScreens>,
    pub stop: Notify,
}

#[async_trait]
impl StreamingService for WsStreaming {
    /// Bring the input stack up, realize the configured density on Wayland (X11's was settled
    /// before the listener opened), and serve until asked to stop.
    async fn start(&self) -> Result<(), String> {
        self.service.begin();
        let host: Arc<dyn GamepadHost> = Arc::new(WsGamepadHost(self.service.clone()));
        self.gamepads.set_host(Some(host));
        self.dispatcher.connect().await;
        self.devices.open(&self.dispatcher).await;
        self.clipboard.spawn_monitor();
        let dispatcher = self.dispatcher.clone();
        tokio::spawn(async move { dispatcher.apps().probe().await });
        if settings().wayland().on {
            self.screens.realize_startup_dpi().await;
        }
        self.stop.notified().await;
        self.service.shutdown().await;
        self.clipboard.stop();
        self.dispatcher.disconnect().await;
        self.gamepads.set_host(None);
        Ok(())
    }

    async fn stop(&self) {
        self.stop.notify_one();
    }

    async fn sessions(&self) -> Vec<Json> {
        self.service.sessions().await
    }

    async fn disconnect_session(&self, id: &str) -> bool {
        self.service.disconnect_session(id).await
    }
}

/// A connection's gamepad authority and its rumble, over the WebSockets registry.
pub struct WsGamepadHost(pub Arc<WsService>);

impl GamepadHost for WsGamepadHost {
    fn send_system_action(&self, conn: ConnId, action: &str) {
        self.0.send_system_action(action, Some(conn));
    }

    fn slot_authority(&self, conn: ConnId) -> SlotAuthority {
        let secure = !settings().master_token().is_empty();
        let Some(client) = self.0.client(conn) else {
            return SlotAuthority { role: Role::Viewer, slot: None, secure };
        };
        let perms = client.perms();
        let slot = match perms.token.as_deref() {
            Some(token) => self.0.deps.tokens.lookup(token).map(|e| e.slot),
            None => Some(perms.slot.clone()),
        };
        SlotAuthority {
            role: if perms.is_viewer() { Role::Viewer } else { Role::Controller },
            slot: slot.filter(|s| !s.is_null()),
            secure,
        }
    }
}

/// Pointer echoes to a trackpad page, framed as the transport frames a system action.
pub struct WsPointerSink(pub OnceLock<Arc<WsService>>);

impl PointerSink for WsPointerSink {
    fn pointer_echo(&self, conn: ConnId, action: &str) {
        if let Some(service) = self.0.get() {
            service.send_system_action(action, Some(conn));
        }
    }
}

/// The keyboard and pointer a session publishes (`publish_input_devices`), opened once the
/// dispatcher can say whether the display reads kernel input.
#[derive(Default)]
pub struct PublishedDevices {
    keyboard: OnceLock<Arc<VirtualInputDevice>>,
    pointer: OnceLock<Arc<VirtualInputDevice>>,
}

impl PublishedDevices {
    /// Open the devices where asked for, once per process, since applications hold them
    /// across a transport switch.
    pub async fn open(&self, dispatcher: &Arc<Dispatcher>) {
        use crate::gamepad::virtual_device::{publish_wanted, published_device_specs};
        let (wanted, dir) = {
            let s = settings();
            (
                publish_wanted(s.publish_input_devices().on, s.wayland_host_display()),
                crate::settings::socket_dir(s.js_socket_path()),
            )
        };
        if !wanted || self.keyboard.get().is_some() {
            return;
        }
        let kernel = !dispatcher.display_reads_kernel_input().await;
        for spec in published_device_specs() {
            let device = VirtualInputDevice::from_spec(&spec, std::path::Path::new(&dir));
            if !device.open(kernel).await {
                warn!(target: "input", "Published device {} could not be opened", spec.name);
                continue;
            }
            let slot = if spec.key == "keyboard" { &self.keyboard } else { &self.pointer };
            let _ = slot.set(device);
        }
    }
}

impl VirtualDeviceSink for PublishedDevices {
    fn key(&self, evdev_code: u32, down: bool) {
        if let Some(device) = self.keyboard.get() {
            device.emit(crate::gamepad::abi::EV_KEY, evdev_code as u16, i32::from(down));
        }
    }

    fn pointer(&self, ev_type: u16, code: u16, value: i32) {
        if let Some(device) = self.pointer.get() {
            device.emit(ev_type, code, value);
        }
    }
}

/// `clipboard` of `WsDeps`, bound once the clipboard exists.
#[derive(Default)]
pub struct LateClipboard(pub OnceLock<Arc<Clipboard>>);

#[async_trait]
impl host::Clipboard for LateClipboard {
    async fn set_binary_enabled(&self, enabled: bool) {
        if let Some(clipboard) = self.0.get() {
            clipboard.set_binary_clipboard(enabled).await;
        }
    }
}

/// What the transport reports to the process: the mode in force, the presence hooks, and the
/// documents waiting in the print spool.
pub struct TransportSupervisor {
    pub modes: Arc<ModeSupervisor>,
    pub files: Arc<FilesState>,
    pub present: std::sync::atomic::AtomicBool,
}

impl host::Supervisor for TransportSupervisor {
    fn current_mode(&self) -> Option<String> {
        crate::http::Supervisor::current_mode(&*self.modes)
    }

    /// Fires `run_after_connect` when the first client arrives and `run_after_disconnect` when
    /// the last leaves, killing a hook that wedges past five minutes.
    fn set_clients_present(&self, present: bool) {
        use std::sync::atomic::Ordering;
        if self.present.swap(present, Ordering::Relaxed) == present {
            return;
        }
        let (name, command) = {
            let s = settings();
            if present {
                ("run_after_connect", s.run_after_connect().to_string())
            } else {
                ("run_after_disconnect", s.run_after_disconnect().to_string())
            }
        };
        if command.is_empty() {
            return;
        }
        tokio::spawn(async move {
            let child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .kill_on_drop(true)
                .spawn();
            let mut child = match child {
                Ok(child) => child,
                Err(e) => {
                    error!(target: "server", "Failed to run {name} command {command:?}: {e}");
                    return;
                }
            };
            let wait = tokio::time::timeout(std::time::Duration::from_secs(300), child.wait());
            match wait.await {
                Err(_) => {
                    warn!(target: "server", "{name} command timed out after 300s; killing: {command}");
                    let _ = child.kill().await;
                }
                Ok(Ok(status)) if !status.success() => {
                    warn!(target: "server", "{name} command exited with status {}: {command}", status.code().unwrap_or(-1));
                }
                _ => {}
            }
        });
    }

    fn pending_print_documents(&self) -> Vec<(String, u64)> {
        self.files.pending_print_documents()
    }
}

/// The files routes' view of the active transport's sessions.
pub struct WsSessions(pub Arc<WsService>);

impl SessionDirectory for WsSessions {
    fn uplink_sessions(&self) -> Vec<crate::files::pacing::UplinkSession> {
        self.0.uplink_sessions()
    }

    fn announce_print_document(&self, name: &str, size_bytes: u64) {
        let service = self.0.clone();
        let name = name.to_string();
        tokio::spawn(async move { service.announce_print_document(&name, size_bytes).await });
    }
}

/// Recordings provision the sink they capture.
pub struct SinkProvisioner;

impl CaptureSinkProvisioner for SinkProvisioner {
    fn ensure_capture_sink(&self, device_name: &str) -> BoxFuture<'_, bool> {
        let device = device_name.to_string();
        Box::pin(async move {
            crate::audio::ensure_capture_sink(Some(&device), "selkies-recording").await
        })
    }
}

/// The sound server's recording streams, for capture on demand; each call runs on a blocking
/// thread, so it blocks on the control plane's own future.
pub struct SoundRecorders(pub tokio::runtime::Handle);

impl SoundRecorders {
    fn control(&self) -> Option<AudioControl> {
        let mut control = AudioControl::new("selkies-capture-demand");
        self.0.block_on(control.open()).then_some(control)
    }
}

impl Recorders for SoundRecorders {
    fn prepare(&self) -> bool {
        let Some(mut control) = self.control() else {
            return false;
        };
        let device = settings().audio_device_name().to_string();
        self.0.block_on(async {
            let provisioned = control.ensure_virtual_microphone(Some(&device), false).await;
            control.close().await;
            provisioned.0.is_some() || provisioned.1
        })
    }

    fn recorders(&self) -> Vec<String> {
        let Some(mut control) = self.control() else {
            return Vec::new();
        };
        self.0.block_on(async {
            let found = control.recorders(&VIRTUAL_MIC_SOURCE_NAMES).await;
            control.close().await;
            found
        })
    }
}

/// The Prometheus registry, for the route and the gauges the transport feeds.
pub struct ServerMetrics(pub Arc<StatsMetrics>);

impl MetricsSource for ServerMetrics {
    fn render(&self) -> (String, String) {
        (
            self.0.render(),
            crate::stats::metrics::CONTENT_TYPE.to_string(),
        )
    }
}

impl host::Metrics for ServerMetrics {
    fn set_fps(&self, fps: f64) {
        self.0.set_fps(fps);
    }

    fn set_latency(&self, ms: f64) {
        self.0.set_latency(ms);
    }
}

/// The capture-demand transitions on the audit trail.
pub struct DemandAudit(pub Arc<crate::http::audit::Audit>);

impl crate::webcam::demand::AuditSink for DemandAudit {
    fn emit(&self, event: &str, fields: Json) {
        let fields = match fields {
            Json::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        self.0.emit(event, fields);
    }
}
