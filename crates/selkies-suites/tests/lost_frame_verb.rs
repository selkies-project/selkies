/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A client that drops a frame tells the server, and the encoder predicts past it.
//!
//! A decoder that cannot keep up lets a frame go, which breaks every frame predicting from it.
//! Rather than freeze until a key frame arrives, the client names the frame it dropped
//! (`LOST_FRAME <id>`) and the server has that display's encoder leave it out of every later
//! prediction, so the next frame decodes there on its own. The verb is floored per display,
//! since any number of clients share one encoder, and a malformed one is ignored rather than
//! taken as a frame id.
//!
//! Driven with a raw page: what is under test is the server's answer to the verb, not a
//! browser's decision to send it.

use std::time::{Duration, Instant};

use selkies_suites::client::Binary;
use selkies_suites::server::test_display;
use selkies_suites::x11::Damage;
use selkies_suites::{Checks, Client, Server, server, tail};
use serde_json::json;

const TOLD: &str = "the encoder predicts past it";
/// The wire ids VP9 and H.265 frames carry in the high nibble of their type byte.
const VP9: u8 = 3;
const H265: u8 = 5;

/// A primary page's SETTINGS for `encoder`.
fn settings(encoder: &str) -> String {
    format!(
        "SETTINGS,{}",
        json!({
            "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
            "manual_resolution": false, "framerate": 60, "encoder": encoder,
            "video_crf": 25, "video_bitrate": 8000, "audio_bitrate": 128000,
            "scaling_dpi": 96,
        })
    )
}

/// A server that logs what its encoders are told.
async fn debug_server(label: &str) -> Server {
    server::launch(label)
        .env("SELKIES_DEBUG", "true")
        .start()
        .await
}

/// The full frames of `codec` that reached `page` from binary index `mark` on.
fn frames(page: &Client, mark: usize, codec: u8) -> Vec<Binary> {
    page.binaries(mark)
        .into_iter()
        .filter(|frame| frame.is_full_frame() && frame.codec() == codec)
        .collect()
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn a_lost_frame_report_reaches_the_encoder_once_and_a_malformed_one_never() {
    let server = debug_server("verb").await;
    let mut checks = Checks::new("lost-frame-verb");
    let page = Client::connect(&server, "").await;
    page.send(&settings("h264enc"));

    let mark = server.mark();
    page.send("LOST_FRAME 41");
    checks.check(
        "the encoder is told to predict past the frame a client lost",
        server.saw(mark, TOLD, Duration::from_secs(15)).await,
        tail(&server.log_from(mark), 2),
    );
    checks.check(
        "and the line names the frame",
        server.log_from(mark).contains("frame 41 lost by a client"),
        tail(&server.log_from(mark), 2),
    );

    let mark = server.mark();
    for frame in 100..140 {
        page.send(&format!("LOST_FRAME {frame}"));
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let told = server.log_from(mark).matches(TOLD).count();
    checks.check(
        "a burst of them adds no flood of log lines",
        told <= 1,
        told,
    );

    let mark = server.mark();
    page.send("LOST_FRAME nonsense");
    page.send("LOST_FRAME");
    checks.check(
        "a malformed report is ignored rather than taken as a frame id",
        server
            .stays_without(mark, TOLD, Duration::from_millis(500))
            .await,
        tail(&server.log_from(mark), 2),
    );

    let mark = server.mark();
    page.send("REQUEST_KEYFRAME");
    checks.check(
        "the session still answers, so nothing about the verb wedged it",
        server
            .saw(mark, "Keyframe requested by", Duration::from_secs(15))
            .await,
        tail(&server.log_from(mark), 2),
    );
    checks.finish(&server.stop().await);
}

/// A lost frame is predicted past where the encoder can name its references, and repaired with a
/// key frame where it cannot.
///
/// Software VP9 tracks its references through libvpx's flexible reference mode, so the frame
/// after the report predicts from one before the loss and no key frame is spent; a hardware VP9
/// session on a render node does the same through its reference slots. Software H.265 offers
/// nothing to steer its references with, names none, and pixelflux codes a key frame there
/// instead; on a host whose GPU takes the H.265 session the encoder predicts past the loss as
/// VP9 does. Its delta frames tell the two apart: a session that tracks its references names the
/// frame each predicts from, one that does not repeats its id.
#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn a_lost_frame_is_predicted_past_or_repaired_with_a_key_frame() {
    let server = debug_server("answers").await;
    let mut checks = Checks::new("lost-frame-verb");
    let _motion = Damage::start(&test_display());
    let page = Client::connect(&server, "").await;
    for (encoder, codec, name) in [("vp9enc", VP9, "VP9"), ("h265enc", H265, "H.265")] {
        let mark = page.binary_count();
        page.send(&settings(encoder));
        // The frames once the switch has taken and the newest is a delta frame: the switch
        // restarts the capture, and a key frame reported lost leaves nothing earlier to predict
        // from.
        let deadline = Instant::now() + Duration::from_secs(25);
        let mut stream = frames(&page, mark, codec);
        while Instant::now() < deadline && stream.last().is_none_or(Binary::is_key) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            stream = frames(&page, mark, codec);
        }
        checks.check(
            &format!("the {name} session streams"),
            !stream.is_empty(),
            format!("{} frames since the switch", page.binaries(mark).len()),
        );
        let Some(last) = stream.last().and_then(Binary::video_frame_id) else {
            continue;
        };
        let tracked = codec == VP9
            || stream
                .iter()
                .any(|frame| !frame.is_key() && Some(frame.reference()) != frame.video_frame_id());
        let mark = page.binary_count();
        page.send(&format!("LOST_FRAME {last}"));
        tokio::time::sleep(Duration::from_secs(2)).await;
        let after = frames(&page, mark, codec);
        let keys = after.iter().filter(|frame| frame.is_key()).count();
        // A frame or two encoded before the report reached the encoder still predict from the
        // lost one; the ones after it reach back past it.
        let behind = after
            .iter()
            .filter(|frame| {
                !frame.is_key()
                    && frame.reference() != last
                    && last.wrapping_sub(frame.reference()) < 0x8000
            })
            .count();
        let detail = format!(
            "{} frames, {keys} key, {behind} predicting from before the loss, first from {:?}, lost {last}",
            after.len(),
            after.first().map(Binary::reference)
        );
        if tracked {
            checks.check(
                &format!(
                    "{name}: the frames after the report predict from before the loss and cost no key frame"
                ),
                keys == 0 && behind > 0,
                detail,
            );
        } else {
            checks.check(
                &format!(
                    "{name}: a session that cannot predict past it is repaired with a key frame"
                ),
                keys > 0,
                detail,
            );
        }
    }
    checks.finish(&server.stop().await);
}
