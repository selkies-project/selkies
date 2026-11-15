/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The example container's entrypoint (addons/base/container-entrypoint.sh), run by bash
//! against stand-ins for what it asks, so what is checked is the shipped script rather than a
//! restatement of it.
//!
//! The address the embedded TURN server hands out. A TURN server is only useful at the address
//! a browser outside the container can reach it on, and nothing inside the container can see
//! that address: it has to be asked of a public resolver. Every way that can go wrong ends in a
//! session that negotiates and then carries no media: a resolver diagnostic mistaken for an
//! answer, an IPv6 literal left unbracketed for a URL, or no answer at all taken as an empty
//! address rather than the container's own. The address helpers run against stubbed lookups,
//! and so does the choice of whether the container starts its own TURN server.
//!
//! Which backend the entrypoint starts is held by the server's own tests
//! (`crates/selkies/src/session_screens/entrypoint_tests.rs`). These need only bash, so they run
//! with `cargo test --workspace`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use selkies_suites::server::run_within;
use selkies_suites::{Checks, repo, workdir};

/// The entrypoint's text.
fn entrypoint() -> String {
    std::fs::read_to_string(repo().join("addons/base/container-entrypoint.sh"))
        .expect("the entrypoint")
}

/// A scratch directory of its own for one run.
fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    let dir = workdir().join(format!(
        "entrypoint-{tag}-{}-{}",
        std::process::id(),
        RUNS.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).expect("the scratch directory");
    dir
}

/// Writes an executable shell script.
fn script(path: &Path, body: &str) {
    std::fs::write(path, body).expect("a script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("its mode");
}

/// A PATH with `dir` in front of this one.
fn path_with(dir: &Path) -> String {
    format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// What bash prints running `file` with `env` over a cleared environment of the variables named.
fn bash(file: &Path, env: &[(&str, String)], cleared: &[&str]) -> (String, String, bool) {
    let mut command = Command::new("bash");
    command.arg(file);
    for name in cleared {
        command.env_remove(name);
    }
    for (name, value) in env {
        command.env(name, value);
    }
    match run_within(&mut command, Duration::from_secs(120)) {
        Ok(ran) => (ran.stdout, ran.stderr, ran.status.success()),
        Err(e) => (String::new(), e, false),
    }
}

/// The address helpers as the entrypoint defines them: from `public_address` up to the export
/// after them, so the value's quoting is not part of what is pinned.
fn address_helpers() -> String {
    let body = entrypoint();
    let (begin, end) = ("public_address() {", "export SELKIES_ENABLE_INTERNAL_TURN");
    let start = body
        .find(begin)
        .expect("the entrypoint defines public_address");
    let stop = body
        .find(end)
        .expect("the entrypoint exports SELKIES_ENABLE_INTERNAL_TURN");
    body[start..stop].to_owned()
}

/// One helper's output with the lookups it depends on stubbed; a stub that is not given answers
/// nothing.
fn ask(lib: &str, call: &str, dig: &str, hostname: &str, getent: &str) -> String {
    let dir = scratch("turn");
    for (name, body) in [("dig", dig), ("hostname", hostname), ("getent", getent)] {
        script(&dir.join("bin").join(name), &format!("#!/bin/sh\n{body}\n"));
    }
    let file = dir.join("lib.sh");
    std::fs::write(&file, format!("{lib}\n{call}\n")).expect("the helper script");
    bash(&file, &[("PATH", path_with(&dir.join("bin")))], &[])
        .0
        .trim()
        .to_owned()
}

const NONE: &str = "exit 1";

/// The TURN variables, each set explicitly so a stray one where the tests run decides nothing.
const TURN_VARS: [&str; 6] = [
    "SELKIES_TURN_REST_URI",
    "SELKIES_TURN_USERNAME",
    "SELKIES_TURN_PASSWORD",
    "SELKIES_TURN_SHARED_SECRET",
    "SELKIES_TURN_HOST",
    "SELKIES_TURN_PORT",
];

/// Whether the entrypoint would start its own TURN server with `set`.
fn internal_turn(lib: &str, set: &[(&str, &str)]) -> bool {
    let dir = scratch("turn-choice");
    let setup: String = TURN_VARS
        .iter()
        .map(|k| {
            format!(
                "export {k}=\"{}\"\n",
                set.iter().find(|(n, _)| n == k).map_or("", |(_, v)| *v)
            )
        })
        .collect();
    let file = dir.join("lib.sh");
    std::fs::write(&file, format!("{setup}{lib}\nif ! external_turn_configured; then echo internal; else echo external; fi\n"))
        .expect("the helper script");
    bash(&file, &[], &[]).0.trim() == "internal"
}

#[test]
fn the_turn_server_hands_out_a_reachable_address() {
    let lib = address_helpers();
    let mut checks = Checks::new("turn-address");
    let v4 = r#"case "$1" in -4) echo '"203.0.113.7"';; *) exit 1;; esac"#;
    let v6 = r#"case "$1" in -6) echo '"2001:db8::5"';; *) exit 1;; esac"#;
    let local = r#"echo "10.1.2.3 fd00::1""#;
    checks.check(
        "an IPv4 answer is used as given",
        ask(&lib, "public_address", v4, local, NONE) == "203.0.113.7",
        "",
    );
    // Bracketed because it goes into a URL, where a bare IPv6 literal's colons are
    // indistinguishable from the port separator.
    checks.check(
        "an IPv6 answer is bracketed",
        ask(&lib, "public_address", v6, local, NONE) == "[2001:db8::5]",
        "",
    );
    checks.check(
        "IPv4 is preferred when both answer",
        ask(
            &lib,
            "public_address",
            r#"echo '"203.0.113.7"'"#,
            local,
            NONE,
        ) == "203.0.113.7",
        "",
    );
    // A resolver that cannot answer still prints, and its diagnostic is not an address.
    checks.check(
        "a resolver diagnostic is not mistaken for an answer",
        ask(
            &lib,
            "public_address",
            r#"echo ";; connection timed out""#,
            local,
            NONE,
        ) == "10.1.2.3",
        "",
    );
    checks.check(
        "no resolver falls back to this container's address",
        ask(&lib, "public_address", NONE, local, NONE) == "10.1.2.3",
        "",
    );
    // Never empty: an empty external address makes coTURN advertise nothing at all.
    checks.check(
        "with nothing to go on, a routable address is still returned",
        ask(&lib, "public_address", NONE, NONE, NONE) == "127.0.0.1",
        "",
    );
    checks.check(
        "a hostname is resolved for coTURN, which binds addresses",
        ask(
            &lib,
            r#"resolved_address "turn.example.com""#,
            NONE,
            NONE,
            r#"echo "198.51.100.9 STREAM turn.example.com""#,
        ) == "198.51.100.9",
        "",
    );
    checks.check("an IPv6-only host resolves bracketed",
                 ask(&lib, r#"resolved_address "turn.example.com""#, NONE, NONE,
                     r#"case "$1" in ahostsv6) echo "2001:db8::9 STREAM turn.example.com";; *) exit 1;; esac"#) == "[2001:db8::9]", "");
    // public_address may have bracketed it already; getent takes the literal.
    checks.check(
        "an already-bracketed literal is unwrapped before lookup",
        ask(
            &lib,
            r#"resolved_address "[2001:db8::5]""#,
            NONE,
            NONE,
            r#"case "$1" in ahostsv6) echo "$2 STREAM $2";; *) exit 1;; esac"#,
        ) == "[2001:db8::5]",
        "",
    );

    // Whether the container starts its own TURN server. Getting this wrong is silent either way:
    // a needless coTURN, or a WebRTC session with no relay when the one configured cannot be used.
    checks.check(
        "nothing configured runs the internal server",
        internal_turn(&lib, &[]),
        "",
    );
    checks.check(
        "a REST service is enough on its own",
        !internal_turn(
            &lib,
            &[("SELKIES_TURN_REST_URI", "https://turn.example.com/creds")],
        ),
        "",
    );
    checks.check(
        "host, port, and a password are enough",
        !internal_turn(
            &lib,
            &[
                ("SELKIES_TURN_HOST", "turn.example.com"),
                ("SELKIES_TURN_PORT", "3478"),
                ("SELKIES_TURN_USERNAME", "u"),
                ("SELKIES_TURN_PASSWORD", "p"),
            ],
        ),
        "",
    );
    checks.check(
        "host, port, and a shared secret are enough",
        !internal_turn(
            &lib,
            &[
                ("SELKIES_TURN_HOST", "turn.example.com"),
                ("SELKIES_TURN_PORT", "3478"),
                ("SELKIES_TURN_SHARED_SECRET", "s"),
            ],
        ),
        "",
    );
    checks.check(
        "credentials without a host are not enough",
        internal_turn(
            &lib,
            &[
                ("SELKIES_TURN_USERNAME", "u"),
                ("SELKIES_TURN_PASSWORD", "p"),
            ],
        ),
        "",
    );
    checks.check(
        "a host without a port is not enough",
        internal_turn(
            &lib,
            &[
                ("SELKIES_TURN_HOST", "turn.example.com"),
                ("SELKIES_TURN_SHARED_SECRET", "s"),
            ],
        ),
        "",
    );
    checks.check(
        "a username without its password is not enough",
        internal_turn(
            &lib,
            &[
                ("SELKIES_TURN_HOST", "turn.example.com"),
                ("SELKIES_TURN_PORT", "3478"),
                ("SELKIES_TURN_USERNAME", "u"),
            ],
        ),
        "",
    );
    checks.finish("");
}
