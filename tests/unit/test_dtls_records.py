#!/usr/bin/env python3
"""Complete DTLS records are delivered without a later network datagram.

Real OpenSSL connections and the SCTP receiver exercise authenticated records;
certificates, ciphertext, and synthetic message bodies stay in memory.
"""
import asyncio
import hashlib
import os
import sys
from typing import Any

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
from OpenSSL import SSL
from selkies.webrtc import rtcdtlstransport as dtls, rtcsctptransport as sctp
from selkies.webrtc.rtcdatachannel import RTCDataChannel, RTCDataChannelParameters


def drain(conn: SSL.Connection) -> bytes:
    """Collect a completed memory-BIO flight without storing it."""
    data = bytearray()
    while True:
        try:
            data.extend(conn.bio_read(65536))
        except SSL.WantReadError:
            return bytes(data)


def records(data: bytes) -> list:
    """Require complete DTLS records and expose only header metadata."""
    rows, pos = [], 0
    while pos < len(data):
        assert len(data) - pos >= 13, 'incomplete_record_header'
        size = int.from_bytes(data[pos + 11:pos + 13], 'big')
        end = pos + 13 + size
        assert end <= len(data), 'incomplete_record_body'
        rows.append(dict(kind=data[pos], version=data[pos + 1:pos + 3].hex(),
                         epoch=int.from_bytes(data[pos + 3:pos + 5], 'big'),
                         sequence=int.from_bytes(data[pos + 5:pos + 11], 'big'),
                         bytes=end-pos, sha256=hashlib.sha256(data[pos:end]).hexdigest()))
        pos = end
    return rows


def pair() -> tuple:
    """Complete a bounded real DTLS handshake entirely in memory."""
    certs = [dtls.RTCCertificate.generateCertificate() for _ in range(2)]
    client, server = [SSL.Connection(c._create_ssl_context(dtls.SRTP_PROFILES), None) for c in certs]
    client.set_connect_state()
    server.set_accept_state()
    ready = [False, False]
    for _ in range(32):
        for i, (a, b) in enumerate([(client, server), (server, client)]):
            if not ready[i]:
                try:
                    a.do_handshake()
                    ready[i] = True
                except SSL.WantReadError:
                    pass
            data = drain(a)
            if data:
                b.bio_write(data)
        if all(ready):
            return client, server, certs[1]
    raise AssertionError('handshake_did_not_finish')


class Network:
    """Supply exact datagrams to the production receive method."""
    role = 'controlled'

    def __init__(self) -> None:
        self.incoming, self.outgoing, self.receives = [], [], 0

    async def _recv(self) -> bytes:
        assert self.incoming, 'unexpected_network_receive'
        self.receives += 1
        return self.incoming.pop(0)

    async def _send(self, data: bytes) -> None:
        self.outgoing.append(data)


def setup() -> tuple:
    """Connect real DTLS decryption to a real SCTP data channel."""
    sender, receiver, cert = pair()
    network = Network()
    transport = dtls.RTCDtlsTransport(network, [cert])
    transport._ssl, transport.encrypted, transport._state = receiver, True, dtls.State.CONNECTED
    application = sctp.RTCSctpTransport(transport)
    application._last_received_tsn = 99
    application._local_verification_tag = 123
    application._remote_verification_tag = 456
    application._remote_port = 5000
    channel = RTCDataChannel(application, RTCDataChannelParameters(label='input', id=0), False)
    channel._setReadyState('open')
    application._data_channels[0] = channel
    messages = []
    channel.on('message', lambda data: messages.append(hashlib.sha256(data.encode()).hexdigest()))
    transport._register_data_receiver(application)
    return sender, receiver, transport, application, network, messages


def payload(number: int) -> tuple:
    """Serialize one ordered synthetic SCTP DATA message."""
    body = ('synthetic_input_%d' % number).encode()
    chunk = sctp.DataChunk(flags=sctp.SCTP_DATA_FIRST_FRAG | sctp.SCTP_DATA_LAST_FRAG | sctp.SCTP_DATA_SACK_IMMEDIATELY)
    chunk.tsn, chunk.stream_id, chunk.stream_seq, chunk.protocol, chunk.user_data = 100 + number, 0, number, sctp.WEBRTC_STRING, body
    return sctp.serialize_packet(5000, 5000, 123, chunk), hashlib.sha256(body).hexdigest()


def encrypt(sender: SSL.Connection, number: int) -> tuple:
    """Generate one authenticated application record."""
    data, digest = payload(number)
    assert sender.send(data) == len(data)
    wire = drain(sender)
    record = records(wire)
    assert len(record) == 1 and record[0]['kind'] == 23
    return wire, digest


def close_application(application: Any) -> None:
    """Cancel the private receiver delayed-ack timer."""
    if application._sack_delay_handle:
        application._sack_delay_handle.cancel()


async def scenario(res: H.Results) -> None:
    """Require eager ordered delivery and preserved duplicate/close behavior."""
    sender, receiver, transport, app, net, delivered = setup()
    try:
        a, first = encrypt(sender, 0)
        b, second = encrypt(sender, 1)
        net.incoming.append(a + b)
        await transport._recv_next()
        res.check("bundled records delivered by one network receive", delivered == [first, second] and net.receives == 1)
        c, third = encrypt(sender, 2)
        net.incoming.append(c)
        await transport._recv_next()
        res.check("next ordinary record delivered immediately", delivered == [first, second, third] and net.receives == 2)
        net.incoming.append(c)
        await transport._recv_next()
        res.check("duplicate yields no duplicate SCTP message", delivered == [first, second, third])
        sender.shutdown()
        net.incoming.append(drain(sender))
        closed = False
        try:
            await transport._recv_next()
        except ConnectionError:
            closed = True
        res.check("close notify preserves connection closure", closed)
    finally:
        close_application(app)


def main() -> bool:
    res = H.Results("dtls-records")
    asyncio.run(scenario(res))
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
