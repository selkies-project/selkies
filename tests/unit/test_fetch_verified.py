#!/usr/bin/env python3
"""A download fetch-verified hands on is the one its source's hash names.

addons/base/fetch-verified lets a build take whichever version is current (the
newest release, a branch's head, a vendor's key file) and still refuse what
does not match: a release asset against the SHA-256 GitHub records for it, a
file at a commit against the blob hash GitHub's API names, an OpenPGP key
against the fingerprints it is meant to carry. The network is stubbed here, so
each case is the comparison alone: what matches is written and what does not
is removed with the command failing. The key case parses Mozilla's apt signing
key, whose fingerprint Mozilla publishes, so the parser is held to a number it
did not compute itself.

Also held here are the places that rely on it: the base image installs it, and
the desktop image and the AppImage build go through it.
"""
import contextlib
import hashlib
import importlib.machinery
import importlib.util
import io
import json
import os
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
HELPER = os.path.join(REPO, "addons", "base", "fetch-verified")

# packages.mozilla.org/apt/repo-signing-key.gpg, fingerprint
# 35BA A0B3 3E9E B396 F59C A838 C0BA 5CE6 DC63 15A3 as Mozilla documents it
MOZILLA_KEY = b"""-----BEGIN PGP PUBLIC KEY BLOCK-----

xsBNBGCRt7MBCADkYJHHQQoL6tKrW/LbmfR9ljz7ib2aWno4JO3VKQvLwjyUMPpq
/SXXMOnx8jXwgWizpPxQYDRJ0SQXS9ULJ1hXRL/OgMnZAYvYDeV2jBnKsAIEdiG/
e1qm8P4W9qpWJc+hNq7FOT13RzGWRx57SdLWSXo0KeY38r9lvjjOmT/cuOcmjwlD
T9XYf/RSO+yJ/AsyMdAr+ZbDeQUd9HYJiPdI04lGaGM02MjDMnx+monc+y54t+Z+
ry1WtQdzoQt9dHlIPlV1tR+xV5DHHsejCZxu9TWzzSlL5wfBBeEz7R/OIzivGJpW
QdJzd+2QDXSRg9q2XYWP5ZVtSgjVVJjNlb6ZABEBAAHNVEFydGlmYWN0IFJlZ2lz
dHJ5IFJlcG9zaXRvcnkgU2lnbmVyIDxhcnRpZmFjdC1yZWdpc3RyeS1yZXBvc2l0
b3J5LXNpZ25lckBnb29nbGUuY29tPsLAjgQTAQoAOBYhBDW6oLM+nrOW9ZyoOMC6
XObcYxWjBQJgkbezAhsDBQsJCAcCBhUKCQgLAgQWAgMBAh4BAheAAAoJEMC6XObc
YxWj+igIAMFh6DrAYMeq9sbZ1ZG6oAMrinUheGQbEqe76nIDQNsZnhDwZ2wWqgVC
7DgOMqlhQmOmzm7M6Nzmq2dvPwq3xC2OeI9fQyzjT72deBTzLP7PJok9PJFOMdLf
ILSsUnmMsheQt4DUO0jYAX2KUuWOIXXJaZ319QyoRNBPYa5qz7qXS7wHLOY89IDq
fHt6Aud8ER5zhyOyhytcYMeaGC1g1IKWmgewnhEq02FantMJGlmmFi2eA0EPD02G
C3742QGqRxLwjWsm5/TpyuU24EYKRGCRm7QdVIo3ugFSetKrn0byOxWGBvtu4fH8
XWvZkRT+u+yzH1s5yFYBqc2JTrrJvRU=
=QnvN
-----END PGP PUBLIC KEY BLOCK-----
"""
MOZILLA_FINGERPRINT = "35BAA0B33E9EB396F59CA838C0BA5CE6DC6315A3"

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    passed, failed = passed + int(ok), failed + int(not ok)
    print(f"{'PASS' if ok else 'FAIL'}  [fetch-verified] {label}  {detail}", flush=True)


def load():
    loader = importlib.machinery.SourceFileLoader("fetch_verified", HELPER)
    spec = importlib.util.spec_from_loader("fetch_verified", loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


def run(fv, responses: dict, *args):
    """fetch-verified's main on `args` with `responses` (URL to bytes) for the
    network: its exit message (None where it succeeded) and what it printed."""
    def get(url, api=False):
        if url not in responses:
            raise SystemExit(f"fetch-verified: {url}: HTTP 404")
        return responses[url]
    fv.get = get
    out = io.StringIO()
    try:
        with contextlib.redirect_stdout(out):
            fv.main(["fetch-verified", *args])
    except SystemExit as e:
        return str(e.code), out.getvalue()
    return None, out.getvalue()


def release_responses(body: bytes, digest: str) -> dict:
    api = "https://api.github.com/repos/o/r/releases"
    url = "https://github.com/o/r/releases/download/v1.2.3/tool-1.2.3.tar.gz"
    info = {"tag_name": "v1.2.3",
            "assets": [{"name": "tool-1.2.3.tar.gz", "digest": digest, "browser_download_url": url}]}
    return {f"{api}/latest": json.dumps(info).encode(), f"{api}/tags/v1.2.3": json.dumps(info).encode(), url: body}


def file_responses(body: bytes, served: bytes) -> dict:
    commit = "c" * 40
    blob = hashlib.sha1(b"blob %d\0" % len(body) + body).hexdigest()
    api = "https://api.github.com/repos/o/r"
    return {f"{api}/commits/main": json.dumps({"sha": commit}).encode(),
            f"{api}/contents/dir/f.sh?ref={commit}": json.dumps({"sha": blob}).encode(),
            f"https://raw.githubusercontent.com/o/r/{commit}/dir/f.sh": served}


def main() -> int:
    fv = load()
    body = b"release asset\n"
    good = "sha256:" + hashlib.sha256(body).hexdigest()
    with tempfile.TemporaryDirectory() as tmp:
        out = os.path.join(tmp, "asset")
        err, printed = run(fv, release_responses(body, good), "release", "o/r", "latest", "tool-{version}.tar.gz", out)
        check("a release asset that hashes to its digest is written, and its tag printed",
              err is None and printed.strip() == "v1.2.3" and open(out, "rb").read() == body, (err or printed).strip())
        err, _ = run(fv, release_responses(body, good), "release", "o/r", "v1.2.3", "tool-1.2.3.tar.gz", out)
        check("a release named by its tag is looked up by that tag", err is None, err)

        open(out, "wb").write(b"stale")
        err, _ = run(fv, release_responses(body + b"x", good), "release", "o/r", "latest", "tool-{version}.tar.gz", out)
        check("an asset that does not hash to its digest fails and leaves no file",
              err is not None and "hashes to" in err and not os.path.exists(out), err)
        err, _ = run(fv, release_responses(body, None), "release", "o/r", "latest", "tool-{version}.tar.gz", out)
        check("an asset with no recorded digest is refused",
              err is not None and "no SHA-256" in err and not os.path.exists(out), err)
        err, _ = run(fv, release_responses(body, good), "release", "o/r", "latest", "other.tar.gz", out)
        check("an asset the release does not carry is refused", err is not None and "has no asset" in err, err)

        script = b"#!/bin/sh\necho verified\n"
        err, printed = run(fv, file_responses(script, script), "file", "o/r", "main", "dir/f.sh", out)
        check("a file at a ref that matches its blob hash is written, and the commit printed",
              err is None and printed.strip() == "c" * 40 and open(out, "rb").read() == script, (err or printed).strip())
        err, _ = run(fv, file_responses(script, script + b"# appended\n"), "file", "o/r", "main", "dir/f.sh", out)
        check("a file whose blob hash differs fails and leaves no file",
              err is not None and "hashes to blob" in err and not os.path.exists(out), err)

        url = "https://example.invalid/key.asc"
        err, _ = run(fv, {url: MOZILLA_KEY}, "key", url, MOZILLA_FINGERPRINT, out)
        check("Mozilla's key parses to the fingerprint Mozilla publishes",
              err is None and open(out, "rb").read() == MOZILLA_KEY, err)
        spaced = " ".join(MOZILLA_FINGERPRINT[i:i + 4] for i in range(0, 40, 4)).lower()
        err, _ = run(fv, {url: MOZILLA_KEY}, "key", url, f"{'0' * 40},{spaced}", out)
        check("a fingerprint is matched spaced, lower-case, and among others", err is None, err)
        err, _ = run(fv, {url: MOZILLA_KEY}, "key", url, "0" * 40, out)
        check("a key that is not the one named fails and leaves no file",
              err is not None and MOZILLA_FINGERPRINT in err and not os.path.exists(out), err)
        err, _ = run(fv, {url: b"<html>not a key</html>"}, "key", url, MOZILLA_FINGERPRINT, out)
        check("a page that is no key is refused", err is not None and not os.path.exists(out), err)

        err, _ = run(fv, {}, "file", "o/r", "main", "dir/f.sh")
        check("a path without its output is a usage error",
              err is not None and "fetch-verified release" in err, (err or "").splitlines()[0])

    base = open(os.path.join(REPO, "addons", "base", "Dockerfile"), encoding="utf-8").read()
    check("the base image installs it",
          "COPY addons/base/fetch-verified /usr/local/bin/fetch-verified" in base, "addons/base/Dockerfile")
    desktop = open(os.path.join(REPO, "addons", "desktop", "Dockerfile"), encoding="utf-8").read()
    check("the desktop image checks its downloads with it",
          desktop.count("fetch-verified key ") == 2 and "fetch-verified release linuxserver/proot-apps" in desktop
          and "dl.google.com/linux/direct" not in desktop, "addons/desktop/Dockerfile")
    appimage = open(os.path.join(REPO, "scripts", "ci", "appimage.sh"), encoding="utf-8").read()
    check("the AppImage build stages it beside the conda plugin",
          'cp addons/base/fetch-verified "${WORK}/fetch-verified"' in appimage, "scripts/ci/appimage.sh")

    print(f"[fetch-verified] {passed}/{passed + failed} passed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
