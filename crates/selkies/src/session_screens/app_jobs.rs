/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The apps panel's jobs: the installs, removals, and updates a controller asks for, run by the
//! server one at a time and streamed to every page while they run.
//!
//! A page names an operation and an application (the `apps,` verb), never a command line: the
//! name is checked here ([`valid_app_name`]) and handed to the runner as an argument, so nothing
//! a page sends reaches a shell. Jobs run in the order they were asked for, one at a time:
//! two installs at once split the link between their downloads and race on the icon cache and
//! the menu reload every install rewrites.
//!
//! What a job prints is its [`Console`]: the lines a terminal would be left showing, a carriage
//! return rewriting the line it ends, so curl's progress bar is one line that moves rather than
//! a frame per tick. The runner is started with `PA_STATUS=1`, under which proot-apps adds
//! `##pa phase <name>` lines (`download`, `install`, `remove`); the console takes those as the
//! job's phase instead of showing them, and a download's progress is the percentage its
//! progress bar last printed. While a job runs, what changed is sent at most once per
//! [`FLUSH_INTERVAL`] as one `apps_job` action: the job's state, the lines committed since the
//! last one, and the line being written. `apps_state` carries every job with its console, and
//! the installed set, for a page opening the panel; [`SNAPSHOT_BYTES`] bounds the consoles it
//! carries, newest job first, so the action stays well inside the 256 KiB a browser takes as one
//! data-channel message.
//!
//! A finished job keeps its console until a page clears it, so the user reads how it ended
//! however quickly it ran; [`FINISHED_KEPT`] bounds how many are kept, and [`PENDING_KEPT`] how
//! many may wait or run, so every job `apps_state` carries fits it. Canceling a queued job
//! ends it before it starts; canceling a running one sends its process group SIGTERM, on which
//! proot-apps removes the download it had started.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value as Json, json};

/// How often a running job's new output is sent.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(150);
/// The finished jobs kept for the panel, the oldest dropped first.
pub const FINISHED_KEPT: usize = 10;
/// The jobs queued or running at once; a request past it is refused.
pub const PENDING_KEPT: usize = 50;
/// The lines a console keeps, the oldest dropped first.
pub const CONSOLE_LINES: usize = 200;
/// The characters a console line keeps.
pub const LINE_CHARS: usize = 500;
/// The console bytes one `apps_state` carries in all.
pub const SNAPSHOT_BYTES: usize = 128 * 1024;
/// The prefix of a status line proot-apps prints under `PA_STATUS`.
const STATUS_PREFIX: &str = "##pa ";

/// Whether `name` may be handed to the runner as an application: what proot-apps takes on its
/// command line (a short name, or `registry/namespace/image:tag`), never `all`, which every
/// verb of the runner reads as each installed application, and never something it would read as
/// an option.
pub fn valid_app_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name != "all"
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | ':' | '-'))
}

/// What a job does to its application.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobOp {
    Install,
    Remove,
    Update,
}

impl JobOp {
    /// The operation a page names, `None` for any other word.
    pub fn parse(word: &str) -> Option<JobOp> {
        match word {
            "install" => Some(JobOp::Install),
            "remove" => Some(JobOp::Remove),
            "update" => Some(JobOp::Update),
            _ => None,
        }
    }

    /// The runner's verb for it.
    pub fn verb(self) -> &'static str {
        match self {
            JobOp::Install => "install",
            JobOp::Remove => "remove",
            JobOp::Update => "update",
        }
    }

    /// The past tense the log states a finished job in.
    pub fn done(self) -> &'static str {
        match self {
            JobOp::Install => "installed",
            JobOp::Remove => "removed",
            JobOp::Update => "updated",
        }
    }
}

/// Where a job is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Canceled,
}

impl JobState {
    /// Whether the job has ended, however it did.
    pub fn finished(self) -> bool {
        matches!(self, JobState::Done | JobState::Failed | JobState::Canceled)
    }
}

/// The text a job printed, as a terminal would be left showing it; see the module docs.
#[derive(Debug, Default)]
pub struct Console {
    /// The committed lines, oldest first.
    lines: VecDeque<String>,
    /// The line being written.
    current: String,
    /// A carriage return ended the last write: the next character starts the line over.
    overwrite: bool,
    /// Lines committed since the last [`Console::take_update`].
    fresh: Vec<String>,
    /// Whether anything changed since the last [`Console::take_update`].
    changed: bool,
}

impl Console {
    /// Take in what the program wrote; the phases its status lines named, in order.
    pub fn feed(&mut self, text: &str) -> Vec<String> {
        let mut phases = Vec::new();
        for c in text.chars() {
            match c {
                '\n' => {
                    if let Some(phase) = self.commit() {
                        phases.push(phase);
                    }
                }
                '\r' => self.overwrite = true,
                _ => {
                    if self.overwrite {
                        self.current.clear();
                        self.overwrite = false;
                    }
                    if self.current.chars().count() < LINE_CHARS {
                        self.current.push(c);
                    }
                }
            }
        }
        self.changed = true;
        phases
    }

    /// End the line being written: a status line comes back as its phase, any other joins the
    /// console.
    fn commit(&mut self) -> Option<String> {
        self.overwrite = false;
        let line = std::mem::take(&mut self.current);
        if let Some(status) = line.strip_prefix(STATUS_PREFIX) {
            return status
                .strip_prefix("phase ")
                .map(|phase| phase.trim().to_string());
        }
        if self.lines.len() == CONSOLE_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line.clone());
        if self.fresh.len() == CONSOLE_LINES {
            self.fresh.remove(0);
        }
        self.fresh.push(line);
        None
    }

    /// End the output: the line being written, where there is one, joins the console.
    fn close(&mut self) {
        if !self.current.is_empty() {
            self.commit();
        }
        self.changed = true;
    }

    /// The line being written, unless it is a status line still arriving.
    pub fn current(&self) -> &str {
        if STATUS_PREFIX.starts_with(self.current.as_str())
            || self.current.starts_with(STATUS_PREFIX)
        {
            ""
        } else {
            &self.current
        }
    }

    /// The lines committed and the line written since the last call, `None` where nothing
    /// changed.
    pub fn take_update(&mut self) -> Option<(Vec<String>, String)> {
        if !self.changed {
            return None;
        }
        self.changed = false;
        Some((std::mem::take(&mut self.fresh), self.current().to_string()))
    }

    /// Every line kept, the last one the line being written where there is one.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self.lines.iter().cloned().collect();
        if !self.current().is_empty() {
            lines.push(self.current().to_string());
        }
        lines
    }
}

/// The percentage a progress bar's line ends with (`######   45.3%`), `None` for another line.
pub fn progress_percent(line: &str) -> Option<f32> {
    let number = line.trim_end().strip_suffix('%')?;
    let start = number
        .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map_or(0, |i| i + 1);
    let value: f32 = number[start..].parse().ok()?;
    Some(value.clamp(0.0, 100.0))
}

/// One job of the panel's; see the module docs.
#[derive(Debug)]
pub struct Job {
    pub id: u64,
    pub app: String,
    pub op: JobOp,
    pub state: JobState,
    /// The step proot-apps last named, `None` before it named one.
    pub phase: Option<String>,
    /// A download's percentage, `None` outside a download.
    pub progress: Option<f32>,
    /// The exit status, signal deaths negative, once the job ran to an end.
    pub code: Option<i32>,
    pub console: Console,
    /// The process group, while it runs.
    pgid: Option<i32>,
    /// A page canceled it while it ran.
    cancel: bool,
    started: Option<Instant>,
}

impl Job {
    fn new(id: u64, op: JobOp, app: String) -> Job {
        Job {
            id,
            app,
            op,
            state: JobState::Queued,
            phase: None,
            progress: None,
            code: None,
            console: Console::default(),
            pgid: None,
            cancel: false,
            started: None,
        }
    }

    /// The job without its console.
    fn summary(&self) -> serde_json::Map<String, Json> {
        let Json::Object(map) = json!({
            "id": self.id,
            "app": self.app,
            "op": self.op,
            "state": self.state,
            "phase": self.phase,
            "progress": self.progress,
            "code": self.code,
        }) else {
            unreachable!("an object literal")
        };
        map
    }

    /// The job with its console's last lines, as many as `budget` bytes hold, which it is
    /// charged for: the committed lines, and the line being written.
    fn full(&self, budget: &mut usize) -> Json {
        let mut map = self.summary();
        let current = self.console.current();
        let current = if current.len() <= *budget {
            current
        } else {
            ""
        };
        *budget -= current.len();
        let mut kept: Vec<&String> = Vec::new();
        for line in self.console.lines.iter().rev() {
            if line.len() + 1 > *budget {
                break;
            }
            *budget -= line.len() + 1;
            kept.push(line);
        }
        kept.reverse();
        map.insert("lines".into(), json!(kept));
        map.insert("current".into(), json!(current));
        Json::Object(map)
    }

    /// The `apps_job` action carrying what changed: the console's new lines and current line,
    /// or the whole console with `reset` where `whole` asks for it.
    fn update_action(&mut self, whole: bool) -> String {
        let mut map = self.summary();
        if whole {
            self.console.take_update();
            map.insert("reset".into(), json!(true));
            map.insert(
                "lines".into(),
                json!(self.console.lines.iter().collect::<Vec<_>>()),
            );
            map.insert("current".into(), json!(self.console.current()));
        } else if let Some((lines, current)) = self.console.take_update() {
            map.insert("append".into(), json!(lines));
            map.insert("current".into(), json!(current));
        }
        format!("apps_job,{}", Json::Object(map))
    }
}

/// How a job ended, for the log.
#[derive(Debug)]
pub struct Ended {
    pub app: String,
    pub op: JobOp,
    pub state: JobState,
    pub code: Option<i32>,
    pub seconds: f64,
    /// The line of its output a reader would quote.
    pub reason: String,
    /// The `apps_job` action announcing it.
    pub action: String,
}

/// The queue and the jobs kept; every method takes the lock for itself and never across an
/// await.
#[derive(Debug, Default)]
pub struct JobBoard {
    inner: Mutex<Board>,
}

#[derive(Debug, Default)]
struct Board {
    jobs: Vec<Job>,
    next_id: u64,
    /// Whether a task is running the queue.
    driving: bool,
}

/// Why a job was not queued.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// Not a name the runner may be given.
    BadName,
    /// The application already has a job queued or running.
    Busy,
    /// [`PENDING_KEPT`] jobs are already queued or running.
    Full,
}

impl JobBoard {
    /// Queue `op` on `app`: the `apps_job` action announcing it, and whether the caller has to
    /// start a task to run the queue, which happens exactly when none is running it.
    pub fn enqueue(&self, op: JobOp, app: &str) -> Result<(String, bool), Refused> {
        if !valid_app_name(app) {
            return Err(Refused::BadName);
        }
        let mut board = self.inner.lock();
        let pending = board.jobs.iter().filter(|j| !j.state.finished());
        if pending.clone().any(|j| j.app == app) {
            return Err(Refused::Busy);
        }
        if pending.count() >= PENDING_KEPT {
            return Err(Refused::Full);
        }
        board.next_id += 1;
        let mut job = Job::new(board.next_id, op, app.to_string());
        let action = job.update_action(true);
        board.jobs.push(job);
        let start = !board.driving;
        board.driving = true;
        Ok((action, start))
    }

    /// Start the next queued job: its id, operation, application, and announcing action; `None`
    /// once the queue is empty, which also releases the queue for the next [`JobBoard::enqueue`]
    /// to start, under the same lock so no job is left queued with nobody running it.
    pub fn start_next(&self) -> Option<(u64, JobOp, String, String)> {
        let mut board = self.inner.lock();
        let Some(job) = board.jobs.iter_mut().find(|j| j.state == JobState::Queued) else {
            board.driving = false;
            return None;
        };
        job.state = JobState::Running;
        job.started = Some(Instant::now());
        let action = job.update_action(true);
        Some((job.id, job.op, job.app.clone(), action))
    }

    /// Note the process group a running job started as.
    pub fn set_group(&self, id: u64, pgid: i32) {
        let mut board = self.inner.lock();
        if let Some(job) = board.jobs.iter_mut().find(|j| j.id == id) {
            job.pgid = Some(pgid);
            if job.cancel {
                let _ = killpg(Pid::from_raw(pgid), Signal::SIGTERM);
            }
        }
    }

    /// Take in output of a running job: its phase and progress follow what it printed.
    pub fn feed(&self, id: u64, text: &str) {
        let mut board = self.inner.lock();
        let Some(job) = board.jobs.iter_mut().find(|j| j.id == id) else {
            return;
        };
        for phase in job.console.feed(text) {
            job.progress = None;
            job.phase = Some(phase);
        }
        if job.phase.as_deref() == Some("download")
            && let Some(percent) = progress_percent(job.console.current())
                .or_else(|| job.console.lines.back().and_then(|l| progress_percent(l)))
        {
            job.progress = Some(percent);
        }
    }

    /// The `apps_job` action for what a running job printed since the last, `None` where it
    /// printed nothing.
    pub fn take_update(&self, id: u64) -> Option<String> {
        let mut board = self.inner.lock();
        let job = board.jobs.iter_mut().find(|j| j.id == id)?;
        if !job.console.changed {
            return None;
        }
        Some(job.update_action(false))
    }

    /// End a running job with its exit status (`None` where it could not start or be waited
    /// for), and drop the oldest finished jobs past [`FINISHED_KEPT`].
    pub fn finish(&self, id: u64, code: Option<i32>) -> Option<Ended> {
        let mut board = self.inner.lock();
        let job = board.jobs.iter_mut().find(|j| j.id == id)?;
        job.console.close();
        job.pgid = None;
        job.code = code;
        job.progress = None;
        job.state = if job.cancel {
            JobState::Canceled
        } else if code == Some(0) {
            JobState::Done
        } else {
            JobState::Failed
        };
        let tail: VecDeque<String> = job
            .console
            .lines
            .iter()
            .rev()
            .take(20)
            .rev()
            .cloned()
            .collect();
        let ended = Ended {
            app: job.app.clone(),
            op: job.op,
            state: job.state,
            code,
            seconds: job.started.map_or(0.0, |s| s.elapsed().as_secs_f64()),
            reason: crate::input::command::command_failure_reason(&tail),
            action: job.update_action(false),
        };
        let finished = board.jobs.iter().filter(|j| j.state.finished()).count();
        let mut excess = finished.saturating_sub(FINISHED_KEPT);
        board.jobs.retain(|j| {
            if excess > 0 && j.state.finished() {
                excess -= 1;
                return false;
            }
            true
        });
        Some(ended)
    }

    /// Cancel a job: a queued one ends at once, a running one is sent SIGTERM and ends as
    /// canceled when it exits. The `apps_job` action where the job's state changed.
    pub fn cancel(&self, id: u64) -> Option<String> {
        let mut board = self.inner.lock();
        let job = board.jobs.iter_mut().find(|j| j.id == id)?;
        match job.state {
            JobState::Queued => {
                job.state = JobState::Canceled;
                Some(job.update_action(false))
            }
            JobState::Running if !job.cancel => {
                job.cancel = true;
                if let Some(pgid) = job.pgid {
                    let _ = killpg(Pid::from_raw(pgid), Signal::SIGTERM);
                }
                None
            }
            _ => None,
        }
    }

    /// Drop a finished job, or every finished job for `None`; whether any was dropped.
    pub fn clear(&self, id: Option<u64>) -> bool {
        let mut board = self.inner.lock();
        let before = board.jobs.len();
        board
            .jobs
            .retain(|j| !(j.state.finished() && id.is_none_or(|id| id == j.id)));
        board.jobs.len() != before
    }

    /// Every job kept, in the order asked for, with as much of each console as
    /// [`SNAPSHOT_BYTES`] holds, the newest job's first.
    pub fn snapshot(&self) -> Vec<Json> {
        let board = self.inner.lock();
        let mut budget = SNAPSHOT_BYTES;
        let mut jobs: Vec<Json> = board
            .jobs
            .iter()
            .rev()
            .map(|j| j.full(&mut budget))
            .collect();
        jobs.reverse();
        jobs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_carriage_return_rewrites_its_line_and_a_status_line_is_a_phase() {
        let mut console = Console::default();
        assert_eq!(
            console.feed("Installing firefox\n##pa phase down"),
            Vec::<String>::new()
        );
        assert_eq!(
            console.current(),
            "",
            "a status line still arriving is not shown"
        );
        assert_eq!(console.feed("load\n"), vec!["download".to_string()]);
        console.feed("\r##        10.0%\r#####     50.5%");
        assert_eq!(console.current(), "#####     50.5%");
        console.feed("\r##########100.0%\r\n");
        assert_eq!(
            console.lines(),
            vec![
                "Installing firefox".to_string(),
                "##########100.0%".to_string()
            ],
            "the meter leaves its last state, and the status line is not shown"
        );
        let (fresh, current) = console.take_update().expect("changed");
        assert_eq!(fresh.len(), 2);
        assert_eq!(current, "");
        assert!(console.take_update().is_none(), "nothing new since");
    }

    #[test]
    fn a_console_keeps_its_last_lines() {
        let mut console = Console::default();
        let text: String = (0..CONSOLE_LINES + 5)
            .map(|i| format!("line {i}\n"))
            .collect();
        console.feed(&text);
        let lines = console.lines();
        assert_eq!(lines.len(), CONSOLE_LINES);
        assert_eq!(lines[0], "line 5");
        console.feed(&"x".repeat(LINE_CHARS * 2));
        assert_eq!(console.current().len(), LINE_CHARS);
    }

    #[test]
    fn a_progress_bar_reads_as_its_percentage() {
        assert_eq!(
            progress_percent("######                      12.3%"),
            Some(12.3)
        );
        assert_eq!(progress_percent("100%"), Some(100.0));
        assert_eq!(progress_percent("##########  100.0% "), Some(100.0));
        assert_eq!(progress_percent("Adding icon"), None);
        assert_eq!(progress_percent("%"), None);
    }

    #[test]
    fn only_a_name_the_runner_takes_as_one_application_is_valid() {
        for good in [
            "firefox",
            "ghcr.io/myorg/proot-apps:tool",
            "firefox-dev",
            "a_b.c",
        ] {
            assert!(valid_app_name(good), "{good}");
        }
        for bad in [
            "",
            "all",
            "-h",
            "fire fox",
            "a;rm -rf ~",
            "$(id)",
            "a\nb",
            "a`b`",
        ] {
            assert!(!valid_app_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn jobs_run_in_order_one_per_application_and_end_as_they_exited() {
        let board = JobBoard::default();
        let (_, start) = board.enqueue(JobOp::Install, "firefox").expect("queued");
        assert!(start, "the first job starts the queue");
        assert_eq!(board.enqueue(JobOp::Remove, "firefox"), Err(Refused::Busy));
        assert_eq!(board.enqueue(JobOp::Install, "all"), Err(Refused::BadName));
        let (_, start) = board.enqueue(JobOp::Install, "gimp").expect("queued");
        assert!(!start, "a queue already running takes it");

        let (id, op, app, _) = board.start_next().expect("first");
        assert_eq!((op, app.as_str()), (JobOp::Install, "firefox"));
        board.feed(id, "##pa phase download\n\r###   40.0%");
        let update = board.take_update(id).expect("printed");
        assert!(update.contains(r#""phase":"download""#), "{update}");
        assert!(update.contains(r#""progress":40.0"#), "{update}");
        let ended = board.finish(id, Some(0)).expect("ended");
        assert_eq!(ended.state, JobState::Done);
        assert!(ended.action.contains(r#""state":"done""#));

        let (id, ..) = board.start_next().expect("second");
        board.feed(id, "error: no manifest\n");
        let ended = board.finish(id, Some(1)).expect("ended");
        assert_eq!(ended.state, JobState::Failed);
        assert_eq!(ended.reason, "error: no manifest");
        assert!(board.start_next().is_none(), "an empty queue is released");
        let (_, start) = board.enqueue(JobOp::Remove, "firefox").expect("queued");
        assert!(start, "and the next job starts it again");

        assert_eq!(board.snapshot().len(), 3);
        assert!(board.clear(Some(1)));
        assert_eq!(board.snapshot().len(), 2);
        assert!(board.clear(None), "every finished job");
        assert_eq!(board.snapshot().len(), 1, "a queued job is not cleared");
    }

    #[test]
    fn a_full_queue_refuses_another_job() {
        let board = JobBoard::default();
        for n in 0..PENDING_KEPT {
            board
                .enqueue(JobOp::Install, &format!("app{n}"))
                .expect("queued");
        }
        assert_eq!(
            board.enqueue(JobOp::Install, "one-more"),
            Err(Refused::Full)
        );
        let (id, ..) = board.start_next().expect("first");
        board.finish(id, Some(0));
        board
            .enqueue(JobOp::Install, "one-more")
            .expect("a finished job frees its place");
    }

    #[test]
    fn a_canceled_job_ends_canceled() {
        let board = JobBoard::default();
        board.enqueue(JobOp::Install, "firefox").expect("queued");
        board.enqueue(JobOp::Install, "gimp").expect("queued");
        let (id, ..) = board.start_next().expect("first");
        assert!(
            board.cancel(id).is_none(),
            "a running job ends when it exits"
        );
        let ended = board.finish(id, Some(-15)).expect("ended");
        assert_eq!(ended.state, JobState::Canceled);
        let gimp = board.snapshot()[1]["id"].as_u64().expect("id");
        let action = board.cancel(gimp).expect("a queued job ends at once");
        assert!(action.contains(r#""state":"canceled""#), "{action}");
        assert!(board.start_next().is_none(), "nothing is left to run");
    }

    #[test]
    fn a_state_carries_the_newest_consoles_within_its_bound() {
        let board = JobBoard::default();
        let line = format!("{}\n", "x".repeat(LINE_CHARS - 1));
        for app in ["old", "new"] {
            board.enqueue(JobOp::Install, app).expect("queued");
            let (id, ..) = board.start_next().expect("started");
            board.feed(id, &line.repeat(CONSOLE_LINES));
            board.finish(id, Some(0));
        }
        let jobs = board.snapshot();
        let bytes = |job: &Json| job["lines"].as_array().map_or(0, |l| l.len() * LINE_CHARS);
        assert_eq!(jobs[1]["app"], "new");
        assert_eq!(
            jobs[1]["lines"].as_array().map(Vec::len),
            Some(CONSOLE_LINES)
        );
        assert!(
            bytes(&jobs[0]) < bytes(&jobs[1]),
            "the older console is cut first"
        );
        assert!(bytes(&jobs[0]) + bytes(&jobs[1]) <= SNAPSHOT_BYTES);
        let mut console = Console::default();
        console.feed("done\n");
        console.close();
        assert_eq!(
            console.lines(),
            vec!["done".to_string()],
            "an ended output adds no empty line"
        );
    }

    #[test]
    fn finished_jobs_past_the_bound_are_dropped_oldest_first() {
        let board = JobBoard::default();
        for i in 0..FINISHED_KEPT + 3 {
            board
                .enqueue(JobOp::Install, &format!("app{i}"))
                .expect("queued");
            let (id, ..) = board.start_next().expect("started");
            board.finish(id, Some(0));
        }
        let kept = board.snapshot();
        assert_eq!(kept.len(), FINISHED_KEPT);
        assert_eq!(kept[0]["app"], "app3");
    }
}
