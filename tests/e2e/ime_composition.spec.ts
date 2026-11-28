/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// IME client paths: replayed composition traces must type the same text the
// IME committed locally, on every platform branch.
//
// Two event anatomies are replayed as DOM events against the real input.ts in
// Chromium (platform-spoofed via CDP where a branch needs it):
//
// - Real IBus (Linux Chrome): the commit arrives as textInput AFTER an
//   empty-data compositionend. A handler that only listens for textInput on
//   Linux erases every committed syllable a Chrome client sends from
//   Windows/macOS -- the "keeps replacing one symbol" report.
// - CDP-style (insertText API): textInput and compositionend BOTH carry the
//   commit. The end must see the stamp and clear the preedit instead of
//   typing the text a second time.
//
// Each preedit stage goes out as it is typed, and a commit of several CJK
// characters goes as one text commit, which the server binds in one keymap
// change; a commit the last update already streamed goes out once.
//
// A dashboard soft modifier is then held over the two soft-keyboard
// anatomies: Android Chromium reports a tap as keyCode 229 with no key or code
// and the letter only as the assist input's text, iOS names the key on the
// keydown and echoes it as text. Both must reach the wire as the one key under
// the held Control, neither dropped nor typed twice.
//
// Finally the two soft-keyboard anatomies of the assist field itself, captured
// from an iPad and an Android phone: iOS keeps the Pinyin preedit in the field
// as composition text (syllables set apart by U+2006), shortens it on
// Backspace, replaces it with the picked candidate, and confirms as a
// deleteCompositionText followed by an insertFromComposition of the same
// text; Android commits each letter as plain insertText with no composition at
// all. The wire must carry the preedit as a rolling diff and the commit
// exactly once.
//
// The page is tools/ime_client_page.html.
import type { Browser, Page } from '@playwright/test';
import { chromium, test, webkit } from '@playwright/test';

import { launchBrowser } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { REPO, sleep } from '../lib/env.ts';
import { serveTypeScript } from '../lib/static.ts';
import { within } from '../lib/stream.ts';

const PAGE = '/tests/tools/ime_client_page.html';

type Step = readonly (string | null)[];
type Group = Step[];

// Captured on Linux Chrome with a live ibus-hangul engine: the preedit rolls
// forward, cancels to an empty update, compositionend arrives data-less, and
// the commit rides the textInput after it.
const IBUS_SYLLABLE: readonly (readonly [string, string])[] = [
    ['compositionupdate', 'PRE1'],
    ['compositionupdate', 'FULL'],
    ['compositionupdate', ''],
    ['textInput', ''],
    ['compositionend', ''],
    ['textInput', 'FULL'],
];
// CDP-driven anatomy: the commit is in textInput and in the compositionend data.
const CDP_SYLLABLE: readonly (readonly [string, string])[] = [
    ['compositionupdate', 'PRE1'],
    ['compositionupdate', 'FULL'],
    ['textInput', 'FULL'],
    ['compositionend', 'FULL'],
];

const groups = (template: readonly (readonly [string, string])[], syllables: readonly (readonly [string, string])[]): Group[] =>
    syllables.map(([p, f]) => template.map(([kind, data]) => [kind, data.replace('PRE1', p).replace('FULL', f)]));

const IBUS_GROUPS = groups(IBUS_SYLLABLE, [['ㅎ', '한'], ['ㄱ', '글'], ['ㅇ', '안'], ['ㄴ', '녕']]);
const CDP_GROUPS = groups(CDP_SYLLABLE, [['ㅎ', '한'], ['ㄱ', '글']]);
const IBUS_EXPECT = '한글안녕';
const IBUS_PROBES = ['한', '한글', '한글안', '한글안녕'];
const IBUS_PROBE_IDX = 1;
const CDP_EXPECT = '한글';

const WIN_UA = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36';
const MAC_UA = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36';

const SOFT_CHORD_EXPECT = ['kd,65507', 'kd,99', 'ku,99', 'ku,65507'];

const SEP = '\u2006';
// iPad, Chinese Pinyin keyboard: d d d, Backspace, candidate 大的.
const IOS_ASSIST: Step[] = [
    ['keydown', 'd'],
    ['compositionstart', ''],
    ['compositionupdate', 'd'],
    ['input', 'd', 'd', 'insertCompositionText'],
    ['keyup', 'd'],
    ['keydown', 'd'],
    ['compositionupdate', `d${SEP}d`],
    ['input', `d${SEP}d`, `d${SEP}d`, 'insertCompositionText'],
    ['keyup', 'd'],
    ['keydown', 'd'],
    ['compositionupdate', ['d', 'd', 'd'].join(SEP)],
    ['input', ['d', 'd', 'd'].join(SEP), ['d', 'd', 'd'].join(SEP), 'insertCompositionText'],
    ['keyup', 'd'],
    ['keydown', 'Backspace'],
    ['compositionupdate', `d${SEP}d`],
    ['input', `d${SEP}d`, `d${SEP}d`, 'insertCompositionText'],
    ['keyup', 'Backspace'],
    ['compositionupdate', '大的'],
    ['input', '大的', '大的', 'insertCompositionText'],
    ['input', null, '', 'deleteCompositionText'],
    ['textInput', '大的'],
    ['input', '大的', '大的', 'insertFromComposition'],
    ['compositionend', '大的'],
];
const IOS_ASSIST_EXPECT = '大的';
// Android, Gboard Chinese keyboard: the picked word arrives as plain text,
// and an English keyboard commits letter by letter the same way.
const ANDROID_ASSIST: Step[] = [
    ['keydown', 'Unidentified'],
    ['textInput', '得到的'],
    ['input', '得到的', '得到的', 'insertText'],
    ['keyup', 'Unidentified'],
    ['keydown', 'Unidentified'],
    ['textInput', ' '],
    ['input', ' ', ' ', 'insertText'],
    ['keyup', 'Unidentified'],
    ['keydown', 'Unidentified'],
    ['textInput', 'x'],
    ['input', 'x', 'x', 'insertText'],
    ['keyup', 'Unidentified'],
];
const ANDROID_ASSIST_EXPECT = '得到的 x';

interface Replay {
    name: string;
    groups: Group[];
    expect: string;
    mid?: string[];
}

const IBUS: Replay = { name: 'ibus anatomy', groups: IBUS_GROUPS, expect: IBUS_EXPECT, mid: IBUS_PROBES };
const CDP: Replay = { name: 'cdp anatomy', groups: CDP_GROUPS, expect: CDP_EXPECT };

const js = (page: Page, code: string): Promise<unknown> => within(page.evaluate(code));
const call = (page: Page, fn: string, ...args: unknown[]): Promise<unknown> => js(page, `window.${fn}(${args.map((a) => JSON.stringify(a)).join(', ')})`);
const strings = (v: unknown): string[] => (Array.isArray(v) ? v.filter((m): m is string => typeof m === 'string') : []);
const same = (a: unknown, b: unknown): boolean => JSON.stringify(a) === JSON.stringify(b);
const r = (v: unknown): string => JSON.stringify(v);

async function ready(page: Page, url: string): Promise<void> {
    await page.goto(`${url}${PAGE}`);
    await page.waitForFunction('window.__ready === true || window.__errs.length > 0', undefined, { timeout: 15_000 });
}

async function runBranch(res: Checks, url: string, tag: string, platform: string | null, ua: string | null, replays: readonly Replay[], engine: 'chromium' | 'firefox' | 'webkit' = 'chromium'): Promise<boolean> {
    const browser: Browser = engine === 'firefox' ? await launchBrowser('firefox') : await (engine === 'webkit' ? webkit : chromium).launch({ headless: true });
    try {
        const context = await browser.newContext(ua ? { userAgent: ua } : {});
        const page = await context.newPage();
        if (platform && engine === 'chromium') {
            const cdp = await context.newCDPSession(page);
            await cdp.send('Emulation.setUserAgentOverride', { userAgent: ua ?? '', platform });
        }
        await ready(page, url);
        const errs = strings(await js(page, 'window.__errs'));
        res.check(`${tag}: input.ts constructs`, errs.length === 0, errs);
        if (errs.length) return false;
        let ok = true;
        for (const replay of replays) {
            await js(page, 'window.__sent = []');
            let midOk = true;
            if (replay.mid) {
                await call(page, '__replayWithProbe', replay.groups, IBUS_PROBE_IDX);
                await sleep(400);
                const gotMid = await js(page, 'window.__mid');
                midOk = same(gotMid, replay.mid);
                res.check(`${tag}: preedit text rolls forward mid-syllable`, midOk, `mid=${r(gotMid)} expect=${r(replay.mid)}`);
            } else {
                await call(page, '__replay', replay.groups);
                await sleep(400);
            }
            const got = await js(page, 'window.__decodeWire()');
            const one = got === replay.expect;
            ok = ok && one && midOk;
            res.check(`${tag}: ${replay.name} mirrors local text remotely`, one, `remote=${r(got)} expect=${r(replay.expect)}`);
        }
        await js(page, 'window.__sent = []');
        await js(page, "window.__dispatch('compositionstart', ''); window.__dispatch('compositionupdate', '한');");
        const during = await js(page, 'window.__decodeWire()');
        await js(page, "window.__dispatch('compositionend', '')");
        const shown = during === '한';
        ok = ok && shown;
        res.check(`${tag}: a preedit stage shows as it is typed`, shown, `during=${r(during)}`);
        await js(page, 'window.__sent = []');
        // The commit rides the end, or the textInput after a clear-only end where the engine commits there.
        await js(page, `(() => {
            window.__dispatch('compositionstart', ''); window.__dispatch('compositionupdate', 'nihao');
            window.__dispatch('compositionend', '你好');
            if (!window.__sent.some((m) => m.startsWith('co,'))) {
                try { window.__dispatch('textInput', '你好'); } catch (e) { /* no TextEvent */ }
            }
        })()`);
        await sleep(350);
        const commits = strings(await js(page, 'window.__sent')).filter((m) => m.startsWith('co,'));
        const whole = same(commits, ['co,end,你好']);
        ok = ok && whole;
        res.check(`${tag}: a commit of several CJK characters goes as one text commit`, whole, `commits=${r(commits)}`);
        await js(page, 'window.__sent = []');
        // Blink's order for a commit (CDP's insertText too): the last update carries it, then textInput and the end.
        await js(page, `(() => {
            window.__dispatch('compositionstart', ''); window.__dispatch('compositionupdate', 'nihao');
            window.__dispatch('compositionupdate', '你好');
            try { window.__dispatch('textInput', '你好'); } catch (e) { /* no TextEvent */ }
            window.__dispatch('compositionend', '你好');
        })()`);
        await sleep(350);
        const sent = strings(await js(page, 'window.__sent'));
        const once = same(sent.filter((m) => m.startsWith('co,')), ['co,end,你好'])
            && sent.filter((m) => m === 'kd,65288').length === 5 && (await js(page, 'window.__decodeWire()')) === '你好';
        ok = ok && once;
        res.check(`${tag}: a commit the last update streamed is not typed again`, once, `sent=${r(sent.filter((m) => !m.startsWith('ku,')))}`);
        return ok;
    } finally {
        await browser.close();
    }
}

/**
 * The assist field's iOS and Android anatomies mirror the local text once,
 * and the field empties after. Run in WebKit, the only engine whose
 * InputEvent carries iOS's `deleteCompositionText` and
 * `insertFromComposition` names, and in Chromium, which blanks them, so the
 * handler is shown not to depend on either.
 */
async function runAssist(res: Checks, url: string, engine: 'chromium' | 'webkit'): Promise<void> {
    const browser = await (engine === 'webkit' ? webkit : chromium).launch({ headless: true });
    try {
        const page = await browser.newPage();
        await ready(page, url);
        for (const [name, steps, expect] of [['ios pinyin', IOS_ASSIST, IOS_ASSIST_EXPECT], ['android', ANDROID_ASSIST, ANDROID_ASSIST_EXPECT]] as const) {
            await js(page, 'window.__sent = []');
            await call(page, '__assistReplay', steps);
            await sleep(300);
            const got = await js(page, 'window.__decodeWire()');
            res.check(`${engine}: assist ${name} mirrors local text remotely`, got === expect, `remote=${r(got)} expect=${r(expect)}`);
            const sent = strings(await js(page, 'window.__sent'));
            const commits = sent.filter((m) => m === `kd,${0x01000000 + (expect.codePointAt(0) ?? 0)}`).length;
            res.check(`${engine}: assist ${name} types the commit once, not erased and retyped`, commits === 1, `presses=${commits} wire=${r(sent)}`);
            const value = await js(page, 'window.__assistValue()');
            res.check(`${engine}: assist ${name} leaves the field empty`, value === '', r(value));
        }
    } finally {
        await browser.close();
    }
}

/** A soft Control over an Android and an iOS soft-keyboard tap, on the wire as Ctrl+c once. */
async function runSoftChord(res: Checks, url: string): Promise<void> {
    const browser = await chromium.launch({ headless: true });
    try {
        const page = await browser.newPage();
        await ready(page, url);
        for (const platform of ['android', 'ios']) {
            await js(page, 'window.__sent = []');
            const got = await call(page, '__softChord', platform);
            res.check(`soft Control over a ${platform} keyboard tap sends Ctrl+c once`, same(got, SOFT_CHORD_EXPECT), `wire=${r(got)}`);
        }
    } finally {
        await browser.close();
    }
}

/**
 * The host IME places its candidates at the composition's caret, so a click on
 * the stream moves the overlay's caret there (selkies#667): an IME composition
 * started after a click at (900, 500) on a 1280x720 overlay laid out as the
 * client lays it out has its caret at the click, not at the overlay's left
 * edge. Read with caretPositionFromPoint, which Chromium answers inside a text
 * control.
 */
async function runAnchor(res: Checks, url: string): Promise<void> {
    const browser = await chromium.launch({ headless: true });
    try {
        const page = await browser.newPage({ viewport: { width: 1280, height: 720 } });
        await ready(page, url);
        await page.addStyleTag({ content: 'body { margin: 0 } #overlayInput { position: absolute; top: 0; left: 0; width: 1280px; height: 720px; padding: 0; margin: 0; border: none; opacity: 0 }' });
        await js(page, 'window.__input.attach_context()');
        const caretAt = async (x: number, y: number): Promise<number[] | null> => {
            const rect = await js(page, `(() => {
                const p = document.caretPositionFromPoint(${x}, ${y});
                const r = p && p.offsetNode && p.offsetNode.id === 'overlayInput' ? p.getClientRect() : null;
                return r ? [Math.round(r.left), Math.round(r.top), Math.round(r.bottom)] : null;
            })()`);
            return Array.isArray(rect) ? rect.filter((n): n is number => typeof n === 'number') : null;
        };
        for (const [x, y] of [[900, 500], [200, 120]] as const) {
            await page.mouse.click(x, y);
            const cdp = await page.context().newCDPSession(page);
            await cdp.send('Input.imeSetComposition', { text: '漢字', selectionStart: 2, selectionEnd: 2 });
            await sleep(100);
            const caret = await caretAt(x, y);
            res.check(`a composition after a click at (${x}, ${y}) starts its caret there`,
                      caret !== null && Math.abs(caret[0] - x) <= 2 && caret[1] <= y && y <= caret[2], `caret=${r(caret)}`);
            await cdp.send('Input.insertText', { text: '' });
            await js(page, "document.getElementById('overlayInput').value = ''");
            await cdp.detach();
        }
    } finally {
        await browser.close();
    }
}

test('ime_composition', async () => {
    const res = new Checks('ime-client');
    const served = await serveTypeScript(REPO);
    try {
        const { url } = served;
        await runSoftChord(res, url);
        await runAnchor(res, url);
        await runAssist(res, url, 'webkit');
        await runAssist(res, url, 'chromium');
        await runBranch(res, url, 'linux-ibus', null, null, [IBUS]);
        await runBranch(res, url, 'win-ibus', 'Windows', WIN_UA, [IBUS]);
        await runBranch(res, url, 'mac-ibus', 'MacIntel', MAC_UA, [IBUS]);
        await runBranch(res, url, 'linux-cdp', null, null, [CDP, { ...CDP, name: 'two cdp syllables stay single' }]);
        await runBranch(res, url, 'win-cdp', 'Windows', WIN_UA, [CDP]);
        // WebKit (the Safari stand-in) commits a composition as textInput
        // before compositionend, the cdp anatomy; the ibus anatomy is the
        // reversed order a platform IME may deliver. Real macOS/iOS Safari
        // stays uncovered.
        await runBranch(res, url, 'webkit-textinput-first', null, null, [CDP], 'webkit');
        await runBranch(res, url, 'webkit-textinput-after', null, null, [IBUS], 'webkit');
        // Older Gecko builds dispatch no textInput at all; the handler asks
        // the engine which event carries the commit, so both anatomies must
        // land here.
        await runBranch(res, url, 'firefox-ibus', null, null, [IBUS], 'firefox');
        await runBranch(res, url, 'firefox-cdp', null, null, [CDP], 'firefox');
    } finally {
        await served.close();
    }
    res.finish();
});
