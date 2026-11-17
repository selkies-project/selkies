/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What the core-parity and start-policy suites read a page's input and
// settings through: a tap on what the page sends, two synthetic gamepads, and
// the cores' storage prefix.
import type { Page } from '@playwright/test';

import { within } from './stream.ts';

/**
 * Records the resolutions, gamepad reports and clipboard writes the page
 * sends (`__resSent`, `__padSent`, `__clipSent`) and its clipboard reads
 * (`__clipReads`), on either transport.
 */
export const PARITY_TAP_JS = `
(() => {
  window.__resSent = [];
  window.__padSent = [];
  window.__clipSent = 0;
  window.__clipReads = 0;
  const tap = (d) => {
    if (typeof d !== 'string') return;
    if (d.startsWith('r,')) window.__resSent.push(d.split(',')[1]);
    else if (d.startsWith('js,')) window.__padSent.push(d.split(',')[1]);
    else if (d.startsWith('cw') || d.startsWith('cb')) window.__clipSent++;
  };
  // The websockets transport runs its socket in a worker, so its sends are
  // observed through the page-side handle rather than WebSocket.prototype.
  let transport = null;
  Object.defineProperty(window, 'selkiesTransport', {
    configurable: true,
    get: () => transport,
    set: (v) => {
      transport = v;
      if (v && typeof v.send === 'function' && !v.__tapped) {
        const orig = v.send.bind(v);
        v.send = (d) => { tap(d); return orig(d); };
        v.__tapped = true;
      }
    },
  });
  const protos = [window.RTCDataChannel && RTCDataChannel.prototype,
                  window.WebSocket && WebSocket.prototype];
  for (const proto of protos) {
    if (!proto || typeof proto.send !== 'function') continue;
    const orig = proto.send;
    proto.send = function(d) { tap(d); return orig.call(this, d); };
  }
  const clip = window.Clipboard && Clipboard.prototype;
  if (clip) {
    for (const m of ['read', 'readText']) {
      const orig = clip[m];
      if (typeof orig !== 'function') continue;
      clip[m] = function(...a) { window.__clipReads++; return orig.apply(this, a); };
    }
  }
})();
`;

/** Two synthetic W3C pads, so a disconnect can take one away while the other stays pressable. */
export const PADS_INIT = `
window.__pads = [0, 1].map((i) => ({
  index: i, id: "Selkies Test Pad (STANDARD GAMEPAD Vendor: 045e Product: 028e)",
  mapping: "standard", connected: true, timestamp: 1,
  buttons: Array.from({length: 17}, () => ({pressed: false, touched: false, value: 0})),
  axes: [0, 0, 0, 0],
}));
navigator.getGamepads = () => [window.__pads[0], window.__pads[1], null, null];
window.__padPress = (p, i, v) => {
  const pad = window.__pads[p];
  pad.buttons[i] = {pressed: v > 0, touched: v > 0, value: v};
  pad.timestamp = performance.now();
};
`;

/** The cores' storage prefix (lib/util.ts getStorageAppName): origin + path with everything but [A-Za-z0-9._-] replaced by '_'. */
export const STORAGE_APP = "(location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_')";

declare global {
    interface Window {
        __padSent?: string[];
    }
}

/**
 * Gamepad button reports the page has put on the wire so far.
 * @param page The page, with the parity tap installed.
 * @returns How many.
 */
export function buttonReports(page: Page): Promise<number> {
    return within(page.evaluate(() => (window.__padSent ?? []).filter((t) => t === 'b').length));
}
