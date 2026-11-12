/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The models an agent runs on: one conversation, two wire formats.
//!
//! A conversation is kept in a neutral form ([`Turn`]) and written out per request in the
//! provider's: Anthropic's Messages API, or the chat completions format OpenAI defined, which
//! nearly every other provider and local server also serves, so all of those are one client
//! and a table of addresses ([`PROVIDERS`]).
//! What a provider answered is kept beside its neutral reading ([`Reply::native`]) and sent
//! back as it came: Anthropic's models return thinking blocks that are only valid unchanged
//! and in the conversation that made them, so the history is append-only and nothing in it is
//! rewritten or summarized.
//!
//! An Anthropic request caches its prefix (`cache_control`), since every step resends the
//! whole conversation, asks a model that reasons for a summary of its reasoning, which a
//! listener is shown and which changes nothing of what the model does, and on Anthropic's own endpoint asks for the server-side fallback, so a
//! request a model's safety classifier declines is answered by another model inside the same
//! call rather than ending the run. A request is retried on the statuses that mean "later"
//! (429 and the 5xx range), twice, and fails with the provider's own message otherwise.

use std::sync::OnceLock;
use std::time::Duration;

use ::pelorus::tools::Definition;
use base64::Engine as _;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value as Json, json};

/// Anthropic's API version, and the beta under which a declined request falls back.
const ANTHROPIC_VERSION: &str = "2026-10-21";
const ANTHROPIC_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const ANTHROPIC_HOST: &str = "https://api.anthropic.com";

/// The most tokens one model turn may write.
const MAX_TOKENS: u32 = 16000;

/// How long one request may take, and how often one is retried.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const RETRIES: usize = 2;

/// The format a provider's API speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    /// Anthropic's Messages API.
    Anthropic,
    /// The chat completions format OpenAI defined and most others serve.
    OpenAi,
}

/// Whether a provider takes an API key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// A run cannot start without one.
    Required,
    /// The server may or may not ask for one.
    Optional,
    /// A local server that takes none.
    None,
}

/// One provider a configuration can name: what a person would otherwise have to look up.
#[derive(Debug, PartialEq, Eq)]
pub struct Provider {
    /// The name settings and requests use.
    pub id: &'static str,
    /// The name a person is shown.
    pub label: &'static str,
    pub wire: Wire,
    /// The provider's own base URL; empty where there is none to know (`custom`).
    pub endpoint: &'static str,
    /// The model used where none is named; empty where a person has to pick one.
    pub model: &'static str,
    pub key: Key,
    /// Whether the endpoint is a person's to set: a server on their own machine or network,
    /// rather than one public address.
    pub local: bool,
}

/// A provider with a public address and a key.
const fn hosted(id: &'static str, label: &'static str, endpoint: &'static str) -> Provider {
    Provider {
        id,
        label,
        wire: Wire::OpenAi,
        endpoint,
        model: "",
        key: Key::Required,
        local: false,
    }
}

/// A server a person runs themselves, at its usual local address.
const fn local(
    id: &'static str,
    label: &'static str,
    endpoint: &'static str,
    key: Key,
) -> Provider {
    Provider {
        id,
        label,
        wire: Wire::OpenAi,
        endpoint,
        model: "",
        key,
        local: true,
    }
}

/// The providers a configuration can name, in the order a person is offered them: the hosted
/// ones by their public addresses, the local servers at their usual ones, and `custom` for any
/// other server that speaks chat completions.
pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: "anthropic",
        label: "Anthropic",
        wire: Wire::Anthropic,
        endpoint: ANTHROPIC_HOST,
        model: "claude-opus-5-5",
        key: Key::Required,
        local: false,
    },
    hosted("openai", "OpenAI", "https://api.openai.com/v1"),
    hosted(
        "gemini",
        "Google Gemini",
        "https://generativelanguage.googleapis.com/v1beta/openai",
    ),
    hosted("openrouter", "OpenRouter", "https://openrouter.ai/api/v1"),
    hosted("groq", "Groq", "https://api.groq.com/openai/v1"),
    hosted("mistral", "Mistral", "https://api.mistral.ai/v1"),
    hosted("deepseek", "DeepSeek", "https://api.deepseek.com/v1"),
    hosted("xai", "xAI", "https://api.x.ai/v1"),
    hosted("together", "Together AI", "https://api.together.xyz/v1"),
    hosted(
        "fireworks",
        "Fireworks AI",
        "https://api.fireworks.ai/inference/v1",
    ),
    hosted("cerebras", "Cerebras", "https://api.cerebras.ai/v1"),
    local("ollama", "Ollama", "http://localhost:11434/v1", Key::None),
    local(
        "lmstudio",
        "LM Studio",
        "http://localhost:1234/v1",
        Key::None,
    ),
    local(
        "llamacpp",
        "llama.cpp",
        "http://localhost:8080/v1",
        Key::Optional,
    ),
    local("custom", "Other (OpenAI-compatible)", "", Key::Optional),
];

impl Provider {
    /// The provider a setting or a request names.
    pub fn named(id: &str) -> Option<&'static Provider> {
        PROVIDERS.iter().find(|provider| provider.id == id)
    }
}

/// The model one run is made on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    pub provider: &'static Provider,
    pub endpoint: String,
    pub name: String,
    pub api_key: String,
    pub vision: bool,
    /// The effort asked of a model whose API takes one (`low` to `max`); empty for its own.
    pub effort: String,
}

/// One tool call a model made.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub id: String,
    pub name: String,
    pub arguments: Json,
}

/// What a tool call answered.
#[derive(Clone, Debug, PartialEq)]
pub struct Return {
    pub id: String,
    pub text: String,
    /// A PNG, where the tool made a picture.
    pub image: Option<Vec<u8>>,
    pub failed: bool,
}

/// Why a model turn ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The model is done and its text is the answer.
    Done,
    /// The model called tools and waits for what they return.
    Tools,
    /// The turn hit the token limit before the model finished.
    Truncated,
    /// The provider paused a long turn; asking again continues it.
    Paused,
    /// The model, or a classifier in front of it, declined; the category where one is given.
    Refused(String),
}

/// One model turn.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    /// The summary of its reasoning the model returned, where it reasons and gives one.
    pub thinking: String,
    pub text: String,
    pub calls: Vec<Call>,
    pub stop: Stop,
    /// The turn as the provider wrote it, replayed unchanged.
    pub native: Json,
}

/// One entry of a conversation.
#[derive(Clone, Debug, PartialEq)]
pub enum Turn {
    User(String),
    Assistant(Reply),
    Returns(Vec<Return>),
}

type Https = hyper_rustls::HttpsConnector<HttpConnector>;

/// The client every request goes out on, or why none could be made.
fn client() -> Result<&'static Client<Https, Full<Bytes>>, String> {
    static CLIENT: OnceLock<Result<Client<Https, Full<Bytes>>, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let connector = hyper_rustls::HttpsConnectorBuilder::new()
                .with_provider_and_native_roots(provider)
                .map_err(|e| format!("no root certificates for the model API: {e}"))?
                .https_or_http()
                .enable_http1()
                .build();
            Ok(Client::builder(TokioExecutor::new())
                .pool_max_idle_per_host(1)
                .build(connector))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// Send a request, `body` as JSON where there is one (a POST, else a GET), and read the answer
/// as JSON, with its status.
async fn send(
    url: &str,
    headers: &[(&str, String)],
    body: Option<&Json>,
) -> Result<(u16, Json), String> {
    let client = client()?;
    let mut request = match body {
        Some(_) => hyper::Request::post(url).header("content-type", "application/json"),
        None => hyper::Request::get(url),
    };
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let payload = body.map_or_else(Bytes::new, |body| Bytes::from(body.to_string()));
    let request = request
        .body(Full::new(payload))
        .map_err(|e| format!("the model API's address is not usable: {e}"))?;
    let answer = async {
        let response = client.request(request).await.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        Ok::<_, String>((status, bytes))
    };
    let (status, bytes) = tokio::time::timeout(REQUEST_TIMEOUT, answer)
        .await
        .map_err(|_| "the model API did not answer in time".to_string())?
        .map_err(|e| format!("the model API could not be reached: {e}"))?;
    let json = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        let text: String = String::from_utf8_lossy(&bytes).chars().take(300).collect();
        json!({"error": {"message": text}})
    });
    Ok((status, json))
}

/// What a provider said was wrong, from the shapes its errors come in.
fn failure(status: u16, json: &Json) -> String {
    let message = json["error"]["message"]
        .as_str()
        .or_else(|| json["error"].as_str())
        .or_else(|| json[0]["error"]["message"].as_str())
        .unwrap_or("no message");
    format!("the model API answered {status}: {message}")
}

/// POST with the retries the module documentation names; a refusal status becomes its message.
async fn ask(url: &str, headers: &[(&str, String)], body: &Json) -> Result<Json, String> {
    let mut attempt = 0;
    loop {
        let (status, json) = send(url, headers, Some(body)).await?;
        if status == 200 {
            return Ok(json);
        }
        let retryable = status == 429 || status >= 500;
        if retryable && attempt < RETRIES {
            attempt += 1;
            tokio::time::sleep(Duration::from_secs(2 << attempt)).await;
            continue;
        }
        return Err(failure(status, &json));
    }
}

/// The headers a provider's API authenticates a request by.
fn headers(model: &Model) -> Vec<(&'static str, String)> {
    match model.provider.wire {
        Wire::Anthropic => {
            let mut headers = vec![
                ("x-api-key", model.api_key.clone()),
                ("anthropic-version", ANTHROPIC_VERSION.to_string()),
            ];
            if anthropic_falls_back(model) {
                headers.push(("anthropic-beta", ANTHROPIC_FALLBACK_BETA.to_string()));
            }
            headers
        }
        _ if model.api_key.is_empty() => Vec::new(),
        _ => vec![("authorization", format!("Bearer {}", model.api_key))],
    }
}

/// The models a provider offers at an endpoint with a key, by name, sorted: what a person
/// picks from where the configuration is edited. Both formats list them under `data` by `id`.
pub async fn list_models(model: &Model) -> Result<Vec<String>, String> {
    let base = model.endpoint.trim_end_matches('/');
    let url = match model.provider.wire {
        Wire::Anthropic => format!("{base}/v1/models?limit=1000"),
        _ => format!("{base}/models"),
    };
    let (status, json) = send(&url, &headers(model), None).await?;
    if status != 200 {
        return Err(failure(status, &json));
    }
    let mut names: Vec<String> = json["data"]
        .as_array()
        .or_else(|| json["models"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| entry["id"].as_str().or_else(|| entry["name"].as_str()))
        .map(|name| name.trim_start_matches("models/").to_string())
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// A PNG as base64.
fn b64(png: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(png)
}

/// The Anthropic request for a conversation.
fn anthropic_body(model: &Model, system: &str, tools: &[Definition], turns: &[Turn]) -> Json {
    let messages: Vec<Json> = turns
        .iter()
        .map(|turn| match turn {
            Turn::User(text) => json!({"role": "user", "content": text}),
            Turn::Assistant(reply) => json!({"role": "assistant", "content": reply.native}),
            Turn::Returns(returns) => {
                let blocks: Vec<Json> = returns
                    .iter()
                    .map(|r| {
                        let mut content = vec![json!({"type": "text", "text": r.text})];
                        if let Some(png) = &r.image {
                            content.push(json!({
                                "type": "image",
                                "source": {"type": "base64", "media_type": "image/png", "data": b64(png)},
                            }));
                        }
                        json!({
                            "type": "tool_result",
                            "tool_use_id": r.id,
                            "content": content,
                            "is_error": r.failed,
                        })
                    })
                    .collect();
                json!({"role": "user", "content": blocks})
            }
        })
        .collect();
    let tools: Vec<Json> = tools
        .iter()
        .map(
            |t| json!({"name": t.name, "description": t.description, "input_schema": t.parameters}),
        )
        .collect();
    let mut body = json!({
        "model": model.name,
        "max_tokens": MAX_TOKENS,
        "system": system,
        "tools": tools,
        "messages": messages,
        "cache_control": {"type": "ephemeral"},
    });
    if anthropic_falls_back(model) {
        body["fallbacks"] = json!("default");
    }
    if anthropic_reasons(model) {
        body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
        if !model.effort.is_empty() {
            body["output_config"] = json!({"effort": model.effort});
        }
    }
    body
}

/// Whether a model takes adaptive thinking and an effort: Anthropic's current families. An
/// older or unknown model is sent neither, since it refuses what it does not know.
fn anthropic_reasons(model: &Model) -> bool {
    [
        "claude-fable-5",
        "claude-mythos-5",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-sonnet-5",
        "claude-sonnet-4-6",
        "claude-haiku-5",
    ]
    .iter()
    .any(|family| model.name.starts_with(family))
}

/// The request a conversation is sent as, in the provider's format: what [`complete`] posts,
/// for a caller that shows it.
pub fn request_body(model: &Model, system: &str, tools: &[Definition], turns: &[Turn]) -> Json {
    match model.provider.wire {
        Wire::Anthropic => anthropic_body(model, system, tools, turns),
        Wire::OpenAi => openai_body(model, system, tools, turns),
    }
}

/// Whether a request asks for the server-side fallback: on Anthropic's own endpoint, which is
/// where it exists, for the models whose classifiers can decline a request.
fn anthropic_falls_back(model: &Model) -> bool {
    model.endpoint.trim_end_matches('/') == ANTHROPIC_HOST
        && ["claude-fable-5", "claude-opus-5", "claude-sonnet-5-5"]
            .iter()
            .any(|family| model.name.starts_with(family))
}

/// The reading of an Anthropic answer.
fn anthropic_reply(answer: &Json) -> Result<Reply, String> {
    let content = answer["content"]
        .as_array()
        .ok_or("the model API's answer has no content")?;
    let mut text = String::new();
    let mut thinking = String::new();
    let mut calls = Vec::new();
    for block in content {
        match block["type"].as_str() {
            Some("thinking") => {
                let summary = block["thinking"].as_str().unwrap_or_default().trim();
                if !summary.is_empty() {
                    if !thinking.is_empty() {
                        thinking.push('\n');
                    }
                    thinking.push_str(summary);
                }
            }
            Some("text") => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(block["text"].as_str().unwrap_or_default());
            }
            Some("tool_use") => calls.push(Call {
                id: block["id"].as_str().unwrap_or_default().to_string(),
                name: block["name"].as_str().unwrap_or_default().to_string(),
                arguments: block["input"].clone(),
            }),
            _ => {}
        }
    }
    let stop = match answer["stop_reason"].as_str() {
        Some("tool_use") => Stop::Tools,
        Some("max_tokens") => Stop::Truncated,
        Some("pause_turn") => Stop::Paused,
        Some("refusal") => Stop::Refused(
            answer["stop_details"]["category"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        ),
        _ if !calls.is_empty() => Stop::Tools,
        _ => Stop::Done,
    };
    Ok(Reply {
        thinking,
        text,
        calls,
        stop,
        native: Json::Array(content.clone()),
    })
}

/// The chat completions request for a conversation. A tool message carries text alone in
/// this format, so a picture a tool made follows its results as a user message.
fn openai_body(model: &Model, system: &str, tools: &[Definition], turns: &[Turn]) -> Json {
    let mut messages = vec![json!({"role": "system", "content": system})];
    for turn in turns {
        match turn {
            Turn::User(text) => messages.push(json!({"role": "user", "content": text})),
            Turn::Assistant(reply) => messages.push(reply.native.clone()),
            Turn::Returns(returns) => {
                for r in returns {
                    messages.push(json!({"role": "tool", "tool_call_id": r.id, "content": r.text}));
                }
                let pictures: Vec<Json> = returns
                    .iter()
                    .filter_map(|r| r.image.as_ref())
                    .map(|png| {
                        json!({
                            "type": "image_url",
                            "image_url": {"url": format!("data:image/png;base64,{}", b64(png))},
                        })
                    })
                    .collect();
                if !pictures.is_empty() {
                    let mut content =
                        vec![json!({"type": "text", "text": "The picture the tool returned:"})];
                    content.extend(pictures);
                    messages.push(json!({"role": "user", "content": content}));
                }
            }
        }
    }
    let tools: Vec<Json> = tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {"name": t.name, "description": t.description, "parameters": t.parameters},
            })
        })
        .collect();
    let mut body = json!({"model": model.name, "messages": messages, "tools": tools});
    if let Some(effort) = openai_effort(&model.effort) {
        body["reasoning_effort"] = json!(effort);
    }
    body
}

/// The `reasoning_effort` a chat completions request carries for a configured effort: the
/// format has three levels, so the two above `high` are sent as it, and none is sent where
/// the configuration names none, since a server that does not reason refuses the field.
fn openai_effort(effort: &str) -> Option<&'static str> {
    match effort {
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "xhigh" | "max" => Some("high"),
        _ => None,
    }
}

/// The reading of a chat completions answer. Arguments arrive as a JSON string, or as an
/// object from servers that do not follow that; a call without an id is given one.
fn openai_reply(answer: &Json) -> Result<Reply, String> {
    let choice = &answer["choices"][0];
    let message = &choice["message"];
    if !message.is_object() {
        return Err("the model API's answer has no message".to_string());
    }
    let mut native = message.clone();
    let mut calls = Vec::new();
    if let Some(list) = native["tool_calls"].as_array_mut() {
        for (index, call) in list.iter_mut().enumerate() {
            if call["id"].as_str().is_none_or(str::is_empty) {
                call["id"] = json!(format!("call_{index}"));
            }
            call["type"] = json!("function");
            let function = &call["function"];
            let arguments = match &function["arguments"] {
                Json::String(text) => serde_json::from_str(text).unwrap_or_else(|_| json!({})),
                Json::Object(map) => Json::Object(map.clone()),
                _ => json!({}),
            };
            calls.push(Call {
                id: call["id"].as_str().unwrap_or_default().to_string(),
                name: function["name"].as_str().unwrap_or_default().to_string(),
                arguments,
            });
            if !call["function"]["arguments"].is_string() {
                let text = call["function"]["arguments"].to_string();
                call["function"]["arguments"] = json!(text);
            }
        }
    }
    let stop = match choice["finish_reason"].as_str() {
        _ if !calls.is_empty() => Stop::Tools,
        Some("length") => Stop::Truncated,
        Some("content_filter") => Stop::Refused(String::new()),
        _ => Stop::Done,
    };
    Ok(Reply {
        thinking: message["reasoning_content"]
            .as_str()
            .or_else(|| message["reasoning"].as_str())
            .unwrap_or_default()
            .to_string(),
        text: message["content"].as_str().unwrap_or_default().to_string(),
        calls,
        stop,
        native,
    })
}

/// Ask the model for the next turn of a conversation.
pub async fn complete(
    model: &Model,
    system: &str,
    tools: &[Definition],
    turns: &[Turn],
) -> Result<Reply, String> {
    let base = model.endpoint.trim_end_matches('/');
    let body = request_body(model, system, tools, turns);
    match model.provider.wire {
        Wire::Anthropic => {
            anthropic_reply(&ask(&format!("{base}/v1/messages"), &headers(model), &body).await?)
        }
        Wire::OpenAi => {
            openai_reply(&ask(&format!("{base}/chat/completions"), &headers(model), &body).await?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, name: &str) -> Model {
        let provider = Provider::named(provider).unwrap();
        Model {
            provider,
            endpoint: provider.endpoint.to_string(),
            name: name.to_string(),
            api_key: "key".to_string(),
            vision: true,
            effort: "low".to_string(),
        }
    }

    fn tool() -> Definition {
        Definition {
            name: "look",
            description: "Read the screen.",
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    #[test]
    fn every_provider_has_an_id_of_its_own_and_an_address_or_is_local() {
        for (index, provider) in PROVIDERS.iter().enumerate() {
            assert!(
                PROVIDERS[..index]
                    .iter()
                    .all(|other| other.id != provider.id),
                "{}",
                provider.id
            );
            assert!(
                provider.local || provider.endpoint.starts_with("https://"),
                "{}",
                provider.id
            );
            assert_eq!(Provider::named(provider.id), Some(provider));
        }
        assert_eq!(Provider::named("nobody"), None);
    }

    #[test]
    fn an_anthropic_turn_is_replayed_as_it_came() {
        let answer = json!({
            "content": [
                {"type": "thinking", "thinking": "The window list first.", "signature": "sig"},
                {"type": "text", "text": "Looking."},
                {"type": "tool_use", "id": "toolu_1", "name": "look", "input": {"window": 3}},
            ],
            "stop_reason": "tool_use",
        });
        let reply = anthropic_reply(&answer).unwrap();
        assert_eq!(reply.text, "Looking.");
        assert_eq!(reply.thinking, "The window list first.");
        assert_eq!(reply.stop, Stop::Tools);
        assert_eq!(reply.calls[0].arguments, json!({"window": 3}));
        let turns = [
            Turn::User("do it".to_string()),
            Turn::Assistant(reply),
            Turn::Returns(vec![Return {
                id: "toolu_1".to_string(),
                text: "ok".to_string(),
                image: Some(vec![1, 2, 3]),
                failed: false,
            }]),
        ];
        let body = anthropic_body(
            &model("anthropic", "claude-opus-5-5"),
            "sys",
            &[tool()],
            &turns,
        );
        assert_eq!(body["messages"][1]["content"], answer["content"]);
        let result = &body["messages"][2]["content"][0];
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["tool_use_id"], "toolu_1");
        assert_eq!(result["content"][1]["source"]["data"], "AQID");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["output_config"]["effort"], "low");
        assert_eq!(body["thinking"]["display"], "summarized");
        let old = anthropic_body(
            &model("anthropic", "claude-haiku-4-5"),
            "sys",
            &[tool()],
            &[],
        );
        assert!(old.get("thinking").is_none() && old.get("output_config").is_none());
    }

    #[test]
    fn the_fallback_is_asked_only_of_anthropics_own_endpoint() {
        let mut proxied = model("anthropic", "claude-opus-5-5");
        proxied.endpoint = "https://proxy.example".to_string();
        assert!(!anthropic_falls_back(&proxied));
        assert!(!anthropic_falls_back(&model(
            "anthropic",
            "claude-haiku-5-5"
        )));
        assert!(anthropic_falls_back(&model(
            "anthropic",
            "claude-fable-5-2"
        )));
    }

    #[test]
    fn a_refusal_and_a_cut_turn_are_told_apart_from_an_answer() {
        let refused =
            json!({"content": [], "stop_reason": "refusal", "stop_details": {"category": "cyber"}});
        assert_eq!(
            anthropic_reply(&refused).unwrap().stop,
            Stop::Refused("cyber".to_string())
        );
        let cut = json!({"content": [{"type": "text", "text": "a"}], "stop_reason": "max_tokens"});
        assert_eq!(anthropic_reply(&cut).unwrap().stop, Stop::Truncated);
        let done = json!({"content": [{"type": "text", "text": "a"}], "stop_reason": "end_turn"});
        assert_eq!(anthropic_reply(&done).unwrap().stop, Stop::Done);
    }

    #[test]
    fn a_chat_completions_call_is_read_whether_its_arguments_are_text_or_an_object() {
        let answer = json!({"choices": [{"finish_reason": "tool_calls", "message": {
            "role": "assistant", "content": null,
            "tool_calls": [
                {"id": "a", "function": {"name": "click", "arguments": "{\"at\":[1,2]}"}},
                {"function": {"name": "look", "arguments": {"window": 4}}},
            ],
        }}]});
        let reply = openai_reply(&answer).unwrap();
        assert_eq!(reply.stop, Stop::Tools);
        assert_eq!(reply.calls[0].arguments, json!({"at": [1, 2]}));
        assert_eq!(reply.calls[1].id, "call_1");
        assert_eq!(reply.calls[1].arguments, json!({"window": 4}));
        assert_eq!(
            reply.native["tool_calls"][1]["function"]["arguments"],
            "{\"window\":4}"
        );
    }

    #[test]
    fn a_picture_follows_its_tool_results_as_a_user_message() {
        let turns = [Turn::Returns(vec![Return {
            id: "a".to_string(),
            text: "The screen.".to_string(),
            image: Some(vec![1, 2, 3]),
            failed: false,
        }])];
        let body = openai_body(&model("openai", "m"), "sys", &[tool()], &turns);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "tool");
        assert_eq!(
            body["messages"][2]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AQID"
        );
        assert_eq!(body["tools"][0]["function"]["name"], "look");
        assert_eq!(body["reasoning_effort"], "low");
        let mut plain = model("ollama", "m");
        plain.effort = String::new();
        assert!(
            openai_body(&plain, "sys", &[tool()], &turns)
                .get("reasoning_effort")
                .is_none()
        );
        assert_eq!(openai_effort("max"), Some("high"));
    }
}
