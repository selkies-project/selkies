/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The client page as the browser suites open it: instrumented to count the
 * binary frames it receives (headless rAF throttling zeroes the client's own
 * fps counter while the stream flows) and to record the clipboard, display
 * and role messages it is posted, its console errors and 404s kept.
 * @module
 */

import type { Browser, BrowserContext, Page } from '@playwright/test';

import { sleep } from './env.ts';
import { launchBrowser, type Engine } from './browsers.ts';
import { within } from './stream.ts';

/** What the page's taps record, read back in `page.evaluate`. */
declare global {
    interface Window {
        __wsFrames?: number;
        __wsTexts?: string[];
        __wsStates?: string[];
        __clipMsgs?: { text?: string }[];
        __wsTypes?: Record<string, number>;
        encoder?: string;
        videoChunksReceived?: number;
        currentAudioBufferSize?: number;
    }
}

/**
 * The receive taps, a classic script given every page before its own: binary
 * frames counted by type, display-config and stream-state texts kept, and the
 * page's own posted messages recorded. The websockets transport runs its
 * socket in a worker, so its receive side is observed through the transport
 * handle the core publishes as `window.selkiesTransport`.
 */
export const PAGE_TAP_JS = `
  window.__wsFrames = 0;
  window.__wsTypes = {};
  window.__wsStates = [];
  (() => {
    const tap = (e) => {
      if (e.data instanceof ArrayBuffer) {
        window.__wsFrames++;
        const type = e.data.byteLength ? new Uint8Array(e.data, 0, 1)[0] : -1;
        window.__wsTypes[type] = (window.__wsTypes[type] || 0) + 1;
      } else if (typeof e.data === 'string') {
        window.__wsTexts = window.__wsTexts || [];
        if (e.data.includes('DISPLAY_CONFIG_UPDATE')) window.__wsTexts.push(e.data);
        if (/^(AUDIO_|VIDEO_|PIPELINE_RESETTING)/.test(e.data)) window.__wsStates.push(e.data);
      }
    };
    const WS = window.WebSocket;
    window.WebSocket = function(...a) {
      const s = a.length === 1 ? new WS(a[0]) : new WS(a[0], a[1]);
      s.__rxTapped = true;
      s.addEventListener('message', tap);
      return s;
    };
    window.WebSocket.prototype = WS.prototype;
    Object.setPrototypeOf(window.WebSocket, WS);
    let transport = null;
    Object.defineProperty(window, 'selkiesTransport', {
      configurable: true,
      get: () => transport,
      set: (v) => {
        transport = v;
        if (v && v.addEventListener && !v.__rxTapped) {
          v.__rxTapped = true;
          v.addEventListener('message', tap);
        }
      },
    });
  })();
  window.__clipMsgs = [];
  window.__displayCfg = [];
  window.__roleUpdates = [];
  window.addEventListener('message', (e) => {
    if (!e.data || !e.data.type) return;
    if (e.data.type === 'clipboardContentUpdate') window.__clipMsgs.push(e.data);
    if (e.data.type === 'displayConfigUpdate' || e.data.type === 'DISPLAY_CONFIG_UPDATE') window.__displayCfg.push(e.data);
    if (e.data.type === 'clientRoleUpdate') window.__roleUpdates.push(e.data.role);
  });
`;

/**
 * Every RTCPeerConnection the page opens, in `window.__pcs`, so a stream that
 * never shows a frame can be asked what it received and decoded.
 */
export const PC_TAP_JS = `
(() => {
  const PC = window.RTCPeerConnection;
  if (!PC) return;
  window.__pcs = [];
  const Tapped = function (...args) {
    const pc = new PC(...args);
    window.__pcs.push(pc);
    return pc;
  };
  Tapped.prototype = PC.prototype;
  Object.setPrototypeOf(Tapped, PC);
  window.RTCPeerConnection = Tapped;
})();
`;

/**
 * WebCodecs audio taken from the page and from every script it starts from a
 * blob (its workers), the way an engine without the API presents, so the
 * audio workers fall back on libopus in WASM.
 */
export const NO_WEBCODECS_AUDIO_JS = `
(() => {
  const strip = "for (const n of ['AudioDecoder', 'AudioEncoder', 'AudioData', 'EncodedAudioChunk']) {" +
    " try { Object.defineProperty(globalThis, n, { value: undefined, configurable: true, writable: true }); } catch (e) {} }\\n";
  new Function(strip)();
  const create = URL.createObjectURL;
  URL.createObjectURL = function (obj) {
    if (obj instanceof Blob && /javascript/.test(obj.type)) obj = new Blob([strip, obj], { type: obj.type });
    return create.call(URL, obj);
  };
})();
`;

/**
 * An init script running `body` on every page-side send, with the payload in
 * `data`; mutating an ArrayBuffer in place changes what goes out.
 * @param body JavaScript statements, evaluated with `data` in scope.
 * @returns The script, for `context.addInitScript`.
 */
export function wireHookJs(body: string): string {
    return `
(() => {
  const hook = (data) => { ${body} };
  for (const proto of [window.RTCDataChannel && RTCDataChannel.prototype,
                       window.WebSocket && WebSocket.prototype]) {
    if (!proto || typeof proto.send !== 'function') continue;
    const orig = proto.send;
    proto.send = function (d) { try { hook(d); } catch (e) {} return orig.call(this, d); };
  }
  const wrap = (t) => {
    if (!t || t.__wireHooked || typeof t.send !== 'function') return t;
    t.__wireHooked = true;
    const orig = t.send.bind(t);
    t.send = (d) => { try { hook(d); } catch (e) {} return orig(d); };
    return t;
  };
  const prev = Object.getOwnPropertyDescriptor(window, 'selkiesTransport');
  let held = null;
  Object.defineProperty(window, 'selkiesTransport', {
    configurable: true,
    get: () => (prev && prev.get ? prev.get() : held),
    set: (v) => { const w = wrap(v); held = w; if (prev && prev.set) prev.set(w); },
  });
})();
`;
}

/** Every text the page sends, in `window.__wireSent`, and its peer connections in `window.__pcs`. */
export const WIRE_TAP_JS: string = 'window.__wireSent = [];\n'
    + wireHookJs("if (typeof data === 'string') window.__wireSent.push(data);") + PC_TAP_JS;

/** An opened client: its browser and page, and the errors and 404s it keeps collecting. */
export interface Client {
    browser: Browser;
    context: BrowserContext;
    page: Page;
    consoleErrors: string[];
    notFound: string[];
}

/** The clipboard permissions each engine is granted. */
const CLIPBOARD_PERMISSIONS: Readonly<Record<Engine, string[]>> = {
    chromium: ['clipboard-read', 'clipboard-write'],
    firefox: ['clipboard-read'],
    webkit: [],
};

/**
 * Open the client of the server at `url` in a new browser.
 * @param url The server.
 * @param options `hash` appended to the URL (`#display2`), `mode` the
 *   transport the page takes at load (skipping the client's mode probe), and
 *   the `engine`.
 * @returns The client.
 */
export async function launchClient(url: string, options: { hash?: string; mode?: string; engine?: Engine } = {}): Promise<Client> {
    const engine = options.engine ?? 'chromium';
    const browser = await launchBrowser(engine);
    const context = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1 });
    const permissions = CLIPBOARD_PERMISSIONS[engine];
    if (permissions.length) await context.grantPermissions(permissions, { origin: url }).catch(() => undefined);
    if (options.mode) await context.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${options.mode}';` });
    const page = await context.newPage();
    const consoleErrors: string[] = [];
    const notFound: string[] = [];
    page.on('console', (m) => { if (m.type() === 'error') consoleErrors.push(m.text()); });
    page.on('pageerror', (e) => { consoleErrors.push(String(e)); });
    page.on('response', (r) => { if (r.status() === 404) notFound.push(r.url()); });
    await page.addInitScript({ content: PAGE_TAP_JS });
    await page.goto(`${url}/${options.hash ?? ''}`, { waitUntil: 'load' });
    return { browser, context, page, consoleErrors, notFound };
}

/**
 * Another window of `context` on the client (a second display, a shared viewer).
 * @param context The browser context.
 * @param url The server.
 * @param options The transport the page takes at load, and the URL's hash.
 * @returns The page.
 */
export async function newPage(context: BrowserContext, url: string, options: { mode?: string; hash?: string } = {}): Promise<Page> {
    if (options.mode) await context.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${options.mode}';` });
    const page = await context.newPage();
    await page.goto(`${url}/${options.hash ?? ''}`, { waitUntil: 'load' });
    return page;
}

/**
 * Post a settings update into the page, as the dashboards do.
 * @param page The client.
 * @param settings The settings.
 */
export async function settingsChange(page: Page, settings: Record<string, unknown>): Promise<void> {
    await within(page.evaluate((s) => { window.postMessage({ type: 'settings', settings: s }, window.location.origin); }, settings));
}

/**
 * Whether the page was pushed `text` as the server's clipboard.
 * @param page The client.
 * @param text The text.
 * @returns True once it was.
 */
export async function serverClipboardPushed(page: Page, text: string): Promise<boolean> {
    const got = await within(page.evaluate(() => (window.__clipMsgs ?? []).map((m) => m.text)));
    return got.includes(text);
}

/**
 * Send `text` from the client's clipboard as each engine truly does.
 * Chromium reads the clipboard on window focus, which needs the document
 * focused. WebKit forwards through the paste event. Firefox strips
 * clipboardData from a synthetic paste, so the clipboard is written for real
 * and Ctrl+V chorded (Playwright's key events are trusted); the paste-ordering
 * hold may take the first chord, so a second follows.
 * @param page The client.
 * @param text The text.
 * @param engine The page's engine.
 */
export async function sendClipboardFromClient(page: Page, text: string, engine: Engine = 'chromium'): Promise<void> {
    const write = (): Promise<void> => within(page.evaluate((t) => { navigator.clipboard.writeText(t).catch(() => undefined); }, text));
    await write();
    await sleep(400);
    if (engine === 'chromium') {
        await page.bringToFront();
        await within(page.evaluate(() => { window.dispatchEvent(new Event('focus')); }));
    } else {
        await within(page.evaluate((t) => {
            const dt = new DataTransfer();
            dt.setData('text/plain', t);
            window.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
        }, text));
        if (engine === 'webkit') return;
        for (let i = 0; i < 2; i++) {
            await page.keyboard.down('Control');
            await page.keyboard.press('v');
            await page.keyboard.up('Control');
            await sleep(700);
            await write();
            await sleep(300);
        }
    }
    await sleep(300);
}
