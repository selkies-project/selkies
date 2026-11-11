/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Who owns a display, and what a controller beside its owner may do.
//!
//! The browser tab of each peer (its signaling hello) names the page, and per display the tab whose
//! controller owns it is recorded: a controller of another tab streams and drives input beside it,
//! and what would size or configure the display waits until it owns it. A display passes to a page
//! beside its owner when the owner hides for `hidden_owner_handoff_s` (`settle_display_owner`), when
//! the owner goes `idle_owner_handoff_s` without its user's input while a page of the same credential
//! is in use (`note_page_use`), or when the owner's tab has been gone through the reconnect grace
//! (`succeed_display_owner`). Nobody is disconnected by any of it.

use std::collections::HashSet;
use std::time::Duration;

use log::info;
use selkies_rtc::rtp::clock;
use selkies_rtc::signaling::ClientType;
use serde_json::{Map, Value as Json, json};

use crate::input::authority::page_put_to_use;
use crate::settings::STREAM_SETTINGS;
use crate::webrtc::engine::PeerEntry;
use crate::webrtc::service::WebRtcService;
use crate::ws::state::display_settings;

/// What sizes or configures the display a peer streams: its owner's alone apply.
const OWNER_ONLY_PREFIXES: [&str; 3] = ["SETTINGS,", "r,", "s,"];
/// The live verbs of the stream settings. A page beside the owner sends its user's picks in a
/// `SETTINGS` too, so these from it are its own changes.
const STREAM_VERB_PREFIXES: [&str; 5] = ["_arg_fps,", "vb,", "ab,", "_crf,", "_rc,"];

/// `SETTINGS,<json>` as its object.
pub(crate) fn parse_settings(msg: &str) -> Option<Map<String, Json>> {
    let body = msg.split_once(',')?.1;
    match serde_json::from_str::<Json>(body).ok()? {
        Json::Object(map) => Some(map),
        _ => None,
    }
}

impl WebRtcService {
    /// The tabs of the peers streaming a display, or connecting to stream it, leaving out peer
    /// `but`.
    ///
    /// A peer's tab is recorded when its session starts, and its connection joins the engine
    /// only once ICE has gathered, which waits out an unanswered TURN server (the ICE agent's
    /// gather timeout, longer than the reconnect grace). A page between the two, such as an
    /// owner's reload, counts on the display its signaling registration names, so the grace
    /// sees it back.
    pub(crate) fn display_tabs(&self, display: &str, but: Option<&str>) -> HashSet<String> {
        let tabs = self.st.lock().peer_tabs.clone();
        let mut found = HashSet::new();
        let mut connecting = Vec::new();
        {
            let peers = self.engine.peers.lock();
            for (pid, tab) in tabs {
                let Some(tab) = tab.filter(|_| Some(pid.as_str()) != but) else {
                    continue;
                };
                match peers.get(&pid) {
                    Some(peer) if peer.display_id == display => {
                        found.insert(tab);
                    }
                    Some(_) => {}
                    None => connecting.push((pid, tab)),
                }
            }
        }
        if let Some(registry) = self.registry.read().clone() {
            for (pid, tab) in connecting {
                if registry.peer(&pid).is_some_and(|p| p.display_id == display) {
                    found.insert(tab);
                }
            }
        }
        found
    }

    /// Whether a peer holds a display's full permissions: a controller that may drive keyboard and
    /// mouse, not a viewer, nor a gamepad player.
    pub(crate) fn holds_full_control(&self, peer: &str) -> bool {
        let details = self
            .engine
            .peers
            .lock()
            .get(peer)
            .map(|p| (p.client_type, p.client_token.clone()));
        details.is_some_and(|(client_type, token)| {
            client_type == ClientType::Controller
                && self
                    .engine
                    .peer_holds_input_authority(client_type, token.as_deref())
        })
    }

    /// The peer of a display's owner.
    pub(crate) fn owner_peer(&self, display: &str) -> Option<String> {
        let (owner, tabs) = {
            let st = self.st.lock();
            (st.owner_tabs.get(display).cloned()?, st.peer_tabs.clone())
        };
        self.engine
            .peers
            .lock()
            .iter()
            .find(|(pid, p)| {
                p.display_id == display
                    && tabs.get(*pid).and_then(Clone::clone).as_ref() == Some(&owner)
            })
            .map(|(pid, _)| pid.clone())
    }

    /// The controller peers streaming a display from another tab than its owner's.
    pub(crate) fn peers_beside_owner(&self, display: &str) -> Vec<String> {
        let (owner, tabs) = {
            let st = self.st.lock();
            (st.owner_tabs.get(display).cloned(), st.peer_tabs.clone())
        };
        let Some(owner) = owner else {
            return Vec::new();
        };
        self.engine
            .peers
            .lock()
            .iter()
            .filter(|(pid, p)| {
                p.display_id == display
                    && p.client_type == ClientType::Controller
                    && tabs
                        .get(*pid)
                        .and_then(Clone::clone)
                        .is_some_and(|tab| tab != owner)
            })
            .map(|(pid, _)| pid.clone())
            .collect()
    }

    /// Whether a peer is the owner of the display it streams, which any peer is while no owner's
    /// tab is known.
    pub(crate) fn peer_owns(&self, peer: &str) -> bool {
        let display = self
            .engine
            .peers
            .lock()
            .get(peer)
            .map(|p| p.display_id.clone());
        let st = self.st.lock();
        let owner = st.owner_tabs.get(display.as_deref().unwrap_or("primary"));
        owner.is_none() || st.peer_tabs.get(peer).and_then(Option::as_ref) == owner
    }

    /// Tell peers what a display streams with: a page beside the owner holds it for its tab, as
    /// does the owner once another page's pick changed it.
    pub(crate) fn tell_display_settings(&self, display: &str, peers: &[String]) {
        let values = {
            let st = self.st.lock();
            display_settings(st.displays.get(display), &st.seeds)
        };
        let settings: Map<String, Json> =
            values.into_iter().filter(|(_, v)| !v.is_null()).collect();
        let data = json!({"displayId": display, "settings": settings});
        for peer in peers {
            let channel = self
                .engine
                .peers
                .lock()
                .get(peer)
                .and_then(|p| p.channel.clone());
            if let Some(channel) = channel {
                channel.send_typed("display_settings", &data);
            }
        }
    }

    /// Hand a peer's message to the input stack. From a controller beside the display's owner, what
    /// would size the display is held back, the latest of each kind replayed once that controller
    /// owns the display; its `SETTINGS` changes the stream by its user's picks alone, and its stream
    /// verbs, its page's own changes, are dropped. The owner's changes to the stream reach the pages
    /// beside it. Its user's input may hand it the display (`note_page_use`). A key-frame request
    /// from a peer that does not own the display is taken once a second.
    pub(crate) async fn on_peer_data_message(&self, msg: String, display: &str, peer: &str) {
        if msg == "REQUEST_KEYFRAME" && !self.engine.peer_recovery_taken(peer, "keyframe") {
            return;
        }
        if msg == "PAGE_HIDDEN" || msg == "PAGE_SHOWN" {
            self.take_page_visibility(msg == "PAGE_SHOWN", display, peer)
                .await;
            return;
        }
        if page_put_to_use(&msg) && self.note_page_use(display, peer) {
            self.input_message(&msg, display, peer).await;
            self.lay_out_for_owner(peer, display).await;
            return;
        }
        let owner_only = OWNER_ONLY_PREFIXES.iter().any(|p| msg.starts_with(p));
        let stream_verb = STREAM_VERB_PREFIXES.iter().any(|p| msg.starts_with(p));
        if !owner_only && !stream_verb {
            self.input_message(&msg, display, peer).await;
            return;
        }
        let (tab, owner) = {
            let st = self.st.lock();
            (
                st.peer_tabs.get(peer).and_then(Clone::clone),
                st.owner_tabs.get(display).cloned(),
            )
        };
        if let (Some(tab), Some(owner)) = (&tab, &owner)
            && tab != owner
        {
            if msg.starts_with("SETTINGS,") {
                self.settings_beside_owner(&msg, display, peer).await;
            } else if owner_only {
                self.hold_owner_message(peer, &msg);
            }
            return;
        }
        if tab.is_some() && owner_only {
            // Kept for when this page owns the display again after a page beside it did.
            if msg.starts_with("SETTINGS,") {
                if let Some(json) = parse_settings(&msg) {
                    self.hold_settings(peer, &json);
                }
            } else {
                self.hold_owner_message(peer, &msg);
            }
        }
        self.input_message(&msg, display, peer).await;
        if msg.starts_with("SETTINGS,") || stream_verb {
            let beside = self.peers_beside_owner(display);
            if !beside.is_empty() {
                self.tell_display_settings(display, &beside);
            }
        }
    }

    fn conn_of(&self, peer: &str) -> u64 {
        self.engine.peers.lock().get(peer).map_or(0, |p| p.conn)
    }

    async fn input_message(&self, msg: &str, display: &str, peer: &str) {
        if let Some(input) = self.input() {
            input.on_message(msg, display, self.conn_of(peer)).await;
        }
    }

    fn hold_owner_message(&self, peer: &str, msg: &str) {
        let head = msg.split(',').next().unwrap_or("").to_string();
        if let Some(entry) = self.engine.peers.lock().get_mut(peer) {
            entry.held_owner_messages.insert(head, msg.to_string());
        }
    }

    /// Keep what of a page's `SETTINGS` sizes and configures the display for when that page owns
    /// it: all but the stream settings, which stay as they are then, merged over what it held.
    fn hold_settings(&self, peer: &str, settings_json: &Map<String, Json>) {
        let mut rest: Map<String, Json> = settings_json
            .iter()
            .filter(|(k, _)| !STREAM_SETTINGS.contains(&k.as_str()) && k.as_str() != "picked")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if rest.is_empty() {
            return;
        }
        if let Some(entry) = self.engine.peers.lock().get_mut(peer) {
            if let Some(mut merged) = entry
                .held_owner_messages
                .get("SETTINGS")
                .and_then(|held| parse_settings(held))
            {
                merged.append(&mut rest);
                rest = merged;
            }
            entry.held_owner_messages.insert(
                "SETTINGS".to_string(),
                format!("SETTINGS,{}", Json::Object(rest)),
            );
        }
    }

    /// A `SETTINGS` from a controller beside the display's owner. What its user picked of the stream
    /// applies where both pages hold full permissions, and the display's pages are told; the rest
    /// waits until that controller owns the display, without the stream settings, and the page is
    /// told what they are now.
    async fn settings_beside_owner(&self, msg: &str, display: &str, peer: &str) {
        let Some(settings_json) = parse_settings(msg) else {
            return;
        };
        let picks: Map<String, Json> = settings_json
            .get("picked")
            .and_then(Json::as_array)
            .map(|picked| {
                picked
                    .iter()
                    .filter_map(Json::as_str)
                    .filter(|k| STREAM_SETTINGS.contains(k))
                    .filter_map(|k| {
                        settings_json
                            .get(k)
                            .filter(|v| !v.is_null())
                            .map(|v| (k.to_string(), v.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.hold_settings(peer, &settings_json);
        let owner = self.owner_peer(display);
        let allowed = !picks.is_empty()
            && self.holds_full_control(peer)
            && owner.as_deref().is_some_and(|o| self.holds_full_control(o));
        if !allowed {
            self.tell_display_settings(display, &[peer.to_string()]);
            return;
        }
        let mut keys: Vec<&str> = picks.keys().map(String::as_str).collect();
        keys.sort_unstable();
        info!(target: "webrtc", "Applying {} picked by the controller beside the owner of '{display}'.", keys.join(", "));
        self.handle_update_settings(&Json::Object(picks.clone()), display)
            .await;
        let mut peers: Vec<String> = owner.into_iter().collect();
        peers.extend(self.peers_beside_owner(display));
        self.tell_display_settings(display, &peers);
    }

    /// Apply what a controller that owns its display now held back: its settings, density, and
    /// size, the latest of each, which it keeps for the next time it owns the display.
    async fn replay_held(&self, peer: &str, display: &str) {
        let held = self
            .engine
            .peers
            .lock()
            .get(peer)
            .map(|p| p.held_owner_messages.clone())
            .unwrap_or_default();
        for kind in ["SETTINGS", "s", "r"] {
            if let Some(msg) = held.get(kind) {
                self.input_message(msg, display, peer).await;
            }
        }
    }

    async fn take_page_visibility(&self, shown: bool, display: &str, peer: &str) {
        let resume = {
            let mut peers = self.engine.peers.lock();
            let Some(entry) = peers.get_mut(peer) else {
                return;
            };
            let now = clock::monotonic();
            if shown {
                entry.page_hidden_at = Some(None);
                entry.page_shown_at = now;
                std::mem::take(&mut entry.paused_on_hand_off)
            } else {
                if !matches!(entry.page_hidden_at, Some(Some(_))) {
                    entry.page_hidden_at = Some(Some(now));
                }
                false
            }
        };
        if resume {
            self.handle_video_consumer_active(peer, display, true).await;
        }
        self.settle_display_owner(display).await;
    }

    /// Hand a display to the controller beside its owner that showed or was used last, once the
    /// owner's page has been hidden for `hidden_owner_handoff_s`: what that controller held back
    /// applies, its size among it, and the owner stays beside it. Only pages that report their
    /// visibility and hold full permissions take it.
    fn settle_display_owner<'a>(&'a self, display: &'a str) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            let (owner, tabs, used) = {
                let st = self.st.lock();
                (
                    st.owner_tabs.get(display).cloned(),
                    st.peer_tabs.clone(),
                    st.tab_used_at.clone(),
                )
            };
            let Some(owner) = owner else { return };
            let (hidden_times, heirs): (Vec<Option<f64>>, Vec<(String, f64)>) = {
                let peers = self.engine.peers.lock();
                let mine: Vec<Option<f64>> = peers
                    .iter()
                    .filter(|(pid, p)| {
                        p.display_id == display
                            && tabs.get(*pid).and_then(Clone::clone).as_ref() == Some(&owner)
                    })
                    .map(|(_, p)| p.page_hidden_at.flatten())
                    .collect();
                let heirs = peers
                    .iter()
                    .filter(|(pid, p)| {
                        p.display_id == display
                            && p.client_type == ClientType::Controller
                            && tabs
                                .get(*pid)
                                .and_then(Clone::clone)
                                .is_some_and(|tab| tab != owner)
                            && p.page_hidden_at == Some(None)
                    })
                    .map(|(pid, p)| {
                        let tab = tabs.get(pid).and_then(Clone::clone).unwrap_or_default();
                        (
                            pid.clone(),
                            p.page_shown_at.max(used.get(&tab).copied().unwrap_or(0.0)),
                        )
                    })
                    .collect();
                (mine, heirs)
            };
            if hidden_times.is_empty() || hidden_times.iter().any(Option::is_none) {
                return;
            }
            let heirs: Vec<(String, f64)> = heirs
                .into_iter()
                .filter(|(pid, _)| self.holds_full_control(pid))
                .collect();
            if heirs.is_empty() {
                return;
            }
            let latest = hidden_times
                .iter()
                .flatten()
                .copied()
                .fold(f64::MIN, f64::max);
            let wait = latest + self.timings.hidden_owner_handoff_s - clock::monotonic();
            if wait > 0.0 {
                let mut st = self.st.lock();
                if st.owner_settling.insert(display.to_string()) {
                    let service = self.arc();
                    let did = display.to_string();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                        service.st.lock().owner_settling.remove(&did);
                        service.settle_display_owner(&did).await;
                    });
                }
                return;
            }
            let Some((heir, _)) = heirs
                .into_iter()
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            else {
                return;
            };
            self.seat_owner(
                &heir,
                display,
                &format!("hid its page: controller {heir} beside it"),
            );
            self.lay_out_for_owner(&heir, display).await;
        })
    }

    /// Note that a controller page's user acted through it, by its tab, and hand it the display
    /// where the owner has gone `idle_owner_handoff_s` without its own user's: one person moving
    /// between devices takes the display along. Only between pages of the same credential, and only
    /// between pages that report their visibility, the one acting shown and holding full
    /// permissions. True when the display is its now, what it held back yet to apply.
    fn note_page_use(&self, display: &str, peer: &str) -> bool {
        let now = clock::monotonic();
        let (tab, owner, owner_used, tabs) = {
            let mut st = self.st.lock();
            let Some(tab) = st.peer_tabs.get(peer).and_then(Clone::clone) else {
                return false;
            };
            st.tab_used_at.insert(tab.clone(), now);
            let owner = st.owner_tabs.get(display).cloned();
            let used = owner
                .as_ref()
                .and_then(|o| st.tab_used_at.get(o).copied())
                .unwrap_or(0.0);
            (tab, owner, used, st.peer_tabs.clone())
        };
        let Some(owner) = owner else { return false };
        if owner == tab || now - owner_used < self.timings.idle_owner_handoff_s {
            return false;
        }
        let allowed = {
            let peers = self.engine.peers.lock();
            let Some(me) = peers.get(peer) else {
                return false;
            };
            let mine: Vec<&PeerEntry> = peers
                .iter()
                .filter(|(pid, p)| {
                    p.display_id == display
                        && tabs.get(*pid).and_then(Clone::clone).as_ref() == Some(&owner)
                })
                .map(|(_, p)| p)
                .collect();
            me.display_id == display
                && me.client_type == ClientType::Controller
                && me.page_hidden_at == Some(None)
                && !mine.is_empty()
                && mine
                    .iter()
                    .all(|p| p.page_hidden_at.is_some() && p.client_token == me.client_token)
        };
        if !allowed || !self.holds_full_control(peer) {
            return false;
        }
        self.seat_owner(
            peer,
            display,
            &format!("went unused: controller {peer} beside it in use"),
        );
        true
    }

    /// Make the tab of `heir`, a controller beside a display's owner, the display's owner, and drop
    /// what the display's congestion control learned about the old page's path, for a page taking it
    /// over. The steered rate stays as the starting point. The old owner's pages that reported
    /// themselves hidden pause, as over WebSockets: a page frozen on hide (a phone) never sends its
    /// `STOP_VIDEO`, and would keep receiving frames.
    fn seat_owner(&self, heir: &str, display: &str, why: &str) {
        let (old, old_pages) = {
            let mut st = self.st.lock();
            let Some(tab) = st.peer_tabs.get(heir).and_then(Clone::clone) else {
                return;
            };
            let old = st.owner_tabs.insert(display.to_string(), tab.clone());
            st.tab_used_at.insert(tab.clone(), clock::monotonic());
            let old_pages: Vec<String> = st
                .peer_tabs
                .iter()
                .filter(|(_, t)| old.is_some() && **t == old)
                .map(|(peer, _)| peer.clone())
                .collect();
            let shown = old.clone().unwrap_or_else(|| "none".into());
            info!(target: "webrtc", "The owner of '{display}' {why} owns the display now (tab {}, the old owner's {}).", &tab[..tab.len().min(8)], &shown[..shown.len().min(8)]);
            (old, old_pages)
        };
        self.forget_path(display);
        if old.is_none() {
            return;
        }
        let mut peers = self.engine.peers.lock();
        for page in old_pages {
            if let Some(entry) = peers.get_mut(&page)
                && entry.display_id == display
                && matches!(entry.page_hidden_at, Some(Some(_)))
                && !entry.video_paused
            {
                entry.video_paused = true;
                entry.paused_on_hand_off = true;
                entry.peer.set_video_enabled(false);
            }
        }
    }

    async fn lay_out_for_owner(&self, heir: &str, display: &str) {
        self.replay_held(heir, display).await;
        let beside = self.peers_beside_owner(display);
        self.tell_display_settings(display, &beside);
    }

    /// Once the owner's tab has been gone through the reconnect grace, hand the display to its
    /// oldest controller that holds full permissions, else to its oldest controller.
    pub(crate) async fn succeed_display_owner(&self, display: String, tab: String) {
        tokio::time::sleep(Duration::from_secs_f64(self.timings.reconnect_grace_s)).await;
        let still_owner = self.st.lock().owner_tabs.get(&display) == Some(&tab);
        if !still_owner || self.display_tabs(&display, None).contains(&tab) {
            return;
        }
        let tabs = self.st.lock().peer_tabs.clone();
        let controllers: Vec<String> = self
            .engine
            .peers
            .lock()
            .iter()
            .filter(|(pid, p)| {
                p.display_id == display
                    && p.client_type == ClientType::Controller
                    && tabs.get(*pid).and_then(Clone::clone).is_some()
            })
            .map(|(pid, _)| pid.clone())
            .collect();
        let heir = controllers
            .iter()
            .find(|pid| self.holds_full_control(pid))
            .or(controllers.first())
            .cloned();
        let Some(heir) = heir else {
            self.st.lock().owner_tabs.remove(&display);
            return;
        };
        {
            let mut st = self.st.lock();
            if let Some(tab) = st.peer_tabs.get(&heir).and_then(Clone::clone) {
                st.owner_tabs.insert(display.clone(), tab.clone());
                st.tab_used_at.insert(tab, clock::monotonic());
            }
        }
        info!(target: "webrtc", "Controller {heir} owns display '{display}' now.");
        self.replay_held(&heir, &display).await;
    }
}
