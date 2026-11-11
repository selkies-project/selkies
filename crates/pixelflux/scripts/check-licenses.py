#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""License gate for the pixelflux dependency graph.

Resolves the crate graph of a build configuration with ``cargo metadata``
(normal dependencies only, the ones compiled into the extension), classifies
every crate's SPDX license expression as permissive, weak copyleft, or
copyleft, and overlays the native libraries the ``-sys`` crates link, load, or
vendor, which crate metadata cannot see (``codec-sys`` is an MIT binding to the
GPL-2.0-or-later libx264 and x265 under its ``x264`` and ``x265`` features). It fails when a copyleft component appears in a
build configuration that is not allowed to carry it, when a crate has no
usable license metadata, or when a new native binding shows up that this file
does not describe yet.

    scripts/check-licenses.py                 # both configurations
    scripts/check-licenses.py --set non-gpl   # PIXELFLUX_ENABLE_GPL=0
    scripts/check-licenses.py --markdown      # tables for LICENSES.md
    scripts/check-licenses.py --metadata m.json --set non-gpl

The configuration names mirror setup.py: ``non-gpl`` is
``--no-default-features --features openh264``, ``gpl`` is the default feature
set. LICENSES.md at the repository root is the human-readable inventory this
check enforces.
"""
import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
from typing import Dict, List, Optional, Sequence, Tuple

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
MANIFEST = os.path.join(REPO, "Cargo.toml")

# Build configurations, as setup.py selects them: the default features (`gpl`,
# libx264) and the PIXELFLUX_ENABLE_GPL=0 build (`openh264` instead of `gpl`).
SETS: Dict[str, List[str]] = {
    "non-gpl": ["--no-default-features", "--features", "openh264"],
    "gpl": [],
}

# Copyleft components and the configurations allowed to contain them. Anything
# copyleft that is not listed here fails in every configuration, so adding a
# GPL dependency means adding a line here and in LICENSES.md.
ALLOWED_COPYLEFT: Dict[str, Tuple[str, ...]] = {
    "codec-sys": ("gpl",),
}

PERMISSIVE = 0
WEAK = 1
COPYLEFT = 2
UNKNOWN = 3
LABELS = {PERMISSIVE: "permissive", WEAK: "weak copyleft", COPYLEFT: "copyleft",
          UNKNOWN: "unknown"}

PERMISSIVE_IDS = {
    "MIT", "MIT-0", "Apache-2.0", "BSD-1-Clause", "BSD-2-Clause", "BSD-3-Clause",
    "0BSD", "ISC", "Zlib", "BSL-1.0", "CC0-1.0", "Unlicense", "Unicode-3.0",
    "Unicode-DFS-2016", "WTFPL", "PSF-2.0", "Python-2.0", "NCSA", "OFL-1.1",
    "Artistic-2.0", "BlueOak-1.0.0", "IJG", "MIT-CMU", "X11", "CDLA-Permissive-2.0",
    "OpenSSL", "SSLeay-standalone", "FTL", "libpng-2.0", "bzip2-1.0.6", "curl",
    "HPND", "ICU", "Beerware", "Fair",
}
# MPL-2.0 is file-level copyleft: modified MPL files must stay MPL, but the
# larger work that links them may be under any license, which is why it sits
# with the permissive group here (pixelflux itself is MPL-2.0).
FILE_LEVEL_COPYLEFT_IDS = {"MPL-2.0", "MPL-1.1", "CDDL-1.0", "CDDL-1.1", "EPL-1.0",
                           "EPL-2.0", "CPL-1.0"}
WEAK_PREFIXES = ("LGPL-",)
COPYLEFT_PREFIXES = ("GPL-", "AGPL-", "SSPL-", "EUPL-", "OSL-", "CC-BY-SA-",
                     "CC-BY-NC", "CECILL", "QPL-", "Sleepycat", "RPL-", "CPAL-")
# Exceptions that lift the copyleft of the base license for the linked work.
LINKING_EXCEPTIONS = {"GCC-exception-3.1", "GCC-exception-2.0", "Classpath-exception-2.0",
                      "LLVM-exception", "Bison-exception-2.2", "Autoconf-exception-3.0",
                      "Autoconf-exception-2.0", "Font-exception-2.0", "mif-exception",
                      "Linux-syscall-note", "GPL-3.0-linking-exception",
                      "LGPL-3.0-linking-exception"}


def classify_id(ident: str) -> int:
    """Rank one SPDX license identifier."""
    if ident in PERMISSIVE_IDS or ident in FILE_LEVEL_COPYLEFT_IDS:
        return PERMISSIVE
    if ident.startswith(WEAK_PREFIXES):
        return WEAK
    if ident.startswith(COPYLEFT_PREFIXES):
        return COPYLEFT
    return UNKNOWN


class ExpressionError(ValueError):
    pass


def classify_expression(expression: str) -> int:
    """Rank an SPDX expression: OR takes the licensee's best choice, AND the worst.

    Accepts the legacy ``/`` separator and bare ``+`` suffixes that older
    crates still publish. Unknown identifiers rank as unknown, which fails the
    check, so a new license has to be classified here deliberately.
    """
    tokens = re.findall(r"\(|\)|[^\s()]+", expression.replace("/", " OR "))
    if not tokens:
        raise ExpressionError("empty license expression")
    pos = 0

    def peek() -> Optional[str]:
        return tokens[pos] if pos < len(tokens) else None

    def take() -> str:
        nonlocal pos
        pos += 1
        return tokens[pos - 1]

    def parse_or() -> int:
        rank = parse_and()
        while peek() is not None and peek().upper() == "OR":
            take()
            rank = min(rank, parse_and())
        return rank

    def parse_and() -> int:
        rank = parse_with()
        while peek() is not None and peek().upper() == "AND":
            take()
            rank = max(rank, parse_with())
        return rank

    def parse_with() -> int:
        rank = parse_atom()
        if peek() is not None and peek().upper() == "WITH":
            take()
            if peek() is None:
                raise ExpressionError("dangling WITH in %r" % expression)
            exception = take()
            if exception in LINKING_EXCEPTIONS and rank in (WEAK, COPYLEFT):
                rank = PERMISSIVE
        return rank

    def parse_atom() -> int:
        token = peek()
        if token is None:
            raise ExpressionError("unexpected end of %r" % expression)
        take()
        if token == "(":
            rank = parse_or()
            if peek() != ")":
                raise ExpressionError("unbalanced parentheses in %r" % expression)
            take()
            return rank
        if token.upper() in ("AND", "OR", "WITH") or token == ")":
            raise ExpressionError("unexpected %r in %r" % (token, expression))
        if token.endswith("+"):
            token = token[:-1] + "-or-later"
        return classify_id(token)

    rank = parse_or()
    if pos != len(tokens):
        raise ExpressionError("trailing tokens in %r" % expression)
    return rank


# Native libraries behind the crates that bind, vendor, or load them. `rank` is
# the license category of the native code, `copyleft_features` the crate
# features that link a copyleft library, and `how` the way it reaches the
# extension. Every crate named like a native binding (-sys, _sys, -ffi) has to
# be described here, so a new binding fails the check until it is inventoried.
NATIVE: Dict[str, Dict[str, object]] = {
    "codec-sys": dict(
        library="libvpx, SVT-AV1, dav1d, and libde265 on every wheel; libx264 and x265 "
                "(GPL-2.0-or-later) on the GPL wheel, kvazaar on the non-GPL one",
        license="BSD-3-Clause (libvpx, kvazaar; SVT-AV1 with the Alliance for Open Media "
                "patent license), BSD-2-Clause (dav1d), LGPL-3.0-or-later (libde265)",
        rank=WEAK, copyleft_features=("x264", "x265"),
        how="linked shared libraries, bound at build time from the headers of the copies "
            "that are linked (bundled into the wheels by auditwheel); crate itself MIT OR "
            "Apache-2.0, path dependency",
        note="the software H.264 (GPL wheel), HEVC, VP8, VP9, and AV1 encoders and the "
             "virtual camera's AV1 and HEVC decoders"),
    "va-sys": dict(
        library="libva (libva.so.2, libva-drm.so.2)", license="MIT", rank=PERMISSIVE,
        how="dlopen at run time, never linked or shipped; the bindings are generated from "
            "the libva headers vendored under pixelflux/va-sys/headers/ (MIT); crate itself "
            "MIT OR Apache-2.0, path dependency",
        note="VA-API encoders and video processor"),
    "openh264-sys2": dict(
        library="Cisco OpenH264 2.6 (vendored source)", license="BSD-2-Clause",
        rank=PERMISSIVE, how="compiled from vendored source and linked statically",
        note="full-frame software H.264 without GPL; AVC patent licenses are the deployer's "
             "concern as with any H.264 encoder"),
    "turbojpeg-sys": dict(
        library="libjpeg-turbo 3.1 (vendored source)",
        license="IJG AND BSD-3-Clause AND Zlib", rank=PERMISSIVE,
        how="compiled from vendored source and linked statically (cmake feature)",
        note="JPEG encoder"),
    "gbm-sys": dict(library="libgbm (Mesa)", license="MIT", rank=PERMISSIVE,
                    how="linked shared library (system)", note="GPU buffer allocation"),
    "pixman-sys": dict(library="libpixman-1", license="MIT", rank=PERMISSIVE,
                       how="linked shared library (system)", note="software renderer"),
    "xkbcommon": dict(library="libxkbcommon", license="MIT", rank=PERMISSIVE,
                      how="linked shared library (system)", note="keymaps"),
    "wayland-sys": dict(library="libwayland-server", license="MIT", rank=PERMISSIVE,
                        how="dlopen at run time (`dlopen` feature), never linked",
                        note="capture compositor; libwayland-client is not used, the client "
                             "side is the pure-Rust backend"),
    "input-sys": dict(library="libinput", license="MIT", rank=PERMISSIVE,
                      how="in the crate graph through smithay's backend_libinput; no "
                          "symbol is referenced, so the linker drops it from NEEDED",
                      note="not present in the built extension"),
    "libudev-sys": dict(library="libudev (systemd)", license="LGPL-2.1-or-later", rank=WEAK,
                        how="in the crate graph through smithay's backend_udev; no symbol "
                            "is referenced, so the linker drops it from NEEDED",
                        note="not present in the built extension"),
    "drm-sys": dict(library="libdrm headers (bindings only)", license="MIT", rank=PERMISSIVE,
                    how="bindings generated from the headers; drm-ffi issues the ioctls "
                        "itself and no libdrm symbol is linked",
                    note="DRM/KMS"),
    "drm-ffi": dict(library="Linux DRM ioctls (no library)", license="MIT", rank=PERMISSIVE,
                    how="pure Rust ioctl wrappers", note="DRM/KMS"),
    "pyo3-ffi": dict(library="libpython (CPython)", license="PSF-2.0", rank=PERMISSIVE,
                     how="extension module: symbols resolved from the hosting interpreter, "
                         "not linked",
                     note="Python binding"),
    "nvcodec-sys": dict(
        library="NVIDIA NVENC (libnvidia-encode.so.1), framebuffer capture "
                "(libnvidia-fbc.so.1), and CUDA driver (libcuda.so.1)",
        license="proprietary driver libraries; nvEncodeAPI.h is MIT, the CUDA bindings "
                "are declarations generated from the CUDA toolkit headers",
        rank=PERMISSIVE,
        how="dlopen at run time, never linked or shipped; crate MIT OR Apache-2.0",
        note="NVENC encoder, path dependency"),
    "linux-raw-sys": dict(library="Linux kernel ABI (syscall numbers and structs)",
                          license="Linux-syscall-note", rank=PERMISSIVE,
                          how="pure-Rust constants, no library", note="rustix backend"),
    "libc": dict(library="C runtime (glibc, or musl on musllinux wheels)",
                 license="LGPL-2.1-or-later (glibc), MIT (musl)", rank=WEAK,
                 how="linked shared library (system), as for every program",
                 note="libm, libpthread, libdl are part of it"),
    "libloading": dict(
        library="libEGL.so.1 (Mesa/Khronos, MIT), libpipewire-0.3.so.0 (MIT), "
                "libwayland-server.so.0 (MIT), libva.so.2 and libva-drm.so.2 (MIT), "
                "libcuda.so.1/libnvidia-encode.so.1/libnvidia-fbc.so.1 "
                "(proprietary), and on aarch64 the Jetson Linux "
                "libnvv4l2.so/libnvbuf_utils.so/libnvbufsurface.so/"
                "libnvbufsurftransform.so (proprietary)",
        license="MIT and proprietary driver libraries", rank=PERMISSIVE,
        how="dlopen at run time, never linked", note="see nvcodec-sys"),
}

NATIVE_NAME = re.compile(r"(-|_)sys\d*$|-ffi$|^lib.*-sys$")


def cargo(args: Sequence[str]) -> str:
    try:
        return subprocess.run(["cargo"] + list(args), check=True, capture_output=True,
                              text=True).stdout
    except FileNotFoundError:
        sys.exit("cargo not found on PATH")
    except subprocess.CalledProcessError as exc:
        sys.exit("cargo %s failed:\n%s" % (args[0], exc.stderr))


def run_cargo_metadata(extra: Sequence[str], target: Optional[str]) -> dict:
    """Resolve one configuration's graph as the crate builds on its own.

    Inside a larger workspace (the Selkies repository), cargo unifies the
    features every member asks of a dependency, so the non-GPL graph would
    carry the x264 and x265 features the Selkies server enables. The graph is
    then resolved in a copy of the crate outside it, as the wheel builds it
    (scripts/ci/crate-licenses.sh does the same with `git archive`): the
    manifest and the crate's own Cargo.lock, beside links to the rest of the
    crate.
    """
    args = ["metadata", "--format-version", "1", "--locked"] + list(extra)
    if target:
        args += ["--filter-platform", target]
    workspace = cargo(["locate-project", "--workspace", "--message-format", "plain",
                       "--manifest-path", MANIFEST]).strip()
    if os.path.realpath(workspace) == os.path.realpath(MANIFEST):
        return json.loads(cargo(args + ["--manifest-path", MANIFEST]))
    with tempfile.TemporaryDirectory(prefix="pixelflux-licenses-") as tmp:
        for entry in os.listdir(REPO):
            if entry not in ("Cargo.toml", "Cargo.lock", "target"):
                os.symlink(os.path.join(REPO, entry), os.path.join(tmp, entry))
        for name in ("Cargo.toml", "Cargo.lock"):
            with open(os.path.join(REPO, name), "rb") as src, \
                    open(os.path.join(tmp, name), "wb") as dst:
                dst.write(src.read())
        meta = json.loads(cargo(args + ["--manifest-path", os.path.join(tmp, "Cargo.toml")]))
    if os.path.realpath(meta["workspace_root"]) != os.path.realpath(tmp):
        sys.exit("cargo resolved %s in place of the crate's own workspace" % meta["workspace_root"])
    return meta


def host_triple() -> Optional[str]:
    try:
        out = subprocess.run(["rustc", "-vV"], check=True, capture_output=True,
                             text=True).stdout
    except (OSError, subprocess.CalledProcessError):
        return None
    for line in out.splitlines():
        if line.startswith("host:"):
            return line.split(":", 1)[1].strip()
    return None


def normal_closure(meta: dict) -> List[dict]:
    """Packages reachable from the root over normal (non-build, non-dev) edges."""
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = meta["resolve"]["root"]
    if root is None:
        sys.exit("cargo metadata has no root package; pass --manifest-path to a crate")
    seen = {root}
    stack = [root]
    while stack:
        node = nodes[stack.pop()]
        for dep in node["deps"]:
            if not any(k["kind"] is None for k in dep["dep_kinds"]):
                continue
            if dep["pkg"] not in seen:
                seen.add(dep["pkg"])
                stack.append(dep["pkg"])
    return sorted((packages[i] for i in seen), key=lambda p: (p["name"], p["version"]))


def audit(meta: dict, set_name: str) -> Tuple[List[dict], List[str]]:
    """Classify every crate of one configuration and collect the violations."""
    rows = []
    problems = []
    features = {n["id"]: n.get("features", ()) for n in meta["resolve"]["nodes"]}
    for pkg in normal_closure(meta):
        name = pkg["name"]
        expr = pkg.get("license")
        note = ""
        if not expr:
            rank = UNKNOWN
            expr = "(none)"
            if pkg.get("license_file"):
                expr = "license-file only: %s" % pkg["license_file"]
            problems.append("%s %s: no machine-readable license (%s)"
                            % (name, pkg["version"], expr))
        else:
            try:
                rank = classify_expression(expr)
            except ExpressionError as exc:
                rank = UNKNOWN
                problems.append("%s %s: %s" % (name, pkg["version"], exc))
            if rank == UNKNOWN:
                problems.append("%s %s: license %r is not classified"
                                % (name, pkg["version"], expr))
            elif rank == COPYLEFT and set_name not in ALLOWED_COPYLEFT.get(name, ()):
                problems.append("%s %s: copyleft crate license %r in the %s set"
                                % (name, pkg["version"], expr, set_name))
        native = NATIVE.get(name)
        native_rank = PERMISSIVE
        if native is not None:
            native_rank = int(native["rank"])  # type: ignore[call-overload]
            copyleft = native.get("copyleft_features", ())
            if set(features[pkg["id"]]) & set(copyleft):  # type: ignore[call-overload]
                native_rank = COPYLEFT
            if native_rank == COPYLEFT and set_name not in ALLOWED_COPYLEFT.get(name, ()):
                problems.append("%s %s: links %s (%s) in the %s set"
                                % (name, pkg["version"], native["library"],
                                   native["license"], set_name))
        elif NATIVE_NAME.search(name):
            problems.append("%s %s: native binding not described in NATIVE (add it there "
                            "and to LICENSES.md)" % (name, pkg["version"]))
        rows.append(dict(name=name, version=pkg["version"], license=expr, rank=rank,
                         native=native, native_rank=native_rank, note=note))
    return rows, problems


def render(rows: List[dict], set_name: str, markdown: bool) -> str:
    worst = max([r["rank"] for r in rows] + [r["native_rank"] for r in rows])
    lines = []
    if markdown:
        lines.append("| Crate | Version | License | Category | Native library | How used |")
        lines.append("| --- | --- | --- | --- | --- | --- |")
    for r in rows:
        category = LABELS[r["rank"]]
        if r["license"].startswith("MPL-"):
            category += " (file-level copyleft)"
        native = r["native"]
        lib = "" if native is None else "%s (%s, %s)" % (
            native["library"], native["license"], LABELS[int(native["rank"])])  # type: ignore[call-overload]
        how = "" if native is None else str(native["how"])
        if markdown:
            cells = [r["name"], r["version"], r["license"], category, lib, how]
            lines.append("| " + " | ".join(c.replace("|", "\\|") for c in cells) + " |")
        else:
            line = "%-22s %-10s %-45s %s" % (r["name"], r["version"], r["license"], category)
            if native is not None:
                line += "  [native: %s]" % lib
            if r["note"]:
                line += "  (%s)" % r["note"]
            lines.append(line)
    header = "%s set: %d crates, worst category %s" % (set_name, len(rows), LABELS[worst])
    return header + "\n" + "\n".join(lines)


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--set", choices=sorted(SETS), action="append",
                        help="configuration to audit (default: all)")
    parser.add_argument("--metadata", metavar="FILE",
                        help="audit this `cargo metadata` JSON instead of resolving the "
                             "graph (use together with one --set)")
    parser.add_argument("--target", default=None,
                        help="target triple for --filter-platform (default: the host)")
    parser.add_argument("--markdown", action="store_true",
                        help="print Markdown tables instead of the aligned listing")
    parser.add_argument("--quiet", action="store_true",
                        help="print only the summary lines and the violations")
    args = parser.parse_args(argv)

    sets = args.set or sorted(SETS)
    if args.metadata and len(sets) != 1:
        parser.error("--metadata needs exactly one --set")
    target = args.target or host_triple()
    failed = False
    for set_name in sets:
        if args.metadata:
            metadata = os.path.realpath(args.metadata)
            if not re.fullmatch(r"/[^\0]*", metadata) or ".." in metadata.split("/"):
                parser.error(f"--metadata {args.metadata}: not a path this reads")
            with open(metadata, "r", encoding="utf-8") as handle:
                meta = json.load(handle)
        else:
            meta = run_cargo_metadata(SETS[set_name], target)
        rows, problems = audit(meta, set_name)
        text = render(rows, set_name, args.markdown)
        print(text.splitlines()[0] if args.quiet else text)
        for problem in problems:
            print("FAIL %s: %s" % (set_name, problem))
        if problems:
            failed = True
        else:
            print("PASS %s: no copyleft component outside the allowed list" % set_name)
        print()
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
