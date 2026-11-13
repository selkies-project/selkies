/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One X11 desktop, one DPI: the primary display's page owns it.
//!
//! Every display page derives its `scaling_dpi` from the density of the screen it is shown on,
//! and on X11 the server applies it to the session, not to one display's region. Two pages on
//! screens of different densities therefore each ask for a different desktop, and the desktop
//! would rescale for whichever spoke last, which is whenever a window is restored or dragged
//! between monitors. A secondary's DPI is refused instead, and it says so rather than moving the
//! whole session. The desktop keeps the last density across sessions, so a new session's page
//! moves it even when its density is the operator's default.
//!
//! Driven with raw pages: what is under test is the server's rule, not a browser's derivation.

use std::time::Duration;

use selkies_suites::{Checks, Client, server};
use serde_json::json;

const APPLIED: &str = "Applying system-level change.";
const REFUSED: &str = "the desktop DPI follows the primary display";
const WITHIN: Duration = Duration::from_secs(8);

/// The SETTINGS of the page of `display` on a screen of density `dpi`.
fn settings(display: &str, dpi: u32) -> String {
    format!(
        "SETTINGS,{}",
        json!({
            "displayId": display, "initialClientWidth": 1280, "initialClientHeight": 720,
            "manual_resolution": false, "framerate": 30, "encoder": "jpeg", "video_crf": 25,
            "video_bitrate": 6000, "audio_bitrate": 128000, "scaling_dpi": dpi,
            "displayPosition": "right",
        })
    )
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn the_desktop_dpi_follows_the_primary_displays_page_alone() {
    let server = server::launch("owner").start().await;
    let mut checks = Checks::new("session-dpi-owner");

    let primary = Client::connect(&server, "").await;
    let mark = server.mark();
    primary.send(&settings("primary", 192));
    checks.check(
        "the primary sets the desktop DPI",
        server.saw(mark, APPLIED, WITHIN).await,
        "",
    );

    let secondary = Client::connect(&server, "").await;
    secondary.send(&settings("display2", 96));
    // Its first settings run a reconfiguration that holds the lock the next settings wait on.
    server
        .settle(
            mark,
            "settings applied for 'display2'",
            Duration::from_secs(60),
        )
        .await;
    // The window is restored on a screen of another density, which is what makes its page derive
    // a new DPI and say so.
    let mark = server.mark();
    secondary.send(&settings("display2", 240));
    checks.check(
        "a secondary that rederives its density is refused",
        server.saw(mark, REFUSED, WITHIN).await,
        "",
    );
    checks.check(
        "and the desktop is not rescaled for it",
        server
            .stays_without(mark, APPLIED, Duration::from_secs(2))
            .await,
        "",
    );

    let mark = server.mark();
    secondary.send("s,96");
    checks.check(
        "the DPI sync verb is refused from a secondary too",
        server.saw(mark, REFUSED, WITHIN).await,
        "",
    );

    let mark = server.mark();
    primary.send(&settings("primary", 144));
    checks.check(
        "the primary still moves it",
        server.saw(mark, APPLIED, WITHIN).await,
        "",
    );

    // Past the reconnect grace, so the next page is a new session rather than the old page
    // returning: the desktop is still at the density the last page left it, and a page whose
    // default is the operator's has to move it back.
    let mark = server.mark();
    secondary.close().await;
    primary.close().await;
    server
        .settle(
            mark,
            "Last client gone after the grace period",
            Duration::from_secs(6),
        )
        .await;
    let primary = Client::connect(&server, "").await;
    let mark = server.mark();
    primary.send(&settings("primary", 96));
    checks.check(
        "a new session's page brings the desktop back to its own density",
        server.saw(mark, "DPI changed from 144 to 96", WITHIN).await,
        "",
    );
    checks.finish(&server.stop().await);
}
