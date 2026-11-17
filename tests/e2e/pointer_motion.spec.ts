/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// How far the remote pointer travels for a given movement of a real mouse.
//
// Locked motion reaches the client as movementX/Y, which browsers report in
// whole CSS pixels while carrying their own sub-pixel remainder. On a client
// whose scale is not a whole number that arrives as a stream of deltas that
// only add up over several events, so what the client does with each one
// decides whether a flick lands where the hand aimed. Motion here is injected
// with XTEST into an installed browser holding a real pointer lock, and the
// deltas it reports are fed through the client's own relative-motion path:
// what is measured is the engine, not a model of it.
//
// The lock is taken through the client's own request, so the engine is asked
// for raw (unadjusted) movement exactly where the client asks for it: that is
// what removes the local acceleration curve, and on the platforms where the
// client withholds the option this measures the accelerated deltas it really
// sends.
//
// The raw_pointer_motion setting is checked against the same engines: turned
// off it must keep the client from asking at all, and turned on it must ask,
// whatever the engine then answers.
import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import { cpSync, existsSync, lstatSync, mkdirSync, openSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { onPath } from '../lib/desktop.ts';
import { REPO, sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { serveTypeScript, type Served } from '../lib/static.ts';

const PAGE = '/tests/tools/pointer_motion_page.html';

// (label, motions, pixels each, seconds between): a slow careful aim, a
// normal drag, and a flick, all at one device pixel of travel per motion or more.
const PATTERNS: readonly (readonly [string, number, number, number])[] = [['slow', 40, 1, 0.025], ['steady', 120, 2, 0.006], ['flick', 60, 8, 0.002]];
// The browsers' windows need this much screen, and the click and the warps go to CLICK.
const SCREEN = [1920, 1080] as const;
const CLICK = [700, 450] as const;

type Report = Record<string, unknown>;

/** What the probe page last posted, swapped in whole, with a count of posts. */
class Reports {
    latest: Report = {};
    seq = 0;

    take(body: Buffer): void {
        try {
            const report: unknown = JSON.parse(body.toString());
            if (report !== null && typeof report === 'object') {
                this.seq++;
                this.latest = { ...report };
            }
        } catch {
            // a report that does not parse leaves the last one standing
        }
    }

    clear(): void {
        this.latest = {};
    }

    num(key: string): number {
        const v = this.latest[key];
        return typeof v === 'number' ? v : 0;
    }
}

const REPORTS = new Reports();
const WARMED = new Set<string>();
const LAUNCHES = new Map<string, number>();

async function waitFor(pred: () => boolean, timeoutS: number): Promise<boolean> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if (pred()) return true;
        await sleep(200);
    }
    return false;
}

/**
 * Wait until the page reports the same travel three times running. Warping
 * the pointer under a lock reaches the engine as one large delta, and a
 * baseline read before that delta is reported puts it in the measurement
 * instead. The page posts every 200 ms, so quiet is three identical posts.
 */
async function settle(timeoutS = 8): Promise<void> {
    let prev: string | null = null;
    let stable = 0;
    let lastSeq: number | null = null;
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if (REPORTS.seq !== lastSeq) {
            lastSeq = REPORTS.seq;
            const now = JSON.stringify([REPORTS.latest.moves ?? null, REPORTS.latest.travelX ?? null]);
            stable = now === prev ? stable + 1 : 0;
            prev = now;
            if (stable >= 2) return;
        }
        await sleep(50);
    }
}

/**
 * The installed browser to drive, or null when it is not on this machine.
 * The suite-wide overrides name a browser that is not on PATH, and the rest of
 * the e2e tier resolves them the same way.
 */
function binary(browser: string): string | null {
    const named = browser === 'chrome' ? process.env.E2E_CHROME : process.env.E2E_FIREFOX;
    if (named) return named;
    return onPath(browser === 'chrome' ? 'google-chrome' : 'firefox');
}

/**
 * Start an installed browser on the test display at a device pixel ratio. Its
 * output is kept: a browser that dies on startup has to be reported as that
 * and not as a page that never loaded.
 */
function launch(served: Served, browser: string, profile: string, dpr: number, query = ''): ChildProcess {
    const url = `${served.url}${PAGE}${query}`;
    let cmd: string[];
    if (browser === 'firefox') {
        writeFileSync(join(profile, 'user.js'), [
            `user_pref("layout.css.devPixelsPerPx", "${dpr}");`,
            'user_pref("browser.shell.checkDefaultBrowser", false);',
            // Nothing may open in front of the probe page: pointer lock needs
            // a click on it, and a welcome tab would take that click instead.
            'user_pref("browser.aboutwelcome.enabled", false);',
            'user_pref("browser.startup.firstrunSkipsHomepage", true);',
            'user_pref("browser.startup.homepage_override.mstone", "ignore");',
            'user_pref("datareporting.policy.firstRunURL", "");',
            // Firefox 156 opens its privacy notice or terms of use over a new
            // profile's first page, and that page takes the clicks.
            'user_pref("datareporting.policy.dataSubmissionPolicyBypassNotification", true);',
            'user_pref("termsofuse.bypassNotification", true);',
            '',
        ].join('\n'));
        cmd = [binary('firefox') ?? 'firefox', '--no-remote', '--profile', profile, '--width', '1600', '--height', '900', url];
    } else {
        // --no-sandbox as everywhere else in the tree: a container without
        // unprivileged user namespaces leaves Chrome only the setuid helper,
        // which it refuses to run under rather than drop the sandbox silently.
        cmd = [binary('chrome') ?? 'google-chrome', '--no-sandbox', '--no-first-run', '--no-default-browser-check', '--disable-gpu',
            `--user-data-dir=${profile}`, `--force-device-scale-factor=${dpr}`, '--window-position=0,0', '--window-size=1600,900', url];
    }
    const log = openSync(join(profile, 'browser.log'), 'w');
    return spawn(cmd[0], cmd.slice(1), { env: { ...process.env, DISPLAY: process.env.E2E_DISPLAY ?? '' }, stdio: ['ignore', log, log] });
}

const exited = (proc: ChildProcess): boolean => proc.exitCode !== null || proc.signalCode !== null;

/** What the browser was doing when the page failed to report: its state, the last line it logged, and whether its profile carries a lock. */
function whySilent(proc: ChildProcess, profile: string): string {
    let tail = '';
    try {
        const said = readFileSync(join(profile, 'browser.log'), 'utf8').split('\n').map((l) => l.trim()).filter(Boolean);
        tail = said.length ? said[said.length - 1].slice(0, 200) : '';
    } catch {
        // no log
    }
    let state = exited(proc) ? `exited ${proc.exitCode ?? proc.signalCode}` : 'still running';
    try {
        lstatSync(join(profile, 'lock'));
        state += ', profile locked';
    } catch {
        // unlocked
    }
    return tail ? `no report, ${state}: ${tail}` : `no report, ${state}`;
}

/**
 * A fresh profile for one launch of `browser`. Firefox's is a copy of a
 * profile warmed once, so a first run's onboarding never opens in the window
 * that has to hold pointer lock, and no launch inherits the lock or the
 * session of the one before it.
 */
function prepareProfile(browser: string): string {
    const n = LAUNCHES.get(browser) ?? 0;
    LAUNCHES.set(browser, n + 1);
    const profile = join(WORKDIR, `motion-profile-${browser}-${n}`);
    rmSync(profile, { recursive: true, force: true });
    if (browser === 'firefox') {
        const warm = join(WORKDIR, 'motion-profile-firefox-warm');
        if (!WARMED.has(browser)) {
            rmSync(warm, { recursive: true, force: true });
            mkdirSync(warm, { recursive: true });
            spawnSync(binary('firefox') ?? 'firefox', ['--headless', '--profile', warm, '--screenshot', '/dev/null', 'about:blank'], { timeout: 180_000 });
            WARMED.add(browser);
        }
        cpSync(warm, profile, { recursive: true, verbatimSymlinks: true });
        for (const name of ['lock', '.parentlock']) rmSync(join(profile, name), { force: true });
    } else {
        mkdirSync(profile, { recursive: true });
    }
    return profile;
}

/**
 * Launch `browser` on a fresh profile and wait for the probe page's first
 * report. A browser that exits before the page reports is launched once more
 * on another fresh profile, with the first attempt's state recorded as a
 * skipped check; a second silence is recorded as the failure.
 */
async function start(res: Checks, served: Served, browser: string, dpr: number, query: string, label: string): Promise<ChildProcess | null> {
    for (let attempt = 0; attempt < 2; attempt++) {
        const profile = prepareProfile(browser);
        REPORTS.clear();
        const proc = launch(served, browser, profile, dpr, query);
        // A browser that has already exited will not report, so the wait ends
        // with it rather than running out the clock three times over.
        if ((await waitFor(() => Object.keys(REPORTS.latest).length > 0 || exited(proc), 90)) && Object.keys(REPORTS.latest).length) return proc;
        const silence = whySilent(proc, profile);
        if (attempt === 0 && exited(proc)) {
            res.skip(`${label}: the browser exited before the page reported, relaunched`, silence);
            continue;
        }
        res.check(`${label}: the probe page loads`, false, silence);
        return null;
    }
    return null;
}

/** Click the loaded probe page until it holds pointer lock; a check is recorded on failure. */
async function lock(res: Checks, host: Host, label: string): Promise<boolean> {
    // A browser still opening its window swallows the first click; a machine
    // under load is not a product regression, so the click is repeated.
    for (let i = 0; i < 15; i++) {
        await host.warp(...CLICK);
        await sleep(500);
        await host.button(1, true);
        await sleep(50);
        await host.button(1, false);
        if (await waitFor(() => Boolean(REPORTS.latest.locked), 3)) break;
    }
    if (!REPORTS.latest.locked) {
        res.check(`${label}: the pointer locks`, false, REPORTS.latest.err ?? '');
        return false;
    }
    return true;
}

async function stopBrowser(proc: ChildProcess): Promise<void> {
    if (exited(proc)) return;
    const gone = new Promise<void>((resolve) => { proc.once('exit', () => { resolve(); }); });
    proc.kill('SIGTERM');
    const timer = setTimeout(() => proc.kill('SIGKILL'), 10_000);
    await gone;
    clearTimeout(timer);
}

/** Lock the pointer in `browser` and record the travel for each pattern. */
async function measure(res: Checks, host: Host, served: Served, browser: string, dpr: number): Promise<void> {
    const label = `${browser} at dpr ${dpr}`;
    const proc = await start(res, served, browser, dpr, '', label);
    if (proc === null) return;
    try {
        if (!(await lock(res, host, label))) return;
        // Raw movement is refused by every engine on Linux, so the fallback to
        // a plain lock is the path this runs on; a lock is what matters either way.
        const said = (k: string): string => {
            const v = REPORTS.latest[k];
            return typeof v === 'string' ? v : '';
        };
        res.check(`${label}: the pointer locks`, true, `${said('lockPath')}, motion from ${said('motionEvent')}`);
        for (const [name, count, delta, gap] of PATTERNS) {
            await host.warp(...CLICK);
            await settle();
            const mark = REPORTS.num('travelX');
            await host.motion(delta, 0, count, gap * 1000);
            await settle();
            const injected = count * delta;
            const traveled = REPORTS.num('travelX') - mark;
            // Tolerance: one event swallowed around the warp, or one percent of
            // sub-pixel carry caught mid-flight; a scale error is tens of percent.
            res.check(`${label}: a ${name} move of ${injected} px travels ${injected} px`, Math.abs(traveled - injected) <= Math.max(delta, Math.floor(injected / 100)), `${traveled}`);
        }
    } finally {
        await stopBrowser(proc);
    }
}

/**
 * Lock with raw_pointer_motion applied, and read back what the engine was
 * asked for. Off, the client must not ask for raw movement at all; on, it must
 * ask, and an engine that refuses (every one on Linux) then gets the plain
 * request that still ends in a lock.
 */
async function checkSetting(res: Checks, host: Host, served: Served, browser: string, raw: boolean): Promise<void> {
    const label = `${browser} with raw pointer motion ${raw ? 'on' : 'off'}`;
    const proc = await start(res, served, browser, 1, `?raw=${raw ? 1 : 0}`, label);
    if (proc === null) return;
    try {
        if (!(await lock(res, host, label))) return;
        const raw_ = REPORTS.latest.asked;
        const asked = Array.isArray(raw_) ? raw_.map(String) : [];
        if (raw) {
            res.check(`${label}: the first request asks for raw movement`, asked[0] === 'unadjusted', asked);
            res.check(`${label}: a refusal ends in a plain lock, not in no lock`, asked[asked.length - 1] === 'plain' || REPORTS.latest.lockPath === 'unadjusted', asked);
        } else {
            res.check(`${label}: the engine is never asked for raw movement`, asked.length === 1 && asked[0] === 'plain', asked);
        }
    } finally {
        await stopBrowser(proc);
    }
}

/** Whether the display is SCREEN in size and the pointer reaches CLICK, and why not. */
async function screenFits(host: Host): Promise<[boolean, string]> {
    const size = await host.rootSize();
    await host.warp(...CLICK);
    const at = await host.pointer();
    const fits = size !== null && size[0] >= SCREEN[0] && size[1] >= SCREEN[1] && at !== null && at[0] === CLICK[0] && at[1] === CLICK[1];
    let monitors = '';
    if (!fits) {
        const listed = spawnSync('xrandr', ['-display', process.env.E2E_DISPLAY ?? '', '--listmonitors'], { encoding: 'utf8', timeout: 10_000 }).stdout ?? '';
        monitors = listed.split('\n').slice(1).map((m) => m.split(/\s+/).filter(Boolean)).filter((m) => m.length > 2).map((m) => m[2]).join('; ');
    }
    return [fits, `root ${JSON.stringify(size)}, pointer at ${JSON.stringify(at)}; ${monitors}`];
}

/**
 * Give the shared test display its full size back, its output's mode with it.
 * No Selkies server sizes it here, and the suite before this one may have left
 * it at its page's size (frame_ancestors' framed page leaves 304x150), or the
 * screen full size under an output still in that mode, which holds the pointer
 * inside it: either way the click and the warps miss. Selkies' own resize sets
 * the output's mode, making one where it has none.
 */
async function sizeScreen(host: Host): Promise<void> {
    if ((await screenFits(host))[0]) return;
    spawnSync(await host.serverBinary(), ['resize', `${SCREEN[0]}x${SCREEN[1]}`], { env: { ...process.env, DISPLAY: process.env.E2E_DISPLAY ?? '' }, timeout: 30_000 });
}

test('pointer_motion', async () => {
    const res = new Checks('pointer-motion');
    const host = new Host(suiteOf(import.meta.filename));
    const browsers = ['chrome', 'firefox'].filter((b) => binary(b) !== null && (binary(b)?.includes('/') ? existsSync(binary(b) ?? '') : true));
    let served: Served | null = null;
    try {
        if (!browsers.length) {
            res.skip('pointer-motion', 'neither Chrome nor Firefox is installed');
        } else {
            await sizeScreen(host);
            const [fits, why] = await screenFits(host);
            res.check('the test display is the size the suite needs and the pointer reaches its click', fits, why);
            served = await serveTypeScript(REPO, (path, body) => { if (path === '/report') REPORTS.take(body); });
            for (const browser of browsers) {
                for (const dpr of [1, 1.25, 1.5]) await measure(res, host, served, browser, dpr);
                for (const raw of [false, true]) await checkSetting(res, host, served, browser, raw);
            }
        }
    } finally {
        await served?.close();
        await host.close();
    }
    res.finish();
});
