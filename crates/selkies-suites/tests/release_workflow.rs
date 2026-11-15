/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The release workflows, read and run without a runner: the steps that decide what a release
//! is called and what it carries are lifted out of their YAML and run by `sh` and `bash`, and the
//! job gates are evaluated over the branch, tag, caller and input contexts a run can have.
//!
//! One version string has to satisfy pip, dpkg, rpm and apk at once. The tag a maintainer types
//! into the Release workflow becomes the wheel version, the image tags and the version stamped
//! into every native package, so it has to be a PEP 440 version, a pre-release such as 2.0.0rc0
//! included, spelled the way pip normalizes it. Each packager then has to be handed the spelling
//! it orders correctly: dpkg and rpm read 2.0.0rc0 as newer than 2.0.0 and need the tilde form,
//! apk cannot parse it at all. Get any of those wrong and the release either never starts or
//! ships an rc that outranks the release superseding it.
//!
//! What a release carries is whatever every job of the run uploaded, minus a list: the collect
//! step runs over a tree holding one of each kind of artifact, and what a user would see on the
//! release page is read back with the SHA-256 sums it is listed under.
//!
//! These need only `sh` and `bash`, so they run with `cargo test --workspace`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use selkies_suites::{Checks, on_path, repo, workdir};

fn workflow(name: &str) -> String {
    std::fs::read_to_string(repo().join(".github/workflows").join(name))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// The script of the `run: |` block after the line `marker` of `source`, lifted out of the YAML.
fn step_script(source: &str, marker: &str) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    let start = lines.iter().position(|ln| ln.trim() == marker)?;
    let run = (start..lines.len()).find(|&i| lines[i].trim() == "run: |")?;
    let first = lines.get(run + 1)?;
    let indent = first.len() - first.trim_start().len();
    let mut body = Vec::new();
    for ln in &lines[run + 1..] {
        if !ln.trim().is_empty() && ln.len() - ln.trim_start().len() < indent {
            break;
        }
        body.push(ln.get(indent..).unwrap_or(""));
    }
    Some(body.join("\n"))
}

/// A scratch directory of its own for one run.
fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    let dir = workdir().join(format!(
        "release-{tag}-{}-{}",
        std::process::id(),
        RUNS.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch directory");
    dir
}

/// The validate step run on `tag`: whether it took the tag, and the outputs it wrote.
///
/// `latest` is the workflow's input as the step sees it (`auto`, `always`, `never`, or empty for a
/// run that left the default); `git_ref` is the one the run was started from.
fn validate(
    step: &str,
    tag: &str,
    latest: &str,
    git_ref: &str,
) -> (bool, BTreeMap<String, String>) {
    let dir = scratch("ver");
    let out = dir.join("github_output");
    std::fs::write(&out, "").expect("the output file");
    let ok = Command::new("sh")
        .args(["-c", step])
        .env("TAG", tag)
        .env("LATEST", latest)
        .env("GITHUB_OUTPUT", &out)
        .env("GITHUB_REF", git_ref)
        .env(
            "GITHUB_REF_NAME",
            git_ref.rsplit('/').next().unwrap_or(git_ref),
        )
        .output()
        .is_ok_and(|ran| ran.status.success());
    let outputs = std::fs::read_to_string(&out)
        .unwrap_or_default()
        .lines()
        .filter_map(|ln| ln.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    let _ = std::fs::remove_dir_all(&dir);
    (ok, outputs)
}

/// A translation from infra/packaging/version.sh, run on `version`.
fn native(function: &str, version: &str) -> String {
    let script = repo().join("infra/packaging/version.sh");
    Command::new("sh")
        .args(["-c", &format!(". \"$1\"; {function} \"$2\""), "sh"])
        .arg(&script)
        .arg(version)
        .output()
        .map(|ran| String::from_utf8_lossy(&ran.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn unset(outputs: &BTreeMap<String, String>, key: &str) -> String {
    outputs
        .get(key)
        .cloned()
        .unwrap_or_else(|| "<unset>".into())
}

#[test]
fn a_release_version_is_one_every_packager_orders() {
    let mut checks = Checks::new("release-version");
    let text = workflow("release.yaml");
    let step = step_script(&text, "- id: ver").unwrap_or_default();
    checks.check(
        "the workflow still has the validate step this runs",
        !step.trim().is_empty(),
        "",
    );
    if step.trim().is_empty() {
        checks.finish("");
        return;
    }

    // A release is cut from main alone: a new tag only from a run started there, and any tag only
    // on a commit of main, which the steps after this one check
    checks.check(
        "a new tag is cut from main alone",
        text.contains(r#"[ "${GITHUB_REF}" = refs/heads/main ]"#),
        "",
    );
    checks.check(
        "a tag off main is refused",
        text.contains("compare/main...${GITHUB_SHA}"),
        "",
    );

    // tag, whether the workflow takes it, and whether it is a pre-release
    let tags: &[(&str, bool, bool)] = &[
        ("1.2.3", true, false),
        // The tag is the version itself, as in every selkies-project repository
        ("v1.2.3", false, false),
        ("2.0.0rc0", true, true),
        ("2.0.0a1", true, true),
        ("2.0.0b2", true, true),
        ("0.0.0.dev0", true, true),
        // A post-release is a release: it supersedes the version it names
        ("2.0.0.post1", true, false),
        // Valid PEP 440, but pip normalizes each of these to a different string
        ("2.0.0-rc.0", false, false),
        ("1.2.3beta1", false, false),
        ("2.0.0RC0", false, false),
        ("2.0.0rc", false, false),
        // Not the three release components a selkies version carries
        ("2.0", false, false),
        ("2.0.0.0", false, false),
        ("", false, false),
    ];
    for &(tag, want_ok, want_pre) in tags {
        let (ok, out) = validate(&step, tag, "", "refs/heads/main");
        let shown = if tag.is_empty() { "<empty>" } else { tag };
        checks.check(
            &format!(
                "{shown} is {}",
                if want_ok { "accepted" } else { "rejected" }
            ),
            ok == want_ok,
            if ok == want_ok {
                String::new()
            } else {
                format!("accepted={ok}")
            },
        );
        if want_ok {
            let want = if want_pre { "true" } else { "false" };
            checks.check(
                &format!(
                    "{tag} is {}",
                    if want_pre {
                        "a pre-release"
                    } else {
                        "a full release"
                    }
                ),
                out.get("prerelease").map(String::as_str) == Some(want),
                unset(&out, "prerelease"),
            );
            checks.check(
                &format!("{tag} is released as {tag}"),
                out.get("version").map(String::as_str) == Some(tag),
                unset(&out, "version"),
            );
        }
    }

    // A release takes the floating `latest` image tags and the "Latest" badge and a pre-release
    // takes neither, unless the run forces it; GitHub gives no pre-release the badge, so a
    // designated one is marked as a release
    let latest: &[(&str, &str, bool, bool)] = &[
        ("1.2.3", "", true, false),
        ("1.2.3", "auto", true, false),
        ("2.0.0.post1", "auto", true, false),
        ("2.0.0rc0", "", false, true),
        ("2.0.0rc0", "auto", false, true),
        ("2.0.0rc1", "auto", false, true),
        ("0.0.0.dev0", "auto", false, true),
        ("2.0.0rc0", "always", true, false),
        ("1.2.3", "always", true, false),
        ("1.2.3", "never", false, false),
        ("2.0.0rc0", "never", false, true),
    ];
    for &(tag, input, want_latest, want_pre) in latest {
        let (_, out) = validate(&step, tag, input, "refs/heads/main");
        let label = format!(
            "latest={}",
            if input.is_empty() { "default" } else { input }
        );
        checks.check(
            &format!(
                "{tag} with {label} {} the latest tags and badge",
                if want_latest { "takes" } else { "leaves" }
            ),
            out.get("latest").map(String::as_str)
                == Some(if want_latest { "true" } else { "false" }),
            unset(&out, "latest"),
        );
        checks.check(
            &format!(
                "{tag} with {label} is marked as a {}",
                if want_pre { "pre-release" } else { "release" }
            ),
            out.get("prerelease").map(String::as_str)
                == Some(if want_pre { "true" } else { "false" }),
            unset(&out, "prerelease"),
        );
    }

    // version, what dpkg and rpm are handed, and what apk is handed
    let packages: &[(&str, &str, &str)] = &[
        ("2.0.0", "2.0.0", "2.0.0"),
        ("2.0.0rc0", "2.0.0~rc0", "2.0.0_rc0"),
        ("2.0.0a1", "2.0.0~a1", "2.0.0_alpha1"),
        ("2.0.0b2", "2.0.0~b2", "2.0.0_beta2"),
        ("2.0.0.post1", "2.0.0.post1", "2.0.0_p1"),
        ("0.0.0.dev0", "0.0.0~dev0", "0.0.0_alpha0"),
    ];
    for &(version, tilde, alpine) in packages {
        let got = native("tilde_version", version);
        checks.check(
            &format!("dpkg and rpm build {version} as {tilde}"),
            got == tilde,
            &got,
        );
        let got = native("alpine_version", version);
        checks.check(
            &format!("apk builds {version} as {alpine}"),
            got == alpine,
            &got,
        );
    }

    // version, and what Cargo stamps into the server: the semver spelling pixelflux stamps its own
    // crate with
    let crates: &[(&str, &str)] = &[
        ("2.0.0", "2.0.0"),
        ("2.0.0rc0", "2.0.0-rc.0"),
        ("2.0.0a1", "2.0.0-alpha.1"),
        ("2.0.0b2", "2.0.0-beta.2"),
        ("2.0.0.post1", "2.0.0-post.1"),
        ("0.0.0.dev0", "0.0.0-dev.0"),
    ];
    for &(version, want) in crates {
        let got = native("crate_version", version);
        checks.check(
            &format!("the server is built as {want} for {version}"),
            got == want,
            &got,
        );
    }

    // The draft names the commit the run built, which the tag is on; left to GitHub, a draft
    // names the default branch's head by the time it is published
    checks.check(
        "the draft release names the commit the run built",
        text.contains("target_commitish: ${{ github.sha }}"),
        "",
    );

    // The point of the translation, on the one comparator a runner always has
    if on_path("dpkg").is_some() {
        for (older, newer) in [
            ("2.0.0~rc0", "2.0.0"),
            ("1.9.9", "2.0.0~rc0"),
            ("2.0.0~a1", "2.0.0~rc0"),
            ("2.0.0", "2.0.0.post1"),
        ] {
            let ok = Command::new("dpkg")
                .args(["--compare-versions", older, "lt", newer])
                .status()
                .is_ok_and(|s| s.success());
            checks.check(&format!("dpkg sorts {older} below {newer}"), ok, "");
        }
    } else {
        println!("note: no dpkg, leaving the package ordering to the string checks above");
    }
    checks.finish("");
}

/// One of each: what the jobs of a release run leave in the artifact tree.
const DELIVERED: &[(&str, &str)] = &[
    ("a glibc bundle", "selkies-2.0.0rc0-x86_64-linux-gnu.tar.gz"),
    (
        "a musl bundle",
        "selkies-2.0.0rc0-aarch64-linux-musl.tar.gz",
    ),
    (
        "a manylinux wheel",
        "selkies-2.0.0rc0-py3-none-manylinux_2_17_x86_64.whl",
    ),
    (
        "a musllinux wheel",
        "selkies-2.0.0rc0-py3-none-musllinux_1_2_aarch64.whl",
    ),
    ("the sdist", "selkies-2.0.0rc0.tar.gz"),
    (
        "a pixelflux manylinux wheel",
        "pixelflux-2.0.0rc0-cp311-abi3-manylinux2014_x86_64.manylinux_2_17_x86_64.whl",
    ),
    (
        "a pixelflux musllinux wheel",
        "pixelflux-2.0.0rc0-cp311-abi3-musllinux_1_2_aarch64.whl",
    ),
    (
        "a pcmflux manylinux wheel",
        "pcmflux-2.0.0rc0-cp311-abi3-manylinux_2_28_aarch64.whl",
    ),
    (
        "a pcmflux musllinux wheel",
        "pcmflux-2.0.0rc0-cp311-abi3-musllinux_1_2_x86_64.whl",
    ),
    ("the AppImage", "selkies-2.0.0rc0-x86_64.AppImage"),
    ("the deb", "selkies-2.0.0rc0-amd64.deb"),
    ("the rpm", "selkies-2.0.0rc0-x86_64.rpm"),
    ("the apk", "selkies-2.0.0rc0-x86_64.apk"),
    ("the pacman package", "selkies-2.0.0rc0-x86_64.pkg.tar.zst"),
    (
        "the Developer Reference",
        "selkies-2.0.0rc0-reference.tar.gz",
    ),
];

const SUMS: &str = "SHA256SUMS";

/// The file names the collect step leaves for the release, given `names`, each file holding its
/// own name, and the SHA-256 sums it lists, by name; the step's error where it failed.
fn released(
    step: &str,
    names: &[String],
) -> Result<(Vec<String>, BTreeMap<String, String>), String> {
    let work = scratch("assets");
    let arts = work.join("release-artifacts/one-job");
    std::fs::create_dir_all(&arts).expect("the artifact tree");
    for name in names {
        std::fs::write(arts.join(name), name).expect("an artifact");
    }
    let ran = Command::new("bash")
        .args(["-c", step])
        .current_dir(&work)
        .output()
        .map_err(|e| e.to_string())?;
    if !ran.status.success() {
        let err = String::from_utf8_lossy(&ran.stderr);
        return Err(err
            .chars()
            .rev()
            .take(200)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect());
    }
    let release = work.join("release");
    let mut on_release: Vec<String> = std::fs::read_dir(&release)
        .map(|dir| {
            dir.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    on_release.sort();
    let sums = std::fs::read_to_string(release.join(SUMS))
        .unwrap_or_default()
        .lines()
        .filter_map(|ln| ln.split_once("  "))
        .map(|(digest, name)| (name.to_owned(), digest.to_owned()))
        .collect();
    let _ = std::fs::remove_dir_all(&work);
    Ok((on_release, sums))
}

fn sha256_hex(data: &[u8]) -> String {
    let ran = Command::new("sha256sum")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().expect("stdin").write_all(data)?;
            child.wait_with_output()
        })
        .expect("sha256sum");
    String::from_utf8_lossy(&ran.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn a_release_carries_its_deliverables_and_nothing_else() {
    let mut checks = Checks::new("release-assets");
    let step =
        step_script(&workflow("release.yaml"), "- name: Collect release files").unwrap_or_default();
    checks.check(
        "the workflow still has the collect step this runs",
        !step.trim().is_empty(),
        "",
    );
    if step.trim().is_empty() {
        checks.finish("");
        return;
    }
    let digest_placeholder = "a".repeat(64);
    let withheld: [(&str, &str); 3] = [
        (
            "an image build record",
            "selkies-project.selkies.8M3GBU.dockerbuild",
        ),
        ("the bundles' web client", "selkies-web-client.tar"),
        ("an image-digest placeholder", &digest_placeholder),
    ];
    let delivered: Vec<String> = DELIVERED.iter().map(|(_, n)| (*n).to_owned()).collect();
    let mut all = delivered.clone();
    all.extend(withheld.iter().map(|(_, n)| (*n).to_owned()));
    let (on_release, sums) = match released(&step, &all) {
        Ok(found) => found,
        Err(e) => {
            checks.check("the collect step runs", false, e);
            (Vec::new(), BTreeMap::new())
        }
    };
    for (label, name) in DELIVERED {
        checks.check(
            &format!("a release carries {label}"),
            on_release.iter().any(|n| n == name),
            name,
        );
    }
    for (label, name) in withheld {
        checks.check(
            &format!("a release withholds {label}"),
            !on_release.iter().any(|n| n == name),
            name,
        );
    }
    checks.check(
        "a release carries its files' SHA-256 sums",
        on_release.iter().any(|n| n == SUMS),
        "",
    );
    let listed: Vec<&String> = sums.keys().collect();
    let mut wanted: Vec<&String> = delivered.iter().collect();
    wanted.sort();
    let odd: Vec<&String> = listed
        .iter()
        .filter(|n| !wanted.contains(n))
        .chain(wanted.iter().filter(|n| !listed.contains(n)))
        .copied()
        .collect();
    checks.check(
        "the sums list every file delivered and nothing else",
        listed == wanted,
        format!("{odd:?}"),
    );
    checks.check(
        "each sum is its file's",
        delivered.iter().all(|name| {
            sums.get(name)
                .is_some_and(|d| *d == sha256_hex(name.as_bytes()))
        }),
        "",
    );
    let extra: Vec<&String> = on_release
        .iter()
        .filter(|n| !delivered.contains(n) && n.as_str() != SUMS)
        .collect();
    let mut expected: Vec<String> = delivered.clone();
    expected.push(SUMS.to_owned());
    expected.sort();
    checks.check(
        "nothing else reaches the release",
        on_release == expected,
        format!("{extra:?}"),
    );
    checks.finish("");
}

/// The body of the job `name` of a workflow: its lines up to the next job.
fn job<'a>(source: &'a str, name: &str) -> &'a str {
    let head = format!("  {name}:\n");
    let start = source
        .match_indices(&head)
        .find(|&(at, _)| at == 0 || source.as_bytes()[at - 1] == b'\n')
        .map(|(at, _)| at + head.len())
        .unwrap_or_else(|| panic!("workflow has no {name} job"));
    let rest = &source[start..];
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let is_job = line.len() > 3
            && line.starts_with("  ")
            && !line.starts_with("   ")
            && line.trim_end().ends_with(':')
            && line
                .trim()
                .trim_end_matches(':')
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if is_job {
            break;
        }
        offset += line.len();
    }
    &rest[..offset]
}

/// The one-line `if: ${{ … }}` gate of the job `name`.
fn gate(source: &str, name: &str) -> String {
    job(source, name)
        .lines()
        .find_map(|ln| {
            ln.strip_prefix("    if: ${{ ")
                .and_then(|rest| rest.strip_suffix(" }}"))
        })
        .unwrap_or_else(|| panic!("{name} has no one-line job gate"))
        .to_owned()
}

/// The value of a GitHub Actions expression.
#[derive(Clone, Debug, PartialEq)]
enum Value {
    Null,
    Bool(bool),
    Str(String),
}

impl Value {
    fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Str(s) => !s.is_empty(),
        }
    }
}

/// The subset of GitHub Actions expressions the workflows' gates use: literals, context
/// properties, `==`, `!=`, `!`, `&&`, `||`, parentheses, `startsWith` and `cancelled`, with
/// GitHub's semantics (`&&` and `||` yield an operand, strings compare ignoring case).
struct Expression<'a> {
    tokens: Vec<&'a str>,
    at: usize,
    context: &'a BTreeMap<&'a str, Value>,
}

fn tokenize(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let start = i;
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'\'' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'\'' {
                i += 1;
            }
            i += 1;
        } else if text[i..].starts_with("&&")
            || text[i..].starts_with("||")
            || text[i..].starts_with("==")
            || text[i..].starts_with("!=")
        {
            i += 2;
        } else if b"()!,".contains(&c) {
            i += 1;
        } else {
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || b"._-".contains(&bytes[i]))
            {
                i += 1;
            }
            assert!(i > start, "unexpected {:?} in {text}", c as char);
        }
        tokens.push(&text[start..i.min(bytes.len())]);
    }
    tokens
}

impl Expression<'_> {
    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.at).copied()
    }

    fn take(&mut self, token: &str) -> bool {
        if self.peek() == Some(token) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn or(&mut self) -> Value {
        let mut left = self.and();
        while self.take("||") {
            let right = self.and();
            if !left.truthy() {
                left = right;
            }
        }
        left
    }

    fn and(&mut self) -> Value {
        let mut left = self.unary();
        while self.take("&&") {
            let right = self.unary();
            if left.truthy() {
                left = right;
            }
        }
        left
    }

    fn unary(&mut self) -> Value {
        if self.take("!") {
            return Value::Bool(!self.unary().truthy());
        }
        let left = self.primary();
        for (op, equal) in [("==", true), ("!=", false)] {
            if self.take(op) {
                let right = self.primary();
                let same = match (&left, &right) {
                    (Value::Str(a), Value::Str(b)) => a.eq_ignore_ascii_case(b),
                    (a, b) => a == b,
                };
                return Value::Bool(same == equal);
            }
        }
        left
    }

    fn primary(&mut self) -> Value {
        let token = self.peek().expect("an operand").to_owned();
        self.at += 1;
        if token == "(" {
            let inner = self.or();
            assert!(self.take(")"), "an unclosed parenthesis");
            return inner;
        }
        if let Some(quoted) = token.strip_prefix('\'') {
            return Value::Str(quoted.trim_end_matches('\'').to_owned());
        }
        match token.as_str() {
            "true" => return Value::Bool(true),
            "false" => return Value::Bool(false),
            "null" => return Value::Null,
            _ => {}
        }
        if self.take("(") {
            let mut args = Vec::new();
            while !self.take(")") {
                args.push(self.or());
                self.take(",");
            }
            return match (token.as_str(), args.as_slice()) {
                ("cancelled", []) => Value::Bool(false),
                ("startsWith", [Value::Str(s), Value::Str(prefix)]) => Value::Bool(
                    s.to_ascii_lowercase()
                        .starts_with(&prefix.to_ascii_lowercase()),
                ),
                ("startsWith", [_, _]) => Value::Bool(false),
                (name, _) => panic!("no function {name} in the evaluated subset"),
            };
        }
        self.context
            .get(token.as_str())
            .cloned()
            .unwrap_or(Value::Null)
    }
}

fn evaluate(expression: &str, context: &BTreeMap<&str, Value>) -> bool {
    let mut parsed = Expression {
        tokens: tokenize(expression),
        at: 0,
        context,
    };
    let value = parsed.or();
    assert!(
        parsed.peek().is_none(),
        "{expression}: trailing {:?}",
        parsed.peek()
    );
    value.truthy()
}

/// Whether a gate lets a job run in an event context.
///
/// A called workflow sees its caller's event, so `event` is `push` or `workflow_dispatch` and
/// never `workflow_call`; `release` is the `workflow_call` input a standalone dispatch does not
/// declare (`None`). The jobs a gate needs succeeded and the run is not cancelled.
fn runs(
    expression: &str,
    event: &str,
    git_ref: &str,
    tag_prefix: &str,
    release: Option<bool>,
    repository: &str,
) -> bool {
    let ref_name = git_ref
        .strip_prefix("refs/heads/")
        .or_else(|| git_ref.strip_prefix("refs/tags/"))
        .unwrap_or(git_ref);
    let context = BTreeMap::from([
        ("github.event_name", Value::Str(event.into())),
        ("github.ref", Value::Str(git_ref.into())),
        ("github.ref_name", Value::Str(ref_name.into())),
        ("github.repository", Value::Str(repository.into())),
        ("inputs.tag_prefix", Value::Str(tag_prefix.into())),
        ("inputs.release", release.map_or(Value::Null, Value::Bool)),
        ("needs.bundles.result", Value::Str("success".into())),
    ]);
    evaluate(expression, &context)
}

/// A gate case: its label, the event, the ref, the `tag_prefix` input, the `release` input, and
/// whether the job runs.
type GateCase = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    Option<bool>,
    bool,
);

const CANONICAL: &str = "selkies-project/selkies";

/// What a job's `needs:` line names.
fn needs(source: &str, name: &str) -> Vec<String> {
    job(source, name)
        .lines()
        .find_map(|ln| ln.strip_prefix("    needs: "))
        .map(|list| {
            list.trim_matches(['[', ']'])
                .split(',')
                .map(|item| item.trim().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn release_jobs_run_where_and_in_the_order_they_should() {
    let mut checks = Checks::new("release-guards");
    let images = workflow("images.yaml");
    for name in ["build", "addons"] {
        let expression = gate(&images, name);
        let cases: &[GateCase] = &[
            (
                "main CI call",
                "push",
                "refs/heads/main",
                "main",
                Some(false),
                true,
            ),
            (
                "main dispatch",
                "workflow_dispatch",
                "refs/heads/main",
                "main",
                None,
                true,
            ),
            (
                "manual release from main",
                "workflow_dispatch",
                "refs/heads/main",
                "2.0.0rc0",
                Some(true),
                true,
            ),
            (
                "pushed-tag release",
                "push",
                "refs/tags/2.0.0rc0",
                "2.0.0rc0",
                Some(true),
                true,
            ),
            (
                "release dispatched from its tag",
                "workflow_dispatch",
                "refs/tags/2.0.0rc0",
                "2.0.0rc0",
                Some(true),
                true,
            ),
            (
                "standalone tag dispatch",
                "workflow_dispatch",
                "refs/tags/2.0.0rc0",
                "2.0.0rc0",
                None,
                false,
            ),
            (
                "tag call with an empty prefix",
                "push",
                "refs/tags/2.0.0rc0",
                "",
                Some(true),
                false,
            ),
            (
                "tag call with a mismatched prefix",
                "push",
                "refs/tags/2.0.0rc0",
                "latest",
                Some(true),
                false,
            ),
            (
                "non-main branch call",
                "push",
                "refs/heads/topic",
                "main",
                Some(false),
                false,
            ),
        ];
        for &(label, event, git_ref, prefix, release, expected) in cases {
            let release_shown = release.map_or("None".to_owned(), |b| {
                if b { "True".into() } else { "False".into() }
            });
            checks.check(
                &format!("{name}: {label}"),
                runs(&expression, event, git_ref, prefix, release, CANONICAL) == expected,
                format!(
                    "event={event} ref={git_ref} tag_prefix={prefix:?} release={release_shown}"
                ),
            );
        }
        checks.check(
            &format!("{name}: component checkout never publishes canonical images"),
            !runs(
                &expression,
                "push",
                "refs/heads/main",
                "main",
                Some(false),
                "selkies-project/pixelflux",
            ),
            "",
        );
    }
    // The video build also waits on the bundles its images carry; past that, both build on the
    // same releases
    let build = gate(&images, "build");
    let build_release = build
        .strip_prefix("!cancelled() && needs.bundles.result != 'failure' && ")
        .unwrap_or(&build);
    checks.check(
        "video and addon jobs use the same release gate",
        build_release == gate(&images, "addons"),
        "",
    );
    let cleanup = gate(&images, "cleanup");
    checks.check(
        "tag releases leave registry cleanup alone",
        !runs(
            &cleanup,
            "push",
            "refs/tags/2.0.0rc0",
            "2.0.0rc0",
            Some(true),
            CANONICAL,
        ),
        "",
    );
    checks.check(
        "main builds retain registry cleanup",
        runs(
            &cleanup,
            "push",
            "refs/heads/main",
            "main",
            Some(false),
            CANONICAL,
        ),
        "",
    );
    checks.check(
        "component builds never prune canonical images",
        !runs(
            &cleanup,
            "push",
            "refs/heads/main",
            "main",
            Some(false),
            "selkies-project/pcmflux",
        ),
        "",
    );

    let restricted = format!("github.repository == '{CANONICAL}'");
    for name in ["merge", "addons-merge"] {
        checks.check(
            &format!("{name} is restricted to canonical Selkies"),
            job(&images, name).contains(&restricted),
            "",
        );
    }
    for (file, names) in [
        ("ci.yaml", &["images", "pre-release"][..]),
        ("docs.yaml", &["publish"][..]),
        ("release.yaml", &["check"][..]),
        ("devcontainer-feature.yaml", &["publish"][..]),
        ("build-reference.yaml", &["attach"][..]),
    ] {
        let source = workflow(file);
        for name in names {
            checks.check(
                &format!("{file} {name} is restricted to canonical Selkies"),
                job(&source, name).contains(&restricted),
                "",
            );
        }
    }
    let scheduled =
        format!("github.event_name != 'schedule' || github.repository == '{CANONICAL}'");
    for (file, names) in [
        (
            "tests.yaml",
            &["unit", "server-unit", "wheel", "wheels", "suites"][..],
        ),
        ("fuzz.yaml", &["fuzz"][..]),
        ("browserstack.yaml", &["credentials"][..]),
    ] {
        let source = workflow(file);
        for name in names {
            checks.check(
                &format!("{file} {name} schedules only in canonical Selkies"),
                job(&source, name).contains(&scheduled),
                "",
            );
        }
    }

    let published = workflow("release-published.yaml");
    for name in ["latest", "pypi-pcmflux", "pypi-pixelflux", "pypi"] {
        checks.check(
            &format!("published {name} is restricted to canonical Selkies"),
            job(&published, name).contains(&restricted),
            "",
        );
    }
    let order: Vec<(&str, Vec<String>)> = ["pypi-pcmflux", "pypi-pixelflux", "pypi"]
        .into_iter()
        .map(|name| (name, needs(&published, name)))
        .collect();
    checks.check(
        "PCM wheels publish first",
        order[1].1 == ["pypi-pcmflux"],
        format!("{order:?}"),
    );
    checks.check(
        "Selkies publishes after PCM and PixelFlux",
        order[2].1 == ["pypi-pixelflux"],
        format!("{order:?}"),
    );

    let ci = workflow("ci.yaml");
    let native = job(&ci, "native-tests");
    let step = native
        .split("\n      - ")
        .find(|s| s.starts_with("name: Test the non-GPL pixelflux configuration\n"));
    let condition = step.and_then(|s| {
        s.lines()
            .find_map(|ln| ln.strip_prefix("        if: "))
            .map(str::to_owned)
    });
    for arch in ["x86_64", "aarch64"] {
        let selected = step.is_some()
            && condition.as_deref().is_none_or(|expression| {
                let expression = expression
                    .trim()
                    .trim_start_matches("${{")
                    .trim_end_matches("}}");
                evaluate(
                    expression,
                    &BTreeMap::from([("matrix.arch", Value::Str(arch.into()))]),
                )
            });
        checks.check(&format!("non-GPL tests run on {arch}"), selected, "");
    }
    checks.finish("");
}
