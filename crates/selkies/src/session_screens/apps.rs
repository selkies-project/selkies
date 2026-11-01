/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The apps panel's runner: whether the panel has anything to work through here, and what the
//! session has installed.
//!
//! Availability is the wrapper plus an environment it can work in; where either is missing,
//! dashboards are told to drop the panel rather than offer buttons that can never succeed. proot
//! emulates its chroot by tracing every process it starts, so a host that denies ptrace (Yama
//! restricted, no `CAP_SYS_PTRACE`) leaves the panel with nothing that can work, and only the
//! wrapper knows how to decide that for the runner it ships, so it is asked (`check`) rather than
//! reimplemented. The answer is assumed good until the probe has spoken, so a working session
//! never flickers the panel.
//!
//! A local repository (`apps_repo_folder`) changes what the panel lists, not whether it shows: the
//! panel reads that catalog from this server (`files::apps`) and the runner installs from the same
//! folder, the setting's reading written where the runner reads it (`PA_REPO_FOLDER`), so the
//! runner and the panel never read two repositories. `command_enabled` is permission, not
//! availability: with it off the panel still shows and the client explains why its actions are
//! disabled, so an operator flipping the flag does not make the panel appear and vanish.
//!
//! Only the runner knows what a session has, and the client cannot run anything, so the server
//! asks (`list`), at startup so the first client to connect is told rather than having to
//! remember, and after a command that changes the set. The set is `None` until a read succeeds,
//! which a caller must not publish as empty: a client told nothing is installed clears its own
//! record. A failed read leaves the last answer standing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use log::{debug, info, warn};
use parking_lot::Mutex;
use tokio::process::Command;

use crate::commands::sessions::which;
use crate::settings::settings;

/// The wrapper the apps panel works through.
pub const APP_RUNNER: &str = "selkies-proot";
/// How long a runner question may take; a stuck child is killed.
const RUNNER_TIMEOUT: Duration = Duration::from_secs(10);

/// The runner's environment: the server's, with the repository where the runner reads it.
pub fn runner_env(repo_folder: &str) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    if repo_folder.is_empty() {
        env.remove("PA_REPO_FOLDER");
    } else {
        env.insert("PA_REPO_FOLDER".into(), repo_folder.to_string());
    }
    env
}

/// Whether a finished command changes the installed set: the runner's `install`, `remove`, or
/// `update`; a launch does not.
pub fn changes_installed_set(command: &str) -> bool {
    let mut parts = command.split_whitespace();
    let is_runner = parts.next().is_some_and(|program| {
        Path::new(program)
            .file_name()
            .is_some_and(|n| n == APP_RUNNER)
    });
    is_runner && matches!(parts.next(), Some("install" | "remove" | "update"))
}

/// One question to the runner: its exit status and what it wrote, `Err` when it could not run or
/// outran [`RUNNER_TIMEOUT`], in which case it is killed.
async fn ask(
    runner: &Path,
    verb: &str,
    repo_folder: &str,
    merge_stderr: bool,
) -> Result<(bool, String), String> {
    let mut command = Command::new(runner);
    command
        .arg(verb)
        .env_clear()
        .envs(runner_env(repo_folder))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if merge_stderr {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .kill_on_drop(true);
    let child = command.spawn().map_err(|e| e.to_string())?;
    match tokio::time::timeout(RUNNER_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            if merge_stderr {
                text.push_str(&String::from_utf8_lossy(&output.stderr));
            }
            Ok((output.status.success(), text))
        }
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("timed out after {RUNNER_TIMEOUT:?}")),
    }
}

/// The apps panel's runner as this session sees it; see the module docs.
pub struct AppsRunner {
    /// The name or path the runner resolves through.
    name: PathBuf,
    /// The probe's answer, `None` until it has spoken.
    usable: Mutex<Option<bool>>,
    installed: Mutex<Option<Vec<String>>>,
}

impl Default for AppsRunner {
    fn default() -> AppsRunner {
        AppsRunner::named(APP_RUNNER)
    }
}

impl AppsRunner {
    /// A runner resolved from `name`: along `PATH`, or as given when it carries a directory.
    pub fn named(name: impl Into<PathBuf>) -> AppsRunner {
        AppsRunner {
            name: name.into(),
            usable: Mutex::new(None),
            installed: Mutex::new(None),
        }
    }

    fn resolved(&self) -> Option<PathBuf> {
        which(&self.name.to_string_lossy())
    }

    /// Whether the panel has a runner it could work through here (module docs).
    pub fn available(&self) -> bool {
        self.resolved().is_some() && *self.usable.lock() != Some(false)
    }

    /// The app names the runner reports installed, as last read; `None` until a read succeeds.
    pub fn installed(&self) -> Option<Vec<String>> {
        self.installed.lock().clone()
    }

    /// Ask the runner whether apps can install and launch here, once, and a usable one what it
    /// already has installed.
    pub async fn probe(&self) {
        let Some(runner) = self.resolved() else {
            return;
        };
        let folder = settings().apps_repo_path();
        let answer = ask(&runner, "check", &folder, true).await;
        let (ok, output) = match answer {
            Ok(answer) => answer,
            Err(e) => {
                warn!(target: "input", "{APP_RUNNER} check failed to run: {e}");
                *self.usable.lock() = Some(false);
                return;
            }
        };
        *self.usable.lock() = Some(ok);
        if !ok {
            let mut lines = output.trim().lines();
            warn!(target: "input", "Apps panel hidden: {APP_RUNNER} cannot run here. {}", lines.next().unwrap_or_default());
            for line in lines {
                warn!(target: "input", "  {}", line.trim());
            }
            return;
        }
        if folder.is_empty() {
            info!(target: "input", "Apps panel enabled: {APP_RUNNER} can run here.");
        } else if Path::new(&folder).is_dir() {
            let count = crate::files::apps::listed_apps(Path::new(&folder)).len();
            let plural = if count == 1 { "" } else { "s" };
            info!(target: "input", "Apps panel enabled: {APP_RUNNER} can run here, catalog from the local repository {folder} ({count} application{plural}).");
        } else {
            warn!(target: "input", "Apps panel enabled: {APP_RUNNER} can run here, but the local repository {folder} is not a directory; the catalog fails to load until it is mounted.");
        }
        self.refresh().await;
    }

    /// Re-read the runner's installed set; whether it changed, so a caller can skip announcing.
    pub async fn refresh(&self) -> bool {
        let Some(runner) = self.resolved() else {
            return false;
        };
        let folder = settings().apps_repo_path();
        let output = match ask(&runner, "list", &folder, false).await {
            Ok((true, output)) => output,
            Ok((false, _)) => {
                debug!(target: "input", "{APP_RUNNER} list failed; keeping the installed set as it was");
                return false;
            }
            Err(e) => {
                warn!(target: "input", "{APP_RUNNER} list failed to run: {e}");
                return false;
            }
        };
        let names: Vec<String> = output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        let mut installed = self.installed.lock();
        let changed = installed.as_ref() != Some(&names);
        if changed {
            let listed = if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            };
            info!(target: "input", "Apps installed in this session: {listed}");
        }
        *installed = Some(names);
        changed
    }

    /// Re-read the set after `command` finished, returning the `apps_installed,<json>` action to
    /// broadcast where it changed: the requester applied the change optimistically, and the
    /// session's other pages have no other way to hear.
    pub async fn note_command_finished(&self, command: &str) -> Option<String> {
        if !changes_installed_set(command) || !self.refresh().await {
            return None;
        }
        let names = self.installed()?;
        Some(format!(
            "apps_installed,{}",
            serde_json::to_string(&names).ok()?
        ))
    }
}
