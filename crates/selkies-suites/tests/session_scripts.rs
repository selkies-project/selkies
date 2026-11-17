/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The shell the repository ships stays parseable and lint-clean.
//!
//! The session scripts run only inside the container against a live compositor and X server, so
//! CI cannot exercise their behavior. It can still guard the failure that reaches a user as a
//! session that will not paint, or, for a Dockerfile, as a build that dies minutes in: a quoting
//! or syntax slip. Every service script is parsed with `bash -n`, so is the shell inside each
//! Dockerfile `RUN`, and every shell script the repository ships is
//! linted at shellcheck's lowest severity, which the tree passes. CI lints with the release
//! scripts/ci/install-shellcheck.sh pins, since which findings that severity reports differs
//! between releases.
//!
//! These need only bash and shellcheck, so they run with `cargo test --workspace`.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use selkies_suites::{Checks, on_path, repo, workdir};

/// The entries of `dir` that `pick` keeps, as `glob` lists them: no dot-names.
fn entries(dir: &Path, pick: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|listing| {
            listing
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| !name_of(p).starts_with('.') && pick(p))
                .collect()
        })
        .unwrap_or_default()
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Each service's directory under `root/services`.
fn service_dirs(root: &Path) -> Vec<PathBuf> {
    entries(&root.join("services"), |p| p.is_dir())
}

fn sorted(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort_by_key(|p| p.to_string_lossy().into_owned());
    paths
}

/// The session's scripts: the base container's runtime and the desktop image's additions.
fn session_scripts(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in roots {
        found.extend(entries(root, |p| {
            p.is_file() && name_of(p).ends_with(".sh")
        }));
        for service in service_dirs(root) {
            for name in ["run", "finish"] {
                let path = service.join(name);
                if path.exists() {
                    found.push(path);
                }
            }
            found.extend(entries(&service, |p| {
                p.is_file() && name_of(p).ends_with(".sh")
            }));
        }
    }
    sorted(found)
}

/// `bash -n` over a file or, with `input`, over text: its error where it does not parse.
fn parse(bash: &Path, file: Option<&Path>, input: &str) -> Result<(), String> {
    let mut command = Command::new(bash);
    command.arg("-n");
    if let Some(file) = file {
        command.arg(file);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(input.as_bytes())
        .map_err(|e| e.to_string())?;
    drop(stdin);
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr)
            .trim()
            .chars()
            .take(200)
            .collect())
    }
}

/// The shell body of each RUN in a Dockerfile's text, as the build executes it: Docker drops the
/// comment lines inside one and joins the continuations into a single command.
fn run_bodies(dockerfile: &str) -> Vec<String> {
    let mut bodies = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in dockerfile.lines() {
        if line.trim().starts_with('#') {
            continue;
        }
        match current.as_mut() {
            None => match line.strip_prefix("RUN ") {
                Some(rest) => current = Some(vec![rest]),
                None => continue,
            },
            Some(lines) => lines.push(line),
        }
        let last = current
            .as_ref()
            .and_then(|l| l.last())
            .copied()
            .unwrap_or("");
        if !last.trim_end().ends_with('\\') {
            let joined = current.take().unwrap_or_default().join("\n");
            bodies.push(joined.replace("\\\n", " "));
        }
    }
    bodies
}

/// `ARG`/`ENV` lines whose value is not double-quoted.
///
/// Docker strips the quotes, so they change no value, but an unquoted one silently truncates at
/// the first space and reads as a shell expansion where it is not one. Keeping every definition
/// quoted makes both impossible to introduce by copy.
fn unquoted_settings(dockerfile: &str) -> Vec<&str> {
    dockerfile
        .lines()
        .filter(|line| {
            let rest = line.trim_start();
            let Some(rest) = rest
                .strip_prefix("ARG")
                .or_else(|| rest.strip_prefix("ENV"))
            else {
                return false;
            };
            let name = rest.trim_start();
            if name.len() == rest.len() {
                return false;
            }
            let mut chars = name.char_indices();
            if !chars
                .next()
                .is_some_and(|(_, c)| c.is_ascii_alphabetic() || c == '_')
            {
                return false;
            }
            let end = chars
                .find(|&(_, c)| !(c.is_ascii_alphanumeric() || c == '_'))
                .map_or(name.len(), |(i, _)| i);
            name[end..]
                .strip_prefix('=')
                .is_some_and(|value| !value.starts_with('"'))
        })
        .collect()
}

/// The directories a walk of the tree leaves out. Cargo's target holds the shell scripts its
/// dependencies' C builds generate.
const SKIP_DIRS: [&str; 7] = [
    ".git",
    "node_modules",
    "dist",
    "target",
    "__pycache__",
    ".venv",
    "venv",
];

/// Every Dockerfile in the tree, as a recursive glob finds them: no dot-directories, nothing under
/// node_modules.
fn dockerfiles(dir: &Path, found: &mut Vec<PathBuf>) {
    for path in entries(dir, |_| true) {
        if name_of(&path) == "node_modules" {
            continue;
        }
        if path.is_dir() {
            dockerfiles(&path, found);
        } else if name_of(&path) == "Dockerfile" {
            found.push(path);
        }
    }
}

/// Whether a shebang line names a shell: `sh`, `bash`, `dash`, `ksh` or `zsh` as a whole word.
fn names_shell(line: &[u8]) -> bool {
    let word = |at: usize| {
        line.get(at)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
    };
    (0..line.len().saturating_sub(1)).any(|at| {
        if &line[at..at + 2] != b"sh" || word(at + 2) {
            return false;
        }
        let starts_word = |start: usize| start == 0 || !word(start - 1);
        starts_word(at)
            || (at >= 1 && matches!(line[at - 1], b'k' | b'z') && starts_word(at - 1))
            || (at >= 2 && matches!(&line[at - 2..at], b"ba" | b"da") && starts_word(at - 2))
    })
}

/// Every shell script under `dir`, by extension or by shebang.
///
/// Walked: dot-directories hold the devcontainer's scripts, and the scripts a container runs by
/// name (service `run`/`finish`, `selkies-proot`) carry no extension at all. A link to a
/// directory is not followed.
fn shell_scripts(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(listing) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in listing.filter_map(Result::ok) {
        let path = entry.path();
        let name = name_of(&path);
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                shell_scripts(&path, found);
            }
            continue;
        }
        if kind.is_symlink() && path.is_dir() {
            continue;
        }
        if name.ends_with(".sh") {
            found.push(path);
            continue;
        }
        if path.extension().is_some() || kind.is_symlink() {
            continue;
        }
        let mut head = Vec::new();
        if std::fs::File::open(&path)
            .and_then(|f| f.take(128).read_to_end(&mut head))
            .is_err()
        {
            continue;
        }
        let first = head.split(|&b| b == b'\n').next().unwrap_or_default();
        if first.starts_with(b"#!") && names_shell(first) {
            found.push(path);
        }
    }
}

#[test]
fn the_shipped_shell_parses_and_lints() {
    let mut checks = Checks::new("desktop-shell");
    let root = repo().canonicalize().expect("the repository");
    let rel = |path: &Path| {
        path.strip_prefix(&root)
            .unwrap_or(path)
            .display()
            .to_string()
    };
    let bash = on_path("bash").expect("bash, which parses the session scripts");
    // The session runtime lives in the base container; the desktop image adds the desktop on top
    // of it, so both directories carry scripts a session depends on.
    let base = root.join("addons/base");
    let roots = [base.clone(), root.join("addons/desktop")];
    let scripts = session_scripts(&roots);
    if scripts.is_empty() {
        checks.check(
            "scripts found",
            false,
            "none matched under addons/base or addons/desktop",
        );
        checks.finish("");
        return;
    }
    for path in &scripts {
        let parsed = parse(&bash, Some(path), "");
        checks.check(
            &format!("parse {}", rel(path)),
            parsed.is_ok(),
            parsed.err().unwrap_or_default(),
        );
    }

    let mut found = Vec::new();
    dockerfiles(&root, &mut found);
    let found = sorted(found);
    checks.check("Dockerfiles found", !found.is_empty(), found.len());
    for dockerfile in &found {
        let text = std::fs::read_to_string(dockerfile).unwrap_or_default();
        for (i, body) in run_bodies(&text).iter().enumerate() {
            let parsed = parse(&bash, None, body);
            checks.check(
                &format!("parse {} RUN #{}", rel(dockerfile), i + 1),
                parsed.is_ok(),
                parsed.err().unwrap_or_default(),
            );
        }
        let loose = unquoted_settings(&text);
        checks.check(
            &format!("{} quotes every ARG/ENV value", rel(dockerfile)),
            loose.is_empty(),
            loose.join("; "),
        );
    }

    // The services' helpers are shell: none ships an interpreter's script beside it.
    let python: Vec<String> = roots
        .iter()
        .flat_map(|r| service_dirs(r))
        .flat_map(|service| entries(&service, |p| name_of(p).ends_with(".py")))
        .map(|p| rel(&p))
        .collect();
    checks.check(
        "no service ships a Python helper",
        python.is_empty(),
        python.join(", "),
    );

    // An absence invisible until a desktop is in front of someone: a raised latency must reach
    // the daemons.
    let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
    let entrypoint = read(&base.join("container-entrypoint.sh"));
    for service in ["pipewire", "pipewire-pulse", "wireplumber"] {
        let path = base.join("services").join(service).join("run");
        checks.check(
            &format!("{service} takes the audio latency an operator set"),
            read(&path).contains("${PIPEWIRE_LATENCY:-"),
            rel(&path),
        );
    }

    // The interposers answer for /dev/video0 and /dev/input in the session's applications;
    // preloaded into the backend they would hide the real device nodes capture and gamepads need,
    // and their process-wide hooks block the asyncio loop. Run rather than grepped: what matters
    // is the value the backend is exec'd with.
    let preloaded: String = entrypoint
        .split_once("LD_PRELOAD=")
        .map(|(_, rest)| rest.chars().take(250).collect())
        .unwrap_or_default();
    checks.check(
        "the session preloads both interposers",
        entrypoint.contains("SELKIES_WEBCAM_INTERPOSER")
            && ["${SELKIES_INTERPOSER}", "${SELKIES_WEBCAM_INTERPOSER}"]
                .iter()
                .all(|v| preloaded.contains(v)),
        "container-entrypoint.sh",
    );

    let tmp = workdir().join(format!("session-scripts-{}", std::process::id()));
    let stub_bin = tmp.join("bin");
    std::fs::create_dir_all(&stub_bin).expect("the stub directory");
    selkies_suites::script(
        &stub_bin.join("selkies"),
        "#!/bin/sh\nprintf %s \"${LD_PRELOAD-unset}\"\n",
    );
    let shims = [
        ("SELKIES_INTERPOSER", "/usr/lib/selkies_input_interposer.so"),
        ("FAKE_UDEV_LIB", "/opt/lib/libudev.so.1.0.0-fake"),
        (
            "SELKIES_WEBCAM_INTERPOSER",
            "/usr/lib/selkies_v4l2_interposer.so",
        ),
    ];
    let preload = [shims[0].1, "/opt/operator.so", shims[1].1, shims[2].1].join(":");
    let ran = Command::new(&bash)
        .arg(base.join("selkies-entrypoint.sh"))
        .env_clear()
        .env("PATH", selkies_suites::path_with(&stub_bin))
        .env("XDG_RUNTIME_DIR", &tmp)
        .env("SELKIES_WAYLAND", "true")
        .env("LD_PRELOAD", &preload)
        .envs(shims)
        .output();
    let printed = ran
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default();
    checks.check(
        "the backend runs with no Selkies interposer preloaded",
        printed == "/opt/operator.so",
        format!("{printed:?}"),
    );
    let _ = std::fs::remove_dir_all(&tmp);

    // Xft resources reach a toolkit only at start, so on X11 the DPI ladder's reload signal needs
    // an XSETTINGS manager to signal, or running apps keep their density.
    let ladder = read(&root.join("crates/selkies/src/display/dpi.rs"));
    let xsettingsd = base.join("services/xsettingsd/run");
    checks.check(
        "the DPI ladder's XSETTINGS manager is a service",
        !ladder.contains("xsettingsd") || xsettingsd.is_file(),
        rel(&xsettingsd),
    );
    // The entrypoint holds services down by name, and a name no service carries is a toggle that
    // silently does nothing: the framebuffer would keep starting under Wayland, or the nested
    // compositor on X11.
    let services: BTreeSet<String> = roots
        .iter()
        .flat_map(|r| service_dirs(r))
        .filter(|service| service.join("run").exists())
        .map(|service| name_of(&service))
        .collect();
    let held: BTreeSet<&str> = entrypoint
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("drop_service "))
        .map(str::trim_end)
        .filter(|name| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        .collect();
    let listed = services.iter().cloned().collect::<Vec<_>>().join(", ");
    for name in held {
        checks.check(
            &format!("the entrypoint holds down a service that exists: {name}"),
            services.contains(name),
            &listed,
        );
    }

    // Every shell script the repository ships: packaging, CI and tooling scripts run unattended,
    // where a quoting slip is the same failure with nobody watching.
    let mut all = Vec::new();
    shell_scripts(&root, &mut all);
    let all = sorted(all);
    if let Some(shellcheck) = on_path("shellcheck") {
        checks.check(
            "shell scripts found to lint",
            all.len() >= scripts.len(),
            all.len(),
        );
        for path in &all {
            // The tree passes the lowest severity, so anything new is this run's. The scripts'
            // `shellcheck source=` directives are repo-relative, hence the path.
            let linted = Command::new(&shellcheck)
                .arg("-x")
                .arg(format!("--source-path={}", root.display()))
                .arg("--severity=style")
                .arg(path)
                .output();
            let (ok, found) = match linted {
                Ok(out) => (
                    out.status.success(),
                    String::from_utf8_lossy(&out.stdout).trim().to_owned(),
                ),
                Err(e) => (false, e.to_string()),
            };
            checks.check(&format!("lint {}", rel(path)), ok, found);
        }
    } else {
        checks.check(
            "shellcheck lints the shipped shell",
            false,
            "shellcheck is not installed",
        );
    }
    checks.finish("");
}
