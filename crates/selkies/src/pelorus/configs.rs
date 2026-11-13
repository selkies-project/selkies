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
//! says only whether one is set, and a stored key is sent only to the provider and endpoint it
//! was stored with, so asking a provider for its models at another address takes a key of its
//! own ([`probe`]).
//!
//! A key saved from the interface is kept one of two ways. Kept on the server, it is in the
//! file as typed, and any caller holding the controller's credential can run on it: the
//! dashboard of any browser, a script against `/pelorus/api/run`. Sealed, the file holds
//! only what the browser that saved it encrypted (`sealed`), under a key that browser keeps
//! and never sends; the browser opens it and sends the key with each run and each model
//! listing, which the server uses for that request alone and never writes. The file, its
//! backups, and anything of the session user's that reads it (the agent's own shell among
//! them) then find nothing they can use. It is no defense against a server someone controls,
//! which sees every key a run is sent and serves the page that opens them, and a configuration
//! sealed by one browser runs from that browser alone. Sealed and kept are exclusive: saving a
//! key one way drops the other.

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

/// The most characters a configuration's name keeps; a longer one is cut.
const NAME_MOST: usize = 80;

/// The most characters an endpoint may hold.
const ENDPOINT_MOST: usize = 2048;

/// The most characters a model's name may hold, saved or asked for by a run.
pub(super) const MODEL_MOST: usize = 256;

/// The most characters a key may hold, typed, opened, or sealed.
pub(super) const KEY_MOST: usize = 4096;

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
    /// The key as the browser that saved it sealed it, opaque here; empty where the key, if
    /// any, is `api_key`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sealed: String,
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
        if self.api_key.is_empty() && self.sealed.is_empty() && provider.key == Key::Required {
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

    /// The configuration as a client is shown it: everything but the key, with the sealed one
    /// for the browser that holds what opens it.
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
            "has_key": !self.api_key.is_empty() || !self.sealed.is_empty(),
            "sealed": self.sealed,
            "default": self.default,
            "editable": editable,
            "usable": self.model().is_ok(),
        })
    }
}

/// What a caller is told of a provider, an effort, or an endpoint the server does not take.
const UNKNOWN_PROVIDER: &str = "That provider is not one this server knows.";
const UNKNOWN_EFFORT: &str = "the effort is default, low, medium, high, xhigh, or max";
const NOT_WEB_ADDRESS: &str = "The endpoint is an http:// or https:// address.";

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
        sealed: String::new(),
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
/// neither. A sealed configuration runs on the `key` its browser opened and sent, and on
/// nothing else; a kept one ignores any key sent.
pub fn resolve(
    id: Option<&str>,
    name: Option<&str>,
    effort: Option<&str>,
    vision: Option<bool>,
    key: Option<&str>,
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
    let mut config = opened(config.clone(), key)?;
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

/// A configuration ready to run: a sealed one with the `key` its browser sent in its place,
/// failing where none was sent; a kept one as it is, whatever was sent.
fn opened(mut config: Saved, key: Option<&str>) -> Result<Saved, String> {
    if !config.sealed.is_empty() {
        let Some(key) = key.filter(|key| !key.is_empty()) else {
            return Err(format!(
                "The key of \"{}\" is sealed by the browser that saved it: run it from that browser.",
                config.name
            ));
        };
        config.api_key = key.to_string();
        config.sealed.clear();
    }
    Ok(config)
}

/// The text of a field of a request, trimmed; `None` where it is absent.
fn field<'a>(request: &'a Json, name: &str) -> Option<&'a str> {
    request.get(name).and_then(Json::as_str).map(str::trim)
}

/// A field of a request as [`field`] reads it, refused where it holds more than `most`
/// characters: an endpoint, a model name, or a key that long is no address, model, or key,
/// and cutting one short would store or send something the person never typed.
pub(super) fn bounded<'a>(
    request: &'a Json,
    name: &str,
    most: usize,
) -> Result<Option<&'a str>, String> {
    match field(request, name) {
        Some(value) if value.chars().count() > most => {
            Err(format!("`{name}` is longer than {most} characters."))
        }
        value => Ok(value),
    }
}

/// Whether `endpoint` is an address the server may fetch: `http://` or `https://`.
fn is_web_address(endpoint: &str) -> bool {
    endpoint.starts_with("http://") || endpoint.starts_with("https://")
}

/// The address a configuration of `provider` reaches at `endpoint`: the provider's own where
/// it names none.
fn reached(endpoint: &str, provider: &Provider) -> String {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        provider.endpoint
    } else {
        endpoint
    }
    .to_string()
}

/// A configuration from a request's fields over `base`: a field the request leaves out keeps
/// what `base` holds, which is how an edit keeps a key it does not send again.
fn merged(base: Saved, request: &Json) -> Result<Saved, String> {
    let mut config = base;
    if let Some(name) = field(request, "name") {
        config.name = name.chars().take(NAME_MOST).collect();
    }
    if let Some(provider) = field(request, "provider") {
        config.provider = provider.to_string();
    }
    if let Some(endpoint) = bounded(request, "endpoint", ENDPOINT_MOST)? {
        config.endpoint = endpoint.to_string();
    }
    if let Some(model) = bounded(request, "model", MODEL_MOST)? {
        config.model = model.to_string();
    }
    if let Some(key) = bounded(request, "api_key", KEY_MOST)?.filter(|key| !key.is_empty()) {
        config.api_key = key.to_string();
        config.sealed.clear();
    }
    if let Some(sealed) = field(request, "sealed") {
        if !is_sealed_key(sealed) {
            return Err("The sealed key does not read.".to_string());
        }
        config.sealed = sealed.to_string();
        if !sealed.is_empty() {
            config.api_key.clear();
        }
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
    if !config.endpoint.is_empty() && !is_web_address(&config.endpoint) {
        return Err(NOT_WEB_ADDRESS.to_string());
    }
    if config.model.is_empty() && provider.model.is_empty() {
        return Err(format!("Name a model for {}.", provider.label));
    }
    if config.endpoint.is_empty() && provider.endpoint.is_empty() {
        return Err(format!("{} needs an endpoint.", provider.label));
    }
    Ok(config)
}

/// Whether `sealed` is what a browser seals a key into, or empty: a bounded run of the
/// characters its encoding uses. The server never reads it beyond that.
fn is_sealed_key(sealed: &str) -> bool {
    sealed.len() <= KEY_MOST
        && sealed.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'+' | b'/' | b'=')
        })
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
/// saved one (an endpoint or key typed and not yet saved), as [`probe`] settles it.
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
    let probe = match probe(&base, &request) {
        Ok(probe) => probe,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &e),
    };
    match model::list_models(&probe).await {
        Ok(names) => axum::Json(json!({"models": names})).into_response(),
        Err(e) => json_error(StatusCode::BAD_GATEWAY, &e),
    }
}

/// The model a listing asks: the saved configuration `base` with the request's provider,
/// endpoint, and key in place of its own, the endpoint held to what [`merged`] would save.
///
/// A stored key goes only to the address it was stored for. A probe that names another
/// provider or endpoint runs on the key it sends or on none, so it cannot carry the server's
/// own key, which no edit reaches, or a kept one to an address of the caller's choosing. A
/// sealed configuration has no stored key here: it is asked with the key its browser sends, or
/// none.
fn probe(base: &Saved, request: &Json) -> Result<Model, String> {
    let named = field(request, "provider").unwrap_or(&base.provider);
    let provider = Provider::named(named).ok_or(UNKNOWN_PROVIDER)?;
    let endpoint = reached(
        bounded(request, "endpoint", ENDPOINT_MOST)?.unwrap_or(&base.endpoint),
        provider,
    );
    if endpoint.is_empty() {
        return Err("Enter the endpoint first.".to_string());
    }
    if !is_web_address(&endpoint) {
        return Err(NOT_WEB_ADDRESS.to_string());
    }
    let stored_for_this = base.sealed.is_empty()
        && Provider::named(&base.provider).is_some_and(|saved| {
            saved.id == provider.id && reached(&base.endpoint, saved) == endpoint
        });
    let stored = if stored_for_this {
        base.api_key.as_str()
    } else {
        ""
    };
    let api_key = bounded(request, "api_key", KEY_MOST)?
        .filter(|key| !key.is_empty())
        .unwrap_or(stored)
        .to_string();
    Ok(Model {
        provider,
        endpoint,
        name: String::new(),
        api_key,
        vision: false,
        effort: String::new(),
    })
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
    fn a_sealed_key_and_a_kept_one_replace_each_other() {
        let sealed = merged(saved_config(), &json!({"sealed": "v1.ab12.iv.data+/="})).unwrap();
        assert_eq!(sealed.api_key, "");
        assert_eq!(sealed.sealed, "v1.ab12.iv.data+/=");
        let listed = sealed.listed(true);
        assert_eq!(listed["has_key"], true);
        assert_eq!(listed["usable"], true);
        let kept = merged(sealed, &json!({"api_key": "fresh"})).unwrap();
        assert_eq!(kept.api_key, "fresh");
        assert_eq!(kept.sealed, "");
        assert!(merged(saved_config(), &json!({"sealed": "<script>"})).is_err());
        assert!(merged(saved_config(), &json!({"sealed": "a".repeat(5000)})).is_err());
    }

    #[test]
    fn a_sealed_configuration_runs_only_on_the_key_its_browser_sends() {
        let sealed = merged(saved_config(), &json!({"sealed": "v1.ab12.iv.data"})).unwrap();
        assert!(opened(sealed.clone(), None).unwrap_err().contains("sealed"));
        assert!(opened(sealed.clone(), Some("")).is_err());
        let run = opened(sealed, Some("opened")).unwrap();
        assert_eq!(run.model().unwrap().api_key, "opened");
        let kept = opened(saved_config(), Some("sent")).unwrap();
        assert_eq!(kept.api_key, "secret");
    }

    #[test]
    fn a_listing_probe_sends_a_stored_key_only_to_the_address_it_was_stored_for() {
        let base = saved_config();
        let key = |request: Json| probe(&base, &request).map(|model| model.api_key);
        assert_eq!(key(json!({"id": "abc"})).unwrap(), "secret");
        assert_eq!(
            key(json!({"endpoint": "https://api.anthropic.com"})).unwrap(),
            "secret",
            "the provider's own address, spelled out, is the same one"
        );
        assert_eq!(
            key(json!({"endpoint": "https://elsewhere.example/v1"})).unwrap(),
            ""
        );
        assert_eq!(
            key(json!({"endpoint": "https://elsewhere.example/v1", "api_key": "typed"})).unwrap(),
            "typed"
        );
        assert_eq!(key(json!({"provider": "openrouter"})).unwrap(), "");
        assert!(key(json!({"endpoint": "file:///etc/passwd"})).is_err());
        assert!(key(json!({"endpoint": "gopher://host"})).is_err());
        assert!(key(json!({"api_key": "k".repeat(KEY_MOST + 1)})).is_err());
        let sealed = merged(saved_config(), &json!({"sealed": "v1.ab12.iv.data"})).unwrap();
        assert_eq!(probe(&sealed, &json!({})).unwrap().api_key, "");
        assert_eq!(
            probe(&sealed, &json!({"api_key": "opened"}))
                .unwrap()
                .api_key,
            "opened"
        );
    }

    #[test]
    fn a_saved_field_too_long_to_be_what_it_names_is_refused() {
        let long = |n: usize| "x".repeat(n);
        let endpoint = format!("https://{}", long(ENDPOINT_MOST));
        assert!(merged(saved_config(), &json!({"endpoint": endpoint})).is_err());
        assert!(merged(saved_config(), &json!({"model": long(MODEL_MOST + 1)})).is_err());
        assert!(merged(saved_config(), &json!({"api_key": long(KEY_MOST + 1)})).is_err());
        let named = merged(saved_config(), &json!({"name": long(NAME_MOST + 20)})).unwrap();
        assert_eq!(named.name.chars().count(), NAME_MOST, "a long name is cut");
        assert!(merged(saved_config(), &json!({"model": long(MODEL_MOST)})).is_ok());
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
