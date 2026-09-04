/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What the shared file-listing footer does to a fancyindex page, run against
// the copy named on the command line: the parent row is removed at the mount's
// root and nowhere else, whichever mount, with or without the root's trailing
// slash, and however deep a subdirectory that happens to be named after a
// mount; a sorted listing, whose every link carries the sort query, keeps its
// directories navigable; and a session token joins a query already there.
//
// Usage: node file_index_footer_audit.mjs <footer.html or stream_server.py>
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import { readFileSync } from 'node:fs';

const source = readFileSync(process.argv[2], 'utf8');
const start = source.indexOf('function processDirectoryListing()');
const end = source.indexOf('let attempts = 0;', start);
if (start === -1 || end === -1) {
    console.log('FAIL  [file-index-footer] processDirectoryListing not found in ' + process.argv[2]);
    process.exit(1);
}
const listing = new Function('window', 'document', source.slice(start, end) + '\nprocessDirectoryListing();');

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [file-index-footer] ${label}  ${detail}`);
}

/** The few DOM members the footer touches, over a listing's rows. */
function node(tag, attrs = {}, parent = null) {
    return {
        tag, attrs: { ...attrs }, parent, style: {}, removed: false, textContent: '',
        getAttribute(k) { return k in this.attrs ? this.attrs[k] : null; },
        setAttribute(k, v) { this.attrs[k] = String(v); },
        hasAttribute(k) { return k in this.attrs; },
        closest(sel) { for (let n = this; n; n = n.parent) if (n.tag === sel) return n; return null; },
        remove() { this.removed = true; },
    };
}

/** Runs the footer over a page at `pathname` whose table holds `hrefs`. */
function run(pathname, search, hrefs) {
    const h1 = node('h1');
    h1.textContent = 'Index of ' + pathname;
    const table = node('table', { id: 'list' });
    const anchors = hrefs.map((href) => {
        const tr = node('tr', {}, table);
        const td = node('td', {}, tr);
        return node('a', { href }, td);
    });
    const live = () => anchors.filter((a) => !a.closest('tr').removed);
    const document = {
        querySelector(sel) {
            if (sel === 'h1') return h1;
            if (sel === 'table#list a[href^="../"]') return live().find((a) => a.attrs.href.startsWith('../')) || null;
            throw new Error('unexpected selector ' + sel);
        },
        querySelectorAll(sel) {
            if (sel === 'table#list td a') return live();
            throw new Error('unexpected selector ' + sel);
        },
    };
    listing({ location: { pathname, search } }, document);
    const parent = anchors.find((a) => a.attrs.href.startsWith('../'));
    return {
        h1: h1.textContent,
        parentShown: !!parent && !parent.closest('tr').removed && parent.closest('tr').style.display !== 'none',
        links: Object.fromEntries(anchors.map((a) => [a.attrs.href, { href: a.attrs.href, download: a.hasAttribute('download') }])),
        anchors,
    };
}

const PLAIN = ['../', 'sub/', 'a.txt'];

// The parent row: gone at the root of either mount, whatever surrounds it,
// and present one level down, also in a subdirectory named after a mount.
for (const [pathname, shown] of [
    ['/files/', false], ['/files', false], ['/api/files/', false], ['/api/files', false],
    ['/selkies/files/', false], ['/selkies/api/files/', false],
    ['/files/sub/', true], ['/api/files/sub/', true],
    ['/files/backup/files/', true], ['/files/api/files/', true], ['/api/files/x/files/', true],
]) {
    const r = run(pathname, '', PLAIN);
    check(`parent row ${shown ? 'kept' : 'removed'} at ${pathname}`, r.parentShown === shown, `h1=${JSON.stringify(r.h1)}`);
}

// The title shows the disk path below the mount.
check('title shows the path below the mount', run('/api/files/sub/', '', PLAIN).h1 === '/sub/');

// Files download, directories navigate, sorted or not.
let r = run('/files/sub/', '', PLAIN);
check('a file is marked for download', r.links['a.txt'].download);
check('a directory is not', !r.links['sub/'].download && !r.links['../'].download);
const SORTED = ['../?C=S&O=A', 'sub/?C=S&O=A', 'a.txt'];
r = run('/files/sub/', '?C=S&O=A', SORTED);
check('a sorted listing keeps its parent row below the root', r.parentShown);
check('a sorted listing keeps its directories navigable',
      !r.links['../?C=S&O=A'].download && !r.links['sub/?C=S&O=A'].download, JSON.stringify(r.links));
check('a sorted listing still marks its files for download', r.links['a.txt'].download);
r = run('/files/', '?C=S&O=A', SORTED);
check('a sorted listing at the root loses its parent row', !r.parentShown);

// A session token rides every link, joining a query already there.
r = run('/api/files/sub/', '?token=t%20k', ['../', 'sub/', 'a.txt', 'b.txt?v=2']);
const hrefs = r.anchors.map((a) => a.attrs.href);
check('the token is appended to plain links', hrefs.includes('sub/?token=t%20k') && hrefs.includes('a.txt?token=t%20k'), JSON.stringify(hrefs));
check('the token joins an existing query with an ampersand', hrefs.includes('b.txt?v=2&token=t%20k'), JSON.stringify(hrefs));

process.exit(failed ? 1 : 0);
