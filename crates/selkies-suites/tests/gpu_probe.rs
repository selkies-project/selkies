/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The GPU report the container reads before choosing a backend.
//!
//! pixelflux owns hardware detection, and `selkies gpu-probe` reports what it finds: whether a
//! Wayland session here would be hardware accelerated, on which node, and whether a GPU is
//! present at all, as KEY=VALUE lines the container's entrypoint exports and one summary line on
//! stderr. The container starts X11 instead of Wayland when the answer is no but a GPU is
//! present, so that decision is only as good as this report: a renamed key or a changed
//! vocabulary leaves the container on whatever backend was asked for, and a session silently
//! loses the GPU it was provisioned with. So the report's shape is pinned here, along with the
//! answers that do not depend on what hardware the test machine has.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use selkies_suites::server::run_within;
use selkies_suites::{Checks, server_binary, workdir};

const SERVER_SIDE: [&str; 4] = [
    "SELKIES_GPU_RENDER_NODE",
    "SELKIES_GPU_DRIVER",
    "SELKIES_GPU_PRESENT",
    "SELKIES_GPU_ACCELERATED",
];

const CLIENT: [&str; 4] = [
    "SELKIES_GPU_GL_VENDOR",
    "SELKIES_GPU_EGL_X11",
    "SELKIES_GPU_MESA_DRIVER",
    "SELKIES_GPU_VULKAN_PRESENTS",
];

/// What `selkies gpu-probe` reports with `env` added: its facts, its summary line, its stderr.
fn probe(env: &[(&str, &str)]) -> (BTreeMap<String, String>, String, String) {
    let mut command = Command::new(server_binary());
    command.arg("gpu-probe").envs(env.iter().copied());
    let Ok(ran) = run_within(&mut command, Duration::from_secs(180)) else {
        return (BTreeMap::new(), String::new(), String::new());
    };
    let facts = ran
        .stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    let summary = ran
        .stderr
        .lines()
        .find(|line| line.starts_with("GPU: "))
        .unwrap_or_default()
        .to_owned();
    (facts, summary, ran.stderr)
}

fn fact<'a>(facts: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    facts.get(key).map_or("", String::as_str)
}

fn has(facts: &BTreeMap<String, String>, key: &str) -> bool {
    facts.contains_key(key)
}

fn excerpt(text: &str) -> String {
    text.trim().chars().take(120).collect()
}

#[test]
#[ignore = "needs a built selkies server (SELKIES_TEST_SERVER)"]
fn the_gpu_report_has_the_shape_the_container_reads() {
    let mut checks = Checks::new("gpu-probe");
    let (facts, summary, _) = probe(&[]);
    let keys: Vec<&String> = facts.keys().collect();
    checks.check(
        "the report has every fact the container reads",
        SERVER_SIDE.iter().all(|k| has(&facts, k)),
        format!("{keys:?}"),
    );
    checks.check(
        "presence and acceleration are answered true or false",
        ["true", "false"].contains(&fact(&facts, "SELKIES_GPU_PRESENT"))
            && ["true", "false"].contains(&fact(&facts, "SELKIES_GPU_ACCELERATED")),
        format!("{facts:?}"),
    );

    // Acceleration and its explanation cannot disagree, whichever way this machine answers: the
    // container prints the summary and acts on the flag.
    let accelerated = fact(&facts, "SELKIES_GPU_ACCELERATED") == "true";
    checks.check(
        "an accelerated report names a node and gives no reason against it",
        !accelerated
            || (!fact(&facts, "SELKIES_GPU_RENDER_NODE").is_empty()
                && !summary.contains("unusable")),
        format!("{facts:?} {summary}"),
    );
    checks.check(
        "an unaccelerated report says why",
        accelerated || summary.contains("unusable: ") || summary == "GPU: none here",
        &summary,
    );

    // A node that cannot back a renderer is unaccelerated however it fails, and saying so is the
    // whole point: /dev/null opens and makes a GBM device on some drivers, so only carrying the
    // bring-up through tells them apart. Nor does the probe fall back to another node when it is
    // given one.
    for path in ["/dev/dri/renderD999", "/dev/null"] {
        let (bogus, said, _) = probe(&[("DRINODE", path)]);
        checks.check(
            &format!("{path} is reported unaccelerated, with why"),
            fact(&bogus, "SELKIES_GPU_ACCELERATED") == "false"
                && (said.contains("unusable: ") || said == "GPU: none here"),
            &said,
        );
        let node = fact(&bogus, "SELKIES_GPU_RENDER_NODE");
        checks.check(
            &format!("{path} names no other node"),
            has(&bogus, "SELKIES_GPU_RENDER_NODE") && (node.is_empty() || node == path),
            node,
        );
    }

    // GPU presence is independent of whether the compositor can use one, which is what lets the
    // container tell "no GPU here" from "GPU it cannot reach".
    checks.check(
        "GPU presence is reported separately from acceleration",
        fact(&facts, "SELKIES_GPU_PRESENT") == "true" || !accelerated,
        format!("{facts:?}"),
    );

    // The recorded environment is the exec shell's to source, not the probe's to read: the probe
    // answers for exactly what it is given, so a variable sitting in the recording alone steers
    // nothing, and one in the actual environment does.
    let runtime = workdir().join(format!("gpu-probe-runtime-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).expect("a runtime directory");
    std::fs::write(
        runtime.join("container-env"),
        "export DRINODE=/dev/dri/renderD999\n",
    )
    .expect("the recorded environment");
    let runtime_dir = runtime.to_string_lossy().into_owned();
    let (_, _, recorded) = probe(&[("XDG_RUNTIME_DIR", &runtime_dir)]);
    checks.check(
        "a recorded variable alone does not steer the probe",
        !recorded.contains("renderD999"),
        excerpt(&recorded),
    );
    let (_, _, given) = probe(&[("XDG_RUNTIME_DIR", &runtime_dir), ("DRINODE", "/dev/null")]);
    checks.check(
        "what the probe is actually given wins",
        given.contains("/dev/null"),
        excerpt(&given),
    );
    let _ = std::fs::remove_dir_all(&runtime);

    // The client-path facts, measured on whatever this machine has: every one reported, each from
    // its own small vocabulary, and consistent with each other, since the entrypoint maps them
    // without looking further.
    checks.check(
        "every client-path fact is reported",
        CLIENT.iter().all(|k| has(&facts, k)),
        format!("{keys:?}"),
    );
    let mesa = fact(&facts, "SELKIES_GPU_MESA_DRIVER");
    let vulkan = fact(&facts, "SELKIES_GPU_VULKAN_PRESENTS");
    let vendor = fact(&facts, "SELKIES_GPU_GL_VENDOR");
    checks.check(
        "Mesa's route is one of native, zink, or software",
        has(&facts, "SELKIES_GPU_MESA_DRIVER") && ["native", "zink", ""].contains(&mesa),
        format!("{facts:?}"),
    );
    checks.check(
        "whether Vulkan presents is answered only where Zink is Mesa's route",
        ["true", "false"].contains(&vulkan) == (mesa == "zink")
            || (has(&facts, "SELKIES_GPU_VULKAN_PRESENTS") && vulkan.is_empty()),
        format!("{facts:?}"),
    );
    checks.check(
        "a GL vendor is only named where a GPU driver was found",
        vendor.is_empty() || !fact(&facts, "SELKIES_GPU_DRIVER").is_empty(),
        format!("{facts:?}"),
    );
    checks.check(
        "Mesa's native route names Mesa as the vendor",
        mesa != "native" || vendor == "mesa",
        format!("{facts:?}"),
    );
    checks.finish("");
}
