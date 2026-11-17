/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Each display carries its own region of the desktop, proven from the wire.
//!
//! The layout can be right while the pixels are wrong: a secondary pinned to the same offset as
//! the primary mirrors it, and a primary sized to the union spans both halves. This paints a
//! different color on each display's region and decodes the JPEG stripes the server actually
//! sends to a client bound to that display, so neither mistake can pass.
//!
//! Read at the protocol level rather than through a browser: the Chromium decode path renders
//! into a worker-owned OffscreenCanvas, whose contents a page-side drawImage cannot read back
//! reliably.

use std::time::Duration;

use selkies_suites::displays::{
    self, Desktop, PRIMARY_RGB, SECONDARY_RGB, Stripe, decode_rgb, jpeg_settings,
};
use selkies_suites::server::test_display;
use selkies_suites::{Checks, Client, server};

const TOLERANCE: i64 = 60;

/// Mean color of the most recently delivered stripe.
///
/// Recency, not size: a screen change in flight produces a transitional frame that compresses
/// larger than the settled one, so the biggest stripe reports a frame the display has already
/// moved past.
fn mean_rgb(stripes: &[Stripe]) -> Option<[i64; 3]> {
    let (_, _, pixels) = decode_rgb(&stripes.last()?.jpeg)?;
    let n = i64::try_from(pixels.len() / 3).ok().filter(|&n| n > 0)?;
    let mut sums = [0i64; 3];
    for px in pixels.as_chunks::<3>().0 {
        for (sum, &c) in sums.iter_mut().zip(px) {
            *sum += i64::from(c);
        }
    }
    // Rounded half to even, as Python's round() does.
    Some(sums.map(|s| {
        let (q, r) = (s / n, s % n);
        match (2 * r).cmp(&n) {
            std::cmp::Ordering::Greater => q + 1,
            std::cmp::Ordering::Equal if q % 2 == 1 => q + 1,
            _ => q,
        }
    }))
}

fn near(sample: Option<[i64; 3]>, rgb: [i64; 3]) -> bool {
    sample.is_some_and(|s| s.iter().zip(rgb).all(|(a, b)| (a - b).abs() <= TOLERANCE))
}

fn shown(sample: Option<[i64; 3]>) -> String {
    sample.map_or("None".into(), |[r, g, b]| format!("({r}, {g}, {b})"))
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn each_display_carries_its_own_region() {
    let mut checks = Checks::new("two-display-pixels");
    let display = test_display();
    let server = server::launch("two-display-pixels")
        .env("SELKIES_USE_CPU", "true")
        .start()
        .await;
    let primary = Client::connect(&server, "").await;
    primary.send(&jpeg_settings("primary"));
    server
        .settle(0, "Capture started for 'primary'", Duration::from_secs(15))
        .await;
    let secondary = Client::connect(&server, "").await;
    secondary.send(&jpeg_settings("display2"));

    checks.check(
        "secondary capture started",
        server
            .saw(0, "Capture started for 'display2'", Duration::from_secs(45))
            .await,
        "",
    );
    let layout = displays::layout(&server);
    let laid_out = layout.as_ref().is_some_and(|l| l.contains_key("display2"));
    checks.check("both displays laid out", laid_out, format!("{layout:?}"));
    let mut desktop = None;
    if let Some(layout) = layout.filter(|_| laid_out) {
        let union_w = layout.values().map(|r| r.x + r.w).max().unwrap_or(0);
        checks.check(
            "framebuffer grew to the union",
            displays::wait_root_width(&display, union_w).await.is_some(),
            union_w,
        );
        // Kept from before the paint, as a socket left unread holds them: a still screen sends
        // no stripes, so the ones that show the paint are the ones it set off.
        primary.keep_payloads();
        secondary.keep_payloads();
        primary.take_payloads();
        secondary.take_payloads();
        let painted = Desktop::open(&display);
        painted.paint(&layout);
        desktop = Some(painted);
        tokio::time::sleep(Duration::from_secs(8)).await;
        let (ps, ss) = (
            displays::stripes(primary.take_payloads()),
            displays::stripes(secondary.take_payloads()),
        );
        checks.check(
            "primary delivered frames after paint",
            !ps.is_empty(),
            ps.len(),
        );
        checks.check(
            "secondary delivered frames after paint",
            !ss.is_empty(),
            ss.len(),
        );
        let (pc, sc) = (mean_rgb(&ps), mean_rgb(&ss));
        checks.check(
            "primary carries its own region",
            near(pc, PRIMARY_RGB),
            format!("{} want {}", shown(pc), shown(Some(PRIMARY_RGB))),
        );
        checks.check(
            "secondary carries its own region",
            near(sc, SECONDARY_RGB),
            format!("{} want {}", shown(sc), shown(Some(SECONDARY_RGB))),
        );
        checks.check(
            "the two displays differ",
            pc != sc,
            format!("{} vs {}", shown(pc), shown(sc)),
        );
    }
    drop(desktop);
    let log = server.stop().await;
    checks.finish(&log);
}
