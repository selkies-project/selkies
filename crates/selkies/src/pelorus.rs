/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Pelorus: the desktop described for a caller that cannot see it, served under `/pelorus`.
//!
//! A model without vision, or a script, drives a session from text: which windows are open,
//! where each one is, and where to click. The window manager is the one source of that
//! ([`crate::wm::desktop_view`]): it places every window, so its answer is exact where an
//! accessibility tree is not, since toolkits disagree on what their coordinates count from.
//! Without the window manager (`--wm` off) nothing here can say where a window is, and the
//! answer says so (`"wm": false`), leaving a caller the picture alone.
//!
//! What is inside a window comes from the applications over the accessibility bus, read and
//! placed by the `pelorus` crate; this module joins the two per request and serves the result
//! as an outline, the text a model reads, or as the tree it was written from.
//!
//! Everything is in the pixels the Computer Use actions take: the window manager's logical
//! layout coordinates multiplied by the output's scale. A caller here holds what the session's
//! user holds, so every route refuses a view-only credential, and none is mounted unless
//! `--pelorus` is on.

mod agent;
mod configs;
mod model;
mod tools;

use std::sync::Mutex;

use ::pelorus::atspi::{Bus, Miss};
use ::pelorus::{Element, Placement, Protocol, outline};
use axum::Router;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use pixelflux::wm::{DesktopView, OutputInfo, Rect, Request, WindowKind, WindowView};
use serde_json::{Value as Json, json};

use crate::http::auth::Ceiling;
use crate::http::routes::json_error;

/// The most controls read of one window.
const TREE_BUDGET: usize = 1500;

/// The most lines an outline holds.
const OUTLINE_LINES: usize = 400;

/// The connection to the session's accessibility bus, made on the first read and again after
/// one that failed, since the bus comes and goes with the session's applications.
static BUS: Mutex<Option<Bus>> = Mutex::new(None);

/// The routes, written from the root the `http` module nests under the subfolder prefix.
pub fn routes() -> Router {
    Router::new()
        .route("/pelorus/api/desktop", get(handle_desktop))
        .route("/pelorus/api/window/{id}/outline", get(handle_outline))
        .route("/pelorus/api/window/{id}/tree", get(handle_tree))
        .route("/pelorus/api/window/{id}/{verb}", post(handle_window))
        .route("/pelorus/api/tools", get(tools::handle_tools))
        .route("/pelorus/api/tool", post(tools::handle_tool))
        .route(
            "/pelorus/api/models",
            get(configs::handle_list).post(configs::handle_create),
        )
        .route("/pelorus/api/models/names", post(configs::handle_names))
        .route(
            "/pelorus/api/models/{id}",
            axum::routing::put(configs::handle_update).delete(configs::handle_delete),
        )
        .route("/pelorus/api/run", post(agent::handle_run))
        .route(
            "/pelorus/api/session/{id}",
            axum::routing::delete(agent::handle_forget),
        )
}

/// The refusal every route gives a view-only credential.
fn refused() -> Response {
    json_error(
        StatusCode::FORBIDDEN,
        "Pelorus takes a controller's credential",
    )
}

/// The request a window verb of the API stands for.
fn window_verb(verb: &str) -> Option<Request> {
    match verb {
        "activate" => Some(Request::Activate),
        "close" => Some(Request::Close),
        "minimize" => Some(Request::Minimize),
        "restore" => Some(Request::Unminimize),
        "maximize" => Some(Request::Maximize),
        "unmaximize" => Some(Request::Unmaximize),
        _ => None,
    }
}

/// `POST /pelorus/api/window/{id}/{verb}`: ask the window manager to activate (focus and
/// raise), close, minimize, restore, maximize, or unmaximize a window, as its taskbar button
/// or frame would. A close is the client's to honor: one with unsaved work may ask first.
async fn handle_window(ceiling: Ceiling, Path((id, verb)): Path<(u32, String)>) -> Response {
    if ceiling.is_viewer() {
        return refused();
    }
    let Some(request) = window_verb(&verb) else {
        return json_error(StatusCode::NOT_FOUND, "unknown window verb");
    };
    match tokio::task::spawn_blocking(move || crate::wm::window_request(id, request)).await {
        Ok(true) => axum::Json(json!({"status": "ok"})).into_response(),
        Ok(false) => json_error(StatusCode::NOT_FOUND, "no window manager knows that window"),
        Err(_) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the request could not be sent",
        ),
    }
}

/// `GET /pelorus/api/desktop`: the outputs and every window, bottom of the stack first.
async fn handle_desktop(ceiling: Ceiling) -> Response {
    if ceiling.is_viewer() {
        return refused();
    }
    match tokio::task::spawn_blocking(crate::wm::desktop_view).await {
        Ok(view) => axum::Json(desktop_json(view.as_ref())).into_response(),
        Err(_) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the desktop could not be read",
        ),
    }
}

/// Why a window's controls could not be read, as a caller is told.
#[derive(Debug, PartialEq, Eq)]
enum Unreadable {
    /// No window manager runs, so nothing says where a window is.
    NoWindowManager,
    /// The window manager knows no window of that id.
    NoWindow,
    /// The window's process is unknown, so no application can be matched to it.
    NoProcess,
    /// The accessibility bus could not be reached.
    NoBus(String),
    /// The application gave no tree.
    Miss(Miss),
}

impl Unreadable {
    /// The status and the sentence a caller gets: what is wrong and what to do about it.
    fn answer(&self) -> (StatusCode, String) {
        match self {
            Unreadable::NoWindowManager => (
                StatusCode::CONFLICT,
                "No window manager runs in this session (--wm is off), so windows cannot be located; \
                 use screenshots."
                    .to_string(),
            ),
            Unreadable::NoWindow => (StatusCode::NOT_FOUND, "No such window.".to_string()),
            Unreadable::NoProcess => (
                StatusCode::CONFLICT,
                "The window's process is not known, so its controls cannot be read; use a screenshot \
                 of the window."
                    .to_string(),
            ),
            Unreadable::NoBus(why) => (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("The accessibility bus is not reachable ({why}); use screenshots."),
            ),
            Unreadable::Miss(Miss::Bare) => (
                StatusCode::CONFLICT,
                "This is a Chromium-based application started without accessibility, so it describes \
                 nothing inside its window. Close it and start it again with \
                 --force-renderer-accessibility, or use a screenshot of the window."
                    .to_string(),
            ),
            Unreadable::Miss(_) => (
                StatusCode::CONFLICT,
                "This application does not describe its window (it draws its own interface); use a \
                 screenshot of the window."
                    .to_string(),
            ),
        }
    }
}

/// A logical rectangle in pixels at `scale`, as the reader's rectangle.
fn px_rect(rect: Rect, scale: f64) -> ::pelorus::Rect {
    ::pelorus::Rect {
        x: px(rect.x, scale) as i32,
        y: px(rect.y, scale) as i32,
        w: px(rect.w, scale) as i32,
        h: px(rect.h, scale) as i32,
    }
}

/// Where the window manager put a window, in pixels, for the reader. An X11 client counts in
/// the server's pixels, a Wayland one in logical units the output's scale multiplies.
fn placement(window: &WindowView, scale: f64) -> Placement {
    let x11 = matches!(window.info.kind, Some(WindowKind::X11 { .. }));
    Placement {
        protocol: if x11 {
            Protocol::X11
        } else {
            Protocol::Wayland
        },
        rect: px_rect(window.rect, scale),
        origin: (
            px(window.origin.0, scale) as i32,
            px(window.origin.1, scale) as i32,
        ),
        scale: if x11 { 1.0 } else { scale },
    }
}

/// What a read of one window gives: its tree, its client area, and the frames of the windows
/// stacked over it, all in pixels.
struct WindowRead {
    title: String,
    tree: Element,
    clip: ::pelorus::Rect,
    above: Vec<::pelorus::Rect>,
}

/// Read the controls of window `id`: the window manager's view for where it is and what lies
/// over it, then the application's tree placed by that. Blocks; runs on a blocking thread.
fn read_window(id: u32) -> Result<WindowRead, Unreadable> {
    let view = crate::wm::desktop_view().ok_or(Unreadable::NoWindowManager)?;
    let index = view
        .windows
        .iter()
        .position(|w| w.info.id == id)
        .ok_or(Unreadable::NoWindow)?;
    let window = &view.windows[index];
    let pid = window.info.pid.ok_or(Unreadable::NoProcess)?;
    let scale = scale_at(&view.outputs, window.rect.x, window.rect.y);
    let placement = placement(window, scale);
    let above = view.windows[index + 1..]
        .iter()
        .filter(|w| w.mapped && !w.states.minimized && !w.states.hidden)
        .map(|w| {
            px_rect(
                w.rect.grown(w.frame),
                scale_at(&view.outputs, w.rect.x, w.rect.y),
            )
        })
        .collect();
    let bus = accessibility_bus()?;
    match bus.window_tree(pid, &window.info.title, &placement, TREE_BUDGET) {
        Ok(tree) => Ok(WindowRead {
            title: window.info.title.clone(),
            tree,
            clip: placement.rect,
            above,
        }),
        Err(miss) => {
            if miss == Miss::Absent {
                *BUS.lock().unwrap_or_else(|p| p.into_inner()) = None;
            }
            Err(Unreadable::Miss(miss))
        }
    }
}

/// The session's accessibility bus, connected on the first call and again after a read found
/// it gone. The lock is held only to take the connection, never across a read: a read makes
/// D-Bus calls per control, each waiting out its timeout on an application that stopped
/// answering, and every other read of the desktop would queue behind it.
fn accessibility_bus() -> Result<Bus, Unreadable> {
    let mut bus = BUS.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(bus) = bus.as_ref() {
        return Ok(bus.clone());
    }
    let address = crate::wm::session_bus_address();
    let connected = Bus::connect(address.as_deref()).map_err(Unreadable::NoBus)?;
    *bus = Some(connected.clone());
    Ok(connected)
}

/// The answer for a window that could not be read.
fn unreadable(why: &Unreadable) -> Response {
    let (status, message) = why.answer();
    json_error(status, &message)
}

/// `GET /pelorus/api/window/{id}/outline`: the window's controls as text, a line a control
/// with the point to click it at (the `pelorus` crate's `outline` module has the format).
async fn handle_outline(ceiling: Ceiling, Path(id): Path<u32>) -> Response {
    if ceiling.is_viewer() {
        return refused();
    }
    match tokio::task::spawn_blocking(move || read_window(id)).await {
        Ok(Ok(read)) => {
            let covered = |x: i32, y: i32| read.above.iter().any(|frame| frame.contains(x, y));
            let body = outline::write(
                &read.tree,
                &outline::Options {
                    marks: outline::Marks::Points,
                    clip: read.clip,
                    covered: &covered,
                    lines: OUTLINE_LINES,
                },
            )
            .text;
            let text = format!("window {id} \"{}\"\n{body}", read.title);
            ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
        }
        Ok(Err(why)) => unreadable(&why),
        Err(_) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the window could not be read",
        ),
    }
}

/// `GET /pelorus/api/window/{id}/tree`: the window's controls as the tree the outline is
/// written from, every control with its rectangle in pixels and nothing folded away.
async fn handle_tree(ceiling: Ceiling, Path(id): Path<u32>) -> Response {
    if ceiling.is_viewer() {
        return refused();
    }
    match tokio::task::spawn_blocking(move || read_window(id)).await {
        Ok(Ok(read)) => axum::Json(json!({
            "id": id,
            "title": read.title,
            "rect": read.clip,
            "tree": read.tree,
        }))
        .into_response(),
        Ok(Err(why)) => unreadable(&why),
        Err(_) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the window could not be read",
        ),
    }
}

/// The scale of the output a logical point lies on, the primary's where it lies on none.
fn scale_at(outputs: &[OutputInfo], x: i32, y: i32) -> f64 {
    outputs
        .iter()
        .find(|o| o.rect.contains(x, y))
        .or_else(|| outputs.iter().find(|o| o.primary))
        .or(outputs.first())
        .map_or(1.0, |o| o.scale)
}

/// A logical length in pixels at `scale`.
fn px(value: i32, scale: f64) -> i64 {
    (f64::from(value) * scale).round() as i64
}

/// A logical rectangle as `[x, y, width, height]` in pixels at `scale`.
fn rect_json(rect: Rect, scale: f64) -> Json {
    json!([
        px(rect.x, scale),
        px(rect.y, scale),
        px(rect.w, scale),
        px(rect.h, scale)
    ])
}

/// One window: who it is, where its client area and its frame are, where its own coordinates
/// count from, and its state.
fn window_json(window: &WindowView, focus: Option<u32>, outputs: &[OutputInfo]) -> Json {
    let info = &window.info;
    let scale = scale_at(outputs, window.rect.x, window.rect.y);
    json!({
        "id": info.id,
        "pid": info.pid,
        "title": info.title,
        "app_id": info.app_id,
        "protocol": match info.kind {
            Some(WindowKind::Xdg) => "wayland",
            Some(WindowKind::X11 { .. }) => "x11",
            None => "unknown",
        },
        "type": format!("{:?}", info.window_type).to_lowercase(),
        "parent": info.parent,
        "modal": info.modal,
        "output": info.output,
        "scale": scale,
        "rect": rect_json(window.rect, scale),
        "frame": rect_json(window.rect.grown(window.frame), scale),
        "origin": [px(window.origin.0, scale), px(window.origin.1, scale)],
        "focused": focus == Some(info.id),
        "mapped": window.mapped,
        "minimized": window.states.minimized,
        "maximized": window.states.maximized,
        "fullscreen": window.states.fullscreen,
        "shaded": window.shaded,
        "other_desktop": window.states.hidden,
        "desktop": window.desktop,
        "listed": window.listed,
    })
}

/// The desktop as JSON; `{"wm": false}` where no window manager answers.
fn desktop_json(view: Option<&DesktopView>) -> Json {
    let Some(view) = view else {
        return json!({"wm": false});
    };
    let outputs: Vec<Json> = view
        .outputs
        .iter()
        .map(|o| {
            json!({
                "id": o.id,
                "primary": o.primary,
                "scale": o.scale,
                "rect": rect_json(o.rect, o.scale),
                "workarea": rect_json(o.workarea, o.scale),
            })
        })
        .collect();
    let windows: Vec<Json> = view
        .windows
        .iter()
        .map(|w| window_json(w, view.focus, &view.outputs))
        .collect();
    json!({
        "wm": true,
        "outputs": outputs,
        "windows": windows,
        "focus": view.focus,
        "desktops": {"count": view.desktop_count, "current": view.current_desktop},
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixelflux::wm::{Extents, WindowInfo};

    fn output(scale: f64) -> OutputInfo {
        OutputInfo {
            id: 0,
            rect: Rect::new(0, 0, 960, 540),
            workarea: Rect::new(0, 0, 960, 500),
            scale,
            primary: true,
        }
    }

    #[test]
    fn a_session_without_a_window_manager_says_so() {
        assert_eq!(desktop_json(None), json!({"wm": false}));
    }

    #[test]
    fn geometry_is_reported_in_pixels_at_the_outputs_scale() {
        let view = DesktopView {
            outputs: vec![output(2.0)],
            windows: vec![WindowView {
                info: WindowInfo {
                    id: 7,
                    pid: Some(42),
                    ..WindowInfo::default()
                },
                rect: Rect::new(100, 50, 300, 200),
                frame: Extents {
                    top: 30,
                    left: 4,
                    right: 4,
                    bottom: 4,
                },
                origin: (90, 40),
                ..WindowView::default()
            }],
            focus: Some(7),
            desktop_count: 1,
            current_desktop: 0,
        };
        let json = desktop_json(Some(&view));
        let window = &json["windows"][0];
        assert_eq!(window["rect"], json!([200, 100, 600, 400]));
        assert_eq!(window["frame"], json!([192, 40, 616, 468]));
        assert_eq!(window["origin"], json!([180, 80]));
        assert_eq!(window["focused"], json!(true));
        assert_eq!(json["outputs"][0]["rect"], json!([0, 0, 1920, 1080]));
    }
}
