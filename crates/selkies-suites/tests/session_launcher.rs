/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `selkies session` starts a whole session from one command and ends it whole.
//!
//! A platform starts the launcher with a port and expects a desktop behind it. The desktop is the
//! host's own: a session file under the XDG data directories, run as a display manager runs it,
//! which here is a stand-in that records the environment it was given and stays up. On the X11
//! backend the launcher starts an Xvfb of its own that admits its cookie alone, on the Wayland
//! backend the session nests in Selkies' compositor, and a nested compositor from the host
//! (labwc) runs there as it would on a desktop. Proved with a raw WebSocket client: the handshake
//! completes and video frames arrive on the launcher's port, painting every row of the display.
//! The stock Xvfb a distribution ships runs it as well as the images'. SIGTERM ends everything
//! the launcher started and removes its runtime directory, and a start script that exits leaves
//! the session running, as a Coder workspace's does.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio_websockets::{ClientBuilder, Limits, Message};

use selkies_suites::server::free_port;
use selkies_suites::{Checks, http, on_path, server_binary, workdir};

const HEIGHT: usize = 720;

/// The HTTP status of a GET on the launcher's port, or -1 when it is not answering.
async fn status(port: u16) -> i32 {
    http::request(port, "GET", "/api/health", &[], &[], Duration::from_secs(2))
        .await
        .map_or(-1, |r| i32::from(r.status))
}

/// An XDG data directory holding one session of `kind`: a stand-in that records its environment
/// to `record`, daemonizes a helper out of its process group as a desktop's agents do, orphans a
/// process that exits at once as a menu's double fork does, starts an agent that daemonizes and
/// hides its environment (recording its pid beside `record`), and stays up.
fn data_dir(root: &Path, kind: &str, record: &Path) -> PathBuf {
    let folder = root.join("share").join(kind);
    std::fs::create_dir_all(&folder).expect("the session directory");
    let agent = env!("CARGO_BIN_EXE_session-agent");
    let record = record.display();
    std::fs::write(
        folder.join("standin.desktop"),
        format!(
            "[Desktop Entry]\nName=Stand-in\nDesktopNames=StandIn;Test;\n\
             Exec=sh -c 'env > {record}; setsid sleep 3600 & sh -c \"true &\"; \
             {agent} {record}.agent; exec sleep 3600'\nType=Application\n"
        ),
    )
    .expect("the session file");
    root.join("share")
}

/// The environment the stand-in recorded, once it has.
async fn environ(path: &Path) -> BTreeMap<String, String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if std::fs::metadata(path).is_ok_and(|m| m.len() > 0) {
            tokio::time::sleep(Duration::from_millis(200)).await;
            return std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    BTreeMap::new()
}

/// The launcher on a free port, answering: its process, its port, and its pid. Detached, it is
/// started in the background by a shell that exits at once, and its pid read back.
async fn start(
    args: &[&str],
    env: &BTreeMap<String, String>,
    log: &Path,
    detached: bool,
) -> Result<(Child, u16, i32), String> {
    let port = free_port();
    let mut command: Vec<String> = vec![server_binary().display().to_string(), "session".into()];
    command.extend(args.iter().map(|a| (*a).to_owned()));
    command.push(format!("--port={port}"));
    command.push("--enable-basic-auth=false".into());
    let pidfile = PathBuf::from(format!("{}.pid", log.display()));
    if detached {
        let quoted: Vec<String> = command
            .iter()
            .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
            .collect();
        command = vec![
            "sh".into(),
            "-c".into(),
            format!(
                "nohup {} >> {} 2>&1 & echo $! > {}",
                quoted.join(" "),
                log.display(),
                pidfile.display()
            ),
        ];
    }
    let sink = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| e.to_string())?;
    let mut launcher = Command::new(&command[0]);
    launcher
        .args(&command[1..])
        .env_clear()
        .envs(env)
        .current_dir(workdir())
        .stdin(Stdio::null())
        .stdout(sink.try_clone().map_err(|e| e.to_string())?)
        .stderr(sink);
    // SAFETY: setsid is async-signal-safe and the closure touches nothing else.
    unsafe {
        launcher.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = launcher.spawn().map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if status(port).await == 200 {
            let pid = if detached {
                std::fs::read_to_string(&pidfile)
                    .ok()
                    .and_then(|p| p.trim().parse().ok())
                    .unwrap_or(-1)
            } else {
                i32::try_from(child.id()).unwrap_or(-1)
            };
            return Ok((child, port, pid));
        }
        if !detached && let Ok(Some(code)) = child.try_wait() {
            return Err(format!("the launcher exited {code}; see {}", log.display()));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(format!(
        "no answer on the launcher's port within 90 s; see {}",
        log.display()
    ))
}

/// The height a JPEG's frame header declares, walking its segments from the start of image; 0
/// when it has none.
fn jpeg_height(data: &[u8]) -> usize {
    let mut at = 2;
    while at + 9 <= data.len() && data[at] == 0xff {
        let marker = data[at + 1];
        if matches!(marker, 0xc0..=0xc2) {
            return usize::from(u16::from_be_bytes([data[at + 5], data[at + 6]]));
        }
        at += 2 + usize::from(u16::from_be_bytes([data[at + 2], data[at + 3]]));
    }
    0
}

/// How many of a display's `height` rows the `(y, rows)` stripes cover.
fn rows_covered(spans: &BTreeSet<(usize, usize)>, height: usize) -> usize {
    let mut covered = vec![false; height];
    for &(y, rows) in spans {
        for row in covered.iter_mut().take((y + rows).min(height)).skip(y) {
            *row = true;
        }
    }
    covered.iter().filter(|&&c| c).count()
}

/// What the data WebSocket on the launcher's port gave: its first message, the video messages
/// of a primary display, how many of its rows they paint, and an error.
#[derive(Debug, Default)]
struct Stream {
    first: Option<String>,
    frames: usize,
    rows: usize,
    error: Option<String>,
}

async fn stream(port: u16) -> Stream {
    let mut out = Stream::default();
    let uri = format!("ws://127.0.0.1:{port}/api/websockets");
    let mut spans = BTreeSet::new();
    let run = async {
        let (mut ws, _) = ClientBuilder::new()
            .uri(&uri)
            .map_err(|e| e.to_string())?
            .limits(Limits::unlimited())
            .connect()
            .await
            .map_err(|e| e.to_string())?;
        let first = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .map_err(|_| "no first message".to_owned())?
            .ok_or("closed")?
            .map_err(|e| e.to_string())?;
        out.first = Some(first.as_text().map_or_else(
            || {
                format!(
                    "{:?}",
                    &first.as_payload()[..first.as_payload().len().min(20)]
                )
            },
            str::to_owned,
        ));
        let settings = json!({
            "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": HEIGHT,
            "manual_resolution": false, "framerate": 60, "encoder": "jpeg", "video_crf": 25,
            "video_bitrate": 6000, "audio_bitrate": 128_000, "scaling_dpi": 96,
            "displayPosition": "right",
        });
        ws.send(Message::text(format!("SETTINGS,{settings}")))
            .await
            .map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && out.rows < HEIGHT {
            let Ok(next) = tokio::time::timeout(Duration::from_secs(1), ws.next()).await else {
                continue;
            };
            let message = next.ok_or("closed")?.map_err(|e| e.to_string())?;
            if !message.is_binary() {
                continue;
            }
            let m: &[u8] = message.as_payload();
            if m.len() < 7 {
                continue;
            }
            let rows = match m[0] {
                0x03 => jpeg_height(&m[6..]),
                0x04 if m.len() >= 12 => usize::from(u16::from_be_bytes([m[8], m[9]])),
                _ => continue,
            };
            out.frames += 1;
            spans.insert((usize::from(u16::from_be_bytes([m[4], m[5]])), rows));
            out.rows = rows_covered(&spans, HEIGHT);
        }
        Ok::<(), String>(())
    };
    if let Err(e) = run.await {
        out.error = Some(e.chars().take(120).collect());
    }
    out
}

fn proc_pids() -> Vec<i32> {
    std::fs::read_dir("/proc")
        .map(|d| {
            d.filter_map(Result::ok)
                .filter_map(|e| e.file_name().to_string_lossy().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn cmdline(pid: i32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|c| String::from_utf8_lossy(&c).replace('\0', " "))
        .unwrap_or_default()
}

/// The processes still carrying the session's runtime directory once twenty seconds pass, or
/// none sooner.
async fn leftovers(runtime: &Path) -> Vec<(i32, String)> {
    let marker = format!("XDG_RUNTIME_DIR={}", runtime.display()).into_bytes();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let found: Vec<(i32, String)> = proc_pids()
            .into_iter()
            .filter(|pid| {
                std::fs::read(format!("/proc/{pid}/environ"))
                    .is_ok_and(|env| env.split(|&b| b == 0).any(|entry| entry == marker))
            })
            .map(|pid| (pid, cmdline(pid).chars().take(60).collect()))
            .collect();
        if found.is_empty() || Instant::now() > deadline {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// A process's state letter and parent, from its stat line.
fn stat(pid: i32) -> Option<(String, i32)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = text.rsplit_once(')')?;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.to_owned();
    let parent = fields.next()?.parse().ok()?;
    Some((state, parent))
}

/// The children of `parent` that exited and that it has not collected.
fn zombies(parent: i32) -> Vec<i32> {
    proc_pids()
        .into_iter()
        .filter(|&pid| stat(pid).is_some_and(|(s, p)| s == "Z" && p == parent))
        .collect()
}

/// Whether `pid` runs: neither gone nor exited.
fn running(pid: Option<i32>) -> bool {
    pid.and_then(stat).is_some_and(|(s, _)| s != "Z")
}

/// The rest of the first launcher line starting with `prefix`.
fn logged(log: &Path, prefix: &str) -> Option<String> {
    let head = format!("[selkies-session] {prefix}");
    let text = String::from_utf8_lossy(&std::fs::read(log).unwrap_or_default()).into_owned();
    text.lines()
        .find_map(|l| l.strip_prefix(&head).map(|rest| rest.trim().to_owned()))
}

/// `logged`, waiting up to `seconds` for the line: on Wayland the launcher starts the desktop
/// only once Selkies' compositor socket is up, which can be after its port answers.
async fn logged_within(log: &Path, prefix: &str, seconds: u64) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let found = logged(log, prefix);
        if found.is_some() || Instant::now() >= deadline {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn kill(pid: i32, signal: libc::c_int) -> bool {
    // SAFETY: a signal to a process this suite started or recorded.
    unsafe { libc::kill(pid, signal) == 0 }
}

/// SIGTERM to the launcher, returning once it has exited (it removes its runtime directory last,
/// after collecting its session); its exit status where this process started it.
async fn stop(child: &mut Child, pid: i32, detached: bool) -> Option<i32> {
    if !kill(pid, libc::SIGTERM) {
        return None;
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    if detached {
        while running(Some(pid)) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        return None;
    }
    while Instant::now() < deadline {
        if let Ok(Some(code)) = child.try_wait() {
            return code.code();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

fn wayland_sockets(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .map(|d| {
            d.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

fn private_dirs(runtime: &Path) -> Vec<String> {
    std::fs::read_dir(runtime)
        .map(|d| {
            d.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("selkies-session-"))
                .collect()
        })
        .unwrap_or_default()
}

/// A block's scratch directory, numbered rather than named for its tag: the session's sockets
/// sit beneath it, and a socket's path is held to the 108 bytes of `sun_path`.
fn block_root() -> PathBuf {
    static BLOCKS: AtomicUsize = AtomicUsize::new(0);
    workdir().join(format!(
        "session-{}-{}",
        BLOCKS.fetch_add(1, Ordering::Relaxed),
        std::process::id()
    ))
}

/// One launcher run and what it must hold.
struct Block<'a> {
    tag: &'a str,
    wayland: bool,
    session: &'a [&'a str],
    path_env: Option<String>,
    detached: bool,
    sound: Option<&'a str>,
}

async fn block(checks: &mut Checks, b: Block<'_>) {
    let tag = b.tag;
    let root = block_root();
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("the block's directory");
    let record = root.join("session.env");
    let runtime = root.join("run");
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&runtime)
            .expect("the runtime directory");
    }
    let log = root.join("launcher.log");
    let kind = if b.wayland {
        "wayland-sessions"
    } else {
        "xsessions"
    };
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    env.insert(
        "PATH".into(),
        b.path_env
            .clone()
            .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default()),
    );
    env.insert("HOME".into(), std::env::var("HOME").unwrap_or_default());
    env.insert("XDG_RUNTIME_DIR".into(), runtime.display().to_string());
    env.insert(
        "SELKIES_WAYLAND".into(),
        if b.wayland { "true" } else { "false" }.into(),
    );
    env.insert(
        "XDG_DATA_DIRS".into(),
        data_dir(&root, kind, &record).display().to_string(),
    );
    env.insert(
        "XDG_DATA_HOME".into(),
        root.join("home-share").display().to_string(),
    );
    for name in [
        "PYTHONWARNINGS",
        "CUDA_VISIBLE_DEVICES",
        "CUDA_DEVICE_ORDER",
    ] {
        if let Ok(value) = std::env::var(name) {
            env.insert(name.into(), value);
        }
    }
    let (mut child, port, pid) = match start(b.session, &env, &log, b.detached).await {
        Ok(started) => started,
        Err(e) => {
            checks.check(&format!("{tag} the launcher answers on its port"), false, e);
            return;
        }
    };
    checks.check(
        &format!("{tag} the launcher answers on its port"),
        true,
        port,
    );
    let private = private_dirs(&runtime);
    let session_dir = private
        .first()
        .map_or_else(|| runtime.clone(), |p| runtime.join(p));
    let mut agent = None;
    if b.detached {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut code = None;
        while Instant::now() < deadline {
            if let Ok(Some(done)) = child.try_wait() {
                code = done.code();
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        checks.check(
            &format!("{tag} the launcher outlives the script that started it"),
            code == Some(0) && status(port).await == 200,
            format!("{code:?}"),
        );
    }
    checks.check(
        &format!("{tag} a runtime directory of the session's own"),
        private.len() == 1,
        format!("{private:?}"),
    );
    let server = if logged(&log, "pipewire serves").is_some() {
        Some("pipewire")
    } else if logged(&log, "pulseaudio serves").is_some() {
        Some("pulseaudio")
    } else {
        None
    };
    checks.check(
        &format!("{tag} a sound server of the session's own"),
        server == b.sound && session_dir.join("pulse/native").exists(),
        format!("{server:?} {:?}", b.sound),
    );
    if b.session.is_empty() {
        let seen = environ(&record).await;
        let get = |k: &str| seen.get(k).map(String::as_str);
        checks.check(
            &format!("{tag} the installed session runs, as a display manager runs it"),
            get("XDG_SESSION_TYPE") == Some(if b.wayland { "wayland" } else { "x11" })
                && get("XDG_CURRENT_DESKTOP") == Some("StandIn:Test")
                && get("DESKTOP_SESSION") == Some("standin"),
            format!(
                "{:?}",
                ["XDG_SESSION_TYPE", "XDG_CURRENT_DESKTOP", "DESKTOP_SESSION"].map(get)
            ),
        );
        if b.wayland {
            let sockets = wayland_sockets(&session_dir);
            checks.check(
                &format!("{tag} the session draws on Selkies' compositor"),
                get("WAYLAND_DISPLAY").is_some_and(|w| sockets.iter().any(|s| s == w))
                    && get("DISPLAY").is_none(),
                format!("{:?} {sockets:?}", get("WAYLAND_DISPLAY")),
            );
        } else {
            let display = get("DISPLAY").unwrap_or("");
            let auth = get("XAUTHORITY").unwrap_or("");
            checks.check(
                &format!("{tag} the session draws on the launcher's Xvfb, with its cookie"),
                display.starts_with(':')
                    && Path::new(&format!("/tmp/.X11-unix/X{}", &display[1..])).exists()
                    && Path::new(auth).is_file(),
                format!("{display} {auth}"),
            );
            let name = format!("{tag} the Xvfb refuses a client without the session's cookie");
            if let Some(probe) = on_path("xdpyinfo") {
                let answers = |cookie: &str| {
                    Command::new(&probe)
                        .args(["-display", display])
                        .env_clear()
                        .env("XAUTHORITY", cookie)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success())
                };
                let (anyone, cookie) = (answers("/dev/null"), answers(auth));
                checks.check(
                    &name,
                    !anyone && cookie,
                    format!("anyone={anyone} cookie={cookie}"),
                );
            } else {
                checks.skip(&name, "no xdpyinfo");
            }
        }
    } else {
        // Started on a Wayland socket, not on an X display: a launcher that moved the session to
        // X11 runs the compositor as an X11 client and nests nothing.
        let nested = logged_within(&log, "desktop ", 20).await;
        checks.check(
            &format!("{tag} the host's compositor nests in Selkies' compositor"),
            nested
                .as_deref()
                .is_some_and(|n| n.contains(" on wayland-")),
            format!("{nested:?}"),
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut sockets = Vec::new();
        while Instant::now() < deadline && sockets.len() < 2 {
            sockets = wayland_sockets(&session_dir);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        checks.check(
            &format!("{tag} the nested compositor offers its own socket beside Selkies'"),
            sockets.len() >= 2,
            format!("{sockets:?}"),
        );
    }
    let got = stream(port).await;
    checks.check(
        &format!("{tag} the data WebSocket handshake completes on the launcher's port"),
        got.first.as_deref().is_some_and(|f| f.starts_with("MODE")),
        format!("{got:?}"),
    );
    // A still screen streams its first paint and then nothing: as many stripes as the host has
    // cores to cut, and a paint-over pass only where one is on, so it is the rows painted that
    // say the picture arrived.
    checks.check(
        &format!("{tag} video frames flow"),
        got.rows == HEIGHT,
        format!("{got:?}"),
    );
    let agent_file = PathBuf::from(format!("{}.agent", record.display()));
    if b.session.is_empty() {
        let deadline = Instant::now() + Duration::from_secs(5);
        while (!zombies(pid).is_empty() || !agent_file.exists()) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        agent = std::fs::read_to_string(&agent_file)
            .ok()
            .and_then(|p| p.trim().parse::<i32>().ok());
        checks.check(
            &format!("{tag} the launcher collects what the session orphans, once it exits"),
            agent.is_some() && zombies(pid).is_empty(),
            format!("{agent:?} {:?}", zombies(pid)),
        );
    }
    let code = stop(&mut child, pid, b.detached).await;
    let left = leftovers(&session_dir).await;
    let mut survived = running(agent);
    if survived {
        tokio::time::sleep(Duration::from_secs(2)).await;
        survived = running(agent);
    }
    for leftover in left
        .iter()
        .map(|(p, _)| *p)
        .chain(agent.filter(|_| survived))
    {
        kill(leftover, libc::SIGKILL);
    }
    let _ = child.try_wait();
    if !b.detached {
        checks.check(
            &format!("{tag} SIGTERM ends the launcher with status 0"),
            code == Some(0),
            format!("{code:?}"),
        );
    }
    checks.check(
        &format!("{tag} SIGTERM ends everything the launcher started"),
        left.is_empty() && status(port).await == -1,
        format!("{:?}", &left[..left.len().min(3)]),
    );
    if let Some(agent) = agent {
        checks.check(
            &format!(
                "{tag} and an agent that daemonized and hid its environment, as ssh-agent does"
            ),
            !survived,
            agent,
        );
    }
    let still = private_dirs(&runtime);
    checks.check(
        &format!("{tag} and removes its runtime directory"),
        still.is_empty(),
        format!("{still:?}"),
    );
}

/// The sound server the launcher takes on this host, and a PATH on which a failing
/// pipewire-pulse leaves it PulseAudio, where both are installed.
fn sound_servers() -> (Option<&'static str>, Option<String>) {
    let pipewire = on_path("pipewire").is_some()
        && on_path("pipewire-pulse").is_some()
        && (on_path("wireplumber").is_some() || on_path("pipewire-media-session").is_some());
    let pulse = on_path("pulseaudio").is_some();
    let default = if pipewire {
        Some("pipewire")
    } else if pulse {
        Some("pulseaudio")
    } else {
        None
    };
    if !(pipewire && pulse) {
        return (default, None);
    }
    let bin = workdir().join(format!("no-pipewire-pulse-{}", std::process::id()));
    selkies_suites::script(&bin.join("pipewire-pulse"), "#!/bin/sh\nexit 1\n");
    (default, Some(selkies_suites::path_with(&bin)))
}

/// A PATH whose Xvfb is the distribution's own, where one is installed apart from the first on
/// PATH.
fn stock_xvfb() -> Option<String> {
    let stock = Path::new("/usr/bin/Xvfb");
    let first = on_path("Xvfb").and_then(|p| p.canonicalize().ok());
    if !stock.exists() || first == stock.canonicalize().ok() {
        return None;
    }
    let bin = workdir().join(format!("stock-xvfb-{}", std::process::id()));
    std::fs::create_dir_all(&bin).ok()?;
    let _ = std::fs::remove_file(bin.join("Xvfb"));
    std::os::unix::fs::symlink(stock, bin.join("Xvfb")).ok()?;
    Some(selkies_suites::path_with(&bin))
}

#[tokio::test]
#[ignore = "needs a built selkies server, Xvfb, and a sound server"]
async fn a_session_starts_and_ends_whole() {
    let mut checks = Checks::new("session-launcher");
    let (sound, fallback) = sound_servers();
    let plain = |tag, wayland, session, path_env, detached, sound| Block {
        tag,
        wayland,
        session,
        path_env,
        detached,
        sound,
    };
    block(&mut checks, plain("[x11]", false, &[], None, false, sound)).await;
    if let Some(path) = fallback {
        block(
            &mut checks,
            plain(
                "[x11 PulseAudio past a failing PipeWire]",
                false,
                &[],
                Some(path),
                false,
                Some("pulseaudio"),
            ),
        )
        .await;
    } else {
        checks.skip(
            "[x11 PulseAudio past a failing PipeWire]",
            "PipeWire and PulseAudio are not both installed",
        );
    }
    if let Some(path) = stock_xvfb() {
        block(
            &mut checks,
            plain("[x11 stock Xvfb]", false, &[], Some(path), false, sound),
        )
        .await;
    } else {
        checks.skip(
            "[x11 stock Xvfb]",
            "the Xvfb on PATH is the distribution's own",
        );
    }
    block(
        &mut checks,
        plain("[wayland]", true, &[], None, false, sound),
    )
    .await;
    if on_path("labwc").is_some() {
        block(
            &mut checks,
            plain(
                "[wayland labwc]",
                true,
                &["--session", "labwc"],
                None,
                false,
                sound,
            ),
        )
        .await;
    } else {
        checks.skip("[wayland labwc]", "labwc is not installed");
    }
    block(
        &mut checks,
        plain("[detached]", false, &[], None, true, sound),
    )
    .await;
    checks.finish("");
}
