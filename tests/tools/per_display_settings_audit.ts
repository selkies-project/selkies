/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The per-display setting lists stay in their intended relationship.
 *
 * Each core keeps the settings a secondary display holds for itself, and the
 * dashboard the union of both. The JPEG controls ride the WebSockets framing
 * JPEG takes, so only that core lists them; every other knob drives both
 * transports, so neither core may drop one.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const WEB = join(import.meta.dirname, '..', '..', 'web');
const SOURCES = {
  'ws-core': 'selkies-web-core/selkies-ws-core.ts',
  'wr-core': 'selkies-web-core/selkies-wr-core.ts',
  dashboard: 'selkies-dashboard/src/utils.ts',
} as const;
const WS_ONLY = new Set(['jpeg_quality', 'paint_over_jpeg_quality']);
const WR_ONLY = new Set<string>();

let failed = 0;

/** Prints one check's line and counts a failure. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
  if (!ok) failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [per-display-settings] ${label}  ${typeof detail === 'string' ? detail : JSON.stringify(detail)}`);
}

/** The PER_DISPLAY_SETTINGS keys a source declares, or null where it declares none. */
function readList(rel: string): Set<string> | null {
  const match = /PER_DISPLAY_SETTINGS\s*(?::[^=]*)?=\s*\[([\s\S]*?)\]/.exec(readFileSync(join(WEB, rel), 'utf8'));
  return match ? new Set([...match[1].matchAll(/'([^']+)'/g)].map((m) => m[1])) : null;
}

const sorted = (set: Iterable<string>): string[] => [...set].sort();
const minus = (a: Set<string>, b: Set<string>): Set<string> => new Set([...a].filter((k) => !b.has(k)));
const equal = (a: Set<string>, b: Set<string>): boolean => a.size === b.size && [...a].every((k) => b.has(k));

const lists: Partial<Record<keyof typeof SOURCES, Set<string>>> = {};
for (const name of ['ws-core', 'wr-core', 'dashboard'] as const) {
  const rel = SOURCES[name];
  const keys = readList(rel);
  check(`${name} declares the list`, keys !== null, rel);
  if (keys === null) process.exit(1);
  lists[name] = keys;
}
const ws = lists['ws-core'] ?? new Set<string>(), wr = lists['wr-core'] ?? new Set<string>(), dash = lists.dashboard ?? new Set<string>();
const union = new Set([...ws, ...wr]);
check('the dashboard holds the union of both cores', equal(dash, union),
      `missing=${JSON.stringify(sorted(minus(union, dash)))} extra=${JSON.stringify(sorted(minus(dash, union)))}`);
check('WebSocket-only keys are absent from the WebRTC core', ![...WS_ONLY].some((k) => wr.has(k)), sorted([...WS_ONLY].filter((k) => wr.has(k))));
check('WebRTC-only keys are absent from the WebSocket core', ![...WR_ONLY].some((k) => ws.has(k)), sorted([...WR_ONLY].filter((k) => ws.has(k))));
const wsRest = minus(ws, WS_ONLY), wrRest = minus(wr, WR_ONLY);
check('the cores agree on everything else', equal(wsRest, wrRest),
      `ws_extra=${JSON.stringify(sorted(minus(wsRest, wrRest)))} wr_extra=${JSON.stringify(sorted(minus(wrRest, wsRest)))}`);
console.log(`[per-display-settings] ${failed} check(s) failed`);
process.exit(failed ? 1 : 0);
