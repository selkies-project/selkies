/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What a copy on the local clipboard goes to the session as. An office
// application copying formatted text offers a picture of the selection beside
// its markup and plain text (Word's bitmap, which Chromium hands the page as
// image/png), so a copy whose markup carries text is text; a copied picture's
// markup is only an img tag, so the picture stays the picture.
//
// The engine's clipboard is modeled by the item its read() returns, and its
// DOMParser by the text a body shows: what is outside the head, styles,
// scripts and comments.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

/** The text `html` shows, read in one pass, so nothing a removal joins up is read as markup. */
function shownText(html) {
    let shown = '';
    let at = 0;
    while (at < html.length) {
        const open = html.indexOf('<', at);
        if (open < 0) {
            shown += html.slice(at);
            break;
        }
        shown += html.slice(at, open);
        const comment = html.startsWith('<!--', open);
        const end = comment ? html.indexOf('-->', open + 4) : html.indexOf('>', open);
        if (end < 0) break;
        at = end + (comment ? 3 : 1);
        const hidden = !comment && /^<(head|style|script|title)\b/i.exec(html.slice(open, at));
        if (hidden) {
            const closing = html.toLowerCase().indexOf(`</${hidden[1].toLowerCase()}`, at);
            const closed = closing < 0 ? -1 : html.indexOf('>', closing);
            at = closed < 0 ? html.length : closed + 1;
        }
    }
    return shown.replaceAll('&nbsp;', '\u00a0');
}

globalThis.DOMParser = class {
    parseFromString(html) {
        return { body: { textContent: shownText(html) } };
    }
};

let local = null;
Object.defineProperty(globalThis, 'navigator', {
    value: { clipboard: { read: async () => [local], readText: async () => '' } },
    configurable: true, writable: true });

/** A clipboard item of `flavours`, mime to string content. */
function item(flavours) {
    return {
        types: Object.keys(flavours),
        getType: async (type) => new Blob([flavours[type]], { type }),
    };
}

const { readLocalClipboard } = await import('../../addons/selkies-web-core/lib/clipboard-sync.js');

let failed = 0;
function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [clip-local-read] ${label}  ${detail}`);
}

const WORD_HTML = '<html><head><style>p.MsoNormal{margin:0}</style></head><body>'
    + '<!--StartFragment--><p class=MsoNormal><b>Quarterly</b> report&nbsp;draft</p><!--EndFragment--></body></html>';
const PICTURE = '\x89PNG\r\n\x1a\n';

{
    local = item({ 'text/html': WORD_HTML, 'text/plain': 'Quarterly report draft', 'image/png': PICTURE });
    const got = await readLocalClipboard(true);
    check('formatted text with a picture of itself beside it goes as the text',
          got && got.kind === 'flavours' && got.html === WORD_HTML && got.text === 'Quarterly report draft',
          JSON.stringify(got && { kind: got.kind, text: got.text }));
}
{
    local = item({ 'text/html': '<meta charset="utf-8"><img src="https://example.org/logo.png" alt="Logo">',
                   'image/png': PICTURE });
    const got = await readLocalClipboard(true);
    check('a copied picture, whose markup is an img tag, goes as the picture',
          got && got.kind === 'image' && got.mime === 'image/png', JSON.stringify(got && got.kind));
}
{
    local = item({ 'text/html': '<html><head><title>x</title></head><body><!--StartFragment-->'
                   + '<p>&nbsp;</p><img src="file:///C:/clip_image001.png"><!--EndFragment--></body></html>',
                   'image/png': PICTURE });
    const got = await readLocalClipboard(true);
    check('a picture copied out of a document, its markup only spaces around the img, goes as the picture',
          got && got.kind === 'image', JSON.stringify(got && got.kind));
}
{
    local = item({ 'image/png': PICTURE });
    const got = await readLocalClipboard(true);
    check('a picture alone goes as the picture', got && got.kind === 'image', JSON.stringify(got && got.kind));
}
{
    local = item({ 'text/html': '<b>bold</b> words', 'text/plain': 'bold words' });
    const got = await readLocalClipboard(true);
    check('formatted text alone goes as the text', got && got.kind === 'flavours' && got.text === 'bold words',
          JSON.stringify(got && got.kind));
}

console.log(`[clip-local-read] ${failed === 0 ? 'all checks passed' : failed + ' failed'}`);
process.exit(failed === 0 ? 0 : 1);
