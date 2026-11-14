# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Repackage a Selkies bundle as the selkies wheel for its platform.

The bundle (infra/bundle/build.sh) is already everything the wheel installs, so
nothing is compiled or repaired here: its `bin/` becomes the wheel's scripts,
which pip puts on the environment's PATH, and its `lib/`, `libexec/`, and
`share/` become the wheel's data, which pip installs under the same prefix.
That keeps the tree as the bundle lays it out -- `<prefix>/bin/selkies` beside
`<prefix>/libexec/selkies` -- so the executable finds its payload and its
libraries exactly as it does unpacked from the bundle, in a virtualenv, a
`--user` install, or a system prefix alike. The one Python module, the Jupyter
entry point, comes from infra/wheel/selkies, and the metadata from
pyproject.toml.

The platform tag is the bundle's C library floor, the platform of the image
it is built in, which infra/bundle/toolchain.sh reads from where
infra/bundle/Dockerfile pins it: manylinux_2_17 for a glibc bundle on x86_64,
manylinux_2_28 on aarch64, and musllinux_1_2 for a musl one; the wheel is
py3-none, since no part of it is bound to an interpreter. Symbolic links,
which a wheel cannot carry, are stored as the files they name.

Usage: build.py <bundle.tar.gz> <out-dir>
"""

import base64
import hashlib
import re
import stat
import subprocess
import sys
import tarfile
import tempfile
import zipfile
from pathlib import Path
from typing import Dict, List, Tuple

if sys.version_info >= (3, 11):
    import tomllib
else:
    sys.exit("build.py needs Python 3.11 or newer, for tomllib")

REPO = Path(__file__).resolve().parents[2]
BUNDLE_NAME = re.compile(r"^selkies-(?P<version>.+)-(?P<arch>x86_64|aarch64)-linux-(?P<libc>gnu|musl)$")
DATA_DIRS = ("lib", "libexec", "share")


def platform_tag(libc: str, arch: str) -> str:
    """The wheel platform tag of a bundle built for `libc` on `arch`."""
    toolchain = REPO / "infra/bundle/toolchain.sh"
    toolchain_libc = {"gnu": "glibc", "musl": "musl"}[libc]
    platform = subprocess.run(
        ["sh", str(toolchain), "platform", toolchain_libc, arch], check=True, capture_output=True, text=True
    ).stdout.strip()
    return f"{platform}_{arch}"


def metadata(version: str) -> str:
    """The core metadata (version 2.4) of the selkies distribution, from pyproject.toml."""
    project = tomllib.loads((REPO / "pyproject.toml").read_text())["project"]
    lines = [
        "Metadata-Version: 2.4",
        f"Name: {project['name']}",
        f"Version: {version}",
        f"Summary: {project['description']}",
        f"License-Expression: {project['license']}",
        "License-File: LICENSE",
    ]
    for author in project.get("authors", []):
        lines.append(f"Author-email: {author['name']} <{author['email']}>")
    for classifier in project.get("classifiers", []):
        lines.append(f"Classifier: {classifier}")
    for label, url in project.get("urls", {}).items():
        lines.append(f"Project-URL: {label}, {url}")
    for extra, requirements in project.get("optional-dependencies", {}).items():
        lines.append(f"Provides-Extra: {extra}")
        for requirement in requirements:
            lines.append(f"Requires-Dist: {requirement}; extra == \"{extra}\"")
    lines.append("Description-Content-Type: text/markdown")
    readme = (REPO / project["readme"]).read_text()
    return "\n".join(lines) + "\n\n" + readme


def entry_points() -> str:
    """The entry point groups pyproject.toml declares, as entry_points.txt."""
    groups = tomllib.loads((REPO / "pyproject.toml").read_text())["project"].get("entry-points", {})
    text = ""
    for group, entries in groups.items():
        text += f"[{group}]\n" + "".join(f"{name} = {target}\n" for name, target in entries.items()) + "\n"
    return text


def bundle_files(root: Path) -> List[Tuple[str, bytes, int]]:
    """Every file of the bundle unpacked at `root` as (path under it, content, mode), links followed."""
    files = []
    for path in sorted(root.rglob("*")):
        if path.is_dir():
            continue
        if not path.resolve().is_relative_to(root.resolve()):
            raise SystemExit(f"{path}: a link out of the bundle")
        files.append((path.relative_to(root).as_posix(), path.read_bytes(), path.stat().st_mode))
    return files


def record_line(path: str, data: bytes) -> str:
    """The RECORD line of `path` holding `data`."""
    digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()
    return f"{path},sha256={digest},{len(data)}"


def build(bundle: Path, out: Path) -> Path:
    """Write the wheel of `bundle` into `out` and return its path."""
    name = bundle.name
    for suffix in (".tar.gz", ".tgz"):
        if name.endswith(suffix):
            name = name[: -len(suffix)]
    match = BUNDLE_NAME.match(name)
    if not match:
        raise SystemExit(f"{bundle.name}: not a bundle named selkies-<version>-<arch>-linux-<gnu|musl>")
    version, arch, libc = match["version"], match["arch"], match["libc"]
    tag = f"py3-none-{platform_tag(libc, arch)}"
    dist = f"selkies-{version}"
    data_dir = f"{dist}.data"
    info_dir = f"{dist}.dist-info"
    wheel = out / f"selkies-{version}-{tag}.whl"
    out.mkdir(parents=True, exist_ok=True)

    entries: Dict[str, Tuple[bytes, int]] = {}
    for module in sorted((REPO / "infra/wheel/selkies").glob("*.py")):
        entries[f"selkies/{module.name}"] = (module.read_bytes(), 0o644)
    with tempfile.TemporaryDirectory() as scratch, tarfile.open(bundle) as archive:
        archive.extractall(scratch, filter="data")
        for path, data, mode in bundle_files(Path(scratch) / name):
            top, _, rest = path.partition("/")
            if top == "bin":
                entries[f"{data_dir}/scripts/{rest}"] = (data, mode)
            elif top in DATA_DIRS:
                entries[f"{data_dir}/data/{path}"] = (data, mode)
            else:
                raise SystemExit(f"{path}: outside the directories a wheel installs")
    if f"{data_dir}/scripts/selkies" not in entries:
        raise SystemExit(f"{bundle.name}: no bin/selkies")
    entries[f"{info_dir}/METADATA"] = (metadata(version).encode(), 0o644)
    entries[f"{info_dir}/WHEEL"] = (
        f"Wheel-Version: 1.0\nGenerator: selkies infra/wheel/build.py\nRoot-Is-Purelib: false\nTag: {tag}\n".encode(),
        0o644,
    )
    entries[f"{info_dir}/entry_points.txt"] = (entry_points().encode(), 0o644)
    entries[f"{info_dir}/licenses/LICENSE"] = ((REPO / "LICENSE").read_bytes(), 0o644)

    record = [record_line(path, data) for path, (data, _) in entries.items()]
    record.append(f"{info_dir}/RECORD,,")
    entries[f"{info_dir}/RECORD"] = (("\n".join(record) + "\n").encode(), 0o644)

    with zipfile.ZipFile(wheel, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as zf:
        for path, (data, mode) in entries.items():
            info = zipfile.ZipInfo(path, date_time=(2020, 1, 1, 0, 0, 0))
            info.external_attr = (stat.S_IFREG | (mode & 0o777)) << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            zf.writestr(info, data)
    return wheel


def main() -> None:
    """Build the wheel the command line names."""
    if len(sys.argv) != 3:
        raise SystemExit("usage: build.py <bundle.tar.gz> <out-dir>")
    print(build(Path(sys.argv[1]), Path(sys.argv[2])))


if __name__ == "__main__":
    main()
