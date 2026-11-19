/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What the transport matrix needs to reach a server either directly or through
 * the authenticated TLS proxy of `authproxy.ts`: the address pages open, the
 * browser context options that go with it, an API client for the status
 * routes, and the checks that only hold behind the proxy (refusals, the socket
 * upgrades it counted, a secure context, and a reload that streams again).
 *
 * A {@link Front} is what every URL of a cell is built from; behind the proxy
 * the server's own address is used by nothing.
 *
 * Over WebSockets a frame counts as decoded output when {@link sinkProbe}
 * finds it in the sink the page is showing: a `<video>` that holds a current
 * frame (a draw of it onto a scratch canvas is not transparent), the worker's
 * OffscreenCanvas once the page has hidden its main canvas (the client does
 * that only after the worker reported a presented frame), or the main canvas
 * after a draw (it is transparent until one). That is a frame the decoder
 * produced and the sink took, not an encoded chunk or a canvas given a size.
 * A desktop of one color is a legitimate picture, so the content is not
 * compared. WebRTC decodes into the `<video>` element, which `waitWrVideo`
 * already requires to hold a frame.
 * @module
 */

import { request, type APIRequestContext, type BrowserContextOptions, type Page } from '@playwright/test';

import { fields, type Fields } from '../../web/selkies-web-core/lib/narrow.ts';
import { AuthProxy } from './authproxy.ts';
import type { Checks } from './checks.ts';
import { sleep } from './env.ts';
import type { Started } from './host.ts';
import { waitWrVideo, within } from './stream.ts';

/** A reply to an API route: its status and its body as JSON. */
export interface ApiReply {
    status: number;
    json: () => Promise<unknown>;
}

/** How a cell reaches its server. */
export interface Front {
    /** The origin pages open. */
    readonly url: string;
    /** What `launchClient` takes besides the URL; empty for a direct server. */
    readonly client: { httpCredentials?: BrowserContextOptions['httpCredentials']; ignoreHTTPSErrors?: boolean };
    /** GET `route` (`/api/status`) with the credentials a client of this front holds. */
    get: (route: string) => Promise<ApiReply>;
    /** Release what the front holds, the proxy last; always stops the proxy. */
    close: () => Promise<void>;
}

/** A front that is the authenticated proxy. */
export interface ProxiedFront extends Front {
    readonly proxy: AuthProxy;
}

/** @param server The started server. @returns A front on its own plain address. */
export function directFront(server: Started): Front {
    return {
        url: server.url,
        client: {},
        get: async (route) => {
            const r = await fetch(`${server.url}${route}`, { signal: AbortSignal.timeout(10_000) });
            return { status: r.status, json: async (): Promise<unknown> => r.json() };
        },
        close: () => Promise.resolve(),
    };
}

/**
 * Put the authenticated TLS proxy in front of `server`.
 * @param server The started server.
 * @returns The front; its `close` stops the proxy.
 */
export async function frontBehindProxy(server: Started): Promise<ProxiedFront> {
    const proxy = await AuthProxy.start(server.port);
    const httpCredentials = { ...proxy.httpCredentials, origin: new URL(proxy.url).origin };
    let rest: APIRequestContext;
    try {
        rest = await request.newContext({ ignoreHTTPSErrors: true, httpCredentials });
    } catch (e) {
        await proxy.stop();
        throw e;
    }
    return {
        proxy,
        url: proxy.url,
        client: { httpCredentials, ignoreHTTPSErrors: true },
        get: async (route) => {
            const r = await rest.get(`${proxy.url}${route}`, { timeout: 10_000, maxRedirects: 0 });
            return {
                status: r.status(),
                json: async (): Promise<unknown> => {
                    const body: unknown = await r.json();
                    return body;
                },
            };
        },
        close: async () => {
            try {
                await rest.dispose();
            } finally {
                await proxy.stop();
            }
        },
    };
}

/**
 * The proxy must refuse a request without credentials and one with wrong ones,
 * and answer the credentials it issued.
 * @param res The cell's checks.
 * @param front The proxied front.
 */
export async function refusalChecks(res: Checks, front: ProxiedFront): Promise<void> {
    const { proxy } = front;
    const origin = new URL(proxy.url).origin;
    const wrongPassword = `${proxy.httpCredentials.password}-wrong`;
    const anon = await request.newContext({ ignoreHTTPSErrors: true });
    const wrong = await request.newContext({ ignoreHTTPSErrors: true, httpCredentials: { ...proxy.httpCredentials, password: wrongPassword, origin } });
    try {
        for (const route of ['/', '/api/status']) {
            const refused = await anon.get(`${proxy.url}${route}`, { timeout: 10_000, maxRedirects: 0 });
            res.check(`proxy: unauthenticated ${route} refused`, refused.status() === 401, refused.status());
        }
        const refused = await wrong.get(`${proxy.url}/api/status`, { timeout: 10_000, maxRedirects: 0 });
        res.check('proxy: wrong credentials refused', refused.status() === 401, refused.status());
    } finally {
        await Promise.all([anon.dispose(), wrong.dispose()]);
    }
    res.check('proxy: refusals counted', proxy.rejected >= 3, proxy.rejected);
    const status = (await front.get('/api/status')).status;
    res.check('proxy: authenticated api/status is 200', status === 200, status);
}

/**
 * The server's description of the stream, once the page holds one that names
 * an encoder, else the last reading.
 * @param page The client.
 * @returns `window.stream_info` as the server sent it, empty before it arrives.
 */
async function waitStreamInfo(page: Page): Promise<Fields> {
    let info: Fields = {};
    for (const until = Date.now() + 15_000; Date.now() < until;) {
        info = fields(await within(page.evaluate('window.stream_info || null')));
        if (typeof info.encoder === 'string' && info.encoder !== '') break;
        await sleep(500);
    }
    return info;
}

/** The sink the WebSockets client shows and the size of what it holds. */
export interface Sink {
    kind: 'video' | 'worker-canvas' | 'canvas';
    w: number;
    h: number;
}

/**
 * Runs in the page: the sink that holds a decoded frame, or null. It reads the
 * page's own elements by the ids the client gives them (`videoStream`,
 * `videoWorkerCanvas`, `videoCanvas`) and installs nothing.
 * @returns The sink, or null while none shows a frame.
 */
export function sinkProbe(): Sink | null {
    const shown = (el: HTMLElement | null): el is HTMLElement => el !== null && el.style.display !== 'none';
    const painted = (source: CanvasImageSource): boolean => {
        const scratch = document.createElement('canvas');
        scratch.width = 16;
        scratch.height = 9;
        const ctx = scratch.getContext('2d');
        if (!ctx) return false;
        ctx.drawImage(source, 0, 0, 16, 9);
        const pixels = ctx.getImageData(0, 0, 16, 9).data;
        for (let i = 3; i < pixels.length; i += 4) if ((pixels[i] ?? 0) > 0) return true;
        return false;
    };
    const video = document.querySelector<HTMLVideoElement>('video#videoStream');
    const worker = document.querySelector<HTMLCanvasElement>('canvas#videoWorkerCanvas');
    const main = document.querySelector<HTMLCanvasElement>('canvas#videoCanvas');
    if (shown(video)) {
        const held = video.readyState >= 2 && video.videoWidth > 0 && painted(video);
        return held ? { kind: 'video', w: video.videoWidth, h: video.videoHeight } : null;
    }
    if (shown(worker)) {
        const presented = main !== null && main.style.display === 'none' && worker.width > 0;
        return presented ? { kind: 'worker-canvas', w: worker.width, h: worker.height } : null;
    }
    if (shown(main) && main.width >= 640 && painted(main)) return { kind: 'canvas', w: main.width, h: main.height };
    return null;
}

/**
 * Wait for the page to show a decoded frame.
 * @param page The client, freshly loaded for a check after a reload.
 * @param mode The transport.
 * @param timeoutS How long to wait.
 * @returns What shows it (the sink, or the `<video>`'s size on WebRTC), or null.
 */
export async function waitDecodedFrame(page: Page, mode: string, timeoutS = 30): Promise<unknown> {
    if (mode !== 'websockets') return waitWrVideo(page, timeoutS + 15);
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        const sink = await within(page.evaluate(sinkProbe));
        if (sink) return sink;
        await sleep(500);
    }
    return null;
}

/**
 * What only holds behind the proxy once the stream runs: the server encodes in
 * software, the browser's own sockets passed the proxy with the credentials it
 * cached, the page is a secure context, and a reload decodes a frame again
 * through a new upgrade.
 * @param res The cell's checks.
 * @param front The proxied front.
 * @param page The client, streaming.
 * @param mode The transport.
 */
export async function streamChecks(res: Checks, front: ProxiedFront, page: Page, mode: string): Promise<void> {
    const { proxy } = front;
    const info = await waitStreamInfo(page);
    res.check('proxy: software encoder reported', typeof info.encoder === 'string' && info.encoder !== '' && info.hardware === false,
        { encoder: info.encoder, hardware: info.hardware, codec: info.codec });
    res.check('proxy: browser upgraded the socket', proxy.upgrades >= 1, { upgrades: proxy.upgrades, rejected: proxy.rejected, requests: proxy.requests });
    res.check('proxy: page is a secure context', await within(page.evaluate(() => window.isSecureContext)) === true, front.url);
    res.check('proxy: a decoded frame is shown', await waitDecodedFrame(page, mode, 15) !== null, mode);
    const before = proxy.upgrades;
    await page.reload({ waitUntil: 'load' });
    const again = await waitDecodedFrame(page, mode);
    res.check('proxy: a decoded frame is shown after a reload', again !== null, again);
    res.check('proxy: reload upgraded a new socket', proxy.upgrades > before, { before, after: proxy.upgrades });
}
