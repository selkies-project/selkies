/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A host without one of the libraries the server opens at run time still runs it.
//!
//! The server opens its GPU stack and a few desktop libraries with `dlopen` when a path needs
//! them: EGL and GL, CUDA, NVENC, NvFBC, NVML, VA-API, Vulkan, X11 and XCB (NvFBC and the GPU
//! probe), libwayland-server (the Wayland backend) and PipeWire. Where one is missing, the path
//! that wanted it declines and the session goes on without it. A loader that panics instead
//! takes the whole server down on such a host, and a machine that has every library never shows
//! it.
//!
//! `tests/tools/hide_libs.so` makes `dlopen` fail for the libraries named, so this host runs the
//! paths of one without them. With each hidden, and with all of them at once: `selkies gpu-probe`
//! answers and exits 0, and an X11 and a Wayland session over WebSockets start, send frames and
//! stay up, with no panic in the server's log. Without libwayland-server there is no Wayland
//! backend at all: that session's server says so and stays up. The same runs with nothing hidden
//! are the control that the shim alone changes nothing. The GPU paths run only where there is a
//! render node, so without one the sessions run with nothing and with everything hidden alone.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use selkies_suites::server::run_within;
use selkies_suites::{Checks, Client, private_dir, repo, server, server_binary};
use serde_json::json;

/// What each name hides: file name prefixes, as `hide_libs.c` matches them.
const LIBRARIES: [(&str, &str); 11] = [
    ("EGL", "libEGL"),
    ("GL", "libGL:libOpenGL"),
    ("CUDA", "libcuda"),
    ("NVENC", "libnvidia-encode"),
    ("NvFBC", "libnvidia-fbc"),
    ("NVML", "libnvidia-ml"),
    ("VA-API", "libva"),
    ("Vulkan", "libvulkan"),
    ("X11 and XCB", "libX11:libxcb"),
    ("libwayland-server", "libwayland-server"),
    ("PipeWire", "libpipewire"),
];
/// What the Wayland compositor's thread logs where it cannot start.
const NO_COMPOSITOR: &str = "compositor thread aborting";
/// How long a session is given to send its first frame.
const FRAME_WAIT: Duration = Duration::from_secs(20);
/// How long the server is watched after its first frame, for a panic on a later path.
const SETTLE: Duration = Duration::from_secs(3);

/// The shim's path, built once: the tests of this target start together.
fn shim() -> &'static Path {
    static SHIM: OnceLock<PathBuf> = OnceLock::new();
    SHIM.get_or_init(|| {
        let tools = repo().join("tests/tools");
        let made = run_within(
            Command::new("make")
                .args(["-s", "-C"])
                .arg(&tools)
                .arg("hide_libs.so"),
            Duration::from_secs(120),
        );
        assert!(
            matches!(made, Ok(ref ran) if ran.status.success()),
            "hide_libs.so does not build here: {}",
            made.map_or_else(|why| why, |ran| ran.stderr)
        );
        tools.join("hide_libs.so")
    })
}

/// The sets of libraries to hide, by name: nothing, each library, and all of them.
fn sets() -> Vec<(&'static str, String)> {
    let every: Vec<&str> = LIBRARIES.iter().map(|(_, prefixes)| *prefixes).collect();
    std::iter::once(("nothing", String::new()))
        .chain(
            LIBRARIES
                .iter()
                .map(|(name, prefixes)| (*name, (*prefixes).to_owned())),
        )
        .chain(std::iter::once(("every library", every.join(":"))))
        .collect()
}

/// Where the panic message starts in `text`, shortened; `otherwise` when it holds none.
fn panic_in(text: &str, otherwise: impl FnOnce() -> String) -> String {
    text.find("panicked")
        .map_or_else(otherwise, |at| text[at..].chars().take(240).collect())
}

/// A server built against a conda environment's libgbm finds the host's GBM backends only where
/// it is pointed at them.
fn gbm_backends() -> Option<String> {
    std::env::var("GBM_BACKENDS_PATH")
        .ok()
        .filter(|v| !v.is_empty())
}

#[tokio::test]
#[ignore = "needs a built selkies server, the X test display (E2E_DISPLAY), make, and a C compiler"]
async fn the_gpu_probe_answers_with_any_library_hidden() {
    let shim = shim();
    let mut checks = Checks::new("missing-libraries");
    for (name, hidden) in sets() {
        let mut probe = Command::new(server_binary());
        probe
            .arg("gpu-probe")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", private_dir("home"))
            .env("XDG_RUNTIME_DIR", private_dir("run"))
            .env("LD_PRELOAD", shim)
            .env("HIDE_LIBS", &hidden);
        for name in ["CUDA_VISIBLE_DEVICES", "CUDA_DEVICE_ORDER", "E2E_DISPLAY"] {
            if let Some(value) = std::env::var_os(name) {
                probe.env(
                    if name == "E2E_DISPLAY" {
                        "DISPLAY"
                    } else {
                        name
                    },
                    value,
                );
            }
        }
        if let Some(backends) = gbm_backends() {
            probe.env("GBM_BACKENDS_PATH", backends);
        }
        let (ok, detail) = match run_within(&mut probe, Duration::from_secs(180)) {
            Ok(ran) => {
                let text = format!("{}{}", ran.stdout, ran.stderr);
                (
                    ran.status.success()
                        && !text.contains("panicked")
                        && ran.stdout.contains("SELKIES_GPU_PRESENT="),
                    format!(
                        "{}: {}",
                        ran.status,
                        panic_in(&text, || selkies_suites::tail(text.trim(), 3))
                    ),
                )
            }
            Err(why) => (false, why),
        };
        checks.check(
            &format!("{name} hidden: gpu-probe answers without a panic"),
            ok,
            detail,
        );
    }
    checks.finish("");
}

/// Start a session of `wayland` or X11 with each set hidden and hold it to sending frames and
/// staying up.
async fn sessions_run(shim: &Path, wayland: bool) {
    let backend = if wayland { "Wayland" } else { "X11" };
    let mut checks = Checks::new("missing-libraries");
    let gpu = std::fs::read_dir("/dev/dri").is_ok_and(|nodes| {
        nodes
            .flatten()
            .any(|node| node.file_name().to_string_lossy().starts_with("renderD"))
    });
    let mut logs = String::new();
    for (name, hidden) in sets() {
        if !gpu && name != "nothing" && name != "every library" {
            continue;
        }
        let no_backend = wayland && hidden.split(':').any(|lib| lib == "libwayland-server");
        let mut launch = server::launch(if wayland { "wayland" } else { "x11" })
            .wayland(wayland)
            .env("LD_PRELOAD", &shim.display().to_string())
            .env("HIDE_LIBS", &hidden);
        if let Some(backends) = gbm_backends() {
            launch = launch.env("GBM_BACKENDS_PATH", &backends);
        }
        let server = launch.spawn().await;
        if let Err(why) = server.came_up().await {
            let log = server.log();
            checks.check(
                &format!("{name} hidden: a {backend} server starts"),
                false,
                panic_in(&log, || why),
            );
            logs.push_str(&server.stop().await);
            continue;
        }
        let page = Client::connect(&server, "").await;
        page.send(&format!(
            "SETTINGS,{}",
            json!({
                "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
                "manual_resolution": false, "framerate": 30, "encoder": "h264enc",
                "video_crf": 25, "video_bitrate": 4000, "audio_bitrate": 128000,
                "scaling_dpi": 96, "displayPosition": "right",
            })
        ));
        // Read on past the first frame, so a panic on the path after it has its moment.
        let mut end = Instant::now() + FRAME_WAIT;
        let mut first = None;
        while Instant::now() < end {
            if first.is_none() && page.newest_frame().is_some() {
                first = Some(Instant::now());
                end = end.min(Instant::now() + SETTLE);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let sent = page
            .binaries(0)
            .iter()
            .filter(|binary| binary.video_frame_id().is_some())
            .count();
        let log = server.log();
        if no_backend {
            checks.check(
                &format!("{name} hidden: the {backend} server says it has no compositor"),
                log.contains(NO_COMPOSITOR),
                sent,
            );
        } else {
            checks.check(
                &format!("{name} hidden: a {backend} session sends frames"),
                sent > 0,
                sent,
            );
        }
        checks.check(
            &format!("{name} hidden: the {backend} server is up and logged no panic"),
            server.exited().is_none() && !log.contains("panicked"),
            panic_in(&log, || format!("exit {:?}", server.exited())),
        );
        logs.push_str(&server.stop().await);
    }
    checks.finish(&logs);
}

#[tokio::test]
#[ignore = "needs a built selkies server, the X test display (E2E_DISPLAY), make, and a C compiler"]
async fn an_x11_session_runs_with_any_library_hidden() {
    sessions_run(shim(), false).await;
}

#[tokio::test]
#[ignore = "needs a built selkies server with the Wayland backend, make, and a C compiler"]
async fn a_wayland_session_runs_with_any_library_hidden() {
    sessions_run(shim(), true).await;
}
