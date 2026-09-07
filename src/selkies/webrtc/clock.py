# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# This file incorporates work covered by the following copyright and
# permission notice:
#
#   Copyright (c) Jeremy Lainé.
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
#       * Neither the name of aiortc nor the names of its contributors may
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

import datetime
import time

NTP_EPOCH = datetime.datetime(1900, 1, 1, tzinfo=datetime.timezone.utc)
# Seconds from the NTP epoch to the Unix epoch.
NTP_UNIX_OFFSET = 2208988800


def current_datetime() -> datetime.datetime:
    return datetime.datetime.now(datetime.timezone.utc)


def current_ms() -> int:
    delta = current_datetime() - NTP_EPOCH
    return int(delta.total_seconds() * 1000)


def current_ntp_time() -> int:
    """The NTP timestamp now, straight from the system clock: every RTP
    packet stamps one, so no datetime is built on the way."""
    now = time.time()
    seconds = int(now)
    return ((seconds + NTP_UNIX_OFFSET) << 32) | int((now - seconds) * (1 << 32))


def datetime_from_ntp(ntp: int) -> datetime.datetime:
    seconds = ntp >> 32
    microseconds = ((ntp & 0xFFFFFFFF) * 1000000) / (1 << 32)
    return NTP_EPOCH + datetime.timedelta(seconds=seconds, microseconds=microseconds)


def datetime_to_ntp(dt: datetime.datetime) -> int:
    delta = dt - NTP_EPOCH
    high = int(delta.total_seconds())
    low = round((delta.microseconds * (1 << 32)) // 1000000)
    return (high << 32) | low
