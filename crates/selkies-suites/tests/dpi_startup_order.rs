/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The X11 desktop's density is settled before the server takes a page.
//!
//! A home that outlives a restart brings the desktop up at the density its last page gave it,
//! and settling it merges resources and stamps the root, tens of milliseconds into startup. A
//! page connecting the moment the port opens, which a browser retrying after a restart does,
//! meets the settled density: its first SETTINGS compares against what the desktop has, so a 1x
//! page brings a desktop left at 192 back to 96 instead of reading as no change. Both transports
//! settle it before they listen.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use selkies_suites::server::output_within;
use selkies_suites::xvfb::Xvfb;
use selkies_suites::{Checks, Client, Server, server, workdir};
use serde_json::json;

const RESTORED: &str = "for DPI 192.";
const LISTENING: &str = "Selkies server running on";
const PROCESSED: &str = "settings applied for 'primary'";
const APPLIED: &str = "DPI changed from 192 to 96";

/// `xrdb` with `args` against `display`.
fn xrdb(display: &str, args: &[&str]) -> Option<String> {
    output_within(
        Command::new("xrdb").args(args).env("DISPLAY", display),
        Duration::from_secs(10),
    )
}

/// `Xft.dpi` in the display's resource database, `None` when unset.
fn xft_dpi(display: &str) -> Option<u32> {
    xrdb(display, &["-query"])?
        .lines()
        .find_map(|line| line.strip_prefix("Xft.dpi:"))
        .and_then(|value| value.trim().parse().ok())
}

/// The state a restart finds: a home with the persisted resources at 192, merged into the X
/// server as an image does at session start.
fn desktop_left_at_192(home: &Path, display: &str) {
    let _ = std::fs::remove_dir_all(home);
    std::fs::create_dir_all(home).expect("the home");
    let resources = home.join(".Xresources");
    std::fs::write(&resources, "Xft.dpi:   192\n").expect("the X resources");
    std::fs::write(
        home.join(".xsettingsd"),
        format!("Xft/DPI {}\n", 192 * 1024),
    )
    .expect("the xsettingsd file");
    xrdb(display, &["-merge", &resources.display().to_string()])
        .expect("xrdb merges the persisted resources");
}

/// A private display left at 192 and a server of `mode` on it, started and not yet waited for.
async fn restarted(mode: &str) -> (Xvfb, Server) {
    let display = Xvfb::start(1280, 720, &[]);
    let home = workdir().join(format!("dpi-startup-home-{mode}"));
    desktop_left_at_192(&home, display.display());
    let server = server::launch(mode)
        .mode(mode)
        .display(display.display())
        .env("HOME", &home.display().to_string())
        .env("SELKIES_DEBUG", "true")
        .spawn()
        .await;
    (display, server)
}

/// Whether the density left in the home was restored before the server listened.
fn restored_before_listening(server: &Server) -> (bool, String) {
    let log = server.log();
    let ordered = matches!(
        (log.find(RESTORED), log.find(LISTENING)),
        (Some(restored), Some(listening)) if restored < listening
    );
    let said: Vec<&str> = log
        .lines()
        .filter(|line| line.contains(RESTORED) || line.contains(LISTENING))
        .collect();
    (ordered, said.join(" "))
}

/// Connect the moment the port accepts, send a 1x page's first SETTINGS, and hold the page open
/// until they are in, as a browser would; how long after the accept the SETTINGS went out, and
/// the page.
async fn first_page(server: &Server) -> Option<(Duration, Client)> {
    let deadline = Instant::now() + Duration::from_secs(60);
    while std::net::TcpStream::connect(("127.0.0.1", server.port())).is_err() {
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let accepted = Instant::now();
    let page = Client::connect(server, "").await;
    page.send(&format!(
        "SETTINGS,{}",
        json!({
            "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
            "manual_resolution": false, "framerate": 60, "encoder": "h264enc", "scaling_dpi": 96,
        })
    ));
    let sent = accepted.elapsed();
    server.settle(0, PROCESSED, Duration::from_secs(30)).await;
    Some((sent, page))
}

#[tokio::test]
#[ignore = "needs a built selkies server, Xvfb, and xrdb"]
async fn a_websockets_page_connecting_as_the_port_opens_meets_the_density_the_home_left() {
    let (display, server) = restarted("websockets").await;
    let mut checks = Checks::new("dpi-startup-order");
    let ((), page) = tokio::join!(server.ready(), first_page(&server));
    let (ordered, said) = restored_before_listening(&server);
    checks.check(
        "websockets: the density left in the home is restored before the server listens",
        ordered,
        said,
    );
    checks.check(
        "websockets: a page connecting as the port opens has its first SETTINGS processed",
        page.is_some() && server.log().contains(PROCESSED),
        page.as_ref().map_or_else(
            || "never".to_owned(),
            |(sent, _)| format!("{} ms after the accept", sent.as_millis()),
        ),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while xft_dpi(display.display()) != Some(96) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let dpi = xft_dpi(display.display());
    checks.check(
        "websockets: its 1x SETTINGS brings a desktop left at 192 back to 96",
        dpi == Some(96),
        format!("{dpi:?}"),
    );
    checks.check(
        "websockets: read as a change from 192",
        server.log().contains(APPLIED),
        APPLIED,
    );
    checks.finish(&server.stop().await);
}

#[tokio::test]
#[ignore = "needs a built selkies server, Xvfb, and xrdb"]
async fn the_webrtc_server_restores_the_density_the_home_left_before_it_listens() {
    let (display, server) = restarted("webrtc").await;
    let mut checks = Checks::new("dpi-startup-order");
    server.ready().await;
    let (ordered, said) = restored_before_listening(&server);
    checks.check(
        "webrtc: the density left in the home is restored before the server listens",
        ordered,
        said,
    );
    let dpi = xft_dpi(display.display());
    checks.check(
        "webrtc: the desktop keeps the density it was left at",
        dpi == Some(192),
        format!("{dpi:?}"),
    );
    checks.finish(&server.stop().await);
}
