/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The cursor a locked page cannot draw, drawn by the display the pointer is on.
//!
//! A pointer lock leaves the client with deltas and no positions, so it hands the cursor to the
//! server (`SET_NATIVE_CURSOR_RENDERING,1`) rather than placing an overlay of its own. Neither
//! half of that is display-aware: the tunable is the session's, every capture composites the
//! sprite from the pointer's root-relative position, and the deltas move the one remote pointer.
//! Locking a single page therefore covers the whole desktop, and the cursor crosses onto the
//! neighbor's stream on its own.
//!
//! Read from the JPEG stripes each display's client is sent, because that is where the drawing
//! is; a page cannot read its own decode back (two_display_pixels.rs says why). The lock is taken
//! on the primary's connection alone, and what has to hold is that its deltas carry the one
//! pointer across the seam, that the neighbor then draws the cursor at the position they reached
//! without having asked for anything, that the primary stops drawing it, and that unlocking hands
//! the drawing back to the client, where pointer motion damages no frame at all.
//!
//! The X11 backend is what this reads, where each capture composites the sprite into its own
//! region of the root; the Wayland compositor paints its cursor per output under the same
//! tunable, and its seam is pixelflux's wayland_seam test's.

use std::collections::BTreeMap;
use std::time::Duration;

use selkies_suites::displays::{
    self, Desktop, PRIMARY_RGB, Rect, SECONDARY_RGB, Stripe, decode_rgb, jpeg_settings,
};
use selkies_suites::server::test_display;
use selkies_suites::{Checks, Client, server};

/// A solid black square, so no theme cursor can be mistaken for it and no JPEG stripe can lose it
/// against the flat color a display's region is painted.
const SPRITE: i64 = 64;
/// Distance from that color which counts as the sprite; ringing on a flat field stays an order of
/// magnitude below it.
const SPRITE_TOL: i64 = 40;
const START: (i64, i64) = (400, 300);
/// How far past the seam the deltas land the pointer: more than the sprite's half, so one display
/// draws all of it.
const OVER: i64 = 320;
const STEP: i64 = 100;
/// A move within the neighbor, small enough to leave the sprite whole on it.
const NUDGE: (i64, i64) = (40, 30);
/// How far a repainted band is shifted from its display's color: enough for the capture to send
/// the band, too little to read as a sprite.
const SHADE: i64 = 12;

/// A box `(left, top, right, bottom)`, right and bottom exclusive, in a display's own pixels.
type Box4 = (i64, i64, i64, i64);

/// The sprite's box within one delivered band, in the display's own pixels: where the band's
/// difference from `bg`, as luma, passes the tolerance.
fn band_box(stripe: &Stripe, bg: [i64; 3]) -> Option<Box4> {
    let (w, _, pixels) = decode_rgb(&stripe.jpeg).expect("a delivered band decodes");
    let mut found: Option<Box4> = None;
    for (i, px) in pixels.as_chunks::<3>().0.iter().enumerate() {
        let d = |c: usize| (i64::from(px[c]) - bg[c]).abs();
        // ITU-R 601-2 luma, in the fixed point PIL's convert("L") uses.
        let luma = (d(0) * 19595 + d(1) * 38470 + d(2) * 7471 + 0x8000) >> 16;
        if luma > SPRITE_TOL {
            let (x, y) = (
                i64::try_from(i % w).unwrap_or(0),
                i64::try_from(i / w).unwrap_or(0),
            );
            found = merge(found, Some((x, y, x + 1, y + 1)));
        }
    }
    let top = i64::from(stripe.y);
    found.map(|(l, t, r, b)| (l, top + t, r, top + b))
}

fn merge(a: Option<Box4>, b: Option<Box4>) -> Option<Box4> {
    match (a, b) {
        (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))),
        (a, b) => a.or(b),
    }
}

/// `(frame id, sprite box)` per delivered frame, oldest first, the bands of one frame merged into
/// a single box.
fn frame_boxes(stripes: &[Stripe], bg: [i64; 3]) -> Vec<(u16, Option<Box4>)> {
    let mut out: Vec<(u16, Option<Box4>)> = Vec::new();
    for stripe in stripes {
        let found = band_box(stripe, bg);
        match out.last_mut() {
            Some(last) if last.0 == stripe.frame => last.1 = merge(last.1, found),
            _ => out.push((stripe.frame, found)),
        }
    }
    out
}

/// Where the sprite is on the picture the client composes: each band as the newest frame that
/// carried it left it.
///
/// Newest, not largest: a frame still in flight shows the sprite where it was, and only a band's
/// newest content says where it is now. By band rather than by frame, because a still screen's
/// cleanup re-sends its bands a few at a time: the newest frame can carry one band of a sprite
/// that spans two.
fn sprite_box(stripes: &[Stripe], bg: [i64; 3]) -> Option<Box4> {
    let mut latest: BTreeMap<u16, &Stripe> = BTreeMap::new();
    for stripe in stripes {
        latest.insert(stripe.y, stripe);
    }
    latest
        .values()
        .fold(None, |found, stripe| merge(found, band_box(stripe, bg)))
}

/// Whether the display is still drawing the sprite, which only its newest frame can say: one
/// frame that carried it keeps `sprite_box` answering, so absence is readable in the frame that
/// vacated it and nowhere else.
fn still_drawn(stripes: &[Stripe], bg: [i64; 3]) -> bool {
    frame_boxes(stripes, bg)
        .last()
        .is_some_and(|(_, found)| found.is_some())
}

/// Whether a sprite box is the cursor standing at `at` on its display.
///
/// The box has to hold that point rather than center on it, since the hotspot is wherever the
/// cursor in use puts it, and be no larger than the cursor set.
fn drawn_at(found: Option<Box4>, at: (i64, i64)) -> bool {
    let slack = 6;
    found.is_some_and(|b| {
        b.0 - slack <= at.0
            && at.0 <= b.2 + slack
            && b.1 - slack <= at.1
            && at.1 <= b.3 + slack
            && b.2 - b.0 <= SPRITE + slack
            && b.3 - b.1 <= SPRITE + slack
    })
}

/// Repaint a band of a display's region in a shade of its own color.
///
/// A display sends only what changed, so silence cannot tell a cursor that has left from one still
/// standing there. The shade, too near the color to read as a sprite, makes the band arrive, and
/// what it carries is then an answer rather than a silence.
fn dirty_band(desktop: &Desktop, rect: Rect, y: i64, rgb: [i64; 3]) {
    let shaded = [rgb[0], (rgb[1] + SHADE).min(255), rgb[2]];
    desktop.band(rect, y, SPRITE * 2, shaded);
}

/// The stripes each page was sent over `window`, together with what arrived since the last read,
/// as a socket left unread holds it.
async fn collect(pages: [&Client; 2], window: Duration) -> [Vec<Stripe>; 2] {
    tokio::time::sleep(window).await;
    pages.map(|page| displays::stripes(page.take_payloads()))
}

/// Drain what a change already in flight sends, so the next window is the answer to the next
/// move alone.
async fn settle(pages: [&Client; 2], window: Duration) {
    collect(pages, window).await;
}

fn boxed(found: Option<Box4>) -> String {
    found.map_or("None".into(), |b| format!("{b:?}"))
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn a_locked_pages_cursor_is_drawn_by_the_display_it_is_on() {
    let mut checks = Checks::new("locked-cursor-across-displays");
    let display = test_display();
    let server = server::launch("locked-cursor-across-displays")
        .env("SELKIES_USE_CPU", "true")
        .start()
        .await;
    let primary = Client::connect(&server, "").await;
    primary.keep_payloads();
    primary.send(&jpeg_settings("primary"));
    server
        .settle(0, "Capture started for 'primary'", Duration::from_secs(15))
        .await;
    let secondary = Client::connect(&server, "").await;
    secondary.keep_payloads();
    secondary.send(&jpeg_settings("display2"));
    let pages = [&primary, &secondary];
    let short = Duration::from_millis(2500);
    let long = Duration::from_secs(5);
    let window = Duration::from_secs(4);

    checks.check(
        "the neighbor's capture started",
        server
            .saw(0, "Capture started for 'display2'", Duration::from_secs(45))
            .await,
        "",
    );
    let layout = displays::layout(&server);
    let laid_out = layout.as_ref().is_some_and(|l| l.contains_key("display2"));
    checks.check("both displays laid out", laid_out, format!("{layout:?}"));
    let Some(layout) = layout.filter(|_| laid_out) else {
        let log = server.stop().await;
        checks.finish(&log);
        return;
    };
    let neighbor = layout["display2"];
    let home = layout["primary"];
    let seam = neighbor.x;
    let union_w = layout.values().map(|r| r.x + r.w).max().unwrap_or(0);
    checks.check(
        "the framebuffer grew to the union",
        displays::wait_root_width(&display, union_w).await.is_some(),
        union_w,
    );

    let painted = Desktop::open(&display);
    painted.paint(&layout);
    let cursor = Desktop::open(&display);
    cursor.cursor_window(u16::try_from(SPRITE).unwrap_or(64));
    primary.send(&format!("m,{},{},0,0", START.0, START.1));
    tokio::time::sleep(Duration::from_secs(2)).await;

    // The client draws the cursor itself until a lock takes its positions away, and out-of-band
    // delivery is what makes that free: the pointer moving over static content damages nothing.
    settle(pages, short).await;
    primary.send("m2,40,20,0,0");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let idle = collect(pages, window).await;
    checks.check(
        "a pointer the client draws sends no frames at all",
        idle[0].is_empty() && idle[1].is_empty(),
        format!(
            "primary {} frames, neighbor {}",
            idle[0].len(),
            idle[1].len()
        ),
    );

    // What a lock asks for, from the locked page's connection alone.
    primary.send("SET_NATIVE_CURSOR_RENDERING,1");
    checks.check(
        "the lock's request reaches the session",
        server
            .saw(
                0,
                "Received SET_NATIVE_CURSOR_RENDERING: True",
                Duration::from_secs(15),
            )
            .await,
        "",
    );
    settle(pages, long).await;
    dirty_band(&painted, neighbor, START.1, SECONDARY_RGB);
    primary.send(&format!("m,{},{},0,0", START.0, START.1));
    tokio::time::sleep(Duration::from_millis(800)).await;
    let shown = collect(pages, window).await;
    let here = sprite_box(&shown[0], PRIMARY_RGB);
    checks.check(
        "the display the pointer is on draws the cursor",
        drawn_at(here, START),
        format!("{} at {START:?}", boxed(here)),
    );
    let there = sprite_box(&shown[1], SECONDARY_RGB);
    checks.check(
        "the display it is away from draws none",
        there.is_none(),
        format!("{} in {} frames", boxed(there), shown[1].len()),
    );

    // The locked page's own messages: deltas, and only deltas.
    settle(pages, short).await;
    let travel = seam + OVER - START.0;
    let mut steps = vec![STEP; usize::try_from(travel / STEP).unwrap_or(0)];
    steps.push(travel % STEP);
    for step in steps.into_iter().filter(|&s| s != 0) {
        primary.send(&format!("m2,{step},0,0,0"));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let pos = painted.pointer();
    checks.check(
        "the deltas carry the one pointer onto the neighbor",
        pos.is_some_and(|(x, _)| {
            neighbor.x <= x && x < neighbor.x + neighbor.w && (x - (seam + OVER)).abs() <= 2
        }),
        format!("{pos:?} in {neighbor:?}"),
    );
    // A nudge once the crossing has drained, so what each display sends answers for where the
    // pointer is now rather than for the frames the travel left in flight.
    settle(pages, short).await;
    dirty_band(&painted, home, START.1, PRIMARY_RGB);
    primary.send(&format!("m2,{},{},0,0", -NUDGE.0, NUDGE.1));
    tokio::time::sleep(Duration::from_millis(800)).await;
    let crossed = collect(pages, window).await;
    let landed = (OVER - NUDGE.0, START.1 + NUDGE.1);
    let reached = sprite_box(&crossed[1], SECONDARY_RGB);
    checks.check(
        "the neighbor draws the cursor where the deltas reached it",
        drawn_at(reached, landed),
        format!("{} at {landed:?}", boxed(reached)),
    );
    let left = frame_boxes(&crossed[0], PRIMARY_RGB);
    checks.check(
        "the display it left stops drawing it",
        !still_drawn(&crossed[0], PRIMARY_RGB),
        format!(
            "{:?} in {} frames",
            left.last().map(|(f, b)| (f, boxed(*b))),
            crossed[0].len()
        ),
    );

    // Unlocking gives the client its positions back, and the drawing with them.
    primary.send("SET_NATIVE_CURSOR_RENDERING,0");
    checks.check(
        "the unlock's request reaches the session",
        server
            .saw(
                0,
                "Received SET_NATIVE_CURSOR_RENDERING: False",
                Duration::from_secs(15),
            )
            .await,
        "",
    );
    settle(pages, long).await;
    dirty_band(&painted, home, landed.1, PRIMARY_RGB);
    dirty_band(&painted, neighbor, landed.1, SECONDARY_RGB);
    tokio::time::sleep(Duration::from_millis(800)).await;
    let back = collect(pages, window).await;
    let after = sprite_box(&back[1], SECONDARY_RGB);
    checks.check(
        "unlocking hands the drawing back to the client",
        sprite_box(&back[0], PRIMARY_RGB).is_none()
            && after.is_none()
            && !back[0].is_empty()
            && !back[1].is_empty(),
        format!(
            "{} in {} and {} frames",
            boxed(after),
            back[0].len(),
            back[1].len()
        ),
    );
    drop(cursor);
    drop(painted);
    let log = server.stop().await;
    checks.finish(&log);
}
