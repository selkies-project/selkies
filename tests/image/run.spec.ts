/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The image tier: the maintainer's human-testing checklist against published images.
//
// Each image is brought up by tag or digest (target.ts), once per capture
// backend, and every item of the checklist runs on every transport in every
// engine, each a cell of image x transport x backend x engine. The items are
// the modules in items/, one per numbered entry of the checklist (README "Image
// tier"); each prints PASS/FAIL/SKIP lines as the other tiers' suites do. The
// run ends with a table of the cells, written with the raw results and the
// screenshots under `$E2E_WORKDIR/image-tier/<run>/`.
//
//     E2E_IMAGES="trixie=ghcr.io/selkies-project/selkies/desktop:main-debiantrixie" \
//         npx playwright test --config tests/playwright.config.ts image/run.spec.ts
//
//     E2E_IMAGES             label=ref pairs, comma-separated
//     E2E_IMAGE_TRANSPORTS   websockets,webrtc       E2E_IMAGE_BACKENDS  x11,wayland
//     E2E_IMAGE_ENGINES      chromium,firefox,webkit E2E_IMAGE_ITEMS     1-10
//     E2E_IMAGE_GPU          none, nvidia (any NVENC GPU, AV1-capable preferred),
//                            nvidia-av1 (Ada or newer only), or amd; E2E_IMAGE_GPU_PRODUCTS
//                            names the nvidia.com/gpu.product values to pick from instead
//     E2E_IMAGE_PRIORITY_CLASS  the pods' priorityClassName
//     E2E_IMAGE_CPU          each pod's CPU request in whole CPUs (default 4; the limit is twice it)
//     E2E_IMAGE_MEMORY       each pod's memory request and limit (default 8Gi)
//     E2E_IMAGE_ENV          extra container environment, comma-separated KEY=VALUE (a value may hold commas)
//     E2E_IMAGE_LABELS       extra pod labels, comma-separated key=value
//     E2E_IMAGE_NAME         pod or container name prefix (default selkies-imagetest)
//     E2E_IMAGE_KEEP         1 leaves each target up after its run
//     E2E_IMAGE_CONSOLE      1 keeps each page's console in the run's folder
//     SELKIES_IMAGE_SITE     the in-session helper to copy in, in place of the static
//                            build of crates/selkies-image-site
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

import { test } from '@playwright/test';

import { fields, stringOr } from '../../web/selkies-web-core/lib/narrow.ts';
import { ENGINES, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, WORKDIR } from '../lib/env.ts';
import { closeBrowser, within } from '../lib/stream.ts';
import * as i01 from './items/i01_encoders.ts';
import * as i02 from './items/i02_media_devices.ts';
import * as i03 from './items/i03_displays.ts';
import * as i04 from './items/i04_clipboard.ts';
import * as i05 from './items/i05_transfers.ts';
import * as i06 from './items/i06_apps.ts';
import * as i07 from './items/i07_gamepad.ts';
import * as i08 from './items/i08_shared.ts';
import * as i09 from './items/i09_shortcuts.ts';
import * as i10 from './items/i10_media_ime.ts';
import { Cell, installSite, launchClient, requireDisplay, sessionBrowser, type Facts } from './lib.ts';
import { make, type Target } from './target.ts';

// NVENC-capable products by generation (the compute-only parts have none);
// E2E_IMAGE_GPU_PRODUCTS replaces the list for a cluster that names others.
const AV1_GPUS = ['NVIDIA-L4', 'NVIDIA-L40', 'NVIDIA-L40S', 'NVIDIA-RTX-4000-Ada-Generation',
    'NVIDIA-RTX-5000-Ada-Generation', 'NVIDIA-GeForce-RTX-4090'];
const NVENC_GPUS = [...AV1_GPUS, 'NVIDIA-GeForce-RTX-3090', 'NVIDIA-A10', 'NVIDIA-A40', 'NVIDIA-RTX-A6000',
    'NVIDIA-RTX-A5000', 'NVIDIA-RTX-A4000', 'NVIDIA-GeForce-RTX-2080-Ti',
    'Quadro-RTX-6000', 'Quadro-RTX-8000', 'NVIDIA-TITAN-RTX', 'Tesla-V100-SXM2-32GB',
    'Tesla-V100-SXM2-16GB', 'Tesla-V100-PCIE-16GB', 'NVIDIA-GeForce-GTX-1080-Ti',
    'NVIDIA-GeForce-GTX-1080', 'NVIDIA-TITAN-Xp', 'NVIDIA-TITAN-X-Pascal'];
const BACKEND_ENV: Record<string, string> = { x11: 'false', wayland: 'true' };

/** Node affinity for an NVIDIA class: GPUs with NVENC (the compute-only parts have none). */
function gpuAffinity(gpu: string): Record<string, unknown> | null {
    const terms = (values: readonly string[]): Record<string, unknown>[] => [{ matchExpressions: [{ key: 'nvidia.com/gpu.product', operator: 'In', values }] }];
    const chosen = (process.env.E2E_IMAGE_GPU_PRODUCTS ?? '').split(',').filter(Boolean);
    if (gpu === 'nvidia-av1') {
        return { nodeAffinity: { requiredDuringSchedulingIgnoredDuringExecution: { nodeSelectorTerms: terms(chosen.length ? chosen : AV1_GPUS) } } };
    }
    if (gpu === 'nvidia') {
        return {
            nodeAffinity: {
                requiredDuringSchedulingIgnoredDuringExecution: { nodeSelectorTerms: terms(chosen.length ? chosen : NVENC_GPUS) },
                preferredDuringSchedulingIgnoredDuringExecution: [{ weight: 100, preference: terms(AV1_GPUS)[0] }],
            },
        };
    }
    return null;
}

/**
 * Comma-separated KEY=VALUE pairs. A part without `sep` continues the value
 * before it, so a value may hold commas (`SELKIES_ENCODER=h264enc,vp8enc`).
 */
function pairs(text: string, sep = '='): Record<string, string> {
    const out: Record<string, string> = {};
    let key: string | null = null;
    for (const part of text.split(',').map((p) => p.trim()).filter(Boolean)) {
        if (!part.includes(sep) && key !== null) {
            out[key] += `,${part}`;
            continue;
        }
        const at = part.indexOf(sep);
        key = at < 0 ? part : part.slice(0, at);
        out[key] = at < 0 ? '' : part.slice(at + sep.length);
    }
    return out;
}

function itemNumbers(text: string): number[] {
    const out: number[] = [];
    for (const part of text.split(',')) {
        const [a, b] = part.split('-');
        for (let n = Number(a); n <= Number(b || a); n += 1) out.push(n);
    }
    return out;
}

/** A checklist item, as items/iNN_*.ts exports it. */
interface Item {
    ITEM: number;
    TITLE: string;
    ENGINES?: readonly string[];
    ENGINE_NOTE?: string;
    run: (cell: Cell) => Promise<void>;
}

// The checklist, one module an entry.
const ITEMS: readonly Item[] = [i01, i02, i03, i04, i05, i06, i07, i08, i09, i10];

/** What an image brings, read once per target. */
async function factsOf(target: Target): Promise<Facts> {
    const tools = await target.out('for t in xdotool xclip xrandr wmctrl wl-copy wl-paste ffmpeg google-chrome chromium firefox '
        + 'fcitx5 ibus-daemon pw-record parec pactl kwin_x11 kwin_wayland labwc qdbus6 qdbus; do '
        + "command -v $t >/dev/null && printf '%s ' $t; done");
    return {
        digest: target.digest, node: target.node, tools: tools.split(/\s+/).filter(Boolean),
        status: await target.api('/api/status'),
        // The server and the pixelflux and pcmflux builds it links, as it reports them
        packages: await target.out('selkies --version 2>&1 | head -1'),
        gpu: await target.out("nvidia-smi --query-gpu=name,driver_version --format=csv,noheader 2>/dev/null | head -1; ls /dev/dri 2>/dev/null | tr '\\n' ' '"),
    };
}

/** The streaming mode the dashboard's switch picks, asked of the server the same way. */
async function switchMode(target: Target, transport: string, timeoutS = 60): Promise<boolean> {
    const mode = async (): Promise<string | undefined> => stringOr(fields(await target.api('/api/status')).current_mode);
    if (await mode() === transport) return true;
    await target.api('/api/switch', { mode: transport }, 30);
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if (await mode() === transport) return true;
        await sleep(1000);
    }
    return false;
}

interface Row {
    cell: string[];
    item: number;
    pass: number;
    fail: number;
    skip: number;
    failed: string[];
    skipped?: string[];
    note?: string;
    secs?: number;
}

async function runCell(items: readonly Item[], target: Target, label: string, transport: string, backend: string, engine: Engine,
    facts: Facts, outdir: string, rows: Row[], save: () => void): Promise<void> {
    let client;
    try {
        client = await launchClient(engine);
    } catch (e) {
        for (const item of items) {
            rows.push({ cell: [label, transport, backend, engine], item: item.ITEM, pass: 0, fail: 0, skip: 1, failed: [], note: `browser: ${String(e)}`.slice(0, 200) });
        }
        return;
    }
    try {
        // Firefox keeps its profile between cells: start each from what a new visitor stores.
        try {
            const fresh = await client.ctx.newPage();
            try {
                await fresh.goto(`${target.url}/api/status`, { timeout: 30_000 });
                await fresh.evaluate(() => { localStorage.clear(); });
            } finally {
                await fresh.close();
            }
        } catch {
            // a fresh profile already
        }
        for (const item of items) {
            const cell = new Cell(target, label, transport, backend, engine, client.ctx, facts, outdir);
            cell.res = new Checks(`${cell.id} ${item.ITEM}`);
            const started = Date.now();
            if (!(item.ENGINES ?? ['chromium', 'firefox', 'webkit']).includes(engine)) {
                cell.res.skip(item.TITLE, item.ENGINE_NOTE ?? `not an item for ${engine}`);
            } else {
                try {
                    await item.run(cell);
                } catch (e) {
                    cell.res.check(`${item.TITLE}: ran to the end`, false, `${e instanceof Error ? `${e.name}: ${e.message}` : String(e)}`.slice(0, 300));
                    process.stderr.write(`${e instanceof Error ? (e.stack ?? e.message) : String(e)}\n`);
                } finally {
                    for (const page of client.ctx.pages()) {
                        try {
                            await within(page.close(), 'the page', 30);
                        } catch {
                            // a page that will not close goes with its browser
                        }
                    }
                }
            }
            cell.res.summary();
            rows.push({ cell: [label, transport, backend, engine], item: item.ITEM, ...cell.res.tally(), secs: Math.round((Date.now() - started) / 1000) });
            // Kept item by item: a browser that takes its driver down loses nothing already checked.
            save();
        }
    } finally {
        await closeBrowser(client.closer);
    }
}

function table(rows: readonly Row[], numbers: readonly number[]): string {
    const cells = new Map<string, { cell: string[]; items: Map<number, Row> }>();
    for (const r of rows) {
        const key = JSON.stringify(r.cell);
        if (!cells.has(key)) cells.set(key, { cell: r.cell, items: new Map() });
        cells.get(key)?.items.set(r.item, r);
    }
    const lines = [`| image | transport | backend | engine | ${numbers.join(' | ')} |`, `|${' --- |'.repeat(4 + numbers.length)}`];
    for (const { cell, items } of cells.values()) {
        const marks = numbers.map((n) => {
            const r = items.get(n);
            if (!r) return '';
            if (r.pass + r.fail === 0) return 'skip';
            return `${r.pass}/${r.pass + r.fail}${r.fail ? ' FAIL' : ''}`;
        });
        lines.push(`| ${cell.join(' | ')} | ${marks.join(' | ')} |`);
    }
    return lines.join('\n');
}

test('run', async () => {
    const res = new Checks('image');
    const images = pairs(process.env.E2E_IMAGES ?? '');
    if (!Object.keys(images).length) {
        res.skip('the image tier', 'no image named: set E2E_IMAGES=label=ref[,label=ref]');
        res.finish();
        return;
    }
    requireDisplay();
    const numbers = itemNumbers(process.env.E2E_IMAGE_ITEMS ?? '1-10');
    const items = ITEMS.filter((item) => numbers.includes(item.ITEM));
    const runId = new Date().toISOString().replace(/[-:]/g, '').replace('T', '-').slice(0, 15);
    const outdir = join(WORKDIR, 'image-tier', runId);
    mkdirSync(outdir, { recursive: true });
    const gpu = process.env.E2E_IMAGE_GPU ?? 'none';
    const prefix = process.env.E2E_IMAGE_NAME ?? 'selkies-imagetest';
    const transports = (process.env.E2E_IMAGE_TRANSPORTS ?? 'websockets,webrtc').split(',');
    const backends = (process.env.E2E_IMAGE_BACKENDS ?? 'x11,wayland').split(',');
    const engines = (process.env.E2E_IMAGE_ENGINES ?? 'chromium,firefox,webkit').split(',').filter((e): e is Engine => (ENGINES as readonly string[]).includes(e));
    const rows: Row[] = [];
    const imagesSeen: Record<string, unknown> = {};
    const save = (): void => {
        writeFileSync(join(outdir, 'results.json'), JSON.stringify({ images: imagesSeen, rows }, null, 1));
    };
    for (const [label, ref] of Object.entries(images)) {
        for (const backend of backends) {
            const env = { TZ: 'UTC', SELKIES_ENABLE_BASIC_AUTH: 'false', SELKIES_WAYLAND: BACKEND_ENV[backend] ?? 'false', ...pairs(process.env.E2E_IMAGE_ENV ?? '') };
            const name = `${prefix}-${label}-${backend}`.toLowerCase().replaceAll('_', '-').slice(0, 63);
            const target = make(ref, name, env, {
                labels: pairs(process.env.E2E_IMAGE_LABELS ?? ''), gpu, affinity: gpuAffinity(gpu),
                cpu: process.env.E2E_IMAGE_CPU ?? '4', memory: process.env.E2E_IMAGE_MEMORY ?? '8Gi',
            });
            process.stdout.write(`=== ${label} ${backend}: bringing up ${ref}\n`);
            try {
                await target.up();
            } catch (e) {
                process.stdout.write(`FAIL  [${label}/${backend}] the image came up  ${String(e)}\n`);
                rows.push({ cell: [label, '-', backend, '-'], item: 0, pass: 0, fail: 1, skip: 0, failed: [`bring-up: ${String(e)}`.slice(0, 300)] });
                await target.down();
                continue;
            }
            try {
                const facts = await factsOf(target);
                facts.site = await installSite(target);
                facts.session_browser = await sessionBrowser(target)
                    && (await new Cell(target, label, '', backend, '', null, facts, outdir).report('pattern', 60)) !== null;
                imagesSeen[`${label}/${backend}`] = { ref, ...facts };
                process.stdout.write(`=== ${label} ${backend}: ${JSON.stringify(facts).slice(0, 600)}\n`);
                for (const transport of transports) {
                    if (!await target.alive()) {
                        process.stdout.write(`=== ${label} ${backend}: the target went away (preempted?); bringing it up again\n`);
                        await target.down();
                        await target.up();
                        Object.assign(facts, await factsOf(target), { site: await installSite(target) });
                        await sessionBrowser(target);
                    }
                    if (!await switchMode(target, transport)) {
                        process.stdout.write(`FAIL  [${label}/${backend}] switched to ${transport}\n`);
                        rows.push({ cell: [label, transport, backend, '-'], item: 0, pass: 0, fail: 1, skip: 0, failed: [`no switch to ${transport}`] });
                        continue;
                    }
                    for (const engine of engines) await runCell(items, target, label, transport, backend, engine, facts, outdir, rows, save);
                }
                writeFileSync(join(outdir, `${label}-${backend}-server.log`), await target.selkiesLog(20_000));
            } finally {
                if (process.env.E2E_IMAGE_KEEP !== '1') await target.down();
            }
        }
    }
    const text = table(rows, numbers);
    writeFileSync(join(outdir, 'table.md'), `${text}\n`);
    save();
    process.stdout.write(`\n${text}\n\nresults: ${outdir}\n`);
    const failed = rows.reduce((sum, r) => sum + r.fail, 0);
    process.stdout.write(`\n=== IMAGE TIER: ${failed ? 'FAIL' : 'PASS'} (${failed} failed checks) ===\n`);
    res.check('every cell passed', failed === 0, `${failed} failed checks`);
    res.finish();
});
