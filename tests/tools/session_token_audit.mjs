/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Where the page URL's session token comes from and where it goes. A token in
// the fragment reaches the server only as a header, a subprotocol, or the
// cookie, never in a URL; a query token keeps its URL carriers. The fragment's
// display or sharing keyword reads the same with or without a token beside it,
// and a sharing link never carries the sharer's token.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

let failed = 0;
function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [session-token] ${label}  ${detail}`);
}

let cookies = [];
function at(url) {
    globalThis.window = { location: new URL(url) };
    cookies = [];
    globalThis.document = { set cookie(value) { cookies.push(value); } };
}
at('http://host.test/app/');

const {
    SESSION_TOKEN_PROTOCOL, SESSION_TOKEN_PROTOCOL_PREFIX, getSessionToken, installSessionCookie,
    sessionAuthHeaders, sessionTokenInFragment, sessionTokenProtocols, withSessionToken,
} = await import('../../addons/selkies-web-core/lib/session-token.js');
const { fragmentWithSessionToken, shareablePageURL, urlFragmentKeyword } =
    await import('../../addons/selkies-web-core/lib/page-url.js');

at('http://host.test/app/#token=f1');
check('a fragment token is read', getSessionToken() === 'f1' && sessionTokenInFragment(), getSessionToken());
check('a bare token leaves no keyword', urlFragmentKeyword() === '', urlFragmentKeyword());

at('http://host.test/app/#display2-left&token=f1');
check('the keyword before the token reads alone',
      urlFragmentKeyword() === '#display2-left' && getSessionToken() === 'f1', urlFragmentKeyword());
at('http://host.test/app/#token=f1&shared');
check('the keyword after the token reads alone',
      urlFragmentKeyword() === '#shared' && getSessionToken() === 'f1', urlFragmentKeyword());

at('http://host.test/app/?token=q1#player2');
check('a query token is read, with the keyword as it was',
      getSessionToken() === 'q1' && !sessionTokenInFragment() && urlFragmentKeyword() === '#player2');
at('http://host.test/app/?token=q1#token=f1');
check('the fragment token wins over the query one', getSessionToken() === 'f1', getSessionToken());
at('http://host.test/app/#token=a%2Bb%26c%C3%BC');
check('a fragment token is percent-decoded', getSessionToken() === 'a+b&cü', getSessionToken());
at('http://host.test/app/#shared');
check('a keyword alone carries no token', getSessionToken() === '' && urlFragmentKeyword() === '#shared');

const odd = 'unit odd/token;=ü&+#';
at(`http://host.test/app/#token=${encodeURIComponent(odd)}`);
const protocols = sessionTokenProtocols();
const carried = protocols[1] || '';
const decoded = Buffer.from(carried.slice(SESSION_TOKEN_PROTOCOL_PREFIX.length), 'base64url').toString('utf8');
check('a fragment token is offered as selkies plus its token subprotocol',
      protocols.length === 2 && protocols[0] === SESSION_TOKEN_PROTOCOL && carried.startsWith(SESSION_TOKEN_PROTOCOL_PREFIX),
      JSON.stringify(protocols));
check('the token subprotocol decodes to the token', decoded === odd, decoded);
check('the token subprotocol stays within base64url',
      /^[A-Za-z0-9_-]+$/.test(carried.slice(SESSION_TOKEN_PROTOCOL_PREFIX.length)), carried);
at('http://host.test/app/?token=q1');
check('a query token is offered no subprotocol', sessionTokenProtocols().length === 0);
at('http://host.test/app/');
check('a page without a token is offered no subprotocol', sessionTokenProtocols().length === 0);

at('http://host.test/app/#token=f1');
check('a fragment token stays off navigated URLs', withSessionToken('./api/files/') === './api/files/');
check('a fragment token rides the Bearer header', sessionAuthHeaders().Authorization === 'Bearer f1');
installSessionCookie();
check('a fragment token is mirrored into the API cookie',
      cookies.length === 1 && cookies[0].startsWith('selkies_token=f1;') && cookies[0].includes('path=/app/api/'),
      cookies[0]);
at('http://host.test/app/?token=q1');
check('a query token rides navigated URLs as before',
      withSessionToken('./api/files/') === 'http://host.test/app/api/files/?token=q1', withSessionToken('./api/files/'));

at(`http://host.test/app/#token=${encodeURIComponent(odd)}`);
const second = fragmentWithSessionToken('display2-right');
at(`http://host.test/app/${second}`);
check('the second display carries a fragment token in its own fragment',
      urlFragmentKeyword() === '#display2-right' && getSessionToken() === odd && sessionTokenInFragment(), second);
at('http://host.test/app/?token=q1');
check('the second display leaves a query token to the query it keeps',
      fragmentWithSessionToken('display2-right') === '#display2-right');

at('http://host.test/app/?token=q1&keep=1#player2');
check('a sharing link drops a query token', shareablePageURL() === 'http://host.test/app/?keep=1', shareablePageURL());
at('http://host.test/app/#display2-left&token=f1');
check('a sharing link drops a fragment token', shareablePageURL() === 'http://host.test/app/', shareablePageURL());

process.exit(failed ? 1 : 0);
