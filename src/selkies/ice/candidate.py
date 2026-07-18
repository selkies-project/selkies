# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# This file incorporates work covered by the following copyright and
# permission notice:
#
#   Copyright (c) 2018-2019 Jeremy Lainé.
#   All rights reserved.
#
#   Redistribution and use in source and binary forms, with or without
#   modification, are permitted provided that the following conditions are met:
#
#       * Redistributions of source code must retain the above copyright notice,
#       this list of conditions and the following disclaimer.
#       * Redistributions in binary form must reproduce the above copyright notice,
#       this list of conditions and the following disclaimer in the documentation
#       and/or other materials provided with the distribution.
#       * Neither the name of aioice nor the names of its contributors may
#       be used to endorse or promote products derived from this software without
#       specific prior written permission.
#
#   THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
#   ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
#   WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
#   DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
#   FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
#   DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
#   SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
#   CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
#   OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
#   OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

import hashlib
import ipaddress
from typing import Optional


def candidate_foundation(
    candidate_type: str, candidate_transport: str, base_address: str
) -> str:
    """
    See RFC 5245 - 4.1.1.3. Computing Foundations
    """
    key = "%s|%s|%s" % (candidate_type, candidate_transport, base_address)
    return hashlib.md5(key.encode("ascii")).hexdigest()


def candidate_priority(
    candidate_component: int, candidate_type: str, local_pref: int = 65535
) -> int:
    """
    See RFC 5245 - 4.1.2.1. Recommended Formula
    """
    if candidate_type == "host":
        type_pref = 126
    elif candidate_type == "prflx":
        type_pref = 110
    elif candidate_type == "srflx":
        type_pref = 100
    else:
        type_pref = 0

    return (1 << 24) * type_pref + (1 << 8) * local_pref + (256 - candidate_component)


class Candidate:
    """
    An ICE candidate.
    """

    def __init__(
        self,
        foundation: str,
        component: int,
        transport: str,
        priority: int,
        host: str,
        port: int,
        type: str,
        related_address: Optional[str] = None,
        related_port: Optional[int] = None,
        tcptype: Optional[str] = None,
        generation: Optional[int] = None,
    ) -> None:
        self.foundation = foundation
        self.component = component
        self.transport = transport
        self.priority = priority
        self.host = host
        self.port = port
        self.type = type
        self.related_address = related_address
        self.related_port = related_port
        self.tcptype = tcptype
        self.generation = generation

    @classmethod
    def from_sdp(cls, sdp: str) -> "Candidate":
        """
        Parse a :class:`Candidate` from SDP.

        .. code-block:: python

           Candidate.from_sdp(
            '6815297761 1 udp 659136 1.2.3.4 31102 typ host generation 0')
        """
        bits = sdp.split()
        if len(bits) < 8:
            raise ValueError("SDP does not have enough properties")

        related_address: Optional[str] = None
        related_port: Optional[int] = None
        tcptype: Optional[str] = None
        generation: Optional[int] = None
        for i in range(8, len(bits) - 1, 2):
            if bits[i] == "raddr":
                related_address = bits[i + 1]
            elif bits[i] == "rport":
                related_port = int(bits[i + 1])
            elif bits[i] == "tcptype":
                tcptype = bits[i + 1]
            elif bits[i] == "generation":
                generation = int(bits[i + 1])

        return Candidate(
            foundation=bits[0],
            component=int(bits[1]),
            transport=bits[2],
            priority=int(bits[3]),
            host=bits[4],
            port=int(bits[5]),
            type=bits[7],
            related_address=related_address,
            related_port=related_port,
            tcptype=tcptype,
            generation=generation,
        )

    def to_sdp(self) -> str:
        """
        Return a string representation suitable for SDP.
        """
        sdp = "%s %d %s %d %s %d typ %s" % (
            self.foundation,
            self.component,
            self.transport,
            self.priority,
            self.host,
            self.port,
            self.type,
        )
        if self.related_address is not None:
            sdp += " raddr %s" % self.related_address
        if self.related_port is not None:
            sdp += " rport %s" % self.related_port
        if self.tcptype is not None:
            sdp += " tcptype %s" % self.tcptype
        if self.generation is not None:
            sdp += " generation %d" % self.generation
        return sdp

    def can_pair_with(self, other: "Candidate") -> bool:
        """
        A local candidate is paired with a remote candidate if and only if
        the two candidates have the same component ID and have the same IP
        address version.
        """
        a = ipaddress.ip_address(self.host)
        b = ipaddress.ip_address(other.host)
        return (
            self.component == other.component
            and self.transport.lower() == other.transport.lower()
            and a.version == b.version
        )

    def __repr__(self) -> str:
        return "Candidate(%s)" % self.to_sdp()
