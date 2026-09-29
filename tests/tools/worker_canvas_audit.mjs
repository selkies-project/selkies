/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Which engines the video worker gives an unaccelerated canvas. WebKit's Linux
// ports draw a worker canvas through Skia's GPU context, which races the page's
// paint and crashes the web process, so there alone the worker asks for a
// canvas the CPU draws. Safari draws through CoreGraphics and keeps its
// accelerated canvas, as do Chromium and Firefox. The user agents and platforms
// are the ones each engine reports inside a worker.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import { isSkiaWebKit } from '../../addons/selkies-web-core/lib/util.js';

let failed = 0;

function check(label, ok, detail = '') {
  if (!ok) failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [worker-canvas] ${label}  ${detail}`);
}

const SAFARI_UA = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.6 Safari/605.1.15';
const CASES = [
  ['WPE WebKit, which carries Safari\'s macOS user agent', SAFARI_UA, 'Linux x86_64', true],
  ['WebKitGTK', 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15 Epiphany/605.1.15', 'Linux aarch64', true],
  ['Safari on macOS', SAFARI_UA, 'MacIntel', false],
  ['Safari on an iPad in its desktop-class default', SAFARI_UA, 'MacIntel', false],
  ['Safari on an iPhone', 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1', 'iPhone', false],
  ['headless Chrome on Linux', 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/154.0.0.0 Safari/537.36', 'Linux x86_64', false],
  ['Chrome on Android', 'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Mobile Safari/537.36', 'Linux armv8l', false],
  ['Edge on Linux', 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36 Edg/154.0.0.0', 'Linux x86_64', false],
  ['Firefox on Linux', 'Mozilla/5.0 (X11; Linux x86_64; rv:156.0) Gecko/20100101 Firefox/156.0', 'Linux x86_64', false],
];

for (const [label, userAgent, platform, unaccelerated] of CASES) {
  check(`${label}: ${unaccelerated ? 'an unaccelerated' : 'an accelerated'} worker canvas`,
        isSkiaWebKit(userAgent, platform) === unaccelerated, platform);
}

console.log(`[worker-canvas] ${failed ? 'FAILED' : 'OK'}`);
process.exit(failed ? 1 : 0);
