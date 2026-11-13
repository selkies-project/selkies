/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Which local-to-server clipboard send wins. Choosing a file for the image
 * upload blurs the page and refocuses it, and the focus read that refocus
 * fires reads the clipboard the user had before, so without precedence the
 * upload lands on the session clipboard and the old value lands on top of it.
 * A push the user asked for therefore outranks a read while it runs and
 * briefly after, and a clipboard the page has already seen is never sent
 * again over what the session took since: only a copy made later reaches the
 * session.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */

// Set before the module loads: it reads neither at import, but the sender
// closes over them per call and a missing global would read as "no clipboard".
import type { ClipboardSync, LocalClipboardSender } from '../../web/selkies-web-core/lib/clipboard-sync.ts';
import type { StandIn } from './stand_in.ts';

/** A clipboard item: its types and the blob of each. */
interface FakeItem {
    types: string[];
    getType: (type: string) => Promise<Blob>;
}

/** The navigator members the sender reads, which a check replaces. */
interface FakeNavigator {
    clipboard: { readText: () => Promise<string>; read?: () => Promise<FakeItem[]> };
    permissions?: { query: () => Promise<{ state: PermissionState }> };
}

/** What a transport was asked to send. */
interface Sent {
    data?: string | ArrayBuffer;
    mime?: string;
}

Object.assign(globalThis, { window: { isSecureContext: true } satisfies StandIn<Window> });
const clipboard = { text: '', read: null };
const browser: FakeNavigator = { clipboard: { readText: () => Promise.resolve(clipboard.text) } };
Object.defineProperty(globalThis, 'navigator', {
    configurable: true,
    value: browser satisfies StandIn<Navigator>,
});

const { createClipboardSync, createLocalClipboardSender } = await import(
    '../../web/selkies-web-core/lib/clipboard-sync.ts');

// The precedence window is a local in the module; pinned here so the audit
// fails when it drifts, and bound to the behavior by the checks below.
const EXPLICIT_PRECEDENCE_MS = 1000;

let failed = 0;

/** Prints one check's line and counts a failure. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [clip-precedence] ${label}  ${String(detail)}`);
}

const realNow = Date.now;
let clock = 1_000_000;
Date.now = () => clock;

/** A sender whose transport records what it was asked to send. */
function sender({ hold = null }: { hold?: ReturnType<typeof gate> | null } = {}):
    { sender: LocalClipboardSender; sent: Sent[]; clipboardSync: ClipboardSync } {
    const sent: Sent[] = [];
    const clipboardSync = createClipboardSync({ sendRequest: () => {} });
    const s = createLocalClipboardSender({
        isChromium: true,
        isSharedMode: () => false,
        canSync: () => true,
        canRead: () => true,
        binaryEnabled: () => false,
        clipboardSync,
        sendClipboardData: async (data, mime) => {
            sent.push({ data, mime });
            if (hold) await hold.promise;
            clipboardSync.markSynced(data, mime);
        },
    });
    return { sender: s, sent, clipboardSync };
}

/** A promise and the call that settles it. */
function gate(): { promise: Promise<void>; release: () => void } {
    let release!: () => void;
    const promise = new Promise<void>((r) => { release = r; });
    return { promise, release };
}

clipboard.text = 'what the user copied before';

// Control: nothing explicit in flight, so the focus read is the sync it exists to be.
{
    const { sender: s, sent } = sender();
    await s.readAndSend();
    check('a focus read sends the local clipboard when nothing else has',
          sent.length === 1 && sent[0].data === clipboard.text, JSON.stringify(sent));
}

// The upload is still on the wire when the refocus fires.
{
    const held = gate();
    const { sender: s, sent } = sender({ hold: held });
    const push = s.sendExplicit('the uploaded image', 'image/png');
    const read = s.readAndSend();
    held.release();
    await Promise.all([push, read]);
    check('a read racing a push in flight sends nothing',
          sent.length === 1 && sent[0].mime === 'image/png', JSON.stringify(sent));
}

// The upload has landed; the refocus arrives a moment later.
{
    const { sender: s, sent } = sender();
    await s.sendExplicit('the uploaded image', 'image/png');
    clock += EXPLICIT_PRECEDENCE_MS - 1;
    await s.readAndSend();
    check('a read just after a push sends nothing',
          sent.length === 1 && sent[0].mime === 'image/png', JSON.stringify(sent));
}

// No read saw the clipboard between the upload and the end of the window (an
// engine that reads only on paste): what it holds then predates the upload.
{
    const { sender: s, sent } = sender();
    await s.sendExplicit('the uploaded image', 'image/png');
    clock += EXPLICIT_PRECEDENCE_MS + 1;
    await s.readAndSend();
    check('the first read after the window finds the clipboard the upload replaced, not a copy',
          sent.length === 1, JSON.stringify(sent));
}

// The picker hands over a File, and the bytes arrive a task later; the push
// has to be on record from the call or the refocus overtakes it.
{
    const held = gate();
    const { sender: s, sent } = sender();
    // Text stands for the bytes: the transport takes either, and the check's line prints it.
    const bytes = 'image bytes' as string | ArrayBuffer as ArrayBuffer;
    const blob = { arrayBuffer: async () => { await held.promise; return bytes; } };
    const push = s.sendExplicit(blob as StandIn<Blob> as Blob, 'image/png');
    const read = s.readAndSend();
    held.release();
    await Promise.all([push, read]);
    check('a push whose bytes are still being read outranks a read',
          sent.length === 1 && sent[0].data === 'image bytes', JSON.stringify(sent));
}

// The refocus read found the clipboard the user had before the upload; a
// later trip to another window and back finds it unchanged.
{
    const { sender: s, sent } = sender();
    await s.sendExplicit('the uploaded image', 'image/png');
    await s.readAndSend();
    clock += EXPLICIT_PRECEDENCE_MS + 1;
    await s.readAndSend();
    check('an upload outlives later focus reads of the clipboard the user already had',
          sent.length === 1 && sent[0].mime === 'image/png', JSON.stringify(sent));
    clipboard.text = 'copied after the upload';
    await s.readAndSend();
    check('a copy made after the upload still reaches the session',
          sent.length === 2 && sent[1].data === 'copied after the upload', JSON.stringify(sent));
    clipboard.text = 'what the user copied before';
}

// The clipboard was seen before the push (a paste sent it), and the user
// copies something else after it: the first read after the push is that copy.
{
    const { sender: s, sent } = sender();
    await s.readAndSend();
    await s.sendExplicit('typed into the clipboard box', 'text/plain');
    clock += EXPLICIT_PRECEDENCE_MS + 1;
    await s.readAndSend();
    check('the clipboard seen before a push is not sent over it',
          sent.length === 2 && sent[1].data === 'typed into the clipboard box', JSON.stringify(sent));
    await s.sendExplicit('typed into the clipboard box again', 'text/plain');
    clipboard.text = 'copied after the box';
    clock += EXPLICIT_PRECEDENCE_MS + 1;
    await s.readAndSend();
    check('the first copy made after a push reaches the session',
          sent.length === 4 && sent[3].data === 'copied after the box', JSON.stringify(sent));
    clipboard.text = 'what the user copied before';
}

// The session took a newer value whose local write never landed (refused,
// withdrawn): the local clipboard is unchanged and must not go back over it.
{
    const { sender: s, sent, clipboardSync } = sender();
    await s.readAndSend();
    clipboardSync.resolveServer('copied in the session');
    await s.readAndSend();
    check('an unchanged local clipboard is not sent over a newer session value',
          sent.length === 1, JSON.stringify(sent));
}

// A push that never left (no connection yet, the socket reconnecting after the
// tab came back) is no send: the content is still news to the session, sent at
// the next read and read again once the next connection's settings arrive. A
// push the server's policy declined is not chased.
/** A sender whose transport declines with `codes`, one per call, then sends. */
function declining(codes: Array<string | undefined>): { sender: LocalClipboardSender; sent: Sent[] } {
    const sent: Sent[] = [];
    const clipboardSync = createClipboardSync({ sendRequest: () => {} });
    const s = createLocalClipboardSender({
        isChromium: true,
        isSharedMode: () => false,
        canSync: () => true,
        canRead: () => true,
        binaryEnabled: () => false,
        clipboardSync,
        sendClipboardData: (data, mime, onSkip) => {
            const code = codes.shift();
            if (code) { if (onSkip) onSkip('declined', code); return Promise.resolve(); }
            sent.push({ data, mime });
            clipboardSync.markSynced(data, mime);
            return Promise.resolve();
        },
    });
    return { sender: s, sent };
}
Object.assign(globalThis, { document: { hasFocus: () => true } satisfies StandIn<Document> });
browser.permissions = { query: () => Promise.resolve({ state: 'granted' }) };
const settle = (): Promise<void> => new Promise((r) => setTimeout(r, 0));
{
    const { sender: s, sent } = declining(['clipboardSkipNotConnected']);
    await s.readAndSend();
    await s.readAndSend();
    check('a read whose push found no connection sends at the next read',
          sent.length === 1 && sent[0].data === clipboard.text, JSON.stringify(sent));
}
{
    const { sender: s, sent } = declining([undefined, 'clipboardSkipNotConnected']);
    await s.maybeInitial();
    await settle();
    clipboard.text = 'copied while the tab was away';
    await s.readAndSend();
    await s.maybeInitial();
    await settle();
    check('the next connection reads again what a dropped push missed',
          sent.length === 2 && sent[1].data === 'copied while the tab was away', JSON.stringify(sent));
    await s.maybeInitial();
    await settle();
    check('and only once', sent.length === 2, JSON.stringify(sent));
    clipboard.text = 'what the user copied before';
}
{
    const { sender: s, sent } = declining(['clipboardSkipSendFailed']);
    await s.readAndSend();
    await s.readAndSend();
    check('a push the connection dropped mid-send sends at the next read',
          sent.length === 1, JSON.stringify(sent));
}
{
    const { sender: s, sent } = declining(['clipboardSkipInDisabled']);
    await s.readAndSend();
    await s.maybeInitial();
    await settle();
    await s.readAndSend();
    check('a push the policy declined is not chased', sent.length === 0, JSON.stringify(sent));
}

// The session's own copy read back in the browser's form is no copy. Chromium on Windows keeps
// plain text with CRLF line ends, and markup comes back rewrapped; sent, either would take the
// session's clipboard from its owner (a file manager's cut becomes text, and its paste offers to
// save the text as a file).
{
    const { sender: s, sent, clipboardSync } = sender();
    const paths = 'file:///home/u/report.odt\nfile:///home/u/notes.txt';
    clipboardSync.resolveServer(paths);
    clipboardSync.noteLocal(clipboardSync.sig(paths));
    clipboard.text = paths.replace(/\n/g, '\r\n');
    await s.readAndSend();
    check("the session's text read back with CRLF line ends is not sent", sent.length === 0, JSON.stringify(sent));
    clipboard.text = 'file:///home/u/other.txt\r\nfile:///home/u/more.txt';
    await s.readAndSend();
    check('a different copy with CRLF line ends is', sent.length === 1, JSON.stringify(sent));
    clipboard.text = 'what the user copied before';
}
{
    const sent: Sent[] = [];
    const clipboardSync = createClipboardSync({ sendRequest: () => {} });
    const s = createLocalClipboardSender({
        isChromium: true, isSharedMode: () => false, canSync: () => true, canRead: () => true,
        binaryEnabled: () => true, clipboardSync,
        sendClipboardData: (data, mime) => { sent.push({ mime }); clipboardSync.markSynced(data, mime); return Promise.resolve(); },
    });
    const item = (html: string, text: string): FakeItem[] => [{
        types: ['text/html', 'text/plain'],
        getType: (t) => Promise.resolve(new Blob([t === 'text/html' ? html : text], { type: t })),
    }];
    const savedRead = browser.clipboard.read;
    browser.clipboard.read = () => Promise.resolve(item("<meta charset='utf-8'><p>from <b>the session</b></p>", 'from the session'));
    // As the landing of a formatted copy leaves it: the server's text synced, the local form unknown.
    clipboardSync.resolveServer('from the session', null, 'application/x-selkies-clipboard-flavours', { __clipDigest: true, byteLength: 40, hash: 7 });
    clipboardSync.noteLocal(null);
    await s.readAndSend();
    check("the session's formatted copy read back rewrapped is not sent", sent.length === 0, JSON.stringify(sent));
    browser.clipboard.read = () => Promise.resolve(item('<p>copied in the browser</p>', 'copied in the browser'));
    await s.readAndSend();
    check('a formatted copy made in the browser is', sent.length === 1, JSON.stringify(sent));
    browser.clipboard.read = savedRead;
}
{
    // The session sent A, this page then sent B: copying A again in the browser is a copy.
    const { sender: s, sent, clipboardSync } = sender();
    clipboardSync.resolveServer('value A');
    clipboardSync.noteLocal(clipboardSync.sig('value A'));
    clipboard.text = 'value B';
    await s.readAndSend();
    clipboard.text = 'value A';
    await s.readAndSend();
    check('copying again what the session sent before a newer value is sent',
          sent.length === 2 && sent[1].data === 'value A', JSON.stringify(sent));
    clipboard.text = 'what the user copied before';
}

Date.now = realNow;
console.log(`[clip-precedence] ${failed === 0 ? 'all checks passed' : failed + ' failed'}`);
process.exit(failed === 0 ? 0 : 1);
