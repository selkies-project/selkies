/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The model configurations an agent can run on: the server's own and the ones saved from
//! the web interface.
//!
//! The server's settings (`--pelorus-provider` and the rest) make one configuration, `server`,
//! which is listed whenever it names enough to run and cannot be edited from a browser. A
//! person adds others from the interface: a name, a provider, an endpoint, a model, a key, an
//! effort, and whether the model sees. Those are kept in `pelorus.json` under the server's
//! configuration directory, readable by its user alone since it holds keys, and one of them
//! can be marked the default a run takes when it names none.
//!
//! An operator who sets `--pelorus` to `true|locked` fixes the session to the `server`
//! configuration: the saved file is neither read nor written, nothing can be added, changed,
//! or listed from a provider, and a run takes no change to its model or effort. A saved
//! endpoint is an address this server fetches for whoever saved it, which in a session that is
//! meant to reach one application and nothing else is more than its user holds otherwise;
//! the lock is how such a session keeps that. A key is taken and never given back: a listing
//! says only whether one is set.

use std::path::PathBuf;
use std::sync::Mutex;

use axum::body::Bytes;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use super::model::{self, Key, Model, PROVIDERS, Provider};
use super::{refused, tools};
use crate::http::auth::Ceiling;
use crate::http::routes::json_error;
use crate::settings::settings;

/// The id of the configuration the server's settings make.
pub const SERVER: &str = "server";

/// The efforts a configuration may name; `default` leaves it to the model.
pub const EFFORTS: &[&str] = &["default", "low", "medium", "high", "xhigh", "max"];

/// The most configurations kept.
const SAVED_MOST: usize = 32;

/// One saved configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub id: String,
    pub name: String,
    pub provider: String,
    #[serde(default)]
    pub endpoint: String,
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub effort: String,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub default: bool,
}

impl Saved {
    /// The model this configuration runs, or why it cannot run one.
    pub fn model(&self) -> Result<Model, String> {
        let provider = Provider::named(&self.provider).ok_or(UNKNOWN_PROVIDER)?;
        let or_default = |value: &str, default: &str| {
            let value = value.trim();
            if value.is_empty() { default } else { value }.to_string()
        };
        let name = or_default(&self.model, provider.model);
        if name.is_empty() {
            return Err(format!("\"{}\" names no model.", self.name));
        }
        let endpoint = or_default(&self.endpoint, provider.endpoint);
        if endpoint.is_empty() {
            return Err(format!("\"{}\" names no endpoint.", self.name));
        }
        if self.api_key.is_empty() && provider.key == Key::Required {
            return Err(format!("\"{}\" has no API key.", self.name));
        }
        if !self.effort.is_empty() && !EFFORTS.contains(&self.effort.as_str()) {
            return Err(UNKNOWN_EFFORT.to_string());
        }
        Ok(Model {
            provider,
            endpoint,
            name,
            api_key: self.api_key.clone(),
            vision: self.vision,
            effort: self.effort.trim_start_matches("default").to_string(),
        })
    }

    /// The configuration as a client is shown it: everything but the key.
    fn listed(&self, editable: bool) -> Json {
        let provider = Provider::named(&self.provider);
        let shown = |value: &str, default: &str| {
            if value.trim().is_empty() {
                default
            } else {
                value
            }
            .to_string()
        };
        json!({
            "id": self.id,
            "name": self.name,
            "provider": self.provider,
            "endpoint": shown(&self.endpoint, provider.map_or("", |p| p.endpoint)),
            "model": shown(&self.model, provider.map_or("", |p| p.model)),
            "effort": shown(&self.effort, "default"),
            "vision": self.vision,
            "has_key": !self.api_key.is_empty(),
            "default": self.default,
            "editable": editable,
            "usable": self.model().is_ok(),
        })
    }
}

/// What a caller is told of a provider or an effort the server does not know.
const UNKNOWN_PROVIDER: &str = "That provider is not one this server knows.";
const UNKNOWN_EFFORT: &str = "the effort is default, low, medium, high, xhigh, or max";

/// Whether the operator fixed the session to the server's configuration.
pub fn locked() -> bool {
    settings().pelorus().locked
}

/// The configuration the server's settings make.
fn from_settings() -> Saved {
    let s = settings();
    Saved {
        id: SERVER.to_string(),
        name: "Server".to_string(),
        provider: s.pelorus_provider().to_string(),
        endpoint: s.pelorus_endpoint().to_string(),
        model: s.pelorus_model().to_string(),
        api_key: s.pelorus_api_key().to_string(),
        effort: s.pelorus_effort().to_string(),
        vision: s.pelorus_vision().on,
        default: false,
    }
}

/// The file the saved configurations are kept in: under `XDG_CONFIG_HOME`, else `~/.config`.
fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("selkies").join("pelorus.json"))
}

/// The saved configurations, read from the file once and kept here after.
static SAVED: Mutex<Option<Vec<Saved>>> = Mutex::new(None);

/// Run `with` on the saved configurations, writing them back where it changed them. Under
/// the lock there are none and nothing is written.
fn saved<T>(with: impl FnOnce(&mut Vec<Saved>) -> T) -> T {
    if locked() {
        return with(&mut Vec::new());
    }
    let mut slot = SAVED.lock().unwrap_or_else(|p| p.into_inner());
    let list = slot.get_or_insert_with(|| {
        path()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<Vec<Saved>>(&bytes).ok())
            .unwrap_or_default()
    });
    let before = list.clone();
    let result = with(list);
    if *list != before {
        write(list);
    }
    result
}

/// Write the saved configurations, readable by the server's user alone; a failure is logged
/// and the configurations stay as they are in memory.
fn write(list: &[Saved]) {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let Some(path) = path() else {
        log::warn!(target: "pelorus", "model configurations are not kept: no configuration directory");
        return;
    };
    let result = (|| {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let staged = path.with_extension("json.new");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&staged)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer_pretty(&mut file, list).map_err(std::io::Error::other)?;
        std::fs::rename(&staged, &path)
    })();
    if let Err(e) = result {
        log::warn!(target: "pelorus", "model configurations could not be written to {}: {e}", path.display());
    }
}

/// Every configuration, the server's first where it can run, with whether each can be edited.
fn all() -> Vec<(Saved, bool)> {
    let server = from_settings();
    let mut list = Vec::new();
    if server.model().is_ok() {
        list.push((server, false));
    }
    list.extend(saved(|saved| saved.clone()).into_iter().map(|c| (c, true)));
    if !list.iter().any(|(c, _)| c.default)
        && let Some((first, _)) = list.first_mut()
    {
        first.default = true;
    }
    list
}

/// The model a run takes: the configuration `id` names, else the default one, with the model
/// name and effort a request asks for in place of its own. Under the lock a request changes
/// neither.
pub fn resolve(
    id: Option<&str>,
    name: Option<&str>,
    effort: Option<&str>,
    vision: Option<bool>,
) -> Result<Model, String> {
    let list = all();
    let config = match id {
        Some(id) => list.iter().find(|(c, _)| c.id == id).map(|(c, _)| c),
        None => list.iter().find(|(c, _)| c.default).map(|(c, _)| c),
    }
    .ok_or_else(|| match id {
        Some(id) => format!("There is no model configuration \"{id}\"."),
        None => "No model is configured for the agent: add one in the settings, or set \
                 --pelorus-api-key (SELKIES_PELORUS_API_KEY) on the server."
            .to_string(),
    })?;
    let mut config = config.clone();
    let changes = name.is_some() || effort.is_some() || vision.is_some();
    if changes && locked() {
        return Err("The model is fixed by the server's settings in this session.".to_string());
    }
    if let Some(name) = name {
        config.model = name.to_string();
    }
    if let Some(effort) = effort {
        config.effort = effort.to_string();
    }
    if let Some(vision) = vision {
        config.vision = vision;
    }
    config.model()
}

/// The text of a field of a request, trimmed; `None` where it is absent.
fn field<'a>(request: &'a Json, name: &str) -> Option<&'a str> {
    request.get(name).and_then(Json::as_str).map(str::trim)
}

/// A configuration from a request's fields over `base`: a field the request leaves out keeps
/// what `base` holds, which is how an edit keeps a key it does not send again.
fn merged(base: Saved, request: &Json) -> Result<Saved, String> {
    let mut config = base;
    if let Some(name) = field(request, "name") {
        config.name = name.chars().take(80).collect();
    }
    if let Some(provider) = field(request, "provider") {
        config.provider = provider.to_string();
    }
    if let Some(endpoint) = field(request, "endpoint") {
        config.endpoint = endpoint.to_string();
    }
    if let Some(model) = field(request, "model") {
        config.model = model.to_string();
    }
    if let Some(key) = field(request, "api_key").filter(|key| !key.is_empty()) {
        config.api_key = key.to_string();
    }
    if let Some(effort) = field(request, "effort") {
        config.effort = effort.to_string();
    }
    if let Some(vision) = request.get("vision").and_then(Json::as_bool) {
        config.vision = vision;
    }
    if let Some(default) = request.get("default").and_then(Json::as_bool) {
        config.default = default;
    }
    if config.name.is_empty() {
        return Err("A configuration needs a name.".to_string());
    }
    let provider = Provider::named(&config.provider).ok_or(UNKNOWN_PROVIDER)?;
    if !config.effort.is_empty() && !EFFORTS.contains(&config.effort.as_str()) {
        return Err(UNKNOWN_EFFORT.to_string());
    }
    if !config.endpoint.is_empty()
        && !config.endpoint.starts_with("http://")
        && !config.endpoint.starts_with("https://")
    {
        return Err("The endpoint is an http:// or https:// address.".to_string());
    }
    if config.model.is_empty() && provider.model.is_empty() {
        return Err(format!("Name a model for {}.", provider.label));
    }
    if config.endpoint.is_empty() && provider.endpoint.is_empty() {
        return Err(format!("{} needs an endpoint.", provider.label));
    }
    Ok(config)
}

/// The answer to every request that would change or probe a configuration under the lock.
fn fixed() -> Response {
    json_error(
        StatusCode::FORBIDDEN,
        "The model configuration is fixed by the server's settings in this session.",
    )
}

/// The body of a request as JSON; `None` where it is not JSON.
fn parsed(body: &Bytes) -> Option<Json> {
    serde_json::from_slice(body).ok()
}

/// The answer to a body that is not JSON.
fn not_json() -> Response {
    json_error(StatusCode::BAD_REQUEST, "the body must be JSON")
}

/// `GET /pelorus/api/models`: the configurations a run can name, never their keys, and what
/// an interface needs to draw itself: whether the configuration is locked, what the session
/// offers the agent, and the providers the server knows, each with its address, its default
/// model, whether it takes a key, and whether its address is a person's to set, so a form asks
/// only for what is not public.
pub async fn handle_list(ceiling: Ceiling) -> Response {
    if ceiling.is_viewer() {
        return refused();
    }
    let models: Vec<Json> = all()
        .iter()
        .map(|(c, editable)| c.listed(*editable))
        .collect();
    let capabilities = tools::capabilities(false).await;
    let providers: Vec<Json> = PROVIDERS
        .iter()
        .map(|p| {
            json!({
                "name": p.id,
                "label": p.label,
                "endpoint": p.endpoint,
                "model": p.model,
                "key": match p.key {
                    Key::Required => "required",
                    Key::Optional => "optional",
                    Key::None => "none",
                },
                "local": p.local,
            })
        })
        .collect();
    axum::Json(json!({
        "locked": locked(),
        "models": models,
        "providers": providers,
        "efforts": EFFORTS,
        "shell": capabilities.shell,
        "windows": capabilities.windows,
        "max_steps": settings().pelorus_max_steps(),
    }))
    .into_response()
}

/// `POST /pelorus/api/models`: save a new configuration; answers with it as listed.
pub async fn handle_create(ceiling: Ceiling, body: Bytes) -> Response {
    if ceiling.is_viewer() {
        return refused();
    }
    if locked() {
        return fixed();
    }
    let Some(request) = parsed(&body) else {
        return not_json();
    };
    let base = Saved {
        id: format!("{:012x}", rand::random::<u64>() & 0xffff_ffff_ffff),
        ..Saved::default()
    };
    let config = match merged(base, &request) {
        Ok(config) => config,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &e),
    };
    let stored = saved(|list| {
        if list.len() >= SAVED_MOST {
            return false;
        }
        if config.default {
            list.iter_mut().for_each(|c| c.default = false);
        }
        list.push(config.clone());
        true
    });
    if !stored {
        return json_error(
            StatusCode::CONFLICT,
            "No more configurations can be saved; delete one first.",
        );
    }
    axum::Json(config.listed(true)).into_response()
}

/// `PUT /pelorus/api/models/{id}`: change a saved configuration; a field left out keeps its
/// value, the key among them.
pub async fn handle_update(ceiling: Ceiling, Path(id): Path<String>, body: Bytes) -> Response {
    if ceiling.is_viewer() {
        return refused();
    }
    if locked() {
        return fixed();
    }
    let Some(request) = parsed(&body) else {
        return not_json();
    };
    let result = saved(|list| {
        let index = list.iter().position(|c| c.id == id).ok_or((
            StatusCode::NOT_FOUND,
            "That configuration is the server's own or does not exist.".to_string(),
        ))?;
        let config =
            merged(list[index].clone(), &request).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        if config.default {
            list.iter_mut().for_each(|c| c.default = false);
        }
        list[index] = config.clone();
        Ok(config)
    });
    match result {
        Ok(config) => axum::Json(config.listed(true)).into_response(),
        Err((status, message)) => json_error(status, &message),
    }
}

/// `DELETE /pelorus/api/models/{id}`: forget a saved configuration.
pub async fn handle_delete(ceiling: Ceiling, Path(id): Path<String>) -> Response {
    if ceiling.is_viewer() {
        return refused();
    }
    if locked() {
        return fixed();
    }
    let removed = saved(|list| {
        let before = list.len();
        list.retain(|c| c.id != id);
        list.len() != before
    });
    if removed {
        axum::Json(json!({"status": "ok"})).into_response()
    } else {
        json_error(
            StatusCode::NOT_FOUND,
            "That configuration is the server's own or does not exist.",
        )
    }
}

/// `POST /pelorus/api/models/names`: the models a provider offers, for a form's list: with an
/// `id`, as that configuration reaches it, any other field of the request in place of the
/// saved one (an endpoint or key typed and not yet saved).
pub async fn handle_names(ceiling: Ceiling, body: Bytes) -> Response {
    if ceiling.is_viewer() {
        return refused();
    }
    if locked() {
        return fixed();
    }
    let Some(request) = parsed(&body) else {
        return not_json();
    };
    let base = field(&request, "id")
        .and_then(|id| all().into_iter().find(|(c, _)| c.id == id))
        .map(|(c, _)| c)
        .unwrap_or_default();
    let mut config = base;
    if let Some(provider) = field(&request, "provider") {
        config.provider = provider.to_string();
    }
    if let Some(endpoint) = field(&request, "endpoint") {
        config.endpoint = endpoint.to_string();
    }
    if let Some(key) = field(&request, "api_key").filter(|key| !key.is_empty()) {
        config.api_key = key.to_string();
    }
    let Some(provider) = Provider::named(&config.provider) else {
        return json_error(StatusCode::BAD_REQUEST, UNKNOWN_PROVIDER);
    };
    let endpoint = if config.endpoint.trim().is_empty() {
        provider.endpoint.to_string()
    } else {
        config.endpoint.trim().to_string()
    };
    if endpoint.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "Enter the endpoint first.");
    }
    let probe = Model {
        provider,
        endpoint,
        name: String::new(),
        api_key: config.api_key,
        vision: false,
        effort: String::new(),
    };
    match model::list_models(&probe).await {
        Ok(names) => axum::Json(json!({"models": names})).into_response(),
        Err(e) => json_error(StatusCode::BAD_GATEWAY, &e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved_config() -> Saved {
        Saved {
            id: "abc".to_string(),
            name: "Local".to_string(),
            provider: "anthropic".to_string(),
            api_key: "secret".to_string(),
            ..Saved::default()
        }
    }

    #[test]
    fn a_configuration_fills_in_its_providers_own_endpoint_and_model() {
        let model = saved_config().model().unwrap();
        assert_eq!(model.endpoint, "https://api.anthropic.com");
        assert_eq!(model.name, "claude-opus-5-5");
        assert_eq!(model.effort, "");
        let mut local = saved_config();
        local.provider = "ollama".to_string();
        local.api_key.clear();
        assert!(local.model().unwrap_err().contains("names no model"));
        local.model = "gemma".to_string();
        assert_eq!(local.model().unwrap().endpoint, "http://localhost:11434/v1");
    }

    #[test]
    fn a_listing_never_carries_the_key() {
        let listed = saved_config().listed(true);
        assert_eq!(listed["has_key"], true);
        assert!(!listed.to_string().contains("secret"));
        assert_eq!(listed["effort"], "default");
    }

    #[test]
    fn an_edit_keeps_what_it_does_not_send() {
        let edited = merged(
            saved_config(),
            &json!({"name": "Renamed", "effort": "low", "api_key": ""}),
        )
        .unwrap();
        assert_eq!(edited.name, "Renamed");
        assert_eq!(edited.api_key, "secret");
        assert_eq!(edited.effort, "low");
        assert!(merged(saved_config(), &json!({"effort": "extreme"})).is_err());
        assert!(merged(saved_config(), &json!({"endpoint": "file:///etc"})).is_err());
        assert!(merged(saved_config(), &json!({"provider": "ollama"})).is_err());
        assert!(merged(saved_config(), &json!({"provider": "custom", "model": "m"})).is_err());
        assert!(merged(saved_config(), &json!({"provider": "nobody"})).is_err());
        assert!(merged(Saved::default(), &json!({"provider": "anthropic"})).is_err());
    }
}
