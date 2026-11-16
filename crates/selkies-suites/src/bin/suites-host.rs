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
//! | `server.mark` | | `mark`: the log's length now |
//! | `server.log` | `from`: a mark | `text`: the log from it on |
//! | `server.saw` | `from`, `needle`, `timeout_ms` | `saw`: whether the log took `needle` past `from` in time |
//! | `listeners` | `pid` | `listeners`: the TCP addresses the process listens on |
//! | `x11.key_held` | `keysym`, `display` | `held`: whether its key is down, null without a keycode |
//! | `x11.pointer` | `display` | `at`: `[x, y]` on the root |
//! | `x11.root_size` | `display` | `size`: `[width, height]` |
//! | `x11.window_manager` | `display` | `manager`: its `pid` (0 unknown) and `name`, null for none |
//! | `x11.clipboard` | `timeout_ms`, `display` | `text`: CLIPBOARD's text, null when none comes |
//! | `x11.own_clipboard` | `payload`, `display` | `handle` of an owner serving `payload` |
//! | `x11.paint` | `paints`, `flicker`, `display` | `handle` of solid windows: each `[x, y, w, h, rgb]`, the flicker `{paint, second, period_ms}` |
//! | `wl.observe` | `socket`, `env` | `handle` of a `wl-observer` on the server's compositor |
//! | `wl.wait` | `handle`, `kind`, `match`, `timeout_ms` | `event`: the first of `kind` matching, or null |
//! | `release` | `handle` | an owner or observer ended |
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

use selkies_suites::wayland::WlObserver;
use selkies_suites::x11::{self, SelectionOwner};
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
    next: u64,
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
            "x11.own_clipboard" => {
                let payload =
                    text(request, "payload").ok_or("x11.own_clipboard takes a payload")?;
                let owner = SelectionOwner::serving(&display(request)?, payload.as_bytes());
                let handle = self.handle();
                self.owners.insert(handle, owner);
                Ok(json!({ "handle": handle }))
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
                let bin = std::env::current_exe()
                    .map_err(|e| format!("this program's path: {e}"))?
                    .with_file_name("wl-observer");
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
            "release" => {
                let handle = whole(request, "handle").ok_or("release takes a handle")?;
                self.owners.remove(&handle);
                self.observers.remove(&handle);
                self.paintings.remove(&handle);
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
    if let Some(running) = host.server.take() {
        running.stop().await;
    }
}
