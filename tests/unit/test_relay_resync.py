#!/usr/bin/env python3
"""A WebSockets relay serves a page on a narrow link without a key frame on the shared stream.

Where the encoder names the frame each one predicts from, a page that does not own its
display is left the frames its link has no room for. The frames predicting from them are
held back, the encoder is told the run's first frame is lost once the page has room again
(or once the run nears what the encoder still predicts from), and the page resumes on the
first frame predicting past it. The owner's relay is never thinned this way, a stripe
stream still skips ahead to the next key frame past the byte budget, and so does a run the
encoder never predicts past within a second.

The encoder is told of each frame every page's relay wrote (`CommonFrames`), and an anchor
it flags as predicting from such a frame goes to a page without room and ends its run. A
frame predicting from one the page was not sent, however long ago, is held back.
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
          codec: int = wm.WIRE_H264, anchor: bool = False) -> dict:
    """A 0x04 chunk with the wire header the relay reads."""
    kind = (0x01 if key else 0x00) | (wm.FRAME_ANCHOR if anchor else 0)
    head = bytes([0x04, (codec << 4) | kind, frame_id >> 8, frame_id & 0xFF, row >> 8, row & 0xFF,
                  0x07, 0x80, 0x04, 0x38, reference >> 8, reference & 0xFF])
    data = head + bytes(size)
    return {'data': memoryview(data), 'owner': data, 'frame_id': frame_id}


def relay(owner: bool = False, taken: bool = True, budget: int = 100_000, server=None):
    """A relay whose server records the lost reports as (frame, within reach), and the
    frames every relay's page holds as `server.acked`; `server` shares another's."""
    reports = []
    sock = Socket()

    def relay_lost(ws, display_id, frame_id, reach):
        if not reach and not server.taken:
            return False
        reports.append((frame_id, reach))
        return True

    if server is None:
        server = SimpleNamespace(taken=taken, owner=owner, clients=set(), _relay_lost=relay_lost,
                                 acked=[], common_frames={})
        server.common_frames_for = lambda display_id: server.common_frames.setdefault(
            display_id, wm.CommonFrames(server.acked.append))
        server._owns_display = lambda ws, display_id: server.owner is ws
    ws = Page(sock)
    if owner:
        server.owner = ws
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
        r._hold(item['data'])


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
    r.offer(chunk(1 + FAR, FAR))  # the frame after an H.264 key frame goes out
    first = 2 + FAR
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
    # is reported at once, and the frame itself is never left out, nor the frame after a key
    # frame, which an encoder keeping two long-term references marks into the second.
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
    r.offer(chunk(2, 1))
    hand(r, sock, 2)
    sock.unsent = 0
    state["pending"][b"behind"] = clock.t - 0.3
    r.offer(chunk(3, 2))
    check(f"a ping unanswered {wm.VIDEO_RELAY_LAG_SECONDS * 1000:.0f} ms past the round trip leaves the page "
          "no room", r.lost_first == 3, r.lost_first)
    del state["pending"][b"behind"]
    state.update(seq=2, answered=clock.t - 0.3)
    r.offer(chunk(4, 3))
    check("its pong gives the room back, and the run is reported", reports == [(3, True)], reports)
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

    # The encoder is told of each frame every page holds: the owner's and a page's relay
    # sharing the display's CommonFrames.
    o, server, _, osock = relay(owner=True)
    r, _, reports, sock = relay(server=server)
    for q in (o, r):
        q.offer(chunk(1, 1, key=True))
    hand(o, osock)
    check("a key frame one page holds is not yet common", server.acked == [], server.acked)
    hand(r, sock)
    check("once every page holds it the encoder is told", server.acked == [1], server.acked)
    for n in (2, 3):
        for q, qs in ((o, osock), (r, sock)):
            q.offer(chunk(n, n - 1))
            hand(q, qs)
            qs.unsent = 0
    check("and of each frame after it", server.acked == [1, 2, 3], server.acked)
    full(r, sock)
    for n in (4, 5, 6):
        o.offer(chunk(n, n - 1))
        hand(o, osock)
        r.offer(chunk(n, n - 1))
    check("a page without room is left frames, which are not common",
          r.lost_first == 4 and server.acked == [1, 2, 3], (r.lost_first, server.acked))
    o.offer(chunk(12, 3, anchor=True))
    hand(o, osock)
    r.offer(chunk(12, 3, anchor=True))
    check("an anchor predicting from a common frame goes to the page without room, ending its run",
          ids(r)[-1:] == [12] and r.lost_first is None, (ids(r), r.lost_first))
    hand(r, sock)
    check("and is common once both hold it", server.acked[-1:] == [12], server.acked)
    for n in range(13, 13 + REACH - 1):
        o.offer(chunk(n, n - 1))
        r.offer(chunk(n, n - 1))
    check("a page without room is left the frames after it", r.lost_first == 13 and reports == [],
          (r.lost_first, reports))
    r.marks.extend(range(FAR))
    o.offer(chunk(24, 12, anchor=True))
    r.offer(chunk(24, 12, anchor=True))
    check("a page too far behind is not sent an anchor, the run now reported as from it",
          r.lost_first == 24 and ids(r)[-1:] != [24], (r.lost_first, ids(r)))
    r.offer(chunk(26, 4))
    check("a frame predicting from one the page was not sent is held back, however long ago",
          ids(r)[-1:] != [26] and r.lost_first == 24, (ids(r), r.lost_first))
    r.flush_for_gate()
    o.offer(chunk(25, 24))
    hand(o, osock, len(o.backlog))
    check("a page waiting for a key frame holds no frame back: the owner's are common",
          server.acked[-1:] == [25], server.acked)
    r.stop()
    o.stop()

    # CommonFrames on its own: a page counts from the key frame it is sent, each frame every
    # page holds is told once, and a page that leaves holds nothing back.
    acked = []
    common = wm.CommonFrames(acked.append)
    a, b = object(), object()
    common.join(a)
    common.join(b)
    common.hold(a, 7, key=True)
    check("a key frame one joined page holds is not common", acked == [], acked)
    common.hold(b, 7, key=True)
    common.hold(b, 7, key=True)
    common.hold(a, 8)
    common.hold(b, 8)
    check("every frame both hold is told once", acked == [7, 8], acked)
    common.leave(b)
    common.hold(a, 9)
    check("the frames of a page alone are common", acked == [7, 8, 9], acked)
    common.hold(a, 7, key=True)
    check("the same key frame again is not told twice", acked == [7, 8, 9], acked)

    # The frame after an H.264 key frame goes out without room: an encoder keeping two
    # long-term references marks it into the second.
    r, server, reports, sock = relay()
    r.offer(chunk(1, 1, key=True))
    full(r, sock)
    r.offer(chunk(2, 1))
    check("the frame after a key frame goes out though the page has no room", ids(r)[-1:] == [2]
          and r.lost_first is None, (ids(r), r.lost_first))
    r.offer(chunk(3, 2))
    check("the one after it does not", r.lost_first == 3, r.lost_first)

    # With anchors running, a run waits for room past the second a key frame would end it at.
    r, server, reports, sock = relay()
    r.offer(chunk(1, 1, key=True))
    r.offer(chunk(2, 1))
    hand(r, sock, 2)
    sock.unsent = 0
    r.offer(chunk(12, 2, anchor=True))
    hand(r, sock)
    full(r, sock)
    r.marks.extend(range(FAR))
    for n in range(13, 16):
        r.offer(chunk(n, n - 1))
    clock.t += wm.VIDEO_RELAY_LOST_RECOVERY_SECONDS + 0.5
    asked = r.offer(chunk(16, 15))
    check("an anchored run is not answered with a key frame after a second", not asked
          and r.lost_first == 13, (asked, r.lost_first))
    clock.t += wm.VIDEO_RELAY_ANCHORED_RECOVERY_SECONDS
    check("but is past its own limit", r.offer(chunk(17, 16)) is True)

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
