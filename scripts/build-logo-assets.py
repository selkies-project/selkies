#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Regenerate every image in the repository from the logo artwork.

Every image here is the Selkies mark: the icon on its own, the icon on the round
plate the app icons use, or one of the two lockups that set it beside or above
the wordmark. They are committed rather than built at release time, so a change
to the artwork otherwise has to be carried by hand into a dozen files and the
ones nobody remembers keep the old logo -- which is exactly what happened to the
gradient before this existed.

Two sources are hand-authored; everything else here is composed from them:

    docs/assets/logo/selkies.svg      the mark
    docs/assets/logo/wordmark.svg     the "Selkies" lettering

The two lockups -- the mark beside the wordmark, and above it -- are built from
those, so the mark cannot end up embedded twice with two different gradients.
Only their layout lives here, as the transform each places its two pieces at.

Needs `rsvg-convert` (librsvg) and Pillow. Run from the repository root:

    python3 scripts/build-logo-assets.py [--check]

`--check` regenerates into a temporary directory and reports which committed
images are stale, without writing any of them.
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile

from PIL import Image

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MARK = "docs/assets/logo/selkies.svg"
WORDMARK = "docs/assets/logo/wordmark.svg"
HORIZONTAL = "docs/assets/logo/horizontal.svg"
VERTICAL = "docs/assets/logo/vertical.svg"

# Where each lockup puts the two pieces. The mark keeps its own internal transform
# and is placed by the first; the wordmark is a bare path placed by the second.
LOCKUPS = [
    {"dest": HORIZONTAL, "width": 308, "height": 89,
     "viewbox": "0 0 3463.466 1000.000",
     "mark": "translate(-14.075 -145.442) scale(2.914093)",
     "wordmark": "translate(1504.884 724.090) scale(0.295501)"},
    {"dest": VERTICAL, "width": 219, "height": 245,
     "viewbox": "0 0 1315.013 1471.000",
     "mark": "translate(-14.075 -145.442) scale(2.914093)",
     "wordmark": "translate(16.446 1467.171) scale(0.191433)"},
]

# The mark is copied verbatim to wherever a dashboard serves it.
COPIES = ["addons/selkies-dashboard/public/selkies.svg",
          "addons/selkies-dashboard-wish/public/selkies.svg"]

# Bare renders of the mark: (source, width, height, destination).
RENDERS = [
    (HORIZONTAL, 1024, 296, "docs/assets/logo/horizontal.png"),
    (HORIZONTAL, 480, 139, "docs/assets/logo/horizontal-480.png"),
    (VERTICAL, 480, 537, "docs/assets/logo/vertical.png"),
    (MARK, 180, 180, "addons/selkies-dashboard/public/icon.png"),
    (MARK, 180, 180, "addons/selkies-dashboard-wish/public/icon.png"),
]

# Single-image 256x256 containers, which is what every consumer here expects.
ICONS = [(MARK, 256, "docs/assets/logo/favicon.ico"),
         (MARK, 256, "addons/selkies-dashboard-wish/public/favicon.ico")]

# The app icons set the mark on an opaque white disc, so a launcher that masks
# the icon to a circle crops nothing and one that does not still gets a shape
# rather than a floating mark. That plate belongs to the installed-app icon
# alone: a browser tab draws its icon on its own strip, where the bare mark is
# the one that reads, so the dashboards keep both and point each consumer at
# the right one. Proportions are of the icon's side.
PLATED = [(MARK, 192, "docs/assets/logo/icon-192x192.png"),
          (MARK, 512, "docs/assets/logo/icon-512x512.png"),
          (MARK, 512, "addons/selkies-dashboard/public/icon-512.png"),
          (MARK, 512, "addons/selkies-dashboard-wish/public/icon-512.png")]
PLATE_RADIUS = 0.4900
MARK_WIDTH = 0.7400
MARK_CENTRE = (0.5000, 0.4844)

# What `--check` treats as a stale image, as a mean per-channel difference. Two
# librsvg builds disagree on edge pixels by single digits at these sizes, while
# artwork that has actually moved on differs by an order of magnitude more, so
# the line sits between the two rather than at zero.
STALE_THRESHOLD = 12.0


def run(*args):
    subprocess.run(args, check=True, capture_output=True)


def render(svg, width, height, out):
    run("rsvg-convert", "-w", str(width), "-h", str(height), "-o", out, svg)


def mark_extent(svg, probe=1024):
    """Where the artwork sits inside its own canvas, as fractions of it.

    Measured rather than read off the viewBox, because the mark does not fill
    its canvas and the plate has to be placed against the ink, not the box.
    """
    with tempfile.TemporaryDirectory() as tmp:
        png = os.path.join(tmp, "probe.png")
        render(svg, probe, probe, png)
        alpha = Image.open(png).convert("RGBA").split()[3]
        box = alpha.getbbox()
    left, top, right, bottom = box
    return (left / probe, top / probe, (right - left) / probe, (bottom - top) / probe)


def plated_svg(svg, size, out):
    """A wrapper placing the mark, at the proportions above, on a white disc."""
    source = open(os.path.join(REPO, svg)).read()
    viewbox = re.search(r'viewBox="([^"]+)"', source).group(1)
    inner = source[source.index(">", source.index("<svg")) + 1:source.rindex("</svg>")]
    mx, my, mw, mh = mark_extent(os.path.join(REPO, svg))
    # Scale the nested canvas so the ink -- not the canvas -- is MARK_WIDTH wide,
    # then place it so the ink's centre lands on MARK_CENTRE.
    canvas = size * MARK_WIDTH / mw
    x = size * MARK_CENTRE[0] - (mx + mw / 2) * canvas
    y = size * MARK_CENTRE[1] - (my + mh / 2) * canvas
    with open(out, "w") as f:
        f.write(
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" '
            f'viewBox="0 0 {size} {size}">\n'
            f'  <circle cx="{size / 2}" cy="{size / 2}" r="{size * PLATE_RADIUS}" '
            f'fill="#ffffff"/>\n'
            f'  <svg x="{x:.3f}" y="{y:.3f}" width="{canvas:.3f}" height="{canvas:.3f}" '
            f'viewBox="{viewbox}">{inner}</svg>\n'
            f'</svg>\n')


def inner(svg_text):
    """A source SVG's contents, without its root element."""
    return svg_text[svg_text.index(">", svg_text.index("<svg")) + 1:svg_text.rindex("</svg>")]


def build_lockup(spec):
    """A lockup: the mark and the wordmark, each placed by its own transform."""
    mark = open(os.path.join(REPO, MARK)).read()
    wordmark = open(os.path.join(REPO, WORDMARK)).read()
    gradient = re.search(r'<defs>.*?</defs>', mark, re.S).group(0)
    art = re.search(r'<g transform="translate\(0,460\) scale\(0\.1,-0\.1\)">.*?</g>',
                    mark, re.S).group(0)
    path = re.search(r'<path[^>]*/>', inner(wordmark)).group(0)
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{spec["width"]}" '
            f'height="{spec["height"]}" viewBox="{spec["viewbox"]}">\n'
            f'  {gradient}\n'
            f'  <g transform="{spec["mark"]}">\n'
            f'    {art}\n'
            f'  </g>\n'
            f'  {path[:-2].rstrip()} transform="{spec["wordmark"]}"/>\n'
            f'</svg>\n')


def check_sources():
    """The mark has to declare the ramp everything else is drawn from."""
    stops = re.findall(r'stop-color="(#[0-9A-Fa-f]{6})"',
                       open(os.path.join(REPO, MARK)).read())
    if not stops:
        sys.exit(f"{MARK} declares no gradient stops")
    for source in (MARK, WORDMARK):
        if not os.path.exists(os.path.join(REPO, source)):
            sys.exit(f"{source} is missing; it is one of the two hand-authored sources")
    print(f"mark gradient: {' '.join(stops)}")


def generate(into):
    """Write every image under `into` (the repository root, or a scratch copy)."""
    produced = []
    for spec in LOCKUPS:
        target = os.path.join(into, spec["dest"])
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(target, "w") as f:
            f.write(build_lockup(spec))
        produced.append(spec["dest"])

    for dest in COPIES:
        target = os.path.join(into, dest)
        os.makedirs(os.path.dirname(target), exist_ok=True)
        shutil.copyfile(os.path.join(REPO, MARK), target)
        produced.append(dest)

    # The lockups were just written under `into`; render from those rather than
    # from the committed ones, or a check would grade fresh images against stale
    # artwork and always call them clean.
    generated = {spec["dest"] for spec in LOCKUPS}

    def source(svg):
        return os.path.join(into if svg in generated else REPO, svg)

    for svg, w, h, dest in RENDERS:
        target = os.path.join(into, dest)
        os.makedirs(os.path.dirname(target), exist_ok=True)
        render(source(svg), w, h, target)
        produced.append(dest)

    with tempfile.TemporaryDirectory() as tmp:
        for svg, size, dest in ICONS:
            target = os.path.join(into, dest)
            os.makedirs(os.path.dirname(target), exist_ok=True)
            png = os.path.join(tmp, "icon.png")
            render(os.path.join(REPO, svg), size, size, png)
            Image.open(png).convert("RGBA").save(target, format="ICO",
                                                 sizes=[(size, size)])
            produced.append(dest)

        for svg, size, dest in PLATED:
            target = os.path.join(into, dest)
            os.makedirs(os.path.dirname(target), exist_ok=True)
            wrapper = os.path.join(tmp, f"plated-{size}.svg")
            plated_svg(svg, size, wrapper)
            render(wrapper, size, size, target)
            produced.append(dest)
    return produced


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true",
                        help="report stale images instead of writing them")
    args = parser.parse_args()

    if not shutil.which("rsvg-convert"):
        sys.exit("rsvg-convert not found; install librsvg to regenerate the images")
    check_sources()

    if not args.check:
        produced = generate(REPO)
        for dest in produced:
            print(f"  wrote {dest}")
        print(f"{len(produced)} images regenerated")
        return 0

    with tempfile.TemporaryDirectory() as tmp:
        stale = []
        for dest in generate(tmp):
            fresh, committed = os.path.join(tmp, dest), os.path.join(REPO, dest)
            if not os.path.exists(committed):
                stale.append(f"{dest} (missing)")
            elif dest.endswith(".svg"):
                if open(fresh, "rb").read() != open(committed, "rb").read():
                    stale.append(dest)
            else:
                # Compared as pixels, and by the average rather than the worst:
                # any two rasterizers disagree on edge pixels by the full range,
                # so only a difference spread across the image is a stale asset.
                a = Image.open(fresh).convert("RGBA")
                b = Image.open(committed).convert("RGBA")
                if a.size != b.size:
                    stale.append(f"{dest} ({b.size} committed, {a.size} generated)")
                else:
                    delta = mean_delta(a, b)
                    if delta > STALE_THRESHOLD:
                        stale.append(f"{dest} (mean difference {delta:.1f}/255)")
        if stale:
            print("stale images:")
            for item in stale:
                print(f"  {item}")
            return 1
        print("every committed image matches the artwork")
        return 0


def mean_delta(a, b):
    """Average per-channel difference between two images, 0..255."""
    from PIL import ImageChops
    pixels = list(ImageChops.difference(a, b).getdata())
    return sum(sum(p) for p in pixels) / (len(pixels) * len(pixels[0]))


if __name__ == "__main__":
    sys.exit(main())
