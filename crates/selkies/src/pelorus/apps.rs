/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The `apps` tool: the applications the session's app catalog offers, and installing,
//! updating, or removing them, as the dashboard's apps panel does.
//!
//! It is the panel's mechanism, not a second one: the catalog is the one the panel lists
//! ([`Settings::apps_catalog_source`](crate::settings::Settings::apps_catalog_source), read by
//! [`crate::files::apps`]'s rule), what is installed is the runner's answer, and a job goes on
//! the one queue the server's transports share ([`AppsRunner::queue`]), so the panel shows a
//! model's install as it runs and the two never run one application's jobs at once. The tool
//! exists only where the panel could act and has something to list: the runner can work here
//! ([`AppsRunner::available`]), the server lets a controller run commands (`command_enabled`),
//! and the catalog the session names is there.
//!
//! A job is queued and the call returns: an install takes minutes, and a run is stopped only
//! between its steps, so a call that waited for it would hold a stop back as long. The model
//! watches the job instead (`status`), a step at a time with `wait` between, and tells the user
//! how far it has got as it goes; the status is the job's own, its phase, download progress, and
//! latest output, read from the queue, or what is installed once the queue keeps no job for it.
//!
//! A remote catalog is fetched by the server and kept for [`CATALOG_KEPT`], since a model
//! searches it several times in one task and it changes with the repository's releases; a local
//! one is read each time, which costs a file read.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value as Json;

use super::model;
use super::tools::Output;
use crate::files::apps::{CatalogApp, catalog_text, parse_catalog};
use crate::session_screens::AppsRunner;
use crate::session_screens::app_jobs::{JobOp, JobState, JobStatus, Refused, valid_app_name};
use crate::settings::{AppsCatalog, settings};

/// How long a remote catalog is kept before it is fetched again.
const CATALOG_KEPT: Duration = Duration::from_secs(600);

/// The most bytes a remote catalog may hold.
const CATALOG_BYTES: usize = 4 * 1024 * 1024;

/// The lines of a job's output a status shows.
const STATUS_LINES: usize = 5;

/// The lines of a failed job's output a status shows.
const FAILURE_LINES: usize = 15;

/// The most applications a search lists.
const SEARCH_RESULTS: usize = 15;

/// The apps runner the transports share.
static RUNNER: OnceLock<Arc<AppsRunner>> = OnceLock::new();

/// The remote catalog last fetched: its URL, when, and what it lists.
static REMOTE: Mutex<Option<(String, Instant, Vec<CatalogApp>)>> = Mutex::new(None);

/// Give the tool the apps runner, once, at startup.
pub fn install(runner: Arc<AppsRunner>) {
    let _ = RUNNER.set(runner);
}

/// Whether the catalog the session names is there to read: a remote one is taken to be until a
/// fetch says otherwise.
fn catalog_present(source: &AppsCatalog) -> bool {
    match source {
        AppsCatalog::Remote(_) => true,
        AppsCatalog::Repo(folder) => Path::new(folder).is_dir(),
        AppsCatalog::File(file) => !file.is_empty() && Path::new(file).is_file(),
    }
}

/// The runner, where the session offers the tool (module docs), read off the runtime: the
/// runner is looked for along `PATH` and the catalog on disk.
async fn runner() -> Option<&'static Arc<AppsRunner>> {
    tokio::task::spawn_blocking(runner_now).await.ok().flatten()
}

/// [`runner`], on the calling thread.
fn runner_now() -> Option<&'static Arc<AppsRunner>> {
    let (enabled, source) = {
        let settings = settings();
        (
            settings.command_enabled().on,
            settings.apps_catalog_source(),
        )
    };
    RUNNER
        .get()
        .filter(|runner| enabled && runner.available() && catalog_present(&source))
}

/// Whether the session offers the tool (module docs).
pub async fn available() -> bool {
    runner().await.is_some()
}

/// The catalog's installable applications.
async fn catalog() -> Result<Vec<CatalogApp>, String> {
    let source = settings().apps_catalog_source();
    let AppsCatalog::Remote(url) = source else {
        let text = tokio::task::spawn_blocking(move || catalog_text(&source))
            .await
            .map_err(|e| e.to_string())?
            .ok_or("the catalog this session names is missing")?;
        return parse_catalog(&text);
    };
    if let Some((kept, at, apps)) = REMOTE.lock().unwrap_or_else(|p| p.into_inner()).as_ref()
        && *kept == url
        && at.elapsed() < CATALOG_KEPT
    {
        return Ok(apps.clone());
    }
    let apps = parse_catalog(&model::get_text(&url, CATALOG_BYTES).await?)?;
    *REMOTE.lock().unwrap_or_else(|p| p.into_inner()) = Some((url, Instant::now(), apps.clone()));
    Ok(apps)
}

/// How well `app` matches the lowercased words of a search, lower first, `None` for not at all:
/// every word has to appear in its name, shown name, category, or description, and a match on
/// the name ranks above one in the description.
fn rank(app: &CatalogApp, words: &[String]) -> Option<u8> {
    let name = app.name.to_lowercase();
    let full_name = app.full_name.to_lowercase();
    let rest = format!("{} {}", app.category, app.description).to_lowercase();
    let mut worst = 0;
    for word in words {
        let score = if name == *word || full_name == *word {
            0
        } else if name.contains(word.as_str()) || full_name.contains(word.as_str()) {
            1
        } else if rest.contains(word.as_str()) {
            2
        } else {
            return None;
        };
        worst = worst.max(score);
    }
    Some(worst)
}

/// One application as a search lists it.
fn line(app: &CatalogApp, installed: &[String], described: bool) -> String {
    let mut text = format!("{}: {}", app.name, app.full_name);
    if !app.category.is_empty() {
        text.push_str(&format!(" ({})", app.category));
    }
    if installed.contains(&app.name) {
        text.push_str(" [installed]");
    }
    if described && !app.description.is_empty() {
        text.push_str(&format!(" - {}", app.description.trim()));
    }
    text
}

/// `search`: the catalog's matches for `query`, or with none every application by name.
async fn search(query: &str, runner: &AppsRunner) -> Output {
    let apps = match catalog().await {
        Ok(apps) => apps,
        Err(e) => return Output::fail(format!("The app catalog could not be read: {e}")),
    };
    let listed = runner.list().await;
    // The runner could not say what is installed: no application is marked either way, and the
    // model is told why rather than shown everything as missing.
    let unknown = if listed.is_none() {
        "\nWhat is installed could not be read, so none is marked installed."
    } else {
        ""
    };
    let installed = listed.unwrap_or_default();
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        let lines: Vec<String> = apps.iter().map(|a| line(a, &installed, false)).collect();
        return Output::ok(format!(
            "The app catalog offers {} applications. Search with a word to read what one is.{unknown}\n{}",
            apps.len(),
            lines.join("\n")
        ));
    }
    let mut found: Vec<(u8, &CatalogApp)> = apps
        .iter()
        .filter_map(|app| rank(app, &words).map(|score| (score, app)))
        .collect();
    found.sort_by_key(|(score, _)| *score);
    if found.is_empty() {
        return Output::ok(format!(
            "No application in the app catalog matches \"{query}\", so it cannot be installed from \
             there."
        ));
    }
    let more = found.len().saturating_sub(SEARCH_RESULTS);
    let mut lines: Vec<String> = found
        .iter()
        .take(SEARCH_RESULTS)
        .map(|(_, app)| line(app, &installed, true))
        .collect();
    if more > 0 {
        lines.push(format!(
            "... and {more} more; search with more words to narrow it."
        ));
    }
    Output::ok(format!(
        "Matches in the app catalog, by the name to install them with:{unknown}\n{}",
        lines.join("\n")
    ))
}

/// What a model is told to do with an application that is installed, where `launch` exists.
fn start_with(launch: bool) -> &'static str {
    if launch {
        " Start it with `launch`."
    } else {
        ""
    }
}

/// `install`, `update`, or `remove`: queue the job and say how to follow it.
async fn job(op: JobOp, app: &str, runner: &Arc<AppsRunner>, launch: bool) -> Output {
    if !valid_app_name(app) {
        return Output::fail("`app` must be an application's name from `search`");
    }
    let Some(installed) = runner.list().await else {
        return Output::fail(
            "What is installed could not be read, so the job was not started. Try again in a moment.",
        );
    };
    match (op, installed.iter().any(|a| a == app)) {
        (JobOp::Install, true) => {
            return Output::ok(format!("{app} is already installed.{}", start_with(launch)));
        }
        (JobOp::Remove | JobOp::Update, false) => {
            return Output::fail(format!("{app} is not installed."));
        }
        _ => {}
    }
    if op == JobOp::Install {
        match catalog().await {
            Ok(apps) if apps.iter().any(|a| a.name == app) => {}
            Ok(_) => {
                return Output::fail(format!(
                    "{app} is not in the app catalog. Use `search` for the name it is listed under."
                ));
            }
            Err(e) => return Output::fail(format!("The app catalog could not be read: {e}")),
        }
    }
    match runner.queue(op, app) {
        Ok(_) => Output::ok(format!(
            "The {} of {app} has started and shows in the apps panel. It takes a while: follow it \
             with `status`, waiting a few seconds between checks, and tell the user how far it has \
             got until it ends.",
            op.verb()
        )),
        Err(Refused::BadName) => Output::fail("`app` must be an application's name from `search`"),
        Err(Refused::Busy) => Output::fail(format!(
            "A job for {app} is already queued or running. Follow it with `status`."
        )),
        Err(Refused::Full) => {
            Output::fail("The apps queue is full. Try again once its jobs have ended.")
        }
        Err(Refused::NoSession) => {
            Output::fail("No session runs the apps queue yet. Try again in a moment.")
        }
    }
}

/// How far a job has got, in the words a model passes on, and whether it failed.
fn describe(app: &str, job: &JobStatus, launch: bool) -> (String, bool) {
    let verb = job.op.verb();
    let output = |lines: &[String]| {
        if lines.is_empty() {
            String::new()
        } else {
            format!(" Its latest output:\n{}", lines.join("\n"))
        }
    };
    let latest = &job.lines[job.lines.len().saturating_sub(STATUS_LINES)..];
    match job.state {
        JobState::Queued => (
            format!("The {verb} of {app} is waiting in the apps queue behind another job."),
            false,
        ),
        JobState::Running => {
            let step = match job.phase.as_deref() {
                Some("download") => "downloading".to_string(),
                Some("install") => "installing".to_string(),
                Some("remove") => "removing".to_string(),
                Some(other) => other.to_string(),
                None => "starting".to_string(),
            };
            let progress = job
                .progress
                .map(|p| format!(", {p:.0}% done"))
                .unwrap_or_default();
            (
                format!(
                    "The {verb} of {app} is running: {step}{progress}.{}",
                    output(latest)
                ),
                false,
            )
        }
        JobState::Done => {
            let next = if job.op == JobOp::Remove {
                ""
            } else {
                start_with(launch)
            };
            (format!("{app} is {}.{next}", job.op.done()), false)
        }
        JobState::Canceled => (format!("The {verb} of {app} was canceled."), true),
        JobState::Failed => {
            let status = job.code.map_or("could not run".to_string(), |c| {
                format!("exited with status {c}")
            });
            (
                format!(
                    "The {verb} of {app} failed: the runner {status}.{}",
                    output(&job.lines)
                ),
                true,
            )
        }
    }
}

/// `status`: where the latest job for `app` is, or with none kept whether it is installed.
async fn status(app: &str, runner: &AppsRunner, launch: bool) -> Output {
    if !valid_app_name(app) {
        return Output::fail("`app` must be an application's name from `search`");
    }
    if let Some(job) = runner.jobs.status(app, FAILURE_LINES) {
        let (text, failed) = describe(app, &job, launch);
        return if failed {
            Output::fail(text)
        } else {
            Output::ok(text)
        };
    }
    match runner.list().await {
        Some(list) if list.iter().any(|a| a == app) => {
            Output::ok(format!("No job for {app} is kept. It is installed."))
        }
        Some(_) => Output::ok(format!("No job for {app} is kept. It is not installed.")),
        None => Output::fail(format!(
            "No job for {app} is kept, and what is installed could not be read."
        )),
    }
}

/// The `apps` tool; `launch` says whether the session has that tool to start an application with.
pub async fn call(args: &Json, launch: bool) -> Output {
    let Some(runner) = runner().await else {
        return Output::fail("Applications cannot be installed from the app catalog here.");
    };
    let app = args.get("app").and_then(Json::as_str).unwrap_or("").trim();
    match args
        .get("action")
        .and_then(Json::as_str)
        .unwrap_or("search")
    {
        "search" => search(app, runner).await,
        "status" => status(app, runner, launch).await,
        action => match JobOp::parse(action) {
            Some(op) => job(op, app, runner, launch).await,
            None => Output::fail("`action` is search, install, update, remove, or status"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, full_name: &str, category: &str, description: &str) -> CatalogApp {
        CatalogApp {
            name: name.into(),
            full_name: full_name.into(),
            description: description.into(),
            category: category.into(),
        }
    }

    fn words(query: &str) -> Vec<String> {
        query.split_whitespace().map(str::to_lowercase).collect()
    }

    #[test]
    fn a_search_ranks_a_name_above_a_description_and_needs_every_word() {
        let firefox = app(
            "firefox",
            "Firefox",
            "Network",
            "A web browser from Mozilla.",
        );
        let zen = app("zen", "Zen Browser", "Network", "A fork of Firefox.");
        let gimp = app("gimp", "GIMP", "Creative", "An image editor.");
        assert_eq!(rank(&firefox, &words("Firefox")), Some(0));
        assert_eq!(rank(&zen, &words("firefox")), Some(2));
        assert_eq!(rank(&zen, &words("browser")), Some(1));
        assert_eq!(rank(&firefox, &words("web browser")), Some(2));
        assert_eq!(rank(&gimp, &words("browser")), None);
        assert_eq!(rank(&firefox, &words("firefox editor")), None);
    }

    fn job(op: JobOp, state: JobState) -> JobStatus {
        JobStatus {
            op,
            state,
            phase: None,
            progress: None,
            code: None,
            lines: Vec::new(),
        }
    }

    #[test]
    fn a_status_says_how_far_a_job_has_got_and_fails_only_where_it_did() {
        let mut running = job(JobOp::Install, JobState::Running);
        running.phase = Some("download".into());
        running.progress = Some(41.6);
        running.lines = (1..=8).map(|n| format!("line {n}")).collect();
        let (text, failed) = describe("gimp", &running, true);
        assert!(!failed);
        assert!(
            text.starts_with("The install of gimp is running: downloading, 42% done."),
            "{text}"
        );
        assert!(
            text.contains("line 4\nline 5") && !text.contains("line 3"),
            "{text}"
        );
        let (text, failed) = describe("gimp", &job(JobOp::Update, JobState::Done), true);
        assert_eq!(
            (text.as_str(), failed),
            ("gimp is updated. Start it with `launch`.", false)
        );
        let (text, _) = describe("gimp", &job(JobOp::Remove, JobState::Done), true);
        assert_eq!(text, "gimp is removed.");
        let mut broken = job(JobOp::Update, JobState::Failed);
        broken.code = Some(1);
        broken.lines = vec!["error: no manifest".into()];
        let (text, failed) = describe("gimp", &broken, true);
        assert!(failed);
        assert_eq!(
            text,
            "The update of gimp failed: the runner exited with status 1. Its latest output:\n\
             error: no manifest"
        );
        assert!(describe("gimp", &job(JobOp::Install, JobState::Canceled), true).1);
        assert_eq!(
            describe("gimp", &job(JobOp::Install, JobState::Done), false).0,
            "gimp is installed.",
            "no `launch` is named where the session has none"
        );
    }

    #[test]
    fn a_catalog_counts_as_present_only_where_it_can_be_read() {
        let root =
            std::env::temp_dir().join(format!("selkies-pelorus-apps-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("m.yml");
        std::fs::write(&file, "include: []\n").unwrap();
        let path = |p: &Path| p.to_str().unwrap().to_string();
        assert!(catalog_present(&AppsCatalog::Remote(
            "https://example.com/m.yml".into()
        )));
        assert!(catalog_present(&AppsCatalog::File(path(&file))));
        assert!(catalog_present(&AppsCatalog::Repo(path(&root))));
        assert!(!catalog_present(&AppsCatalog::File(String::new())));
        assert!(!catalog_present(&AppsCatalog::File(path(
            &root.join("absent.yml")
        ))));
        assert!(!catalog_present(&AppsCatalog::Repo(path(
            &root.join("absent")
        ))));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_listed_application_says_whether_it_is_installed() {
        let firefox = app("firefox", "Firefox", "Network", "A web browser. ");
        let installed = vec!["firefox".to_string()];
        assert_eq!(
            line(&firefox, &installed, true),
            "firefox: Firefox (Network) [installed] - A web browser."
        );
        assert_eq!(
            line(&app("tool", "Tool", "", ""), &installed, true),
            "tool: Tool"
        );
        assert_eq!(line(&firefox, &[], false), "firefox: Firefox (Network)");
    }
}
