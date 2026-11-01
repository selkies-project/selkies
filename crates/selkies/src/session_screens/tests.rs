/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The session-screen ladder over a recording pixelflux and a live labwc
//! (`test_session_screen_kwin.py`, `test_session_xwayland.py`), the layout passes against a model
//! of the capture compositor's overlap rule (`test_wayland_secondary_shrink.py`), the start verdict
//! (`test_wayland_start_verdict.py`), and the apps runner (`test_apps_local_repo.py`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use pixelflux::{Error, OutputDesc, ScreenCapture};

use super::apps::{changes_installed_set, runner_env};
use super::layout::{self, PrimaryCapture};
use super::testing::{PrivateLabwc, Scratch, isolated, run_isolated, script};
use super::*;
use crate::display::wayland::CompositorOutputs;
use crate::display::{Layout, WaylandOutputs};
use crate::settings::Settings;
use crate::ws::host::CaptureHandle;
use crate::ws::testkit::{FakeCapture, settings_installed};

const SESSION: &str = "wayland-0";

/// pixelflux's session-screen calls, recording what was asked.
struct FakeScreens {
    kde: Mutex<bool>,
    calls: Mutex<Vec<String>>,
    screens: Mutex<Vec<AppScreen>>,
    parked: Mutex<usize>,
    fail_add: Mutex<bool>,
    fail_control: Mutex<bool>,
    /// What the output-management calls answer: whether the compositor manages outputs.
    manages: Mutex<Result<bool, String>>,
    outputs: Mutex<Vec<u32>>,
}

impl FakeScreens {
    fn new(kde: bool) -> Arc<FakeScreens> {
        Arc::new(FakeScreens {
            kde: Mutex::new(kde),
            calls: Mutex::default(),
            screens: Mutex::new(vec![("WL-1".into(), 0, 0, 1920, 1080)]),
            parked: Mutex::default(),
            fail_add: Mutex::default(),
            fail_control: Mutex::default(),
            manages: Mutex::new(Ok(true)),
            outputs: Mutex::default(),
        })
    }

    fn note(&self, call: String) {
        self.calls.lock().push(call);
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.calls.lock())
    }

    fn asked(&self, prefix: &str) -> bool {
        self.calls.lock().iter().any(|c| c.starts_with(prefix))
    }
}

impl AppScreens for FakeScreens {
    fn set_app_output_scale(
        &self,
        display: &str,
        scale: f64,
        index: usize,
    ) -> Result<bool, String> {
        self.note(format!("scale {display} {scale} {index}"));
        self.manages.lock().clone()
    }

    fn set_app_screen_geometry(
        &self,
        display: &str,
        width: i32,
        height: i32,
        scale: f64,
        index: usize,
    ) -> Result<bool, String> {
        self.note(format!(
            "geometry {display} {width}x{height} {scale} {index}"
        ));
        self.manages.lock().clone()
    }

    fn set_app_screen_layout(
        &self,
        display: &str,
        rects: Vec<(i32, i32, i32, i32)>,
    ) -> Result<usize, String> {
        self.note(format!("layout {display} {rects:?}"));
        Ok(rects.len())
    }

    fn list_app_screens(&self, display: &str) -> Result<Vec<AppScreen>, String> {
        self.note(format!("list {display}"));
        Ok(self.screens.lock().clone())
    }

    fn app_screen_control_offered(&self, display: &str) -> Result<bool, String> {
        self.note(format!("control {display}"));
        if *self.fail_control.lock() {
            return Err("connect: no such socket".into());
        }
        Ok(*self.kde.lock())
    }

    fn add_app_screen(
        &self,
        display: &str,
        name: &str,
        width: i32,
        height: i32,
        scale: f64,
    ) -> Result<(), String> {
        self.note(format!("add {display} {name} {width} {height} {scale}"));
        if *self.fail_add.lock() {
            return Err("virtual screen refused".into());
        }
        self.screens
            .lock()
            .push((name.to_string(), 1920, 0, width, height));
        *self.parked.lock() += 1;
        Ok(())
    }

    fn remove_app_screen(&self, name: &str) -> Result<bool, String> {
        self.note(format!("remove {name}"));
        let mut screens = self.screens.lock();
        let before = screens.len();
        screens.retain(|s| s.0 != name);
        Ok(screens.len() != before)
    }

    fn hold_spare_app_screens(
        &self,
        display: &str,
        keep: usize,
        width: i32,
        height: i32,
    ) -> Result<usize, String> {
        self.note(format!("hold {display} {keep} {width}x{height}"));
        Ok(self.screens.lock().len().saturating_sub(keep))
    }

    fn parked_windows(&self) -> Result<usize, String> {
        Ok(*self.parked.lock())
    }

    fn capture_output_ids(&self) -> Result<Vec<u32>, String> {
        Ok(self.outputs.lock().clone())
    }
}

/// The input handler's reading of the session.
struct FakeSession {
    display: String,
    separate: Mutex<bool>,
    x11: Mutex<Option<String>>,
    dpi: Mutex<f64>,
}

impl FakeSession {
    fn nested(display: &str) -> Arc<FakeSession> {
        Arc::new(FakeSession {
            display: display.to_string(),
            separate: Mutex::new(true),
            x11: Mutex::new(None),
            dpi: Mutex::new(96.0),
        })
    }

    fn plain() -> Arc<FakeSession> {
        let session = Self::nested(SESSION);
        *session.separate.lock() = false;
        session
    }
}

impl AppCompositor for FakeSession {
    fn app_wayland_display(&self) -> String {
        self.display.clone()
    }

    fn has_separate_app_compositor(&self) -> bool {
        *self.separate.lock()
    }

    fn x11_session_display(&self) -> Option<String> {
        self.x11.lock().clone()
    }

    fn system_dpi(&self) -> f64 {
        *self.dpi.lock()
    }
}

/// A ladder over `screens` with no control socket in an empty runtime directory.
fn ladder(
    screens: &Arc<FakeScreens>,
    session: &Arc<FakeSession>,
) -> (Arc<SessionScreens>, Scratch) {
    settings_installed();
    let runtime = Scratch::new("screens");
    let backend: Arc<dyn AppScreens> = screens.clone();
    let ladder = SessionScreens::new(Some(backend), true, Some(runtime.0.clone()));
    ladder.attach(session);
    (ladder, runtime)
}

// --- The KWin rung (test_session_screen_kwin.py) -------------------------------------------------

#[tokio::test]
async fn a_session_without_the_protocol_needs_a_spare_screen() {
    let pf = FakeScreens::new(false);
    let session = FakeSession::nested(SESSION);
    let (screens, _runtime) = ladder(&pf, &session);
    let (ok, why) = screens.probe_capability().await;
    assert!(
        !ok && why.contains("no screen control"),
        "a session serving no virtual-output protocol, holding one screen, offers no second display: {why}"
    );
    assert!(
        pf.asked(&format!("control {SESSION}")),
        "after its registry was read"
    );
    assert!(pf.asked("list "), "and the screen count decided");
    pf.take();
    screens.probe_capability().await;
    assert!(
        !pf.asked("control "),
        "a re-probe keeps that answer rather than reading the registry again"
    );

    let pf = FakeScreens::new(false);
    pf.screens.lock().push(("WL-2".into(), 1920, 0, 1920, 1080));
    let (screens, _runtime) = ladder(&pf, &session);
    let (ok, _) = screens.probe_capability().await;
    assert!(
        ok && !screens.control_available() && screens.probed_screen_count() == 2,
        "a session without the protocol falls back to its spare screens"
    );
}

#[tokio::test]
async fn an_unreachable_compositor_is_asked_again() {
    let pf = FakeScreens::new(true);
    let session = FakeSession::nested(SESSION);
    let (screens, _runtime) = ladder(&pf, &session);
    *pf.fail_control.lock() = true;
    screens.probe_capability().await;
    *pf.fail_control.lock() = false;
    pf.take();
    let (ok, _) = screens.probe_capability().await;
    assert!(ok && pf.asked(&format!("control {SESSION}")));
}

#[tokio::test]
async fn a_kwin_grows_and_retires_screens_as_virtual_outputs() {
    let pf = FakeScreens::new(true);
    let session = FakeSession::nested(SESSION);
    let (screens, runtime) = ladder(&pf, &session);
    let (ok, _) = screens.probe_capability().await;
    assert!(
        ok,
        "a kwin serving the virtual-output protocol is offered the rung"
    );
    assert_eq!(
        pf.take(),
        [format!("control {SESSION}")],
        "after the one question"
    );
    assert!(
        screens.control_available() && !screens.ipc_available(),
        "and the control is what the ladder reports"
    );
    screens.probe_capability().await;
    assert!(
        !pf.asked("control "),
        "the answer is kept while the session socket is the same"
    );
    std::fs::write(runtime.path(SESSION), b"").expect("a new socket");
    pf.take();
    screens.probe_capability().await;
    assert!(
        pf.asked(&format!("control {SESSION}")),
        "a restarted session compositor, a new socket, is probed afresh"
    );

    pf.take();
    let grown = screens
        .ensure_screen("display2", Some((1280, 720)), 1.25)
        .await;
    assert!(grown, "a display grows a screen");
    assert!(
        pf.asked(&format!("add {SESSION} SELKIES-2 1280 720 1.25")),
        "named after its output id, seeded with its size: {:?}",
        pf.calls.lock()
    );
    assert_eq!(
        screens.owned.lock().get("display2").map(String::as_str),
        Some("SELKIES-2"),
        "and the ladder records the screen it grew"
    );

    pf.take();
    screens
        .ensure_screen("display2", Some((1280, 720)), 1.0)
        .await;
    assert!(
        !pf.asked("add "),
        "a display already holding a screen grows no second one"
    );
    screens
        .ensure_screen("primary", Some((1280, 720)), 1.0)
        .await;
    assert!(
        !pf.asked("add "),
        "nor does the primary, whose screen the session booted with"
    );

    let positions = screens
        .positions(&["primary".to_string(), "display2".to_string()])
        .await;
    assert_eq!(
        (positions["primary"], positions["display2"]),
        (0, 1),
        "the grown screen is addressed by its place among the session's screens"
    );

    pf.take();
    screens.ensure_screens(&["display2".to_string()]).await;
    assert!(
        !pf.asked("remove "),
        "a display still attached keeps its screen"
    );

    pf.take();
    screens.ensure_screens(&[]).await;
    assert!(
        pf.asked("remove SELKIES-2") && screens.owned.lock().is_empty(),
        "a departed display's screen is closed through the rung that grew it"
    );
    assert_eq!(
        pf.screens
            .lock()
            .iter()
            .map(|s| s.0.as_str())
            .collect::<Vec<_>>(),
        ["WL-1"],
        "and the session is back to the screen it booted with"
    );

    *pf.fail_add.lock() = true;
    pf.take();
    let grown = screens
        .ensure_screen("display3", Some((800, 600)), 1.0)
        .await;
    assert!(
        !grown && screens.owned.lock().is_empty(),
        "a refused virtual output leaves the display without a screen"
    );
}

/// The one logger of a process of its own, recording the lines.
struct Recorder(Mutex<Vec<String>>);

impl log::Log for Recorder {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        self.0.lock().push(record.args().to_string());
    }

    fn flush(&self) {}
}

static RECORDER: Recorder = Recorder(Mutex::new(Vec::new()));

#[tokio::test]
#[ignore = "run by a_refused_screen_says_so, in a process of its own"]
async fn a_refused_screen_says_so_in_its_process() {
    if !isolated() {
        return;
    }
    log::set_logger(&RECORDER).expect("the only logger of this process");
    log::set_max_level(log::LevelFilter::Debug);
    let pf = FakeScreens::new(true);
    let session = FakeSession::nested(SESSION);
    let (screens, _runtime) = ladder(&pf, &session);
    screens.probe_capability().await;
    *pf.fail_add.lock() = true;
    screens
        .ensure_screen("display3", Some((800, 600)), 1.0)
        .await;
    let lines = RECORDER.0.lock();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("could not add a screen for 'display3'")),
        "{lines:?}"
    );
}

#[test]
fn a_refused_screen_says_so() {
    run_isolated(
        "session_screens::tests::a_refused_screen_says_so_in_its_process",
        &[],
    );
}

// --- The second display's reasons (test_session_xwayland.py) -------------------------------------

#[tokio::test]
async fn what_a_session_offers_a_second_display() {
    let pf = FakeScreens::new(false);
    let session = FakeSession::plain();
    let (screens, _runtime) = ladder(&pf, &session);
    assert_eq!(
        screens.capability(),
        (true, String::new()),
        "Wayland clients on the capture compositor are offered a second display"
    );

    *session.x11.lock() = Some(":83".into());
    let (ok, why) = screens.capability();
    assert!(
        !ok && why.contains("rootful Xwayland"),
        "a rootful Xwayland desktop is offered no second display, and told why: {why}"
    );

    screens.ipc_ok.store(true, Ordering::Relaxed);
    assert_eq!(
        screens.capability(),
        (true, String::new()),
        "a session compositor with a screen control still is"
    );

    let x11 = SessionScreens::new(None, false, None);
    assert_eq!(x11.capability(), (true, String::new()), "X11 has no bound");
    assert!(!x11.probe_ipc().await, "and no control socket");
}

// --- The DPI ladder ------------------------------------------------------------------------------

#[tokio::test]
async fn a_dpi_lands_on_the_session_or_stays_on_the_capture() {
    let pf = FakeScreens::new(false);
    let session = FakeSession::nested(SESSION);
    let (screens, _runtime) = ladder(&pf, &session);
    assert_eq!(
        screens.realize_dpi("192", "primary", None).await,
        1.0,
        "a nested session absorbs the scale and the capture keeps 1.0"
    );
    assert_eq!(pf.take(), [format!("scale {SESSION} 2 0")]);
    assert_eq!(
        screens
            .realize_dpi("144", "primary", Some((1920, 1080)))
            .await,
        1.0
    );
    assert_eq!(
        pf.take(),
        [format!("geometry {SESSION} 1920x1080 1.5 0")],
        "a known size lands with the scale, in one configuration"
    );
    screens.realize_dpi("192", "primary", Some((0, 1080))).await;
    assert_eq!(
        pf.take(),
        [format!("scale {SESSION} 2 0")],
        "a size that is no size does not"
    );

    *pf.manages.lock() = Ok(false);
    assert_eq!(
        screens.realize_dpi("192", "primary", None).await,
        2.0,
        "a session managing no outputs follows the capture output's scale"
    );
    *pf.manages.lock() = Err("no socket".into());
    assert_eq!(
        screens.realize_dpi("192", "primary", None).await,
        2.0,
        "and so does one that cannot be reached"
    );

    let plain = FakeSession::plain();
    let pf = FakeScreens::new(false);
    let (screens, _runtime) = ladder(&pf, &plain);
    pf.take();
    assert_eq!(
        screens.realize_dpi("192", "primary", None).await,
        2.0,
        "a plain session's capture output is the only screen there is"
    );
    assert!(pf.take().is_empty());
    assert_eq!(screens.realize_dpi("nonsense", "primary", None).await, 1.0);
    assert_eq!(
        screens.realize_dpi("1", "primary", None).await,
        0.1,
        "floored"
    );
}

#[tokio::test]
async fn pre_provisioned_screens_are_addressed_by_their_order() {
    let pf = FakeScreens::new(false);
    let session = FakeSession::nested(SESSION);
    let (screens, _runtime) = ladder(&pf, &session);
    screens
        .ensure_screens(&[
            "display3".to_string(),
            "display2".to_string(),
            "primary".to_string(),
        ])
        .await;
    assert!(
        pf.take().is_empty(),
        "with no control, nothing is grown or retired"
    );
    assert_eq!(screens.position("primary").await, 0);
    assert_eq!(screens.position("").await, 0, "an empty id is the primary");
    assert_eq!(screens.position("display2").await, 1);
    assert_eq!(screens.position("display3").await, 2);
    assert_eq!(
        screens.position("display4").await,
        3,
        "a display not laid out yet follows those that are"
    );
    screens.realize_dpi("192", "display3", None).await;
    assert_eq!(pf.take(), [format!("scale {SESSION} 2 2")]);
}

#[tokio::test]
async fn every_screen_is_placed_in_one_configuration_by_its_position() {
    let pf = FakeScreens::new(true);
    let session = FakeSession::nested(SESSION);
    let (screens, _runtime) = ladder(&pf, &session);
    screens.probe_capability().await;
    screens
        .ensure_screen("display2", Some((800, 600)), 1.0)
        .await;
    pf.take();
    let mut layouts = Layouts::new();
    layouts.insert("primary".into(), Layout::new(800, 0, 1920, 1080));
    layouts.insert("display2".into(), Layout::new(0, 0, 800, 600));
    screens.apply_layout(&layouts).await;
    assert_eq!(
        pf.take().last().map(String::as_str),
        Some(format!("layout {SESSION} [(800, 0, 1920, 1080), (0, 0, 800, 600)]").as_str()),
        "the primary's rectangle first, whatever the map's order"
    );
}

#[tokio::test]
async fn spare_screens_are_held_only_without_a_control() {
    let pf = FakeScreens::new(false);
    pf.screens.lock().push(("WL-2".into(), 1920, 0, 1920, 1080));
    *pf.outputs.lock() = vec![0, 1];
    let session = FakeSession::nested(SESSION);
    let (screens, _runtime) = ladder(&pf, &session);
    screens.resync();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pf.asked("hold ") {
        assert!(Instant::now() < deadline, "the hold never ran");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        pf.take(),
        [format!("hold {SESSION} 1 320x240")],
        "the primary's view is a capture over screen 0, not a screen"
    );
    *pf.outputs.lock() = vec![0, 1, 2];
    screens.hold_spares().await;
    assert_eq!(
        pf.take(),
        [format!("hold {SESSION} 2 320x240")],
        "a secondary's screen counts"
    );

    let pf = FakeScreens::new(true);
    let (screens, _runtime) = ladder(&pf, &session);
    screens.probe_capability().await;
    pf.take();
    screens.resync();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !pf.asked("hold "),
        "a hold computed before a layout pass would shrink the screen its output just adopted"
    );
}

/// What the transport was asked when a session compositor was adopted.
#[derive(Default)]
struct FakeTransport {
    dpis: Mutex<Vec<i32>>,
    republished: Mutex<u32>,
}

#[async_trait]
impl ScreenTransport for FakeTransport {
    async fn resync_session_scale(&self, dpi: i32) {
        self.dpis.lock().push(dpi);
    }

    async fn republish_second_screen(&self) {
        *self.republished.lock() += 1;
    }
}

#[tokio::test]
async fn an_adopted_compositor_takes_the_last_dpi_and_the_pages_hear() {
    let pf = FakeScreens::new(false);
    let session = FakeSession::nested(SESSION);
    let (screens, _runtime) = ladder(&pf, &session);
    let transport = Arc::new(FakeTransport::default());
    screens.compositor_adopted(transport.clone());
    let deadline = Instant::now() + Duration::from_secs(5);
    while *transport.republished.lock() == 0 || !pf.asked("hold ") {
        assert!(Instant::now() < deadline, "the adoption never ran");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        transport.dpis.lock().is_empty(),
        "96 is unity: nothing to apply and nothing the capture could have taken"
    );

    *session.dpi.lock() = 192.0;
    screens.compositor_adopted(transport.clone());
    while transport.dpis.lock().is_empty() {
        assert!(Instant::now() < deadline, "the ladder never ran");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(*transport.dpis.lock(), [192], "the last client-synced DPI");
}

#[test]
fn an_adoption_off_the_runtime_does_nothing() {
    let pf = FakeScreens::new(false);
    let session = FakeSession::nested(SESSION);
    let (screens, _runtime) = ladder(&pf, &session);
    screens.compositor_adopted(Arc::new(FakeTransport::default()));
    screens.resync();
    let mut layouts = Layouts::new();
    layouts.insert("primary".into(), Layout::new(0, 0, 800, 600));
    screens.schedule_layout(&layouts);
    assert!(pf.take().is_empty());
}

#[tokio::test]
#[ignore = "run by an_operator_dpi_governs_an_adopted_compositor, in a process of its own"]
async fn an_operator_dpi_governs_in_its_process() {
    if !isolated() {
        return;
    }
    crate::settings::install(Settings::parse(&["--scaling-dpi=144".to_string()], &|_| {
        None
    }));
    let pf = FakeScreens::new(false);
    let session = FakeSession::nested(SESSION);
    let (screens, _runtime) = ladder(&pf, &session);
    *session.dpi.lock() = 192.0;
    assert_eq!(screens.adoption_dpi(), Some(144));
    assert_eq!(screens.realize_startup_dpi().await, 1.0);
    assert_eq!(pf.take(), [format!("scale {SESSION} 1.5 0")]);
}

#[test]
fn an_operator_dpi_governs_an_adopted_compositor() {
    run_isolated(
        "session_screens::tests::an_operator_dpi_governs_in_its_process",
        &[],
    );
}

// --- The control socket rung ---------------------------------------------------------------------

/// A control socket answering the labwc contract, recording the commands.
async fn control_socket(runtime: &Scratch, replies: Vec<&'static str>) -> Arc<Mutex<Vec<String>>> {
    let listener = tokio::net::UnixListener::bind(runtime.path(SESSION_IPC_SOCKET)).expect("bind");
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let recorded = seen.clone();
    tokio::spawn(async move {
        let mut replies = replies.into_iter();
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (reader, mut writer) = stream.into_split();
            let mut line = String::new();
            let mut reader = BufReader::new(reader);
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                continue;
            }
            recorded.lock().push(line.trim_end().to_string());
            if let Some(reply) = replies.next() {
                let _ = writer.write_all(reply.as_bytes()).await;
            }
        }
    });
    seen
}

#[tokio::test]
async fn the_control_socket_grows_and_retires_by_the_names_it_coins() {
    let pf = FakeScreens::new(false);
    let session = FakeSession::nested(SESSION);
    let (screens, runtime) = ladder(&pf, &session);
    let seen = control_socket(
        &runtime,
        vec![
            "{\"ok\":false,\"error\":\"no room\"}\n",
            "{\"ok\":true,\"output\":\"HEADLESS-2\"}\n",
            "{\"ok\":false,\"error\":\"busy\"}\n",
            "{\"ok\":true}\n",
        ],
    )
    .await;
    assert!(
        screens.probe_capability().await.0,
        "the socket answering is the control"
    );
    assert!(screens.ipc_available());
    assert!(!pf.asked("control "), "and KWin's rung is not consulted");

    assert!(
        !screens.ensure_screen("display2", None, 1.0).await,
        "a socket answering no leaves the display nowhere to show"
    );
    assert!(screens.owned.lock().is_empty());

    *pf.parked.lock() = 0;
    let parker = pf.clone();
    let parking = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        *parker.parked.lock() += 1;
    });
    let started = Instant::now();
    assert!(screens.ensure_screen("display2", None, 1.0).await);
    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "the host window is waited for"
    );
    parking.await.expect("the parker");
    assert_eq!(
        screens.owned.lock().get("display2").map(String::as_str),
        Some("HEADLESS-2"),
        "the name the reply coined"
    );

    pf.screens
        .lock()
        .push(("HEADLESS-2".into(), 1920, 0, 800, 600));
    screens.ensure_screens(&[]).await;
    assert_eq!(
        screens.owned.lock().get("display2").map(String::as_str),
        Some("HEADLESS-2"),
        "a refused removal leaves the record"
    );
    screens.ensure_screens(&[]).await;
    assert!(screens.owned.lock().is_empty());
    assert_eq!(
        *seen.lock(),
        [
            "ADD_SCREEN",
            "ADD_SCREEN",
            "REMOVE_SCREEN HEADLESS-2",
            "REMOVE_SCREEN HEADLESS-2",
        ]
    );
    assert!(!pf.asked("remove "), "removal follows the rung that grew");

    drop(pf.screens.lock().drain(1..));
    pf.screens.lock().push(("OTHER".into(), 1920, 0, 800, 600));
    screens.ensure_screens(&[]).await;
    assert_eq!(
        seen.lock().last().map(String::as_str),
        Some("REMOVE_SCREEN OTHER"),
        "a screen owned by no display is retired the same way"
    );
}

#[tokio::test]
async fn a_stale_socket_file_is_no_control() {
    let pf = FakeScreens::new(false);
    let session = FakeSession::nested(SESSION);
    let (screens, runtime) = ladder(&pf, &session);
    drop(tokio::net::UnixListener::bind(runtime.path(SESSION_IPC_SOCKET)).expect("bind"));
    assert!(runtime.path(SESSION_IPC_SOCKET).exists());
    assert!(!screens.probe_ipc().await);
}

// --- Live labwc ----------------------------------------------------------------------------------

/// A ladder over the real pixelflux calls on `labwc`, its control socket in `runtime`.
fn live_ladder(labwc: &PrivateLabwc, runtime: PathBuf) -> (Arc<SessionScreens>, Arc<FakeSession>) {
    settings_installed();
    let session = FakeSession::nested(&labwc.socket());
    let backend: Arc<dyn AppScreens> = Arc::new(ScreenCapture::new());
    let screens = SessionScreens::new(Some(backend), true, Some(runtime));
    screens.attach(&session);
    (screens, session)
}

fn live_list(labwc: &PrivateLabwc) -> Vec<AppScreen> {
    AppScreens::list_app_screens(&ScreenCapture::new(), &labwc.socket()).expect("the screens")
}

/// The whole ladder on the images' shape: a labwc nested on the real capture compositor, its screens
/// grown over the control socket, each adopted by the capture output created for its display. No
/// capture runs; what is asserted is what the two compositors ended up with.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "run by a_nested_labwc_follows_the_layout_passes, in a process of its own"]
async fn a_nested_labwc_follows_the_layout_passes_in_its_process() {
    if !isolated() {
        return;
    }
    crate::settings::install(Settings::parse(&["--wayland=true".to_string()], &|_| None));
    let runtime = std::env::var("XDG_RUNTIME_DIR").expect("a private runtime directory");
    let config = Path::new(&runtime).join("config");
    std::fs::create_dir_all(config.join("labwc")).expect("configuration");
    std::fs::write(
        config.join("labwc/rc.xml"),
        "<?xml version=\"1.0\"?>\n<labwc_config>\n  <core><decoration>server</decoration></core>\n</labwc_config>\n",
    )
    .expect("rc.xml");
    let compositor =
        pixelflux::ensure_compositor(1920, 1080, String::new(), String::new(), String::new(), -1)
            .expect("the capture compositor");
    let capture_socket = pixelflux::wayland_display_name().expect("its socket");
    let _labwc = super::testing::Owned::spawn(
        std::process::Command::new("labwc")
            .env("WAYLAND_DISPLAY", &capture_socket)
            .env("WLR_BACKENDS", "wayland")
            .env("WLR_RENDERER", "pixman")
            .env("XDG_CONFIG_HOME", &config)
            .env_remove("WLR_WL_OUTPUTS")
            .env_remove("DISPLAY"),
    );

    async fn poll<T>(what: &str, mut read: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(value) = read() {
                return value;
            }
            assert!(Instant::now() < deadline, "{what}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    let inner = poll("the nested compositor came up", || {
        crate::input::handler::detect_app_compositor_in(&runtime, &capture_socket)
    })
    .await;
    assert_ne!(inner, capture_socket);
    let session = FakeSession::nested(&inner);
    let backend: Arc<ScreenCapture> = Arc::new(ScreenCapture::new());
    let dyn_backend: Arc<dyn AppScreens> = backend.clone();
    let screens = SessionScreens::new(Some(dyn_backend), true, None);
    screens.attach(&session);
    poll("its control socket answered", || {
        crate::clipboard::x11::socket_live(&Path::new(&runtime).join(SESSION_IPC_SOCKET))
            .then_some(())
    })
    .await;
    assert_eq!(screens.probe_capability().await, (true, String::new()));
    assert!(screens.ipc_available(), "the control socket is the rung");
    let listed = || AppScreens::list_app_screens(&*backend, &inner).expect("the screens");
    poll("the session booted with a single screen", || {
        (listed().len() == 1).then_some(())
    })
    .await;
    let parked = || AppScreens::parked_windows(&*backend).expect("the windows");
    assert_eq!(parked(), 0, "the primary's host window has its output");

    let outputs = WaylandOutputs::new(compositor);
    for round in 0..2 {
        let mut layouts = Layouts::new();
        layouts.insert("primary".into(), Layout::new(1280, 0, 1920, 1080));
        layouts.insert("display2".into(), Layout::new(0, 0, 1280, 720));
        let keep: HashSet<String> = HashSet::new();
        let before =
            layout::before_primary(&outputs, Some(&screens), &mut layouts, &keep, 1.0).await;
        assert!(
            before.dropped.is_empty(),
            "round {round}: {:?}",
            before.dropped
        );
        assert_eq!(
            outputs.list().await.get(&0).map(|o| (o.1, o.2)),
            Some((1280, 0)),
            "the primary moved to its offset"
        );

        let dpis: HashMap<String, String> = [("display2".to_string(), "192".to_string())].into();
        let started = Instant::now();
        let after = layout::after_primary(&outputs, Some(&screens), &mut layouts, &dpis).await;
        assert!(
            after.dropped.is_empty(),
            "round {round}: {:?}",
            after.dropped
        );
        assert!(
            started.elapsed() < SCREEN_PARK_WAIT,
            "the grown screen's host window parked: {:?}",
            started.elapsed()
        );
        assert_eq!(
            after.scales.get("display2"),
            Some(&1.0),
            "the session took the display's DPI, and its capture keeps 1.0"
        );
        let name = screens
            .owned
            .lock()
            .get("display2")
            .cloned()
            .expect("a coined name");
        assert_eq!(listed()[1].0, name, "the reply named the screen");
        assert_eq!(screens.position("display2").await, 1);
        assert!(
            outputs.list().await.contains_key(&2),
            "the display's capture output exists"
        );
        poll("the waiting screen adopted it", || {
            (parked() == 0).then_some(())
        })
        .await;

        screens.apply_layout(&layouts).await;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let arranged = listed();
            // Closed up at the scaled screen's logical edge.
            if arranged[0].1 == 640 && arranged[1].1 == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the session arranges its screens as the capture outputs: {arranged:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            screens
                .realize_dpi("144", "primary", Some((1920, 1080)))
                .await,
            1.0,
            "the primary's screen takes a DPI as well"
        );

        let mut alone = Layouts::new();
        alone.insert("primary".into(), Layout::new(0, 0, 1920, 1080));
        let keep: HashSet<String> = ["primary".to_string()].into_iter().collect();
        layout::before_primary(&outputs, Some(&screens), &mut alone, &keep, 1.0).await;
        assert!(
            !outputs.list().await.contains_key(&2),
            "its output is destroyed"
        );
        poll("REMOVE_SCREEN retired its screen", || {
            (listed().len() == 1).then_some(())
        })
        .await;
        assert!(screens.owned.lock().is_empty());
    }
}

#[test]
#[ignore = "needs a labwc carrying the control-socket patch"]
fn a_nested_labwc_follows_the_layout_passes() {
    let runtime = Scratch::new("nested");
    run_isolated(
        "session_screens::tests::a_nested_labwc_follows_the_layout_passes_in_its_process",
        &[
            ("XDG_RUNTIME_DIR", &runtime.text()),
            ("LIBGL_ALWAYS_SOFTWARE", "1"),
        ],
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a labwc carrying the control-socket patch"]
async fn a_live_headless_labwc_refuses_a_screen_and_takes_its_arrangement() {
    let labwc = PrivateLabwc::start(2);
    let (screens, _session) = live_ladder(&labwc, labwc.runtime.0.clone());
    assert_eq!(live_list(&labwc).len(), 2);
    assert_eq!(
        screens.probe_capability().await,
        (true, String::new()),
        "its control socket answers"
    );
    assert!(screens.ipc_available());
    assert!(
        !screens
            .ensure_screen("display3", Some((800, 600)), 1.0)
            .await,
        "a screen is grown on the Wayland backend alone, and its refusal leaves the display none"
    );
    assert!(screens.owned.lock().is_empty());

    assert_eq!(
        screens
            .realize_dpi("192", "primary", Some((1280, 720)))
            .await,
        1.0,
        "output management takes the mode and the scale"
    );
    let listed = live_list(&labwc);
    assert_eq!((listed[0].3, listed[0].4), (1280, 720), "{listed:?}");

    let mut layouts = Layouts::new();
    layouts.insert("primary".into(), Layout::new(800, 0, 1280, 720));
    layouts.insert("display2".into(), Layout::new(0, 0, 800, 600));
    screens.ensure_screens(&["display2".to_string()]).await;
    screens.apply_layout(&layouts).await;
    let listed = live_list(&labwc);
    assert_eq!((listed[0].1, listed[1].1), (800, 0), "arranged: {listed:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs labwc"]
async fn a_live_session_without_a_control_holds_its_spare_screens() {
    let labwc = PrivateLabwc::start(2);
    let empty = Scratch::new("no-control");
    let (screens, _session) = live_ladder(&labwc, empty.0.clone());
    assert_eq!(live_list(&labwc).len(), 2);
    assert_eq!(
        screens.probe_capability().await,
        (true, String::new()),
        "a spare screen backs a second display"
    );
    assert!(
        !screens.control_available(),
        "labwc serves no virtual outputs"
    );
    assert_eq!(screens.probed_screen_count(), 2);

    assert!(
        screens.ensure_screen("display2", None, 1.0).await,
        "a session with no control needs no screen grown"
    );
    assert_eq!(live_list(&labwc).len(), 2);

    screens.hold_spares().await;
    let listed = live_list(&labwc);
    assert_eq!(
        (listed[1].3, listed[1].4),
        SPARE_SCREEN_SIZE,
        "the screen with no capture output is held small: {listed:?}"
    );
    assert_ne!(
        (listed[0].3, listed[0].4),
        SPARE_SCREEN_SIZE,
        "the first keeps its size"
    );

    screens.ensure_screens(&["display2".to_string()]).await;
    assert_eq!(
        screens.position("display2").await,
        1,
        "the attached secondary holds the spare"
    );
    screens
        .realize_dpi("192", "display2", Some((800, 600)))
        .await;
    let listed = live_list(&labwc);
    assert_eq!(
        (listed[1].3, listed[1].4),
        (800, 600),
        "and the display's mode lands on it: {listed:?}"
    );
}

// --- The layout passes (test_wayland_secondary_shrink.py) ----------------------------------------

/// The primary's capture is a view (output 1) over its screen (output 0): it moves with the screen
/// and never stands in anything's way.
const PRIMARY_VIEW: u32 = 1;

type Rect = (i32, i32, i32, i32);
/// An output as the model holds it: its rectangle and scale.
type Modeled = (i32, i32, i32, i32, f64);

fn overlaps(a: Rect, b: Rect) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

/// pixelflux's output management: create and reposition refuse a rectangle that overlaps a live
/// screen, a resize takes any size, and every call is recorded.
#[derive(Default)]
struct FakeCompositor {
    outputs: Mutex<HashMap<u32, Modeled>>,
    calls: Mutex<Vec<String>>,
    refuse_resize: Mutex<bool>,
    refuse_create: Mutex<bool>,
}

impl FakeCompositor {
    fn with(outputs: &[(u32, Modeled)]) -> Arc<FakeCompositor> {
        let compositor = FakeCompositor::default();
        compositor.outputs.lock().extend(outputs.iter().copied());
        Arc::new(compositor)
    }

    fn free(&self, oid: u32, rect: Rect) -> bool {
        !self.outputs.lock().iter().any(|(other, o)| {
            *other != oid && *other != PRIMARY_VIEW && overlaps(rect, (o.0, o.1, o.2, o.3))
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().clone()
    }
}

impl CompositorOutputs for FakeCompositor {
    fn list_outputs(&self) -> Result<Vec<OutputDesc>, Error> {
        let mut listed: Vec<OutputDesc> = self
            .outputs
            .lock()
            .iter()
            .map(|(oid, o)| (*oid, o.0, o.1, o.2, o.3, o.4, true))
            .collect();
        listed.sort_by_key(|o| o.0);
        Ok(listed)
    }

    fn create_output(
        &self,
        id: u32,
        width: i32,
        height: i32,
        x: i32,
        y: i32,
        scale: f64,
    ) -> Result<bool, Error> {
        self.calls
            .lock()
            .push(format!("create {id} {width} {height} {x} {y} {scale}"));
        if *self.refuse_create.lock()
            || self.outputs.lock().contains_key(&id)
            || !self.free(id, (x, y, width, height))
        {
            return Ok(false);
        }
        self.outputs.lock().insert(id, (x, y, width, height, scale));
        Ok(true)
    }

    fn destroy_output(&self, id: u32) -> Result<bool, Error> {
        self.calls.lock().push(format!("destroy {id}"));
        Ok(self.outputs.lock().remove(&id).is_some())
    }

    fn reposition_output(&self, id: u32, x: i32, y: i32) -> Result<bool, Error> {
        self.calls.lock().push(format!("move {id} {x} {y}"));
        let Some(current) = self.outputs.lock().get(&id).copied() else {
            return Ok(false);
        };
        if !self.free(id, (x, y, current.2, current.3)) {
            return Ok(false);
        }
        let mut outputs = self.outputs.lock();
        outputs.insert(id, (x, y, current.2, current.3, current.4));
        if id == 0
            && let Some(view) = outputs.get(&PRIMARY_VIEW).copied()
        {
            outputs.insert(PRIMARY_VIEW, (x, y, view.2, view.3, view.4));
        }
        Ok(true)
    }

    fn resize_output(&self, id: u32, width: i32, height: i32, scale: f64) -> Result<bool, Error> {
        self.calls
            .lock()
            .push(format!("resize {id} {width} {height}"));
        let mut outputs = self.outputs.lock();
        let Some(current) = outputs.get(&id).copied() else {
            return Ok(false);
        };
        if *self.refuse_resize.lock() {
            return Ok(false);
        }
        outputs.insert(id, (current.0, current.1, width, height, scale));
        Ok(true)
    }

    fn set_cursor_size(&self, _size: i32) -> Result<bool, Error> {
        Ok(true)
    }

    fn get_realized_geometry(&self, _id: u32) -> Result<Option<(i32, i32, f64)>, Error> {
        Ok(None)
    }
}

/// The report's geometry: a left-hand secondary at density 2 that comes down to density 1, so the
/// primary moves from 3024 to 1512 into its old rectangle.
fn before() -> Arc<FakeCompositor> {
    FakeCompositor::with(&[
        (0, (3024, 0, 1920, 992, 1.0)),
        (1, (3024, 0, 1920, 992, 1.0)),
        (2, (0, 0, 3024, 1764, 1.0)),
    ])
}

fn shrunk() -> Layouts {
    let mut layouts = Layouts::new();
    layouts.insert("primary".into(), Layout::new(1512, 0, 1920, 992));
    layouts.insert("display2".into(), Layout::new(0, 0, 1512, 882));
    layouts
}

fn outputs_over(compositor: &Arc<FakeCompositor>) -> WaylandOutputs {
    WaylandOutputs::new(compositor.clone())
}

/// A primary capture whose fit is recorded.
#[derive(Default)]
struct FakePrimary {
    fits: Mutex<Vec<(i32, i32, i32, i32)>>,
}

#[async_trait]
impl PrimaryCapture for FakePrimary {
    async fn fit(&self, x: i32, y: i32, width: i32, height: i32) {
        self.fits.lock().push((x, y, width, height));
    }
}

async fn extension(compositor: &Arc<FakeCompositor>, layouts: &Layouts) -> Result<(), String> {
    layout::apply_extension(
        &outputs_over(compositor),
        None,
        "display2",
        layouts,
        "96",
        &FakePrimary::default(),
    )
    .await
}

fn keep_both() -> HashSet<String> {
    ["primary", "display2"]
        .map(String::from)
        .into_iter()
        .collect()
}

#[tokio::test]
async fn a_shrinking_secondary_gives_its_room_up_before_the_primary_moves_webrtc() {
    let comp = before();
    assert_eq!(
        extension(&comp, &shrunk()).await,
        Ok(()),
        "the secondary is kept"
    );
    assert_eq!(
        comp.calls(),
        ["resize 2 1512 882", "move 0 1512 0"],
        "it shrinks before the primary moves into its old room"
    );
    let outputs = comp.outputs.lock();
    assert_eq!(
        (
            (outputs[&0].0, outputs[&0].1),
            (outputs[&2].2, outputs[&2].3)
        ),
        ((1512, 0), (1512, 882)),
        "the primary sits at the new offset beside the shrunken output"
    );
}

#[tokio::test]
async fn a_shrinking_secondary_gives_its_room_up_before_the_primary_moves_websockets() {
    let comp = before();
    let mut layouts = shrunk();
    let outcome =
        layout::before_primary(&outputs_over(&comp), None, &mut layouts, &keep_both(), 1.0).await;
    assert!(
        outcome.restart.is_empty() && outcome.dropped.is_empty(),
        "the shrinking secondary is kept live"
    );
    assert_eq!(
        comp.calls(),
        ["resize 2 1512 882", "move 0 1512 0", "resize 0 1920 992"],
        "it shrinks before the primary moves, and the primary's screen is fitted"
    );
}

#[tokio::test]
async fn a_mixed_change_shrinks_only_the_axis_that_gives_way() {
    let mut layouts = shrunk();
    layouts.insert("display2".into(), Layout::new(0, 0, 1512, 2000));
    let comp = before();
    assert_eq!(extension(&comp, &layouts).await, Ok(()));
    assert_eq!(comp.calls()[..2], ["resize 2 1512 1764", "move 0 1512 0"]);
}

#[tokio::test]
async fn a_growing_secondary_is_moved_away_from_not_resized() {
    let small = || {
        FakeCompositor::with(&[
            (0, (1512, 0, 1920, 992, 1.0)),
            (1, (1512, 0, 1920, 992, 1.0)),
            (2, (0, 0, 1512, 882, 1.0)),
        ])
    };
    let mut grown = Layouts::new();
    grown.insert("primary".into(), Layout::new(3024, 0, 1920, 992));
    grown.insert("display2".into(), Layout::new(0, 0, 3024, 1764));

    let comp = small();
    assert_eq!(extension(&comp, &grown).await, Ok(()));
    assert_eq!(
        comp.calls(),
        ["move 0 3024 0"],
        "it waits for the move and its capture start"
    );

    let comp = small();
    let outcome =
        layout::before_primary(&outputs_over(&comp), None, &mut grown, &keep_both(), 1.0).await;
    assert_eq!(comp.calls(), ["move 0 3024 0", "resize 0 1920 992"]);
    assert!(outcome.restart.is_empty());
}

#[tokio::test]
async fn a_refused_shrink_recreates_the_output() {
    let comp = before();
    *comp.refuse_resize.lock() = true;
    assert_eq!(extension(&comp, &shrunk()).await, Ok(()));
    assert_eq!(
        comp.calls(),
        [
            "resize 2 1512 882",
            "destroy 2",
            "move 0 1512 0",
            "create 2 1512 882 0 0 1"
        ],
        "[webrtc] after the move"
    );

    let comp = before();
    *comp.refuse_resize.lock() = true;
    let mut layouts = shrunk();
    let outcome =
        layout::before_primary(&outputs_over(&comp), None, &mut layouts, &keep_both(), 1.0).await;
    assert_eq!(
        outcome.restart,
        ["display2"],
        "[websockets] and leaves it to the start loop"
    );
    assert_eq!(
        comp.calls()[..3],
        ["resize 2 1512 882", "destroy 2", "move 0 1512 0"]
    );
}

#[tokio::test]
async fn an_unchanged_layout_touches_no_output() {
    let comp = before();
    let mut same = Layouts::new();
    same.insert("primary".into(), Layout::new(3024, 0, 1920, 992));
    same.insert("display2".into(), Layout::new(0, 0, 3024, 1764));
    assert_eq!(extension(&comp, &same).await, Ok(()));
    assert!(comp.calls().is_empty(), "{:?}", comp.calls());
}

#[tokio::test]
async fn an_output_in_the_primarys_way_is_recreated_and_an_impossible_move_reanchors() {
    // Same origin and no shrink, yet its rectangle covers the primary's new place.
    let comp = FakeCompositor::with(&[
        (0, (2000, 0, 1000, 800, 1.0)),
        (1, (2000, 0, 1000, 800, 1.0)),
        (2, (0, 0, 1500, 800, 1.0)),
    ]);
    let mut layouts = Layouts::new();
    layouts.insert("primary".into(), Layout::new(1000, 0, 1000, 800));
    layouts.insert("display2".into(), Layout::new(0, 0, 1500, 800));
    let outcome =
        layout::before_primary(&outputs_over(&comp), None, &mut layouts, &keep_both(), 1.0).await;
    assert_eq!(
        outcome.restart,
        ["display2"],
        "it blocks the primary's move"
    );
    assert_eq!(
        comp.calls()[..3],
        ["move 0 1000 0", "destroy 2", "move 0 1000 0"]
    );
    assert!(!outcome.reanchored);

    // A stale output the pass does not own is destroyed first, so nothing is left in the way.
    let comp = FakeCompositor::with(&[
        (0, (2000, 0, 1000, 800, 1.0)),
        (1, (2000, 0, 1000, 800, 1.0)),
        (5, (0, 0, 1500, 800, 1.0)),
    ]);
    let mut layouts = Layouts::new();
    layouts.insert("primary".into(), Layout::new(1000, 0, 1000, 800));
    layout::before_primary(
        &outputs_over(&comp),
        None,
        &mut layouts,
        &HashSet::new(),
        1.0,
    )
    .await;
    assert_eq!(comp.calls()[..2], ["destroy 5", "move 0 1000 0"]);

    // Nothing the pass can remove frees the primary's place, so it re-anchors alone.
    let comp = FakeCompositor::with(&[
        (0, (2000, 0, 1000, 800, 1.0)),
        (1, (2000, 0, 1000, 800, 1.0)),
    ]);
    comp.outputs.lock().insert(0, (2000, 0, 1000, 800, 1.0));
    let mut layouts = Layouts::new();
    layouts.insert("primary".into(), Layout::new(-5, 0, 1000, 800));
    layouts.insert("display2".into(), Layout::new(1000, 0, 800, 600));
    let refusing = Arc::new(RefusingMoves(comp.clone()));
    let outcome = layout::before_primary(
        &WaylandOutputs::new(refusing),
        None,
        &mut layouts,
        &keep_both(),
        1.0,
    )
    .await;
    assert!(outcome.reanchored);
    assert_eq!(
        (layouts["primary"].x, outcome.dropped[0].display_id.as_str()),
        (0, "display2")
    );
    assert!(!layouts.contains_key("display2"));
}

/// A compositor that refuses every move to anywhere but the origin.
struct RefusingMoves(Arc<FakeCompositor>);

impl CompositorOutputs for RefusingMoves {
    fn list_outputs(&self) -> Result<Vec<OutputDesc>, Error> {
        self.0.list_outputs()
    }

    fn create_output(
        &self,
        id: u32,
        width: i32,
        height: i32,
        x: i32,
        y: i32,
        scale: f64,
    ) -> Result<bool, Error> {
        self.0.create_output(id, width, height, x, y, scale)
    }

    fn destroy_output(&self, id: u32) -> Result<bool, Error> {
        self.0.destroy_output(id)
    }

    fn reposition_output(&self, id: u32, x: i32, y: i32) -> Result<bool, Error> {
        if (x, y) != (0, 0) {
            return Ok(false);
        }
        self.0.reposition_output(id, x, y)
    }

    fn resize_output(&self, id: u32, width: i32, height: i32, scale: f64) -> Result<bool, Error> {
        self.0.resize_output(id, width, height, scale)
    }

    fn set_cursor_size(&self, size: i32) -> Result<bool, Error> {
        self.0.set_cursor_size(size)
    }

    fn get_realized_geometry(&self, id: u32) -> Result<Option<(i32, i32, f64)>, Error> {
        self.0.get_realized_geometry(id)
    }
}

/// The session side of a layout: a screen grown right before its output, and what the session left
/// for the output's capture scale.
#[tokio::test]
async fn each_output_adopts_the_screen_grown_for_it_at_the_scale_the_session_left() {
    let pf = FakeScreens::new(true);
    let session = FakeSession::nested(SESSION);
    let (screens, _runtime) = ladder(&pf, &session);
    screens.probe_capability().await;
    pf.take();
    let comp = FakeCompositor::with(&[(0, (0, 0, 1920, 992, 1.0)), (1, (0, 0, 1920, 992, 1.0))]);
    let mut layouts = Layouts::new();
    layouts.insert("primary".into(), Layout::new(0, 0, 1920, 992));
    layouts.insert("display2".into(), Layout::new(1920, 0, 1280, 720));
    let dpis: HashMap<String, String> = [("display2".to_string(), "192".to_string())].into();
    let outcome =
        layout::after_primary(&outputs_over(&comp), Some(&screens), &mut layouts, &dpis).await;
    assert_eq!(
        comp.calls(),
        ["create 2 1280 720 1920 0 1"],
        "at 1.0: the session took the DPI"
    );
    assert_eq!(outcome.scales.get("display2"), Some(&1.0));
    let asked = pf.take();
    assert!(
        asked.contains(&format!("add {SESSION} SELKIES-2 1280 720 2")),
        "seeded with the display's own DPI: {asked:?}"
    );
    assert!(
        asked.contains(&format!("geometry {SESSION} 1280x720 2 1")),
        "then its screen is given it: {asked:?}"
    );

    // A session that manages no outputs leaves the scale on the capture.
    let pf = FakeScreens::new(true);
    *pf.manages.lock() = Ok(false);
    let (screens, _runtime) = ladder(&pf, &session);
    screens.probe_capability().await;
    let comp = FakeCompositor::with(&[(0, (0, 0, 1920, 992, 1.0)), (1, (0, 0, 1920, 992, 1.0))]);
    let mut layouts = Layouts::new();
    layouts.insert("display2".into(), Layout::new(1920, 0, 1280, 720));
    let outcome =
        layout::after_primary(&outputs_over(&comp), Some(&screens), &mut layouts, &dpis).await;
    assert_eq!(comp.calls(), ["create 2 1280 720 1920 0 2"]);
    assert_eq!(outcome.scales.get("display2"), Some(&2.0));
}

#[tokio::test]
async fn a_display_either_compositor_refuses_is_dropped_and_the_primary_returns_home() {
    let pf = FakeScreens::new(true);
    *pf.fail_add.lock() = true;
    let session = FakeSession::nested(SESSION);
    let (screens, _runtime) = ladder(&pf, &session);
    screens.probe_capability().await;
    let comp =
        FakeCompositor::with(&[(0, (800, 0, 1920, 992, 1.0)), (1, (800, 0, 1920, 992, 1.0))]);
    let mut layouts = Layouts::new();
    layouts.insert("primary".into(), Layout::new(800, 0, 1920, 992));
    layouts.insert("display2".into(), Layout::new(0, 0, 800, 600));
    let outcome = layout::after_primary(
        &outputs_over(&comp),
        Some(&screens),
        &mut layouts,
        &HashMap::new(),
    )
    .await;
    assert_eq!(
        outcome.dropped[0].reason,
        "The session compositor cannot add a screen for this display."
    );
    assert!(!comp.calls().iter().any(|c| c.starts_with("create")));
    assert_eq!(
        comp.calls(),
        ["move 0 0 0"],
        "the arrangement was built around it"
    );
    assert_eq!((layouts["primary"].x, layouts.len()), (0, 1));

    let comp = FakeCompositor::with(&[(0, (0, 0, 1920, 992, 1.0)), (1, (0, 0, 1920, 992, 1.0))]);
    *comp.refuse_create.lock() = true;
    let mut layouts = Layouts::new();
    layouts.insert("display2".into(), Layout::new(1920, 0, 800, 600));
    let outcome =
        layout::after_primary(&outputs_over(&comp), None, &mut layouts, &HashMap::new()).await;
    assert_eq!(
        outcome.dropped[0].reason,
        "The compositor cannot create an output for this display."
    );

    let comp = FakeCompositor::with(&[(0, (0, 0, 1920, 992, 1.0)), (1, (0, 0, 1920, 992, 1.0))]);
    let mut layouts = Layouts::new();
    layouts.insert("display3".into(), Layout::new(1920, 0, 800, 600));
    layouts.insert("other3".into(), Layout::new(2720, 0, 800, 600));
    let outcome =
        layout::after_primary(&outputs_over(&comp), None, &mut layouts, &HashMap::new()).await;
    assert_eq!(
        outcome.dropped[0].reason,
        "This display's name collides with another display's output."
    );
    assert_eq!(layouts.len(), 1);

    // WebRTC's single placement says the same.
    let pf = FakeScreens::new(true);
    *pf.fail_add.lock() = true;
    let (screens, _runtime) = ladder(&pf, &session);
    screens.probe_capability().await;
    let comp = FakeCompositor::with(&[(0, (0, 0, 1920, 992, 1.0)), (1, (0, 0, 1920, 992, 1.0))]);
    let mut layouts = Layouts::new();
    layouts.insert("primary".into(), Layout::new(0, 0, 1920, 992));
    layouts.insert("display2".into(), Layout::new(1920, 0, 800, 600));
    let refusal = layout::apply_extension(
        &outputs_over(&comp),
        Some(&screens),
        "display2",
        &layouts,
        "96",
        &FakePrimary::default(),
    )
    .await;
    assert_eq!(
        refusal,
        Err("The session compositor cannot add a screen for this display.".to_string())
    );
}

#[tokio::test]
async fn a_secondary_dropping_into_a_shrinking_primarys_room_waits_for_its_capture() {
    let comp = FakeCompositor::with(&[(0, (0, 0, 1920, 1080, 1.5)), (1, (0, 0, 1920, 1080, 1.5))]);
    let mut layouts = Layouts::new();
    layouts.insert("primary".into(), Layout::new(0, 0, 1000, 1080));
    layouts.insert("display2".into(), Layout::new(1000, 0, 800, 600));
    let primary = FakePrimary::default();
    let placed = layout::apply_extension(
        &outputs_over(&comp),
        None,
        "display2",
        &layouts,
        "144",
        &primary,
    )
    .await;
    assert_eq!(placed, Ok(()));
    assert_eq!(
        *primary.fits.lock(),
        [(0, 0, 1000, 1080)],
        "the capture takes the rectangle first"
    );
    assert_eq!(
        comp.calls(),
        ["resize 0 1000 1080", "create 2 800 600 1000 0 1.5"],
        "then the screen gives the room up, before the secondary is placed"
    );
}

#[tokio::test]
async fn a_teardown_retires_every_secondary_and_its_screen() {
    let pf = FakeScreens::new(true);
    let session = FakeSession::nested(SESSION);
    let (screens, _runtime) = ladder(&pf, &session);
    screens.probe_capability().await;
    screens.ensure_screen("display2", None, 1.0).await;
    let comp = FakeCompositor::with(&[
        (0, (0, 0, 1920, 992, 1.0)),
        (1, (0, 0, 1920, 992, 1.0)),
        (2, (1920, 0, 800, 600, 1.0)),
    ]);
    pf.take();
    layout::retire_outputs(&outputs_over(&comp), Some(&screens), None).await;
    assert_eq!(
        comp.calls(),
        ["destroy 2"],
        "the primary's screen and its view persist"
    );
    assert!(pf.asked("remove SELKIES-2"));
}

// --- The start verdict (test_wayland_start_verdict.py) -------------------------------------------

fn capture(
    geometry: crate::ws::testkit::RealizedGeometry,
    state: &str,
    last_error: Option<&str>,
    capturing: bool,
) -> Arc<dyn CaptureHandle> {
    let capture = FakeCapture::default();
    *capture.geometry.lock() = geometry;
    *capture.state.lock() = (state.to_string(), last_error.map(str::to_string));
    *capture.capturing.lock() = capturing;
    Arc::new(capture)
}

#[tokio::test]
async fn a_wayland_start_reads_back_what_truly_happened() {
    use super::verdict::{last_error, start_verdict};
    let clean = capture(Ok(Some((800, 480, 1.0))), "running", None, true);
    assert_eq!(
        start_verdict(&clean, "primary").await,
        (true, None),
        "a clean start is live with no error"
    );
    let missing = capture(
        Ok(Some((0, 0, 0.0))),
        "failed",
        Some("no output with display id 3"),
        false,
    );
    assert_eq!(
        start_verdict(&missing, "display3").await,
        (false, Some("no output with display id 3".to_string())),
        "a missing display id reads as a failed start"
    );
    let dead = capture(
        Ok(None),
        "failed",
        Some("host compositor connection lost; capture stopped"),
        false,
    );
    let (live, err) = start_verdict(&dead, "primary").await;
    assert!(
        !live && err.is_some_and(|e| e.contains("host compositor connection lost")),
        "a dead host reads as a failed start with its reason"
    );
    let degraded = capture(
        Ok(Some((800, 480, 1.0))),
        "running",
        Some("NVENC init failed (x); using CPU encode"),
        true,
    );
    assert_eq!(
        start_verdict(&degraded, "primary").await,
        (
            true,
            Some("NVENC init failed (x); using CPU encode".to_string())
        ),
        "a CPU fallback is live with a caveat"
    );
    let timeout = capture(Ok(None), "running", None, true);
    assert_eq!(
        start_verdict(&timeout, "primary").await,
        (true, None),
        "a geometry timeout still reads liveness"
    );
    let barrier = capture(Err("gone".into()), "running", None, true);
    assert_eq!(
        start_verdict(&barrier, "primary").await,
        (true, None),
        "and so does a failed barrier"
    );
    let idle = capture(Ok(Some((0, 0, 0.0))), "idle", None, false);
    assert_eq!(
        start_verdict(&idle, "primary").await,
        (false, None),
        "no live pipeline and no error still fails"
    );
    let exhausted = capture(
        Ok(Some((0, 0, 0.0))),
        "failed",
        Some("encoder session exhausted"),
        false,
    );
    assert_eq!(
        last_error(Some(&exhausted), "display2").as_deref(),
        Some("encoder session exhausted"),
        "last-error reads the recorded reason"
    );
    assert_eq!(
        last_error(None, "primary"),
        None,
        "and tolerates a missing capture"
    );
}

// --- The apps runner (test_apps_local_repo.py, the runner's side) --------------------------------

/// A runner in `dir` whose `check` exits `check` and whose `list` prints the file `listed`, or
/// fails while `list-fails` exists, recording the repository it was given.
fn runner_script(dir: &Scratch, check: i32) -> PathBuf {
    let path = dir.path(APP_RUNNER);
    let root = dir.text();
    script(
        &path,
        &format!(
            r#"echo "$1 ${{PA_REPO_FOLDER:-none}}" >> "{root}/asked"
case "$1" in
check) echo "denied: ptrace"; echo "  second line" ; exit {check} ;;
list) [ -e "{root}/list-fails" ] && exit 1; cat "{root}/listed" ;;
esac"#
        ),
    );
    path
}

#[tokio::test]
async fn the_panel_shows_only_what_the_runner_can_do() {
    settings_installed();
    let absent = AppsRunner::named("selkies-no-such-runner");
    assert!(!absent.available(), "no wrapper, no panel");
    absent.probe().await;
    assert_eq!(absent.installed(), None);

    let dir = Scratch::new("runner");
    std::fs::write(dir.path("listed"), "firefox\n\n  gimp \n").expect("listed");
    let runner = AppsRunner::named(runner_script(&dir, 0));
    assert!(
        runner.available(),
        "assumed good until the probe has spoken"
    );
    assert_eq!(
        runner.installed(),
        None,
        "None until a read succeeds, never published as empty"
    );
    runner.probe().await;
    assert!(runner.available());
    assert_eq!(
        runner.installed(),
        Some(vec!["firefox".to_string(), "gimp".to_string()]),
        "a usable runner is asked what it has"
    );

    let dir = Scratch::new("runner");
    let denied = AppsRunner::named(runner_script(&dir, 3));
    denied.probe().await;
    assert!(
        !denied.available(),
        "a host the wrapper refuses drops the panel"
    );
    assert_eq!(denied.installed(), None, "and is not asked what it has");
    let asked = std::fs::read_to_string(dir.path("asked")).expect("asked");
    assert_eq!(asked.lines().count(), 1, "{asked}");
}

#[tokio::test]
async fn a_failed_read_leaves_the_last_answer_standing() {
    settings_installed();
    let dir = Scratch::new("runner");
    let runner = AppsRunner::named(runner_script(&dir, 0));
    std::fs::write(dir.path("listed"), "firefox\n").expect("listed");
    assert!(runner.refresh().await, "the first read changes the set");
    assert!(!runner.refresh().await, "the same answer does not");
    std::fs::write(dir.path("list-fails"), b"").expect("failing");
    std::fs::write(dir.path("listed"), "").expect("listed");
    assert!(!runner.refresh().await);
    assert_eq!(runner.installed(), Some(vec!["firefox".to_string()]));
    std::fs::remove_file(dir.path("list-fails")).expect("recovered");
    assert!(runner.refresh().await, "nothing installed is an answer too");
    assert_eq!(runner.installed(), Some(Vec::new()));
}

#[tokio::test]
async fn a_finished_install_is_announced_where_the_set_changed() {
    settings_installed();
    let dir = Scratch::new("runner");
    let runner = AppsRunner::named(runner_script(&dir, 0));
    std::fs::write(dir.path("listed"), "firefox\n").expect("listed");
    assert_eq!(
        runner
            .note_command_finished("selkies-proot install firefox")
            .await
            .as_deref(),
        Some(r#"apps_installed,["firefox"]"#)
    );
    assert_eq!(
        runner
            .note_command_finished("/usr/bin/selkies-proot update firefox")
            .await,
        None,
        "nothing changed, nothing to say"
    );
    std::fs::write(dir.path("listed"), "").expect("listed");
    assert_eq!(
        runner
            .note_command_finished("selkies-proot run firefox")
            .await,
        None,
        "a launch changes nothing and reads nothing"
    );
    assert_eq!(runner.note_command_finished("rm -rf install").await, None);
    assert_eq!(
        runner
            .note_command_finished("selkies-proot remove firefox")
            .await
            .as_deref(),
        Some("apps_installed,[]")
    );
}

#[test]
fn which_commands_change_the_installed_set() {
    for yes in [
        "selkies-proot install a",
        "selkies-proot remove a",
        "/opt/bin/selkies-proot update a",
    ] {
        assert!(changes_installed_set(yes), "{yes}");
    }
    for no in [
        "",
        "selkies-proot",
        "selkies-proot run a",
        "proot-apps install a",
        "selkies-proot-x install a",
    ] {
        assert!(!changes_installed_set(no), "{no}");
    }
}

#[test]
fn the_runner_reads_the_repository_the_panel_lists() {
    assert_eq!(
        runner_env("/mnt/apps")
            .get("PA_REPO_FOLDER")
            .map(String::as_str),
        Some("/mnt/apps")
    );
    assert!(
        !runner_env("").contains_key("PA_REPO_FOLDER"),
        "an empty setting withdraws it, so the two never read two repositories"
    );
}

#[test]
fn the_repository_reaches_the_server_as_one_absolute_path() {
    let parse = |vars: &[(&str, &str)]| {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Settings::parse(&[], &move |name: &str| vars.get(name).cloned())
    };
    let fallback = parse(&[("PA_REPO_FOLDER", "~/repo/")]);
    assert_eq!(
        fallback.apps_repo_folder(),
        "~/repo/",
        "the variable proot-apps reads is the setting's fallback"
    );
    let own = parse(&[
        ("PA_REPO_FOLDER", "~/repo/"),
        ("SELKIES_APPS_REPO_FOLDER", "/mnt/apps"),
    ]);
    assert_eq!(
        own.apps_repo_folder(),
        "/mnt/apps",
        "the setting's own variable wins over the fallback"
    );
    let home = std::env::var("HOME").unwrap_or_default();
    assert_eq!(
        parse(&[("SELKIES_APPS_REPO_FOLDER", " ~/repo/ ")]).apps_repo_path(),
        format!("{home}/repo"),
        "the folder is read as one absolute path"
    );
    assert_eq!(
        parse(&[]).apps_repo_path(),
        "",
        "no folder means the remote repository"
    );
}
