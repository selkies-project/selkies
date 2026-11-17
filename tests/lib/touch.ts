/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Touches on a page and the input they put into the session: the touch
// suites' fingers, and a window covering the X display that reads back what
// the server injected.
import type { BrowserContext, CDPSession, Page } from '@playwright/test';

import type { Engine } from './browsers.ts';
import type { Host, InputEvent, InputWatch } from './host.ts';
import { within } from './stream.ts';

// Synthetic touches, for the engines without a CDP touch channel. Each call
// dispatches one TouchEvent on the stream overlay. WebKit refuses to construct
// a Touch and makes one through its legacy document.createTouch instead, and
// takes its TouchEvent's lists as TouchLists rather than arrays.
const SYNTHETIC_TOUCH_JS = `((a) => {
  const el = document.getElementById('overlayInput');
  const legacy = (() => { try { new Touch({identifier: 0, target: el}); return false; } catch (e) { return true; } })();
  const mk = (p) => legacy
    ? document.createTouch(window, el, p[0], p[1], p[2], p[1], p[2])
    : new Touch({identifier: p[0], target: el, clientX: p[1], clientY: p[2], pageX: p[1], pageY: p[2],
                 screenX: p[1], screenY: p[2], radiusX: 1, radiusY: 1, force: 1});
  const list = (ps) => legacy ? document.createTouchList(...ps.map(mk)) : ps.map(mk);
  el.dispatchEvent(new TouchEvent(a.type, {touches: list(a.touches), targetTouches: list(a.touches),
      changedTouches: list(a.changed), bubbles: true, cancelable: true}));
})`;

// Firefox exposes touch events on a desktop without a touch screen only when
// asked, `ontouchstart` included, which the client looks for.
export const FIREFOX_TOUCH_PREFS = { 'dom.w3c_touch_events.enabled': 1, 'dom.w3c_touch_events.legacy_apis.enabled': true };

export type Point = readonly [number, number, number];
/**
 * Fingers on the page: CDP multi-touch on Chromium, synthetic TouchEvents
 * elsewhere. `down` and `up` land or lift fingers, `move` moves them; each
 * takes (id, x, y) points in viewport coordinates. CDP presses and moves the
 * points a touchStart or touchMove lists, and releases the ones a touchEnd
 * lists: a touchMove that leaves a point out does not lift it.
 */
export class Fingers {
    private readonly live = new Map<number, [number, number]>();
    private readonly page: Page;
    private readonly cdp: CDPSession | null;

    private constructor(page: Page, cdp: CDPSession | null) {
        this.page = page;
        this.cdp = cdp;
    }

    static async on(page: Page, ctx: BrowserContext, engine: Engine): Promise<Fingers> {
        return new Fingers(page, engine === 'chromium' ? await ctx.newCDPSession(page) : null);
    }

    private async sendCdp(cdp: CDPSession, type: 'touchStart' | 'touchMove' | 'touchEnd', points?: readonly Point[]): Promise<void> {
        const list = points ?? [...this.live].map(([i, [x, y]]): Point => [i, x, y]);
        await cdp.send('Input.dispatchTouchEvent', { type, touchPoints: list.map(([id, x, y]) => ({ x, y, id })) });
    }

    private async synthetic(type: string, changed: readonly Point[]): Promise<void> {
        const touches = [...this.live].map(([i, [x, y]]) => [i, x, y]);
        await within(this.page.evaluate(`${SYNTHETIC_TOUCH_JS}(${JSON.stringify({ type, touches, changed })})`));
    }

    async down(...points: Point[]): Promise<void> {
        for (const [i, x, y] of points) {
            this.live.set(i, [x, y]);
            if (this.cdp) await this.sendCdp(this.cdp, 'touchStart');
        }
        if (!this.cdp) await this.synthetic('touchstart', points);
    }

    async move(...points: Point[]): Promise<void> {
        for (const [i, x, y] of points) this.live.set(i, [x, y]);
        if (this.cdp) await this.sendCdp(this.cdp, 'touchMove');
        else await this.synthetic('touchmove', points);
    }

    async up(...ids: number[]): Promise<void> {
        const changed = ids.map((i): Point => {
            const [x, y] = this.live.get(i) ?? [0, 0];
            return [i, x, y];
        });
        for (const i of ids) this.live.delete(i);
        if (this.cdp) await this.sendCdp(this.cdp, 'touchEnd', changed);
        else await this.synthetic('touchend', changed);
    }
}

/** Every pointer and key event the session gets, stamped as it arrives, from a window covering the display. */
export class XWatcher {
    private readonly watch: InputWatch;
    readonly control: number;

    private constructor(watch: InputWatch, control: number) {
        this.watch = watch;
        this.control = control;
    }

    static async start(host: Host): Promise<XWatcher> {
        return new XWatcher(await host.watchInput(true), (await host.keycode(0xFFE3)) ?? -1);
    }

    async since(t: number, kind?: string, detail?: number): Promise<InputEvent[]> {
        return (await this.watch.events()).filter((e) => e.at >= t && (kind === undefined || e.kind === kind) && (detail === undefined || e.detail === detail));
    }

    async close(): Promise<void> {
        await this.watch.release();
    }
}
