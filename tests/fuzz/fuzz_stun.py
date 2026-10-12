#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Fuzzes STUN parsing, which the ICE agent and the UDP mux run on every
datagram a peer sends, before any authentication."""
import sys

import atheris

with atheris.instrument_imports():
    from selkies.ice import stun


def TestOneInput(data):
    # The first byte chooses whether MESSAGE-INTEGRITY is checked, as it is on
    # a pair whose password is known.
    key = b"fuzz-password" if data[:1] == b"\x01" else None
    try:
        stun.parse_message(data[1:], integrity_key=key)
    except ValueError:
        pass


def main():
    atheris.Setup(sys.argv, TestOneInput)
    atheris.Fuzz()


if __name__ == "__main__":
    main()
