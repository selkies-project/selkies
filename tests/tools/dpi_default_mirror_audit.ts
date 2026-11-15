/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The derived `scaling_dpi` default is the same number on both sides.
 *
 * The web core's stream density and the dashboard's DPI picker each derive a
 * default from a manual resolution or the display's pixel ratio. They are
 * written the same way in both, so a change to either rule has to be made in
 * both: the same stops, the same unity rows, the same expressions.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const WEB = join(import.meta.dirname, '..', '..', 'web');
const SOURCES = {
  lib: 'selkies-web-core/lib/stream-density.ts',
  dashboard: 'selkies-dashboard/src/state/settings.ts',
};
/** The density each side reads off a manual resolution, and off the display when there is none. */
const EXPRESSIONS = ['96 * rows / DPI_UNITY_ROWS', 'Math.round(dpr * 4) * 24'];

let passed = 0;
let failed = 0;

/** Prints one check's line and counts it. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
  if (ok) passed++;
  else failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [dpi-default-mirror] ${label}  ${typeof detail === 'string' ? detail : JSON.stringify(detail)}`);
}

/** The DPI stops a source offers, from either shape they are written in. */
function stops(text: string): number[] {
  const listed = /DPI_STOPS\s*(?::[^=]*)?=\s*\[([^\]]*)\]/.exec(text);
  if (listed) return [...listed[1].matchAll(/\d+/g)].map((m) => Number(m[0]));
  const options = /dpiScalingOptions\s*(?::[^=]*)?=\s*\[([\s\S]*?)\n\]/.exec(text);
  return options ? [...options[1].matchAll(/value:\s*(\d+)/g)].map((m) => Number(m[1])) : [];
}

const texts = { lib: readFileSync(join(WEB, SOURCES.lib), 'utf8'), dashboard: readFileSync(join(WEB, SOURCES.dashboard), 'utf8') };
const reference = stops(texts.lib);
check('the lib names the stops', reference.length > 1, reference);
const got = stops(texts.dashboard);
check('the dashboard offers the same stops', JSON.stringify(got) === JSON.stringify(reference),
      `${JSON.stringify(got)} vs ${JSON.stringify(reference)}`);
const rows = Object.fromEntries(Object.entries(texts).map(([name, text]) =>
  [name, [...text.matchAll(/DPI_UNITY_ROWS\s*(?::\s*number)?\s*=\s*(\d+)/g)].map((m) => m[1])]));
check('every side reads a resolution against the same rows',
      new Set(Object.values(rows).map((v) => JSON.stringify(v))).size === 1 && rows.lib.length > 0, rows);
for (const expression of EXPRESSIONS) {
  const missing = Object.entries(texts).filter(([, text]) => !text.includes(expression)).map(([name]) => name);
  check(`every side derives from \`${expression}\``, !missing.length, `missing in ${JSON.stringify(missing)}`);
}
console.log(`[dpi-default-mirror] ${passed}/${passed + failed} passed`);
process.exit(failed ? 1 : 0);
