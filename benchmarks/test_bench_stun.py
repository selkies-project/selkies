# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Benchmarks of the STUN framing the ICE agent and the UDP mux run on every
connectivity check, with and without MESSAGE-INTEGRITY."""
from selkies.ice import stun

KEY = b"asd88fgpdd777uzjYhagZg9A"
TRANSACTION_ID = b"\x01" * 12


def _binding_request() -> stun.Message:
    message = stun.Message(stun.Method.BINDING, stun.Class.REQUEST, transaction_id=TRANSACTION_ID)
    message.attributes["USERNAME"] = "8hhY:remoteufrag"
    message.attributes["PRIORITY"] = 1853824767
    message.attributes["ICE-CONTROLLING"] = 0x1122334455667788
    message.attributes["USE-CANDIDATE"] = None
    return message


def _binding_response() -> bytes:
    message = stun.Message(stun.Method.BINDING, stun.Class.RESPONSE, transaction_id=TRANSACTION_ID)
    message.attributes["XOR-MAPPED-ADDRESS"] = ("203.0.113.7", 61000)
    message.add_message_integrity(KEY)
    return bytes(message)


BINDING_RESPONSE = _binding_response()


def test_stun_binding_request_serialize(benchmark):
    def serialize():
        message = _binding_request()
        message.add_message_integrity(KEY)
        return bytes(message)

    assert benchmark(serialize)


def test_stun_parse(benchmark):
    message = benchmark(stun.parse_message, BINDING_RESPONSE)
    assert message.attributes["XOR-MAPPED-ADDRESS"] == ("203.0.113.7", 61000)


def test_stun_parse_with_integrity(benchmark):
    message = benchmark(stun.parse_message, BINDING_RESPONSE, integrity_key=KEY)
    assert message.attributes["XOR-MAPPED-ADDRESS"] == ("203.0.113.7", 61000)
