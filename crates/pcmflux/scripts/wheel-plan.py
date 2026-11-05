#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Plan of the wheel build the ``Wheels`` workflow runs.

``matrix IMAGE JOBS`` prints the two job matrices as ``name=json`` lines for
``$GITHUB_OUTPUT``. ``groups`` deals the wheels the installed cibuildwheel
selects from ``pyproject.toml`` into jobs, each platform's into as many as
JOBS (``manylinux=2 musllinux=4``) gives its libc. ``images`` lists the
platforms whose image the Actions cache holds no copy of that this run can
restore, each naming the base image that cibuildwheel release pins for it. Both
carry the cache key of the platform's image, a hash of its recipe, its base,
the variables passed into the build and the ISO week, so an image is rebuilt
when any of them changes and toolchain and package updates reach the wheels
within a week.

``recipe`` prints the ``before-all`` of ``pyproject.toml`` as a script, under
the environment cibuildwheel would run it in, so an image carrying the native
dependencies is built by the same recipe a plain cibuildwheel run uses.
"""

import configparser
import datetime
import hashlib
import json
import os
import subprocess
import sys
import urllib.parse
import urllib.request
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parent.parent
RUNNERS = {"x86_64": "ubuntu-latest", "aarch64": "ubuntu-24.04-arm"}
DEFAULT_IMAGES = {"manylinux": "manylinux_2_28", "musllinux": "musllinux_1_2"}


def config():
    with open(ROOT / "pyproject.toml", "rb") as f:
        return tomllib.load(f)["tool"]["cibuildwheel"]


def recipe():
    settings = config()
    if any("before-all" in override for override in settings.get("overrides", [])):
        sys.exit("an override's before-all would be left out of the image")
    exports = "".join(f'export {name}="{value}"\n' for name, value in settings.get("environment", {}).items())
    return exports + settings["linux"]["before-all"] + "\n"


def cached(keys):
    """The keys of ``keys`` the Actions cache holds an entry under that this run can restore."""
    api, repo, token = (os.environ.get(name) for name in ("GITHUB_API_URL", "GITHUB_REPOSITORY", "GH_TOKEN"))
    if not (api and repo and token):
        return set()
    refs = {os.environ.get("GITHUB_REF"), os.environ.get("DEFAULT_REF")}
    found = set()
    for key in keys:
        request = urllib.request.Request(
            f"{api}/repos/{repo}/actions/caches?per_page=100&key={urllib.parse.quote(key)}",
            headers={"Authorization": f"Bearer {token}", "Accept": "application/vnd.github+json"},
        )
        with urllib.request.urlopen(request, timeout=30) as response:
            entries = json.load(response)["actions_caches"]
        if any(entry["key"] == key and entry["ref"] in refs for entry in entries):
            found.add(key)
    return found


def matrix(image, jobs):
    import cibuildwheel

    pins = configparser.ConfigParser()
    pins.read(Path(cibuildwheel.__file__).parent / "resources" / "pinned_docker_images.cfg")
    settings = config()
    jobs = {libc: int(count) for libc, count in (pair.split("=") for pair in jobs.split())}
    identifiers = subprocess.run(
        [sys.executable, "-m", "cibuildwheel", "--print-build-identifiers", "--platform", "linux", str(ROOT)],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.split()
    if not identifiers:
        sys.exit("cibuildwheel selected no wheel")
    year, week, _ = datetime.date.today().isocalendar()
    passed = {name: os.environ.get(name) for name in settings.get("linux", {}).get("environment-pass", [])}
    script = recipe()
    platforms = {}
    for identifier in identifiers:
        platforms.setdefault(identifier.split("-", 1)[1], []).append(identifier)
    images, groups = [], []
    for platform, members in platforms.items():
        libc, arch = platform.split("_", 1)
        name = settings.get(f"{libc}-{arch}-image", DEFAULT_IMAGES[libc])
        base = pins[arch].get(name, name).split("#")[0].strip()
        digest = hashlib.sha256(json.dumps([script, base, passed, f"{year}-W{week:02d}"]).encode()).hexdigest()
        key = f"{image}-{platform}-{digest[:16]}"
        images.append({"platform": platform, "base": base, "runner": RUNNERS[arch], "key": key})
        count = max(1, min(jobs.get(libc, 1), len(members)))
        for n in range(count):
            share = members[n * len(members) // count : (n + 1) * len(members) // count]
            first, last = share[0].split("-", 1)[0], share[-1].split("-", 1)[0]
            groups.append({
                "name": share[0] if len(share) == 1 else f"{first}-{last}-{platform}",
                "build": " ".join(share),
                "platform": platform,
                "runner": RUNNERS[arch],
                "key": key,
            })
    held = cached([i["key"] for i in images])
    print(f"groups={json.dumps(groups)}")
    print(f"images={json.dumps([i for i in images if i['key'] not in held])}")


if __name__ == "__main__":
    if sys.argv[1] == "recipe":
        print(recipe(), end="")
    else:
        matrix(*sys.argv[2:4])
