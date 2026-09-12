/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// How the client asks for pointer lock, and where the lock is allowed to land. It wants raw movement deltas
// (unadjustedMovement), which every engine on Linux and Android refuses with
// NotSupportedError, so the refusal has to end in a plain lock rather than in
// no lock at all -- and it has to be remembered, or every lock pays for a
// refused request. macOS is the platform that grants the option and is not
// asked for it by default, since removing its acceleration curve leaves the
// pointer heavy and the client has nothing to put in its place; which platform
// that is has to be decided narrowly, because an iPad reports a Mac's platform
// string too. The `raw_pointer_motion` setting decides over the platform, on
// either side, through the shared settings ladder and the core's setter, which
// switches a lock already held rather than waiting for the next one. The lock belongs to gaming mode alone -- plain fullscreen
// leaves the pointer to the browser so the dashboard stays usable -- and its
// caller guards the request (gaming mode, stream fullscreen, not already locked,
// an input context attached); the request it re-runs after a refusal must pass those
// guards again, since the page can leave fullscreen while the first one is still
// pending.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import { Input } from '../../addons/selkies-web-core/lib/input.js';
import { RAW_POINTER_MOTION_SPEC, resolveSpec } from '../../addons/selkies-web-core/lib/conditional-settings.js';

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [pointer-lock] ${label}  ${detail}`);
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const notSupported = () => {
    const err = new Error('unadjustedMovement');
    err.name = 'NotSupportedError';
    return err;
};

/**
 * An element that records how lock was asked for and answers the way `outcome`
 * says an engine would: 'ok', 'refuse-option' (no raw movement), 'fail'
 * (rejects everything), or 'no-promise' (pre-promise engine).
 */
function makeElement(outcome) {
    const calls = [];
    const element = {
        calls,
        contains: (node) => node === element,
        requestPointerLock(options) {
            calls.push(options && options.unadjustedMovement ? 'unadjusted' : 'plain');
            if (outcome === 'no-promise') return undefined;
            if (outcome === 'fail') return Promise.reject(new Error('WrongDocumentError'));
            if (outcome === 'refuse-option' && options && options.unadjustedMovement) {
                return Promise.reject(notSupported());
            }
            return Promise.resolve();
        },
    };
    return element;
}

/** An Input with only what the pointer lock paths touch. */
function makeInput(element, gaming = true, attached = true) {
    const input = Object.create(Input.prototype);
    input.element = element;
    input.isSharedMode = false;
    input.inputAttached = attached;
    input.gamingMode = gaming;
    input.shortcutsEnabled = true;
    return input;
}

/** A fresh page: raw movement wanted, and the engine not yet asked for it. */
function reset(element) {
    Input.rawPointerMotion = true;
    Input._rawMotionRefused = false;
    globalThis.document = { pointerLockElement: null, fullscreenElement: element,
                            getElementById: () => null };
}

// --- the request itself ---------------------------------------------------
{
    const element = makeElement('ok');
    reset(element);
    const input = makeInput(element);
    input._requestPointerLock(element, () => {}, () => {});
    await sleep(10);
    check('raw movement is asked for', element.calls.join(',') === 'unadjusted',
          element.calls.join(','));
}
{
    const element = makeElement('refuse-option');
    reset(element);
    const input = makeInput(element);
    let failures = 0;
    const lock = () => input._requestPointerLock(element, lock, () => { failures++; });
    lock();
    await sleep(10);
    check('a refused option locks anyway', element.calls.join(',') === 'unadjusted,plain',
          element.calls.join(','));
    check('a refused option is not a failure', failures === 0, String(failures));
    check('the refusal is remembered', Input._rawMotionRefused === true);

    element.calls.length = 0;
    lock();
    await sleep(10);
    check('later locks ask once', element.calls.join(',') === 'plain', element.calls.join(','));
}
{
    const element = makeElement('fail');
    reset(element);
    const input = makeInput(element);
    const errors = [];
    input._requestPointerLock(element, () => { errors.push('retried'); }, (e) => errors.push(e.message));
    await sleep(10);
    check('any other rejection is reported once',
          element.calls.join(',') === 'unadjusted' && errors.join(',') === 'WrongDocumentError',
          `${element.calls.join(',')} / ${errors.join(',')}`);
    check('a real failure does not disable raw movement', Input._rawMotionRefused === false);
}
{
    const element = makeElement('no-promise');
    reset(element);
    const input = makeInput(element);
    input._requestPointerLock(element, () => {}, () => {});
    await sleep(10);
    check('a pre-promise engine is asked once', element.calls.join(',') === 'unadjusted',
          element.calls.join(','));
}

// --- which platforms are asked ---------------------------------------------
// macOS is the one platform that GRANTS raw movement and is worse for it: the
// curve it drops is what carried a slow hand across the remote screen, and the
// client has nothing to put in its place, so the pointer feels heavy. It is
// asked plainly instead. The decision is taken once, when the module loads, so
// it takes a fresh load under a faked navigator to pin it; a query string is
// what makes the second import a second evaluation rather than the cached one.
// The cases that matter are the ones where a platform only looks like macOS, or
// only looks like something else: an iPad reports `MacIntel` too, and a client
// hint may say `Unknown` on a real Mac.
{
    const realNavigator = globalThis.navigator;
    const setNavigator = (value) => Object.defineProperty(globalThis, 'navigator', {
        value, configurable: true, writable: true });
    let seq = 0;
    const loadUnder = async (nav) => {
        setNavigator(nav);
        const mod = await import(`../../addons/selkies-web-core/lib/input.js?platform=${seq++}`);
        return mod.Input.rawPointerMotion;
    };
    try {
        const asksPlainly = [
            ['a Mac', { platform: 'MacIntel' }],
            ['a Mac whose client hint says nothing useful',
             { platform: 'MacIntel', userAgentData: { platform: 'Unknown' } }],
            ['a Mac that only reports a client hint', { userAgentData: { platform: 'macOS' } }],
        ];
        for (const [what, nav] of asksPlainly) {
            check(`${what} starts without raw movement`, (await loadUnder(nav)) === false);
        }
        const asksForRaw = [
            ['an iPad in its desktop-class default', { platform: 'MacIntel', maxTouchPoints: 5 }],
            ['an iPhone', { platform: 'iPhone', maxTouchPoints: 5 }],
            ['Windows', { platform: 'Win32' }],
            ['Linux', { platform: 'Linux x86_64' }],
            ['a navigator that says nothing', {}],
        ];
        for (const [what, nav] of asksForRaw) {
            check(`${what} starts with raw movement`, (await loadUnder(nav)) === true);
        }
    } finally {
        setNavigator(realNavigator);
    }
}
{
    // What the macOS start means at the request: the option is never asked for,
    // so there is no refusal to recover from and the first lock is the lock.
    const element = makeElement('ok');
    reset(element);
    Input.rawPointerMotion = false;
    const input = makeInput(element);
    let retried = 0;
    input._requestPointerLock(element, () => { retried++; }, () => {});
    await sleep(10);
    check('a macOS start asks plainly the first time', element.calls.join(',') === 'plain',
          element.calls.join(','));
    check('and needs no second request', retried === 0, String(retried));
}

// --- the setting -----------------------------------------------------------
// The platform only decides until the core resolves `raw_pointer_motion`: the
// server's default, off on a Mac unless someone chose or the operator set it,
// and a locked value that nobody overrides.
{
    const resolve = (ctx, stored, server) => resolveSpec(
        RAW_POINTER_MOTION_SPEC, { raw_pointer_motion: server }, ctx, () => stored);
    check('the setting defaults on where the platform grants or refuses it harmlessly',
          resolve({ macDesktop: false }, null, { value: true }) === true);
    check('the setting defaults off on a Mac', resolve({ macDesktop: true }, null, { value: true }) === false);
    check('a stored choice beats the Mac default', resolve({ macDesktop: true }, 'true', { value: true }) === true);
    check('an operator value beats the Mac default',
          resolve({ macDesktop: true }, null, { value: true, overridden: true }) === true);
    check('a stored choice beats an operator value that is not locked',
          resolve({ macDesktop: false }, 'false', { value: true, overridden: true }) === false);
    check('a locked value beats a stored choice',
          resolve({ macDesktop: true }, 'true', { value: false, locked: true }) === false);
    const posted = [];
    RAW_POINTER_MOTION_SPEC.propagate(false, {}, { postToCore: (m) => posted.push(m), postSetting: () => {} });
    check('a change is propagated to the core as setRawPointerMotion',
          JSON.stringify(posted) === '[{"type":"setRawPointerMotion","value":false}]', JSON.stringify(posted));
}
{
    // Turned off: the request asks plainly, with no refusal to recover from.
    const element = makeElement('ok');
    reset(element);
    const input = makeInput(element);
    input.setRawPointerMotion(false);
    input._requestPointerLock(element, () => {}, () => {});
    await sleep(10);
    check('turned off, the lock asks plainly', element.calls.join(',') === 'plain', element.calls.join(','));
    // Turned back on: asked for again, since nothing was refused.
    element.calls.length = 0;
    input.setRawPointerMotion(true);
    input._requestPointerLock(element, () => {}, () => {});
    await sleep(10);
    check('turned on, the lock asks for raw movement', element.calls.join(',') === 'unadjusted',
          element.calls.join(','));
}
{
    // Turned on after a refusal: the engine is not asked twice for what it refused.
    const element = makeElement('refuse-option');
    reset(element);
    const input = makeInput(element);
    const lock = () => input._requestPointerLock(element, lock, () => {});
    lock();
    await sleep(10);
    input.setRawPointerMotion(false);
    input.setRawPointerMotion(true);
    element.calls.length = 0;
    lock();
    await sleep(10);
    check('a refusal outlives the toggle', element.calls.join(',') === 'plain', element.calls.join(','));
}
{
    // Changed while the stream holds the lock: the held lock is asked again with
    // the new option, so the change is not deferred to the next lock.
    const element = makeElement('ok');
    reset(element);
    const input = makeInput(element);
    document.pointerLockElement = element;
    input.setRawPointerMotion(false);
    await sleep(10);
    check('a held lock is re-asked plainly when raw movement is turned off',
          element.calls.join(',') === 'plain', element.calls.join(','));
    element.calls.length = 0;
    input.setRawPointerMotion(true);
    await sleep(10);
    check('and re-asked for raw movement when it is turned on', element.calls.join(',') === 'unadjusted',
          element.calls.join(','));
    element.calls.length = 0;
    input.setRawPointerMotion(true);
    await sleep(10);
    check('an unchanged setting asks nothing', element.calls.length === 0, element.calls.join(','));
    document.pointerLockElement = null;
    input.setRawPointerMotion(false);
    await sleep(10);
    check('with no lock held nothing is asked', element.calls.length === 0, element.calls.join(','));
}
{
    // The held lock is on an engine that refuses the option: the refusal is
    // remembered and the lock is asked plainly again, so it is kept as it was.
    const element = makeElement('refuse-option');
    reset(element);
    const input = makeInput(element);
    Input.rawPointerMotion = false;
    document.pointerLockElement = element;
    input.setRawPointerMotion(true);
    await sleep(20);
    check('a held lock whose engine refuses falls back to a plain re-request',
          element.calls.join(',') === 'unadjusted,plain' && Input._rawMotionRefused === true,
          `${element.calls.join(',')} refused=${Input._rawMotionRefused}`);
    document.pointerLockElement = null;
}

// --- where a lock may land -------------------------------------------------
// A Ctrl-Shift-Click asks the element it hit, which is the input overlay
// wherever the overlay covers the stream and one of the sinks under it where it
// does not: either core's video element, and the ws-core canvases. A lock the
// guard does not recognise leaves the pointer locked while motion is still sent
// as absolute position, so the guard has to know every one of them.
function stage(ids, locked = null) {
    const made = {};
    for (const id of ids) made[id] = { id, requestPointerLock: () => Promise.resolve() };
    globalThis.document = { pointerLockElement: locked, fullscreenElement: null,
                            getElementById: (id) => made[id] || null };
    return made;
}

{
    const overlay = makeElement('ok');
    const sinks = ['videoCanvas', 'videoWorkerCanvas', 'videoStream', 'stream'];
    for (const id of sinks) {
        const made = stage(sinks);
        globalThis.document.pointerLockElement = made[id];
        const input = makeInput(overlay);
        check(`a lock on ${id} reads as locked to the stream`, input._isStreamLocked());
    }
    const made = stage([...sinks, 'sidebar']);
    const input = makeInput(overlay);
    globalThis.document.pointerLockElement = overlay;
    check('a lock on the overlay reads as locked to the stream', input._isStreamLocked());
    globalThis.document.pointerLockElement = made.sidebar;
    check('a lock on something else does not', !input._isStreamLocked());
    globalThis.document.pointerLockElement = null;
    check('no lock at all does not', !input._isStreamLocked());
}
{
    // The request itself: the click's own element when it is a sink, and the
    // overlay for anything else -- every element carries requestPointerLock, so
    // asking whether the target has one picks the target every time.
    const asked = [];
    const overlay = makeElement('ok');
    const sinks = ['stream', 'sidebar'];
    const made = stage(sinks);
    for (const el of [overlay, made.stream, made.sidebar]) {
        el.requestPointerLock = () => { asked.push(el.id || 'overlay'); return Promise.resolve(); };
    }
    const input = makeInput(overlay);
    Object.assign(input, {
        buttonMask: 1, inputAttached: false, use_browser_cursors: true,
        cursorDiv: { style: {} }, _trackpadMode: false,
        _releaseDesyncedModifiers: () => {}, _focusCompositionHost: () => {},
        _inputDpr: () => 1,
    });
    const click = (target) => input._mouseButtonMovement({
        type: 'mousedown', button: 0, ctrlKey: true, shiftKey: true, target,
        clientX: 4, clientY: 4, preventDefault: () => {},
    });
    click(overlay);
    click(made.stream);
    click(made.sidebar);
    await sleep(10);
    check('a lock is asked of the sink that was clicked, and of the overlay otherwise',
          asked.join(',') === 'overlay,stream,overlay', asked.join(','));
}

// --- the gaming-mode caller -----------------------------------------------
{
    const element = makeElement('refuse-option');
    reset(element);
    const input = makeInput(element);
    input._armPointerLock();
    await sleep(10);
    check('gaming mode locks through a refusal', element.calls.join(',') === 'unadjusted,plain',
          element.calls.join(','));
}
{
    // Plain fullscreen: the pointer is the browser's, so nothing is asked for.
    const element = makeElement('ok');
    reset(element);
    makeInput(element, false)._armPointerLock();
    await sleep(10);
    check('plain fullscreen asks for no lock', element.calls.length === 0, element.calls.join(','));
}
{
    // The page leaves fullscreen while the first request is still pending
    const element = makeElement('refuse-option');
    reset(element);
    const input = makeInput(element);
    element.requestPointerLock = (options) => {
        element.calls.push(options && options.unadjustedMovement ? 'unadjusted' : 'plain');
        if (options && options.unadjustedMovement) {
            return new Promise((_, reject) => setTimeout(() => reject(notSupported()), 20));
        }
        return Promise.resolve();
    };
    input._armPointerLock();
    document.fullscreenElement = null;
    await sleep(60);
    check('leaving fullscreen stops the retried request',
          element.calls.join(',') === 'unadjusted', element.calls.join(','));
}
{
    const element = makeElement('fail');
    reset(element);
    const input = makeInput(element);
    input._armPointerLock();
    await sleep(700);
    // The transition race is retried a few times; asking for raw movement must
    // not double the requests that budget allows
    check('a failing lock retries a bounded number of times',
          element.calls.length === 6, String(element.calls.length));
}
{
    const element = makeElement('ok');
    reset(element);
    const input = makeInput(element);
    document.fullscreenElement = null;
    input._armPointerLock();
    document.fullscreenElement = element;
    document.pointerLockElement = element;
    input._armPointerLock();
    document.pointerLockElement = null;
    input.inputAttached = false;
    input._armPointerLock();
    await sleep(10);
    input.inputAttached = true;
    input.gamingMode = false;
    input._armPointerLock();
    await sleep(10);
    check('no lock outside fullscreen, when locked, without an input context, or outside gaming mode',
          element.calls.length === 0, element.calls.join(','));
}
{
    // A viewer the server granted mouse and keyboard access holds a context
    // while it keeps the viewer role, and gaming mode is where its relative
    // motion comes from; a viewer holding no context is the one refused.
    const element = makeElement('ok');
    reset(element);
    const input = makeInput(element);
    input.isSharedMode = true;
    input._armPointerLock();
    await sleep(10);
    check('a viewer granted an input context locks the pointer in gaming mode',
          element.calls.length === 1, element.calls.join(','));

    const refused = makeElement('ok');
    reset(refused);
    const viewer = makeInput(refused, true, false);
    viewer.isSharedMode = true;
    viewer._armPointerLock();
    await sleep(10);
    check('a viewer without one is refused it',
          refused.calls.length === 0, refused.calls.join(','));
}

// --- the two fullscreen modes ---------------------------------------------
{
    const element = makeElement('ok');
    reset(element);
    const events = [];
    const keyboard = { calls: [], lock: (keys) => { keyboard.calls.push('lock'); return Promise.resolve(); },
                       unlock: () => keyboard.calls.push('unlock') };
    Object.defineProperty(globalThis, 'navigator', { value: { keyboard }, configurable: true });
    const requested = [];
    document.documentElement = { requestFullscreen: () => { requested.push('fullscreen'); return Promise.resolve(); } };
    document.exitPointerLock = () => { document.pointerLockElement = null; };
    const input = makeInput(element, false);
    input.ongamingmode = (active) => events.push(active);
    input.send = () => {};
    input.resetKeyboard = () => {};

    document.fullscreenElement = null;
    input.enterFullscreen();
    await sleep(10);
    check('plain fullscreen is fullscreen and nothing else',
          requested.join(',') === 'fullscreen' && input.gamingMode === false && events.length === 0,
          `${requested.join(',')} gaming=${input.gamingMode} events=${events.join(',')}`);

    // What the engine reports once the transition lands, with no mode asked for.
    document.fullscreenElement = element;
    input._onFullscreenChange();
    await sleep(10);
    check('plain fullscreen locks neither pointer nor keyboard',
          element.calls.length === 0 && keyboard.calls.length === 0,
          `${element.calls.join(',')} / ${keyboard.calls.join(',')}`);

    document.fullscreenElement = null;
    input.enterGamingMode();
    await sleep(10);
    check('gaming mode asks for fullscreen and announces itself',
          requested.length === 2 && input.gamingMode === true && events.join(',') === 'true',
          `${requested.join(',')} gaming=${input.gamingMode} events=${events.join(',')}`);

    document.fullscreenElement = element;
    input._onFullscreenChange();
    await sleep(10);
    check('gaming mode takes the pointer and the keyboard',
          element.calls.length > 0 && keyboard.calls.join(',') === 'lock',
          `${element.calls.join(',')} / ${keyboard.calls.join(',')}`);

    document.fullscreenElement = null;
    input._onFullscreenChange();
    await sleep(10);
    check('leaving fullscreen leaves gaming mode',
          input.gamingMode === false && events.join(',') === 'true,false'
          && keyboard.calls.join(',') === 'lock,unlock',
          `gaming=${input.gamingMode} events=${events.join(',')} kb=${keyboard.calls.join(',')}`);
}
{
    // An engine that refuses fullscreen must not leave the mode behind: the locks
    // would then be armed by the next transition the page makes for any reason.
    const element = makeElement('ok');
    reset(element);
    document.fullscreenElement = null;
    document.documentElement = { requestFullscreen: () => Promise.reject(new Error('denied')) };
    const input = makeInput(element, false);
    const events = [];
    input.ongamingmode = (active) => events.push(active);
    input.enterGamingMode();
    await sleep(20);
    check('a refused fullscreen rolls the mode back',
          input.gamingMode === false && events.join(',') === 'true,false',
          `gaming=${input.gamingMode} events=${events.join(',')}`);
}

// --- a keyboard the engine will not lock -----------------------------------
// Without the Keyboard Lock API a single Escape leaves fullscreen and the
// pointer lock, and gaming mode with them; the page can neither prevent that
// nor see the key, so it says so once, naming Brave's Shields where those are
// what withhold the API. An engine that locks the keyboard hears nothing.
{
    const element = makeElement('ok');
    reset(element);
    document.fullscreenElement = element;
    Object.defineProperty(globalThis, 'navigator', { value: {}, configurable: true });
    Input._keyboardLockNoticed = false;
    const input = makeInput(element);
    const notices = [];
    input.onnotice = (code, text) => notices.push(`${code}:${text.length > 0}`);
    input.requestKeyboardLock();
    input.requestKeyboardLock();
    check('a missing keyboard lock is announced once, as unavailable',
          notices.join(',') === 'keyboardLockUnavailable:true', notices.join(','));

    Object.defineProperty(globalThis, 'navigator', { value: { brave: {} }, configurable: true });
    Input._keyboardLockNoticed = false;
    notices.length = 0;
    input.requestKeyboardLock();
    check('Brave without the API is told about its Shields',
          notices.join(',') === 'keyboardLockBlockedByShields:true', notices.join(','));

    const keyboard = { calls: [], lock: () => { keyboard.calls.push('lock'); return Promise.resolve(); },
                       unlock: () => keyboard.calls.push('unlock') };
    Object.defineProperty(globalThis, 'navigator', { value: { keyboard }, configurable: true });
    Input._keyboardLockNoticed = false;
    notices.length = 0;
    input.requestKeyboardLock();
    check('an engine that locks the keyboard hears no notice',
          keyboard.calls.join(',') === 'lock' && notices.length === 0,
          `${keyboard.calls.join(',')} / ${notices.join(',')}`);
}

process.exit(failed === 0 ? 0 : 1);
