/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The stream server: the one HTTP application every deployment fronts and the supervisor
//! of the active streaming service (websockets or webrtc), so only one owns capture at a
//! time (`stream_server.py`'s `CentralizedStreamServer`, `__main__.py`'s `run`).
//!
//! `run` publishes the socket directories the interposers read, starts the Computer-Use
//! server where configured, waits for the app's ready file, settles the desktop's density
//! on X11 before any page can compare against it, probes the encoders this host serves once
//! (off the runtime, since the probe opens the GPU), logs the one startup line, switches to
//! the configured mode, and serves until SIGTERM, SIGHUP, or Ctrl-C, when the first signal
//! wins and the teardown runs once.

pub mod stack;

use log::{info, warn};

use crate::settings::{settings, socket_dir};

/// The one line that says what this server came up as: transport, capture backend, encoder
/// and rate, how the desktop is sized, and which of audio, gamepads, and access control are
/// on; the facts a log reader needs before any client line makes sense.
pub fn startup_summary() -> String {
    let s = settings();
    let backend = if s.wayland().on {
        let host = s.wayland_host_display();
        if host.is_empty() {
            "Wayland".to_string()
        } else {
            format!("Wayland (host compositor '{host}')")
        }
    } else {
        format!(
            "X11 ({})",
            std::env::var("DISPLAY")
                .ok()
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| "no DISPLAY".into())
        )
    };
    let (lo, hi) = s.framerate();
    let fps = if lo == hi {
        format!("{lo}")
    } else {
        format!("{lo}-{hi}")
    };
    let size = if s.manual_resolution().on {
        format!("fixed {}x{}", s.manual_width(), s.manual_height())
    } else if s.enable_resize().on {
        "sized by the client".to_string()
    } else {
        "desktop size kept".to_string()
    };
    let access = if !s.master_token().is_empty() {
        "token"
    } else if s.enable_basic_auth().on {
        "basic auth"
    } else {
        "open"
    };
    format!(
        "Selkies {} starting: {} transport, {backend} capture, encoder {} at {fps} fps, {size}, audio {}, gamepads {}, access {access}.",
        env!("CARGO_PKG_VERSION"),
        s.mode(),
        s.encoder(),
        if s.audio_enabled().on { "on" } else { "off" },
        if s.gamepad_enabled().on { "on" } else { "off" },
    )
}

/// Wait for the streaming app's ready signal: returns at once unless `app_wait_ready` is
/// set, else polls until a sidecar creates the ready file.
async fn wait_for_app_ready() {
    let (ready_file, wait) = {
        let s = settings();
        (s.app_ready_file().to_string(), s.app_wait_ready().on)
    };
    if !wait {
        return;
    }
    info!(target: "main", "Waiting for streaming app ready file: {ready_file}");
    while !std::path::Path::new(&ready_file).exists() {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Resolve until the first of SIGTERM, SIGHUP, or Ctrl-C; later signals are absorbed while
/// the teardown runs.
#[cfg(target_os = "linux")]
async fn shutdown_signal() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).ok();
    let mut hup = signal(SignalKind::hangup()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => "SIGINT",
        _ = async { match term.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending::<()>().await } } => "SIGTERM",
        _ = async { match hup.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending::<()>().await } } => "SIGHUP",
    }
}

/// Resolve until Ctrl-C, the one signal the console carries on this OS; later signals are
/// absorbed while the teardown runs.
#[cfg(not(target_os = "linux"))]
async fn shutdown_signal() -> &'static str {
    let _ = tokio::signal::ctrl_c().await;
    "Ctrl-C"
}

/// Bring the pixelflux compositor socket up on Wayland before any session app starts, so an
/// early-launched app finds `WAYLAND_DISPLAY`.
/// Linux alone: the compositor it brings up is the in-process Wayland one.
#[cfg(target_os = "linux")]
fn bring_up_wayland() {
    let (width, height, render, auto_gpu, cursor, host) = {
        let s = settings();
        if !s.wayland().on {
            return;
        }
        (
            s.manual_width() as i32,
            s.manual_height() as i32,
            s.render_dri().to_string(),
            s.auto_gpu().to_string(),
            s.cursor_size() as i32,
            s.wayland_host_display().to_string(),
        )
    };
    match pixelflux::ensure_wayland_display(width, height, render, auto_gpu, cursor) {
        Ok(name) if !name.is_empty() => {
            let shown = if host.is_empty() { name.clone() } else { host };
            // Before any task reads the environment: nothing else runs yet.
            unsafe { std::env::set_var("WAYLAND_DISPLAY", &shown) };
            info!(target: "server", "Wayland compositor socket: {name}");
        }
        Ok(_) => {
            warn!(target: "server", "Wayland compositor socket did not come up within its startup window; WAYLAND_DISPLAY left unchanged.");
        }
        Err(e) => warn!(target: "server", "Wayland display not initialized: {e}"),
    }
}

/// Nothing to bring up where no compositor runs: the Wayland socket is Linux's.
#[cfg(not(target_os = "linux"))]
fn bring_up_wayland() {}

/// Build the stream server, register its services, and run until a signal.
pub async fn run() -> Result<(), String> {
    use crate::clipboard::{Clipboard, CompositorClipboard, Policy};
    use crate::files::{self, FilesConfig, FilesState, Hooks};
    use crate::gamepad::GamepadSlots;
    use crate::http::{self, AppState, HttpConfig, Server};
    use crate::input::{Dispatcher, GamepadSink, InputConfig, WaylandSeat};
    use crate::session_screens::{AppScreens, SessionScreens};
    use crate::webrtc::WebRtcService;
    use crate::webrtc::host::{PixelPipelines, ServerPlane, WebRtcDeps};
    use crate::webrtc::wiring::{
        RtcClipboardHost, RtcClipboardPolicy, RtcInputHost, RtcPointerSink, RtcStreaming,
        ice_topology,
    };
    use crate::ws::host::WsDeps;
    use crate::ws::{self, WsService, wiring};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    {
        let s = settings();
        http::require_configured_credentials(&s).map_err(|e| e.to_string())?;
        http::require_one_listen_setting(&s).map_err(|e| e.to_string())?;
        // The LD_PRELOAD interposers in app processes read these, so they use the same
        // directories selkies does however the settings were supplied.
        unsafe {
            std::env::set_var("SELKIES_JS_SOCKET_PATH", socket_dir(s.js_socket_path()));
            std::env::set_var(
                "SELKIES_WEBCAM_SOCKET_PATH",
                socket_dir(s.webcam_socket_path()),
            );
        }
        if !s.computer_use_bind().is_empty() {
            let token = s.computer_use_token();
            if let Err(e) = pixelflux::start_computer_use(
                s.computer_use_bind(),
                (!token.is_empty()).then_some(token),
            ) {
                warn!(target: "main", "Computer-Use server not started: {e}");
            }
        }
    }
    wait_for_app_ready().await;

    let is_wayland = settings().wayland().on;
    if !is_wayland {
        let (dpi, locked, cursor) = {
            let s = settings();
            (
                s.scaling_dpi().parse::<f64>().unwrap_or(96.0) as i32,
                s.was_provided("scaling_dpi"),
                s.cursor_size() as i32,
            )
        };
        let startup_dpi = crate::display::dpi::restore_dpi(dpi, locked).await;
        if cursor > 0 {
            crate::display::dpi::set_cursor_size(crate::display::cursor_size_for_dpi(
                startup_dpi as f64,
                cursor,
            ))
            .await;
        }
    }

    tokio::task::spawn_blocking(|| crate::settings::settings_mut().resolve_encoder_backends())
        .await
        .map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(bring_up_wayland)
        .await
        .map_err(|e| e.to_string())?;

    let cfg = HttpConfig::from_settings(&settings());
    http::audit::install(http::audit::AuditConfig {
        url: settings().audit_webhook_url().to_string(),
        token: settings().audit_webhook_token().to_string(),
        timeout: std::time::Duration::from_secs_f64(settings().audit_webhook_timeout().max(0.1)),
    });
    let static_root = http::static_files::resolve_static_root(&cfg.web_root);

    let late_ws = Arc::new(stack::LateService::default());
    let late_rtc = Arc::new(stack::LateService::default());
    let modes = Arc::new(http::supervisor::ModeSupervisor::new(vec![
        (
            "websockets".to_string(),
            late_ws.clone() as Arc<dyn http::supervisor::StreamingService>,
        ),
        (
            "webrtc".to_string(),
            late_rtc.clone() as Arc<dyn http::supervisor::StreamingService>,
        ),
    ]));
    let metrics = settings().enable_metrics_http().on.then(|| {
        Arc::new(stack::ServerMetrics(Arc::new(
            crate::stats::metrics::Metrics::new(false),
        )))
    });
    let mut state = AppState::new(cfg, modes.clone()).with_static_root(static_root);
    if let Some(m) = &metrics {
        state = state.with_metrics(m.clone());
    }

    let files_state = Arc::new(FilesState::new(
        FilesConfig::from_settings(&settings()),
        Hooks::default(),
    ));
    let screens_backend = Arc::new(pixelflux::ScreenCapture::new());
    let screens = SessionScreens::new(
        is_wayland.then(|| screens_backend.clone() as Arc<dyn AppScreens>),
        is_wayland,
        std::env::var_os("XDG_RUNTIME_DIR").map(Into::into),
    );
    let transport_supervisor = Arc::new(stack::TransportSupervisor {
        modes: modes.clone(),
        files: files_state.clone(),
        present: AtomicBool::new(false),
    });
    let late_clipboard = Arc::new(stack::LateClipboard::default());
    let service = WsService::new(WsDeps {
        tokens: Arc::new(state.tokens.clone()),
        clipboard: late_clipboard.clone(),
        display: Arc::new(wiring::ServerDisplay::new(screens.clone())),
        captures: Arc::new(wiring::Captures),
        audio: Arc::new(wiring::EngineAudio(crate::audio::AudioEngine::new(
            crate::audio::Framing::Headered,
        ))),
        webcam: Arc::new(wiring::SharedWebcam),
        stats: Arc::new(wiring::MonitorStats::default()),
        supervisor: transport_supervisor.clone(),
        metrics: metrics.clone().map(|m| m as Arc<dyn ws::host::Metrics>),
    });

    let gamepads = GamepadSlots::from_settings();
    gamepads.start().await;
    gamepads.run_sweeper();
    let devices = Arc::new(stack::PublishedDevices::default());
    let pointer = stack::WsPointerSink(service.clone().into());
    let dispatcher = Dispatcher::new(
        InputConfig::from_settings(),
        Arc::new(wiring::WsInputHost {
            service: service.clone(),
            screens: screens.clone(),
        }),
        gamepads.clone() as Arc<dyn GamepadSink>,
        Arc::new(pointer),
        is_wayland.then(|| screens_backend.clone() as Arc<dyn WaylandSeat>),
        Some(devices.clone()),
    );
    screens.attach(&dispatcher);
    let clipboard = Clipboard::new(
        service.clone(),
        dispatcher.clone(),
        is_wayland.then(|| screens_backend.clone() as Arc<dyn CompositorClipboard>),
        Policy::from_settings(),
    );
    dispatcher.set_clipboard(Arc::new(clipboard.clone()));
    let _ = late_clipboard.0.set(clipboard.clone());
    service.attach_input(Arc::new(wiring::DispatcherInput(dispatcher.clone())));
    state.tokens.add_reconcile_hook(service.clone());
    late_ws.bind(Arc::new(stack::WsStreaming {
        service: service.clone(),
        dispatcher,
        clipboard,
        gamepads: gamepads.clone(),
        devices: devices.clone(),
        screens: screens.clone(),
        stop: tokio::sync::Notify::new(),
    }));

    // The WebRTC transport has its own input stack and clipboard over the same seat, gamepads, and
    // devices, and a session ladder of its own; only one transport runs at a time.
    let rtc_screens = SessionScreens::new(
        is_wayland.then(|| screens_backend.clone() as Arc<dyn AppScreens>),
        is_wayland,
        std::env::var_os("XDG_RUNTIME_DIR").map(Into::into),
    );
    let rtc_policy = Arc::new(RtcClipboardPolicy(std::sync::OnceLock::new()));
    let rtc_service = WebRtcService::new(
        WebRtcDeps {
            tokens: Arc::new(state.tokens.clone()),
            clipboard: rtc_policy.clone(),
            display: Arc::new(ServerPlane(Arc::new(wiring::ServerDisplay::new(
                rtc_screens.clone(),
            )))),
            pipelines: Arc::new(PixelPipelines {
                captures: Arc::new(wiring::Captures),
            }),
            audio: Some(Arc::new(crate::audio::AudioEngine::new(
                crate::audio::Framing::Raw,
            ))),
            webcam: Arc::new(wiring::SharedWebcam),
            stats: Arc::new(wiring::MonitorStats::default()),
            supervisor: transport_supervisor.clone(),
            metrics: metrics.clone().map(|m| m as Arc<dyn ws::host::Metrics>),
            rtc_metrics: metrics
                .clone()
                .map(|m| Arc::new(m.0.clone()) as Arc<dyn crate::webrtc::host::RtcMetrics>),
        },
        ice_topology(Vec::new())?,
    )?;
    let rtc_dispatcher = Dispatcher::new(
        InputConfig::from_settings(),
        Arc::new(RtcInputHost {
            service: rtc_service.clone(),
            screens: rtc_screens.clone(),
        }),
        gamepads.clone() as Arc<dyn GamepadSink>,
        Arc::new(RtcPointerSink(Arc::downgrade(&rtc_service))),
        is_wayland.then(|| screens_backend.clone() as Arc<dyn WaylandSeat>),
        Some(devices.clone()),
    );
    rtc_screens.attach(&rtc_dispatcher);
    let rtc_clipboard = Clipboard::new(
        RtcClipboardHost::new(rtc_service.engine().clone()),
        rtc_dispatcher.clone(),
        is_wayland.then(|| screens_backend.clone() as Arc<dyn CompositorClipboard>),
        Policy::from_settings(),
    );
    rtc_dispatcher.set_clipboard(Arc::new(rtc_clipboard.clone()));
    let _ = rtc_policy.0.set(rtc_clipboard.clone());
    rtc_service.attach_input(Arc::new(wiring::DispatcherInput(rtc_dispatcher.clone())));
    state.tokens.add_reconcile_hook(rtc_service.clone());
    late_rtc.bind(Arc::new(RtcStreaming {
        service: rtc_service.clone(),
        dispatcher: rtc_dispatcher,
        clipboard: rtc_clipboard,
        gamepads: gamepads.clone(),
        devices: devices.clone(),
        screens: rtc_screens,
        stop: tokio::sync::Notify::new(),
    }));
    let rtc_sessions = Arc::new(crate::webrtc::wiring::RtcSessions::default());
    rtc_sessions.bind(rtc_service.clone());
    let state = state.with_turn(rtc_service.clone());

    let hooks = Hooks {
        sessions: Some(Arc::new(stack::ActiveSessions {
            modes: modes.clone(),
            ws: service.clone(),
            rtc: rtc_sessions,
        })),
        audit: http::audit::current().map(|a| a as Arc<dyn files::AuditSink>),
        capture_sink: Some(Arc::new(stack::SinkProvisioner)),
        wayland_output_id: Some(|name: &str| crate::display::wayland_output_id(Some(name))),
    };
    files_state.set_hooks(hooks);
    if let Some(audit) = http::audit::current() {
        crate::webcam::demand::install_audit_sink(Arc::new(stack::DemandAudit(audit)));
    }
    crate::webcam::demand::install_recorders(Arc::new(stack::SoundRecorders(
        tokio::runtime::Handle::current(),
    )));

    let extra = vec![
        axum::Router::new().nest("/api", files::router(files_state.clone())),
        ws::routes(service.clone()),
        crate::webrtc::routes(rtc_service.clone()),
    ];
    let router = http::build(state.clone(), extra);

    // The service comes up before the port opens, so a page connecting the moment it accepts
    // finds its transport in force and has its first SETTINGS read.
    info!(target: "main", "{}", startup_summary());
    let mode = settings().mode().to_string();
    crate::http::Supervisor::switch_to_mode(&*modes, &mode).await?;
    let server = Server::start(&state, router)
        .await
        .map_err(|e| e.to_string())?;

    let mut print_watcher = None;
    let mut print_queue = None;
    if settings().printing_enabled().on {
        let spool = files_state.print_spool().to_path_buf();
        let announce = files_state.clone();
        match files::printing::SpoolWatcher::start(
            spool.clone(),
            Arc::new(move |name: &str, size: u64| {
                if let Some(sessions) = announce.hooks().sessions {
                    sessions.announce_print_document(name, size);
                }
            }),
        ) {
            Ok(watcher) => {
                files_state.set_print_watching(true);
                let mut queue = files::printing::PrintQueue::new(spool);
                queue.start().await;
                print_queue = Some(queue);
                print_watcher = Some(watcher);
            }
            Err(e) => {
                warn!(target: "printing", "Printing is off: the spool cannot be watched ({e}).")
            }
        }
    }

    let name = shutdown_signal().await;
    info!(target: "main", "Received {name}, shutting down");
    if let Some(mut queue) = print_queue {
        queue.stop().await;
    }
    if let Some(mut watcher) = print_watcher {
        watcher.stop();
    }
    modes.stop().await;
    server.stop().await;
    gamepads.close().await;
    crate::webcam::stop_shared_webcam().await;
    http::audit::close().await;
    tokio::task::spawn_blocking(pixelflux::shutdown).await.ok();
    Ok(())
}
