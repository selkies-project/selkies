/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What a viewer's page has been handed: a gamepad slot to drive, and the
 * keyboard and pointer. A shared viewer has no menu, so the floating buttons
 * it gets (the touch gamepad, the on-screen keyboard) follow these grants,
 * which change live as the session's controller hands slots and input around.
 *
 * Both cores learn them where the server says them: the role verdict
 * (`AUTH_SUCCESS` and `ROLE_UPDATE` over WebSockets, `auth_success` and
 * `role_update` over WebRTC) carries a token's slots, and `MK_ACCESS` or
 * `mk_access` whether the page holds input. Without tokens a `#playerN` page
 * holds the slot its keyword names and a `#shared` page holds nothing. Each
 * core publishes the grants on `window.selkiesViewerGrants`, which a dashboard
 * mounted after the verdict reads, and posts `viewerGrants` on every change.
 * @module
 */
import { isRecord } from './narrow.ts';

/** A viewer's grants. */
export interface ViewerGrants {
    /** Whether the page holds a gamepad slot. */
    pad: boolean;
    /** Whether the page holds the keyboard and pointer. */
    input: boolean;
}

/** The grants of a page told nothing yet. */
export const NO_GRANTS: ViewerGrants = { pad: false, input: false };

/**
 * Whether a value is a grants record, as the window mirror and the message carry it.
 * @param value What was read.
 * @returns True where both grants are booleans.
 */
export function isViewerGrants(value: unknown): value is ViewerGrants {
    return isRecord(value) && typeof value.pad === 'boolean' && typeof value.input === 'boolean';
}

/**
 * Changes the page's grants, mirroring them on the window and posting
 * `viewerGrants` where they moved.
 * @param change The grants that changed; the others keep their value.
 */
export function publishViewerGrants(change: Partial<ViewerGrants>): void {
    if (typeof window === 'undefined') return;
    const current = window.selkiesViewerGrants ?? NO_GRANTS;
    const next: ViewerGrants = { pad: change.pad ?? current.pad, input: change.input ?? current.input };
    if (window.selkiesViewerGrants && next.pad === current.pad && next.input === current.input) return;
    window.selkiesViewerGrants = next;
    window.postMessage({ type: 'viewerGrants', ...next }, window.location.origin);
}
