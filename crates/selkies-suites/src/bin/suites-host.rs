/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The host side of a Playwright suite (`tests/lib/host.ts`): the servers it runs and what it
//! reads off the host, through this crate's harness, so a suite in TypeScript starts its
//! server in the sandbox the Rust suites start theirs in.
//!
//! One request per line on standard input, one answer per line on standard output, in order:
//!
//! ```text
//! {"id": 1, "op": "server.start", "label": "release", "mode": "webrtc", "env": {"K": "V"}}
//! {"id": 1, "ok": {"port": 41234, "pid": 5150, "log": "/…/gamepad_release-release.log"}}
//! {"id": 2, "op": "nope"}
//! {"id": 2, "error": "no operation nope"}
//! ```
//!
//! | op | takes | answers |
//! | --- | --- | --- |
//! | `server.start` | `label`, `mode`, `wayland`, `display`, `socket`, `port`, `env` | `port`, `pid`, `log` |
//! | `server.stop` | | `tail`: the end of its log |
//! | `server.binary` | | `path`: the server under test, as the harness resolves it |
//! | `server.mark` | | `mark`: the log's length now |
//! | `server.log` | `from`: a mark | `text`: the log from it on |
//! | `server.saw` | `from`, `needle`, `timeout_ms` | `saw`: whether the log took `needle` past `from` in time |
//! | `listeners` | `pid` | `listeners`: the TCP addresses the process listens on |
//! | `x11.key_held` | `keysym`, `display` | `held`: whether its key is down, null without a keycode |
//! | `x11.pointer` | `display` | `at`: `[x, y]` on the root |
//! | `x11.root_size` | `display` | `size`: `[width, height]` |
//! | `x11.window_manager` | `display` | `manager`: its `pid` (0 unknown) and `name`, null for none |
//! | `x11.clipboard` | `timeout_ms`, `display` | `text`: CLIPBOARD's text, null when none comes |
//! | `x11.own_clipboard` | `payload` or `payload_hex`, `formats`, `display` | `handle` of an owner answering each of `formats` (`{target, text}` or `{target, hex}`) with its bytes, and any other target with the payload where one is given |
//! | `x11.owns` | `handle`, `display` | `owns`: whether that owner still holds CLIPBOARD |
//! | `x11.targets` | `timeout_ms`, `display` | `targets`: the names CLIPBOARD's owner lists for `TARGETS`, null when none answers |
//! | `x11.convert` | `target`, `timeout_ms`, `display` | `hex`: CLIPBOARD converted to `target`, null when refused or unanswered |
//! | `x11.keycode` | `keysym`, `display` | `code`: the keycode typing it, null without one |
//! | `x11.buttons` | `display` | `held`: the pointer buttons held, lowest first |
//! | `x11.watch` | `cover`, `display` | `handle` of a window in the middle, or covering the display, that records keys, buttons and motion |
//! | `x11.events` | `handle` | `events`: what the window received, each `{kind, detail, x, y, root_x, root_y, at}`, `at` in seconds on `CLOCK_MONOTONIC` |
//! | `x11.probe` | `name`, `width`, `height`, `display` | `handle` and X `id` of a plain titled window for a window manager to take |
//! | `x11.frame` | `handle` | `frame`: `[x, y, width, height]` of the frame a manager put the probe in, null while it has none |
//! | `x11.churn` | `width`, `height`, `display` | `handle` of a window drawing a moving block 30 times a second |
//! | `x11.root_cardinal` | `name`, `value`, `display` | `value`: the root's CARDINAL `name`, null where absent; a `value` given sets it first, a null one deletes it |
//! | `x11.warp` | `x`, `y`, `display` | `done`: the pointer put there |
//! | `x11.key` | `keysym`, `press`, `display` | `done`: an XTEST press or release of the key typing `keysym` |
//! | `x11.button` | `button`, `press`, `display` | `done`: an XTEST press or release |
//! | `x11.motion` | `dx`, `dy`, `count`, `gap_ms`, `display` | `done`: `count` XTEST relative moves, `gap_ms` apart |
//! | `x11.paint` | `paints`, `flicker`, `display` | `handle` of solid windows: each `[x, y, w, h, rgb]`, the flicker `{paint, second, period_ms}` |
//! | `wl.observe` | `socket`, `env`, `program` | `handle` of a client on the server's compositor: `wl-observer`, or the crate's `program` |
//! | `wl.wait` | `handle`, `kind`, `match`, `timeout_ms` | `event`: the first of `kind` matching, or null |
//! | `wl.output` | `handle` | `lines`: what the client printed that was no event |
//! | `wl.lines` | `handle`, `from` | `lines`: its event lines from index `from` on |
//! | `xvfb.start` | `width`, `height`, `extra`, `binary` | `display` and `handle` of a private Xvfb (`binary` a path or a name on `PATH`, `Xvfb` by default) |
//! | `unix.stale` | `path` | a socket file at `path` whose listener has gone away |
//! | `threads` | `pid` | `threads`: the thread ids the process runs now |
//! | `load.start` | `threads`, `stall` | `handle` of busy processes sharing one CPU with `threads`, stopped |
//! | `load.resume` / `load.pause` | `handle` | the burst begun or ended |
//! | `release` | `handle` | an owner, observer, painting, X server or load ended |
//!
//! `display` defaults to `E2E_DISPLAY`, the display the servers capture.
//!
//! A new `server.start` stops the server before it, as one suite runs one server at a time. The
//! program ends when its standard input does, which is when the suite's process does however it
//! ends, and takes its server along; a server is tied to this program's thread as well, so not
//! even a kill of this program leaves one running.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::time::Duration;

use selkies_suites::load::{self, Load};
use selkies_suites::wayland::WlObserver;
use selkies_suites::x11::{self, SelectionOwner};
use selkies_suites::xvfb::Xvfb;
use selkies_suites::{Server, private_dir, server};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// What the program holds between requests.
#[derive(Default)]
struct Host {
    server: Option<Server>,
    /// Clipboard owners, by the handle given out for them; dropping one releases the selection.
    owners: HashMap<u64, SelectionOwner>,
    /// Observers, by the handle given out for them.
    observers: HashMap<u64, WlObserver>,
    /// Painted windows, by the handle given out for them.
    paintings: HashMap<u64, x11::Painting>,
    /// Churning windows, by the handle given out for them.
    churns: HashMap<u64, x11::Churn>,
    /// Input watches, by the handle given out for them.
    watches: HashMap<u64, x11::InputWatch>,
    /// Windows for a window manager to take, by the handle given out for them.
    probes: HashMap<u64, x11::ManagedProbe>,
    /// Private X servers, by the handle given out for them.
    servers: HashMap<u64, Xvfb>,
    /// CPU loads, by the handle given out for them.
    loads: HashMap<u64, Load>,
    next: u64,
}

/// `bytes` as lowercase hexadecimal, two digits a byte.
fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// The bytes `hex` spells, or `None` where it is no even run of hexadecimal digits.
fn from_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok())
        .collect()
}

/// The bytes a request carries in `text_field` as text, or in `hex_field` as hexadecimal.
fn bytes_of(request: &Map<String, Value>, text_field: &str, hex_field: &str) -> Option<Vec<u8>> {
    text(request, text_field)
        .map(|t| t.as_bytes().to_vec())
        .or_else(|| text(request, hex_field).and_then(from_hex))
}

/// `field` of `request` as a string, if it is one.
fn text<'a>(request: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    request.get(field).and_then(Value::as_str)
}

/// `field` of `request` as a number, if it is a whole one.
fn whole(request: &Map<String, Value>, field: &str) -> Option<u64> {
    request.get(field).and_then(Value::as_u64)
}

/// `timeout_ms` of `request`, else `default`.
fn timeout(request: &Map<String, Value>, default: Duration) -> Duration {
    whole(request, "timeout_ms").map_or(default, Duration::from_millis)
}

/// A window to paint, `[x, y, w, h, rgb]`.
fn paint(value: &Value) -> Option<x11::Paint> {
    let n = |i: usize| value.get(i).and_then(Value::as_i64);
    Some(x11::Paint {
        x: i16::try_from(n(0)?).ok()?,
        y: i16::try_from(n(1)?).ok()?,
        width: u16::try_from(n(2)?).ok()?,
        height: u16::try_from(n(3)?).ok()?,
        color: u32::try_from(n(4)?).ok()?,
    })
}

/// The X display a request names, else the test display.
fn display(request: &Map<String, Value>) -> Result<String, String> {
    if let Some(named) = text(request, "display") {
        return Ok(named.to_owned());
    }
    std::env::var("E2E_DISPLAY")
        .ok()
        .filter(|d| !d.is_empty())
        .ok_or_else(|| "E2E_DISPLAY is not set and the request names no display".to_owned())
}

impl Host {
    async fn answer(&mut self, op: &str, request: &Map<String, Value>) -> Result<Value, String> {
        match op {
            "server.start" => {
                if let Some(running) = self.server.take() {
                    running.stop().await;
                }
                let label = text(request, "label").unwrap_or("server");
                let mut launch = server::launch(label)
                    .mode(text(request, "mode").unwrap_or("websockets"))
                    .wayland(
                        request
                            .get("wayland")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    );
                if let Some(display) = text(request, "display") {
                    launch = launch.display(display);
                }
                if let Some(port) = whole(request, "port").and_then(|p| u16::try_from(p).ok()) {
                    launch = launch.port(port);
                }
                if let Some(socket) = text(request, "socket") {
                    launch = launch.unix_socket(Path::new(socket));
                }
                if let Some(env) = request.get("env").and_then(Value::as_object) {
                    for (name, value) in env {
                        let value = value
                            .as_str()
                            .ok_or_else(|| format!("env {name} is not a string"))?;
                        launch = launch.env(name, value);
                    }
                }
                let started = launch.spawn().await;
                if let Err(why) = started.came_up().await {
                    let tail = started.stop().await;
                    return Err(format!("{why}:\n{tail}"));
                }
                let answer = json!({
                    "port": started.port(),
                    "pid": started.pid(),
                    "log": started.log_path().to_string_lossy(),
                });
                self.server = Some(started);
                Ok(answer)
            }
            "server.stop" => match self.server.take() {
                Some(running) => Ok(json!({ "tail": running.stop().await })),
                None => Ok(json!({ "tail": "" })),
            },
            "server.binary" => Ok(json!({ "path": selkies_suites::server_binary() })),
            "server.mark" => Ok(json!({ "mark": self.running()?.mark() })),
            "server.log" => {
                let from = request
                    .get("from")
                    .and_then(Value::as_u64)
                    .and_then(|m| usize::try_from(m).ok())
                    .unwrap_or(0);
                Ok(json!({ "text": self.running()?.log_from(from) }))
            }
            "server.saw" => {
                let from = whole(request, "from")
                    .and_then(|m| usize::try_from(m).ok())
                    .unwrap_or(0);
                let needle = text(request, "needle").ok_or("server.saw takes a needle")?;
                let within = timeout(request, Duration::from_secs(12));
                Ok(json!({ "saw": self.running()?.saw(from, needle, within).await }))
            }
            "x11.key_held" => {
                let keysym = whole(request, "keysym")
                    .and_then(|k| u32::try_from(k).ok())
                    .ok_or("x11.key_held takes a keysym")?;
                Ok(json!({ "held": x11::key_held(&display(request)?, keysym) }))
            }
            "x11.keycode" => {
                let keysym = whole(request, "keysym")
                    .and_then(|k| u32::try_from(k).ok())
                    .ok_or("x11.keycode takes a keysym")?;
                Ok(json!({ "code": x11::keycode(&display(request)?, keysym) }))
            }
            "x11.churn" => {
                let size = |field: &str, default: u16| {
                    whole(request, field)
                        .and_then(|v| u16::try_from(v).ok())
                        .unwrap_or(default)
                };
                let churn =
                    x11::Churn::start(&display(request)?, size("width", 900), size("height", 700))?;
                let handle = self.handle();
                self.churns.insert(handle, churn);
                Ok(json!({ "handle": handle }))
            }
            "x11.root_cardinal" => {
                let shown = display(request)?;
                let name = text(request, "name").ok_or("x11.root_cardinal takes a name")?;
                if let Some(value) = request.get("value") {
                    let value = value.as_u64().and_then(|v| u32::try_from(v).ok());
                    x11::set_root_cardinal(&shown, name, value);
                }
                Ok(json!({ "value": x11::root_cardinal(&shown, name) }))
            }
            "x11.warp" => {
                let at = |field: &str| {
                    request
                        .get(field)
                        .and_then(Value::as_i64)
                        .and_then(|v| i16::try_from(v).ok())
                        .unwrap_or(0)
                };
                Ok(json!({ "done": x11::warp(&display(request)?, at("x"), at("y")) }))
            }
            "x11.key" => {
                let keysym = whole(request, "keysym")
                    .and_then(|k| u32::try_from(k).ok())
                    .ok_or("x11.key takes a keysym")?;
                let press = request
                    .get("press")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                Ok(json!({ "done": x11::fake_key(&display(request)?, keysym, press) }))
            }
            "x11.button" => {
                let button = whole(request, "button")
                    .and_then(|b| u8::try_from(b).ok())
                    .ok_or("x11.button takes a button")?;
                let press = request
                    .get("press")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                Ok(json!({ "done": x11::fake_button(&display(request)?, button, press) }))
            }
            "x11.motion" => {
                let delta = |field: &str| {
                    request
                        .get(field)
                        .and_then(Value::as_i64)
                        .and_then(|v| i16::try_from(v).ok())
                        .unwrap_or(0)
                };
                let count = whole(request, "count")
                    .and_then(|c| u32::try_from(c).ok())
                    .unwrap_or(1);
                let gap = Duration::from_millis(whole(request, "gap_ms").unwrap_or(0));
                let shown = display(request)?;
                let (dx, dy) = (delta("dx"), delta("dy"));
                let done = tokio::task::spawn_blocking(move || {
                    x11::fake_motion(&shown, dx, dy, count, gap)
                })
                .await
                .map_err(|e| e.to_string())?;
                Ok(json!({ "done": done }))
            }
            "x11.buttons" => Ok(json!({ "held": x11::buttons_held(&display(request)?) })),
            "x11.watch" => {
                let cover = request
                    .get("cover")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let watch = x11::InputWatch::start(&display(request)?, cover)?;
                let handle = self.handle();
                self.watches.insert(handle, watch);
                Ok(json!({ "handle": handle }))
            }
            "x11.events" => {
                let handle = whole(request, "handle").ok_or("x11.events takes a handle")?;
                let watch = self.watches.get(&handle).ok_or("no such watch")?;
                let events: Vec<Value> = watch
                    .events()
                    .iter()
                    .map(|e| {
                        json!({
                            "kind": e.kind, "detail": e.detail, "x": e.x, "y": e.y,
                            "root_x": e.root_x, "root_y": e.root_y, "at": e.at,
                        })
                    })
                    .collect();
                Ok(json!({ "events": events }))
            }
            "x11.probe" => {
                let name = text(request, "name").unwrap_or("selkies-probe");
                let size = |field: &str, default: u16| {
                    whole(request, field)
                        .and_then(|v| u16::try_from(v).ok())
                        .unwrap_or(default)
                };
                let probe = x11::ManagedProbe::start(
                    &display(request)?,
                    name,
                    size("width", 400),
                    size("height", 260),
                )?;
                let id = probe.id();
                let handle = self.handle();
                self.probes.insert(handle, probe);
                Ok(json!({ "handle": handle, "id": id }))
            }
            "x11.frame" => {
                let handle = whole(request, "handle").ok_or("x11.frame takes a handle")?;
                let probe = self.probes.get(&handle).ok_or("no such probe")?;
                Ok(json!({ "frame": probe.frame() }))
            }
            "x11.pointer" => Ok(json!({ "at": x11::pointer(&display(request)?) })),
            "x11.window_manager" => Ok(json!({
                "manager": x11::window_manager(&display(request)?)
                    .map(|(pid, name)| json!({ "pid": pid, "name": name })),
            })),
            "x11.root_size" => Ok(json!({ "size": x11::root_size(&display(request)?) })),
            "x11.clipboard" => {
                let within = timeout(request, Duration::from_secs(8));
                Ok(json!({ "text": x11::read_clipboard(&display(request)?, within) }))
            }
            "x11.targets" => {
                let within = timeout(request, Duration::from_secs(8));
                Ok(json!({ "targets": x11::clipboard_targets(&display(request)?, within) }))
            }
            "x11.convert" => {
                let within = timeout(request, Duration::from_secs(5));
                let target = text(request, "target").ok_or("x11.convert takes a target")?;
                let value = x11::convert_clipboard(&display(request)?, target, within);
                Ok(json!({ "hex": value.map(|v| to_hex(&v)) }))
            }
            "x11.own_clipboard" => {
                let payload = bytes_of(request, "payload", "payload_hex");
                let formats: Vec<(String, Vec<u8>)> = request
                    .get("formats")
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .filter_map(Value::as_object)
                            .filter_map(|f| {
                                Some((text(f, "target")?.to_owned(), bytes_of(f, "text", "hex")?))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if payload.is_none() && formats.is_empty() {
                    return Err("x11.own_clipboard takes a payload or formats".into());
                }
                let owner =
                    SelectionOwner::offering(&display(request)?, &formats, payload.as_deref())?;
                let handle = self.handle();
                self.owners.insert(handle, owner);
                Ok(json!({ "handle": handle }))
            }
            "x11.owns" => {
                let handle = whole(request, "handle").ok_or("x11.owns takes a handle")?;
                let owner = self.owners.get(&handle).ok_or("no such owner")?;
                Ok(json!({ "owns": owner.owns(&display(request)?) }))
            }
            "x11.paint" => {
                let paints = request
                    .get("paints")
                    .and_then(Value::as_array)
                    .map(|list| list.iter().filter_map(paint).collect::<Vec<_>>())
                    .unwrap_or_default();
                let flicker = request.get("flicker").and_then(|f| {
                    Some((
                        paint(f.get("paint")?)?,
                        u32::try_from(f.get("second")?.as_u64()?).ok()?,
                        Duration::from_millis(f.get("period_ms")?.as_u64()?),
                    ))
                });
                let painting = x11::Painting::start(&display(request)?, &paints, flicker)?;
                let handle = self.handle();
                self.paintings.insert(handle, painting);
                Ok(json!({ "handle": handle }))
            }
            "wl.observe" => {
                let socket = text(request, "socket").ok_or("wl.observe takes a socket")?;
                let program = text(request, "program").unwrap_or("wl-observer");
                if !program
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                {
                    return Err(format!("{program} is no program of this crate"));
                }
                let bin = std::env::current_exe()
                    .map_err(|e| format!("this program's path: {e}"))?
                    .with_file_name(program);
                let env: Vec<(String, String)> = request
                    .get("env")
                    .and_then(Value::as_object)
                    .map(|env| {
                        env.iter()
                            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                            .collect()
                    })
                    .unwrap_or_default();
                let env: Vec<(&str, &str)> =
                    env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                let observer = WlObserver::start(&bin, socket, &private_dir("run"), &env);
                let handle = self.handle();
                self.observers.insert(handle, observer);
                Ok(json!({ "handle": handle }))
            }
            "wl.wait" => {
                let handle = whole(request, "handle").ok_or("wl.wait takes a handle")?;
                let Some(observer) = self.observers.get(&handle) else {
                    return Err(format!("no observer {handle}"));
                };
                let kind = text(request, "kind").ok_or("wl.wait takes a kind")?;
                let want = request
                    .get("match")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let within = timeout(request, Duration::from_secs(8));
                Ok(json!({ "event": observer.wait_for_match(kind, &want, within).await }))
            }
            "wl.output" => {
                let handle = whole(request, "handle").ok_or("wl.output takes a handle")?;
                let observer = self
                    .observers
                    .get(&handle)
                    .ok_or_else(|| format!("no observer {handle}"))?;
                Ok(json!({ "lines": observer.output() }))
            }
            "wl.lines" => {
                let handle = whole(request, "handle").ok_or("wl.lines takes a handle")?;
                let observer = self
                    .observers
                    .get(&handle)
                    .ok_or_else(|| format!("no observer {handle}"))?;
                let from = whole(request, "from")
                    .and_then(|f| usize::try_from(f).ok())
                    .unwrap_or(0);
                Ok(json!({ "lines": observer.lines_from(from) }))
            }
            "xvfb.start" => {
                let size = |field: &str, default: u32| {
                    whole(request, field)
                        .and_then(|v| u32::try_from(v).ok())
                        .unwrap_or(default)
                };
                let (width, height) = (size("width", 1280), size("height", 720));
                let extra: Vec<String> = request
                    .get("extra")
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
                let binary = text(request, "binary").unwrap_or("Xvfb");
                let xvfb = Xvfb::start_binary(binary, width, height, &extra)?;
                let display = xvfb.display().to_owned();
                let handle = self.handle();
                self.servers.insert(handle, xvfb);
                Ok(json!({ "display": display, "handle": handle }))
            }
            "release" => {
                let handle = whole(request, "handle").ok_or("release takes a handle")?;
                self.owners.remove(&handle);
                self.observers.remove(&handle);
                self.paintings.remove(&handle);
                self.servers.remove(&handle);
                self.loads.remove(&handle);
                self.watches.remove(&handle);
                self.churns.remove(&handle);
                self.probes.remove(&handle);
                Ok(json!({}))
            }
            "unix.stale" => {
                let path = text(request, "path").ok_or("unix.stale takes a path")?;
                let _ = std::fs::remove_file(path);
                // A listener bound and dropped leaves its socket file behind, as a dead server does.
                std::os::unix::net::UnixListener::bind(path).map_err(|e| format!("{path}: {e}"))?;
                Ok(json!({}))
            }
            "threads" => {
                let pid = whole(request, "pid")
                    .and_then(|p| u32::try_from(p).ok())
                    .ok_or("threads takes a pid")?;
                Ok(json!({ "threads": load::threads(pid) }))
            }
            "load.start" => {
                let threads: Vec<i32> = request
                    .get("threads")
                    .and_then(Value::as_array)
                    .ok_or("load.start takes threads")?
                    .iter()
                    .filter_map(|t| t.as_i64().and_then(|t| i32::try_from(t).ok()))
                    .collect();
                let stall = request
                    .get("stall")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let handle = self.handle();
                self.loads.insert(handle, Load::start(&threads, stall));
                Ok(json!({ "handle": handle }))
            }
            "load.resume" | "load.pause" => {
                let handle = whole(request, "handle").ok_or("a load op takes a handle")?;
                let held = self.loads.get(&handle).ok_or("no such load")?;
                if op == "load.resume" {
                    held.resume();
                } else {
                    held.pause();
                }
                Ok(json!({}))
            }
            "listeners" => {
                let pid = request
                    .get("pid")
                    .and_then(Value::as_u64)
                    .ok_or("listeners takes a pid")?;
                Ok(json!({ "listeners": tcp_listeners(pid) }))
            }
            other => Err(format!("no operation {other}")),
        }
    }

    /// A new handle for something held until it is released.
    fn handle(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn running(&self) -> Result<&Server, String> {
        self.server
            .as_ref()
            .ok_or_else(|| "no server is running".to_owned())
    }
}

/// The TCP addresses process `pid` itself listens on: its socket descriptors matched against the
/// kernel's tables of listening sockets.
fn tcp_listeners(pid: u64) -> Vec<String> {
    let inodes: BTreeSet<String> = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|dir| {
            dir.filter_map(|fd| std::fs::read_link(fd.ok()?.path()).ok())
                .filter_map(|target| {
                    let target = target.to_string_lossy().into_owned();
                    let inode = target.strip_prefix("socket:[")?.strip_suffix(']')?;
                    Some(inode.to_owned())
                })
                .collect()
        })
        .unwrap_or_default();
    let mut found = Vec::new();
    for table in ["tcp", "tcp6"] {
        let Ok(rows) = std::fs::read_to_string(format!("/proc/{pid}/net/{table}")) else {
            continue;
        };
        for row in rows.lines().skip(1) {
            let cols: Vec<&str> = row.split_whitespace().collect();
            // local_address, rem_address, st (0A is LISTEN), ..., inode
            if let (Some(&local), Some(&"0A"), Some(&inode)) =
                (cols.get(1), cols.get(3), cols.get(9))
                && inodes.contains(inode)
            {
                found.push(format!("{table} {local}"));
            }
        }
    }
    found
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut host = Host::default();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Object(request)) => {
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                let op = text(&request, "op").unwrap_or_default().to_owned();
                match host.answer(&op, &request).await {
                    Ok(ok) => json!({ "id": id, "ok": ok }),
                    Err(error) => json!({ "id": id, "error": error }),
                }
            }
            Ok(_) => json!({ "id": null, "error": "a request is a JSON object" }),
            Err(e) => json!({ "id": null, "error": format!("not JSON: {e}") }),
        };
        let mut said = answer.to_string();
        said.push('\n');
        if out.write_all(said.as_bytes()).await.is_err() || out.flush().await.is_err() {
            break;
        }
    }
    host.owners.clear();
    host.observers.clear();
    host.paintings.clear();
    // The servers under test go first: one may be capturing a private X server.
    if let Some(running) = host.server.take() {
        running.stop().await;
    }
    host.servers.clear();
}
