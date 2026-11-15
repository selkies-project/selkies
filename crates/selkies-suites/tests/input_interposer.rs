/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The input interposer's hooks (addons/input-interposer) under the conditions a host process
//! puts them in, built off the tree with the C compiler and run under `LD_PRELOAD`.
//!
//! - Hooked calls a library makes from its own constructor, before the interposer's has run,
//!   succeed (`tests/tools/interposer_early_calls.c`).
//! - The hooks survive a signal handler that calls them. The interposer hooks read, ioctl, close
//!   and the stat family for every fd in a process and looks each fd up in its handle tables under
//!   a lock; a signal handler that reads a pipe (uvloop's, an SDL game's) runs on whichever thread
//!   the signal lands on, which may be one inside that lookup, so the lookup has to be re-enterable
//!   from the handler. `tests/tools/interposer_signal_stress.c` hammers the hooked calls from
//!   worker threads while a timer signal's handler calls them too, three times over.
//!
//! They need only a C compiler, which building the workspace needs anyway, so they run with
//! `cargo test --workspace`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use selkies_suites::server::run_within;
use selkies_suites::{Checks, repo, workdir};

/// A scratch directory of this test's own under the work directory.
fn scratch(name: &str) -> PathBuf {
    let dir = workdir().join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch directory");
    dir
}

/// Compiles with the C compiler, failing the test with the compiler's errors.
fn cc(args: &[&str]) {
    let ran = run_within(Command::new("cc").args(args), Duration::from_secs(120))
        .unwrap_or_else(|e| panic!("cc {args:?}: {e}"));
    assert!(ran.status.success(), "cc {args:?} failed: {}", ran.stderr);
}

/// The interposer, built into `dir`.
fn interposer(dir: &Path) -> PathBuf {
    let lib = dir.join("selkies_input_interposer.so");
    let source = repo().join("addons/input-interposer/input_interposer.c");
    cc(&[
        "-shared",
        "-fPIC",
        "-o",
        &lib.to_string_lossy(),
        &source.to_string_lossy(),
        "-ldl",
        "-pthread",
    ]);
    lib
}

/// `program` under the interposer's preload, its socket directory `dir`, within `limit`.
fn preloaded(
    program: &Path,
    lib: Option<&Path>,
    dir: &Path,
    limit: Duration,
) -> Result<(bool, String), String> {
    let mut command = Command::new(program);
    command.env_remove("LD_PRELOAD");
    if let Some(lib) = lib {
        command
            .env("LD_PRELOAD", lib)
            .env("SELKIES_JS_SOCKET_PATH", dir);
    }
    run_within(&mut command, limit)
        .map(|ran| (ran.status.success(), ran.stderr.trim().replace('\n', "; ")))
}

#[test]
fn hooked_calls_made_before_the_constructor_succeed() {
    let dir = scratch("input-interposer-early-calls");
    let lib = interposer(&dir);
    let early = repo().join("tests/tools/interposer_early_calls.c");
    let early_lib = dir.join("libearly_calls.so");
    cc(&[
        "-shared",
        "-fPIC",
        "-DEARLY_CALLS_LIB",
        "-o",
        &early_lib.to_string_lossy(),
        &early.to_string_lossy(),
    ]);
    let tool = dir.join("interposer_early_calls");
    let rpath = format!("-Wl,-rpath,{}", dir.display());
    cc(&[
        "-o",
        &tool.to_string_lossy(),
        &early.to_string_lossy(),
        "-L",
        &dir.to_string_lossy(),
        "-l:libearly_calls.so",
        &rpath,
    ]);

    let mut checks = Checks::new("input-interposer-early-calls");
    let plain = preloaded(&tool, None, &dir, Duration::from_secs(20));
    checks.check(
        "the calls succeed without the preload",
        plain.as_ref().is_ok_and(|r| r.0),
        format!("{plain:?}"),
    );
    let hooked = preloaded(&tool, Some(&lib), &dir, Duration::from_secs(20));
    checks.check(
        "hooked calls made before the constructor succeed",
        hooked.as_ref().is_ok_and(|r| r.0),
        format!("{hooked:?}"),
    );
    checks.finish("");
}

#[test]
fn the_hooks_survive_a_signal_handler_that_calls_them() {
    let dir = scratch("input-interposer-signals");
    let lib = interposer(&dir);
    let tool = dir.join("interposer_signal_stress");
    let stress = repo().join("tests/tools/interposer_signal_stress.c");
    cc(&[
        "-O1",
        "-o",
        &tool.to_string_lossy(),
        &stress.to_string_lossy(),
        "-pthread",
    ]);

    let mut checks = Checks::new("input-interposer-signals");
    for attempt in 1..=3 {
        // Bounded well above the tool's own three seconds: a deadlock runs into the bound.
        let storm = preloaded(&tool, Some(&lib), &dir, Duration::from_secs(20));
        let detail = match &storm {
            Err(_) => "deadlocked".to_owned(),
            Ok((ok, stderr)) => {
                if *ok {
                    String::new()
                } else {
                    format!("failed: {stderr}")
                }
            }
        };
        checks.check(
            &format!("the hooks survive a signal handler that calls them (run {attempt})"),
            storm.as_ref().is_ok_and(|r| r.0),
            detail,
        );
    }
    checks.finish("");
}
