#!/usr/bin/env python3
"""Filter wheel paths or release ``URL digest`` lines for this interpreter.

Wheel tags, not interpreter-looking substrings in filenames, decide compatibility.
Invalid wheels and wheels for another distribution are ignored. The selector is
shared by the integration workflow and the devcontainer release downloader.

    select-compatible-wheels.py [--tags FILE] PROJECT < wheels-or-release-assets

``--tags`` takes the supported tags from FILE, one ``interpreter-abi-platform``
tag per line (compressed tag sets allowed), in place of this interpreter's, so
the selection for another interpreter or platform can be asked here.
"""
import os
import sys
from urllib.parse import unquote, urlparse

from packaging.tags import parse_tag, sys_tags
from packaging.utils import InvalidWheelFilename, canonicalize_name, parse_wheel_filename


def compatible(project: str, filename: str, supported_tags=None) -> bool:
    """Whether *filename* is a wheel of *project* installable on *supported_tags*."""
    try:
        name, _, _, wheel_tags = parse_wheel_filename(filename)
    except InvalidWheelFilename:
        return False
    supported = frozenset(sys_tags() if supported_tags is None else supported_tags)
    return canonicalize_name(name) == canonicalize_name(project) and not wheel_tags.isdisjoint(supported)


def main() -> None:
    args = sys.argv[1:]
    supported = None
    if len(args) == 3 and args[0] == "--tags":
        with open(args[1], encoding="utf-8") as f:
            supported = frozenset(tag for line in f if line.strip() for tag in parse_tag(line.strip()))
        args = args[2:]
    if len(args) != 1:
        raise SystemExit(f"usage: {sys.argv[0]} [--tags FILE] PROJECT < wheels-or-release-assets")
    project = args[0]
    if supported is None:
        supported = frozenset(sys_tags())
    for line in sys.stdin:
        line = line.rstrip("\n")
        target = line.split(" ", 1)[0]
        parsed = urlparse(target)
        filename = unquote(os.path.basename(parsed.path if parsed.scheme else target))
        if filename and compatible(project, filename, supported):
            print(line)


if __name__ == "__main__":
    main()
