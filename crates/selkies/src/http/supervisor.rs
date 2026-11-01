/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The supervisor over the streaming services: exactly one registered service is active at
//! a time, and [`ModeSupervisor::switch_to_mode`] serializes transitions under a lock so
//! capture is never owned twice.
//!
//! A switch stops the active service, waiting up to [`STOP_GRACE`] for its task before
//! canceling it (a teardown includes gamepad-close waits of a couple of seconds), then brings
//! the settings in line with the transport before the next one reads them: the encoder knob
//! (a websockets-only encoder such as JPEG cannot ride the WebRTC pipeline, and a switch back
//! restores the operator's menu and value), and only then the paint-over and threading
//! defaults, which depend on the resolved encoder. A service task that dies logs the error
//! and clears the active mode, so the status route tells the truth.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value as Json;
use tokio::task::JoinHandle;

use super::Supervisor;
use crate::settings::settings_mut;

/// How long a stopping service's task may take before it is canceled.
pub const STOP_GRACE: Duration = Duration::from_secs(15);

/// One transport as the supervisor drives it.
#[async_trait]
pub trait StreamingService: Send + Sync {
    /// Serve until `stop` is called; the future is the service's lifetime, and an error is
    /// the service dying.
    async fn start(&self) -> Result<(), String>;
    /// Ask the running service to end; `start` returns once it has.
    async fn stop(&self);
    /// The connected pages (see [`Supervisor::sessions`]).
    async fn sessions(&self) -> Vec<Json>;
    /// Close one page's connection; false for an unknown id.
    async fn disconnect_session(&self, id: &str) -> bool;
}

struct Active {
    mode: Option<String>,
    task: Option<JoinHandle<Result<(), String>>>,
    /// Bumped per start, so a dying task clears only the mode it was started for.
    generation: u64,
}

/// The server's supervisor: the registered services, the active one, and the switch lock.
pub struct ModeSupervisor {
    services: Vec<(String, Arc<dyn StreamingService>)>,
    lock: tokio::sync::Mutex<()>,
    active: Arc<Mutex<Active>>,
}

impl ModeSupervisor {
    /// A supervisor over `services`, in the order given, none active yet.
    pub fn new(services: Vec<(String, Arc<dyn StreamingService>)>) -> ModeSupervisor {
        ModeSupervisor {
            services,
            lock: tokio::sync::Mutex::new(()),
            active: Arc::new(Mutex::new(Active {
                mode: None,
                task: None,
                generation: 0,
            })),
        }
    }

    fn service(&self, mode: &str) -> Option<Arc<dyn StreamingService>> {
        self.services
            .iter()
            .find(|(name, _)| name == mode)
            .map(|(_, s)| s.clone())
    }

    fn active_service(&self) -> Option<Arc<dyn StreamingService>> {
        let mode = self.active.lock().mode.clone()?;
        self.service(&mode)
    }

    /// Stop the active service, escalating to a cancel after [`STOP_GRACE`].
    async fn stop_service(&self) {
        let (mode, task) = {
            let mut active = self.active.lock();
            let Some(mode) = active.mode.clone() else {
                return;
            };
            (mode, active.task.take())
        };
        log::info!(target: "server", "Stopping service: {mode}");
        if let Some(service) = self.service(&mode) {
            service.stop().await;
        }
        if let Some(task) = task {
            let abort = task.abort_handle();
            match tokio::time::timeout(STOP_GRACE, task).await {
                Ok(_) => {}
                Err(_) => {
                    log::warn!(target: "server", "Timeout while stopping '{mode}'. Canceling task.");
                    abort.abort();
                    log::info!(target: "server", "Task canceled after timeout for '{mode}'.");
                }
            }
        }
        let mut active = self.active.lock();
        active.mode = None;
        active.task = None;
    }

    /// Stop whatever runs, for shutdown.
    pub async fn stop(&self) {
        let _guard = self.lock.lock().await;
        self.stop_service().await;
    }
}

/// Bring the settings in line with the transport about to start.
fn apply_mode_to_settings(mode: &str) {
    let mut s = settings_mut();
    s.set_str("mode", mode.to_string());
    s.apply_webrtc_encoder_filter();
    s.resolve_paint_over_default();
    s.resolve_cpu_encode_threading_default();
}

#[async_trait]
impl Supervisor for ModeSupervisor {
    fn current_mode(&self) -> Option<String> {
        self.active.lock().mode.clone()
    }

    fn available_modes(&self) -> Vec<String> {
        self.services.iter().map(|(name, _)| name.clone()).collect()
    }

    async fn switch_to_mode(&self, mode: &str) -> Result<(), String> {
        let Some(service) = self.service(mode) else {
            return Err(format!("Service {mode} not found"));
        };
        let _guard = self.lock.lock().await;
        if self.active.lock().mode.as_deref() == Some(mode) {
            log::info!(target: "server", "Mode {mode} is already active.");
            return Ok(());
        }
        self.stop_service().await;
        log::info!(target: "server", "Starting service: {mode}");
        apply_mode_to_settings(mode);
        let shared = self.active.clone();
        let name = mode.to_string();
        // The mode is published before the task can run, so one that dies at once clears it.
        let mut active = self.active.lock();
        active.generation += 1;
        let generation = active.generation;
        active.mode = Some(mode.to_string());
        active.task = Some(tokio::spawn(async move {
            let outcome = service.start().await;
            if let Err(exc) = &outcome {
                log::error!(target: "server", "Service '{name}' terminated unexpectedly: {exc:?}");
                let mut active = shared.lock();
                if active.generation == generation {
                    active.mode = None;
                    active.task = None;
                }
            }
            outcome
        }));
        Ok(())
    }

    async fn sessions(&self) -> Vec<Json> {
        match self.active_service() {
            Some(service) => service.sessions().await,
            None => Vec::new(),
        }
    }

    async fn disconnect_session(&self, id: &str) -> bool {
        match self.active_service() {
            Some(service) => service.disconnect_session(id).await,
            None => false,
        }
    }
}

/// A supervisor over a fixed answer set, for tests of the routes alone.
#[cfg(test)]
pub(crate) struct FixedSupervisor {
    pub mode: Mutex<Option<String>>,
    pub modes: Vec<String>,
    pub sessions: Mutex<Vec<Json>>,
    pub closed: Mutex<Vec<String>>,
}

#[cfg(test)]
impl FixedSupervisor {
    pub(crate) fn new(mode: Option<&str>, modes: &[&str]) -> Arc<FixedSupervisor> {
        Arc::new(FixedSupervisor {
            mode: Mutex::new(mode.map(str::to_string)),
            modes: modes.iter().map(|m| m.to_string()).collect(),
            sessions: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
        })
    }
}

#[cfg(test)]
#[async_trait]
impl Supervisor for FixedSupervisor {
    fn current_mode(&self) -> Option<String> {
        self.mode.lock().clone()
    }

    fn available_modes(&self) -> Vec<String> {
        self.modes.clone()
    }

    async fn switch_to_mode(&self, mode: &str) -> Result<(), String> {
        if !self.modes.iter().any(|m| m == mode) {
            return Err(format!("Service {mode} not found"));
        }
        *self.mode.lock() = Some(mode.to_string());
        Ok(())
    }

    async fn sessions(&self) -> Vec<Json> {
        if self.mode.lock().is_none() {
            return Vec::new();
        }
        self.sessions.lock().clone()
    }

    async fn disconnect_session(&self, id: &str) -> bool {
        if self.mode.lock().is_none() {
            return false;
        }
        self.closed.lock().push(id.to_string());
        id == "abc"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    /// A service whose lifetime is a notify it ends on `stop`, counting its starts and stops.
    struct Fake {
        started: AtomicUsize,
        stopped: AtomicUsize,
        end: Notify,
        running: Mutex<bool>,
        die: bool,
    }

    impl Fake {
        fn new(die: bool) -> Arc<Fake> {
            Arc::new(Fake {
                started: AtomicUsize::new(0),
                stopped: AtomicUsize::new(0),
                end: Notify::new(),
                running: Mutex::new(false),
                die,
            })
        }
    }

    #[async_trait]
    impl StreamingService for Fake {
        async fn start(&self) -> Result<(), String> {
            self.started.fetch_add(1, Ordering::SeqCst);
            if self.die {
                return Err("boom".into());
            }
            *self.running.lock() = true;
            self.end.notified().await;
            *self.running.lock() = false;
            Ok(())
        }

        async fn stop(&self) {
            self.stopped.fetch_add(1, Ordering::SeqCst);
            self.end.notify_one();
        }

        async fn sessions(&self) -> Vec<Json> {
            vec![serde_json::json!({"id": "s1"})]
        }

        async fn disconnect_session(&self, id: &str) -> bool {
            id == "s1"
        }
    }

    #[tokio::test]
    async fn switch_stops_one_and_starts_the_other() {
        if crate::http::testing::rerun_alone(
            "http::supervisor::tests::switch_stops_one_and_starts_the_other",
        ) {
            return;
        }
        let ws = Fake::new(false);
        let rtc = Fake::new(false);
        crate::http::testing::ensure_settings();
        let sup = ModeSupervisor::new(vec![
            ("websockets".into(), ws.clone()),
            ("webrtc".into(), rtc.clone()),
        ]);
        assert_eq!(sup.current_mode(), None);
        assert!(
            sup.sessions().await.is_empty(),
            "no active transport lists nothing"
        );
        sup.switch_to_mode("websockets").await.unwrap();
        tokio::task::yield_now().await;
        assert_eq!(sup.current_mode().as_deref(), Some("websockets"));
        assert_eq!(
            crate::settings::settings().mode(),
            "websockets",
            "the switch writes the mode into the settings"
        );
        assert_eq!(sup.sessions().await.len(), 1);
        sup.switch_to_mode("websockets").await.unwrap();
        assert_eq!(
            ws.started.load(Ordering::SeqCst),
            1,
            "the same mode is a no-op"
        );
        sup.switch_to_mode("webrtc").await.unwrap();
        tokio::task::yield_now().await;
        assert_eq!(ws.stopped.load(Ordering::SeqCst), 1);
        assert!(
            !*ws.running.lock(),
            "the old service's task ended before the new one started"
        );
        assert_eq!(rtc.started.load(Ordering::SeqCst), 1);
        assert_eq!(sup.current_mode().as_deref(), Some("webrtc"));
        assert_eq!(
            sup.switch_to_mode("nope").await,
            Err("Service nope not found".into())
        );
        sup.stop().await;
        assert_eq!(sup.current_mode(), None);
    }

    #[tokio::test]
    async fn a_dying_service_clears_the_mode() {
        if crate::http::testing::rerun_alone(
            "http::supervisor::tests::a_dying_service_clears_the_mode",
        ) {
            return;
        }
        let dying = Fake::new(true);
        crate::http::testing::ensure_settings();
        let sup = ModeSupervisor::new(vec![("websockets".into(), dying)]);
        sup.switch_to_mode("websockets").await.unwrap();
        for _ in 0..50 {
            if sup.current_mode().is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            sup.current_mode(),
            None,
            "a task that dies leaves no stale mode"
        );
    }
}
