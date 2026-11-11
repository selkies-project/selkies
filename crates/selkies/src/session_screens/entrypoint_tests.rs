/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Which backend the example container starts, given what the GPU probe found
//! (`test_wayland_gpu_fallback.py`).
//!
//! The entrypoint decides, and it has to decide before anything derived from the backend: the
//! display, the session type, and the toolkit defaults all follow it, so a late switch yields a
//! half-Wayland container. This runs the real entrypoint against a stand-in probe and reads the
//! environment it hands to the services, rather than restating its logic here.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use super::testing::{Scratch, script};

/// Everything the services read is in the environment file by this line, so the copy under test
/// stops here and never hands over to s6 or touches `/etc/service`.
const STOP_AT: &str = "# Derive the service set from the environment toggles";

/// What `selkies gpu-probe` prints about the session's GPU.
fn facts(accelerated: bool, node: &str) -> String {
    let driver = "nvidia";
    [
        format!("SELKIES_GPU_RENDER_NODE={node}"),
        format!("SELKIES_GPU_DRIVER={driver}"),
        "SELKIES_GPU_PRESENT=true".to_string(),
        format!("SELKIES_GPU_ACCELERATED={accelerated}"),
        "SELKIES_GPU_GL_VENDOR=nvidia".to_string(),
        "SELKIES_GPU_EGL_X11=mesa".to_string(),
        "SELKIES_GPU_MESA_DRIVER=zink".to_string(),
        "SELKIES_GPU_VULKAN_PRESENTS=true".to_string(),
    ]
    .join("\n")
}

fn good() -> String {
    facts(true, "/dev/dri/renderD128")
}

fn software() -> String {
    facts(false, "")
}

/// A variable a Selkies session sets for itself, which the suite must not inherit.
fn session_setting(name: &str) -> bool {
    [
        "SELKIES_",
        "PIXELFLUX_",
        "GALLIUM_",
        "MESA_",
        "LIBGL_",
        "__GLX_",
        "__EGL_",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
        || name.ends_with("DISABLE_ZINK")
}

/// What one run left behind: the exported environment, and each question the probe was asked.
struct Run {
    exported: HashMap<String, String>,
    probe_calls: Vec<String>,
}

impl Run {
    fn get(&self, name: &str) -> Option<&str> {
        self.exported.get(name).map(String::as_str)
    }
}

/// Run the entrypoint's environment phase against a stubbed probe.
fn run(probe_stdout: &str, probe_rc: i32, env: &[(&str, &str)]) -> Run {
    let tmp = Scratch::new("entrypoint");
    let stubs = tmp.path("bin");
    std::fs::create_dir(&stubs).expect("stubs");
    let emit = if probe_stdout.is_empty() {
        String::new()
    } else {
        let quoted = probe_stdout.replace('\'', "'\\''");
        format!("printf '%s\\n' '{quoted}'\n")
    };
    script(
        &stubs.join("selkies"),
        &format!(
            "[ \"$1\" = gpu-probe ] || exit 99\nshift\necho \"$@\" >> {root}/probe-calls\n{emit}exit {probe_rc}",
            root = tmp.text()
        ),
    );
    for name in ["sudo-root", "nvidia-smi", "dig"] {
        script(&stubs.join(name), "exit 0");
    }
    let entrypoint =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../addons/base/container-entrypoint.sh");
    let body = std::fs::read_to_string(&entrypoint).expect("the entrypoint");
    let (head, _) = body
        .split_once(STOP_AT)
        .expect("the entrypoint has the structure this suite relies on");
    let copy = tmp.path("entrypoint.sh");
    std::fs::write(&copy, head).expect("the copy");
    let runtime = tmp.path("runtime");
    let mut command = Command::new("bash");
    command.arg(&copy);
    for (name, _) in std::env::vars() {
        if session_setting(&name)
            || [
                "DISPLAY",
                "XDG_SESSION_TYPE",
                "GDK_BACKEND",
                "MOZ_ENABLE_WAYLAND",
            ]
            .contains(&name.as_str())
        {
            command.env_remove(name);
        }
    }
    command
        .envs(env.iter().copied())
        .env("XDG_RUNTIME_DIR", &runtime)
        .env(
            "PATH",
            format!(
                "{}:{}",
                stubs.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
    command.output().expect("bash runs the entrypoint");
    let mut exported = HashMap::new();
    if let Ok(text) = std::fs::read_to_string(runtime.join("container-env")) {
        for line in text.lines() {
            if let Some((name, value)) =
                line.strip_prefix("export ").and_then(|l| l.split_once('='))
            {
                exported.insert(
                    name.to_string(),
                    value.trim().trim_matches('\'').to_string(),
                );
            }
        }
    }
    let probe_calls = std::fs::read_to_string(tmp.path("probe-calls"))
        .map(|text| text.lines().map(|l| l.trim().to_string()).collect())
        .unwrap_or_default();
    Run {
        exported,
        probe_calls,
    }
}

#[test]
fn a_wayland_verdict_keeps_wayland() {
    let env = run(&good(), 0, &[("SELKIES_WAYLAND", "true")]);
    assert_eq!(env.get("SELKIES_WAYLAND"), Some("true"));
    assert_eq!(
        env.get("DISPLAY"),
        Some(":0"),
        "Wayland takes the compositor's display"
    );
    assert_eq!(
        env.get("GALLIUM_DRIVER"),
        Some("zink"),
        "an accelerated Wayland session keeps hardware GL"
    );
    assert_eq!(
        env.probe_calls,
        [""],
        "a Wayland session asks the same one question"
    );
}

#[test]
fn an_x11_verdict_switches_the_backend_first() {
    let env = run(&software(), 0, &[("SELKIES_WAYLAND", "true")]);
    assert_eq!(env.get("SELKIES_WAYLAND"), Some("false"));
    assert_eq!(
        env.get("DISPLAY"),
        Some(":20"),
        "the fallback takes the X11 display"
    );
    assert_ne!(
        env.get("XDG_SESSION_TYPE"),
        Some("wayland"),
        "the fallback drops the Wayland session type"
    );
    assert!(
        env.get("MOZ_ENABLE_WAYLAND").is_none()
            && !env.get("GDK_BACKEND").unwrap_or("").starts_with("wayland"),
        "the fallback leaves toolkits on X11"
    );
    assert_eq!(
        env.get("GALLIUM_DRIVER"),
        Some("zink"),
        "the X11 fallback keeps hardware GL for the session"
    );
}

#[test]
fn a_software_verdict_keeps_wayland_on_software_gl() {
    let asked = [
        ("SELKIES_WAYLAND", "true"),
        ("SELKIES_WAYLAND_X11_FALLBACK", "false"),
    ];
    let env = run(&software(), 0, &asked);
    assert_eq!(env.get("SELKIES_WAYLAND"), Some("true"));
    assert!(
        env.get("GALLIUM_DRIVER").is_none() && env.get("MESA_LOADER_DRIVER_OVERRIDE").is_none(),
        "a software Wayland session gets software GL"
    );
    assert_eq!(
        env.get("SELKIES_WAYLAND_RENDERER"),
        Some("pixman"),
        "and its capture compositor renders in software"
    );
    let env = run(
        &software(),
        0,
        &[
            ("SELKIES_WAYLAND", "true"),
            ("SELKIES_WAYLAND_X11_FALLBACK", "false"),
            ("SELKIES_WAYLAND_RENDERER", "vulkan"),
        ],
    );
    assert_eq!(
        env.get("SELKIES_WAYLAND_RENDERER"),
        Some("vulkan"),
        "an operator's own renderer stands"
    );
}

#[test]
fn a_silent_probe_leaves_the_backend_alone() {
    let env = run("", 1, &[("SELKIES_WAYLAND", "true")]);
    assert_eq!(env.get("SELKIES_WAYLAND"), Some("true"));
}

#[test]
fn x11_asks_about_the_gpu_too() {
    let env = run(&good(), 0, &[("SELKIES_WAYLAND", "false")]);
    assert_eq!(env.probe_calls, [""], "its GL stack follows it");
    assert_eq!(
        env.get("SELKIES_WAYLAND"),
        Some("false"),
        "and X11 stays X11"
    );
}

#[test]
fn the_legacy_toggle_is_judged_too() {
    let env = run(&software(), 0, &[("PIXELFLUX_WAYLAND", "true")]);
    assert_eq!(env.get("SELKIES_WAYLAND"), Some("false"));
    let env = run(&good(), 0, &[("PIXELFLUX_WAYLAND", "TRUE")]);
    assert_eq!(
        env.get("SELKIES_WAYLAND"),
        Some("true"),
        "the legacy alias is read case-insensitively too"
    );
}

#[test]
fn every_spelling_canonicalizes_to_one_value() {
    for spelling in ["True", "TRUE", "1", " true "] {
        let env = run(&good(), 0, &[("SELKIES_WAYLAND", spelling)]);
        assert_eq!(env.get("SELKIES_WAYLAND"), Some("true"), "{spelling:?}");
        assert_eq!(env.probe_calls, [""], "{spelling:?}");
    }
    for spelling in ["False", "FALSE", "0", "no"] {
        let env = run(&good(), 0, &[("SELKIES_WAYLAND", spelling)]);
        assert_eq!(env.get("SELKIES_WAYLAND"), Some("false"), "{spelling:?}");
        assert_eq!(env.probe_calls, [""], "{spelling:?}");
    }
}
