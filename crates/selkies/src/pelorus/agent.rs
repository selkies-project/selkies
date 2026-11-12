/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The agent: a model given the session's tools and a task, run until it answers.
//!
//! One run is a loop: the model is asked for its next turn, the tools it calls are carried
//! out one after another, what they returned goes back to it, and it ends when the model
//! answers without calling one, declines, is cut off, or has taken the most steps a run may.
//! Everything that happens is an [`Event`], sent to whoever asked as it happens (a
//! server-sent stream) or gathered into one answer, and a listener that goes away ends the
//! run at its next step, which is how a run is stopped. A person watching can ask for the
//! screen as well ([`Watch::screens`]): a picture before the run, after each tool that acted,
//! and at the end, for their eyes alone, since the model works from text.
//!
//! A conversation outlives its run under a session id, so a later prompt continues it with
//! everything the model did before in view. The history is append-only (`model` module): a
//! run adds to it and nothing is taken out, and a session is dropped whole when more recent
//! ones need its place or its model changes, since one provider cannot replay another's turns.
//!
//! The model is one of the configurations the `configs` module keeps: the one a request
//! names, else the default, with the model name and effort the request asks for in place of
//! the configuration's own unless the operator locked it.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Mutex;

use ::pelorus::tools::{definitions, system_prompt};
use axum::body::Bytes;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use serde_json::{Value as Json, json};
use tokio::sync::mpsc;

use super::model::{self, Model, Return, Stop, Turn, Wire};
use super::{configs, refused, tools};
use crate::http::auth::Ceiling;
use crate::http::routes::json_error;
use crate::settings::settings;

/// The most conversations kept at once.
const SESSIONS_KEPT: usize = 16;

/// The most characters of a person's own instructions a run takes.
const SUFFIX_MOST: usize = 2000;

/// The longest a tool's text is shown in an event; the model is given all of it.
const EVENT_TEXT: usize = 4000;

/// What happens in a run, as a listener is told.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Start {
        session: String,
        model: String,
    },
    /// A model turn begins, counted from one.
    Step(usize),
    /// The screen for a person watching: before the run (`start`), after a tool that acted
    /// (`step`), and at the end (`end`), as a JPEG. Never sent to the model.
    Screen {
        when: &'static str,
        jpeg: Vec<u8>,
    },
    /// The model's summary of its reasoning before a step.
    Thinking(String),
    /// What the model said beside or instead of calling tools.
    Text(String),
    Call {
        name: String,
        arguments: Json,
    },
    Result {
        name: String,
        text: String,
        failed: bool,
        image: Option<Vec<u8>>,
    },
    /// The run ended: whether the model finished the task itself, and its last words or why
    /// it was ended.
    Done {
        success: bool,
        text: String,
    },
    Error(String),
}

impl Event {
    /// The event as the JSON a client reads, a picture as base64 PNG.
    pub fn json(&self) -> Json {
        match self {
            Event::Start { session, model } => {
                json!({"type": "start", "session": session, "model": model})
            }
            Event::Step(number) => json!({"type": "step", "number": number}),
            Event::Screen { when, jpeg } => {
                let image = base64::engine::general_purpose::STANDARD.encode(jpeg);
                json!({"type": "screen", "when": when, "image": image})
            }
            Event::Thinking(text) => json!({"type": "thinking", "text": text}),
            Event::Text(text) => json!({"type": "text", "text": text}),
            Event::Call { name, arguments } => {
                json!({"type": "call", "name": name, "arguments": arguments})
            }
            Event::Result {
                name,
                text,
                failed,
                image,
            } => {
                let shown: String = text.chars().take(EVENT_TEXT).collect();
                let image = image
                    .as_ref()
                    .map(|png| base64::engine::general_purpose::STANDARD.encode(png));
                json!({"type": "result", "name": name, "text": shown, "failed": failed, "image": image})
            }
            Event::Done { success, text } => {
                json!({"type": "done", "success": success, "text": text})
            }
            Event::Error(message) => json!({"type": "error", "message": message}),
        }
    }
}

/// One kept conversation.
struct Session {
    id: String,
    wire: Wire,
    turns: Vec<Turn>,
    /// A run is adding to it.
    busy: bool,
}

/// The kept conversations, least recently used first.
static SESSIONS: Mutex<VecDeque<Session>> = Mutex::new(VecDeque::new());

/// Take a session's history for a run, marking it busy; a new session where the id is not
/// kept or was another provider's. `None` while another run holds it.
fn take_session(id: &str, wire: Wire) -> Option<Vec<Turn>> {
    let mut sessions = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    let kept = sessions.iter().position(|s| s.id == id);
    let mut session = match kept.and_then(|index| sessions.remove(index)) {
        Some(session) if session.busy => {
            sessions.push_back(session);
            return None;
        }
        Some(session) if session.wire == wire => session,
        _ => Session {
            id: id.to_string(),
            wire,
            turns: Vec::new(),
            busy: false,
        },
    };
    session.busy = true;
    let turns = session.turns.clone();
    sessions.push_back(session);
    while sessions.len() > SESSIONS_KEPT {
        match sessions.iter().position(|s| !s.busy) {
            Some(index) => {
                sessions.remove(index);
            }
            None => break,
        }
    }
    Some(turns)
}

/// Put a session's history back after its run.
fn return_session(id: &str, turns: Vec<Turn>) {
    let mut sessions = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(session) = sessions.iter_mut().find(|s| s.id == id) {
        session.turns = turns;
        session.busy = false;
    }
}

/// What a run is asked for beside its task.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Watch {
    /// Send the screen to the listener as the run goes.
    pub screens: bool,
    /// Instructions of the person's own, added after the server's.
    pub suffix: String,
}

/// The instructions a run works by: the server's, then the person's own where they gave any.
fn instructions(capabilities: ::pelorus::tools::Capabilities, suffix: &str) -> String {
    let date = chrono::Local::now().format("%A, %B %-d, %Y").to_string();
    let mut system = system_prompt(capabilities, &date);
    if !suffix.trim().is_empty() {
        system.push_str("\nThe user's standing instructions:\n");
        system.push_str(suffix.trim());
        system.push('\n');
    }
    system
}

/// The screen for a person watching, where they asked for it and it can be captured.
async fn screen(watch: &Watch, when: &'static str) -> Option<Event> {
    if !watch.screens {
        return None;
    }
    let jpeg = tools::watch_screen().await?;
    Some(Event::Screen { when, jpeg })
}

/// Run the model on a conversation until it ends, telling `emit` each event; `emit` answers
/// whether anyone still listens. Returns the conversation with the run added.
pub async fn run(
    model: &Model,
    mut turns: Vec<Turn>,
    max_steps: usize,
    watch: &Watch,
    mut emit: impl FnMut(Event) -> bool + Send,
) -> Vec<Turn> {
    let capabilities = tools::capabilities(model.vision).await;
    let system = instructions(capabilities, &watch.suffix);
    let tool_list = definitions(capabilities);
    if let Some(event) = screen(watch, "start").await {
        emit(event);
    }
    for step in 1..=max_steps {
        emit(Event::Step(step));
        let reply = match model::complete(model, &system, &tool_list, &turns).await {
            Ok(reply) => reply,
            Err(e) => {
                log::warn!(target: "pelorus", "the agent's run ended: {e}");
                emit(Event::Error(e));
                return turns;
            }
        };
        turns.push(Turn::Assistant(reply.clone()));
        if !reply.thinking.is_empty() {
            emit(Event::Thinking(reply.thinking.clone()));
        }
        let listening = reply.text.trim().is_empty()
            || reply.stop == Stop::Done
            || emit(Event::Text(reply.text.clone()));
        match reply.stop {
            Stop::Done => {
                if let Some(event) = screen(watch, "end").await {
                    emit(event);
                }
                emit(Event::Done {
                    success: true,
                    text: reply.text,
                });
                return turns;
            }
            Stop::Truncated => {
                emit(Event::Done {
                    success: false,
                    text: "The model's turn was cut off at its length limit.".to_string(),
                });
                return turns;
            }
            Stop::Refused(category) => {
                let why = if category.is_empty() {
                    "The model declined to continue.".to_string()
                } else {
                    format!("The model declined to continue ({category}).")
                };
                emit(Event::Done {
                    success: false,
                    text: why,
                });
                return turns;
            }
            Stop::Paused => {}
            Stop::Tools => {
                let mut returns = Vec::new();
                let mut heard = listening;
                for call in &reply.calls {
                    heard = heard
                        && emit(Event::Call {
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        });
                    let output = if heard {
                        tools::call(&call.name, &call.arguments, capabilities).await
                    } else {
                        tools::Output {
                            text: "The run was stopped before this was done.".to_string(),
                            image: None,
                            failed: true,
                        }
                    };
                    heard = heard
                        && emit(Event::Result {
                            name: call.name.clone(),
                            text: output.text.clone(),
                            failed: output.failed,
                            image: output.image.clone(),
                        });
                    if heard
                        && tools::acts(&call.name)
                        && let Some(event) = screen(watch, "step").await
                    {
                        emit(event);
                    }
                    returns.push(Return {
                        id: call.id.clone(),
                        text: output.text,
                        image: output.image,
                        failed: output.failed,
                    });
                }
                turns.push(Turn::Returns(returns));
                if !heard {
                    return turns;
                }
            }
        }
    }
    emit(Event::Done {
        success: false,
        text: format!("Stopped after {max_steps} steps without finishing."),
    });
    turns
}

/// `POST /pelorus/api/run`: give the agent a task. The body names the `prompt`, a `session`
/// to continue (a new one where absent), `stream` for a server-sent stream of events instead
/// of one answer, the `config` to run on (the default where absent), and what of it the
/// request changes (`model`, `effort`, `vision`), the `api_key` a sealed configuration's
/// browser opened (used for this run alone), with `max_steps` to end sooner, `suffix`
/// for instructions of the person's own after the server's, and `screenshots` to be sent the
/// screen as the run goes.
/// Closing a stream stops its run. With `transcript`, an answer that is not a stream carries
/// the conversation as it would next be sent to the model, in the provider's own format: the
/// instructions, the tools, and every turn, for someone studying what the model is given.
pub async fn handle_run(ceiling: Ceiling, body: Bytes) -> Response {
    if ceiling.is_viewer() {
        return refused();
    }
    let Ok(request) = serde_json::from_slice::<Json>(&body) else {
        return json_error(StatusCode::BAD_REQUEST, "the body must be JSON");
    };
    let Some(prompt) = request
        .get("prompt")
        .and_then(Json::as_str)
        .filter(|p| !p.trim().is_empty())
        .map(str::to_string)
    else {
        return json_error(StatusCode::BAD_REQUEST, "`prompt` is required");
    };
    let asked = |name: &str| {
        request
            .get(name)
            .and_then(Json::as_str)
            .filter(|v| !v.is_empty())
    };
    let vision = request.get("vision").and_then(Json::as_bool);
    let model = match configs::resolve(
        asked("config"),
        asked("model"),
        asked("effort"),
        vision,
        asked("api_key"),
    ) {
        Ok(model) => model,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &e),
    };
    let limit = settings().pelorus_max_steps().max(1) as u64;
    let max_steps = request
        .get("max_steps")
        .and_then(Json::as_u64)
        .map_or(limit, |asked| asked.clamp(1, limit)) as usize;
    let session = request
        .get("session")
        .and_then(Json::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 64)
        .map_or_else(|| format!("{:016x}", rand::random::<u64>()), str::to_string);
    let watch = Watch {
        screens: request
            .get("screenshots")
            .and_then(Json::as_bool)
            .unwrap_or(false),
        suffix: request
            .get("suffix")
            .and_then(Json::as_str)
            .map(|suffix| suffix.chars().take(SUFFIX_MOST).collect())
            .unwrap_or_default(),
    };
    let Some(mut turns) = take_session(&session, model.provider.wire) else {
        return json_error(
            StatusCode::CONFLICT,
            "that session is running; stop it or wait for it",
        );
    };
    turns.push(Turn::User(prompt));
    log::info!(target: "pelorus", "agent run on {} ({}), session {session}", model.name, model.provider.id);
    let start = Event::Start {
        session: session.clone(),
        model: model.name.clone(),
    };
    if request
        .get("stream")
        .and_then(Json::as_bool)
        .unwrap_or(false)
    {
        let (tx, rx) = mpsc::unbounded_channel::<Event>();
        tokio::spawn(async move {
            let _ = tx.send(start);
            let turns = run(&model, turns, max_steps, &watch, |event| {
                tx.send(event).is_ok()
            })
            .await;
            return_session(&session, turns);
        });
        let stream = futures::stream::unfold(rx, |mut rx| async {
            let event = rx.recv().await?;
            let sse = SseEvent::default().data(event.json().to_string());
            Some((Ok::<_, Infallible>(sse), rx))
        });
        return Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response();
    }
    let mut events = vec![start.json()];
    let mut outcome = (false, String::new());
    let turns = run(&model, turns, max_steps, &watch, |event| {
        match &event {
            Event::Done { success, text } => outcome = (*success, text.clone()),
            Event::Error(message) => outcome = (false, message.clone()),
            _ => {}
        }
        events.push(event.json());
        true
    })
    .await;
    let transcript = if request
        .get("transcript")
        .and_then(Json::as_bool)
        .unwrap_or(false)
    {
        let capabilities = tools::capabilities(model.vision).await;
        model::request_body(
            &model,
            &instructions(capabilities, &watch.suffix),
            &definitions(capabilities),
            &turns,
        )
    } else {
        Json::Null
    };
    return_session(&session, turns);
    axum::Json(json!({
        "success": outcome.0,
        "text": outcome.1,
        "session": session,
        "events": events,
        "transcript": transcript,
    }))
    .into_response()
}

/// `DELETE /pelorus/api/session/{id}`: forget a conversation; refused while a run holds it.
pub async fn handle_forget(ceiling: Ceiling, Path(id): Path<String>) -> Response {
    if ceiling.is_viewer() {
        return refused();
    }
    let mut sessions = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    match sessions.iter().position(|s| s.id == id) {
        Some(index) if sessions[index].busy => {
            json_error(StatusCode::CONFLICT, "that session is running")
        }
        Some(index) => {
            sessions.remove(index);
            axum::Json(json!({"status": "ok"})).into_response()
        }
        None => json_error(StatusCode::NOT_FOUND, "no such session"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_held_by_one_run_and_kept_for_the_next() {
        let id = "test-held-session";
        assert_eq!(take_session(id, Wire::Anthropic), Some(Vec::new()));
        assert_eq!(take_session(id, Wire::Anthropic), None);
        return_session(id, vec![Turn::User("first".to_string())]);
        assert_eq!(
            take_session(id, Wire::Anthropic),
            Some(vec![Turn::User("first".to_string())])
        );
        return_session(id, vec![Turn::User("first".to_string())]);
        assert_eq!(take_session(id, Wire::OpenAi), Some(Vec::new()));
        return_session(id, Vec::new());
    }

    #[test]
    fn an_event_shows_a_picture_as_base64_and_cuts_long_text() {
        let event = Event::Result {
            name: "screenshot".to_string(),
            text: "x".repeat(EVENT_TEXT + 10),
            failed: false,
            image: Some(vec![1, 2, 3]),
        };
        let json = event.json();
        assert_eq!(json["image"], "AQID");
        assert_eq!(json["text"].as_str().unwrap().len(), EVENT_TEXT);
        assert_eq!(
            Event::Text("hi".to_string()).json(),
            json!({"type": "text", "text": "hi"})
        );
    }
}
