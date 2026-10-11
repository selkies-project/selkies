#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Fuzzes parsing of the session descriptions a peer sends over signaling."""
import sys

import atheris

with atheris.instrument_imports():
    from selkies.webrtc import sdp


def TestOneInput(data):
    try:
        sdp.SessionDescription.parse(data.decode("utf-8", "replace"))
    except ValueError:
        pass


def main():
    atheris.Setup(sys.argv, TestOneInput)
    atheris.Fuzz()


if __name__ == "__main__":
    main()
