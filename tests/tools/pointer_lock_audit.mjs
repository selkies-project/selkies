/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// How the client asks for pointer lock. It wants raw movement deltas
// (unadjustedMovement), which every engine on Linux and Android refuses with
// NotSupportedError, so the refusal has to end in a plain lock rather than in
// no lock at all -- and it has to be remembered, or every lock pays for a
// refused request. The fullscreen caller guards its request (gaming mode, stream
// fullscreen, not already locked, not a shared viewer); the request it re-runs
// after a refusal must pass those guards again, since the page can leave
// fullscreen while the first one is still pending.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import { Input } from '../../addons/selkies-web-core/lib/input.js';

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
function makeInput(element) {
    const input = Object.create(Input.prototype);
    input.element = element;
    input.isSharedMode = false;
    input.gamingMode = true;
    return input;
}

/** A fresh page: the engine has not been asked for raw movement yet. */
function reset(element) {
    Input._unadjustedMovement = true;
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
    check('the refusal is remembered', Input._unadjustedMovement === false);

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
    check('a real failure does not disable raw movement', Input._unadjustedMovement === true);
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

// --- the fullscreen caller ------------------------------------------------
{
    const element = makeElement('refuse-option');
    reset(element);
    const input = makeInput(element);
    input._armPointerLock();
    await sleep(10);
    check('fullscreen locks through a refusal', element.calls.join(',') === 'unadjusted,plain',
          element.calls.join(','));
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
    check('a failing fullscreen lock retries a bounded number of times',
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
    input.isSharedMode = true;
    input._armPointerLock();
    input.isSharedMode = false;
    // A plain fullscreen of the same document is not gaming mode
    input.gamingMode = false;
    input._armPointerLock();
    await sleep(10);
    check('no fullscreen lock outside fullscreen or gaming mode, when locked, or when shared',
          element.calls.length === 0, element.calls.join(','));
}

process.exit(failed === 0 ? 0 : 1);
