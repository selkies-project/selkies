/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The socket worker's `_sent,<n>` probe, driven as plain logic.
 *
 * Firefox takes each pong's payload off `bufferedAmount` and then refuses to
 * take off a message larger than what is left, so the message stays counted
 * after it went out. The worker source the WebSockets core starts runs over a
 * stand-in WebSocket, a virtual clock and manual timers, so it is pinned that a
 * count staying up is probed, that the server's echo discounts what the count
 * still holds of the bytes sent before the probe and nothing sent after it,
 * that a stale echo changes nothing, and that a browser whose count is honest
 * reports exactly what it did before.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */
import { type Fields, fields, numberOr } from '../../web/selkies-web-core/lib/narrow.ts';
import { CORE, post, readSource, startWorker, workerSource, type WorkerScope } from './core_source.ts';

let passed = 0;
let failed = 0;

/** Prints one check's line and counts it. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
  if (ok) passed++;
  else failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [socket-worker-sent-probe] ${label}  ${JSON.stringify(detail)}`);
}

// A virtual clock and timers that run only when the audit advances it.
let clock = 1000;
const timers = new Map<number, { fn: () => void; ms: number; due: number }>();
let nextTimer = 1;
Object.assign(globalThis, {
  performance: { now: () => clock, timeOrigin: 0 },
  setInterval: (fn: () => void, ms: number): number => { timers.set(nextTimer, { fn, ms, due: clock + ms }); return nextTimer++; },
  clearInterval: (id: number): void => { timers.delete(id); },
});

/** Moves the clock on, running each timer that falls due in turn. */
function advance(ms: number): void {
  const end = clock + ms;
  for (;;) {
    let next: { fn: () => void; ms: number; due: number } | null = null;
    for (const t of timers.values()) if (t.due <= end && (!next || t.due < next.due)) next = t;
    if (!next) break;
    clock = next.due;
    next.due += next.ms;
    next.fn();
  }
  clock = end;
}

/** The worker's socket: it counts what is sent, and the audit sets what stays counted. */
class FakeWebSocket {
  static last: FakeWebSocket | null = null;
  readyState = 1;
  bufferedAmount = 0;
  sent: (string | ArrayBuffer)[] = [];
  onmessage: ((event: { data: unknown }) => void) | null = null;
  constructor() { FakeWebSocket.last = this; }
  send(d: string | ArrayBuffer): void {
    this.sent.push(d);
    this.bufferedAmount += typeof d === 'string' ? Buffer.byteLength(d) : d.byteLength;
  }
  close(): void {}
}
Object.assign(globalThis, { WebSocket: FakeWebSocket });

const toPage: Fields[] = [];
const scope: WorkerScope = { postMessage: (m) => toPage.push(fields(m)), onmessage: null };
startWorker(workerSource(readSource(CORE), 'SOCKET_WORKER_SRC'), 'socket-worker.js', scope);

/** The count the worker last reported to the page. */
const lastReport = (): number | undefined => numberOr(toPage.filter((m) => m.type === 'buffered').at(-1)?.amount);
/** The probes a socket carried. */
const probes = (s: FakeWebSocket): string[] => s.sent.filter((d): d is string => typeof d === 'string' && d.startsWith('_sent,'));
/** The socket the worker opened last. */
const opened = (): FakeWebSocket => {
  const s = FakeWebSocket.last;
  if (!s) throw new Error('the worker opened no socket');
  return s;
};
/** Hands the worker's socket a message from the server. */
const receive = (s: FakeWebSocket, data: string): void => {
  if (!s.onmessage) throw new Error('the worker reads no socket');
  s.onmessage({ data });
};

// Firefox: a 200 kB message goes out, a pong takes 8 bytes off, the message's
// own acknowledgement is refused, and 199992 bytes stay counted.
post(scope, { type: 'open', url: 'ws://stub', primary: true });
let s = opened();
post(scope, { type: 'send', data: new ArrayBuffer(200000) });
s.bufferedAmount = 199992;
advance(150);
const noProbeEarly = probes(s).length;
advance(200);
const firstProbes = probes(s);
const probeBytes = Buffer.byteLength(firstProbes[0] || '');
// The probe went out and was acknowledged (its own size came off), then a
// 50 kB message is queued behind it before the echo arrives.
s.bufferedAmount -= probeBytes;
post(scope, { type: 'send', data: new ArrayBuffer(50000) });
const pageBefore = toPage.filter((m) => m.type === 'message').length;
receive(s, '_sent,999');
const staleEcho = lastReport();
receive(s, firstProbes[0] ?? '');
const afterEcho = lastReport();
const echoForwarded = toPage.filter((m) => m.type === 'message').length - pageBefore;
s.bufferedAmount -= 50000;
advance(40);
const drained = lastReport();
const timersLeft = timers.size;
// Later pongs take more off what stays counted: never a negative report.
s.bufferedAmount -= 16;
post(scope, { type: 'send', data: new ArrayBuffer(10) });
const afterPongs = lastReport();

// An honest count: a slow message really is in flight while the probe goes
// out, and is gone (with the probe) by the echo.
post(scope, { type: 'open', url: 'ws://stub', primary: true });
s = opened();
post(scope, { type: 'send', data: new ArrayBuffer(200000) });
advance(400);
const honestProbes = probes(s);
const honestStalled = lastReport();
s.bufferedAmount = 0;
receive(s, honestProbes[0] ?? '');
const honestAfterEcho = lastReport();
post(scope, { type: 'send', data: new ArrayBuffer(30000) });
const honestNext = lastReport();
advance(3000);
const honestReprobes = probes(s).length;

check('a count staying up is not probed before 250 ms', noProbeEarly === 0, noProbeEarly);
check('then one probe goes out', JSON.stringify(firstProbes) === JSON.stringify(['_sent,1']), firstProbes);
check('an echo of another probe changes nothing', staleEcho === 199992 + 50000, staleEcho);
check('the echo discounts what stayed counted, not what was sent after the probe', afterEcho === 50000, afterEcho);
check('the echo is not handed to the page', echoForwarded === 0, echoForwarded);
check('the socket reads empty once the later message goes', drained === 0, drained);
check('and the drain timer stops', timersLeft === 0, timersLeft);
check('pongs taking more off never turn the report negative', afterPongs === 10, afterPongs);
check('an honest count in a stall is probed too', JSON.stringify(honestProbes) === JSON.stringify(['_sent,2']), honestProbes);
check('and reported in full meanwhile', honestStalled === 200000 + '_sent,2'.length, honestStalled);
check('its echo discounts nothing', honestAfterEcho === 0, honestAfterEcho);
check('the next message is counted in full', honestNext === 30000, honestNext);
check('an unanswered probe is repeated no sooner than every 2 s', honestReprobes <= 3, honestReprobes);
console.log(`[socket-worker-sent-probe] ${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
