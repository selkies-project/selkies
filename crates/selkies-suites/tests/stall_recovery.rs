/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! How the server tells an idle client from a dead one, and how a gated client gets its stream
//! back, driven by a raw WebSocket client on a damage-gated screen (turbo off).
//!
//! A still screen sends no frames, so silence is not a stall: the server marks a client stalled
//! only when a frame it sent goes unanswered, and a client that repeats its last ack as a
//! heartbeat, or one that acks only new ids, is left alone. After the still period the first frame
//! must not read as a lagging client either: ids run at the capture cadence, so their distance
//! overstates how far behind a client that was sent nothing is. A client that stops acking under
//! motion is gated and re-probed on a keyframe, one that stops reading gets its stream back once
//! it drains, and one that never acks again costs a keyframe per re-probe rather than a stream. A
//! client whose acks keep coming but stop at one id (a presenter that stalled; the reproducer of
//! the ack freezing while the screen changes, at 10 and 60 fps) is re-probed the same way rather
//! than held forever, and one whose decoder waits for a keyframe gets one from the first re-probe.
//! The gate is shared by both backends and both transports' fan-out, so one X11 run covers it.
//!
//! The client here is the suite's own rather than the harness's page, because it answers each
//! frame as it arrives and can stop reading its socket. Uses `E2E_DISPLAY` when set; otherwise
//! starts a throwaway Xvfb.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio_websockets::{ClientBuilder, Limits, MaybeTlsStream, Message, WebSocketStream};

use selkies_suites::x11::Damage;
use selkies_suites::xvfb::Xvfb;
use selkies_suites::{Checks, Server, server};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// The gate's log lines since `mark`, counted by kind.
fn events(server: &Server, mark: usize) -> BTreeMap<&'static str, usize> {
    let text = server.log_from(mark);
    [
        ("stall", "Client stall for"),
        ("reprobe", "Re-probing stalled client"),
        ("desync_reprobe", "Re-probing desynced client"),
        ("lifted", "Backpressure LIFTED"),
        ("triggered", "Backpressure TRIGGERED"),
        ("traceback", "Traceback"),
    ]
    .into_iter()
    .map(|(kind, needle)| (kind, text.matches(needle).count()))
    .collect()
}

/// What a client sends while it pumps.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ack {
    /// Acks each new frame id.
    New,
    /// Also repeats the last id every second, as the client does.
    Heartbeat,
    /// Sends nothing.
    None,
    /// Repeats the id held when it began every second and acks nothing newer.
    Frozen,
    /// As `Frozen` until a keyframe arrives, then as `Heartbeat`.
    Keygate,
}

/// One frame as it arrived: when, its id, whether it is a keyframe.
type Frame = (Instant, u16, bool);

/// A display's client on the raw protocol: frames in, acks out.
struct Raw {
    ws: Socket,
    last_id: Option<u16>,
    last_at: Instant,
    frames: Vec<Frame>,
    hb_at: Option<Instant>,
    held: Option<(Option<u16>, Instant)>,
}

impl Raw {
    async fn connect(server: &Server, encoder: &str, fps: u32) -> Raw {
        let uri = server.websocket("");
        let (mut ws, _) = ClientBuilder::new()
            .uri(&uri)
            .expect("a WebSocket address")
            .limits(Limits::unlimited())
            .connect()
            .await
            .unwrap_or_else(|e| panic!("{uri} refused the socket: {e}"));
        let _ = tokio::time::timeout(Duration::from_secs(10), ws.next()).await;
        let settings = json!({
            "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
            "manual_resolution": false, "framerate": fps, "video_crf": 25,
            "video_bitrate": 6000, "audio_bitrate": 128_000, "scaling_dpi": 96,
            "displayPosition": "right", "encoder": encoder,
        });
        let _ = ws.send(Message::text(format!("SETTINGS,{settings}"))).await;
        Raw {
            ws,
            last_id: None,
            last_at: Instant::now(),
            frames: Vec::new(),
            hb_at: None,
            held: None,
        }
    }

    async fn ack(&mut self, id: Option<u16>, held_ms: u128) -> Result<(), String> {
        let id = id.map_or(-1, i64::from);
        self.ws
            .send(Message::text(format!("CLIENT_FRAME_ACK {id} {held_ms}")))
            .await
            .map_err(|e| e.to_string())
    }

    fn heartbeat_due(&self, now: Instant) -> bool {
        self.hb_at
            .is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1))
    }

    /// Receive for `seconds`, acking as `ack` says; with `read` false the socket is left unread,
    /// as a suspended app leaves it. The error is a socket the server closed.
    async fn pump(&mut self, seconds: f64, ack: Ack, read: bool) -> Result<(), String> {
        let end = Instant::now() + Duration::from_secs_f64(seconds);
        if !read {
            tokio::time::sleep_until(end.into()).await;
            return Ok(());
        }
        if matches!(ack, Ack::Frozen | Ack::Keygate) && self.held.is_none() {
            self.held = Some((self.last_id, self.last_at));
        }
        while Instant::now() < end {
            let message =
                match tokio::time::timeout(Duration::from_millis(200), self.ws.next()).await {
                    Ok(Some(Ok(message))) => Some(message),
                    Ok(Some(Err(e))) => return Err(e.to_string()),
                    Ok(None) => return Err("the server closed the socket".into()),
                    Err(_) => None,
                };
            let now = Instant::now();
            if let Some(message) = message.filter(Message::is_binary) {
                let m: &[u8] = message.as_payload();
                if m.len() >= 4 && matches!(m[0], 0x03 | 0x04) {
                    let id = u16::from_be_bytes([m[2], m[3]]);
                    let key = m[0] == 0x04 && m[1] & 0x0f == 0x01;
                    if self.last_id != Some(id) {
                        self.last_id = Some(id);
                        self.last_at = now;
                        self.frames.push((now, id, key));
                        if ack == Ack::Keygate && key {
                            self.held = None;
                        }
                        if matches!(ack, Ack::New | Ack::Heartbeat)
                            || (ack == Ack::Keygate && self.held.is_none())
                        {
                            self.ack(Some(id), 0).await?;
                            self.hb_at = Some(now);
                        }
                    }
                }
            }
            if matches!(ack, Ack::Frozen | Ack::Keygate)
                && let Some((held_id, held_at)) = self.held
            {
                if self.heartbeat_due(now) {
                    self.ack(held_id, now.duration_since(held_at).as_millis())
                        .await?;
                    self.hb_at = Some(now);
                }
            } else if matches!(ack, Ack::Heartbeat | Ack::Keygate)
                && self.last_id.is_some()
                && self.heartbeat_due(now)
            {
                let held = now.duration_since(self.last_at).as_millis();
                self.ack(self.last_id, held).await?;
                self.hb_at = Some(now);
            }
        }
        Ok(())
    }

    fn since(&self, t0: Instant) -> Vec<Frame> {
        self.frames.iter().filter(|f| f.0 >= t0).copied().collect()
    }

    async fn close(mut self) {
        let _ = self.ws.close().await;
    }
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// The gaps between consecutive frames.
fn gaps(frames: &[Frame]) -> Vec<f64> {
    frames
        .windows(2)
        .map(|w| secs(w[1].0.duration_since(w[0].0)))
        .collect()
}

fn max_of(values: &[f64]) -> Option<f64> {
    values.iter().copied().reduce(f64::max)
}

/// What a test needs at hand: the server, the display to churn, its checks.
struct Run<'a> {
    server: &'a Server,
    display: &'a str,
    checks: &'a mut Checks,
    encoder: &'a str,
    tag: String,
}

impl Run<'_> {
    fn check(&mut self, name: &str, ok: bool, detail: impl std::fmt::Display) {
        self.checks
            .check(&format!("{} {name}", self.tag), ok, detail);
    }

    /// Motion, a still screen for 10 s, then motion again; `kind` is what the client sends
    /// meanwhile: the heartbeat, or new ids only (an older client).
    async fn still_screen(&mut self, kind: &str) {
        let ack = if kind == "heartbeat" {
            Ack::Heartbeat
        } else {
            Ack::New
        };
        let churn = Damage::start(self.display);
        let mut c = Raw::connect(self.server, self.encoder, 60).await;
        let _ = c.pump(4.0, Ack::New, true).await;
        let n = c.frames.len();
        self.check(
            &format!("{kind}: frames flow with motion"),
            n > 20,
            format!("{n} frames"),
        );
        let mark = self.server.mark();
        drop(churn);
        let t0 = Instant::now();
        let _ = c.pump(10.0, ack, true).await;
        let quiet = c.since(t0 + Duration::from_millis(1500));
        let ev = events(self.server, mark);
        self.check(
            &format!("{kind}: a still screen sends nothing"),
            quiet.is_empty(),
            format!("{} frames after the repaint", quiet.len()),
        );
        self.check(
            &format!("{kind}: a still screen is not a stall"),
            ev["stall"] == 0,
            format!("{ev:?}"),
        );
        let mark = self.server.mark();
        let t1 = Instant::now();
        let churn = Damage::start(self.display);
        let _ = c.pump(4.0, ack, true).await;
        let fresh = c.since(t1);
        let first = fresh.first().map(|f| secs(f.0.duration_since(t1)));
        let gaps_early: Vec<f64> = fresh
            .windows(2)
            .filter(|w| w[1].0.duration_since(t1) < Duration::from_millis(2500))
            .map(|w| secs(w[1].0.duration_since(w[0].0)))
            .collect();
        let ev = events(self.server, mark);
        self.check(
            &format!("{kind}: motion resumes the stream at once"),
            first.is_some_and(|f| f < 1.5),
            format!("first frame after {:?} s", first.map(round2)),
        );
        self.check(
            &format!("{kind}: the resume is not gated as a lagging client"),
            ev["triggered"] == 0 && ev["lifted"] == 0,
            format!("{ev:?}"),
        );
        self.check(
            &format!("{kind}: no freeze follows the first frame"),
            max_of(&gaps_early).is_some_and(|g| g < 0.3),
            format!(
                "max gap {:?} s over {} frames",
                max_of(&gaps_early).map(round2),
                gaps_early.len()
            ),
        );
        c.close().await;
        drop(churn);
    }

    /// The client keeps reading but stops acking for 5 s under motion.
    async fn silent(&mut self) {
        let churn = Damage::start(self.display);
        let mut c = Raw::connect(self.server, self.encoder, 60).await;
        let _ = c.pump(3.0, Ack::New, true).await;
        let mark = self.server.mark();
        let _ = c.pump(5.0, Ack::None, true).await;
        let ev = events(self.server, mark);
        self.check(
            "silent: a client that stops acking is gated",
            ev["stall"] + ev["triggered"] >= 1,
            format!("{ev:?}"),
        );
        self.check(
            "silent: the re-probe waits its turn",
            ev["reprobe"] == 0,
            format!("{ev:?}"),
        );
        let mark = self.server.mark();
        let t1 = Instant::now();
        let _ = c.ack(c.last_id, 0).await;
        let _ = c.pump(5.0, Ack::Heartbeat, true).await;
        let fresh = c.since(t1);
        let first = fresh.first().map(|f| secs(f.0.duration_since(t1)));
        let ev = events(self.server, mark);
        self.check(
            "silent: the stream resumes once the client acks again",
            first.is_some_and(|f| f < 1.5) && fresh.len() > 20,
            format!(
                "first frame after {:?} s, {} frames, {ev:?}",
                first.map(round2),
                fresh.len()
            ),
        );
        let h264 = self.encoder == "h264enc";
        self.check(
            "silent: the lift opens on a keyframe",
            ev["lifted"] >= 1 && fresh.first().is_some_and(|f| !h264 || f.2),
            format!("{:?} {ev:?}", fresh.first().map(|f| (f.1, f.2))),
        );
        c.close().await;
        drop(churn);
    }

    /// The client stops reading and acking for 8 s, as a suspended app does.
    ///
    /// Two ends are right. Where the socket buffers hold the pause, the client is gated,
    /// re-probed, and gets live frames once it drains. Where they do not, a send blocks past the
    /// liveness bound and the server drops the socket on purpose, which the page answers with a
    /// reconnect.
    async fn suspended(&mut self) {
        let churn = Damage::start(self.display);
        let mut c = Raw::connect(self.server, self.encoder, 60).await;
        let _ = c.pump(3.0, Ack::New, true).await;
        let mark = self.server.mark();
        let _ = c.pump(8.0, Ack::None, false).await;
        let t1 = Instant::now();
        if let Err(e) = c.pump(8.0, Ack::Heartbeat, true).await {
            let dropped = self.server.log_from(mark).contains("send stalled past");
            self.check(
                "suspended: a socket the buffers could not hold was dropped on purpose",
                dropped,
                format!(
                    "{}; server log {} the stalled send",
                    e.chars().take(80).collect::<String>(),
                    if dropped { "names" } else { "lacks" }
                ),
            );
            drop(churn);
            return;
        }
        let ev = events(self.server, mark);
        let late = c.since(t1 + Duration::from_secs(4));
        self.check(
            "suspended: the client was gated and re-probed",
            ev["stall"] + ev["triggered"] >= 1 && ev["reprobe"] >= 1,
            format!("{ev:?}"),
        );
        self.check(
            "suspended: live frames flow again once the backlog drains",
            late.len() > 20,
            format!("{} frames in the last 4 s, {ev:?}", late.len()),
        );
        c.close().await;
        drop(churn);
    }

    /// The client reads but never acks again.
    async fn dead(&mut self) {
        let churn = Damage::start(self.display);
        let mut c = Raw::connect(self.server, self.encoder, 60).await;
        let _ = c.pump(3.0, Ack::New, true).await;
        let mark = self.server.mark();
        let t0 = Instant::now();
        let _ = c.pump(20.0, Ack::None, true).await;
        let ev = events(self.server, mark);
        let keys = c
            .since(t0 + Duration::from_secs(5))
            .iter()
            .filter(|f| f.2)
            .count();
        self.check(
            "dead: the gate cycles gate, re-probe, gate",
            ev["stall"] + ev["triggered"] >= 2 && ev["reprobe"] >= 2,
            format!("{ev:?}"),
        );
        if self.encoder == "h264enc" {
            self.check(
                "dead: each re-probe costs one keyframe, not a stream",
                (1..=ev["reprobe"] + 1).contains(&keys),
                format!("{keys} IDRs for {} re-probes", ev["reprobe"]),
            );
        }
        self.check(
            "dead: nothing raised in the server",
            ev["traceback"] == 0,
            format!("{ev:?}"),
        );
        c.close().await;
        drop(churn);
    }

    /// The ack stops at one id, repeated every second, while the screen keeps changing: the gate
    /// closes on it and must not stay closed.
    async fn frozen(&mut self, fps: u32) {
        let churn = Damage::start(self.display);
        let mut c = Raw::connect(self.server, self.encoder, fps).await;
        let _ = c.pump(3.0, Ack::New, true).await;
        let mark = self.server.mark();
        let t0 = Instant::now();
        let _ = c.pump(12.0, Ack::Frozen, true).await;
        let ev = events(self.server, mark);
        let got = c.since(t0 + Duration::from_secs(1));
        let mut stamps = vec![t0 + Duration::from_secs(1)];
        stamps.extend(got.iter().map(|f| f.0));
        stamps.push(t0 + Duration::from_secs(12));
        let longest = stamps
            .windows(2)
            .map(|w| secs(w[1].saturating_duration_since(w[0])))
            .fold(0.0, f64::max);
        let keys = got.iter().filter(|f| f.2).count();
        let name = |what: &str| format!("frozen {fps} fps: {what}");
        self.check(
            &name("the gate closes on an ack that stops"),
            ev["triggered"] >= 1,
            format!("{ev:?}"),
        );
        self.check(
            &name("and is re-probed rather than held"),
            ev["desync_reprobe"] >= 2,
            format!("{ev:?}"),
        );
        self.check(
            &name("frames keep reaching the client between re-probes"),
            longest < 4.0,
            format!("longest gap {longest:.2} s, {} frames in 11 s", got.len()),
        );
        if self.encoder != "jpeg" {
            let reprobes = ev["desync_reprobe"];
            self.check(
                &name("each re-probe costs a keyframe, not a stream"),
                reprobes.saturating_sub(1) <= keys && keys <= reprobes + 2,
                format!("{keys} IDRs for {reprobes} re-probes"),
            );
        }
        c.held = None;
        let t1 = Instant::now();
        let _ = c.pump(4.0, Ack::Heartbeat, true).await;
        let late = c.since(t1 + Duration::from_secs(1));
        let late_gaps = gaps(&late);
        self.check(
            &name("the stream is whole again once the acks move"),
            late.len() > usize::try_from(fps).unwrap_or(usize::MAX)
                && max_of(&late_gaps).is_some_and(|g| g < 0.5),
            format!(
                "{} frames in 3 s, max gap {:?} s",
                late.len(),
                max_of(&late_gaps).map(round2)
            ),
        );
        self.check(
            &name("nothing raised in the server"),
            ev["traceback"] == 0,
            format!("{ev:?}"),
        );
        c.close().await;
        drop(churn);
    }

    /// A decoder that lost its reference: the ack holds until a keyframe arrives, which only a
    /// gate that reopens can send.
    async fn keygate(&mut self) {
        let churn = Damage::start(self.display);
        let mut c = Raw::connect(self.server, self.encoder, 60).await;
        let _ = c.pump(3.0, Ack::New, true).await;
        let mark = self.server.mark();
        let t0 = Instant::now();
        let _ = c.pump(10.0, Ack::Keygate, true).await;
        let ev = events(self.server, mark);
        let recovered = c
            .since(t0)
            .iter()
            .find(|f| f.2)
            .map(|f| secs(f.0.duration_since(t0)));
        let late = c.since(t0 + Duration::from_secs(7));
        // The gate closes at a check (every 0.5 s) once the held acks fall a quarter second
        // behind, re-probes two seconds after they stop moving, counted from an ack the held
        // client sends once a second, and then sends the IDR: 3.5-4 s here, a step or two more on
        // a slow runner.
        self.check(
            "keygate: the keyframe the client waits for reaches it",
            recovered.is_some_and(|r| r < 6.0),
            format!("after {:?} s, {ev:?}", recovered.map(round2)),
        );
        self.check(
            "keygate: and its stream flows on",
            late.len() > 60,
            format!("{} frames in the last 3 s, {ev:?}", late.len()),
        );
        c.close().await;
        drop(churn);
    }
}

async fn drive(encoder: &str) {
    let mut checks = Checks::new("stall-recovery");
    let private = std::env::var("E2E_DISPLAY")
        .ok()
        .filter(|d| !d.is_empty())
        .is_none()
        .then(|| Xvfb::start(1920, 1080, &[]));
    let display = private
        .as_ref()
        .map_or_else(selkies_suites::server::test_display, |x| {
            x.display().to_owned()
        });
    let server = server::launch(&format!("stall-recovery-{encoder}"))
        .display(&display)
        .env("SELKIES_USE_CPU", "true")
        .env("SELKIES_VIDEO_STREAMING_MODE", "false")
        .env("SELKIES_ENCODER", encoder)
        .env("SELKIES_DEBUG", "true")
        .start()
        .await;
    let mut run = Run {
        server: &server,
        display: &display,
        checks: &mut checks,
        encoder,
        tag: format!("[{encoder}]"),
    };
    run.still_screen("heartbeat").await;
    run.still_screen("new-ids-only").await;
    run.silent().await;
    run.suspended().await;
    run.dead().await;
    run.frozen(10).await;
    run.frozen(60).await;
    if encoder != "jpeg" {
        run.keygate().await;
    }
    let log = server.stop().await;
    drop(private);
    checks.finish(&log);
}

#[tokio::test]
#[ignore = "needs a built selkies server and an X display (E2E_DISPLAY, or Xvfb)"]
async fn a_gated_client_gets_its_stream_back_h264enc() {
    drive("h264enc").await;
}

#[tokio::test]
#[ignore = "needs a built selkies server and an X display (E2E_DISPLAY, or Xvfb)"]
async fn a_gated_client_gets_its_stream_back_jpeg() {
    drive("jpeg").await;
}
