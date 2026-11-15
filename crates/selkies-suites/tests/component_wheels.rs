/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The server and its optional Python modules use one local native source tree.
//!
//! These checks guard the workspace path dependencies, the local wheel package folders, the
//! dependency-image recipes, and the artifact names the suites consume. They build no native
//! library and contact no external repository, so they run with `cargo test --workspace`.

use selkies_suites::{Checks, repo};

fn read(path: &str) -> String {
    std::fs::read_to_string(repo().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The inside of a one-line `name = { … }` table of a manifest.
fn inline_table<'a>(manifest: &'a str, name: &str) -> Option<&'a str> {
    let head = format!("{name} = {{");
    manifest.lines().find_map(|line| {
        let inner = line.strip_prefix(&head)?.strip_suffix('}')?;
        (!inner.is_empty() && !inner.contains('}')).then_some(inner)
    })
}

/// Whether `source` declares the workflow input `name` as a string defaulting to empty.
fn empty_string_input(source: &str, name: &str) -> bool {
    let lines: Vec<&str> = source.lines().collect();
    let key = format!("{name}:");
    lines.iter().enumerate().any(|(i, line)| {
        let indented = |at: usize, text: &str| {
            lines
                .get(at)
                .is_some_and(|l| l.len() > l.trim_start().len() && l.trim_start().starts_with(text))
        };
        line.ends_with(&key)
            && indented(i + 1, "description:")
            && indented(i + 2, "type: string")
            && lines.get(i + 2).is_some_and(|l| l.trim() == "type: string")
            && indented(i + 3, "default: ''")
            && lines.get(i + 3).is_some_and(|l| l.trim() == "default: ''")
    })
}

#[test]
fn the_component_wheels_build_from_the_local_tree() {
    let mut checks = Checks::new("pixelflux-pcmflux-wheels");
    let manifest = read("Cargo.toml");
    let workflow = read(".github/workflows/build-pixelflux-pcmflux-wheels.yaml");
    let postcreate = read(".devcontainer/postcreate.sh");
    let members = manifest.split("default-members").next().unwrap_or_default();

    for project in ["pixelflux", "pcmflux"] {
        let dependency = inline_table(&manifest, project);
        checks.check(
            &format!("{project} is a local path dependency"),
            dependency.is_some_and(|d| {
                d.contains(&format!("path = \"crates/{project}\""))
                    && !d.contains(" git = ")
                    && !d.contains(" rev = ")
            }),
            "",
        );
        checks.check(
            &format!("{project} is an explicit workspace member"),
            members.contains(&format!("\"crates/{project}\"")),
            "",
        );
        checks.check(
            &format!("the workflow plans from crates/{project}"),
            workflow.contains("cd \"crates/${project}\"")
                && workflow.contains("for project in pixelflux pcmflux"),
            "",
        );
        checks.check(
            &format!("{project} wheels are written to and uploaded from its local wheelhouse"),
            workflow.contains("output-dir: crates/${{ matrix.group.project }}/wheelhouse")
                && workflow.contains("path: crates/${{ matrix.group.project }}/wheelhouse/*.whl")
                && workflow.contains("name: ${{ matrix.group.project }}-wheels-"),
            "",
        );
        checks.check(
            &format!("{project} wheels come from its local package directory"),
            workflow.contains("package-dir: crates/${{ matrix.group.project }}"),
            "",
        );
        checks.check(
            &format!("development installs the local {project} wheel"),
            postcreate.contains(&format!("./crates/{project}"))
                && postcreate.contains("pip wheel --no-deps"),
            "",
        );
        let cargo = read(&format!("crates/{project}/Cargo.toml"));
        let setup = read(&format!("crates/{project}/setup.py"));
        let pyproject = read(&format!("crates/{project}/pyproject.toml"));
        checks.check(
            &format!("{project} opt-ins separate full-API and 3.11 stable-ABI extensions"),
            cargo.contains(r#"extension-module = ["python", "pyo3/extension-module"]"#)
                && cargo
                    .contains(r#"abi3-extension-module = ["extension-module", "pyo3/abi3-py311"]"#)
                && cargo.lines().any(|line| {
                    line.strip_prefix(r#"python = ["dep:pyo3""#)
                        .is_some_and(|rest| rest.starts_with(',') || rest.starts_with(']'))
                }),
            "",
        );
        checks.check(
            &format!("{project} retains full-API source builds on Python 3.9+"),
            setup.contains(
                r#"options={"bdist_wheel": {"py_limited_api": "cp311"}} if _abi3 else {}"#,
            ) && setup.contains(r#""abi3-extension-module" if _abi3 else "extension-module""#)
                && setup.contains(r#"python_requires=">=3.9""#),
            "",
        );
        checks.check(
            &format!("{project} builds release wheels explicitly in ABI3 mode on CPython 3.11"),
            pyproject.contains(r#"build = "cp311-*""#)
                && pyproject.contains(&format!("{}_ABI3 = \"1\"", project.to_uppercase()))
                && pyproject.contains(r#"skip = "*t-*""#),
            "",
        );
    }

    checks.check(
        "no external release location or polling remains",
        !workflow.contains("locate:")
            && !workflow.contains("gh release")
            && !workflow.contains("gh run")
            && !workflow.contains("repository: ${{ github.repository_owner }}")
            && !workflow.contains("src/${project}"),
        "",
    );
    checks.check(
        "the wheel image jobs execute only the planned recipe",
        workflow.contains("RECIPE: ${{ matrix.image.recipe }}")
            && workflow.contains("bash /recipe.sh"),
        "",
    );
    checks.check(
        "pixelflux keeps its dependency image and job split",
        workflow.contains("image=pixelflux-wheel-deps")
            && workflow.contains(r#"jobs="manylinux=2 musllinux=4""#),
        "",
    );
    checks.check(
        "pcmflux keeps its dependency image and job split",
        workflow.contains("image=pcmflux-wheel-deps")
            && workflow.contains(r#"jobs="manylinux=1 musllinux=1""#),
        "",
    );
    checks.check(
        "release builds stamp component and root locks",
        workflow.contains("crates/${PROJECT}/Cargo.lock")
            && workflow.contains("Cargo.lock")
            && workflow.contains("inputs.package_version"),
        "",
    );
    checks.check(
        "wheel checkout inputs default to the caller repository and commit",
        empty_string_input(&workflow, "checkout_repository")
            && empty_string_input(&workflow, "checkout_ref")
            && workflow.contains("${{ inputs.checkout_repository || github.repository }}")
            && workflow.contains("${{ inputs.checkout_ref || github.sha }}"),
        "",
    );
    let pinned_ref = "ref: ${{ needs.plan.outputs.checkout_commit }}";
    let pinned_head =
        "test \"$(git rev-parse HEAD)\" = '${{ needs.plan.outputs.checkout_commit }}'";
    checks.check(
        "every native build is pinned to the exact planned checkout",
        workflow.contains("value: ${{ jobs.plan.outputs.checkout_commit }}")
            && workflow.matches(pinned_ref).count() == 2
            && workflow.matches(pinned_head).count() == 2,
        "",
    );
    checks.finish("");
}
