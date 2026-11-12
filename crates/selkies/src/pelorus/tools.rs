/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The tools of `pelorus::tools` carried out on the running session.
//!
//! [`call`] is the one way in, for a model's loop and for a caller of the API alike, so both
//! get the same session through the same rules. A tool that reads asks the window manager and
//! the accessibility bus; one that acts goes to the Computer Use backend in process
//! (`pixelflux::computer_use::act`), the same injection a Computer Use client gets, and
//! answers with what its action changed among the windows, read a moment after it so a window
//! the action opens is there to be seen. `look` at a window activates it first: a control it
//! lists is then one a click reaches, not one under another window. Calls run one at a time,
//! since two callers moving one pointer would each be told of the other's changes.
//!
//! `look` numbers the controls it lists and this module keeps where each number points
//! ([`References`]); the pointer tools take the number. Numbers are never reused, so one from
//! an earlier look cannot land on a different control: looking at a window again retires the
//! numbers its last look gave, and a retired number is refused with a sentence saying to look
//! again, which is what a caller holding a stale list needs to hear.
//!
//! `shell` runs a command as the session's user in the session's environment, and exists only
//! where the server lets a controller run commands at all (`command_enabled`), the setting the
//! hardened images turn off: Pelorus is never a way around it. Its output is cut to a bound,
//! keeping the end, and it is ended at its timeout.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use ::pelorus::summary::{self, WindowLine};
use ::pelorus::tools::{Capabilities, definitions};
use ::pelorus::{Rect as PxRect, outline};
use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use pixelflux::wm::{DesktopView, Request, WindowType};
use selkies_draw::apps::{self, Apps, Entry};
use serde_json::{Value as Json, json};
use tokio::io::AsyncReadExt as _;

use super::{OUTLINE_LINES, px_rect, read_window, refused, scale_at};
use crate::http::auth::Ceiling;
use crate::http::routes::json_error;
use crate::settings::settings;

/// How long after an action the windows are first read for what it changed, how often again
/// until two reads agree, and the longest that is waited for.
const SETTLE: Duration = Duration::from_millis(300);
const SETTLE_AGAIN: Duration = Duration::from_millis(200);
const SETTLE_LONGEST: Duration = Duration::from_millis(1500);

/// How long a launch waits for the application's window.
const LAUNCH_WAIT: Duration = Duration::from_secs(6);

/// The most characters of a command's output returned, its end kept.
const SHELL_OUTPUT: usize = 6000;

/// The default and the longest a command may run, in seconds.
const SHELL_TIMEOUT: u64 = 30;
const SHELL_TIMEOUT_MAX: u64 = 300;

/// What a tool answers: text for the model, a PNG where it asked for a picture, and whether
/// the call failed.
#[derive(Debug, Default)]
pub struct Output {
    pub text: String,
    pub image: Option<Vec<u8>>,
    pub failed: bool,
}

impl Output {
    fn ok(text: impl Into<String>) -> Output {
        Output {
            text: text.into(),
            ..Output::default()
        }
    }

    fn fail(text: impl Into<String>) -> Output {
        Output {
            text: text.into(),
            failed: true,
            ..Output::default()
        }
    }
}

/// Where the reference numbers `look` gave out point.
#[derive(Default)]
struct References {
    /// The next number to give out.
    next: u32,
    /// Each window's current numbers: the first, and the points in order.
    current: BTreeMap<u32, (u32, Vec<(i32, i32)>)>,
}

impl References {
    /// The first number of a new look at `window`, which retires the window's earlier ones.
    fn begin(&mut self, window: u32) -> u32 {
        self.current.remove(&window);
        self.next.max(1)
    }

    /// Keep the points a look at `window` numbered from `first`.
    fn keep(&mut self, window: u32, first: u32, points: Vec<(i32, i32)>) {
        self.next = first + points.len() as u32;
        self.current.insert(window, (first, points));
    }

    /// The point a number stands for, or the sentence a caller is told.
    fn point(&self, reference: u32) -> Result<(i64, i64), String> {
        for (first, points) in self.current.values() {
            if let Some((x, y)) = reference
                .checked_sub(*first)
                .and_then(|index| points.get(index as usize))
            {
                return Ok((i64::from(*x), i64::from(*y)));
            }
        }
        Err(if reference < self.next {
            format!(
                "Reference e{reference} is from an earlier look and its window has changed or been read \
                 again since. Look at the window again and use the references from that list."
            )
        } else {
            format!(
                "There is no reference e{reference}. Use one written in square brackets by `look`."
            )
        })
    }
}

static REFERENCES: Mutex<References> = Mutex::new(References {
    next: 1,
    current: BTreeMap::new(),
});

/// One tool call at a time.
static ACTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What this session offers a model that does or does not take images.
pub async fn capabilities(vision: bool) -> Capabilities {
    let windows = tokio::task::spawn_blocking(|| crate::wm::desktop_view().is_some())
        .await
        .unwrap_or(false);
    Capabilities {
        shell: settings().command_enabled().on,
        vision,
        windows,
    }
}

/// The windows a model is told of, top of the stack first: the mapped ones that are not the
/// desktop's own surfaces or passing decorations.
fn window_lines(view: &DesktopView) -> Vec<WindowLine> {
    view.windows
        .iter()
        .rev()
        .filter(|w| w.mapped)
        .filter(|w| {
            !matches!(
                w.info.window_type,
                WindowType::Desktop
                    | WindowType::Dock
                    | WindowType::Tooltip
                    | WindowType::Dnd
                    | WindowType::Notification
            )
        })
        .map(|w| {
            let scale = scale_at(&view.outputs, w.rect.x, w.rect.y);
            let title = if w.info.title.is_empty() {
                format!("({})", format!("{:?}", w.info.window_type).to_lowercase())
            } else {
                w.info.title.clone()
            };
            WindowLine {
                id: w.info.id,
                title,
                app: w.info.app_id.clone(),
                rect: px_rect(w.rect.grown(w.frame), scale),
                focused: view.focus == Some(w.info.id),
                minimized: w.states.minimized,
                maximized: w.states.maximized,
                fullscreen: w.states.fullscreen,
                elsewhere: w.states.hidden,
            }
        })
        .collect()
}

/// The windows now, and the size of the screen they are on; `None` with no window manager.
async fn desktop() -> Option<(Vec<WindowLine>, (i32, i32))> {
    let view = tokio::task::spawn_blocking(crate::wm::desktop_view)
        .await
        .ok()??;
    let screen = view.outputs.iter().fold((0, 0), |size, o| {
        let rect: PxRect = px_rect(o.rect, o.scale);
        (size.0.max(rect.x + rect.w), size.1.max(rect.y + rect.h))
    });
    Some((window_lines(&view), screen))
}

/// The windows once they stop changing after an action: a dialog an action closes, or the
/// window it opens, takes the application a moment, so the list is read until two reads in a
/// row agree, within a bound, and for longer while it still reads as it did `before`, since
/// "nothing changed" and "nothing changed yet" look the same at first.
async fn settled(before: &[WindowLine]) -> Option<Vec<WindowLine>> {
    tokio::time::sleep(SETTLE).await;
    let started = tokio::time::Instant::now();
    let mut last = desktop().await?.0;
    let mut agreed = 0;
    while started.elapsed() < SETTLE_LONGEST {
        tokio::time::sleep(SETTLE_AGAIN).await;
        let now = desktop().await?.0;
        agreed = if now == last { agreed + 1 } else { 0 };
        last = now;
        if agreed >= if last.as_slice() == before { 3 } else { 1 } {
            break;
        }
    }
    Some(last)
}

/// One Computer Use action, its reply's error where it has one.
async fn computer(action: Json) -> Result<Json, String> {
    let body = action.to_string();
    let reply = tokio::task::spawn_blocking(move || pixelflux::computer_use::act(&body))
        .await
        .map_err(|e| e.to_string())?;
    let reply: Json = serde_json::from_str(&reply).map_err(|e| e.to_string())?;
    match reply.get("error").and_then(Json::as_str) {
        Some(error) => Err(error.to_string()),
        None => Ok(reply),
    }
}

/// Carry out input actions in order and answer with what they changed among the windows.
async fn act(actions: Vec<Json>) -> Output {
    let before = desktop().await;
    for action in actions {
        if let Err(e) = computer(action).await {
            return Output::fail(format!("The action failed: {e}"));
        }
    }
    let Some((before, _)) = before else {
        return Output::ok("Done.");
    };
    let Some(after) = settled(&before).await else {
        return Output::ok("Done.");
    };
    let changed = summary::changes(&before, &after);
    if changed.is_empty() {
        Output::ok("Done. No window opened, closed, or changed title or focus.")
    } else {
        Output::ok(format!("Done.\n{changed}"))
    }
}

/// A `[x, y]` argument.
fn point(args: &Json, name: &str) -> Result<(i64, i64), String> {
    let pair = args.get(name).and_then(Json::as_array);
    match pair.map(|p| {
        (
            p.first().and_then(Json::as_i64),
            p.get(1).and_then(Json::as_i64),
        )
    }) {
        Some((Some(x), Some(y))) => Ok((x, y)),
        _ => Err(format!("`{name}` must be a screen point [x, y]")),
    }
}

/// Where a pointer tool acts: the control `reference` names, else the point `at` names.
fn target(args: &Json, reference: &str, at: &str) -> Result<(i64, i64), String> {
    match args.get(reference).filter(|v| !v.is_null()) {
        Some(number) => {
            let number = match number {
                Json::String(text) => text
                    .trim()
                    .trim_start_matches(['e', 'E', '['])
                    .trim_end_matches(']')
                    .parse()
                    .ok(),
                other => other.as_u64().and_then(|n| u32::try_from(n).ok()),
            }
            .ok_or_else(|| format!("`{reference}` must be a reference from `look`, like e41"))?;
            REFERENCES
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .point(number)
        }
        None if args.get(at).is_some_and(|v| !v.is_null()) => point(args, at),
        None => Err(format!(
            "Name the control with `{reference}`, its reference from `look`."
        )),
    }
}

/// A window number argument.
fn window_id(args: &Json) -> Result<u32, String> {
    args.get("window")
        .and_then(Json::as_u64)
        .and_then(|id| u32::try_from(id).ok())
        .ok_or_else(|| "`window` must be a window number from the window list".to_string())
}

async fn look(args: &Json) -> Output {
    let Some(id) = args.get("window").filter(|v| !v.is_null()) else {
        return match desktop().await {
            Some((windows, screen)) => Output::ok(summary::write(screen, &windows)),
            None => {
                Output::fail("No window manager runs in this session, so windows cannot be listed.")
            }
        };
    };
    let Some(id) = id.as_u64().and_then(|id| u32::try_from(id).ok()) else {
        return Output::fail("`window` must be a window number from the window list");
    };
    let raised = tokio::task::spawn_blocking(move || {
        crate::wm::window_request(id, Request::Unminimize);
        crate::wm::window_request(id, Request::Activate)
    })
    .await
    .unwrap_or(false);
    if !raised {
        return Output::fail(format!(
            "There is no window #{id}. Look without arguments for the window list."
        ));
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    match tokio::task::spawn_blocking(move || read_window(id)).await {
        Ok(Ok(read)) => {
            let covered = |x: i32, y: i32| read.above.iter().any(|frame| frame.contains(x, y));
            let mut references = REFERENCES.lock().unwrap_or_else(|p| p.into_inner());
            let first = references.begin(id);
            let outline = outline::write(
                &read.tree,
                &outline::Options {
                    marks: outline::Marks::Refs { first },
                    clip: read.clip,
                    covered: &covered,
                    lines: OUTLINE_LINES,
                },
            );
            references.keep(id, first, outline.points);
            Output::ok(format!(
                "Window #{id} \"{}\":\n{}",
                read.title, outline.text
            ))
        }
        Ok(Err(why)) => Output::ok(why.answer().1),
        Err(_) => Output::fail("The window could not be read."),
    }
}

async fn click(args: &Json) -> Output {
    let (x, y) = match target(args, "ref", "at") {
        Ok(at) => at,
        Err(e) => return Output::fail(e),
    };
    let button = args.get("button").and_then(Json::as_str).unwrap_or("left");
    let count = args.get("count").and_then(Json::as_u64).unwrap_or(1);
    let action = match (button, count) {
        ("left", 1) => "left_click",
        ("left", 2) => "double_click",
        ("left", 3) => "triple_click",
        ("right", 1) => "right_click",
        ("middle", 1) => "middle_click",
        _ => {
            return Output::fail(
                "`button` is left, right, or middle, and only the left button takes a `count`",
            );
        }
    };
    act(vec![json!({"action": action, "coordinate": [x, y]})]).await
}

async fn type_text(args: &Json) -> Output {
    let Some(text) = args.get("text").and_then(Json::as_str) else {
        return Output::fail("`text` is required");
    };
    let flag = |name: &str| args.get(name).and_then(Json::as_bool).unwrap_or(false);
    let mut actions = Vec::new();
    if args.get("ref").is_some_and(|v| !v.is_null()) {
        match target(args, "ref", "at") {
            Ok((x, y)) => actions.push(json!({"action": "left_click", "coordinate": [x, y]})),
            Err(e) => return Output::fail(e),
        }
    }
    if flag("replace") {
        actions.push(json!({"action": "key", "text": "ctrl+a"}));
    }
    actions.push(json!({"action": "type", "text": text}));
    if flag("enter") {
        actions.push(json!({"action": "key", "text": "Return"}));
    }
    act(actions).await
}

async fn key(args: &Json) -> Output {
    match args.get("keys").and_then(Json::as_str) {
        Some(keys) if !keys.trim().is_empty() => {
            act(vec![json!({"action": "key", "text": keys})]).await
        }
        _ => Output::fail("`keys` is required"),
    }
}

async fn scroll(args: &Json) -> Output {
    let (x, y) = match target(args, "ref", "at") {
        Ok(at) => at,
        Err(e) => return Output::fail(e),
    };
    let Some(direction) = args
        .get("direction")
        .and_then(Json::as_str)
        .filter(|d| matches!(*d, "up" | "down" | "left" | "right"))
    else {
        return Output::fail("`direction` is up, down, left, or right");
    };
    let amount = args
        .get("amount")
        .and_then(Json::as_i64)
        .unwrap_or(3)
        .clamp(1, 50);
    act(vec![json!({
        "action": "scroll",
        "coordinate": [x, y],
        "scroll_direction": direction,
        "scroll_amount": amount,
    })])
    .await
}

async fn drag(args: &Json) -> Output {
    match (
        target(args, "from_ref", "from"),
        target(args, "to_ref", "to"),
    ) {
        (Ok(from), Ok(to)) => {
            act(vec![json!({
                "action": "left_click_drag",
                "start_coordinate": [from.0, from.1],
                "coordinate": [to.0, to.1],
            })])
            .await
        }
        (Err(e), _) | (_, Err(e)) => Output::fail(e),
    }
}

async fn window(args: &Json) -> Output {
    let id = match window_id(args) {
        Ok(id) => id,
        Err(e) => return Output::fail(e),
    };
    let request = match args.get("action").and_then(Json::as_str) {
        Some("close") => Request::Close,
        Some("minimize") => Request::Minimize,
        Some("maximize") => Request::Maximize,
        Some("unmaximize") => Request::Unmaximize,
        Some("restore") => Request::Unminimize,
        _ => return Output::fail("`action` is close, minimize, maximize, unmaximize, or restore"),
    };
    let before = desktop().await;
    let known = tokio::task::spawn_blocking(move || crate::wm::window_request(id, request))
        .await
        .unwrap_or(false);
    if !known {
        return Output::fail(format!("There is no window #{id}."));
    }
    let Some((before, _)) = before else {
        return Output::ok("Done.");
    };
    let Some(after) = settled(&before).await else {
        return Output::ok("Done.");
    };
    let changed = summary::changes(&before, &after);
    if changed.is_empty() {
        Output::ok(
            "The window manager took the request and no window changed yet. A window asked to close may be \
             asking a question first: look at the window list.",
        )
    } else {
        Output::ok(format!("Done.\n{changed}"))
    }
}

/// The entries a name picks: one whose id or name is the text, else the best-ranked of those
/// that match it.
fn pick<'a>(entries: &'a [Entry], wanted: &str) -> Vec<&'a Entry> {
    let needle = wanted.trim().to_lowercase();
    let id = needle.trim_end_matches(".desktop");
    let exact: Vec<&Entry> = entries
        .iter()
        .filter(|e| {
            e.id.trim_end_matches(".desktop").to_lowercase() == id
                || e.name.to_lowercase() == needle
        })
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    let matching: Vec<&Entry> = entries.iter().filter(|e| e.matches(&needle)).collect();
    let best = matching.iter().map(|e| e.rank(&needle)).min();
    matching
        .into_iter()
        .filter(|e| Some(e.rank(&needle)) == best)
        .collect()
}

/// The command line that starts an entry, as the window manager's applications menu builds it.
fn entry_command(entry: &Entry, terminal: Option<&Vec<String>>) -> String {
    let mut argv = apps::exec_argv(&entry.exec, entry.icon.as_deref(), &entry.name, &entry.path);
    if entry.terminal
        && let Some(terminal) = terminal
    {
        let mut wrapped = terminal.clone();
        wrapped.append(&mut argv);
        argv = wrapped;
    }
    let command = apps::shell_command(&argv);
    match &entry.working_dir {
        Some(dir) => format!(
            "cd {} && {command}",
            apps::shell_command(std::slice::from_ref(dir))
        ),
        None => command,
    }
}

async fn launch(args: &Json) -> Output {
    let wanted = args
        .get("app")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_string();
    let Some(env) = crate::wm::session_env() else {
        return Output::fail(
            "No window manager runs in this session, so applications cannot be started here.",
        );
    };
    let chosen = tokio::task::spawn_blocking(move || {
        let terminal = apps::find_terminal(None);
        let apps = Apps::load(terminal.is_some());
        let mut names: Vec<String> = apps.entries.iter().map(|e| e.name.clone()).collect();
        names.sort();
        names.dedup();
        if wanted.trim().is_empty() {
            return Err(format!("Installed applications:\n{}", names.join("\n")));
        }
        let picked = pick(&apps.entries, &wanted);
        match picked.as_slice() {
            [] => Err(format!(
                "No installed application matches \"{wanted}\". Installed applications:\n{}",
                names.join("\n")
            )),
            [entry] => Ok((entry.name.clone(), entry_command(entry, terminal.as_ref()))),
            several => {
                let mut matches: Vec<&str> = several.iter().map(|e| e.name.as_str()).collect();
                matches.dedup();
                if matches.len() == 1 {
                    Ok((
                        several[0].name.clone(),
                        entry_command(several[0], terminal.as_ref()),
                    ))
                } else {
                    Err(format!(
                        "\"{wanted}\" matches several applications; name one exactly:\n{}",
                        matches.join("\n")
                    ))
                }
            }
        }
    })
    .await;
    let (name, command) = match chosen {
        Ok(Ok(found)) => found,
        Ok(Err(listing)) => return Output::ok(listing),
        Err(_) => return Output::fail("The applications could not be read."),
    };
    let before = desktop()
        .await
        .map(|(windows, _)| windows)
        .unwrap_or_default();
    let mut child = tokio::process::Command::from(env.command("sh"));
    child
        .args(["-c", &command])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match child.spawn() {
        Ok(mut child) => {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        Err(e) => return Output::fail(format!("{name} could not be started: {e}")),
    }
    log::info!(target: "pelorus", "launched {name}");
    let deadline = tokio::time::Instant::now() + LAUNCH_WAIT;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let after = desktop()
            .await
            .map(|(windows, _)| windows)
            .unwrap_or_default();
        if after.iter().any(|w| !before.iter().any(|b| b.id == w.id)) {
            let after = settled(&after).await.unwrap_or(after);
            return Output::ok(format!(
                "Started {name}.\n{}",
                summary::changes(&before, &after)
            ));
        }
    }
    Output::ok(format!(
        "Started {name}; it has opened no window yet. Wait a moment and look at the window list."
    ))
}

/// The end of a command's output, cut to the bound on a character boundary.
fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let count = text.chars().count();
    if count <= SHELL_OUTPUT {
        return text.into_owned();
    }
    let kept: String = text.chars().skip(count - SHELL_OUTPUT).collect();
    format!(
        "… ({} earlier characters not shown)\n{kept}",
        count - SHELL_OUTPUT
    )
}

async fn shell(args: &Json) -> Output {
    if !settings().command_enabled().on {
        return Output::fail("Running commands is turned off in this session.");
    }
    let Some(command) = args
        .get("command")
        .and_then(Json::as_str)
        .filter(|c| !c.trim().is_empty())
    else {
        return Output::fail("`command` is required");
    };
    let seconds = args
        .get("timeout")
        .and_then(Json::as_u64)
        .unwrap_or(SHELL_TIMEOUT)
        .clamp(1, SHELL_TIMEOUT_MAX);
    let mut child = match crate::wm::session_env() {
        Some(env) => tokio::process::Command::from(env.command("sh")),
        None => tokio::process::Command::new("sh"),
    };
    child
        .args(["-c", command])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    log::info!(target: "pelorus", "shell: {command}");
    let mut child = match child.spawn() {
        Ok(child) => child,
        Err(e) => return Output::fail(format!("The command could not be started: {e}")),
    };
    let (mut stdout, mut stderr) = (child.stdout.take(), child.stderr.take());
    let run = async {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let read_out = async {
            if let Some(pipe) = stdout.as_mut() {
                let _ = pipe.read_to_end(&mut out).await;
            }
        };
        let read_err = async {
            if let Some(pipe) = stderr.as_mut() {
                let _ = pipe.read_to_end(&mut err).await;
            }
        };
        let (_, _, status) = tokio::join!(read_out, read_err, child.wait());
        (out, err, status)
    };
    let before = desktop().await;
    let result = tokio::time::timeout(Duration::from_secs(seconds), run).await;
    let mut text = match result {
        Ok((out, err, status)) => {
            let code = status
                .ok()
                .and_then(|s| s.code())
                .map_or("ended by a signal".to_string(), |c| {
                    format!("exit code {c}")
                });
            let mut text = format!("{code}\n");
            if !out.is_empty() {
                text.push_str(&format!("stdout:\n{}\n", tail(&out).trim_end()));
            }
            if !err.is_empty() {
                text.push_str(&format!("stderr:\n{}\n", tail(&err).trim_end()));
            }
            text
        }
        Err(_) => format!(
            "The command was still running after {seconds} seconds and was ended. Run a long-lived program \
             in the background with `&`, its output sent to a file.\n"
        ),
    };
    if let (Some((before, _)), Some((after, _))) = (before, desktop().await) {
        text.push_str(&summary::changes(&before, &after));
    }
    Output::ok(text)
}

async fn screenshot(args: &Json) -> Output {
    let Some(id) = args.get("window").filter(|v| !v.is_null()) else {
        return match tokio::task::spawn_blocking(|| pixelflux::screenshot_png(0)).await {
            Ok(Ok(png)) => Output {
                text: "The screen.".to_string(),
                image: Some(png),
                failed: false,
            },
            Ok(Err(e)) => Output::fail(format!("No screenshot: {e}")),
            Err(_) => Output::fail("No screenshot."),
        };
    };
    let Some(id) = id.as_u64() else {
        return Output::fail("`window` must be a window number from the window list");
    };
    let Some((windows, _)) = desktop().await else {
        return Output::fail(
            "No window manager runs in this session; take a screenshot of the screen.",
        );
    };
    let Some(window) = windows.iter().find(|w| u64::from(w.id) == id) else {
        return Output::fail(format!("There is no window #{id}."));
    };
    let rect = window.rect;
    let region = json!([
        rect.x.max(0),
        rect.y.max(0),
        rect.x + rect.w,
        rect.y + rect.h
    ]);
    match computer(json!({"action": "zoom", "region": region})).await {
        Ok(reply) => {
            let png = reply
                .get("data")
                .and_then(Json::as_str)
                .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok());
            match png {
                Some(png) => Output {
                    text: format!(
                        "Window #{id} as it shows on the screen. Its top-left corner is at screen point \
                         {},{} and it is {}x{}; a point in the picture may be scaled, so place it by its \
                         fraction of the window.",
                        rect.x, rect.y, rect.w, rect.h
                    ),
                    image: Some(png),
                    failed: false,
                },
                None => Output::fail("The screenshot came back empty."),
            }
        }
        Err(e) => Output::fail(format!("No screenshot: {e}")),
    }
}

/// Carry out one tool call. An unknown tool, or one this session does not offer, fails with a
/// sentence the model can act on.
pub async fn call(name: &str, args: &Json, capabilities: Capabilities) -> Output {
    if !definitions(capabilities)
        .iter()
        .any(|tool| tool.name == name)
    {
        return Output::fail(format!("There is no tool named `{name}` in this session."));
    }
    let _one = ACTING.lock().await;
    match name {
        "look" => look(args).await,
        "click" => click(args).await,
        "type" => type_text(args).await,
        "key" => key(args).await,
        "scroll" => scroll(args).await,
        "drag" => drag(args).await,
        "window" => window(args).await,
        "launch" => launch(args).await,
        "shell" => shell(args).await,
        "screenshot" => screenshot(args).await,
        "wait" => {
            let seconds = args
                .get("seconds")
                .and_then(Json::as_f64)
                .unwrap_or(1.0)
                .clamp(0.0, 30.0);
            tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
            Output::ok(format!("Waited {seconds} seconds."))
        }
        _ => Output::fail(format!("There is no tool named `{name}` in this session.")),
    }
}

/// `GET /pelorus/api/tools`: the tools this session offers and their argument schemas, for a
/// caller that brings its own model. `screenshot` is listed, since the caller decides whether
/// its model sees.
pub async fn handle_tools(ceiling: Ceiling) -> Response {
    if ceiling.is_viewer() {
        return refused();
    }
    let tools: Vec<Json> = definitions(capabilities(true).await)
        .into_iter()
        .map(|tool| json!({"name": tool.name, "description": tool.description, "parameters": tool.parameters}))
        .collect();
    axum::Json(json!({"tools": tools})).into_response()
}

/// `POST /pelorus/api/tool`: one tool call, `{"name": ..., "arguments": {...}}`, answered with
/// `{"text", "failed"}` and the picture as base64 PNG in `image` where the tool made one.
pub async fn handle_tool(ceiling: Ceiling, body: Bytes) -> Response {
    if ceiling.is_viewer() {
        return refused();
    }
    let Ok(request) = serde_json::from_slice::<Json>(&body) else {
        return json_error(StatusCode::BAD_REQUEST, "the body must be JSON");
    };
    let Some(name) = request.get("name").and_then(Json::as_str) else {
        return json_error(StatusCode::BAD_REQUEST, "`name` is required");
    };
    let args = request
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let output = call(name, &args, capabilities(true).await).await;
    let image = output
        .image
        .map(|png| base64::engine::general_purpose::STANDARD.encode(png));
    axum::Json(json!({"text": output.text, "failed": output.failed, "image": image}))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(id: &str, name: &str) -> Entry {
        Entry {
            id: id.to_string(),
            name: name.to_string(),
            comment: None,
            generic_name: None,
            keywords: Vec::new(),
            exec: id.to_string(),
            icon: None,
            path: PathBuf::from("/tmp"),
            terminal: false,
            emulator: false,
            working_dir: None,
            category: String::new(),
            wm_class: None,
        }
    }

    #[test]
    fn an_application_is_picked_by_its_exact_name_before_a_partial_one() {
        let entries = [
            entry("org.gnome.Calculator.desktop", "Calculator"),
            entry("calc-pro", "Calculator Pro"),
        ];
        let picked = pick(&entries, "calculator");
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].name, "Calculator");
        assert_eq!(pick(&entries, "org.gnome.Calculator")[0].name, "Calculator");
        assert_eq!(pick(&entries, "pro")[0].name, "Calculator Pro");
        assert!(pick(&entries, "editor").is_empty());
    }

    #[test]
    fn a_long_output_keeps_its_end() {
        let long = "a".repeat(SHELL_OUTPUT) + "END";
        let cut = tail(long.as_bytes());
        assert!(cut.ends_with("END"));
        assert!(cut.starts_with("… (3 earlier characters not shown)"));
        assert_eq!(tail(b"short"), "short");
    }

    #[test]
    fn a_reference_points_where_its_look_put_it_until_the_window_is_read_again() {
        let mut references = References::default();
        let first = references.begin(7);
        assert_eq!(first, 1);
        references.keep(7, first, vec![(10, 20), (30, 40)]);
        let other = references.begin(9);
        assert_eq!(other, 3);
        references.keep(9, other, vec![(50, 60)]);
        assert_eq!(references.point(2), Ok((30, 40)));
        assert_eq!(references.point(3), Ok((50, 60)));
        let again = references.begin(7);
        references.keep(7, again, vec![(11, 21)]);
        assert_eq!(references.point(4), Ok((11, 21)));
        assert!(references.point(2).unwrap_err().contains("earlier look"));
        assert!(references.point(99).unwrap_err().contains("no reference"));
    }

    #[test]
    fn a_reference_is_read_as_look_writes_it() {
        let mut references = REFERENCES.lock().unwrap_or_else(|p| p.into_inner());
        let first = references.begin(4242);
        references.keep(4242, first, vec![(7, 9)]);
        drop(references);
        for written in [
            json!(format!("e{first}")),
            json!(format!("[e{first}]")),
            json!(first),
        ] {
            assert_eq!(target(&json!({"ref": written}), "ref", "at"), Ok((7, 9)));
        }
        assert!(target(&json!({"ref": "seven"}), "ref", "at").is_err());
        assert_eq!(target(&json!({"at": [3, 4]}), "ref", "at"), Ok((3, 4)));
        assert!(target(&json!({}), "ref", "at").is_err());
    }

    #[test]
    fn a_point_argument_is_two_integers() {
        assert_eq!(point(&json!({"at": [3, 4]}), "at"), Ok((3, 4)));
        assert!(point(&json!({"at": [3]}), "at").is_err());
        assert!(point(&json!({}), "at").is_err());
    }
}
