/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What a Chromium Ctrl+V sends ahead of its V. The focus read covers a copy
 * made in another window, but the clipboard also changes while the window
 * keeps focus (a copy in another tab, a clipboard tool, a desktop shortcut
 * that does not move focus), and the chord would then paste the value the
 * last focus read sent. So the chord's keydown starts a read of its own and
 * its V waits for it.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */

import type { ClipboardSync } from '../../web/selkies-web-core/lib/clipboard-sync.ts';
import type { StandIn } from './stand_in.ts';

/** A key listener as the stand-in window calls it. */
type KeyListener = (event: StandIn<KeyboardEvent>) => void;

/** A key event as the gestures read one, with what they did to it. */
interface FakeKey {
    type: string;
    code: string;
    key: string;
    ctrlKey: boolean;
    metaKey: boolean;
    altKey: boolean;
    repeat: boolean;
    isComposing: boolean;
    prevented: boolean;
    stopped: boolean;
    preventDefault(): void;
    stopImmediatePropagation(): void;
}

const listeners: Array<{ type: string; fn: KeyListener }> = [];
const replayed: KeyboardEvent[] = [];
const page: { activeElement: Pick<Element, 'id' | 'tagName'> | null } = { activeElement: null };
Object.assign(globalThis, {
    window: {
        addEventListener: (type: string, fn: KeyListener) => listeners.push({ type, fn }),
        removeEventListener: (type: string, fn: KeyListener) => {
            const i = listeners.findIndex((l) => l.type === type && l.fn === fn);
            if (i >= 0) listeners.splice(i, 1);
        },
        dispatchEvent: (ev: Event) => { replayed.push(ev as KeyboardEvent); },
    } satisfies StandIn<Window>,
    document: page satisfies StandIn<Document>,
    KeyboardEvent: class {
        type: string;
        constructor(type: string, init: KeyboardEventInit) { Object.assign(this, init); this.type = type; }
    },
});

const { createClipboardGestures } = await import(
    '../../web/selkies-web-core/lib/clipboard-sync.ts');

let failed = 0;

/** Prints one check's line and counts a failure. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [clip-paste-chord] ${label}  ${String(detail)}`);
}

/** A promise and the call that settles it. */
function gate(): { promise: Promise<void>; release: () => void } {
    let release!: () => void;
    const promise = new Promise<void>((r) => { release = r; });
    return { promise, release };
}

/** A Ctrl-held key event. */
function keyEvent(type: string, code: string, init: Partial<FakeKey> = {}): FakeKey {
    return {
        type, code, key: code === 'KeyV' ? 'v' : '', ctrlKey: true, metaKey: false, altKey: false,
        repeat: false, isComposing: false, prevented: false, stopped: false,
        preventDefault() { this.prevented = true; },
        stopImmediatePropagation() { this.stopped = true; },
        ...init,
    };
}

/** Hands a key event to the listeners of its type. */
function press(ev: FakeKey | KeyboardEvent): void {
    for (const l of listeners.filter((x) => x.type === ev.type)) l.fn(ev as StandIn<KeyboardEvent>);
}

/** Gestures wired on a fresh window, with a read that waits on `hold`. */
function wire({ isChromium = true, hold = null }: { isChromium?: boolean; hold?: ReturnType<typeof gate> | null } = {}):
    { reads: number[]; unwire: () => void } {
    listeners.length = 0;
    replayed.length = 0;
    const reads: number[] = [];
    const g = createClipboardGestures({
        isChromium,
        clipboardSync: {} as ClipboardSync,
        sendClipboardData: () => Promise.resolve(),
        canSync: () => true,
        canRead: () => true,
        canWrite: () => true,
        binaryEnabled: () => false,
        getSendInFlight: () => null,
        readLocalClipboard: () => {
            reads.push(Date.now());
            return hold ? hold.promise : Promise.resolve();
        },
    });
    g.wire();
    return { reads, unwire: () => g.unwire() };
}

const settle = (): Promise<void> => new Promise((r) => setTimeout(r, 20));

// The keydown reads and holds its V until the read settles, then replays it.
{
    const hold = gate();
    const { reads, unwire } = wire({ hold });
    const down = keyEvent('keydown', 'KeyV');
    press(down);
    check('a Chromium Ctrl+V keydown reads the local clipboard', reads.length === 1, `reads=${reads.length}`);
    check('its V is held while that read is in flight', down.prevented && down.stopped,
          `prevented=${down.prevented} stopped=${down.stopped}`);
    await settle();
    check('nothing is replayed before the read settles', replayed.length === 0, `replayed=${replayed.length}`);
    hold.release();
    await settle();
    check('the V is replayed once the read settles',
          replayed.length === 1 && replayed[0].code === 'KeyV' && replayed[0].__selkiesClipReplay === true,
          `replayed=${replayed.length}`);
    unwire();
}

// The replayed V and autorepeat do not start another read.
{
    const { reads, unwire } = wire();
    press(keyEvent('keydown', 'KeyV'));
    await settle();
    for (const ev of replayed.splice(0)) press(ev);
    press(keyEvent('keydown', 'KeyV', { repeat: true }));
    await settle();
    check('a replayed or autorepeated V reads nothing more', reads.length === 1, `reads=${reads.length}`);
    unwire();
}

// A paste into the page's own form fields stays the page's.
{
    const { reads, unwire } = wire();
    page.activeElement = { id: 'settingsField', tagName: 'INPUT' };
    const down = keyEvent('keydown', 'KeyV');
    press(down);
    page.activeElement = null;
    check('a Ctrl+V in a page form field reads nothing and is not held',
          reads.length === 0 && !down.stopped, `reads=${reads.length} stopped=${down.stopped}`);
    unwire();
}

// Outside Chromium the paste event carries the clipboard; the hook stays unused.
{
    const { reads, unwire } = wire({ isChromium: false });
    press(keyEvent('keydown', 'KeyV'));
    await settle();
    check('outside Chromium the chord does not call the read', reads.length === 0, `reads=${reads.length}`);
    unwire();
}

process.exit(failed ? 1 : 0);
