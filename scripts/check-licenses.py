#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""License gate for what the Selkies server links and the web client ships.

Resolves the crate graph of each build configuration with ``cargo metadata`` (normal
dependencies only, the ones compiled into the executable, over the four Linux targets
the releases build), ranks every crate's SPDX license expression, and overlays the native
libraries the bindings link, load, or vendor, which crate metadata cannot see
(``x264-sys`` is an MIT binding whose only library is the GPL-2.0-or-later libx264). The
npm side reads the license each package of the web packages' lockfiles records, so it needs no
``npm ci``. It fails on a copyleft component in a configuration not allowed to carry it,
on a crate or package without usable license metadata, on a native binding, a direct
dependency, or a vendored crate this file does not describe, and on a
``docs/licensing.md`` whose generated sections differ from what the graphs yield.

    python3 scripts/check-licenses.py                  # audit both configurations and the page
    python3 scripts/check-licenses.py --write          # regenerate the page's sections
    python3 scripts/check-licenses.py --markdown       # print the sections instead
    python3 scripts/check-licenses.py --set non-gpl    # audit one configuration alone
    python3 scripts/check-licenses.py --metadata m.json --set gpl

The ``gpl`` configuration is the workspace as committed, pixelflux with its ``gpl``
feature, which every release ships. ``non-gpl`` is the build an operator gets by naming
pixelflux's ``openh264`` feature in its place: Selkies has no feature of its own for the
switch, so the script resolves a copy of the workspace manifest with that one feature
swapped, beside a copy of the committed ``Cargo.lock`` and the real ``crates/``
directory. Cargo drops packages the copied configuration no longer selects; any package
or version the copy gains beyond the committed lockfile fails the check.

The page sections sit between ``check-licenses:begin`` and ``check-licenses:end``
markers. They name crates and licenses, never counts or versions beside a vendored
crate's, and name a crate of the wider graph only where MIT or Apache-2.0 alone is not
among its choices, so a lockfile bump changes the page only where a license changes, or
a direct dependency, a native binding, or a crate under other terms comes or goes. A
page check needs both configurations resolved by cargo, so ``--set`` and ``--metadata``
audit the graph alone.
"""

import argparse
import difflib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from typing import Callable, Dict, FrozenSet, Iterable, List, NamedTuple, Optional, Sequence, Set, Tuple, Union

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MANIFEST = os.path.join(REPO, "Cargo.toml")
LOCKFILE = os.path.join(REPO, "Cargo.lock")
PAGE = os.path.join(REPO, "docs", "licensing.md")
NPM_ADDONS = ("web/selkies-web-core", "web/selkies-dashboard")

# The Linux targets the releases build: the glibc and musl executables and wheels on
# x86_64 and aarch64. The graph is their union, so the page reads the same on any host.
TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
)
SETS = ("gpl", "non-gpl")
SET_LABELS = {"gpl": "GPL", "non-gpl": "non-GPL"}

# Crates whose manifest carries no `license` field; deny.toml clarifies the same ones.
CLARIFY: Dict[str, str] = {}

# Copyleft components and the configurations allowed to contain them. Anything copyleft
# (or LGPL, which a statically linked crate cannot honor) that is not listed here fails
# in every configuration, so a GPL dependency is added here and in docs/licensing.md on
# purpose.
ALLOWED_COPYLEFT: Dict[str, Tuple[str, ...]] = {
    "x264-sys": ("gpl",),
    "codec-sys": ("gpl",),
}

# License categories, ordered so the worst of an AND is the larger one. File-level
# copyleft (MPL-2.0) is accepted wherever permissive terms are: a modified MPL file stays
# MPL, the larger work around it may be under any license, and Selkies itself is MPL-2.0.
PERMISSIVE = 0
FILE_LEVEL = 1
WEAK = 2
COPYLEFT = 3
UNKNOWN = 4
LABELS = {
    PERMISSIVE: "permissive",
    FILE_LEVEL: "weak copyleft",
    WEAK: "weak copyleft",
    COPYLEFT: "copyleft",
    UNKNOWN: "unknown",
}

PERMISSIVE_IDS = {
    "MIT", "MIT-0", "Apache-2.0", "BSD-1-Clause", "BSD-2-Clause", "BSD-3-Clause", "0BSD", "ISC",
    "Zlib", "BSL-1.0", "CC0-1.0", "CC-BY-4.0", "Unlicense", "Unicode-3.0", "Unicode-DFS-2016",
    "WTFPL", "PSF-2.0", "Python-2.0", "NCSA", "OFL-1.1", "BlueOak-1.0.0", "IJG", "MIT-CMU", "X11",
    "OpenSSL", "Linux-syscall-note",
}
FILE_LEVEL_IDS = {"MPL-2.0", "MPL-1.1", "CDDL-1.0", "CDDL-1.1", "EPL-1.0", "EPL-2.0"}
WEAK_PREFIXES = ("LGPL-",)
COPYLEFT_PREFIXES = ("GPL-", "AGPL-", "SSPL-", "EUPL-", "OSL-", "CC-BY-SA-", "CC-BY-NC", "CECILL", "RPL-", "CPAL-")
# Exceptions that lift the copyleft of their base license for the linked work.
LINKING_EXCEPTIONS = {
    "GCC-exception-3.1", "GCC-exception-2.0", "Classpath-exception-2.0", "LLVM-exception",
    "Linux-syscall-note", "GPL-3.0-linking-exception", "LGPL-3.0-linking-exception",
}
# The terms most crates offer; an expression with one of them as a complete choice is
# listed on the page without naming its crates.
DEFAULT_TERMS = {"MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception"}


def classify_id(ident: str) -> int:
    """Rank one SPDX license identifier; an identifier not classified here ranks as unknown."""
    if ident in PERMISSIVE_IDS:
        return PERMISSIVE
    if ident in FILE_LEVEL_IDS:
        return FILE_LEVEL
    if ident.startswith(WEAK_PREFIXES):
        return WEAK
    if ident.startswith(COPYLEFT_PREFIXES):
        return COPYLEFT
    return UNKNOWN


class ExpressionError(ValueError):
    """An SPDX expression that does not parse."""


# A parsed expression: a license (`"MIT"`, `"Apache-2.0 WITH LLVM-exception"`) or an
# operator over its operands.
Expr = Union[str, Tuple[str, Tuple["Expr", ...]]]


def parse_expression(expression: str) -> Expr:
    """Parse an SPDX expression, the legacy ``/`` separator and a bare ``+`` suffix included.

    Operands of one operator are flattened, deduplicated, and sorted, so every spelling of
    an expression (``MIT/Apache-2.0``, ``Apache-2.0 OR MIT``) parses to the same value.
    """
    tokens = re.findall(r"\(|\)|[^\s()]+", expression.replace("/", " OR "))
    pos = 0

    def peek() -> Optional[str]:
        return tokens[pos] if pos < len(tokens) else None

    def take() -> str:
        nonlocal pos
        pos += 1
        return tokens[pos - 1]

    def combine(op: str, operands: List[Expr]) -> Expr:
        flat: Set[Expr] = set()
        for operand in operands:
            if isinstance(operand, tuple) and operand[0] == op:
                flat.update(operand[1])
            else:
                flat.add(operand)
        if len(flat) == 1:
            return flat.pop()
        return (op, tuple(sorted(flat, key=render_expression)))

    def parse_binary(op: str, operand: Callable[[], Expr]) -> Expr:
        operands = [operand()]
        while (peek() or "").upper() == op:
            take()
            operands.append(operand())
        return combine(op, operands)

    def parse_or() -> Expr:
        return parse_binary("OR", parse_and)

    def parse_and() -> Expr:
        return parse_binary("AND", parse_atom)

    def parse_atom() -> Expr:
        token = peek()
        if token is None:
            raise ExpressionError(f"unexpected end of {expression!r}")
        take()
        if token == "(":
            inner = parse_or()
            if peek() != ")":
                raise ExpressionError(f"unbalanced parentheses in {expression!r}")
            take()
            return inner
        if token.upper() in ("AND", "OR", "WITH") or token == ")":
            raise ExpressionError(f"unexpected {token!r} in {expression!r}")
        if token.endswith("+"):
            token = token[:-1] + "-or-later"
        if (peek() or "").upper() == "WITH":
            take()
            if peek() is None:
                raise ExpressionError(f"dangling WITH in {expression!r}")
            token = f"{token} WITH {take()}"
        return token

    if not tokens:
        raise ExpressionError("empty license expression")
    parsed = parse_or()
    if pos != len(tokens):
        raise ExpressionError(f"trailing tokens in {expression!r}")
    return parsed


def render_expression(expr: Expr, parent: Optional[str] = None) -> str:
    """Print a parsed expression, parenthesizing an operator nested in a different one."""
    if isinstance(expr, str):
        return expr
    op, operands = expr
    text = f" {op} ".join(render_expression(o, op) for o in operands)
    return f"({text})" if parent is not None and parent != op else text


def rank_expression(expr: Expr) -> int:
    """Rank a parsed expression: OR takes the licensee's best choice, AND the worst."""
    if isinstance(expr, str):
        base, _, exception = expr.partition(" WITH ")
        rank = classify_id(base)
        if exception and exception in LINKING_EXCEPTIONS and rank in (WEAK, COPYLEFT):
            return PERMISSIVE
        return rank
    op, operands = expr
    ranks = [rank_expression(o) for o in operands]
    return min(ranks) if op == "OR" else max(ranks)


def choices(expr: Expr) -> List[FrozenSet[str]]:
    """The sets of licenses one of which a licensee satisfies in full (the expression's DNF)."""
    if isinstance(expr, str):
        return [frozenset([expr])]
    op, operands = expr
    if op == "OR":
        return [c for o in operands for c in choices(o)]
    result = [frozenset()]
    for operand in operands:
        result = [a | b for a in result for b in choices(operand)]
    return result


def offers_default_terms(expr: Expr) -> bool:
    """Whether MIT or Apache-2.0 alone is one of the expression's choices."""
    return any(c <= DEFAULT_TERMS for c in choices(expr))


class Native(NamedTuple):
    """A native library behind a crate that binds, vendors, or loads it.

    ``rank`` is the category of the native code; ``copyleft_features`` the crate features
    that link a copyleft library in its place.
    """

    library: str
    license: str
    rank: int
    how: str
    copyleft_features: Tuple[str, ...] = ()


# Every crate named like a native binding (-sys, _sys, -ffi) has to be described here, and
# every entry has to be in a graph, so a new binding fails the check until it is
# inventoried and a dropped one until it is taken off the page.
NATIVE: Dict[str, Native] = {
    "x264-sys": Native(
        "libx264", "GPL-2.0-or-later", COPYLEFT,
        "linked shared library, through pixelflux's `gpl` feature; software H.264"),
    "codec-sys": Native(
        "libvpx, SVT-AV1, dav1d, and libde265 in every build; x265 in the GPL build, kvazaar in the non-GPL one",
        "BSD-3-Clause (libvpx, kvazaar; SVT-AV1 with the Alliance for Open Media patent license), "
        "BSD-2-Clause (dav1d), LGPL-3.0-or-later (libde265), GPL-2.0-or-later (x265)",
        WEAK,
        "linked shared libraries, through pixelflux; the software HEVC, VP8, VP9, and AV1 encoders and the "
        "virtual camera's decoders",
        ("x265",)),
    "openh264-sys2": Native(
        "Cisco OpenH264, and libstdc++ for its C++", "BSD-2-Clause (OpenH264), GPL-3.0-or-later WITH "
        "GCC-exception-3.1 (libstdc++)", PERMISSIVE,
        "compiled from the source vendored in the crate and linked statically, through pixelflux, libstdc++ as a "
        "shared library; full-frame software H.264 of the non-GPL build and the virtual camera's H.264 decoder"),
    "turbojpeg-sys": Native(
        "libjpeg-turbo", "IJG AND BSD-3-Clause AND Zlib", PERMISSIVE,
        "compiled from the source vendored in the crate and linked statically, through pixelflux; JPEG"),
    "va-sys": Native(
        "libva, libva-drm", "MIT", PERMISSIVE,
        "`dlopen` at run time through pixelflux, never linked; VA-API encoding"),
    "nvcodec-sys": Native(
        "NVENC, NvFBC, and the CUDA driver (`libnvidia-encode.so.1`, `libnvidia-fbc.so.1`, `libcuda.so.1`)",
        "proprietary (NVIDIA driver); the vendored headers MIT", PERMISSIVE,
        "`dlopen` at run time through pixelflux, never linked or shipped"),
    "gbm-sys": Native("libgbm (Mesa)", "MIT", PERMISSIVE, "linked shared library, through pixelflux"),
    "pixman-sys": Native(
        "libpixman-1", "MIT", PERMISSIVE,
        "linked shared library, through pixelflux's capture compositor"),
    "drm-sys": Native(
        "libdrm headers", "MIT", PERMISSIVE,
        "bindings generated from the headers, through pixelflux; no libdrm symbol is linked"),
    "drm-ffi": Native(
        "Linux DRM ioctls (no library)", "MIT", PERMISSIVE,
        "pure Rust ioctl wrappers, through pixelflux"),
    "wayland-sys": Native(
        "libwayland-server", "MIT", PERMISSIVE,
        "`dlopen` at run time through pixelflux's capture compositor, never linked"),
    "input-sys": Native(
        "libinput", "MIT", PERMISSIVE,
        "in the graph through smithay's libinput backend, which pixelflux does not use; nothing is linked"),
    "libudev-sys": Native(
        "libudev (systemd)", "LGPL-2.1-or-later", WEAK,
        "in the graph through smithay's udev backend, which pixelflux does not use; nothing is linked"),
    "xkbcommon": Native(
        "libxkbcommon", "MIT", PERMISSIVE,
        "linked shared library; keymaps, in the server and in pixelflux"),
    "libpulse-sys": Native(
        "libpulse", "LGPL-2.1-or-later", WEAK,
        "linked shared library, replaceable; the sound-server control plane (`libpulse-binding`) and "
        "pcmflux's capture"),
    "opusic-sys": Native(
        "libopus", "BSD-3-Clause", PERMISSIVE,
        "compiled from the source bundled in the crate and linked statically, through pcmflux; Opus"),
    "aws-lc-sys": Native(
        "AWS-LC", "ISC AND Apache-2.0 AND MIT AND BSD-3-Clause", PERMISSIVE,
        "compiled from the source vendored in the crate and linked statically; the HTTPS server's and "
        "outgoing requests' cryptography (`aws-lc-rs`)"),
    "nvml-wrapper-sys": Native(
        "NVML (`libnvidia-ml.so.1`)", "proprietary (NVIDIA driver)", PERMISSIVE,
        "`dlopen` at run time when present, never linked or shipped; GPU statistics"),
    "linux-raw-sys": Native(
        "Linux kernel ABI (no library)", "GPL-2.0-only WITH Linux-syscall-note (the kernel's UAPI headers)",
        PERMISSIVE, "syscall numbers and structures as Rust constants (rustix)"),
    "libc": Native(
        "C runtime: glibc and libgcc_s, or musl in the musl builds",
        "LGPL-2.1-or-later (glibc), GPL-3.0-or-later WITH GCC-exception-3.1 (libgcc_s), MIT (musl)", WEAK,
        "linked shared libraries (glibc, libgcc_s) or statically (musl), as for every program"),
    "libloading": Native(
        "the driver and desktop libraries above that are opened at run time", "MIT and proprietary", PERMISSIVE,
        "`dlopen`, never linked"),
}
NATIVE_NAME = re.compile(r"(-|_)sys\d*$|-ffi$")


class Role(NamedTuple):
    """What a group of the workspace's direct dependencies does in the server."""

    crates: Tuple[str, ...]
    how: str
    notes: str = ""


# Every direct dependency of the server crates, in the order the page lists them; a
# new one fails the check until it is placed here.
ROLES: Tuple[Role, ...] = (
    Role(("pixelflux",), "screen capture and video encoding, over the native libraries below",
         "see [pixelflux's LICENSES.md](https://github.com/selkies-project/selkies/blob/main/crates/pixelflux/LICENSES.md)"),
    Role(("pcmflux",), "audio capture and Opus encoding, over libpulse and libopus",
         "see [pcmflux's LICENSES.md](https://github.com/selkies-project/selkies/blob/main/crates/pcmflux/LICENSES.md)"),
    Role(("selkies-rtc",), "the WebRTC stack, in this repository under `crates/selkies-rtc`"),
    Role(("rtc-crypto", "rtc-dtls", "rtc-ice", "rtc-mdns", "rtc-sctp", "rtc-sdp", "rtc-shared", "rtc-srtp",
          "rtc-stun", "rtc-turn", "sansio"),
         "the [webrtc-rs](https://github.com/webrtc-rs/rtc) protocol crates `selkies-rtc` is built over",
         "`rtc-sctp` vendored (below)"),
    Role(("tokio", "tokio-util", "futures", "async-trait", "bytes"), "async runtime"),
    Role(("axum", "hyper", "hyper-util", "tower", "tower-http", "http", "http-body-util"),
         "HTTP and WebSocket server"),
    Role(("rustls", "tokio-rustls", "hyper-rustls", "rustls-native-certs"),
         "HTTPS, TLS to TURN servers, and the outgoing requests of the TURN REST and Cloudflare sources",
         "cryptography from `aws-lc-rs`"),
    Role(("rcgen",), "self-signed certificates"),
    Role(("aws-lc-rs", "rand"), "hashes, HMAC credentials, constant-time comparison, random identifiers"),
    Role(("x11rb",), "X11 input injection (XTEST), the clipboard (XFixes), and the display's outputs (RandR)",
         "speaks the X protocol itself, no libxcb"),
    Role(("xkbcommon",), "keymaps"),
    Role(("libpulse-binding",), "the sound-server control plane"),
    Role(("nvml-wrapper",), "GPU statistics"),
    Role(("sysinfo",), "system and process statistics"),
    Role(("prometheus-client",), "metrics endpoint"),
    Role(("image",), "clipboard images and cursors"),
    Role(("zip", "flate2"), "folder downloads, compressed messages"),
    Role(("serde", "serde_json", "rmp-serde", "base64", "percent-encoding", "url"),
         "JSON messages, MessagePack to the uinput mouse socket, encodings"),
    Role(("libc", "nix", "socket2"), "system calls: sockets, descriptors, signals, processes"),
    Role(("log", "thiserror", "parking_lot", "regex", "memchr", "mime_guess", "chrono", "shlex",
          "tempfile"),
         "logging, errors, locks, text, MIME types, time, and temporary files"),
)

# Crates vendored under crates/*/vendor and what Selkies does with each.
VENDORED: Dict[str, str] = {
    "rtc-sctp": "the workspace patches the published crate with it; `rtc-sctp-selkies.patch` beside it is the "
                "diff, whose changes are MPL-2.0",
}


class Crate(NamedTuple):
    """One crate of a configuration's graph, with its license as parsed."""

    name: str
    version: str
    license: str
    expr: Optional[Expr]
    rank: int
    native_rank: int
    manifest: str


def cargo_metadata(manifest: str, locked: bool = True, no_default_features: bool = False) -> dict:
    """Resolve the graph of ``manifest`` over the release targets from the lockfile beside it."""
    cmd = ["cargo", "metadata", "--format-version", "1", "--manifest-path", manifest]
    if locked:
        cmd.append("--locked")
    if no_default_features:
        cmd.append("--no-default-features")
    for target in TARGETS:
        cmd += ["--filter-platform", target]
    try:
        out = subprocess.run(cmd, check=True, capture_output=True, text=True).stdout
    except FileNotFoundError:
        sys.exit("cargo not found on PATH")
    except subprocess.CalledProcessError as exc:
        sys.exit(f"cargo metadata failed:\n{exc.stderr}")
    return json.loads(out)


def non_gpl_manifest(manifest: str) -> str:
    """The workspace manifest with pixelflux's ``gpl`` feature replaced by ``openh264``.

    Fails when the pixelflux dependency is not one line naming ``gpl`` among its features,
    so a reshaped manifest cannot leave the non-GPL audit resolving the GPL graph.
    """
    pattern = re.compile(r'^(pixelflux\s*=\s*\{.*\bfeatures\s*=\s*\[[^\]]*)"gpl"([^\]]*\].*)$', re.MULTILINE)
    swapped, count = pattern.subn(r'\1"openh264"\2', manifest)
    if count != 1:
        sys.exit('Cargo.toml: expected one `pixelflux = { ... features = [... "gpl" ...] }` line to swap '
                 "for the non-GPL configuration")
    return swapped


def resolve(set_name: str) -> dict:
    """Resolve one configuration's graph with cargo."""
    if set_name == "gpl":
        return cargo_metadata(MANIFEST)
    with open(MANIFEST, encoding="utf-8") as f:
        swapped = non_gpl_manifest(f.read())
    with tempfile.TemporaryDirectory(prefix="selkies-licenses-") as tmp:
        with open(os.path.join(tmp, "Cargo.toml"), "w", encoding="utf-8") as f:
            f.write(swapped)
        shutil.copyfile(LOCKFILE, os.path.join(tmp, "Cargo.lock"))
        os.symlink(os.path.join(REPO, "crates"), os.path.join(tmp, "crates"))
        meta = cargo_metadata(os.path.join(tmp, "Cargo.toml"), locked=False, no_default_features=True)
        unpinned = lock_packages(os.path.join(tmp, "Cargo.lock")) - lock_packages(LOCKFILE)
    if os.path.realpath(meta["workspace_root"]) == os.path.realpath(REPO):
        sys.exit("cargo resolved the committed workspace in place of the non-GPL copy")
    if unpinned:
        sys.exit("the non-GPL configuration resolves packages Cargo.lock does not pin: "
                 + ", ".join(f"{name} {version}" for name, version, _ in sorted(unpinned)))
    return meta


def lock_packages(path: str) -> Set[Tuple[str, str, str]]:
    """The name, version, and source of every package a Cargo.lock pins."""
    with open(path, encoding="utf-8") as f:
        blocks = f.read().split("[[package]]")[1:]
    found = set()
    for block in blocks:
        fields = dict(re.findall(r'^(name|version|source) = "([^"]*)"$', block, re.MULTILINE))
        found.add((fields["name"], fields["version"], fields.get("source", "")))
    return found


def normal_closure(meta: dict) -> List[dict]:
    """The packages the server links, without feature-unifying other workspace members."""
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    seen = {member for member in meta["workspace_members"] if packages[member]["name"] == "selkies"}
    stack = list(seen)
    while stack:
        for dep in nodes[stack.pop()]["deps"]:
            if any(k["kind"] is None for k in dep["dep_kinds"]) and dep["pkg"] not in seen:
                seen.add(dep["pkg"])
                stack.append(dep["pkg"])
    return sorted((packages[i] for i in seen), key=lambda p: (p["name"], p["version"]))


def direct_dependencies(meta: dict) -> Set[str]:
    """The server crates' normal dependencies, whose roles the page describes."""
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    names = set()
    for member in meta["workspace_members"]:
        if packages[member]["name"] not in ("selkies", "selkies-rtc"):
            continue
        for dep in nodes[member]["deps"]:
            if any(k["kind"] is None for k in dep["dep_kinds"]):
                names.add(packages[dep["pkg"]]["name"])
    return names


def audit(meta: dict, set_name: str) -> Tuple[List[Crate], List[str]]:
    """Rank every crate of one configuration and collect the violations."""
    crates: List[Crate] = []
    problems: List[str] = []
    features = {n["id"]: set(n.get("features", ())) for n in meta["resolve"]["nodes"]}
    for pkg in normal_closure(meta):
        name, version = pkg["name"], pkg["version"]
        published = pkg.get("license") or CLARIFY.get(name)
        expr: Optional[Expr] = None
        if not published:
            rank = UNKNOWN
            published = f"license-file only: {pkg['license_file']}" if pkg.get("license_file") else "none"
            problems.append(f"{name} {version}: no machine-readable license ({published})")
        else:
            try:
                expr = parse_expression(published)
                rank = rank_expression(expr)
            except ExpressionError as exc:
                rank = UNKNOWN
                problems.append(f"{name} {version}: {exc}")
            if rank == UNKNOWN and expr is not None:
                problems.append(f"{name} {version}: license {published!r} is not classified")
            elif rank in (WEAK, COPYLEFT) and set_name not in ALLOWED_COPYLEFT.get(name, ()):
                problems.append(f"{name} {version}: {LABELS[rank]} crate license {published!r} in the {set_name} set")
        native = NATIVE.get(name)
        native_rank = PERMISSIVE
        if native is not None:
            native_rank = native.rank
            if features[pkg["id"]] & set(native.copyleft_features):
                native_rank = COPYLEFT
            if native_rank == COPYLEFT and set_name not in ALLOWED_COPYLEFT.get(name, ()):
                problems.append(f"{name} {version}: links {native.library} ({native.license}) in the {set_name} set")
        elif NATIVE_NAME.search(name):
            problems.append(f"{name} {version}: native binding not described in NATIVE (add it there, then --write)")
        crates.append(Crate(name, version, published, expr, rank, native_rank, pkg["manifest_path"]))
    return crates, problems


def listing(crates: List[Crate], set_name: str) -> str:
    """The aligned per-crate listing printed for one configuration."""
    worst = max([c.rank for c in crates] + [c.native_rank for c in crates])
    lines = [f"{set_name} set: {len(crates)} crates, worst category {LABELS[worst]}"]
    for c in crates:
        line = f"{c.name:<28} {c.version:<12} {c.license:<50} {LABELS[c.rank]}"
        if c.name in NATIVE:
            line += f"  [native: {NATIVE[c.name].library}, {LABELS[c.native_rank]}]"
        lines.append(line)
    return "\n".join(lines)


def cell(text: str) -> str:
    """A Markdown table cell's text, its pipes escaped."""
    return text.replace("|", "\\|")


def series(items: Iterable[str]) -> str:
    """Items joined as an English list, with the serial comma."""
    items = list(items)
    if len(items) < 3:
        return " and ".join(items)
    return ", ".join(items[:-1]) + ", and " + items[-1]


def table(head: Sequence[str], rows: Iterable[Sequence[str]]) -> str:
    """A Markdown table."""
    lines = ["| " + " | ".join(head) + " |", "| " + " | ".join("---" for _ in head) + " |"]
    lines += ["| " + " | ".join(cell(c) for c in row) + " |" for row in rows]
    return "\n".join(lines)


def build_label(present: Iterable[str]) -> str:
    """Which configurations contain a crate, for a table's Build column."""
    present = set(present)
    if present == set(SETS):
        return "both"
    return " and ".join(SET_LABELS[s] for s in SETS if s in present) + " only"


def licenses_of(crates: Iterable[Crate]) -> Tuple[str, int]:
    """The distinct expressions of several crates in first-seen order, and their worst rank."""
    seen: List[str] = []
    worst = PERMISSIVE
    for c in crates:
        text = render_expression(c.expr) if c.expr is not None else c.license
        if text not in seen:
            seen.append(text)
        worst = max(worst, c.rank)
    return "; ".join(seen), worst


def render_rust_direct(graphs: Dict[str, List[Crate]], problems: List[str], direct: Set[str]) -> str:
    """The table of the workspace's direct dependencies and what each does."""
    by_name: Dict[str, List[Crate]] = {}
    for c in graphs["gpl"]:
        by_name.setdefault(c.name, []).append(c)
    placed = [name for role in ROLES for name in role.crates]
    for name in sorted(direct - set(placed)):
        problems.append(f"{name}: a direct dependency ROLES does not describe (add it there, then --write)")
    for name in sorted(set(placed) - direct):
        problems.append(f"{name}: ROLES describes it, but no workspace crate depends on it")
    for name in sorted({n for n in placed if placed.count(n) > 1}):
        problems.append(f"{name}: placed in ROLES twice")
    rows = []
    for role in ROLES:
        members = [c for name in role.crates for c in by_name.get(name, [])]
        license_text, worst = licenses_of(members)
        names = ", ".join(f"`{n}`" for n in role.crates)
        rows.append((names, license_text, LABELS[worst], role.how, role.notes))
    return table(("Crate", "License", "Category", "How used", "Notes"), rows)


def render_native(graphs: Dict[str, List[Crate]], problems: List[str]) -> str:
    """The table of the native libraries behind the graph's bindings."""
    present: Dict[str, Set[str]] = {}
    for set_name, crates in graphs.items():
        for c in crates:
            if c.name in NATIVE:
                present.setdefault(c.name, set()).add(set_name)
    rows = []
    for name, native in NATIVE.items():
        if name not in present:
            problems.append(f"{name}: NATIVE describes it, but neither configuration's graph contains it")
            continue
        category = LABELS[native.rank]
        if native.copyleft_features:
            category += " (copyleft in the GPL build)"
        rows.append((native.library, f"`{name}`", native.license, category, build_label(present[name]), native.how))
    return table(("Native library", "Crate", "License", "Category", "Build", "How used"), rows)


def render_vendored(graphs: Dict[str, List[Crate]], problems: List[str]) -> str:
    """The table of the crates vendored into this repository."""
    vendor_root = os.path.join(os.path.realpath(REPO), "crates")
    found: Dict[str, Crate] = {}
    for crates in graphs.values():
        for c in crates:
            path = os.path.realpath(c.manifest)
            if path.startswith(vendor_root + os.sep) and f"{os.sep}vendor{os.sep}" in path:
                found[c.name] = c
    rows = []
    for name in sorted(set(found) | set(VENDORED)):
        if name not in found:
            problems.append(f"{name}: VENDORED describes it, but no vendored crate of that name is in the graph")
            continue
        if name not in VENDORED:
            problems.append(f"{name}: a vendored crate VENDORED does not describe (add it there, then --write)")
            continue
        c = found[name]
        where = os.path.relpath(os.path.dirname(os.path.realpath(c.manifest)), os.path.realpath(REPO))
        license_text, rank = licenses_of([c])
        rows.append((f"`{name}`", c.version, f"`{where}`", license_text, LABELS[rank], VENDORED[name]))
    return table(("Crate", "Version", "Where", "License", "Category", "Notes"), rows)


def render_rust_other(graphs: Dict[str, List[Crate]]) -> str:
    """Every license expression in the graphs, naming the crates that do not offer MIT or Apache-2.0 alone."""
    present: Dict[Tuple[str, str], Set[str]] = {}
    exprs: Dict[str, Tuple[Expr, int]] = {}
    for set_name, crates in graphs.items():
        for c in crates:
            if c.expr is None:
                continue
            text = render_expression(c.expr)
            exprs[text] = (c.expr, c.rank)
            present.setdefault((text, c.name), set()).add(set_name)
    common = sorted(t for t, (e, _) in exprs.items() if offers_default_terms(e))
    rows = []
    for text in sorted((t for t in exprs if t not in common), key=lambda t: (exprs[t][1], t)):
        names = []
        for (expr_text, name), sets in sorted(present.items()):
            if expr_text == text:
                label = build_label(sets)
                names.append(f"`{name}`" + ("" if label == "both" else f" ({label})"))
        rows.append((text, LABELS[exprs[text][1]], ", ".join(names)))
    lead = ("Most crates in either graph, the dependencies of pixelflux and pcmflux included, can be taken "
            "under MIT or under Apache-2.0 alone; the expressions they publish, spellings of one expression "
            "merged, are " + series(f"`{t}`" for t in common) + ". Every other crate, by expression:")
    return lead + "\n\n" + table(("License", "Category", "Crates"), rows)


def npm_audit(problems: List[str]) -> Tuple[str, str]:
    """The tables of the web packages' npm licenses, read from their lockfiles.

    A package the lockfile marks ``dev`` or ``devOptional`` is reached only through
    ``devDependencies`` and runs at build time; every other package is what ``npm ci
    --omit=dev`` installs, which is where the shipped bundle's code comes from, so it may
    carry no copyleft and no LGPL.
    """
    direct_rows = []
    scope_rows = []
    for addon in NPM_ADDONS:
        with open(os.path.join(REPO, addon, "package-lock.json"), encoding="utf-8") as f:
            lock = json.load(f)
        with open(os.path.join(REPO, addon, "package.json"), encoding="utf-8") as f:
            manifest = json.load(f)
        scopes: Dict[str, Dict[str, int]] = {"dependencies": {}, "devDependencies": {}}
        licenses: Dict[str, str] = {}
        for path, entry in lock.get("packages", {}).items():
            if not path or entry.get("link"):
                continue
            name = path.rsplit("node_modules/", 1)[-1]
            scope = "devDependencies" if entry.get("dev") or entry.get("devOptional") else "dependencies"
            published = entry.get("license")
            if not isinstance(published, str) or not published:
                problems.append(f"{addon} {name}: no machine-readable license in package-lock.json ({published!r})")
                continue
            try:
                expr = parse_expression(published)
            except ExpressionError as exc:
                problems.append(f"{addon} {name}: {exc}")
                continue
            rank = rank_expression(expr)
            text = render_expression(expr)
            licenses.setdefault(name, text)
            if rank == UNKNOWN:
                problems.append(f"{addon} {name}: license {published!r} is not classified")
            elif rank in (WEAK, COPYLEFT) and scope == "dependencies":
                problems.append(f"{addon} {name}: {LABELS[rank]} license {published!r} among the runtime dependencies")
            scopes[scope][text] = max(scopes[scope].get(text, PERMISSIVE), rank)
        for name in sorted(manifest.get("dependencies", {})):
            direct_rows.append((f"`{addon}`", f"`{name}`", licenses.get(name, "(not in package-lock.json)")))
        for scope, found in scopes.items():
            if found:
                listed = ", ".join(f"{t} ({LABELS[r]})" if r != PERMISSIVE else t for t, r in sorted(found.items()))
                scope_rows.append((f"`{addon}`", f"`{scope}` and what they pull in", listed))
    return (table(("Addon", "Package", "License"), direct_rows),
            table(("Addon", "Packages", "Licenses"), scope_rows))


def page_sections(graphs: Dict[str, List[Crate]], direct: Set[str], problems: List[str]) -> Dict[str, str]:
    """The generated sections of docs/licensing.md, by marker name."""
    npm_direct, npm_scopes = npm_audit(problems)
    return {
        "rust-dependencies": render_rust_direct(graphs, problems, direct),
        "rust-licenses": render_rust_other(graphs),
        "native-libraries": render_native(graphs, problems),
        "vendored-crates": render_vendored(graphs, problems),
        "npm-dependencies": npm_direct,
        "npm-licenses": npm_scopes,
    }


def marker(name: str, end: bool) -> str:
    """The HTML comment that opens or closes a generated section of the page."""
    if end:
        return f"<!-- check-licenses:end {name} -->"
    return (f"<!-- check-licenses:begin {name}: generated by scripts/check-licenses.py; "
            "rerun it with --write rather than editing this section by hand -->")


def splice(page: str, sections: Dict[str, str]) -> Tuple[str, List[str]]:
    """The page with each generated section replaced, and the names of the sections it lacks."""
    missing = []
    for name, body in sections.items():
        pattern = re.compile(re.escape(f"<!-- check-licenses:begin {name}") + r".*?"
                             + re.escape(marker(name, end=True)), re.DOTALL)
        replacement = f"{marker(name, end=False)}\n\n{body}\n\n{marker(name, end=True)}"
        page, count = pattern.subn(lambda _m, r=replacement: r, page)
        if count != 1:
            missing.append(name)
    return page, missing


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--set", choices=SETS, action="append", help="configuration to audit (default: both)")
    parser.add_argument("--metadata", metavar="FILE",
                        help="audit this `cargo metadata` JSON instead of resolving the graph (with one --set)")
    parser.add_argument("--page", default=PAGE, help="the page to check or write (default: docs/licensing.md)")
    parser.add_argument("--markdown", action="store_true", help="print the page's generated sections")
    parser.add_argument("--write", action="store_true", help="rewrite the page's generated sections")
    parser.add_argument("--quiet", action="store_true", help="print only the summary lines and the violations")
    args = parser.parse_args(argv)

    sets = args.set or list(SETS)
    if args.metadata and len(sets) != 1:
        parser.error("--metadata needs exactly one --set")
    whole = sets == list(SETS) and not args.metadata
    if (args.markdown or args.write) and not whole:
        parser.error("--markdown and --write need both configurations resolved by cargo")

    failed = False
    graphs: Dict[str, List[Crate]] = {}
    direct: Set[str] = set()
    for set_name in sets:
        if args.metadata:
            with open(args.metadata, encoding="utf-8") as f:
                meta = json.load(f)
        else:
            meta = resolve(set_name)
        crates, problems = audit(meta, set_name)
        graphs[set_name] = crates
        if set_name == "gpl":
            direct = direct_dependencies(meta)
        if not args.markdown:
            text = listing(crates, set_name)
            print(text.splitlines()[0] if args.quiet else text)
        for problem in problems:
            print(f"FAIL {set_name}: {problem}")
        failed = failed or bool(problems)
        if not problems and not args.markdown:
            print(f"PASS {set_name}: no copyleft component outside the allowed list\n")
    if not whole:
        return 1 if failed else 0

    problems = []
    sections = page_sections(graphs, direct, problems)
    for problem in problems:
        print(f"FAIL page: {problem}")
    failed = failed or bool(problems)
    if args.markdown:
        for name, body in sections.items():
            print(f"{marker(name, end=False)}\n\n{body}\n\n{marker(name, end=True)}\n")
        return 1 if failed else 0

    page_path = os.path.realpath(args.page)
    inside = page_path.startswith(os.path.realpath(REPO) + os.sep)
    page_name = os.path.relpath(page_path, REPO) if inside else page_path
    fix = "python3 scripts/check-licenses.py --write" + ("" if page_path == os.path.realpath(PAGE) else f" --page {page_name}")
    with open(args.page, encoding="utf-8") as f:
        current = f.read()
    rendered, missing = splice(current, sections)
    for name in missing:
        print(f"FAIL page: {page_name} has no `check-licenses` section named {name}")
        failed = True
    if args.write:
        if rendered != current:
            with open(args.page, "w", encoding="utf-8") as f:
                f.write(rendered)
            print(f"wrote {page_name}")
        else:
            print(f"{page_name} is up to date")
    elif rendered != current:
        sys.stdout.writelines(difflib.unified_diff(
            current.splitlines(keepends=True), rendered.splitlines(keepends=True), page_name, page_name + " (generated)"))
        print(f"FAIL page: {page_name} differs from the graphs; run {fix}")
        failed = True
    elif not missing:
        print(f"PASS page: {page_name} matches the graphs")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
