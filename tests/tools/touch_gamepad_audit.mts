/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What the on-screen touch gamepad reports for the touches it is given.
 *
 * The module under test is the one the dashboard bundles, loaded as it is
 * there: for its side effects, over a page. The page here is a stand-in with
 * the elements, events, storage, and clock the module touches, so every
 * check drives it the way a page does (`postMessage`, touch events on the
 * elements it rendered) and reads it the way a page does
 * (`navigator.getGamepads()`, the events on `window`). The core's own
 * `GamepadManager` reads the pad beside the checks, since the two meet only
 * through that API.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */
import { GamepadManager } from '../../web/selkies-web-core/lib/gamepad.ts';

let failed = 0;

/**
 * Prints one check's line and counts a failure.
 * @param label What is held.
 * @param ok Whether it held.
 * @param detail What was seen instead.
 */
function check(label: string, ok: boolean, detail: unknown = ''): void {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [touch-gamepad] ${label}  ${typeof detail === 'string' ? detail : JSON.stringify(detail)}`);
}

/** A listener as the stand-in page calls it. */
type Listener = (event: unknown) => void;

/** One finger of a touch event. */
interface Finger {
    identifier: number;
    clientX: number;
    clientY: number;
}

/** A box in client coordinates, as `getBoundingClientRect` returns it. */
interface Box {
    left: number;
    top: number;
    right: number;
    bottom: number;
    width: number;
    height: number;
}

/**
 * A box from its corner and size.
 * @param left
 * @param top
 * @param width
 * @param height
 * @returns The box.
 */
const box = (left: number, top: number, width: number, height: number): Box =>
    ({ left, top, right: left + width, bottom: top + height, width, height });

/** An element of the stand-in page: a tree node with the members the module uses. */
class PageElement {
    tag: string;
    id = '';
    className = '';
    textContent = '';
    markup = '';
    style: Record<string, string> = {};
    dataset: Record<string, string> = {};
    children: PageElement[] = [];
    parentElement: PageElement | null = null;
    listeners: Record<string, Listener[]> = {};
    rect: Box = box(0, 0, 0, 0);

    constructor(tag: string) {
        this.tag = tag;
    }

    get classList() {
        const names = () => this.className.split(/\s+/).filter(Boolean);
        return {
            add: (name: string) => { if (!names().includes(name)) this.className = [...names(), name].join(' '); },
            remove: (name: string) => { this.className = names().filter((n) => n !== name).join(' '); },
            contains: (name: string) => names().includes(name),
        };
    }

    get innerHTML(): string {
        return this.markup;
    }

    set innerHTML(markup: string) {
        for (const child of this.children) child.parentElement = null;
        this.children = [];
        this.markup = markup;
    }

    appendChild(child: PageElement): PageElement {
        child.remove();
        child.parentElement = this;
        this.children.push(child);
        return child;
    }

    remove(): void {
        const parent = this.parentElement;
        if (parent) parent.children = parent.children.filter((c) => c !== this);
        this.parentElement = null;
    }

    addEventListener(type: string, listener: Listener): void {
        (this.listeners[type] ??= []).push(listener);
    }

    getBoundingClientRect(): Box {
        return this.rect;
    }

    /**
     * Calls the element's listeners for `type`.
     * @param type
     * @param event
     */
    fire(type: string, event: unknown): void {
        for (const listener of this.listeners[type] ?? []) listener(event);
    }

    /**
     * The elements under this one that carry a class, in document order.
     * @param name
     * @returns The elements.
     */
    withClass(name: string): PageElement[] {
        return this.children.flatMap((child) => [...(child.classList.contains(name) ? [child] : []), ...child.withClass(name)]);
    }
}

// --- a virtual clock --------------------------------------------------------
let now = 1000;
const timers: Array<{ at: number; run: () => void }> = [];
Date.now = () => now;
Object.assign(globalThis, {
    setTimeout: (run: () => void, ms = 0) => { timers.push({ at: now + ms, run }); return timers.length; },
    setInterval: () => 0,
    clearInterval: () => {},
});

/**
 * Runs every timer due within `ms`, in order, and leaves the clock there.
 * @param ms
 */
function advance(ms: number): void {
    const end = now + ms;
    for (;;) {
        timers.sort((a, b) => a.at - b.at);
        const next = timers[0];
        if (!next || next.at > end) break;
        timers.shift();
        now = next.at;
        next.run();
    }
    now = end;
}

// --- the page ----------------------------------------------------------------
const ORIGIN = 'https://selkies.test';
const HOST = 'touch-gamepad-host';
const PROFILE_KEY = 'universalTouchGamepad_currentProfile';
const PAD_ID = 'Universal Touch Gamepad';

const body = new PageElement('body');
const hosts: Record<string, PageElement> = { [HOST]: new PageElement('div') };
const stored: Record<string, string> = {};
const windowListeners: Record<string, Listener[]> = {};
/** Every event the module dispatched on `window`, by type, in order. */
const dispatched: string[] = [];
const errors: string[] = [];
const vibrations: number[] = [];

/** A physical pad in the standard layout, at rest. */
const physicalPad = (index: number) => ({
    index, id: 'Audit Pad (STANDARD GAMEPAD Vendor: 045e Product: 028e)', mapping: 'standard', connected: true,
    axes: [0, 0, 0, 0], buttons: Array.from({ length: 17 }, () => ({ pressed: false, touched: false, value: 0 })),
});
let physical: Array<ReturnType<typeof physicalPad> | null> = [null, null, null, null];

Object.assign(globalThis, {
    window: {
        innerHeight: 800,
        location: { origin: ORIGIN },
        addEventListener: (type: string, listener: Listener) => { (windowListeners[type] ??= []).push(listener); },
        removeEventListener: (type: string, listener: Listener) => {
            windowListeners[type] = (windowListeners[type] ?? []).filter((l) => l !== listener);
        },
        dispatchEvent: (event: Event) => {
            dispatched.push(event.type);
            for (const listener of windowListeners[event.type] ?? []) listener(event);
            return true;
        },
    },
    document: {
        head: new PageElement('head'),
        body,
        documentElement: { style: { setProperty: () => {} } },
        createElement: (tag: string) => new PageElement(tag),
        getElementById: (id: string) => hosts[id] ?? null,
    },
});
Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: {
        getItem: (key: string) => stored[key] ?? null,
        setItem: (key: string, value: string) => { stored[key] = value; },
    },
});
Object.assign(navigator, {
    getGamepads: () => physical,
    vibrate: (ms: number) => { vibrations.push(ms); return true; },
});
const consoleError = console.error;
console.error = (...args: unknown[]) => { errors.push(args.join(' ')); };
console.log = ((log) => (...args: unknown[]) => {
    if (typeof args[0] === 'string' && /^(PASS|FAIL) /.test(args[0])) log(...args);
})(console.log);

await import('../../web/universal-touch-gamepad/universalTouchGamepad.ts');

/**
 * Posts a message to the page as a frame of `origin` would.
 * @param data
 * @param origin
 */
function post(data: unknown, origin = ORIGIN): void {
    for (const listener of windowListeners.message ?? []) listener({ origin, data });
}

/** The virtual pad as the page reads it, or null while it is not connected. */
const touchPad = (): Gamepad | null => navigator.getGamepads().find((pad) => pad?.id === PAD_ID) ?? null;

/** The live overlay, or undefined before the first show. */
const overlay = (): PageElement | undefined => body.children.find((el) => el.id === 'universal-touch-gamepad-controls-overlay');

/**
 * The live overlay's elements of a class.
 * @param name
 * @returns The elements.
 */
const controls = (name: string): PageElement[] => overlay()?.withClass(name) ?? [];

/**
 * The live overlay's button with a label.
 * @param label
 * @returns The button.
 * @throws Error when the layout has no such button.
 */
function button(label: string): PageElement {
    const found = controls('touch-button').find((el) => el.textContent === label);
    if (!found) throw new Error(`no ${label} button in the overlay`);
    return found;
}

/**
 * The live overlay's cluster with an id.
 * @param id
 * @returns The cluster.
 * @throws Error when the layout has no such cluster.
 */
function cluster(id: string): PageElement {
    const found = controls('touch-gamepad-cluster').find((el) => el.id === `cluster-${id}`);
    if (!found) throw new Error(`no ${id} cluster in the overlay`);
    return found;
}

/**
 * A touch event as the module's handlers read one.
 * @param type
 * @param touches Fingers on the surface after the event.
 * @param changed Fingers the event is about.
 * @returns The event.
 */
const touchEvent = (type: string, touches: Finger[], changed: Finger[] = touches) => ({
    type, touches, changedTouches: changed, cancelable: true, preventDefault: () => {}, stopPropagation: () => {},
});

/**
 * A finger at a point.
 * @param identifier
 * @param clientX
 * @param clientY
 * @returns The finger.
 */
const finger = (identifier: number, clientX: number, clientY: number): Finger => ({ identifier, clientX, clientY });

/** Lets the announcement the module queued for a touch event run. */
const settle = (): Promise<void> => Promise.resolve();

/** Opens the picker and returns its previews by profile key. */
function openPicker(): Record<string, PageElement> {
    controls('settings-icon-host')[0].fire('click', { stopPropagation: () => {} });
    const picker = body.children[body.children.length - 1];
    const previews: Record<string, PageElement> = {};
    for (const preview of picker.children[0].children) previews[preview.dataset.profileKey] = preview;
    return previews;
}

/** What the core reported for the pad, in order. */
const reported: Array<[string, number, number]> = [];
const manager = new GamepadManager(null,
    (_index, control, value) => { reported.push(['button', control, value]); },
    (_index, control, value) => { reported.push(['axis', control, value]); });

// --- who may drive it, and when the pad exists -------------------------------
{
    check('the module leaves no pad in the list before it is shown',
          navigator.getGamepads().length === 4 && touchPad() === null, navigator.getGamepads().map((pad) => pad?.id ?? null));

    post({ type: 'TOUCH_GAMEPAD_SETUP', payload: { targetDivId: HOST, visible: true } }, 'https://elsewhere.test');
    check('a frame of another origin does not set it up', overlay() === undefined && touchPad() === null);

    post({ type: 'TOUCH_GAMEPAD_VISIBILITY', payload: { visible: true } });
    check('showing it before a setup names a host is refused',
          overlay() === undefined && errors.some((line) => line.includes('Host anchor not set')), errors);

    errors.length = 0;
    post({ type: 'TOUCH_GAMEPAD_SETUP', payload: { targetDivId: 'absent', visible: true } });
    check('a setup naming an element the page lacks is refused',
          overlay() === undefined && errors.some((line) => line.includes('#absent not found')), errors);

    post({ type: 'TOUCH_GAMEPAD_SETUP', payload: { targetDivId: HOST, visible: false } });
    check('a setup that is not visible draws nothing', overlay() === undefined && touchPad() === null);

    dispatched.length = 0;
    post({ type: 'TOUCH_GAMEPAD_SETUP', payload: { targetDivId: HOST, initialProfileName: 'sixteenBit', visible: true } });
    const pad = touchPad();
    check('a visible setup draws the overlay and connects the pad in the standard layout',
          overlay()?.style.display === 'block' && dispatched.join() === 'gamepadconnected' &&
          pad?.index === 0 && pad.mapping === 'standard' && pad.buttons.length === 18 && pad.axes.length === 4,
          { dispatched, pad: pad && { index: pad.index, mapping: pad.mapping } });
}

// --- profiles ----------------------------------------------------------------
{
    check('the profile a setup names is drawn where none is persisted: 16-bit has twelve buttons and no stick',
          controls('touch-button').length === 12 && controls('touch-joystick-base').length === 0 &&
          controls('touch-gamepad-cluster').length === 5,
          [controls('touch-button').length, controls('touch-joystick-base').length, controls('touch-gamepad-cluster').length]);

    const previews = openPicker();
    check('the picker offers the three layouts', Object.keys(previews).join() === 'eightBit,sixteenBit,modern',
          Object.keys(previews));
    check('and titles each with its name',
          Object.values(previews).map((preview) => preview.children[0].textContent).join() === '8-bit,16-bit,Modern');
    check('a preview is inert: no touch surface is rendered into it',
          Object.values(previews).every((preview) => preview.withClass('touch-gamepad-cluster').length === 0 &&
              preview.withClass('touch-joystick-base').every((stick) => !stick.listeners.touchstart)));

    previews.eightBit.fire('click', {});
    check('a pick persists, closes the picker, and redraws the overlay: 8-bit has eight buttons',
          stored[PROFILE_KEY] === 'eightBit' && body.children.length === 1 && controls('touch-button').length === 8,
          [stored[PROFILE_KEY], body.children.length, controls('touch-button').length]);

    post({ type: 'TOUCH_GAMEPAD_SETUP', payload: { targetDivId: HOST, initialProfileName: 'modern', visible: true } });
    check('the persisted pick wins over the profile a later setup names', controls('touch-button').length === 8,
          controls('touch-button').length);

    openPicker().modern.fire('click', {});
    check('modern has thirteen buttons, two sticks, two triggers, and five clusters',
          controls('touch-button').length === 13 && controls('touch-joystick-base').length === 2 &&
          controls('touch-analog-trigger').length === 2 && controls('touch-gamepad-cluster').length === 5,
          [controls('touch-button').length, controls('touch-joystick-base').length,
           controls('touch-analog-trigger').length, controls('touch-gamepad-cluster').length]);
    check('a control is placed inside the safe area', button('A').style.right === 'calc(125px + 15px)' &&
          button('A').style.bottom === 'calc(100px + 10px)', [button('A').style.right, button('A').style.bottom]);
}

// --- hit testing in a cluster ------------------------------------------------
{
    const face = cluster('faceCluster_mod');
    button('A').rect = box(100, 200, 45, 45);
    button('B').rect = box(160, 150, 45, 45);
    button('X').rect = box(40, 150, 45, 45);
    button('Y').rect = box(100, 100, 45, 45);
    const pressed = () => (touchPad()?.buttons ?? []).flatMap((b, i) => (b.pressed ? [i] : []));

    manager._poll();
    reported.length = 0;
    dispatched.length = 0;
    face.fire('touchstart', touchEvent('touchstart', [finger(1, 120, 220)]));
    check('a touch on a button presses it', pressed().join() === '0' && touchPad()?.buttons[0].value === 1 &&
          button('A').classList.contains('pressed'), pressed());
    check('the change is announced only once the handler is done', dispatched.length === 0, dispatched);
    await settle();
    check('then once, and the core reads the press at that announcement, before any tick',
          dispatched.join() === 'touchgamepadinput' && JSON.stringify(reported) === '[["button",0,1]]',
          { dispatched, reported });

    reported.length = 0;
    face.fire('touchmove', touchEvent('touchmove', [finger(1, 180, 170)]));
    await settle();
    check('a finger sliding onto a neighbor re-targets without lifting',
          pressed().join() === '1' && !button('A').classList.contains('pressed') && button('B').classList.contains('pressed') &&
          JSON.stringify(reported) === '[["button",0,0],["button",1,1]]', { pressed: pressed(), reported });

    face.fire('touchmove', touchEvent('touchmove', [finger(1, 214, 170)]));
    check('a finger just past the edge still holds the button', pressed().join() === '1', pressed());
    face.fire('touchmove', touchEvent('touchmove', [finger(1, 216, 170)]));
    check('and farther out lets it go', pressed().join() === '', pressed());

    face.fire('touchstart', touchEvent('touchstart', [finger(1, 120, 220)]));
    face.fire('touchstart', touchEvent('touchstart', [finger(1, 120, 220), finger(2, 130, 230)], [finger(2, 130, 230)]));
    face.fire('touchend', touchEvent('touchend', [finger(2, 130, 230)], [finger(1, 120, 220)]));
    const heldByOne = pressed().join();
    face.fire('touchend', touchEvent('touchend', [], [finger(2, 130, 230)]));
    check('a button two fingers hold stays pressed until both lift', heldByOne === '0' && pressed().join() === '',
          [heldByOne, pressed()]);

    face.fire('touchstart', touchEvent('touchstart', [finger(1, 120, 220), finger(2, 60, 170)]));
    check('two fingers press two buttons of one cluster', pressed().join() === '0,2', pressed());
    face.fire('touchcancel', touchEvent('touchcancel', [], [finger(1, 120, 220), finger(2, 60, 170)]));
    check('and a canceled touch releases them', pressed().join() === '', pressed());
    await settle();
}

// --- sticks ------------------------------------------------------------------
{
    const [left, right] = controls('touch-joystick-base');
    left.rect = box(100, 400, 90, 90);
    right.rect = box(500, 400, 90, 90);
    const axes = () => [...(touchPad()?.axes ?? [])];
    const clicks = () => [touchPad()?.buttons[10].pressed, touchPad()?.buttons[11].pressed];

    dispatched.length = 0;
    reported.length = 0;
    left.fire('touchstart', touchEvent('touchstart', [finger(1, 154, 445)]));
    check('a stick reads the touch against its travel', JSON.stringify(axes()) === '[0.5,0,0,0]', axes());
    advance(300);
    left.fire('touchmove', touchEvent('touchmove', [finger(1, 145, 545)]));
    check('clamps it to the base, and down is positive', JSON.stringify(axes()) === '[0,1,0,0]', axes());
    await settle();
    check('a move of both axes is one announcement, and the core reads both',
          dispatched.join() === 'touchgamepadinput' && JSON.stringify(reported) === '[["axis",1,1]]', { dispatched, reported });
    left.fire('touchend', touchEvent('touchend', [], [finger(1, 145, 545)]));
    advance(100);
    check('lifting centers the stick, and a held or moved touch is no click',
          JSON.stringify(axes()) === '[0,0,0,0]' && clicks().join() === 'false,false', [axes(), clicks()]);

    right.fire('touchstart', touchEvent('touchstart', [finger(1, 545, 445)]));
    advance(100);
    right.fire('touchend', touchEvent('touchend', [], [finger(1, 545, 445)]));
    const tapped = clicks().join();
    advance(59);
    const held = clicks().join();
    advance(1);
    check('a short, still tap on the right stick clicks R3 for 60 ms',
          tapped === 'false,true' && held === 'false,true' && clicks().join() === 'false,false', [tapped, held, clicks()]);

    left.fire('touchstart', touchEvent('touchstart', [finger(1, 145, 445)]));
    advance(250);
    left.fire('touchend', touchEvent('touchend', [], [finger(1, 145, 445)]));
    check('a still touch held for 250 ms is not a tap', clicks().join() === 'false,false', clicks());

    left.fire('touchstart', touchEvent('touchstart', [finger(1, 145, 445)]));
    left.fire('touchmove', touchEvent('touchmove', [finger(1, 168, 445)]));
    left.fire('touchmove', touchEvent('touchmove', [finger(1, 145, 445)]));
    advance(100);
    left.fire('touchend', touchEvent('touchend', [], [finger(1, 145, 445)]));
    check('nor is a quick one that strayed past a quarter of the base', clicks().join() === 'false,false', clicks());

    left.fire('touchstart', touchEvent('touchstart', [finger(1, 145, 445)]));
    left.fire('touchstart', touchEvent('touchstart', [finger(1, 145, 445), finger(2, 118, 445)], [finger(2, 118, 445)]));
    check('a second finger on a held stick does not move it', JSON.stringify(axes()) === '[0,0,0,0]', axes());
    left.fire('touchend', touchEvent('touchend', [], [finger(1, 145, 445)]));
    advance(100);
    await settle();
}

// --- analog triggers ---------------------------------------------------------
{
    const [l2] = controls('touch-analog-trigger');
    l2.rect = box(40, 100, 110, 50);
    const trigger = () => touchPad()?.buttons[6];

    l2.fire('touchstart', touchEvent('touchstart', [finger(1, 60, 125)]));
    check('a trigger reads how far down it is touched',
          trigger()?.value === 0.5 && trigger()?.pressed === true && l2.children[0].style.height === '50%',
          [trigger()?.value, l2.children[0].style.height]);
    l2.fire('touchmove', touchEvent('touchmove', [finger(1, 60, 102)]));
    check('and is not pressed at 5% or less', trigger()?.value === 0.04 && trigger()?.pressed === false,
          [trigger()?.value, trigger()?.pressed]);
    l2.fire('touchmove', touchEvent('touchmove', [finger(1, 60, 400)]));
    check('past its end it is fully pressed', trigger()?.value === 1 && trigger()?.pressed === true, trigger()?.value);
    l2.fire('touchend', touchEvent('touchend', [], [finger(1, 60, 400)]));
    check('lifting releases it', trigger()?.value === 0 && trigger()?.pressed === false &&
          l2.children[0].style.height === '0%', trigger()?.value);
    await settle();
}

// --- rumble ------------------------------------------------------------------
{
    vibrations.length = 0;
    manager.rumble(0.8, 0, 200);
    check('a rumble the core relays vibrates the device for its duration', vibrations.join() === '200', vibrations);
    manager.rumble(0, 0, 0);
    check('and stopping it stops the vibrator', vibrations.join() === '200,0', vibrations);
}

// --- beside physical pads, and hidden ----------------------------------------
{
    physical = [physicalPad(0), null, null, null];
    const pads = navigator.getGamepads();
    check('a physical pad keeps its slot and the touch pad takes the next free one',
          pads[0]?.id.startsWith('Audit Pad') === true && pads[1]?.id === PAD_ID && pads[1].index === 1,
          pads.map((pad) => pad && [pad.index, pad.id]));
    physical = [null, null, null, null];

    cluster('faceCluster_mod').fire('touchstart', touchEvent('touchstart', [finger(1, 120, 220)]));
    await settle();
    dispatched.length = 0;
    post({ type: 'TOUCH_GAMEPAD_VISIBILITY', payload: { visible: false, targetDivId: HOST } });
    check('hiding the overlay disconnects the pad', dispatched.join() === 'gamepaddisconnected' && touchPad() === null &&
          overlay()?.style.display === 'none' && overlay()?.children.length === 0, dispatched);

    dispatched.length = 0;
    post({ type: 'TOUCH_GAMEPAD_VISIBILITY', payload: { visible: true, targetDivId: HOST } });
    check('showing it again connects a pad at rest, in the persisted layout',
          dispatched.join() === 'gamepadconnected' && touchPad()?.buttons.every((b) => !b.pressed && b.value === 0) === true &&
          controls('touch-button').length === 13, dispatched);
}

manager.destroy();
console.error = consoleError;
process.exitCode = failed === 0 ? 0 : 1;
