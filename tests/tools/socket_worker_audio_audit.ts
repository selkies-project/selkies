/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The socket worker's audio route, driven as plain logic.
 *
 * The worker source the WebSockets core starts runs over a stand-in WebSocket,
 * so it is pinned that audio packets and the server's quiet mark go down the
 * decoder's port in the order they arrived, and that none is handed to the page.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */
import { type Fields, fields } from '../../web/selkies-web-core/lib/narrow.ts';
import { CORE, post, readSource, startWorker, workerSource, type WorkerScope } from './core_source.ts';

let passed = 0;
let failed = 0;

/** Prints one check's line and counts it. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
  if (ok) passed++;
  else failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [socket-worker-audio] ${label}  ${JSON.stringify(detail)}`);
}

/** The worker's socket, as far as the audit drives it. */
class FakeWebSocket {
  static last: FakeWebSocket | null = null;
  readyState = 1;
  onmessage: ((event: { data: unknown }) => void) | null = null;
  constructor() { FakeWebSocket.last = this; }
  send(): void {}
  close(): void {}
}
Object.assign(globalThis, { WebSocket: FakeWebSocket });

const toPage: Fields[] = [];
const toDecoder: Fields[] = [];
const scope: WorkerScope = { postMessage: (m) => toPage.push(fields(m)), onmessage: null };
startWorker(workerSource(readSource(CORE), 'SOCKET_WORKER_SRC'), 'socket-worker.js', scope);
post(scope, { type: 'audioPort', port: { postMessage: (m: unknown) => toDecoder.push(fields(m)) } });
post(scope, { type: 'open', url: 'ws://stub', primary: true });
const socket = FakeWebSocket.last;
if (!socket?.onmessage) throw new Error('the worker opened no socket');
const sent = [[0x01, 0x00, 0xfc, 0x01, 0x02], [0x01, 0x00, 0xfc, 0x03], [0x01, 0x80]];
for (const bytes of sent) socket.onmessage({ data: new Uint8Array(bytes).buffer });
const got = toDecoder.map((m) => (m.buffer instanceof ArrayBuffer ? Array.from(new Uint8Array(m.buffer)) : null));
const pageBuffers = toPage.filter((m) => m.type === 'message').length;
check('audio and the quiet mark go down the decoder\'s port in arrival order', JSON.stringify(got) === JSON.stringify(sent), got);
check('neither is handed to the page', pageBuffers === 0, pageBuffers);
console.log(`[socket-worker-audio] ${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
