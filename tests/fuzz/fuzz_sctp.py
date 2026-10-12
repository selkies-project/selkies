#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Fuzzes the SCTP association with the packets a peer sends over the data
channel's DTLS transport: parsing, then every chunk handler, on a started
server-side association that the first byte's low bit has the harness
establish first. The checksum and the verification tag are made to match, so
inputs reach the handlers rather than stopping at those checks."""
import asyncio
import struct
import sys
import types

import atheris

with atheris.instrument_imports():
    from google_crc32c import value as crc32c
    from selkies.webrtc.rtcsctptransport import (
        SCTP_STATE_COOKIE,
        CookieEchoChunk,
        InitChunk,
        RTCSctpTransport,
        parse_packet,
        serialize_packet,
    )


class _Dtls:
    state = "connected"
    transport = types.SimpleNamespace(role="controlled")

    def __init__(self):
        self.sent = []

    def _register_data_receiver(self, receiver):
        pass

    def _unregister_data_receiver(self, receiver):
        pass

    async def _send_data(self, data):
        self.sent.append(data)


async def establish(sctp, dtls):
    init = InitChunk()
    init.initiate_tag = 1
    init.advertised_rwnd = 131072
    init.outbound_streams = init.inbound_streams = 65535
    init.initial_tsn = 1000
    await sctp._handle_data(serialize_packet(5000, 5000, 0, init))
    cookie = dict(parse_packet(dtls.sent[-1])[3][0].params)[SCTP_STATE_COOKIE]
    echo = CookieEchoChunk(body=cookie)
    await sctp._handle_data(serialize_packet(5000, 5000, sctp._local_verification_tag, echo))


async def receive(data):
    dtls = _Dtls()
    sctp = RTCSctpTransport(dtls, port=5000)
    await sctp.start(None, 5000)
    if data[0] & 1:
        await establish(sctp, dtls)
    data = data[1:]
    # A zero tag is kept for INIT; any other is replaced by the association's.
    tag = data[4:8] if data[4:8] == b"\0\0\0\0" else struct.pack("!L", sctp._local_verification_tag)
    packet = data[:4] + tag + b"\0\0\0\0" + data[12:]
    await sctp._handle_data(packet[:8] + struct.pack("<L", crc32c(packet)) + packet[12:])


def TestOneInput(data):
    if len(data) >= 13:
        asyncio.run(receive(data))


def main():
    atheris.Setup(sys.argv, TestOneInput)
    atheris.Fuzz()


if __name__ == "__main__":
    main()
