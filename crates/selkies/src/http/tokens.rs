/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Session tokens and the input authority they carry, shared by both transports.
//!
//! [`TokenTable`] is the live table `POST /api/tokens` and the tokens file provision: each
//! token's role, gamepad slots, and whether it holds `mk_control`, the keyboard and mouse
//! while one is named. Every transport authorizes a handshake and input against it here:
//! [`TokenTable::lookup`] compares every provisioned token in constant time, so the reply
//! timing tells a probing client neither how far its guess matched nor which entry it
//! resembles; [`TokenTable::mk_access_verdict`] is the one `MK_ACCESS` rule both transports
//! tell a client. A table replaces the last whole; nothing expires on its own, a token lives
//! until the next table omits it. The first table opens the configuration gate secure-mode
//! streaming waits on, and every table runs the [`ReconcileHook`]s the transports register
//! to hold their live clients to it. The tokens file is polled every [`TOKENS_FILE_POLL`]
//! rather than watched, so a rename over it or a Kubernetes secret update is seen too, and
//! refused unless root or this process's user owns it and nobody else can write to it.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use parking_lot::{Mutex, RwLock};
use serde_json::{Map, Value as Json, json};
use tokio::sync::watch;

use super::AppState;
use super::auth::ct_eq;
use crate::core::Role;

/// The kernel gamepads a session presents; a token's slots are one-based among them.
pub const GAMEPAD_SLOTS: u8 = 4;
/// How often the tokens file is checked for a change.
pub const TOKENS_FILE_POLL: Duration = Duration::from_secs(2);

/// One provisioned token's permissions, as both transports read them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenEntry {
    /// `Controller` only where the entry says so, `Viewer` for any other or none.
    pub role: Role,
    /// The one-based gamepad slots in the page's order, none for a token without pads.
    pub slots: Vec<u8>,
    /// Whether the entry asks for the keyboard and mouse (the JSON `true` alone).
    pub mk_control: bool,
    /// Whether the entry named `controller` or `viewer` at all.
    pub named_role: bool,
}

impl TokenEntry {
    /// The entry a table's JSON object describes.
    pub fn from_json(perms: &Map<String, Json>) -> TokenEntry {
        let role_text = perms.get("role").and_then(Json::as_str);
        TokenEntry {
            role: if role_text == Some("controller") {
                Role::Controller
            } else {
                Role::Viewer
            },
            slots: perms.get("slot").map(token_slots).unwrap_or_default(),
            mk_control: perms.get("mk_control") == Some(&Json::Bool(true)),
            named_role: matches!(role_text, Some("controller") | Some("viewer")),
        }
    }

    /// The `slot` a page is told: null for none, the number for one, the list for several.
    pub fn slot_json(&self) -> Json {
        match self.slots.as_slice() {
            [] => Json::Null,
            [one] => json!(one),
            many => json!(many),
        }
    }
}

/// The one-based gamepad slots a token entry's `slot` names, in its order.
///
/// An entry names one slot (1 to [`GAMEPAD_SLOTS`], or its decimal string), a list of them,
/// or none (null, 0, or an empty list). A value that is no such slot names none, a bool is
/// no slot, and a slot named twice counts once.
pub fn token_slots(value: &Json) -> Vec<u8> {
    let items: Vec<&Json> = match value {
        Json::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    let mut slots = Vec::new();
    for item in items {
        let slot = match item {
            Json::Bool(_) => None,
            Json::Number(n) => n.as_f64().map(|f| f as i64),
            Json::String(s) => s.trim().parse::<i64>().ok(),
            _ => None,
        };
        if let Some(slot) = slot
            && (1..=GAMEPAD_SLOTS as i64).contains(&slot)
            && !slots.contains(&(slot as u8))
        {
            slots.push(slot as u8);
        }
    }
    slots
}

/// A token entry's `slot` as the table keeps and pages are told it.
pub fn stored_slot(value: &Json) -> Json {
    match token_slots(value).as_slice() {
        [] => Json::Null,
        [one] => json!(one),
        many => json!(many),
    }
}

/// Why `table` is not a token table `/api/tokens` takes, or none when it is. The reason
/// names no token, since the tokens are secrets and it is logged.
pub fn token_table_error(table: &Json) -> Option<&'static str> {
    let Json::Object(entries) = table else {
        return Some("the table must be a JSON object");
    };
    if !entries.values().all(Json::is_object) {
        return Some("every token entry must be a JSON object");
    }
    None
}

/// What a transport does with its live clients when the table changes: close a revoked or
/// re-roled token's connection, tell the survivors their slots and `MK_ACCESS` verdict.
#[async_trait]
pub trait ReconcileHook: Send + Sync {
    async fn reconcile(&self);
}

struct TableState {
    tokens: HashMap<String, TokenEntry>,
    /// The holder of the keyboard and mouse while a table names one.
    active_mk_token: Option<String>,
}

struct Inner {
    state: RwLock<TableState>,
    gate: watch::Sender<bool>,
    hooks: Mutex<Vec<Arc<dyn ReconcileHook>>>,
}

/// The live token table, shared by the gate, the routes, and the transports.
#[derive(Clone)]
pub struct TokenTable(Arc<Inner>);

impl Default for TokenTable {
    fn default() -> Self {
        TokenTable::new()
    }
}

impl TokenTable {
    /// An empty table with its gate closed.
    pub fn new() -> TokenTable {
        let (gate, _) = watch::channel(false);
        TokenTable(Arc::new(Inner {
            state: RwLock::new(TableState {
                tokens: HashMap::new(),
                active_mk_token: None,
            }),
            gate,
            hooks: Mutex::new(Vec::new()),
        }))
    }

    /// The entry for a session token, compared in constant time over the whole table;
    /// none for an empty token or one that is not provisioned.
    pub fn lookup(&self, token: &str) -> Option<TokenEntry> {
        if token.is_empty() {
            return None;
        }
        let state = self.0.state.read();
        let mut found = None;
        for (known, entry) in &state.tokens {
            if ct_eq(token.as_bytes(), known.as_bytes()) {
                found = Some(entry.clone());
            }
        }
        found
    }

    /// The token holding the keyboard and mouse, while the table names one.
    pub fn active_mk_token(&self) -> Option<String> {
        self.0.state.read().active_mk_token.clone()
    }

    /// Every provisioned token with its entry.
    pub fn snapshot(&self) -> HashMap<String, TokenEntry> {
        self.0.state.read().tokens.clone()
    }

    /// How many tokens the table holds.
    pub fn len(&self) -> usize {
        self.0.state.read().tokens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether a table has been provisioned, which secure-mode streaming waits on.
    pub fn is_gate_open(&self) -> bool {
        *self.0.gate.borrow()
    }

    /// Resolve once the first table has opened the gate.
    pub async fn wait_gate(&self) {
        let mut rx = self.0.gate.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Register what a transport does with its live clients on every table.
    pub fn add_reconcile_hook(&self, hook: Arc<dyn ReconcileHook>) {
        self.0.hooks.lock().push(hook);
    }

    /// Whether a client holding `token` with `role` may drive keyboard and mouse: while an
    /// mk token is active only its holder, otherwise any controller.
    pub fn holds_input_authority(&self, role: Role, token: Option<&str>) -> bool {
        match self.active_mk_token() {
            Some(mk) => token == Some(mk.as_str()),
            None => role == Role::Controller,
        }
    }

    /// The `MK_ACCESS` verdict a tokened client is told: input authority under the mk-token
    /// rule, with a viewer additionally held to `enable_collab`, since a read-only viewer
    /// must not attach an input context whose every message the gate then drops.
    pub fn mk_access_verdict(&self, role: Role, token: Option<&str>, enable_collab: bool) -> bool {
        if role != Role::Controller && !enable_collab {
            return false;
        }
        self.holds_input_authority(role, token)
    }

    /// Replace the table with `table` (an object `token_table_error` accepts), as
    /// `/api/tokens` and the tokens file do: the first table opens the gate, every one runs
    /// the reconcile hooks. Only the JSON `true` elects an entry's `mk_control`, as only
    /// `"controller"` names a controller: a string such as `"false"` is no grant.
    pub fn provision(&self, table: &Map<String, Json>) {
        let mut tokens = HashMap::with_capacity(table.len());
        let mut mk_owner = None;
        let mut roleless = 0;
        for (token, perms) in table {
            let Json::Object(perms) = perms else { continue };
            let entry = TokenEntry::from_json(perms);
            if entry.mk_control && mk_owner.is_none() {
                mk_owner = Some(token.clone());
            }
            if !entry.named_role {
                roleless += 1;
            }
            tokens.insert(token.clone(), entry);
        }
        if roleless > 0 {
            log::warn!(target: "ws", "{roleless} token entries name no role 'controller' or 'viewer'; they connect as viewers.");
        }
        let count = tokens.len();
        {
            let mut state = self.0.state.write();
            state.tokens = tokens;
            state.active_mk_token = mk_owner;
        }
        log::info!(target: "ws", "Updated user tokens. Now tracking {count} tokens.");
        if !self.is_gate_open() {
            self.0.gate.send_replace(true);
            log::info!(target: "ws", "Configuration gate is now open. WebSocket server will accept connections.");
        }
        let hooks: Vec<Arc<dyn ReconcileHook>> = self.0.hooks.lock().clone();
        if !hooks.is_empty() {
            tokio::spawn(async move {
                for hook in hooks {
                    hook.reconcile().await;
                }
            });
        }
    }

    /// Forget every token (a revocation, for a test).
    #[cfg(test)]
    pub(crate) fn clear(&self) {
        let mut state = self.0.state.write();
        state.tokens.clear();
        state.active_mk_token = None;
    }
}

/// `POST /api/tokens`: a full replacement of the session's token table.
///
/// Provisioning is transport-independent, so tokens are accepted in any active mode. Without
/// a master token the endpoint says the server is not in secure mode. The detail of a
/// refused table stays in the log, since the endpoint is reachable without authentication
/// when no master token is set.
pub async fn handle_tokens(State(state): State<AppState>, body: Bytes) -> Response {
    if !state.config.secure_mode() {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": "Server not in secure mode"})),
        )
            .into_response();
    }
    let parsed: Result<Json, _> = serde_json::from_slice(&body);
    let error = match &parsed {
        Ok(table) => token_table_error(table).map(str::to_string),
        Err(e) => Some(e.to_string()),
    };
    if let Some(error) = error {
        log::warn!(target: "ws", "Rejected a token table update: {error}");
        return (StatusCode::BAD_REQUEST, "Bad Request").into_response();
    }
    if let Ok(Json::Object(table)) = parsed {
        state.tokens.provision(&table);
    }
    (StatusCode::OK, "OK").into_response()
}

/// The token table `path` holds, or none when it is unsafe or holds none, with the reason
/// logged. A symlink is followed, as a Kubernetes secret mount is one, and the file it
/// reaches is the one checked.
pub fn read_tokens_file(path: &str) -> Option<Map<String, Json>> {
    let read = || -> std::io::Result<Option<Map<String, Json>>> {
        let file = std::fs::File::open(path)?;
        let info = file.metadata()?;
        let uid = nix::unistd::getuid().as_raw();
        if !(info.uid() == 0 || info.uid() == uid) || info.mode() & 0o022 != 0 {
            log::warn!(target: "ws", "Refusing the tokens file {path:?}: another user owns it or can write to it.");
            return Ok(None);
        }
        let text = std::io::read_to_string(file)?;
        let table: Json = serde_json::from_str(&text).map_err(std::io::Error::other)?;
        if let Some(error) = token_table_error(&table) {
            log::warn!(target: "ws", "Rejected the tokens file {path:?}: {error}.");
            return Ok(None);
        }
        match table {
            Json::Object(map) => Ok(Some(map)),
            _ => Ok(None),
        }
    };
    match read() {
        Ok(table) => table,
        Err(exc) => {
            log::warn!(target: "ws", "Could not read the tokens file {path:?}: {exc}");
            None
        }
    }
}

/// Provision the table from `path` at start and on every change, polling every `poll`.
///
/// The file holds the body `/api/tokens` takes and is provisioned the same way, so a restart
/// needs no POST. A POST still replaces the table until the file next changes, and a file
/// that cannot be used leaves the table as it was. Without a master token the file is ignored
/// and the watcher returns at once.
pub async fn watch_tokens_file(table: TokenTable, path: String, secure_mode: bool, poll: Duration) {
    if !secure_mode {
        log::warn!(target: "ws", "tokens_file is ignored: it needs secure mode, which a master token turns on.");
        return;
    }
    let mut seen: Option<Option<(u64, i64, i64, u64)>> = None;
    loop {
        let stat_path = path.clone();
        let stat = tokio::task::spawn_blocking(move || std::fs::metadata(&stat_path)).await;
        match stat {
            Ok(Ok(info)) => {
                let current = Some((info.ino(), info.ctime(), info.ctime_nsec(), info.size()));
                if seen != Some(current) {
                    seen = Some(current);
                    let read_path = path.clone();
                    if let Ok(Some(found)) =
                        tokio::task::spawn_blocking(move || read_tokens_file(&read_path)).await
                    {
                        table.provision(&found);
                    }
                }
            }
            Ok(Err(exc)) => {
                if seen != Some(None) {
                    log::warn!(target: "ws", "The tokens file {path:?} cannot be read yet: {exc}");
                    seen = Some(None);
                }
            }
            Err(_) => return,
        }
        tokio::time::sleep(poll).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::testing::tempfile_dir;
    use std::os::unix::fs::PermissionsExt;

    fn table(entries: Json) -> Map<String, Json> {
        entries.as_object().unwrap().clone()
    }

    #[tokio::test]
    async fn mk_control_elects_true_alone() {
        let t = TokenTable::new();
        for value in [
            json!(true),
            json!("true"),
            json!("false"),
            json!(1),
            json!("1"),
            json!("yes"),
            Json::Null,
            json!({}),
        ] {
            t.provision(&table(
                json!({"view": {"role": "viewer", "slot": null, "mk_control": value}}),
            ));
            assert_eq!(
                t.active_mk_token() == Some("view".into()),
                value == Json::Bool(true),
                "mk_control {value}"
            );
        }
        t.provision(&table(json!({"ctrl": {"role": "controller", "slot": null},
                                  "view": {"role": "viewer", "slot": null, "mk_control": true}})));
        assert_eq!(
            t.active_mk_token().as_deref(),
            Some("view"),
            "the first true entry holds the input authority"
        );
        assert!(t.is_gate_open(), "the first table opens the gate");
    }

    #[test]
    fn slots_normalize() {
        assert_eq!(token_slots(&json!(1)), vec![1]);
        assert_eq!(token_slots(&json!("2")), vec![2]);
        assert_eq!(token_slots(&json!([3, 3, 1, 9, "x", true, 0])), vec![3, 1]);
        assert_eq!(token_slots(&Json::Null), Vec::<u8>::new());
        assert_eq!(stored_slot(&json!([2])), json!(2));
        assert_eq!(stored_slot(&json!([2, 4])), json!([2, 4]));
        assert_eq!(stored_slot(&json!(0)), Json::Null);
    }

    #[test]
    fn table_validation() {
        assert_eq!(
            token_table_error(&json!([])),
            Some("the table must be a JSON object")
        );
        assert_eq!(
            token_table_error(&json!({"a": "controller"})),
            Some("every token entry must be a JSON object")
        );
        assert_eq!(token_table_error(&json!({"a": {"role": "viewer"}})), None);
    }

    #[tokio::test]
    async fn lookup_and_verdicts() {
        let t = TokenTable::new();
        t.provision(&table(json!({"ctrl": {"role": "controller", "slot": 1}, "view": {"role": "viewer", "slot": null}})));
        assert_eq!(
            t.lookup("view").map(|e| e.role),
            Some(Role::Viewer),
            "lookup finds the provisioned entry"
        );
        assert!(
            t.lookup("vi").is_none()
                && t.lookup("no-such-token").is_none()
                && t.lookup("").is_none(),
            "lookup rejects prefixes and empties"
        );
        assert!(
            t.mk_access_verdict(Role::Controller, Some("ctrl"), true),
            "no mk token: a controller holds input"
        );
        assert!(
            !t.mk_access_verdict(Role::Viewer, Some("view"), true),
            "no mk token: a viewer does not"
        );
        t.provision(&table(json!({"ctrl": {"role": "controller", "slot": 1},
                                  "view": {"role": "viewer", "slot": null, "mk_control": true}})));
        assert!(
            t.mk_access_verdict(Role::Viewer, Some("view"), true),
            "viewer holding the mk token is granted (collab on)"
        );
        assert!(
            !t.mk_access_verdict(Role::Controller, Some("ctrl"), true),
            "controller outranked by the mk token is refused"
        );
        assert!(
            !t.mk_access_verdict(Role::Viewer, Some("view"), false),
            "collab off: a viewer holding the mk token stays read-only"
        );
        assert!(!t.mk_access_verdict(Role::Controller, Some("ctrl"), false));
        assert_eq!(t.lookup("ctrl").unwrap().slot_json(), json!(1));
    }

    fn write(path: &std::path::Path, body: &str, mode: u32) {
        let staged = path.with_extension("new");
        std::fs::write(&staged, body).unwrap();
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(mode)).unwrap();
        std::fs::rename(&staged, path).unwrap();
    }

    async fn settle(t: &TokenTable, expect: &[&str]) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let mut have: Vec<String> = t.snapshot().into_keys().collect();
            have.sort();
            if have == expect {
                return true;
            }
            if tokio::time::Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn keys(t: &TokenTable) -> Vec<String> {
        let mut have: Vec<String> = t.snapshot().into_keys().collect();
        have.sort();
        have
    }

    #[tokio::test]
    async fn tokens_file_provisions_without_a_post() {
        let root = tempfile_dir("selkies-tokens-file-");
        let path = root.join("tokens.json");
        write(
            &path,
            r#"{"ctrl": {"role": "controller", "slot": null}}"#,
            0o600,
        );
        let t = TokenTable::new();
        let watcher = tokio::spawn(watch_tokens_file(
            t.clone(),
            path.to_string_lossy().into_owned(),
            true,
            Duration::from_millis(50),
        ));
        assert!(
            settle(&t, &["ctrl"]).await,
            "the file's table is provisioned without a POST"
        );
        assert!(t.is_gate_open(), "the first table opens the config gate");

        write(
            &path,
            r#"{"view": {"role": "viewer", "slot": null}}"#,
            0o600,
        );
        assert!(
            settle(&t, &["view"]).await,
            "a file renamed over it replaces the table"
        );

        write(
            &path,
            r#"{"other": {"role": "controller", "slot": null}}"#,
            0o620,
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            keys(&t),
            ["view"],
            "a file another user can write to is refused"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            settle(&t, &["other"]).await,
            "a chmod that makes it safe provisions it"
        );

        write(&path, "{not json", 0o600);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            keys(&t),
            ["other"],
            "a file that is not JSON leaves the table as it was"
        );
        write(&path, r#"{"bad": "controller"}"#, 0o600);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            keys(&t),
            ["other"],
            "a file whose entries are not objects leaves it too"
        );

        let target = root.join("mounted.json");
        write(
            &target,
            r#"{"linked": {"role": "controller", "slot": null}}"#,
            0o600,
        );
        let link = root.join("tokens.json.link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        std::fs::rename(&link, &path).unwrap();
        assert!(
            settle(&t, &["linked"]).await,
            "a symlink is followed to the file it names"
        );
        watcher.abort();

        let t = TokenTable::new();
        tokio::time::timeout(
            Duration::from_secs(1),
            watch_tokens_file(
                t.clone(),
                path.to_string_lossy().into_owned(),
                false,
                Duration::from_millis(50),
            ),
        )
        .await
        .expect("without a master token the watcher returns");
        assert!(
            t.is_empty() && !t.is_gate_open(),
            "without a master token the file is ignored"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
