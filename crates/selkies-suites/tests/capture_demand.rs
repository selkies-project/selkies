/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The demand loop end to end below the browser: does the session ask a page to capture only
//! while something in the desktop is using the device?
//!
//! A real server, a real WebSocket client, a real process opening the virtual camera through the
//! interposer, and a real recorder on the virtual microphone. What this catches that the unit
//! suites cannot: the watcher polls sinks the fakes stand in for, so a sink that stops reporting
//! a reader, a hold-off that never elapses, or a demand published to nobody leaves the page
//! holding the user's camera with the light on and nothing asking for it back.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use selkies_suites::{Checks, Client, on_path, private_dir, repo, server};

const VIRTUAL_MIC: &str = "SelkiesVirtualMic";
/// The camera passes a reader on at the poll that finds it and holds off before letting go, so a
/// window has to outlast the release hold-off plus the device re-check floor.
const HOLD_OFF_WINDOW: Duration = Duration::from_secs(14);
const MIC_HOLD_OFF_WINDOW: Duration = Duration::from_secs(22);

/// Every CAPTURE_DEMAND the server sends in `window`, for `subject` when given.
async fn demands(page: &Client, window: Duration, subject: &str) -> Vec<String> {
    let mark = page.texts();
    let subject = format!(" {subject} ");
    page.texts_during(mark, window, |text| {
        text.starts_with("CAPTURE_DEMAND") && (subject.trim().is_empty() || text.contains(&subject))
    })
    .await
    .into_iter()
    .map(|text| text.trim().to_owned())
    .collect()
}

fn any_ending(messages: &[String], value: char) -> bool {
    messages.iter().any(|m| m.ends_with(value))
}

/// Kills and reaps a child when it goes out of scope.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
#[ignore = "needs a built selkies server, the X test display (E2E_DISPLAY) and PulseAudio"]
async fn capture_is_asked_for_only_while_the_desktop_uses_the_device() {
    let mut checks = Checks::new("capture-demand");
    let Some(parec) = on_path("parec") else {
        checks.check(
            "parec is required by this suite and is not installed",
            false,
            "",
        );
        checks.finish("");
        return;
    };
    let addon = repo().join("addons/v4l2-interposer");
    let built = Command::new("make")
        .arg("-C")
        .arg(&addon)
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    assert!(built, "make -C {} failed", addon.display());
    let interposer = addon.join("selkies_v4l2_interposer.so");
    let index = std::env::var("SELKIES_WEBCAM_DEVICE").unwrap_or_else(|_| "0".into());
    let device = format!("/dev/video{index}");

    let server = server::launch("capture-demand")
        .env("SELKIES_WEBCAM_ENABLED", "true")
        .env("SELKIES_WEBCAM_ON_START", "demand")
        .env("SELKIES_MICROPHONE_ENABLED", "true")
        .env("SELKIES_MICROPHONE_ON_START", "demand")
        .start()
        .await;
    let page = Client::connect(&server, "").await;

    // An unread device asks for nothing at all; the checks below are what prove the server
    // answers when something does read it.
    let idle = demands(&page, Duration::from_secs(6), "").await;
    checks.check(
        "no capture is asked for while nothing uses the device",
        !any_ending(&idle, '1'),
        format!("{idle:?}"),
    );

    // /dev/video0 is the interposer's own default, not a guess about this host's loopback nodes:
    // SELKIES_WEBCAM_DEVICE picks another index and neither the server nor this suite sets it,
    // so the two agree by construction. The camera socket is in the server's runtime directory,
    // which an application of its session shares. The shell opens the device and becomes the
    // sleep that holds it, so ending that one process closes it.
    let opener = Command::new("bash")
        .arg("-c")
        .arg(format!("exec 3<{device} && exec sleep 40"))
        .env("LD_PRELOAD", &interposer)
        .env("XDG_RUNTIME_DIR", private_dir("run"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(Reaped);
    let held = demands(&page, Duration::from_secs(8), "webcam").await;
    checks.check(
        "the camera is asked for when an application opens the device",
        any_ending(&held, '1'),
        format!("{held:?}"),
    );
    drop(opener);
    let released = demands(&page, HOLD_OFF_WINDOW, "webcam").await;
    checks.check(
        "and released once the device is closed",
        any_ending(&released, '0'),
        format!("{released:?}"),
    );

    // PipeWire publishes a virtual source as `output.<name>` and PulseAudio as the bare name; ask
    // the server which one it made rather than guessing, or the two microphone checks fail for
    // the wrong reason on half the hosts.
    let listed = Command::new("pactl")
        .args(["list", "short", "sources"])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default();
    let prefixed = format!("output.{VIRTUAL_MIC}");
    let source = [prefixed.as_str(), VIRTUAL_MIC]
        .into_iter()
        .find(|name| listed.contains(name));
    if let Some(source) = source {
        let recorder = Command::new(&parec)
            .args(["-d", source, "--raw"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(Reaped);
        let held = demands(&page, Duration::from_secs(10), "microphone").await;
        checks.check(
            "the microphone is asked for when an application records",
            any_ending(&held, '1'),
            format!("{held:?}"),
        );
        drop(recorder);
        let released = demands(&page, MIC_HOLD_OFF_WINDOW, "microphone").await;
        checks.check(
            "and released once the recording stops",
            any_ending(&released, '0'),
            format!("{released:?}"),
        );
    } else {
        checks.check(
            "the virtual microphone source exists",
            false,
            listed.chars().take(160).collect::<String>(),
        );
    }
    let log = server.stop().await;
    checks.finish(&log);
}
