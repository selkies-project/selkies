/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A second display opens where the client asked for it, on the Wayland backend.
//!
//! "Left", "up", and "down" have to place the second screen there, not beside the first: each
//! display is a screen of the capture compositor's own, the secondary created at the rectangle
//! the layout says and the primary's screen moved off the origin for the arrangements that put
//! something left of or above it, so the asked-for position is the one the compositor captures.
//! (The session compositor nested inside arranges its own screens by its own rule -- side by
//! side, in the order they were opened -- until Selkies mirrors this arrangement into it, which
//! is the nested session's business and not checked here.)
//!
//! Both halves are checked for each position: the layout the server computes, and where the
//! compositor actually put the screens. Driven with raw WebSocket clients against the in-process
//! pixelflux compositor, no browser.

use std::collections::BTreeMap;
use std::time::Duration;

use selkies_suites::displays::{Rect, layouts_in};
use selkies_suites::{Checks, Client, Server, server};
use serde_json::json;

const PRIMARY: (u32, u32) = (1920, 1080);
const SECONDARY: (u32, u32) = (1280, 720);

fn settings(display_id: &str, size: (u32, u32), position: &str) -> String {
    format!(
        "SETTINGS,{}",
        json!({
            "displayId": display_id, "initialClientWidth": size.0,
            "initialClientHeight": size.1, "manual_resolution": false,
            "framerate": 30, "encoder": "jpeg", "video_crf": 25,
            "video_bitrate": 6000, "audio_bitrate": 128_000,
            "scaling_dpi": 96, "displayPosition": position,
        })
    )
}

/// What the server did about reconfiguring in `log`, for a run that produced no layout.
fn reconfigure_trail(log: &str) -> String {
    const MARKS: [&str; 7] = [
        "Starting display reconfiguration",
        "No display clients connected",
        "Calculating new extended desktop layout",
        "total display size is zero",
        "Removing and triggering full display reconfiguration",
        "dropped on Wayland",
        "Layout calculated",
    ];
    let seen: Vec<&str> = log
        .lines()
        .filter(|line| MARKS.iter().any(|m| line.contains(m)))
        .map(|line| line.rsplit(" - ").next().unwrap_or(line))
        .collect();
    let joined = seen.join(" | ");
    if joined.is_empty() {
        return "no reconfiguration logged".to_owned();
    }
    let start = joined.char_indices().rev().nth(399).map_or(0, |(at, _)| at);
    joined[start..].to_owned()
}

/// Whether `second` really sits at `position` relative to `first`.
fn beside(position: &str, first: Rect, second: Rect) -> bool {
    match position {
        "right" => second.x >= first.x + first.w,
        "left" => second.x + second.w <= first.x,
        "down" => second.y >= first.y + first.h,
        _ => second.y + second.h <= first.y,
    }
}

/// Where the compositor last put each display's screen, by output id, from its own log lines.
/// The primary's screen (output 0) boots at the origin and logs only its moves; a secondary is
/// recreated wherever it goes.
fn outputs(log: &str) -> BTreeMap<u32, (i64, i64)> {
    let mut where_ = BTreeMap::from([(0, (0, 0))]);
    for line in log.lines() {
        let Some(rest) = line.split("Output ").nth(1) else {
            continue;
        };
        let Some((id, tail)) = rest.split_once(' ') else {
            continue;
        };
        let Ok(id) = id.parse::<u32>() else {
            continue;
        };
        let point = if let Some(t) = tail.strip_prefix("repositioned to (") {
            t
        } else if let Some(t) = tail.strip_prefix("created: ") {
            match t.split_once(" @ (") {
                Some((size, t)) if size.split_once('x').is_some() => t,
                _ => continue,
            }
        } else {
            continue;
        };
        let Some((x, rest)) = point.split_once(", ") else {
            continue;
        };
        let Some((y, _)) = rest.split_once(')') else {
            continue;
        };
        if let (Ok(x), Ok(y)) = (x.parse(), y.parse()) {
            where_.insert(id, (x, y));
        }
    }
    where_
}

/// The compositor's output id for a display, as the server assigns them: 0 for the primary's
/// screen, N for `displayN`, 2 for a secondary name without usable digits.
fn output_id(display_id: &str) -> u32 {
    if display_id == "primary" {
        return 0;
    }
    let digits = &display_id[display_id
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .len()..];
    let n = if (1..=9).contains(&digits.len()) {
        digits.parse().unwrap_or(2)
    } else {
        2
    };
    n.max(2)
}

async fn drive(server: &Server, checks: &mut Checks) {
    // One pair of connections for the whole run, re-sent with a new position each time: the
    // server debounces reconnects, and a live position change is what a client does anyway.
    let primary = Client::connect(server, "").await;
    let mark = server.mark();
    primary.send(&settings("primary", PRIMARY, "right"));
    if !server
        .saw(
            mark,
            "Capture started for 'primary'",
            Duration::from_secs(45),
        )
        .await
    {
        checks.check("the primary streams", false, "no capture");
        return;
    }
    // The server debounces reconnects from one address; the second client is a fresh
    // connection from the same one.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let secondary = Client::connect(server, "").await;
    for position in ["right", "left", "up", "down"] {
        let mark = server.mark();
        secondary.send(&settings("display2", SECONDARY, position));
        server
            .settle(
                mark,
                "Capture started for 'display2'",
                Duration::from_secs(20),
            )
            .await;
        tokio::time::sleep(Duration::from_secs(4)).await;
        let since = server.log_from(mark);
        let got = layouts_in(&since).pop().unwrap_or_default();
        let placed = matches!(
            (got.get("primary"), got.get("display2")),
            (Some(first), Some(second)) if beside(position, *first, *second)
        );
        checks.check(
            &format!("the layout puts a '{position}' display there"),
            placed,
            if got.is_empty() {
                reconfigure_trail(&since)
            } else {
                format!("{got:?}")
            },
        );
        let want: BTreeMap<u32, (i64, i64)> = got
            .iter()
            .map(|(id, rect)| (output_id(id), (rect.x, rect.y)))
            .collect();
        let screens = outputs(&server.log());
        checks.check(
            &format!("the compositor's screens follow it for '{position}'"),
            !want.is_empty() && want.iter().all(|(id, at)| screens.get(id) == Some(at)),
            format!("screens={screens:?} layout={want:?}"),
        );
    }
    secondary.close().await;
    primary.close().await;
}

#[tokio::test]
#[ignore = "needs a built selkies server"]
async fn display_positions() {
    let server = server::launch("positions")
        .wayland(true)
        .env("SELKIES_DEBUG", "true")
        .start()
        .await;
    let mut checks = Checks::new("display-positions");
    drive(&server, &mut checks).await;
    let log = server.stop().await;
    checks.finish(&log);
}

#[test]
fn a_layout_literal_parses() {
    let got = layouts_in(
        "ws - Layout calculated: Total Size=3200x1080. Layouts: {'display2': {'x': -1280, 'y': 0, 'w': 1280, 'h': 720}, 'primary': {'x': 0, 'y': 0, 'w': 1920, 'h': 1080}}",
    );
    assert_eq!(
        got.last().and_then(|l| l.get("display2").copied()),
        Some(Rect {
            x: -1280,
            y: 0,
            w: 1280,
            h: 720
        })
    );
    let screens = outputs(
        "[Wayland] Output 2 created: 1280x720 @ (1920, 0) scale 1.00.\n[Wayland] Output 0 repositioned to (0, 720).",
    );
    assert_eq!(screens.get(&2), Some(&(1920, 0)));
    assert_eq!(screens.get(&0), Some(&(0, 720)));
    assert_eq!(output_id("display2"), 2);
}
