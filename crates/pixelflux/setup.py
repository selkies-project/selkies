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


# Release wheels opt in explicitly. An ordinary source build remains a full-API
# extension, including on CPython 3.9/3.10 where the native buffer protocol predates
# the limited API's buffer support.
_abi3 = os.environ.get("PIXELFLUX_ABI3", "0").strip().lower() in ("1", "true", "yes", "on")
if _abi3 and sys.version_info < (3, 11):
    raise RuntimeError("PIXELFLUX_ABI3=1 requires CPython 3.11 or newer")


# The software H.264 and H.265 encoders are chosen at build time. The default build enables
# the GPL components (GPL-2.0+ libx264 and x265 encode every software H.264 and H.265
# session). Set PIXELFLUX_ENABLE_GPL=0 (or "false"/"no") to build without them: the
# BSD-licensed OpenH264 and kvazaar then encode those sessions, with no other difference
# in the API (pixelflux.SOFTWARE_ENCODERS reports which encoder a build carries per codec).
_enable_gpl = os.environ.get("PIXELFLUX_ENABLE_GPL", "1").strip().lower() not in (
    "0",
    "false",
    "no",
    "off",
)
if _enable_gpl:
    print(
        "NOTICE: pixelflux is being built WITH GPL-licensed components "
        "(GPL-2.0+ libx264 and x265 as the software H.264 and H.265 encoders), which is the "
        "default. Set PIXELFLUX_ENABLE_GPL=0 to exclude every GPL-licensed component.",
        file=sys.stderr,
    )
else:
    print(
        "NOTICE: pixelflux is being built WITHOUT GPL-licensed components "
        "(PIXELFLUX_ENABLE_GPL=0): libx264 and x265 are excluded, and OpenH264 and kvazaar "
        "(BSD) are the software H.264 and H.265 encoders.",
        file=sys.stderr,
    )

setup(
    name="pixelflux",
    version=crate_version(),
    author="Selkies Project",
    author_email="pypi@linuxserver.io",
    description="A performant web native pixel delivery pipeline for diverse sources, blending VNC-inspired parallel processing of pixel buffers with flexible modern encoding formats.",
    long_description=long_description,
    long_description_content_type="text/markdown",
    license="MPL-2.0",
    url="https://github.com/selkies-project/selkies/tree/main/crates/pixelflux",

    # Single self-contained Rust extension: the top-level `pixelflux` module does X11 (XShm)
    # and Wayland capture plus all encoding/conversion. No C/C++ sources, no Python package
    # layer -- import resolves directly to the extension. ABI3 mode builds pixelflux.abi3.so
    # for GIL-enabled CPython 3.11+; ordinary source builds carry the interpreter's own tag.
    packages=[],
    options={"bdist_wheel": {"py_limited_api": "cp311"}} if _abi3 else {},
    rust_extensions=[
        RustExtension(
            "pixelflux",
            "Cargo.toml",
            binding=Binding.PyO3,
            debug=False,
            strip=Strip.All,
            # `--locked`: a wheel carries the crate versions Cargo.lock names, and a
            # manifest that has outrun the lock fails the build rather than silently
            # resolving past it.
            features=["python", "abi3-extension-module" if _abi3 else "extension-module"],
            args=["--locked", *([] if _enable_gpl else ["--no-default-features", "--features", "openh264"])],
        )
    ],

    classifiers=[
        "Programming Language :: Python :: 3",
        "Operating System :: POSIX :: Linux",
    ],
    python_requires=">=3.9",
    zip_safe=False,
)
