/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The socket worker stamps every frame ack by its own clock.
 *
 * The page and the socket worker each count time from an origin of their own,
 * and the server reads how long an ack's way back held it off the distance
 * between its stamp and the server's clock: acks stamped by both contexts
 * would read the distance between the two origins as a way back held. The
 * worker source the WebSockets core starts runs over a stand-in WebSocket and
 * a clock whose origin is not the page's, so it is pinned that an ack the page
 * hands the worker leaves with the worker's stamp, the page's frame and hold
 * kept and the worker's count of what the socket handed over behind it, and
 * that any other message leaves as the page wrote it.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */
import { CORE, post, readSource, startWorker, workerSource, type WorkerScope } from './core_source.ts';

let passed = 0;
let failed = 0;

/** Prints one check's line and counts it. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
  if (ok) passed++;
  else failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [socket-worker-ack-stamp] ${label}  ${JSON.stringify(detail)}`);
}

// The worker's clock: an origin a quarter second off the page's.
const WORKER_ORIGIN = 1_700_000_000_250;
const clock = 4321.5;
Object.assign(globalThis, {
  performance: { now: () => clock, timeOrigin: WORKER_ORIGIN },
  setInterval: (): number => 1,
  clearInterval: (): void => {},
});

/** The worker's socket: it keeps what is sent. */
class FakeWebSocket {
  static last: FakeWebSocket | null = null;
  readyState = 1;
  bufferedAmount = 0;
  sent: (string | ArrayBuffer)[] = [];
  onmessage: ((event: { data: unknown }) => void) | null = null;
  constructor() { FakeWebSocket.last = this; }
  send(d: string | ArrayBuffer): void { this.sent.push(d); }
  close(): void {}
}
Object.assign(globalThis, { WebSocket: FakeWebSocket });

const scope: WorkerScope = { postMessage: () => {}, onmessage: null };
startWorker(workerSource(readSource(CORE), 'SOCKET_WORKER_SRC'), 'socket-worker.js', scope);
post(scope, { type: 'open', url: 'ws://stub', primary: true });
const socket = FakeWebSocket.last;
if (!socket?.onmessage) throw new Error('the worker reads no socket');
// The socket hands over 1500 bytes of stream before the ack goes.
socket.onmessage({ data: new ArrayBuffer(1000) });
socket.onmessage({ data: new ArrayBuffer(500) });

const pageStamp = (1_700_000_000_000 + 4321.5).toFixed(1);
post(scope, { type: 'send', data: `CLIENT_FRAME_ACK 42 7 ${pageStamp}` });
post(scope, { type: 'send', data: 'cr' });
const [ack, other] = socket.sent.filter((d): d is string => typeof d === 'string');

const expected = `CLIENT_FRAME_ACK 42 7 ${(WORKER_ORIGIN + clock).toFixed(1)} 1500`;
check('the page\'s ack leaves stamped by the worker\'s clock, its count behind it', ack === expected, ack);
check('any other message leaves as the page wrote it', other === 'cr', other);
console.log(`[socket-worker-ack-stamp] ${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
