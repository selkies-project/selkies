/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The clipboard read-back must not hold the messages behind it.
//!
//! `cr` is the first thing a client sends on a new connection, ahead of the messages that bring
//! the session to the size and density its page asked for. An X selection whose owner answers no
//! conversion request (a browser being torn down still owns CLIPBOARD) is read to the reader's
//! own bound, tens of seconds, so the read is answered off the dispatch loop and the session
//! comes up while it is still outstanding.
//!
//! The dispatcher is the one both transports share, so the WebSockets wire covers the WebRTC
//! data channel with it.

use std::time::{Duration, Instant};

use selkies_suites::server::test_display;
use selkies_suites::x11::{SelectionOwner, root_size};
use selkies_suites::{Checks, Client, server};
use serde_json::json;

const INITIAL: (u16, u16) = (1280, 720);
const WANTED: (u16, u16) = (1600, 900);
const PAYLOAD: &[u8] = b"read-back served by an owner that answers";
/// "read-back served by an owner that answers", as a clipboard message carries it.
const PAYLOAD_BASE64: &str = "cmVhZC1iYWNrIHNlcnZlZCBieSBhbiBvd25lciB0aGF0IGFuc3dlcnM=";

/// The initial SETTINGS a primary page sends, sizing the desktop to it.
fn settings() -> String {
    format!(
        "SETTINGS,{}",
        json!({
            "displayId": "primary",
            "initialClientWidth": INITIAL.0, "initialClientHeight": INITIAL.1,
            "manual_resolution": false, "framerate": 60, "encoder": "h264enc",
            "scaling_dpi": 96,
        })
    )
}

/// Whether `size` is within the CVT cell of `wanted`.
fn near(size: (u16, u16), wanted: (u16, u16)) -> bool {
    size.0.abs_diff(wanted.0) <= 16 && size.1.abs_diff(wanted.1) <= 16
}

/// The X root's size once it is within the CVT cell of `wanted`, or the last size read when
/// `timeout` passes first.
async fn root_reaching(wanted: (u16, u16), timeout: Duration) -> (u16, u16) {
    let deadline = Instant::now() + timeout;
    loop {
        let size = root_size(&test_display()).unwrap_or_default();
        if near(size, wanted) || Instant::now() >= deadline {
            return size;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn a_resize_behind_an_unanswered_read_back_is_still_applied() {
    let server = server::launch("stalled").start().await;
    let _owner = SelectionOwner::silent(&test_display());
    let mut checks = Checks::new("clipboard-read-stall");
    let page = Client::connect(&server, "").await;
    page.send(&settings());
    let sized = root_reaching(INITIAL, Duration::from_secs(30)).await;
    checks.check(
        "the session comes up at the size its settings asked for",
        sized.0.abs_diff(INITIAL.0) <= 16,
        format!("root={sized:?}"),
    );
    page.send("cr");
    page.send(&format!("r,{}x{},primary", WANTED.0, WANTED.1));
    let realized = root_reaching(WANTED, Duration::from_secs(20)).await;
    checks.check(
        "a resize behind an unanswered read-back is still applied",
        near(realized, WANTED),
        format!("root={realized:?}"),
    );
    checks.finish(&server.stop().await);
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn an_answered_read_back_comes_back_tagged_with_the_selection() {
    // A server of its own, so the read-backs a silent owner is still holding do not queue this
    // one behind them inside the selection monitor.
    let server = server::launch("answered").start().await;
    let _owner = SelectionOwner::serving(&test_display(), PAYLOAD);
    let mut checks = Checks::new("clipboard-read-stall");
    let page = Client::connect(&server, "").await;
    page.send(&settings());
    let mark = page.texts();
    page.send("cr");
    // The selection monitor announces the owner just installed with an untagged `clipboard`
    // message of its own, so only the one that follows the tag is the answer.
    let within = Duration::from_secs(20);
    let tagged = page
        .text_at(mark, within, |text| text == "clipboard_reply,cr")
        .await;
    checks.check(
        "an answered read-back comes back tagged",
        tagged.is_some(),
        "",
    );
    let payload = match tagged {
        Some((at, _)) => page
            .text_at(at + 1, within, |text| text.starts_with("clipboard,"))
            .await
            .map(|(_, text)| text),
        None => None,
    };
    checks.check(
        "an answered read-back carries the selection",
        payload.as_deref() == Some(&format!("clipboard,{PAYLOAD_BASE64}")),
        format!("{payload:.64?}"),
    );
    checks.finish(&server.stop().await);
}
