/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The microphone suites' browser and sound server: a Chromium page whose fake
// audio device stands in for a microphone, the dashboard's pipeline control,
// and the server's SelkiesVirtualMic read back through pactl.
import { spawnSync } from 'node:child_process';

import type { Browser, Page } from '@playwright/test';
import { chromium } from '@playwright/test';

import { BROWSER_ARGS } from './browsers.ts';
import { onPath } from './desktop.ts';
import { sleep } from './env.ts';
import { within } from './stream.ts';

export const VIRTUAL_MIC = 'SelkiesVirtualMic';

export const MIC_JS = `
  window.__micStatus = [];
  window.addEventListener('message', (e) => {
    const d = e.data;
    if (!d || !d.type) return;
    if ((d.type === 'sidebarButtonStatusUpdate' || d.type === 'pipelineStatusUpdate') && d.microphone !== undefined) {
      window.__micStatus.push(!!d.microphone);
    }
  });
`;

function pactl(...args: string[]): string {
    if (onPath('pactl') === null) return '';
    return spawnSync('pactl', args, { encoding: 'utf8', timeout: 10_000, env: { ...process.env, LC_ALL: 'C' } }).stdout ?? '';
}

const rows = (text: string): string[][] => text.split('\n').filter(Boolean).map((l) => l.split('\t'));
export const virtualMicSources = (): string[] => rows(pactl('list', 'short', 'sources')).filter((p) => p.length > 1 && p[1].includes(VIRTUAL_MIC)).map((p) => p[1]);
const virtualMicModules = (): string[] => pactl('list', 'short', 'modules').split('\n').filter((l) => l.includes('module-virtual-source') && l.includes(VIRTUAL_MIC)).map((l) => l.split('\t')[0]);

/** A SelkiesVirtualMic a previous run left behind would be reused rather than created, hiding the provisioning under test; unload it first. */
export function unloadLeftoverVirtualMic(): void {
    for (const module of virtualMicModules()) pactl('unload-module', module);
}

/** Index of the sink pcmflux's mic playback stream feeds, or null. */
export function pcmfluxPlaybackSink(): string | null {
    let sink: string | null = null;
    for (const block of pactl('list', 'sink-inputs').split('Sink Input #').slice(1)) {
        if (!block.includes('application.name = "pcmflux"')) continue;
        for (const line of block.split('\n')) if (line.trim().startsWith('Sink:')) sink = line.split(':', 2)[1].trim();
    }
    return sink;
}

export const sinkIndex = (name: string): string | null => rows(pactl('list', 'short', 'sinks')).find((p) => p.length > 1 && p[1] === name)?.[0] ?? null;

export function defaultSource(): string {
    const line = pactl('info').split('\n').find((l) => l.startsWith('Default Source:'));
    return line ? line.split(':', 2)[1].trim() : '';
}

export async function waitFor(pred: () => boolean | Promise<boolean>, timeoutS: number, intervalS = 0.5): Promise<boolean> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if (await pred()) return true;
        await sleep(intervalS * 1000);
    }
    return Boolean(await pred());
}

export const toggle = (page: Page, enabled: boolean): Promise<unknown> => within(page.evaluate(`window.postMessage({type: 'pipelineControl', pipeline: 'microphone', enabled: ${enabled}}, window.location.origin)`));
export const statuses = async (page: Page): Promise<unknown[]> => {
    const got = await within(page.evaluate('window.__micStatus'));
    return Array.isArray(got) ? got.map((v): unknown => v) : [];
};
export const waitStatus = (page: Page, value: boolean, timeoutS = 20): Promise<boolean> => waitFor(async () => (await statuses(page)).at(-1) === value, timeoutS, 0.25);

/**
 * The full Chromium build (new headless mode) or the system Chrome E2E_CHROME
 * names: the headless shell has no media capture. The fake device plays a
 * tone into getUserMedia.
 */
export async function launchMic(url: string, mode: string): Promise<[Browser, Page, string[]]> {
    const executablePath = process.env.E2E_CHROME || undefined;
    const browser = await chromium.launch({ headless: true, args: [...BROWSER_ARGS, '--use-fake-device-for-media-stream'], ...(executablePath ? { executablePath } : { channel: 'chromium' }) });
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, permissions: ['microphone'] });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    await ctx.addInitScript({ content: MIC_JS });
    const page = await ctx.newPage();
    const errors: string[] = [];
    page.on('pageerror', (e) => errors.push(String(e)));
    page.on('dialog', (d) => {
        errors.push(`dialog: ${d.message()}`);
        void d.dismiss();
    });
    await page.goto(`${url}/`, { waitUntil: 'load' });
    return [browser, page, errors];
}
