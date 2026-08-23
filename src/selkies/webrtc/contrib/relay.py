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

"""Fan one live media source out to many consumers.

The relay carries whatever the source track yields opaquely — for Selkies that
is the pre-encoded `EncodedPacket` from pixelflux/pcmflux — so it needs no libav
of its own. The libav-backed players and recorders stay in `media.py`, which
Selkies does not import.
"""

import asyncio
import logging
from typing import Any, Optional

from ..mediastreams import MediaStreamError, MediaStreamTrack

logger = logging.getLogger(__name__)


class RelayStreamTrack(MediaStreamTrack):
    def __init__(
        self,
        relay: "MediaRelay",
        source: MediaStreamTrack,
        buffered: bool,
    ) -> None:
        super().__init__()
        self.kind = source.kind
        self._relay = relay
        self._source: Optional[MediaStreamTrack] = source
        self._buffered = buffered

        self._frame: Any = None
        self._queue: Optional[asyncio.Queue] = None
        self._new_frame_event: Optional[asyncio.Event] = None

        if self._buffered:
            self._queue = asyncio.Queue()
        else:
            self._new_frame_event = asyncio.Event()

    async def recv(self) -> Any:
        if self.readyState != "live":
            raise MediaStreamError

        self._relay._start(self)

        if self._buffered:
            self._frame = await self._queue.get()
        else:
            await self._new_frame_event.wait()
            self._new_frame_event.clear()

        if self._frame is None:
            self.stop()
            raise MediaStreamError
        return self._frame

    def stop(self) -> None:
        super().stop()
        if self._relay is not None:
            self._relay._stop(self)
            self._relay = None
            self._source = None


class MediaRelay:
    """
    A media source that relays one or more tracks to multiple consumers.

    This is especially useful for live tracks such as webcams or media received
    over the network.
    """

    def __init__(self) -> None:
        self.__proxies: dict[MediaStreamTrack, set[RelayStreamTrack]] = {}
        self.__tasks: dict[MediaStreamTrack, asyncio.Future[None]] = {}

    def subscribe(
        self, track: MediaStreamTrack, buffered: bool = True
    ) -> MediaStreamTrack:
        """
        Create a proxy around the given `track` for a new consumer.

        :param track: Source :class:`MediaStreamTrack` which is relayed.
        :param buffered: Whether there need a buffer between the source track and
            relayed track.

        :rtype: :class: MediaStreamTrack
        """
        proxy = RelayStreamTrack(self, track, buffered)
        self.__log_debug("Create proxy %s for source %s", id(proxy), id(track))
        if track not in self.__proxies:
            self.__proxies[track] = set()
        return proxy

    def _start(self, proxy: RelayStreamTrack) -> None:
        track = proxy._source
        if track is not None and track in self.__proxies:
            # register proxy
            if proxy not in self.__proxies[track]:
                self.__log_debug("Start proxy %s", id(proxy))
                self.__proxies[track].add(proxy)

            # start worker
            if track not in self.__tasks:
                self.__tasks[track] = asyncio.ensure_future(self.__run_track(track))

    def _stop(self, proxy: RelayStreamTrack) -> None:
        track = proxy._source
        if track is not None and track in self.__proxies:
            # unregister proxy
            self.__log_debug("Stop proxy %s", id(proxy))
            self.__proxies[track].discard(proxy)

    async def stop(self) -> None:
        """Cancel and reap every relay worker. __run_track only ends when its
        SOURCE track raises MediaStreamError; a teardown that merely drops the
        relay leaves workers pending in recv() forever, surfacing as
        'Task was destroyed but it is pending!' once they are GC'd."""
        tasks = list(self.__tasks.values())
        self.__tasks.clear()
        self.__proxies.clear()
        for task in tasks:
            task.cancel()
        for task in tasks:
            try:
                await task
            except (asyncio.CancelledError, Exception):
                pass

    def __log_debug(self, msg: str, *args: object) -> None:
        logger.debug(f"MediaRelay(%s) {msg}", id(self), *args)

    async def __run_track(self, track: MediaStreamTrack) -> None:
        self.__log_debug("Start reading source %s" % id(track))

        while True:
            try:
                frame = await track.recv()
            except MediaStreamError:
                frame = None
            for proxy in self.__proxies[track]:
                if proxy._buffered:
                    proxy._queue.put_nowait(frame)
                else:
                    proxy._frame = frame
                    proxy._new_frame_event.set()
            if frame is None:
                break

        self.__log_debug("Stop reading source %s", id(track))
        del self.__proxies[track]
        del self.__tasks[track]
