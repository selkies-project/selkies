#!/usr/bin/env python3
"""One version string has to satisfy pip, dpkg, rpm, and apk at once.

The tag a maintainer types into the Release workflow becomes the wheel version,
the image tags, and the version stamped into every native package, so it has to
be a PEP 440 version -- a pre-release such as 2.0.0rc0 included -- spelled the
way pip normalizes it, or the tag and the artifacts it names drift apart. Each
packager then has to be handed the spelling it orders correctly: dpkg and rpm
read 2.0.0rc0 as newer than 2.0.0 and need the tilde form, apk cannot parse it
at all, and pacman already sorts a trailing alphabetic suffix below the bare
release. Get any of those wrong and the release either never starts or ships an
rc that outranks the release superseding it.
"""
import os
import shutil
import subprocess
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
WORKFLOW = os.path.join(REPO, ".github", "workflows", "release.yaml")
VERSION_SH = os.path.join(REPO, "infra", "packaging", "version.sh")

passed = failed = 0


def check(label, ok, detail=""):
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [release-version] {label}  {detail}", flush=True)


def sh(script, *args):
    return subprocess.run(["sh", "-c", script, "sh", *args],
                          capture_output=True, text=True, timeout=60)


def validate_step():
    """The script the workflow's validate step runs, lifted out of the YAML."""
    lines = open(WORKFLOW).read().splitlines()
    start = next(i for i, ln in enumerate(lines) if ln.strip() == "- id: ver")
    run = next(i for i in range(start, len(lines)) if lines[i].strip() == "run: |")
    indent = len(lines[run + 1]) - len(lines[run + 1].lstrip())
    body = []
    for ln in lines[run + 1:]:
        if ln.strip() and len(ln) - len(ln.lstrip()) < indent:
            break
        body.append(ln[indent:])
    return "\n".join(body)


try:
    STEP = validate_step()
except StopIteration:
    STEP = ""
check("the workflow still has the validate step this runs", bool(STEP.strip()))
if not STEP.strip():
    print(f"[release-version] {passed}/{passed + failed} passed")
    sys.exit(1)


def validate(tag, latest_tags="auto"):
    """The validate step itself, run on a tag: (accepted, step outputs)."""
    with tempfile.TemporaryDirectory() as tmp:
        out = os.path.join(tmp, "github_output")
        open(out, "w").close()
        proc = subprocess.run(
            ["sh", "-c", STEP], capture_output=True, text=True, timeout=60,
            env=dict(os.environ, TAG=tag, LATEST_TAGS=latest_tags, GITHUB_OUTPUT=out))
        outputs = dict(ln.split("=", 1) for ln in open(out).read().splitlines() if "=" in ln)
    return proc.returncode == 0, outputs


def native(function, version):
    """A translation from infra/packaging/version.sh, run on a version."""
    return sh(f'. "$1"; {function} "$2"', VERSION_SH, version).stdout.strip()


# tag -> whether the workflow takes it, and whether it is a pre-release
TAGS = {
    "1.2.3": (True, False),
    "v1.2.3": (True, False),
    "2.0.0rc0": (True, True),
    "2.0.0a1": (True, True),
    "2.0.0b2": (True, True),
    "0.0.0.dev0": (True, True),
    # A post-release is a release: it supersedes the version it names
    "2.0.0.post1": (True, False),
    # Valid PEP 440, but pip normalizes each of these to a different string
    "2.0.0-rc.0": (False, False),
    "1.2.3beta1": (False, False),
    "2.0.0RC0": (False, False),
    "2.0.0rc": (False, False),
    # Not the three release components a selkies version carries
    "2.0": (False, False),
    "2.0.0.0": (False, False),
    "": (False, False),
}
for tag, (want_ok, want_pre) in TAGS.items():
    ok, out = validate(tag)
    check(f"{tag or '<empty>'} is {'accepted' if want_ok else 'rejected'}",
          ok == want_ok, "" if ok == want_ok else f"accepted={ok}")
    if want_ok:
        want = "true" if want_pre else "false"
        check(f"{tag} is {'a pre-release' if want_pre else 'a full release'}",
              out.get("prerelease") == want, out.get("prerelease", "<unset>"))
        check(f"{tag} is released as {tag}",
              out.get("version") == tag.lstrip("v"), out.get("version", "<unset>"))

# Releases take the floating `latest` image tags and pre-releases leave them
# alone, which the maintainer can override per run
LATEST = {
    ("1.2.3", "auto"): "true",
    ("2.0.0.post1", "auto"): "true",
    ("2.0.0rc0", "auto"): "false",
    ("0.0.0.dev0", "auto"): "false",
    ("2.0.0rc0", "always"): "true",
    ("1.2.3", "never"): "false",
}
for (tag, mode), want in LATEST.items():
    _, out = validate(tag, mode)
    check(f"{tag} with latest_tags={mode} {'moves' if want == 'true' else 'leaves'} the latest tags",
          out.get("floating_tags") == want, out.get("floating_tags", "<unset>"))

# version -> what dpkg and rpm are handed, and what apk is handed
NATIVE = {
    "2.0.0": ("2.0.0", "2.0.0"),
    "2.0.0rc0": ("2.0.0~rc0", "2.0.0_rc0"),
    "2.0.0a1": ("2.0.0~a1", "2.0.0_alpha1"),
    "2.0.0b2": ("2.0.0~b2", "2.0.0_beta2"),
    "2.0.0.post1": ("2.0.0.post1", "2.0.0_p1"),
    "0.0.0.dev0": ("0.0.0~dev0", "0.0.0_alpha0"),
}
for version, (want_tilde, want_alpine) in NATIVE.items():
    got = native("tilde_version", version)
    check(f"dpkg and rpm build {version} as {want_tilde}", got == want_tilde, got)
    got = native("alpine_version", version)
    check(f"apk builds {version} as {want_alpine}", got == want_alpine, got)

# The point of the translation, on the one comparator a runner always has
if shutil.which("dpkg"):
    for older, newer in (("2.0.0~rc0", "2.0.0"), ("1.9.9", "2.0.0~rc0"),
                         ("2.0.0~a1", "2.0.0~rc0"), ("2.0.0", "2.0.0.post1")):
        ok = subprocess.run(["dpkg", "--compare-versions", older, "lt", newer]).returncode == 0
        check(f"dpkg sorts {older} below {newer}", ok)
else:
    print("note: no dpkg, leaving the package ordering to the string checks above")

print(f"[release-version] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
