/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `cargo xtask suites`: the driver of the test suites in tests/ and crates/selkies-suites.
//!
//! Every suite is a program of its own that prints one `PASS`/`FAIL` line per check and exits
//! non-zero when one failed: a Playwright Test spec (`.spec.ts`, one test per selector), a Node
//! audit (`.ts`), a test target of `crates/selkies-suites` (`.rs`), or, for the tiers whose tool
//! is Python's, a Python program. [`table`] lists them, each with the tier that says what it
//! needs, the budget it is killed at, and the selectors that split it into cases.
//!
//! ```text
//! cargo xtask suites [--tier T]... [--match EXPR] [--shard K/N] [--junit FILE] [--list]
//! ```
//!
//! `--tier` keeps the cases of the tiers named (any number, or one argument joined by ` or `);
//! `--match` keeps those whose id an expression of words joined by `and`, `or`, `not` and
//! parentheses matches, each word a case-insensitive part of the id (`unit:test_i18n_keys`,
//! `e2e:test_codecs-ws-x11`); `--shard K/N` keeps the K-th of N shards, the cases dealt longest
//! budget first to the shard with the least budget so far, so the shards take about as long.
//!
//! Each case runs in a process group of its own, told its budget less a minute
//! (`SELKIES_SUITE_DEADLINE`) so a suite that stalls says where before the group is killed at
//! the budget, or as soon as the driver is interrupted. A case that loses a browser or its player is run once more, both runs printed.
//! The logs a case left in the work directory (`E2E_WORKDIR`) are kept under
//! `suite-logs/<case>/`. A case passes on a zero exit having made a check (a Rust target, having
//! run a test); a case that skips as a whole fails the run, as a dependency the runner was meant
//! to provide went missing; a check that skips is listed. `--junit` writes the report CI keeps.

mod expr;
mod table;

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

pub use table::SUITES;

/// The exit status a suite gives when it skips as a whole.
const SKIP_EXIT: i32 = 77;

/// One suite of the table.
pub struct Suite {
    /// Its path, relative to tests/.
    pub path: &'static str,
    pub tier: &'static str,
    /// Its budget, in seconds.
    pub timeout: u64,
    /// The cases it is split into; none runs it whole.
    pub selectors: &'static [&'static str],
    /// The name its cases and logs carry, where it is not the one its path gives.
    pub name: Option<&'static str>,
}

pub const TIERS: [&str; 6] = ["unit", "integration", "e2e", "perf", "soak", "image"];

/// What a suite's output says when the browser under it went away rather than the suite failing.
const STALLED: [&str; 5] = [
    "did not answer within",
    "stopped answering earlier",
    "Target page, context or browser has been closed",
    "Page crashed",
    "sat at HAVE_NOTHING on a live track",
];

/// One case: a suite and the selector naming its part.
pub struct Case {
    pub suite: &'static Suite,
    pub selector: Option<&'static str>,
}

impl Case {
    /// The suite's name: `e2e/test_codecs` for `e2e/codecs.spec.ts`, `integration/test_protocol`
    /// for the Rust target `protocol.rs`, `unit/test_fit_frame` for the audit
    /// `tools/fit_frame_audit.ts`, `soak/test_capture_api` for `soak/test_capture_api.py`, so a
    /// suite keeps its name in whichever language it is written.
    pub fn name(&self) -> String {
        if let Some(name) = self.suite.name {
            return name.to_owned();
        }
        let path = self.suite.path;
        let file = Path::new(path)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(path);
        let tier = self.suite.tier;
        if let Some(stem) = file.strip_suffix(".rs") {
            format!("{tier}/test_{stem}")
        } else if let Some(stem) = file.strip_suffix(".spec.ts") {
            format!("{tier}/test_{stem}")
        } else if let Some(stem) = file.strip_suffix(".ts") {
            format!(
                "{tier}/test_{}",
                stem.strip_suffix("_audit").unwrap_or(stem)
            )
        } else {
            path.strip_suffix(".py").unwrap_or(path).to_owned()
        }
    }

    /// The id a case is selected by: `e2e:test_codecs-ws-x11`.
    pub fn id(&self) -> String {
        let name = self.name().replace('/', ":");
        self.selector
            .map_or(name.clone(), |s| format!("{name}-{s}"))
    }

    /// The folder its logs are kept in: `e2e-test_codecs-ws-x11`.
    fn label(&self) -> String {
        let name = self.name().replace('/', "-");
        self.selector
            .map_or(name.clone(), |s| format!("{name}-{s}"))
    }

    /// The program that runs it, from tests/.
    fn command(&self, tests: &Path) -> Command {
        let path = self.suite.path;
        if path.ends_with(".rs") {
            let target = Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(path);
            // Every test of a target is ignored until a run names the target, as here: `cargo
            // test --workspace` runs on hosts with no display. A selector filters the target's
            // tests by name, which spells its hyphens as underscores.
            let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
            cmd.args([
                "test",
                "--locked",
                "--package",
                "selkies-suites",
                "--test",
                target,
                "--",
                "--include-ignored",
                // A passing test's `SKIP` lines are among what the run lists.
                "--show-output",
            ]);
            if let Some(selector) = self.selector {
                cmd.arg(selector.replace('-', "_"));
            }
            return cmd;
        }
        if path.ends_with(".spec.ts") {
            let mut cmd = Command::new("node");
            cmd.arg(tests.join("node_modules/@playwright/test/cli.js"))
                .args(["test", "--config"])
                .arg(tests.join("playwright.config.ts"))
                .arg(path);
            // A selector names one test of the suite, whole: the expression matches the title's
            // end, so `websockets` does not also run `websockets-wayland`.
            if let Some(selector) = self.selector {
                cmd.arg("--grep")
                    .arg(format!("(^| ){}$", regex::escape(selector)));
            }
            return cmd;
        }
        let mut cmd = Command::new(if path.ends_with(".ts") {
            "node"
        } else {
            "python3"
        });
        cmd.arg(tests.join(path));
        if let Some(selector) = self.selector {
            cmd.arg(selector);
        }
        cmd
    }
}

/// Every case of the table, in its order.
pub fn cases() -> Vec<Case> {
    SUITES
        .iter()
        .flat_map(|suite| {
            let selectors: Vec<Option<&'static str>> = if suite.selectors.is_empty() {
                vec![None]
            } else {
                suite.selectors.iter().map(|s| Some(*s)).collect()
            };
            selectors
                .into_iter()
                .map(move |selector| Case { suite, selector })
        })
        .collect()
}

/// The `k`-th (from 1) of `n` shards of `cases`: each dealt, longest budget first, to the shard
/// with the least budget so far.
pub fn shard(cases: Vec<Case>, k: usize, n: usize) -> Vec<Case> {
    let mut order: Vec<usize> = (0..cases.len()).collect();
    // Stable, so cases of one budget are dealt in the table's order.
    order.sort_by(|a, b| cases[*b].suite.timeout.cmp(&cases[*a].suite.timeout));
    let mut load = vec![0u64; n];
    let mut keep = vec![false; cases.len()];
    for i in order {
        let least = (0..n).min_by_key(|s| load[*s]).unwrap_or(0);
        load[least] += cases[i].suite.timeout;
        keep[i] = least == k - 1;
    }
    cases
        .into_iter()
        .zip(keep)
        .filter_map(|(c, k)| k.then_some(c))
        .collect()
}

/// How a case ended.
enum Verdict {
    Passed,
    Failed(String),
    Skipped(String),
}

struct Ran {
    status: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

/// Set by SIGINT and SIGTERM, which [`run`] answers by ending the case it is running.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn interrupted(_signal: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

/// Have an interrupt of the driver end the running case's process group before the driver
/// exits: the group is the case's own, so the terminal's signal never reaches it.
fn watch_interrupts() {
    let handler = interrupted as extern "C" fn(libc::c_int) as libc::sighandler_t;
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        unsafe { libc::signal(signal, handler) };
    }
}

/// Run `cmd` in a process group of its own, killing the group when it runs past `budget` or the
/// driver is interrupted, which exits the driver with 130.
fn run(mut cmd: Command, budget: Duration) -> Result<Ran, String> {
    use std::os::unix::process::CommandExt as _;
    cmd.process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("{cmd:?}: {e}"))?;
    let pid = child.id();
    let read = |stream: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(mut s) = stream {
                let _ = s.read_to_end(&mut text);
            }
            String::from_utf8_lossy(&text).into_owned()
        })
    };
    let out = read(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    );
    let err = read(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    );
    let deadline = Instant::now() + budget;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if INTERRUPTED.load(Ordering::Relaxed) => {
                kill_group(pid);
                let _ = child.wait();
                std::process::exit(130);
            }
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                kill_group(pid);
                break child.wait().ok().and_then(|s| s.code());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => return Err(e.to_string()),
        }
    };
    // What the group left running (a server a suite forked off) goes with it.
    kill_group(pid);
    Ok(Ran {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
        timed_out,
    })
}

fn kill_group(pid: u32) {
    let Ok(group) = libc::pid_t::try_from(pid) else {
        return;
    };
    // SAFETY: a signal to a process group, by the id of the child that leads it.
    unsafe { libc::killpg(group, libc::SIGKILL) };
}

/// Copy the logs a case left in `workdir` since `since` into `suite-logs/<label>/`. The
/// directory is shared, so only what this case wrote is taken: a log an earlier one left
/// untouched would read as this one's.
fn keep_logs(workdir: &Path, label: &str, since: SystemTime) {
    let dest = workdir.join("suite-logs").join(label);
    let Ok(entries) = std::fs::read_dir(workdir) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dest);
    for entry in entries.flatten() {
        let path = entry.path();
        let fresh = entry
            .metadata()
            .ok()
            .filter(|m| m.is_file())
            .and_then(|m| m.modified().ok())
            .is_some_and(|t| t >= since);
        if fresh && path.extension().is_some_and(|e| e == "log") {
            let _ = std::fs::copy(&path, dest.join(entry.file_name()));
        }
    }
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

fn tail_chars(text: &str, chars: usize) -> &str {
    let start = text
        .char_indices()
        .rev()
        .nth(chars.saturating_sub(1))
        .map_or(0, |(at, _)| at);
    &text[start..]
}

/// Run one case to its verdict, printing what it printed.
fn verdict(case: &Case, tests: &Path, workdir: &Path) -> (Verdict, String) {
    let path = case.suite.path;
    let budget = case.suite.timeout;
    let label = case.label();
    let attempt = || {
        let mut cmd = case.command(tests);
        cmd.current_dir(tests)
            // A minute short of the kill, so what a stalled suite says lands in the output kept.
            .env(
                "SELKIES_SUITE_DEADLINE",
                budget.saturating_sub(60).max(30).to_string(),
            );
        if path.ends_with(".ts") && std::env::var_os("NODE_COMPILE_CACHE").is_none() {
            // Node keeps the code it compiled for the next case.
            cmd.env("NODE_COMPILE_CACHE", workdir.join("node-compile-cache"));
        }
        run(cmd, Duration::from_secs(budget))
    };
    let started = SystemTime::now();
    let mut ran = match attempt() {
        Ok(ran) => ran,
        Err(e) => return (Verdict::Failed(e), String::new()),
    };
    let mut printed = String::new();
    if !ran.timed_out
        && ran.status != Some(0)
        && STALLED
            .iter()
            .any(|m| ran.stdout.contains(m) || ran.stderr.contains(m))
    {
        let _ = writeln!(
            printed,
            "{}{}note: {label} lost a browser or its player mid-run; running it once more",
            ran.stdout, ran.stderr
        );
        keep_logs(workdir, &format!("{label}-stalled"), started);
        ran = match attempt() {
            Ok(ran) => ran,
            Err(e) => return (Verdict::Failed(e), printed),
        };
    }
    keep_logs(workdir, &label, started);
    printed.push_str(&ran.stdout);
    printed.push_str(&ran.stderr);
    if ran.timed_out {
        // The checks the suite did finish are its record of where it stuck.
        let said = if ran.stdout.trim().is_empty() {
            tail_chars(&ran.stderr, 2000).to_owned()
        } else {
            tail(&ran.stdout, 20)
        };
        return (
            Verdict::Failed(format!(
                "{path} {} ran past {budget}s\n{said}",
                case.selector.unwrap_or("")
            )),
            printed,
        );
    }
    if ran.status == Some(SKIP_EXIT) {
        let reason = ran
            .stdout
            .lines()
            .find(|l| l.starts_with("SKIP"))
            .unwrap_or("subject not installed");
        return (Verdict::Skipped(format!("{path}: {reason}")), printed);
    }
    // libtest passes a filter that matches nothing, which is a selector naming no test of the
    // target, not a suite that passed.
    if path.ends_with(".rs") && ran.status == Some(0) && ran.stdout.contains("running 0 tests") {
        return (
            Verdict::Failed(format!(
                "{path} {} ran no test",
                case.selector.unwrap_or("")
            )),
            printed,
        );
    }
    // A suite that exits 0 having checked nothing has tested nothing.
    if path.ends_with(".ts")
        && !ran
            .stdout
            .lines()
            .any(|l| l.starts_with("PASS") || l.starts_with("FAIL"))
    {
        return (
            Verdict::Failed(format!(
                "{path} printed no check\n{}",
                tail_chars(&ran.stderr, 2000)
            )),
            printed,
        );
    }
    if ran.status != Some(0) {
        let failed: Vec<&str> = ran
            .stdout
            .lines()
            .filter(|l| l.starts_with("FAIL"))
            .take(20)
            .collect();
        // A suite that died rather than failing checks reports through stderr; a Rust one's panic
        // is in the test's own output, and stderr holds cargo's.
        let died = if path.ends_with(".rs") {
            tail_chars(&ran.stdout, 4000)
        } else if ran.stderr.is_empty() {
            tail_chars(&ran.stdout, 2000)
        } else {
            tail_chars(&ran.stderr, 2000)
        };
        let detail = if failed.is_empty() {
            died.to_owned()
        } else {
            failed.join("\n")
        };
        let status = ran
            .status
            .map_or("on a signal".to_owned(), |s| s.to_string());
        return (
            Verdict::Failed(format!(
                "{path} {} exited {status}\n{detail}",
                case.selector.unwrap_or("")
            )),
            printed,
        );
    }
    (Verdict::Passed, printed)
}

/// `text` with what XML cannot carry dropped and its markup escaped.
fn xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if c < ' ' => {}
            c => out.push(c),
        }
    }
    out
}

struct Options {
    tiers: Vec<String>,
    matching: Option<expr::Expr>,
    shard: Option<(usize, usize)>,
    junit: Option<PathBuf>,
    list: bool,
}

fn options(args: &[String]) -> Result<Options, String> {
    let mut o = Options {
        tiers: Vec::new(),
        matching: None,
        shard: None,
        junit: None,
        list: false,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} takes a value"));
        match arg.as_str() {
            "--tier" => {
                for tier in value()?
                    .split(" or ")
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                {
                    if !TIERS.contains(&tier) {
                        return Err(format!("no tier {tier}; one of {}", TIERS.join(", ")));
                    }
                    o.tiers.push(tier.to_owned());
                }
            }
            "--match" => {
                let text = value()?;
                o.matching = if text.trim().is_empty() {
                    None
                } else {
                    Some(expr::parse(text)?)
                };
            }
            "--shard" => {
                let spec = value()?;
                let (k, n) = spec.split_once('/').ok_or("--shard takes K/N")?;
                let (k, n): (usize, usize) = (
                    k.parse().map_err(|_| "--shard takes K/N")?,
                    n.parse().map_err(|_| "--shard takes K/N")?,
                );
                if n == 0 || k == 0 || k > n {
                    return Err(format!("no shard {spec}"));
                }
                o.shard = Some((k, n));
            }
            "--junit" => o.junit = Some(PathBuf::from(value()?)),
            "--list" => o.list = true,
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    Ok(o)
}

/// The cases `o` selects, in the table's order.
fn selected(o: &Options) -> Vec<Case> {
    let mut chosen: Vec<Case> = cases()
        .into_iter()
        .filter(|c| o.tiers.is_empty() || o.tiers.iter().any(|t| t == c.suite.tier))
        .filter(|c| {
            o.matching
                .as_ref()
                .is_none_or(|m| m.matches(&c.id(), c.suite.tier))
        })
        .collect();
    if let Some((k, n)) = o.shard {
        chosen = shard(chosen, k, n);
    }
    chosen
}

/// `cargo xtask suites`; whether every case selected passed.
pub fn main(args: &[String]) -> Result<bool, String> {
    let o = options(args)?;
    let chosen = selected(&o);
    if o.list {
        for case in &chosen {
            println!("{}\t{}s", case.id(), case.suite.timeout);
        }
        return Ok(true);
    }
    watch_interrupts();
    let tests = crate::repo().join("tests");
    let workdir = PathBuf::from(std::env::var("E2E_WORKDIR").unwrap_or_else(|_| {
        std::env::temp_dir()
            .join("selkies-tests")
            .to_string_lossy()
            .into_owned()
    }));
    let _ = std::fs::create_dir_all(&workdir);
    let mut report = String::new();
    let (mut failures, mut skips, mut unobserved) = (Vec::new(), Vec::new(), Vec::<String>::new());
    let started = Instant::now();
    for case in &chosen {
        let id = case.id();
        println!("=== {id}");
        let began = Instant::now();
        let (verdict, printed) = verdict(case, &tests, &workdir);
        print!("{printed}");
        let secs = began.elapsed().as_secs_f64();
        let _ = write!(
            report,
            "<testcase classname=\"suites\" name=\"{}\" time=\"{secs:.1}\">",
            xml(&id)
        );
        match &verdict {
            Verdict::Passed => println!("=== {id} PASSED in {secs:.0}s"),
            Verdict::Failed(why) => {
                println!("=== {id} FAILED in {secs:.0}s\n{why}");
                let _ = write!(
                    report,
                    "<failure message=\"{}\">{}</failure>",
                    xml(why.lines().next().unwrap_or("")),
                    xml(why)
                );
                failures.push(id.clone());
            }
            Verdict::Skipped(why) => {
                println!("=== {id} SKIPPED: {why}");
                let _ = write!(report, "<skipped message=\"{}\"/>", xml(why));
                skips.push(format!("{id}: {why}"));
            }
        }
        // A check a suite could not observe here is listed, not failed.
        unobserved.extend(
            printed
                .lines()
                .filter(|l| l.starts_with("SKIP  ["))
                .map(|l| format!("{id}: {}", tail_chars(l[6..].trim(), 160))),
        );
        let _ = writeln!(
            report,
            "<system-out>{}</system-out></testcase>",
            xml(&printed)
        );
    }
    if let Some(path) = &o.junit {
        let xml_out = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<testsuites><testsuite name=\"suites\" tests=\"{}\" failures=\"{}\" skipped=\"{}\" time=\"{:.1}\">\n{report}</testsuite></testsuites>\n",
            chosen.len(),
            failures.len(),
            skips.len(),
            started.elapsed().as_secs_f64()
        );
        std::fs::write(path, xml_out).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    println!(
        "\n{} passed, {} failed, {} skipped of {} cases",
        chosen.len() - failures.len() - skips.len(),
        failures.len(),
        skips.len(),
        chosen.len()
    );
    for line in &unobserved {
        println!("note: check not observed here -- {line}");
    }
    for id in &failures {
        println!("FAILED {id}");
    }
    // A skip means a dependency the runner was meant to provide went missing, and it would read
    // as green: it fails the run.
    for skip in &skips {
        println!("SKIPPED {skip}");
    }
    Ok(failures.is_empty() && skips.is_empty())
}
