/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The relays' links carry what they are set to (`tests/lib/udprelay.ts`).
 *
 * The WebRTC suites' constrained paths are a `Shaper` of a fixed rate or a
 * `SteppedShaper` whose rate a run changes, and a suite's verdict on the
 * congestion controller is only as good as the link it measures against.
 * What is held here, for both, with datagrams of a media packet's size and
 * of the largest a path carries, at rates from a narrowed link's to a fast
 * one's: traffic under the rate arrives whole and on time, an overload is
 * held to the rate with the rest dropped at the cap, and, for the stepped
 * link, a rate set mid-run is the one it then carries. A datagram whose time
 * on the wire is longer than the timers' grain has to leave all the same.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */
import { Shaper, SteppedShaper, type Leg } from '../lib/udprelay.ts';

let passed = 0;
let failed = 0;

/** Prints one check's line and counts it. */
function check(label: string, ok: boolean, detail = ''): void {
  if (ok) passed++;
  else failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [udprelay] ${label}  ${detail}`);
}

interface Carried {
  /** Bits per second delivered by the run's end. */
  bps: number;
  /** Bytes delivered once what was due has left. */
  total: number;
  /** Datagrams dropped. */
  dropped: number;
  sent: number;
}

/**
 * Offer `size`-byte datagrams at `offered` bits per second into `leg` for
 * `seconds`, `step` changing the leg halfway: what came out by the end, and
 * in all once what was queued has had a moment more to leave.
 */
async function carry(leg: Leg, size: number, offered: number, seconds: number, step?: () => void): Promise<Carried> {
  let out = 0;
  let sent = 0;
  let dropped = 0;
  const interval = (size * 8) / offered;
  const start = performance.now() / 1000;
  let next = start;
  let stepped = false;
  for (;;) {
    const now = performance.now() / 1000;
    if (now - start >= seconds) break;
    if (step && !stepped && now - start >= seconds / 2) {
      step();
      stepped = true;
    }
    while (next <= now) {
      sent++;
      leg.push(size, () => { out += size; });
      next += interval;
    }
    await new Promise((r) => setTimeout(r, 1));
  }
  const byEnd = out;
  await new Promise((r) => setTimeout(r, 250));
  if (leg instanceof SteppedShaper || leg instanceof Shaper) dropped = leg.dropped;
  return { bps: (byEnd * 8) / seconds, total: out, dropped, sent };
}

const mbit = (bps: number): string => `${(bps / 1e6).toFixed(2)} Mbit/s`;

for (const [name, make] of [
  ['Shaper', (rate: number): Leg => new Shaper(rate, 0)],
  ['SteppedShaper', (rate: number): Leg => new SteppedShaper(rate, 0)],
] as const) {
  for (const rate of [1.5e6, 4e6, 10e6, 20e6]) {
    for (const size of [1200, 1400]) {
      const under = await carry(make(rate), size, 0.8 * rate, 1);
      check(`${name}: ${size}-byte datagrams under ${mbit(rate)} arrive whole`,
        under.dropped === 0 && under.total === under.sent * size && Math.abs(under.bps - 0.8 * rate) <= 0.1 * rate,
        `${mbit(under.bps)} of ${mbit(0.8 * rate)}, ${under.dropped} dropped`);
    }
    // Half the rate again: what the link cannot carry queues, and past the
    // cap it is dropped.
    const over = await carry(make(rate), 1200, 1.5 * rate, 2);
    const spills = (0.5 * rate * 2) / 8 > Shaper.CAP_BYTES * 1.2;
    check(`${name}: an overload of ${mbit(rate)} is held to it`,
      (!spills || over.dropped > 0) && over.bps <= 1.05 * rate && over.bps >= 0.85 * rate,
      `${mbit(over.bps)}, ${over.dropped} of ${over.sent} dropped`);
  }
}

// A step down mid-run: the second half carries the new rate, so the whole run
// carries their mean; a step up likewise.
for (const [from, to] of [[10e6, 1.5e6], [1.5e6, 10e6]]) {
  const leg = new SteppedShaper(from, 0);
  const got = await carry(leg, 1200, 1.2 * Math.max(from, to), 2, () => { leg.rate = to; });
  const mean = (from + to) / 2;
  check(`SteppedShaper: a step from ${mbit(from)} to ${mbit(to)} carries each in turn`,
    Math.abs(got.bps - mean) <= 0.15 * mean, `${mbit(got.bps)}, expected about ${mbit(mean)}`);
}

console.log(`[udprelay] ${passed}/${passed + failed} passed`);
process.exit(failed ? 1 : 0);
