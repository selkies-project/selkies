/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The apps panel's runner: whether the panel has anything to work through here, what the
//! session has installed, and the jobs it runs for the panel (`app_jobs`).
//!
//! Availability is the wrapper plus an environment it can work in; where either is missing,
//! dashboards are told to drop the panel rather than offer buttons that can never succeed. A
//! proot that cannot start a guest here leaves the panel with nothing that can work, and only the
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
//! What a session has installed is the runner's install directory and nothing else: the runner
//! is asked (`list`) whenever a page wants to know -- the panel opening, a job ending -- and its
//! answer is never kept, since listing a directory costs less than keeping a second record
//! current against installs made from a terminal.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use log::{debug, info, warn};
use parking_lot::Mutex;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::app_jobs::{FLUSH_INTERVAL, JobBoard, JobOp, JobState};
use crate::commands::sessions::which;
use crate::input::command::spawn_merged;
use crate::settings::settings;

/// The wrapper the apps panel works through.
pub const APP_RUNNER: &str = "selkies-proot";
/// How long a runner question may take; a stuck child is killed.
const RUNNER_TIMEOUT: Duration = Duration::from_secs(10);
/// The terminal width a job's runner is told, which curl sizes its progress bar to.
const JOB_COLUMNS: &str = "60";
/// How long a job's output is still read once its runner exited.
const JOB_DRAIN: Duration = Duration::from_millis(300);

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

/// Decode output that may end inside a character: the complete text comes back, and the bytes of
/// a character the read split stay in `carry` for the next read.
pub fn decode_chunk(carry: &mut Vec<u8>, bytes: &[u8]) -> String {
    carry.extend_from_slice(bytes);
    let mut text = String::new();
    loop {
        match std::str::from_utf8(carry) {
            Ok(valid) => {
                text.push_str(valid);
                carry.clear();
                return text;
            }
            Err(e) => {
                let (valid, rest) = carry.split_at(e.valid_up_to());
                text.push_str(std::str::from_utf8(valid).unwrap_or_default());
                match e.error_len() {
                    // A character the next read completes.
                    None => {
                        *carry = rest.to_vec();
                        return text;
                    }
                    Some(bad) => {
                        text.push(char::REPLACEMENT_CHARACTER);
                        *carry = rest[bad..].to_vec();
                    }
                }
            }
        }
    }
}

/// What running the panel's jobs needs from the session: the environment its applications run
/// in, and a way to tell every page.
#[async_trait]
pub trait JobHost: Send + Sync {
    /// The environment a job's runner starts in: the session's display, bus, and repository.
    async fn job_env(&self) -> Option<HashMap<String, String>>;
    /// Send a system action to every page.
    fn broadcast(&self, action: &str);
}

/// The apps panel's runner as this session sees it; see the module docs.
pub struct AppsRunner {
    /// The name or path the runner resolves through.
    name: PathBuf,
    /// The probe's answer, `None` until it has spoken.
    usable: Mutex<Option<bool>>,
    /// The panel's jobs.
    pub jobs: JobBoard,
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
            jobs: JobBoard::default(),
        }
    }

    fn resolved(&self) -> Option<PathBuf> {
        which(&self.name.to_string_lossy())
    }

    /// Whether the panel has a runner it could work through here (module docs).
    pub fn available(&self) -> bool {
        self.resolved().is_some() && *self.usable.lock() != Some(false)
    }

    /// Ask the runner whether apps can install and launch here, once, and a usable one what it
    /// already has installed, for the log.
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
        if let Some(names) = self.list().await {
            let listed = if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            };
            info!(target: "input", "Apps installed in this session: {listed}");
        }
    }

    /// The app names the runner finds installed now, `None` where it could not say: a caller
    /// must not publish that as empty, since a page told nothing is installed offers to install
    /// what the session has.
    pub async fn list(&self) -> Option<Vec<String>> {
        let runner = self.resolved()?;
        let folder = settings().apps_repo_path();
        match ask(&runner, "list", &folder, false).await {
            Ok((true, output)) => Some(
                output
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string)
                    .collect(),
            ),
            Ok((false, _)) => {
                debug!(target: "input", "{APP_RUNNER} list failed");
                None
            }
            Err(e) => {
                warn!(target: "input", "{APP_RUNNER} list failed to run: {e}");
                None
            }
        }
    }

    /// The `apps_state,<json>` action: what is installed, read now (`null` where the runner could
    /// not say), and every job kept, each with its whole console.
    pub async fn state_action(&self) -> String {
        let installed = self.list().await;
        let state = json!({"installed": installed, "jobs": self.jobs.snapshot()});
        format!("apps_state,{state}")
    }

    /// Run the queue until it is empty, telling every page how each job goes and, after each,
    /// what is installed. Started by whoever queued a job onto an idle queue
    /// ([`JobBoard::enqueue`]), so one task runs it at a time.
    pub async fn drive(&self, host: &dyn JobHost) {
        while let Some((id, op, app, started)) = self.jobs.start_next() {
            host.broadcast(&started);
            let code = self.run_job(id, op, &app, host).await;
            if let Some(ended) = self.jobs.finish(id, code) {
                host.broadcast(&ended.action);
                let detail = if ended.reason.is_empty() {
                    String::new()
                } else {
                    format!(" -- {}", ended.reason)
                };
                match ended.state {
                    JobState::Done => {
                        info!(target: "input", "App {} {} in {:.1}s", ended.app, ended.op.done(), ended.seconds)
                    }
                    JobState::Canceled => {
                        info!(target: "input", "App {} {} canceled after {:.1}s", ended.op.verb(), ended.app, ended.seconds)
                    }
                    _ => {
                        let status = ended.code.map_or("could not run".to_string(), |c| {
                            format!("exited with status {c}")
                        });
                        warn!(target: "input", "App {} {} failed: {APP_RUNNER} {status} after {:.1}s{detail}", ended.op.verb(), ended.app, ended.seconds)
                    }
                }
            }
            host.broadcast(&self.state_action().await);
        }
    }

    /// Run one job's runner to its end, streaming what it prints; its exit status, signal deaths
    /// negative, `None` where it could not start. The job ends when the runner exits, its output
    /// read for [`JOB_DRAIN`] more, rather than when the output stream closes: whatever the runner
    /// leaves running on the session's behalf may hold the stream long after.
    async fn run_job(&self, id: u64, op: JobOp, app: &str, host: &dyn JobHost) -> Option<i32> {
        let runner = self.resolved()?;
        let mut env = host.job_env().await?;
        env.insert("PA_STATUS".into(), "1".into());
        env.insert("COLUMNS".into(), JOB_COLUMNS.into());
        let mut command = Command::new(&runner);
        command
            .arg(op.verb())
            .arg(app)
            .process_group(0)
            .kill_on_drop(true);
        let (mut child, mut reader) = match spawn_merged(command, &env) {
            Ok(pair) => pair,
            Err(e) => {
                self.jobs
                    .feed(id, &format!("{APP_RUNNER} could not start: {e}\n"));
                return None;
            }
        };
        if let Some(pid) = child.id() {
            self.jobs.set_group(id, pid as i32);
        }
        let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut chunk = vec![0u8; 4096];
        let mut carry = Vec::new();
        let mut exited = None;
        let drained = tokio::time::sleep(Duration::MAX);
        tokio::pin!(drained);
        loop {
            tokio::select! {
                read = reader.read(&mut chunk) => match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => self.jobs.feed(id, &decode_chunk(&mut carry, &chunk[..n])),
                },
                status = child.wait(), if exited.is_none() => {
                    exited = Some(status);
                    drained.as_mut().reset(tokio::time::Instant::now() + JOB_DRAIN);
                }
                _ = &mut drained, if exited.is_some() => break,
                _ = ticker.tick() => {
                    if let Some(update) = self.jobs.take_update(id) {
                        host.broadcast(&update);
                    }
                }
            }
        }
        use std::os::unix::process::ExitStatusExt;
        let status = match exited {
            Some(status) => status,
            None => child.wait().await,
        }
        .ok()?;
        Some(
            status
                .code()
                .unwrap_or_else(|| -status.signal().unwrap_or(1)),
        )
    }
}
