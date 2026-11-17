/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Native abi3 and server wheels are selected by their parsed compatibility tags.
//!
//! The devcontainer's wheel selector
//! (.devcontainer/features/desktop-selkies/src/select-compatible-wheels.py) picks the wheels an
//! interpreter can install for the release downloader and the integration workflow. It runs here
//! as they run it, over each interpreter's supported tags handed to it with `--tags`: CPython 3.9
//! to 3.15, free-threaded CPython, another platform. Its callers pass no `--tags`, so the last
//! check runs it that way and holds the answer to the one it gives for the running interpreter's
//! own tags.
//!
//! The selector is a Python script on `packaging`, so these need `python3` with `packaging`; they
//! run with `cargo test --workspace`.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use selkies_suites::{Checks, repo, workdir};

const P: &str = "manylinux_2_17_x86_64";
const ABI3: &str = "pixelflux-2.3.0-cp311-abi3-manylinux2014_x86_64.manylinux_2_17_x86_64.whl";
const PCM: &str = "pcmflux-2.2.2-cp311-abi3-manylinux2014_x86_64.manylinux_2_17_x86_64.whl";
const ROOT: &str = "selkies-2.3.0-py3-none-manylinux_2_17_x86_64.whl";
const OLD: &str = "pixelflux-2.2.3-cp312-cp312-manylinux_2_28_x86_64.whl";

fn selector() -> PathBuf {
    repo().join(".devcontainer/features/desktop-selkies/src/select-compatible-wheels.py")
}

/// The lines of `input` the selector keeps for `project` on the supported `tags`, or on the
/// running interpreter's own where `tags` is `None`, as its callers run it.
fn select(project: &str, input: &str, tags: Option<&[String]>) -> Result<Vec<String>, String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    let file = workdir().join(format!(
        "wheel-tags-{}-{}",
        std::process::id(),
        RUNS.fetch_add(1, Ordering::Relaxed)
    ));
    let mut command = Command::new("python3");
    command.arg(selector());
    if let Some(tags) = tags {
        std::fs::write(&file, tags.join("\n")).map_err(|e| e.to_string())?;
        command.arg("--tags").arg(&file);
    }
    let mut child = command
        .arg(project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("python3: {e}"))?;
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&file);
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// The tags the running `python3` supports, as `packaging` lists them.
fn own_tags() -> Result<Vec<String>, String> {
    let out = Command::new("python3")
        .args([
            "-c",
            "from packaging.tags import sys_tags\nfor tag in sys_tags(): print(tag)",
        ])
        .output()
        .map_err(|e| format!("python3: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// Whether the selector takes `filename` as a wheel of `project` on the supported `tags`.
fn compatible(project: &str, filename: &str, tags: &[String]) -> Result<bool, String> {
    select(project, &format!("{filename}\n"), Some(tags))
        .map(|kept| kept.iter().any(|k| k == filename))
}

/// An interpreter's own tag and the pure-Python one, on `platform`.
fn tags(interpreter: &str, abi: &str, platform: &str) -> Vec<String> {
    vec![
        format!("{interpreter}-{abi}-{platform}"),
        format!("py3-none-{platform}"),
    ]
}

fn tag(text: &str) -> Vec<String> {
    vec![text.to_owned()]
}

/// Records the check `label`, which holds when the selector answered `want`.
fn verdict(checks: &mut Checks, label: &str, got: Result<bool, String>, want: bool) {
    let detail = got.as_ref().err().cloned().unwrap_or_default();
    checks.check(label, got == Ok(want), detail);
}

#[test]
fn wheels_are_selected_by_their_tags() {
    let mut checks = Checks::new("wheel-selection");
    for minor in 11..16 {
        let supported = vec![
            format!("cp3{minor}-cp3{minor}-{P}"),
            format!("cp311-abi3-{P}"),
            format!("py3-none-{P}"),
        ];
        verdict(
            &mut checks,
            &format!("cp311-abi3 fits CPython 3.{minor}"),
            compatible("pixelflux", ABI3, &supported),
            true,
        );
        verdict(
            &mut checks,
            &format!("the server wheel fits CPython 3.{minor}"),
            compatible("selkies", ROOT, &supported),
            true,
        );
    }
    for minor in [9, 10] {
        let own = format!("cp3{minor}");
        verdict(
            &mut checks,
            &format!("cp311-abi3 is refused on CPython 3.{minor}"),
            compatible("pixelflux", ABI3, &tags(&own, &own, P)),
            false,
        );
        verdict(
            &mut checks,
            &format!("the server wheel still fits CPython 3.{minor}"),
            compatible("selkies", ROOT, &tags(&own, &own, P)),
            true,
        );
    }
    verdict(
        &mut checks,
        "old full-API wheels fit their own interpreter",
        compatible(
            "pixelflux",
            OLD,
            &tags("cp312", "cp312", "manylinux_2_28_x86_64"),
        ),
        true,
    );
    verdict(
        &mut checks,
        "old full-API wheels do not fit a newer interpreter",
        compatible(
            "pixelflux",
            OLD,
            &[format!("cp313-cp313-{P}"), format!("cp311-abi3-{P}")],
        ),
        false,
    );
    verdict(
        &mut checks,
        "abi3 is refused on free-threaded CPython",
        compatible("pixelflux", ABI3, &tags("cp313", "cp313t", P)),
        false,
    );
    let abi3_here = tag(&format!("cp311-abi3-{P}"));
    verdict(
        &mut checks,
        "abi3 is refused for another project",
        compatible("pcmflux", ABI3, &abi3_here),
        false,
    );
    verdict(
        &mut checks,
        "abi3 is refused for another platform",
        compatible("pixelflux", ABI3, &tag("cp311-abi3-manylinux_2_28_aarch64")),
        false,
    );
    verdict(
        &mut checks,
        "a malformed filename is not a wheel",
        compatible("pixelflux", "pixelflux-2.3.0-abi3.tar.gz", &abi3_here),
        false,
    );
    verdict(
        &mut checks,
        "an unrelated abi3 asset is not accepted as pcmflux",
        compatible(
            "pcmflux",
            &ABI3.replace("pixelflux", "some-other-project"),
            &abi3_here,
        ),
        false,
    );

    // Release asset lines keep their digests, and only the compatible ones are kept
    let assets = format!(
        "https://example.invalid/{ABI3} sha256:abc\n\
         https://example.invalid/not-a-wheel.txt sha256:def\n\
         https://example.invalid/{PCM} sha256:123\n"
    );
    let selected = select("pcmflux", &assets, Some(&abi3_here));
    checks.check(
        "the CLI preserves only compatible release assets and digests",
        selected.as_deref() == Ok(&[format!("https://example.invalid/{PCM} sha256:123")][..]),
        format!("{selected:?}"),
    );
    let own = own_tags();
    let asked = own
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|tags| select("pcmflux", &assets, Some(tags)));
    let unasked = select("pcmflux", &assets, None);
    checks.check(
        "without --tags the CLI selects for the running interpreter",
        asked.is_ok() && unasked == asked,
        format!("{unasked:?} against {asked:?}"),
    );

    let workflow =
        std::fs::read_to_string(repo().join(".github/workflows/tests.yaml")).unwrap_or_default();
    checks.check(
        "the integration workflow uses the shared parser",
        workflow.contains(r#"python "${SELECTOR}" "${project}""#)
            && !workflow.contains(r#"WHEEL_GLOB="*${ABI}-"#),
        "",
    );
    checks.finish("");
}
