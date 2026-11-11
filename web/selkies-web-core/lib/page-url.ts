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
 * (`urlFragmentKeyword`), so no hash parser sees the token and none logs it; a
 * window this page opens for the session carries a fragment token on in its own
 * fragment (`fragmentWithSessionToken`), and a sharing link starts from the page
 * without any token (`shareablePageURL`).
 * @module
 */

/** The fragment's keyword (`#display2-right`) and token, either possibly `''`. */
export interface UrlFragment {
  keyword: string;
  token: string;
}

/**
 * Splits the page URL's fragment into its keyword and its token.
 *
 * Items are `&`-separated; the token is the `token=` item, percent-decoded as a
 * query value is, and the keyword is the first other item.
 * @returns The keyword with its `#`, or `''` without one, and the token, or `''`.
 */
export function readUrlFragment(): UrlFragment {
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
 * @returns The keyword with its `#`, or `''` when there is none.
 */
export function urlFragmentKeyword(): string {
  return readUrlFragment().keyword;
}

/**
 * The fragment for another page of this session, such as the second display's
 * window: the keyword, then the page's token when it came in the fragment. A
 * query token stays in the query, which the caller keeps.
 * @param keyword The keyword without its `#` (`display2-right`).
 * @returns `#keyword`, with `&token=` added for a fragment token.
 */
export function fragmentWithSessionToken(keyword: string): string {
  const { token } = readUrlFragment();
  return token ? `#${keyword}&token=${encodeURIComponent(token)}` : `#${keyword}`;
}

/**
 * The page URL a sharing link starts from: this page without its fragment and
 * without a `token` query parameter, so a link handed to someone else never
 * carries the sharer's own credential.
 */
export function shareablePageURL(): string {
  if (typeof window === 'undefined' || !window.location) return '';
  try {
    const url = new URL(window.location.href);
    url.searchParams.delete('token');
    url.hash = '';
    return url.href;
  } catch (_) {
    return `${window.location.origin}${window.location.pathname}`;
  }
}

/**
 * `shareablePageURL` as a page-relative link: the path and the query without
 * `token`, every segment and value encoded again, for the `href` of a link the
 * page renders.
 */
export function shareablePagePath(): string {
  if (typeof window === 'undefined' || !window.location) return '';
  const decoded = (part: string): string => {
    try { return decodeURIComponent(part); } catch (_) { return part; }
  };
  const url = new URL(window.location.href);
  url.searchParams.delete('token');
  const segments: string[] = [];
  for (const segment of url.pathname.split('/')) segments.push(encodeURIComponent(decoded(segment)));
  const pairs: string[] = [];
  for (const [key, value] of url.searchParams) pairs.push(`${encodeURIComponent(key)}=${encodeURIComponent(value)}`);
  const path = segments.join('/');
  return pairs.length ? `${path}?${pairs.join('&')}` : path;
}
