# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

import os
import re
import sys

from setuptools import setup
from setuptools_rust import Binding, RustExtension, Strip

with open("README.md", "r", encoding="utf-8") as fh:
    long_description = fh.read()


def crate_version() -> str:
    """The crate's version as pip spells it: a `2.1.0-rc.1` in Cargo.toml is `2.1.0rc1` here."""
    manifest = os.path.join(os.path.dirname(os.path.abspath(__file__)), "Cargo.toml")
    with open(manifest, encoding="utf-8") as fh:
        semver = re.search(r'^version = "([^"]+)"', fh.read(), re.M).group(1)
    spelled = {"alpha": "a", "beta": "b", "rc": "rc", "dev": ".dev", "post": ".post"}
    return re.sub(r"-(alpha|beta|rc|dev|post)\.(\d+)$", lambda m: spelled[m.group(1)] + m.group(2), semver)


# Release wheels opt in explicitly. Ordinary source builds retain the full CPython API,
# including the zero-copy buffer exporter on CPython 3.9/3.10.
_abi3 = os.environ.get("PCMFLUX_ABI3", "0").strip().lower() in ("1", "true", "yes", "on")
if _abi3 and sys.version_info < (3, 11):
    raise RuntimeError("PCMFLUX_ABI3=1 requires CPython 3.11 or newer")


setup(
    name="pcmflux",
    version=crate_version(),
    author="Selkies Project",
    author_email="pypi@linuxserver.io",
    description="A performant audio capture pipeline that encodes raw PCM to Opus, skipping silence.",
    long_description=long_description,
    long_description_content_type="text/markdown",
    license="MPL-2.0",
    url="https://github.com/selkies-project/selkies/tree/main/crates/pcmflux",
    packages=[],
    # The `abi3-py311` Cargo feature and wheel tag are paired only in release mode.
    options={"bdist_wheel": {"py_limited_api": "cp311"}} if _abi3 else {},
    rust_extensions=[
        RustExtension(
            "pcmflux",
            "Cargo.toml",
            binding=Binding.PyO3,
            features=["python", "abi3-extension-module" if _abi3 else "extension-module"],
            debug=False,
            strip=Strip.All,
            # `--locked`: a wheel carries the crate versions Cargo.lock names, and a
            # manifest that has outrun the lock fails the build rather than silently
            # resolving past it.
            args=["--locked"],
        )
    ],
    classifiers=[
        "Programming Language :: Python :: 3",
        "Operating System :: POSIX :: Linux",
    ],
    python_requires=">=3.9",
    zip_safe=False,
)
