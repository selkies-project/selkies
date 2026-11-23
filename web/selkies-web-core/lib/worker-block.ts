/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Whether the page's Content-Security-Policy refuses the client's workers.
 *
 * Every worker the client starts runs from a `blob:` or `data:` URL. A policy
 * whose `worker-src` (or the `child-src`, `script-src`, or `default-src` it
 * falls back to) leaves those out does not make `new Worker()` throw: Chromium,
 * Firefox, and WebKit hand back a worker whose `error` event fires later, the
 * same event a worker whose own script throws fires. What tells the two apart
 * is the `securitypolicyviolation` event the document receives for the
 * refused fetch, which all three engines dispatch before the worker's `error`
 * event; a reader answering that `error` event still asks on the task after
 * it, so an engine ordering the two the other way is answered the same. The
 * fallbacks that run a worker's job on the page take this answer,
 * never the `error` event alone, so a session whose policy allows its workers
 * keeps every path it had, even when one of those workers fails on its own.
 * A report-only policy blocks nothing and does not count.
 *
 * The listener is installed when this module is evaluated, ahead of the first
 * worker the importing modules start.
 * @module
 */

let blocked = false;

/** The URL schemes the client's workers are started from. */
const WORKER_SCHEMES = /^(blob|data)(:|$)/;

if (typeof document !== 'undefined') {
    document.addEventListener('securitypolicyviolation', (e: SecurityPolicyViolationEvent): void => {
        if (e.disposition === 'enforce' && e.effectiveDirective === 'worker-src' && WORKER_SCHEMES.test(e.blockedURI)) {
            blocked = true;
        }
    });
}

/** @returns Whether the page's policy has refused one of the client's workers. */
export function workersBlocked(): boolean {
    return blocked;
}
