/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The supervisor's own routes: liveness and status, the mode switch, the desktop or
//! application session mode, the session listing and disconnect, the token table, the metrics
//! exposition, and the TURN configuration.
//!
//! Every route is written as `/api/<name>`; [`super::build`] nests them under the subfolder
//! prefix. JSON bodies are capped at 1 MiB.
//! A view-only credential ([`Ceiling::is_viewer`]) is refused by every route that changes
//! host or session state and may read everything the session shows anyway.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde_json::{Value as Json, json};

use super::auth::Ceiling;
use super::{AppState, TurnVerdict, tokens};

/// The most a JSON request body may hold.
const JSON_BODY_LIMIT: usize = 1 << 20;

/// The supervisor's routes, relative to the prefix, with their body limit.
pub fn api_router(state: &AppState) -> Router<AppState> {
    let mut router = Router::new()
        .route("/api/status", get(handle_status))
        .route("/api/health", get(handle_health))
        .route("/api/switch", post(handle_switch))
        .route(
            "/api/session",
            get(handle_session_mode).post(handle_session_mode_switch),
        )
        .route("/api/tokens", post(tokens::handle_tokens))
        .route("/api/sessions", get(handle_sessions))
        .route("/api/sessions/{id}", delete(handle_session_delete))
        .route("/api/turn", get(handle_turn));
    if state.config.enable_metrics_http {
        router = router.route("/api/metrics", get(handle_metrics));
    }
    router.route_layer(DefaultBodyLimit::max(JSON_BODY_LIMIT))
}

/// A request body read as JSON, untyped: nothing in it chooses a type to build, and serde_json
/// refuses nesting past 128 levels. Every route that takes a JSON body reads it here.
pub fn json_body(body: &[u8]) -> serde_json::Result<Json> {
    let mut reader = serde_json::Deserializer::from_slice(body);
    let value = <Json as serde::Deserialize>::deserialize(&mut reader)?;
    reader.end()?;
    Ok(value)
}

/// A JSON error body of the shape the dashboards read: `{"status": "error", "message"}`.
pub fn json_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(json!({"status": "error", "message": message})),
    )
        .into_response()
}

/// `GET /api/status`: the active mode, the modes available, and what the dashboard paints
/// before any stream connects (the dual-mode toggle, the layout and the layouts allowed, and
/// whether Pelorus is served, which decides whether its panel is offered).
async fn handle_status(State(state): State<AppState>) -> Response {
    let body = json!({
        "current_mode": state.supervisor.current_mode(),
        "available_modes": state.supervisor.available_modes(),
        "enable_dual_mode": state.config.enable_dual_mode,
        "ui_layout": state.config.ui_layout,
        "ui_layout_allowed": state.config.ui_layout_allowed,
        "pelorus": state.config.pelorus,
    });
    axum::Json(body).into_response()
}

/// `GET /api/health`: liveness probe, always 200.
async fn handle_health() -> &'static str {
    "OK"
}

/// `POST /api/switch`: change the active streaming mode.
///
/// A controller may switch: it already drives the desktop, so withholding the transport from
/// it protects nothing; view-only credentials and viewer-role tokens are refused, and the
/// master token remains the operator's way in. Refused as well when dual mode is disabled.
async fn handle_switch(State(state): State<AppState>, ceiling: Ceiling, body: Bytes) -> Response {
    if ceiling.is_viewer() {
        return json_error(
            StatusCode::FORBIDDEN,
            "View-only credentials cannot switch streaming mode",
        );
    }
    if !state.config.enable_dual_mode {
        return json_error(StatusCode::FORBIDDEN, "Dual streaming mode disabled");
    }
    let parsed = json_body(&body);
    let mode = match parsed {
        Ok(Json::Object(map)) => match map.get("mode") {
            Some(Json::String(mode)) => mode.clone(),
            Some(other) => other.to_string(),
            None => "None".to_string(),
        },
        Ok(_) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                "Request body must be a JSON object",
            );
        }
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    match state.supervisor.switch_to_mode(&mode).await {
        Ok(()) => axum::Json(json!({"status": "success", "mode": mode})).into_response(),
        Err(message) => json_error(StatusCode::BAD_REQUEST, &message),
    }
}

/// The session's mode as `/api/session` answers it: whether the desktop shell runs, whether the
/// window manager's menus offer the switch, whether the server may switch it, and whether the
/// session's application runs (`null` where there is no `autostart`).
fn session_mode_body(mode: selkies_wm::core::SessionMode) -> Json {
    json!({
        "desktop": mode.desktop,
        "switchable": mode.switchable,
        "server_switch": mode.server_switch,
        "application": mode.application,
    })
}

/// `GET /api/session`: the session's mode, the desktop or the application alone, as the window
/// manager holds it; 404 where no window manager runs.
async fn handle_session_mode() -> Response {
    match crate::wm::session_mode() {
        Some(mode) => axum::Json(session_mode_body(mode)).into_response(),
        None => json_error(
            StatusCode::NOT_FOUND,
            &crate::wm::ModeRefusal::NoWindowManager.to_string(),
        ),
    }
}

/// `POST /api/session`: switch the session to the desktop or to the application alone, the
/// body `{"desktop": true|false}`, answered with the mode as it stands after.
///
/// The operator (the master token, or anyone on a server without credentials) switches wherever
/// the desktop setting is not locked and the shell is installed, whether or not the menus offer
/// the switch, and the switch is not kept for the next session
/// ([`crate::wm::set_desktop_mode`]). Any other controller switches only where the menus offer
/// it, since there it could switch from them; viewers never.
async fn handle_session_mode_switch(ceiling: Ceiling, body: Bytes) -> Response {
    if ceiling.is_viewer() {
        return json_error(
            StatusCode::FORBIDDEN,
            "View-only credentials cannot switch the session's mode",
        );
    }
    let on = match json_body(&body) {
        Ok(Json::Object(map)) => match map.get("desktop") {
            Some(Json::Bool(on)) => *on,
            _ => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "The body must name \"desktop\": true or false",
                );
            }
        },
        Ok(_) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                "Request body must be a JSON object",
            );
        }
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let Some(mode) = crate::wm::session_mode() else {
        return json_error(
            StatusCode::NOT_FOUND,
            &crate::wm::ModeRefusal::NoWindowManager.to_string(),
        );
    };
    if ceiling.0.is_some() && !mode.switchable {
        return json_error(
            StatusCode::FORBIDDEN,
            "Only the master token switches a session whose menus do not offer the switch",
        );
    }
    let Ok(result) = tokio::task::spawn_blocking(move || crate::wm::set_desktop_mode(on)).await
    else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    match result {
        Ok(mode) => axum::Json(session_mode_body(mode)).into_response(),
        Err(refusal) => {
            let status = match refusal {
                crate::wm::ModeRefusal::NoWindowManager => StatusCode::NOT_FOUND,
                crate::wm::ModeRefusal::Locked => StatusCode::CONFLICT,
                crate::wm::ModeRefusal::NoAnswer => StatusCode::SERVICE_UNAVAILABLE,
            };
            json_error(status, &refusal.to_string())
        }
    }
}

/// `GET /api/sessions`: the pages connected to the active transport.
async fn handle_sessions(State(state): State<AppState>) -> Response {
    let sessions = state.supervisor.sessions().await;
    axum::Json(json!({"sessions": sessions})).into_response()
}

/// `DELETE /api/sessions/{id}`: close that page's connection. Refused to view-only
/// credentials, like every change to the session.
async fn handle_session_delete(
    State(state): State<AppState>,
    ceiling: Ceiling,
    Path(id): Path<String>,
) -> Response {
    if ceiling.is_viewer() {
        return (
            StatusCode::FORBIDDEN,
            "View-only credentials cannot disconnect a session",
        )
            .into_response();
    }
    if !state.supervisor.disconnect_session(&id).await {
        return (StatusCode::NOT_FOUND, "No such session").into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `GET /api/metrics`: the Prometheus exposition of the registry the stats agent exposes,
/// rendered on a blocking thread, never cached.
async fn handle_metrics(State(state): State<AppState>) -> Response {
    let Some(metrics) = state.metrics.clone() else {
        return (StatusCode::NOT_IMPLEMENTED, "no metrics registry").into_response();
    };
    let Ok((body, content_type)) = tokio::task::spawn_blocking(move || metrics.render()).await
    else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (
                header::CACHE_CONTROL,
                "no-cache, no-store, must-revalidate".to_string(),
            ),
            (header::PRAGMA, "no-cache".to_string()),
            (header::EXPIRES, "0".to_string()),
        ],
        body,
    )
        .into_response()
}

/// `GET /api/turn`: the WebRTC ICE configuration, by the verdict the service gives.
async fn handle_turn(State(state): State<AppState>, uri: axum::http::Uri) -> Response {
    let verdict = state
        .turn
        .as_ref()
        .map(|t| t.rtc_config())
        .unwrap_or(TurnVerdict::Inactive);
    match verdict {
        TurnVerdict::Inactive => (
            StatusCode::CONFLICT,
            axum::Json(json!({"error": "WebRTC mode is inactive"})),
        )
            .into_response(),
        TurnVerdict::Starting => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "1")],
            axum::Json(json!({"error": "WebRTC service is still starting"})),
        )
            .into_response(),
        TurnVerdict::Config(text) => {
            ([(header::CONTENT_TYPE, "application/json")], text).into_response()
        }
        TurnVerdict::Missing => {
            log::warn!(target: "server", "HTTP GET {} 404 NOT FOUND - Missing RTC config", uri.path());
            (StatusCode::NOT_FOUND, "404 NOT FOUND").into_response()
        }
    }
}
