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
//! | `server.start` | `label`, `mode`, `wayland`, `display`, `socket`, `env` | `port`, `pid`, `log` |
//! | `server.stop` | | `tail`: the end of its log |
//! | `server.mark` | | `mark`: the log's length now |
//! | `server.log` | `from`: a mark | `text`: the log from it on |
//! | `listeners` | `pid` | `listeners`: the TCP addresses the process listens on |
//!
//! A new `server.start` stops the server before it, as one suite runs one server at a time. The
//! program ends when its standard input does, which is when the suite's process does however it
//! ends, and takes its server along; a server is tied to this program's thread as well, so not
//! even a kill of this program leaves one running.

use std::collections::BTreeSet;
use std::path::Path;

use selkies_suites::{Server, server};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// What the program holds between requests.
#[derive(Default)]
struct Host {
    server: Option<Server>,
}

/// `field` of `request` as a string, if it is one.
fn text<'a>(request: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    request.get(field).and_then(Value::as_str)
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
    if let Some(running) = host.server.take() {
        running.stop().await;
    }
}
