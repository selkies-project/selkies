/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The apps panel while one of its jobs is running, and what it knows after.
//
// Driven over both transports: the jobs and the installed set ride the
// apps_job and apps_state system actions, which each transport carries its own
// way.
//
// An install is a job the server runs on the session for a minute or more,
// and a panel that goes back to looking idle the moment it is clicked reads as
// one that dropped the click. So the button that started it says so while it
// runs and holds the row, and what the runner prints streams into the panel's
// console as it prints it, without the status lines the runner adds for the
// server. The console stays when the job ends, however quickly, until the user
// clears it. What ends up installed is read from the session, so a page that
// kept no record of the install is still told about it.
//
// A session on a proot-apps local repository installs only what that folder
// holds, so the panel lists the folder's own catalog, served by the server,
// and never reaches for the remote one.
//
// Driven against a stand-in selkies-proot -- the runner whose presence
// publishes the panel -- and a stand-in catalog, so nothing here reaches the
// network.
import { chmodSync, existsSync, mkdirSync, rmSync, unlinkSync, writeFileSync } from 'node:fs';
import { delimiter, join } from 'node:path';

import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { openAppsModal } from '../lib/dashboard.ts';
import { DASHBOARD_DIST, sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host } from '../lib/host.ts';

const APP = 'e2eapp';
// The catalog shape the panel reads: one installable entry, its icon left to
// fail (the panel hides a broken one rather than waiting for it).
const CATALOG = `include:
  - name: ${APP}
    full_name: E2E App
    description: A stand-in catalog entry
    icon: ${APP}.png
`;
// Long enough that the panel is observed mid-command, short enough to wait out.
const INSTALL_SECONDS = 6;
const LOCAL_APP = 'localapp';
const LOCAL_CATALOG = `include:
  - name: ${LOCAL_APP}
    full_name: Local App
    description: From the local repository
    icon: ${LOCAL_APP}.png
`;
// A 1x1 PNG, so a rendered icon has a size to measure.
const LOCAL_ICON = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==', 'base64');
// The stand-in answers list from a file it writes on install, the way the
// real wrapper answers it from the runner's install directory: what is
// installed is the session's fact, and the panel has to be able to ask for it.
const runner = (state: string): string => `#!/bin/bash
state="${state}"
case "$1" in
  check) exit 0 ;;
  list) [ -f "$state" ] && cat "$state"; exit 0 ;;
  install)
    echo "Installing $2"
    echo "##pa phase download"
    for p in 10 40 70 100; do printf '\\r####  %s.0%%' "$p"; sleep ${INSTALL_SECONDS / 4}; done
    printf '\\n'
    echo "##pa phase install"
    echo "Adding icon"
    echo "$2" >> "$state"; exit 0 ;;
  remove) echo "Removing $2"; if [ -f "$state" ]; then grep -vx "$2" "$state" > "$state.new" || true; mv "$state.new" "$state"; fi; exit 0 ;;
  *) exit 0 ;;
esac
`;

/** Write the selkies-proot stand-in; the directory to prepend to PATH. */
function runnerStub(directory: string): string {
    mkdirSync(directory, { recursive: true });
    const state = join(directory, 'installed');
    if (existsSync(state)) unlinkSync(state);
    const path = join(directory, 'selkies-proot');
    writeFileSync(path, runner(state));
    chmodSync(path, 0o755);
    return directory;
}

/** Lay out a proot-apps local repository holding one application and its catalog. */
function localRepository(folder: string): string {
    rmSync(folder, { recursive: true, force: true });
    mkdirSync(join(folder, `ghcr.io_linuxserver_proot-apps_${LOCAL_APP}`), { recursive: true });
    mkdirSync(join(folder, 'metadata', 'img'), { recursive: true });
    writeFileSync(join(folder, 'metadata', 'metadata.yml'), LOCAL_CATALOG);
    writeFileSync(join(folder, 'metadata', 'img', `${LOCAL_APP}.png`), LOCAL_ICON);
    return folder;
}

const spinning = async (page: Page): Promise<boolean> => (await page.locator('[role="dialog"] button:has(svg.animate-spin)').count()) > 0;
const has = async (page: Page, selector: string): Promise<boolean> => (await page.locator(selector).count()) > 0;

/** One install through the panel over `mode`, watching what it reports. */
async function run(host: Host, mode: string, res: Checks): Promise<void> {
    const stub = runnerStub(join(WORKDIR, `apps-stub-bin-${mode}`));
    const { url } = await host.start({ label: mode, mode, env: { SELKIES_WEB_ROOT: DASHBOARD_DIST, PATH: `${stub}${delimiter}${process.env.PATH ?? ''}`, SELKIES_COMMAND_ENABLED: 'true' } });
    const browser = await launchChromium();
    try {
        const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 });
        await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
        const page = await ctx.newPage();
        await page.route('**/proot-apps/**/metadata.yml', (route) => route.fulfill({ status: 200, contentType: 'text/yaml', body: CATALOG }));
        await page.goto(url, { waitUntil: 'load' });
        await sleep(8000);
        res.check(`${mode}: the panel is published where its runner works`, await openAppsModal(page));

        const card = page.locator('[role="dialog"] [data-slot="card"]:has-text("E2E App")').first();
        res.check(`${mode}: the catalog is listed`, (await card.count()) > 0);
        await page.locator('text=E2E App').first().click();
        await sleep(500);
        const install = page.locator('[role="dialog"] button:has-text("Install")').first();
        res.check(`${mode}: an uninstalled app offers Install`, (await install.count()) > 0);

        await install.click();
        let running = false;
        let held = false;
        for (const until = Date.now() + (INSTALL_SECONDS - 1) * 1000; Date.now() < until;) {
            if (await spinning(page)) {
                running = true;
                held = await install.isDisabled();
                break;
            }
            await sleep(200);
        }
        res.check(`${mode}: the button that started the job reports it running`, running);
        res.check(`${mode}: and holds the row while it runs`, held);
        const console = page.locator('[role="dialog"] .app-job-console').first();
        const text = async (): Promise<string> => ((await console.count()) ? console.innerText() : '');
        let streamed = '';
        for (const until = Date.now() + INSTALL_SECONDS * 1000; Date.now() < until;) {
            streamed = await text();
            if (streamed.includes('Installing e2eapp') && streamed.includes('%')) break;
            await sleep(200);
        }
        res.check(`${mode}: what the runner prints streams into the console while it runs`, streamed.includes('Installing e2eapp') && streamed.includes('%'), streamed.slice(-200));
        res.check(`${mode}: without the status lines the runner adds for the server`, !streamed.includes('##pa'), streamed.slice(-200));

        let settled = false;
        for (const until = Date.now() + (INSTALL_SECONDS + 20) * 1000; Date.now() < until;) {
            if (!(await spinning(page))) {
                settled = true;
                break;
            }
            await sleep(300);
        }
        res.check(`${mode}: the job ending stops the spinner`, settled);
        res.check(`${mode}: the panel then offers the app as installed`, await has(page, '[role="dialog"] button:has-text("Remove")'));
        const finished = await text();
        res.check(`${mode}: the console stays after the job, at its last state`,
            (await console.getAttribute('data-state')) === 'done' && finished.includes('Adding icon') && finished.includes('100.0%') && !finished.includes('40.0%'), finished.slice(-200));
        await page.locator('[role="dialog"] .app-job-console .app-job-button.clear').first().click();
        let cleared = false;
        for (const until = Date.now() + 5000; Date.now() < until;) {
            if (!(await has(page, '[role="dialog"] .app-job-console'))) {
                cleared = true;
                break;
            }
            await sleep(200);
        }
        res.check(`${mode}: clearing it takes it away`, cleared);

        await page.locator('[role="dialog"] button:has-text("Remove")').first().click();
        let removed = '';
        for (const until = Date.now() + 10_000; Date.now() < until;) {
            const done = page.locator('[role="dialog"] .app-job-console[data-state="done"]');
            if ((await done.count()) && (await has(page, '[role="dialog"] button:has-text("Install")'))) {
                removed = await done.first().innerText();
                break;
            }
            await sleep(200);
        }
        res.check(`${mode}: an instant removal still leaves its console for the user to clear`, removed.includes('Removing e2eapp'), removed.slice(-200));
        await page.locator('[role="dialog"] button:has-text("Install")').first().click();
        for (const until = Date.now() + (INSTALL_SECONDS + 20) * 1000; Date.now() < until;) {
            if (await has(page, '[role="dialog"] button:has-text("Remove")')) break;
            await sleep(300);
        }

        // What is installed belongs to the session, not to one browser: a page
        // with no stored record of the install must still be told.
        await page.evaluate(() => { localStorage.clear(); });
        await page.reload({ waitUntil: 'load' });
        await sleep(8000);
        const reopened = await openAppsModal(page);
        await page.locator('text=E2E App').first().click();
        await sleep(500);
        res.check(`${mode}: a page with no stored record is told what is installed`, reopened && (await has(page, '[role="dialog"] button:has-text("Remove")')));
    } finally {
        await browser.close();
        await host.stop();
    }
}

/** The panel over `mode` on a session installing from a local repository. */
async function runLocalRepository(host: Host, mode: string, res: Checks): Promise<void> {
    const stub = runnerStub(join(WORKDIR, `apps-stub-bin-${mode}`));
    const folder = localRepository(join(WORKDIR, `apps-local-repo-${mode}`));
    const { url } = await host.start({ label: `local-${mode}`, mode, env: { SELKIES_WEB_ROOT: DASHBOARD_DIST, PATH: `${stub}${delimiter}${process.env.PATH ?? ''}`, SELKIES_COMMAND_ENABLED: 'true', PA_REPO_FOLDER: folder } });
    const browser = await launchChromium();
    try {
        const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 });
        await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
        const page = await ctx.newPage();
        const remote: string[] = [];
        await page.route('**/raw.githubusercontent.com/**', (route) => { remote.push(route.request().url()); return route.abort(); });
        await page.goto(url, { waitUntil: 'load' });
        await sleep(8000);
        res.check(`${mode}: the panel is published on a local repository`, await openAppsModal(page));
        const card = page.locator('[role="dialog"] [data-slot="card"]:has-text("Local App")').first();
        res.check(`${mode}: the repository's own catalog is listed`, (await card.count()) > 0);
        const icon = card.locator('img').first();
        let width = 0;
        for (const until = Date.now() + 10_000; Date.now() < until;) {
            width = (await icon.count()) ? await icon.evaluate((img) => (img instanceof HTMLImageElement ? img.naturalWidth : 0)) : 0;
            if (width) break;
            await sleep(200);
        }
        res.check(`${mode}: its icon is served by the server`, width === 1, width);
        res.check(`${mode}: the remote catalog is never asked for`, remote.length === 0, remote.slice(0, 2));
    } finally {
        await browser.close();
        await host.stop();
    }
}

test('apps_panel', async () => {
    const res = new Checks('apps-panel');
    const host = new Host(suiteOf(import.meta.filename));
    try {
        for (const mode of ['websockets', 'webrtc']) await run(host, mode, res);
        for (const mode of ['websockets', 'webrtc']) await runLocalRepository(host, mode, res);
    } finally {
        await host.close();
    }
    res.finish();
});
