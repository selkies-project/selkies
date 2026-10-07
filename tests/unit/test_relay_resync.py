#!/usr/bin/env python3
"""A WebSockets relay serves a page on a narrow link without a key frame on the shared stream.

Where the encoder names the frame each one predicts from, a page that does not own its
display is left the frames its link has no room for. The frames predicting from them are
held back, the encoder is told the run's first frame is lost once the page has room again
(or once the run nears what the encoder still predicts from), and the page resumes on the
first frame predicting past it. The owner's relay is never thinned this way, a stripe
stream still skips ahead to the next key frame past the byte budget, and so does a run the
encoder never predicts past within a second.
"""
import os
import sys
from types import SimpleNamespace

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "src"))

from selkies import websockets_mode as wm  # noqa: E402

failures = 0
ROOM = wm.VIDEO_RELAY_ROOM_FRAMES
REACH = wm.VIDEO_RELAY_REACH_FRAMES
FAR = wm.VIDEO_RELAY_FAR_FRAMES
WRAP = wm.FRAME_NUM_WRAP


def check(name: str, ok: bool, detail: object = "") -> None:
    global failures
    print(("PASS  " if ok else "FAIL  ") + name + (f"  {detail}" if detail != "" and not ok else ""))
    if not ok:
        failures += 1


class Clock:
    def __init__(self) -> None:
        self.t = 100.0

    def __call__(self) -> float:
        return self.t


class Socket:
    """What the relay's socket still owes the network (`_ws_write_backlog`)."""

    def __init__(self) -> None:
        self.unsent = 0


class Page:
    """The client's socket, keyed by its ping state (`_uplink_session_state`)."""

    def __init__(self, sock: Socket) -> None:
        self.sock = sock


def chunk(frame_id: int, reference: int, key: bool = False, size: int = 400, row: int = 0,
          codec: int = wm.WIRE_H264) -> dict:
    """A 0x04 chunk with the wire header the relay reads."""
    head = bytes([0x04, (codec << 4) | (0x01 if key else 0x00), frame_id >> 8, frame_id & 0xFF, row >> 8, row & 0xFF,
                  0x07, 0x80, 0x04, 0x38, reference >> 8, reference & 0xFF])
    data = head + bytes(size)
    return {'data': memoryview(data), 'owner': data, 'frame_id': frame_id}


def relay(owner: bool = False, taken: bool = True, budget: int = 100_000):
    """A relay whose server records the lost reports as (frame, within reach)."""
    reports = []
    sock = Socket()

    def relay_lost(ws, display_id, frame_id, reach):
        if not reach and not server.taken:
            return False
        reports.append((frame_id, reach))
        return True

    server = SimpleNamespace(taken=taken, owner=owner, clients=set(), _relay_lost=relay_lost)
    server._owns_display = lambda ws, display_id: server.owner
    ws = Page(sock)
    r = wm._VideoRelay(server, "primary", ws, budget)
    r.verdict = SimpleNamespace(note=lambda *a: None)
    return r, server, reports, sock


def hand(r, sock, count: int = 1) -> None:
    """The drain task hands the oldest queued frames to the socket, which sends none yet."""
    for _ in range(count):
        item = r.backlog.popleft()
        n = len(item['data'])
        r.backlog_bytes -= n
        r.written += n
        r.marks.append(r.written)
        sock.unsent += n


def full(r, sock) -> None:
    """Three of the page's frames handed to its socket and not sent yet."""
    r.written += 3000
    r.marks.extend([r.written - 2000, r.written - 1000, r.written])
    sock.unsent += 3000


def ids(r) -> list:
    return [(it['data'][2] << 8) | it['data'][3] for it in r.backlog]


def main() -> int:
    clock = Clock()
    wm.time.monotonic = clock
    wm._ws_write_backlog = lambda ws: ws.sock.unsent

    # A page beside the owner on a link that stops taking frames.
    r, server, reports, sock = relay()
    asks = [r.offer(chunk(1, 1, key=True))]
    hand(r, sock)
    asks += [r.offer(chunk(n, n - 1)) for n in (2, 3)]
    check("frames go out while the page has room for them", ids(r) == [2, 3], ids(r))
    asks += [r.offer(chunk(n, n - 1)) for n in (4, 5)]
    check(f"with {ROOM} of its frames on their way, the next is left out with those predicting from it",
          ids(r) == [2, 3] and r.lost_first == 4, (ids(r), r.lost_first))
    check("and the encoder is not told while the page has no room", reports == [], reports)
    hand(r, sock, 2)
    sock.unsent = 0
    asks += [r.offer(chunk(6, 5))]
    check("once the frames ahead have left, the encoder is told the run's first frame is lost",
          reports == [(4, True)], reports)
    asks += [r.offer(chunk(7, 6))]
    check("a frame of the run is still held back, and the report not repeated",
          ids(r) == [] and reports == [(4, True)], (ids(r), reports))
    asks += [r.offer(chunk(8, 3))]
    check("the frame the encoder predicted from the last one sent goes out", ids(r) == [8], ids(r))
    asks += [r.offer(chunk(9, 8))]
    check("and the stream goes on from it", ids(r) == [8, 9], ids(r))
    check("without a key frame asked for", not any(asks), asks)

    # A link that stays full: the run is reported at the reach, and the frame answering it
    # goes out even without room, so the encoder never has to fall back to a key frame.
    r, server, reports, sock = relay()
    r.offer(chunk(1, 1, key=True))
    hand(r, sock)
    for n in range(2, 2 + ROOM - 1):
        r.offer(chunk(n, n - 1))
    first = 2 + ROOM - 1
    for n in range(first, first + REACH - 1):
        r.offer(chunk(n, n - 1))
    check("a run shorter than the reach waits for room", reports == [], reports)
    r.offer(chunk(first + REACH - 1, first + REACH - 2))
    check("at the reach the encoder is told, while it holds the frame to predict from",
          reports == [(first, True)], reports)
    queued = ids(r)
    r.offer(chunk(first + REACH, first - 1))
    check("the frame answering it goes out though the page has no room",
          ids(r) == queued + [first + REACH], ids(r))

    # A page far behind is not answered at the reach: once it has room the run is past it,
    # and its report is taken as a key-frame request is.
    r, server, reports, sock = relay(taken=False)
    for n in range(1, 1 + FAR):
        r.offer(chunk(n, n, key=True))
    hand(r, sock, len(r.backlog))
    first = 1 + FAR
    for n in range(first, first + REACH + 2):
        r.offer(chunk(n, n - 1))
    check("a page far behind is not answered at the reach", reports == [], reports)
    sock.unsent = 0
    r.offer(chunk(first + REACH + 2, first + REACH + 1))
    check("a report past the reach waits while the page's key-frame requests are spent", reports == [], reports)
    server.taken = True
    r.offer(chunk(first + REACH + 3, first + REACH + 2))
    check("and is made on the next held frame once taken", reports == [(first, False)], reports)

    # A frame_num can wrap only a multiple of FRAME_NUM_WRAP frames past a key frame, and the
    # encoder answers a run covering that frame with a key frame: a run open just ahead of it
    # is reported at once, and the frame itself is never left out.
    r, server, reports, sock = relay()
    r.offer(chunk(1, 1, key=True))
    for n in range(2, WRAP - 1):
        hand(r, sock)
        sock.unsent = 0
        r.offer(chunk(n, n - 1))
    hand(r, sock)
    full(r, sock)
    n = WRAP - 1
    r.offer(chunk(n, n - 1))
    check(f"a run opened {WRAP - 2} frames past a key frame is reported at once, room or not",
          r.lost_first == n and reports == [(n, True)], (r.lost_first, reports))
    r.offer(chunk(n + 1, n - 1))
    r.offer(chunk(n + 2, n + 1))
    check("the frame answering it, and the one where frame_num may wrap, go out without room",
          ids(r)[-2:] == [n + 1, n + 2] and r.lost_first is None, (ids(r), r.lost_first))
    r.offer(chunk(n + 3, n + 2))
    check("past it a page without room is left frames again", r.lost_first == n + 3, r.lost_first)
    r, server, reports, sock = relay()
    av1 = 4
    r.offer(chunk(1, 1, key=True, codec=av1))
    for n in range(2, WRAP + 1):
        hand(r, sock)
        sock.unsent = 0
        r.offer(chunk(n, n - 1, codec=av1))
    hand(r, sock)
    full(r, sock)
    r.offer(chunk(WRAP + 1, WRAP, codec=av1))
    check("a codec without frame_num is left that frame like any other, and not reported early",
          r.lost_first == WRAP + 1 and reports == [], (r.lost_first, reports))

    # Arrival is read end to end: a ping written behind the page's frames that has gone
    # unanswered past the round trip says a queue stands on the path, which no local count sees.
    r, server, reports, sock = relay()
    state = wm._uplink_session_state(r.ws)
    state.update(seq=1, rtt_us=20_000, answered=clock.t - 1.0)
    r.offer(chunk(1, 1, key=True))
    hand(r, sock)
    sock.unsent = 0
    state["pending"][b"behind"] = clock.t - 0.3
    r.offer(chunk(2, 1))
    check(f"a ping unanswered {wm.VIDEO_RELAY_LAG_SECONDS * 1000:.0f} ms past the round trip leaves the page "
          "no room", r.lost_first == 2, r.lost_first)
    del state["pending"][b"behind"]
    state.update(seq=2, answered=clock.t - 0.3)
    r.offer(chunk(3, 2))
    check("its pong gives the room back, and the run is reported", reports == [(2, True)], reports)
    state["pending"][b"lost"] = clock.t - 5.0
    state["answered"] = clock.t - 0.1
    check("a ping older than the newest one answered was lost, and stands for no queue", r._lag() == 0.0,
          r._lag())

    # A run the encoder never predicts past falls back to a key frame.
    r, server, reports, sock = relay()
    r.offer(chunk(1, 1, key=True))
    hand(r, sock)
    for n in range(2, 2 + ROOM + 2):
        r.offer(chunk(n, n - 1))
    clock.t += wm.VIDEO_RELAY_LOST_RECOVERY_SECONDS + 0.1
    asked = r.offer(chunk(2 + ROOM + 2, 2 + ROOM + 1))
    check("after the recovery window a held frame asks for a key frame", asked)
    r.offer(chunk(20, 1))
    check("and the client waits for it, gated", ids(r)[-1:] != [20], ids(r))
    r.offer(chunk(21, 21, key=True))
    check("the key frame goes out and reopens the stream", ids(r)[-1:] == [21], ids(r))

    # The owner's relay is not thinned: its acknowledgements hold it instead.
    clock.t += 5
    r, server, reports, sock = relay(owner=True)
    r.offer(chunk(1, 1, key=True))
    hand(r, sock)
    for n in range(2, 12):
        r.offer(chunk(n, n - 1))
    check("the owner is sent every frame however many are on their way",
          ids(r) == list(range(2, 12)) and reports == [], (ids(r), reports))

    # A stripe stream names its own frame: a drop skips ahead to the next key frame.
    r, server, reports, sock = relay(budget=2000)
    r.offer(chunk(1, 1, key=True))
    hand(r, sock)
    asks = [r.offer(chunk(n, n)) for n in range(2, 9)]
    check("an untracked stream is queued to its budget, then asks for a key frame", any(asks), asks)
    check("and tells the encoder nothing", reports == [], reports)
    r.offer(chunk(9, 9))
    check("its deltas wait for that key frame", ids(r) == [], ids(r))

    print(f"\n{failures} failure(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
