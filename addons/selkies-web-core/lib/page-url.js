/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The page URL as the client reads and hands it on.
 *
 * Its fragment holds a keyword that picks the display or the sharing role
 * (`#display2-right`, `#shared`, `#player2`) and may hold the session token as
 * a `token=` item beside it (`#display2-right&token=<t>`), in either order.
 * Everything that reads the fragment reads the keyword alone
 * (`urlFragmentKeyword`), so no hash parser sees the token and none logs it,
 * and a window this page opens for the session carries a fragment token on in
 * its own fragment (`fragmentWithSessionToken`).
 * @module
 */

/**
 * Splits the page URL's fragment into its keyword and its token.
 *
 * Items are `&`-separated; the token is the `token=` item, percent-decoded as a
 * query value is, and the keyword is the first other item.
 * @returns {{keyword: string, token: string}} The keyword with its `#`, or `''`
 *     without one, and the token, or `''`.
 */
export function readUrlFragment() {
    let keyword = '';
    let token = '';
    if (typeof window === 'undefined' || !window.location) return { keyword, token };
    const fragment = String(window.location.hash || '').replace(/^#/, '');
    for (const item of fragment.split('&')) {
        if (item.startsWith('token=')) {
            if (!token) {
                try {
                    token = new URLSearchParams(item).get('token') || '';
                } catch (_) { /* an undecodable item carries no token */ }
            }
        } else if (!keyword && item) {
            keyword = `#${item}`;
        }
    }
    return { keyword, token };
}

/**
 * The display or sharing keyword of the page URL's fragment, without the token
 * that may ride beside it.
 * @returns {string} The keyword with its `#`, or `''` when there is none.
 */
export function urlFragmentKeyword() {
    return readUrlFragment().keyword;
}

/**
 * The fragment for another page of this session, such as the second display's
 * window: the keyword, then the page's token when it came in the fragment. A
 * query token stays in the query, which the caller keeps.
 * @param {string} keyword The keyword without its `#` (`display2-right`).
 * @returns {string} `#keyword`, with `&token=` added for a fragment token.
 */
export function fragmentWithSessionToken(keyword) {
    const { token } = readUrlFragment();
    return token ? `#${keyword}&token=${encodeURIComponent(token)}` : `#${keyword}`;
}

