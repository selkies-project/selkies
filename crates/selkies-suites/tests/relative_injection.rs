/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What the remote pointer does with a relative motion message.
//!
//! The client sends locked and trackpad motion as deltas, and both backends inject them
//! verbatim: no acceleration curve of their own, no loss when a movement arrives split into many
//! small deltas rather than one large one. That is what makes the client's own scaling the only
//! thing standing between a hand's movement and the remote pointer's, and it has to hold
//! identically on X11 (XTEST relative motion) and on Wayland (the compositor's relative pointer),
//! or the same gesture lands differently on the two backends.
//!
//! The seam back to absolute mode is pinned too: a drag held against the screen edge stops at the
//! edge and unwinds from there rather than from where the deltas would have carried it, and the
//! absolute move after it lands where it says, whatever the tracked position made of the drag.
//! A game asks for more than the deltas: it locks the pointer, and a lock the compositor confirms
//! but does not honor leaves the pointer moving under the holder, which reads every move a second
//! time off the absolute motion it is still sent. The Wayland lock block pins both halves of that.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use selkies_suites::displays::Desktop;
use selkies_suites::server::{Server, test_display};
use selkies_suites::wayland::WlObserver;
use selkies_suites::{Checks, Client, private_dir, server};

const WL_SOCKET: &str = "wayland-1";
const START: (i64, i64) = (600, 400);
const TRAVEL: i64 = 120;

/// Open one page, send each message in order, and let them land before it closes.
async fn send_all(server: &Server, messages: &[String]) {
    let page = Client::connect(server, "").await;
    let settings = json!({
        "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
        "manual_resolution": false, "framerate": 60, "encoder": "h264enc",
        "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128_000,
        "scaling_dpi": 96, "displayPosition": "right",
    });
    page.send(&format!("SETTINGS,{settings}"));
    tokio::time::sleep(Duration::from_secs(2)).await;
    for message in messages {
        page.send(message);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(350)).await;
}

fn start_move() -> String {
    format!("m,{},{},0,0", START.0, START.1)
}

fn repeated(message: &str, times: i64) -> Vec<String> {
    (0..times).map(|_| message.to_owned()).collect()
}

fn with_start(rest: Vec<String>) -> Vec<String> {
    let mut all = vec![start_move()];
    all.extend(rest);
    all
}

fn drag() -> Vec<String> {
    ["m,600,400,0,0", "m2,-700,0,0,0", "m2,300,0,0,0"]
        .map(str::to_owned)
        .to_vec()
}

/// The surface-local x of each pointer arrival and motion the observer saw.
fn pointer_xs(lines: &[Value]) -> Vec<f64> {
    lines
        .iter()
        .filter(|l| matches!(l["kind"].as_str(), Some("ptr_enter" | "ptr_motion")))
        .filter_map(|l| l["x"].as_f64())
        .collect()
}

async fn x11_block(checks: &mut Checks) -> String {
    let server = server::launch("relative-injection-x11").start().await;
    let desktop = Desktop::open(&test_display());
    let x = |desktop: &Desktop| desktop.pointer().map_or(i64::MIN, |(x, _)| x);
    let travel = async |messages: Vec<String>| {
        send_all(&server, &with_start(messages)).await;
        x(&desktop) - START.0
    };
    let one = travel(vec![format!("m2,{TRAVEL},0,0,0")]).await;
    checks.check(
        "x11: one delta moves the pointer by exactly that much",
        one == TRAVEL,
        one,
    );
    let many = travel(repeated("m2,1,0,0,0", TRAVEL)).await;
    checks.check(
        "x11: the same movement split into single pixels travels as far",
        many == TRAVEL,
        many,
    );
    let back = travel(vec![
        format!("m2,{TRAVEL},0,0,0"),
        format!("m2,{},0,0,0", -TRAVEL),
    ])
    .await;
    checks.check(
        "x11: there and back leaves the pointer where it started",
        back == 0,
        back,
    );
    send_all(&server, &drag()).await;
    let unwound = x(&desktop);
    checks.check(
        "x11: a drag pinned against the edge unwinds from the edge",
        unwound == 300,
        unwound,
    );
    let mut then_absolute = drag();
    then_absolute.push("m,200,400,0,0".into());
    send_all(&server, &then_absolute).await;
    let pinned = x(&desktop);
    checks.check(
        "x11: an absolute move after the drag lands where it says",
        pinned == 200,
        pinned,
    );
    server.stop().await
}

async fn wayland_block(checks: &mut Checks, bin: &Path) -> String {
    let server = server::launch("relative-injection-wayland")
        .wayland(true)
        .start()
        .await;
    let obs = WlObserver::start(
        bin,
        WL_SOCKET,
        &private_dir("run"),
        &[("WLOBS_DURATION", "60")],
    );
    if !obs.ready(Duration::from_secs(20)).await {
        checks.skip("wayland: the observer surface maps", "no mapped event");
        drop(obs);
        return server.stop().await;
    }
    let travel = async |messages: Vec<String>| {
        obs.clear();
        send_all(&server, &with_start(messages)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        // The pointer's first arrival on the surface is an enter, not a motion, and it carries
        // the position the opening absolute message put it at: the origin to measure from.
        let seen = pointer_xs(&obs.lines());
        match (seen.first(), seen.last()) {
            (Some(first), Some(last)) if seen.len() >= 2 => last - first,
            _ => f64::NAN,
        }
    };
    let one = travel(vec![format!("m2,{TRAVEL},0,0,0")]).await;
    checks.check(
        "wayland: one delta moves the pointer by exactly that much",
        one == TRAVEL as f64,
        one,
    );
    let many = travel(repeated("m2,1,0,0,0", TRAVEL)).await;
    checks.check(
        "wayland: the same movement split into single pixels travels as far",
        many == TRAVEL as f64,
        many,
    );
    let back = travel(vec![
        format!("m2,{TRAVEL},0,0,0"),
        format!("m2,{},0,0,0", -TRAVEL),
    ])
    .await;
    checks.check(
        "wayland: there and back leaves the pointer where it started",
        back == 0.0,
        back,
    );
    let last_seen = |obs: &WlObserver| pointer_xs(&obs.lines()).last().copied().unwrap_or(f64::NAN);
    obs.clear();
    send_all(&server, &drag()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let unwound = last_seen(&obs);
    checks.check(
        "wayland: a drag pinned against the edge unwinds from the edge",
        unwound == 300.0,
        unwound,
    );
    obs.clear();
    let mut then_absolute = drag();
    then_absolute.push("m,200,400,0,0".into());
    send_all(&server, &then_absolute).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let pinned = last_seen(&obs);
    checks.check(
        "wayland: an absolute move after the drag lands where it says",
        pinned == 200.0,
        pinned,
    );
    drop(obs);
    server.stop().await
}

/// What a client holding a pointer lock is told: every delta, and no move.
async fn lock_block(checks: &mut Checks, bin: &Path) -> String {
    let server = server::launch("relative-injection-lock")
        .wayland(true)
        .start()
        .await;
    let probe = WlObserver::start(
        bin,
        WL_SOCKET,
        &private_dir("run"),
        &[("WL_LOCK_DURATION", "60"), ("WLOBS_DURATION", "60")],
    );
    let tail = |probe: &WlObserver, n: usize| {
        let lines = probe.lines();
        let shown: Vec<String> = lines[lines.len().saturating_sub(n)..]
            .iter()
            .map(Value::to_string)
            .collect();
        format!("[{}]", shown.join(", "))
    };
    if !probe.ready(Duration::from_secs(20)).await {
        checks.skip(
            "wayland: a client's pointer lock is confirmed",
            format!("the lock probe never mapped: {}", tail(&probe, 3)),
        );
        drop(probe);
        return server.stop().await;
    }
    // A lock holds only a surface the pointer is already inside.
    send_all(&server, &[start_move()]).await;
    let locked = probe.wait_for("locked", Duration::from_secs(8)).await;
    checks.check(
        "wayland: a client's pointer lock is confirmed",
        locked.is_some(),
        tail(&probe, 4),
    );
    if locked.is_some() {
        let mark = probe.count();
        let deltas: [(i64, i64); 3] = [(7, 3), (-4, 11), (60, -25)];
        let sent: Vec<String> = deltas
            .iter()
            .map(|(dx, dy)| format!("m2,{dx},{dy},0,0"))
            .collect();
        send_all(&server, &sent).await;
        let after = probe.lines_from(mark);
        let carried: Vec<(i64, i64)> = after
            .iter()
            .filter(|l| l["kind"] == "relative")
            .map(|l| {
                (
                    l["dx"].as_f64().unwrap_or(f64::NAN) as i64,
                    l["dy"].as_f64().unwrap_or(f64::NAN) as i64,
                )
            })
            .collect();
        checks.check(
            "wayland: the lock's holder is given every delta, once",
            carried == deltas,
            format!("{carried:?}"),
        );
        let moved: Vec<&Value> = after.iter().filter(|l| l["kind"] == "motion").collect();
        checks.check(
            "wayland: and is not moved as well, so it reads no delta twice",
            moved.is_empty(),
            format!("{:?}", &moved[..moved.len().min(4)]),
        );
    }
    drop(probe);
    server.stop().await
}

#[tokio::test]
#[ignore = "needs a built selkies server, the X test display (E2E_DISPLAY) and the Wayland backend"]
async fn relative_motion_lands_verbatim_on_both_backends() {
    let mut checks = Checks::new("relative-injection");
    let mut logs = x11_block(&mut checks).await;
    logs.push_str(&wayland_block(&mut checks, Path::new(env!("CARGO_BIN_EXE_wl-observer"))).await);
    logs.push_str(&lock_block(&mut checks, Path::new(env!("CARGO_BIN_EXE_wl-lock-probe"))).await);
    checks.finish(&logs);
}
