/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The WebRTC transport assembled as the server assembles it, for proving it against a real browser
//! until the server's supervisor registers it: the real HTTP application, the real capture of
//! `DISPLAY`, the real input stack, and the dashboard. `build/webrtc-rig/` holds the scripts that
//! drive it through Playwright. Its settings come from the command line and `SELKIES_*`, as the
//! server's do.

use std::sync::Arc;
use std::sync::OnceLock;

use log::info;
use selkies::audio::{AudioEngine, Framing};
use selkies::clipboard::{Clipboard, CompositorClipboard, Policy};
use selkies::gamepad::GamepadSlots;
use selkies::http::{self, AppState, HttpConfig, Server};
use selkies::input::{Dispatcher, GamepadSink, InputConfig, WaylandSeat};
use selkies::session_screens::{AppScreens, SessionScreens};
use selkies::settings::{self, Settings, settings};
use selkies::webrtc::host::{PixelPipelines, ServerPlane, WebRtcDeps};
use selkies::webrtc::wiring::{
    RtcClipboardHost, RtcClipboardPolicy, RtcInputHost, RtcPointerSink, RtcStreaming, ice_topology,
};
use selkies::webrtc::{WebRtcService, routes};
use selkies::ws::host::Supervisor as WsSupervisor;
use selkies::ws::wiring;

/// The supervisor a rig needs of its transport: always the mode it serves.
struct RigSupervisor;

impl WsSupervisor for RigSupervisor {
    fn current_mode(&self) -> Option<String> {
        Some("webrtc".to_string())
    }

    fn set_clients_present(&self, _present: bool) {}

    fn pending_print_documents(&self) -> Vec<(String, u64)> {
        Vec::new()
    }
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let resolved = Settings::parse(&args, &|name| std::env::var(name).ok());
    selkies::log::configure(resolved.debug().on);
    settings::install(resolved);
    tokio::task::spawn_blocking(|| settings::settings_mut().resolve_encoder_backends())
        .await
        .map_err(|e| e.to_string())?;
    let is_wayland = settings().wayland().on;

    let cfg = HttpConfig::from_settings(&settings());
    let static_root = http::static_files::resolve_static_root(&cfg.web_root);
    let streaming: Arc<selkies::server::stack::LateService> = Arc::default();
    let modes = Arc::new(http::supervisor::ModeSupervisor::new(vec![(
        "webrtc".to_string(),
        streaming.clone() as Arc<dyn http::supervisor::StreamingService>,
    )]));
    let state = AppState::new(cfg, modes.clone()).with_static_root(static_root);

    let screens_backend = Arc::new(pixelflux::ScreenCapture::new());
    let screens = SessionScreens::new(
        is_wayland.then(|| screens_backend.clone() as Arc<dyn AppScreens>),
        is_wayland,
        std::env::var_os("XDG_RUNTIME_DIR").map(Into::into),
    );
    let policy = Arc::new(RtcClipboardPolicy(OnceLock::new()));
    let service = WebRtcService::new(
        WebRtcDeps {
            tokens: Arc::new(state.tokens.clone()),
            clipboard: policy.clone(),
            display: Arc::new(ServerPlane(Arc::new(wiring::ServerDisplay::new(
                screens.clone(),
            )))),
            pipelines: Arc::new(PixelPipelines {
                captures: Arc::new(wiring::Captures),
            }),
            audio: Some(Arc::new(AudioEngine::new(Framing::Raw))),
            webcam: Arc::new(wiring::SharedWebcam),
            stats: Arc::new(wiring::MonitorStats::default()),
            supervisor: Arc::new(RigSupervisor),
            metrics: None,
            rtc_metrics: None,
        },
        ice_topology(Vec::new())?,
    )?;
    let gamepads = GamepadSlots::from_settings();
    gamepads.start().await;
    gamepads.run_sweeper();
    let dispatcher = Dispatcher::new(
        InputConfig::from_settings(),
        Arc::new(RtcInputHost {
            service: service.clone(),
            screens: screens.clone(),
        }),
        gamepads.clone() as Arc<dyn GamepadSink>,
        Arc::new(RtcPointerSink(Arc::downgrade(&service))),
        is_wayland.then(|| screens_backend.clone() as Arc<dyn WaylandSeat>),
        None,
    );
    screens.attach(&dispatcher);
    let clipboard = Clipboard::new(
        RtcClipboardHost::new(service.engine().clone()),
        dispatcher.clone(),
        is_wayland.then(|| screens_backend.clone() as Arc<dyn CompositorClipboard>),
        Policy::from_settings(),
    );
    dispatcher.set_clipboard(Arc::new(clipboard.clone()));
    let _ = policy.0.set(clipboard.clone());
    service.attach_input(Arc::new(wiring::DispatcherInput(dispatcher.clone())));
    streaming.bind(Arc::new(RtcStreaming {
        service: service.clone(),
        dispatcher,
        clipboard,
        gamepads,
        devices: Arc::default(),
        screens,
        stop: tokio::sync::Notify::new(),
    }));
    let state = state.with_turn(service.clone());
    state.tokens.add_reconcile_hook(service.clone());
    let router = http::build(state.clone(), vec![routes(service.clone())]);
    let server = Server::start(&state, router)
        .await
        .map_err(|e| e.to_string())?;
    http::Supervisor::switch_to_mode(&*modes, "webrtc").await?;
    info!(target: "main", "Rig serving");
    tokio::signal::ctrl_c().await.map_err(|e| e.to_string())?;
    modes.stop().await;
    server.stop().await;
    Ok(())
}
