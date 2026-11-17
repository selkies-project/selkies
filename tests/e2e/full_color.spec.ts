/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Full color is asked for only where the engine can decode it.
//
// video_fullcolor makes the H.264 encoders emit 4:4:4, which is High 4:4:4
// Predictive on the wire, and engines differ on whether their decoder has that
// profile. One that does not shows no picture at all rather than a worse one,
// with every decoder built and refused for as long as the session runs, so
// the client settles the question against its own decoder before it asks the
// server for anything. This suite asks each engine the same way rather than
// naming which ones can: what an engine decodes changes with its releases.
//
// Driven with the setting stored on before the page loads, the way a user who
// turned it on in Chromium and then opened the same URL in Safari arrives.
// Both transports, because the server encodes the same 4:4:4 for either.
//
// Firefox is covered on websockets alone: its WebRTC answer needs the OpenH264
// plugin the browser matrix side-loads into a profile of its own, and the
// answer about its decoder is the same on either transport.
//
// A -vp9 suffix drives the same question for VP9, whose 4:4:4 is profile 1:
// ws-chromium-vp9 asks the WebCodecs decoder, wr-chromium-vp9 the RTP
// receiver's capabilities, which is what the WebRTC client consults.
//
// ws-default and wr-default hold full color on through the server's own
// unlocked default against an engine that refuses it: the client turns it off
// and streams 4:2:0 on the same codec.
import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchBrowser, launchChromium, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { serverSoftwareEncoder } from '../lib/encode.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { WIRE_TAP_JS } from '../lib/page.ts';
import { closeBrowser, waitWrVideo, waitWsVideo, within, type VideoSize } from '../lib/stream.ts';

// The profile the full-color encoders emit, at the lowest level, so what the
// probe answers is about the profile and not the size of any one stream.
const FULLCOLOR_CODEC = 'avc1.F4001E';
const VP9_FULLCOLOR_CODEC = 'vp09.01.10.08.03';

const RTP_VP9_PROBE_JS = String.raw`(() => {
  try {
    return RTCRtpReceiver.getCapabilities('video').codecs.some((c) =>
      /^video\/vp9$/i.test(c.mimeType) && /(^|;)profile-id=1(;|$)/.test(c.sdpFmtpLine || ''));
  } catch (e) { return false; }
})()`;

declare global {
    interface Window {
        __fullcolorProbe?: boolean;
        __wireSent?: unknown[];
        video_fullcolor?: boolean;
    }
}

/** The page's stored value of `name` (video_fullcolor, encoder). */
function stored(page: Page, name = 'video_fullcolor'): Promise<string | null> {
    return within(page.evaluate((n) => {
        const k = (location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_');
        return localStorage.getItem(`${k}_${n}`);
    }, name));
}

/**
 * Whether the engine's decoder takes `codec`, or null when it never answers.
 * The answer is parked on the window and collected with a deadline: an
 * engine's isConfigSupported has none of its own, and an evaluate awaiting a
 * promise that never settles would hold the suite to the runner's kill.
 */
async function probeDecoder(page: Page, codec: string, timeoutS = 20): Promise<boolean | null> {
    await within(page.evaluate((c) => {
        window.__fullcolorProbe = undefined;
        void (async () => {
            if (typeof VideoDecoder === 'undefined') return false;
            try {
                const s = await VideoDecoder.isConfigSupported({ codec: c, codedWidth: 1280, codedHeight: 720 });
                return Boolean(s.supported);
            } catch {
                return false;
            }
        })().then((v) => { window.__fullcolorProbe = v; }, () => { window.__fullcolorProbe = false; });
    }, codec));
    try {
        await page.waitForFunction(() => window.__fullcolorProbe !== undefined, undefined, { timeout: timeoutS * 1000 });
    } catch {
        return null;
    }
    return within(page.evaluate(() => window.__fullcolorProbe ?? null));
}

/** Stores the encoder and full color before the client's first line runs. */
function initScript(mode: string, encoder = 'h264enc', fullcolor = true): string {
    return `
window.__SELKIES_STREAMING_MODE__ = '${mode}';
(() => {
  const k = (location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_');
  localStorage.setItem(k + '_encoder', '${encoder}');
  ${fullcolor ? "localStorage.setItem(k + '_video_fullcolor', 'true');" : ''}
})();
`;
}

// A decoder without the 4:4:4 profile, whatever this engine's really has: the
// locked scenarios are about what the client does with a refusal, and which
// engines refuse changes with their releases and the codecs their host carries.
const REFUSE_FULLCOLOR_JS = String.raw`
(() => {
  if (typeof VideoDecoder === 'undefined') return;
  const refused = (cfg) => /^avc1\.f4/i.test((cfg && cfg.codec) || '');
  const supported = VideoDecoder.isConfigSupported.bind(VideoDecoder);
  VideoDecoder.isConfigSupported = (cfg) =>
    refused(cfg) ? Promise.resolve({supported: false, config: cfg}) : supported(cfg);
  const configure = VideoDecoder.prototype.configure;
  VideoDecoder.prototype.configure = function (cfg) {
    if (refused(cfg)) throw new DOMException('Unsupported configuration', 'NotSupportedError');
    return configure.call(this, cfg);
  };
})();
`;

// A decoder that takes the probe and never answers it, which is what WebKit's
// does on a loaded machine.
const STALL_PROBE_JS = `
(() => {
  if (typeof VideoDecoder === 'undefined') return;
  VideoDecoder.isConfigSupported = () => new Promise(() => {});
})();
`;

/** How many settings payloads the page has put on the wire. */
function settingsSent(page: Page): Promise<number> {
    return within(page.evaluate(() => (window.__wireSent ?? []).filter((d) => typeof d === 'string' && d.startsWith('SETTINGS,')).length));
}

function video(page: Page, mode: string, timeoutS: number): Promise<VideoSize | null> {
    return mode === 'webrtc' ? waitWrVideo(page, timeoutS) : waitWsVideo(page, timeoutS);
}

interface Ctx {
    res: Checks;
    host: Host;
    url: string;
}

/** A page of `engine` at `path` with `script` before its own, its console kept. */
async function open(engine: Engine, script: string, url: string, said: string[] = []): Promise<[import('@playwright/test').Browser, Page]> {
    const browser = engine === 'chromium' ? await launchChromium() : await launchBrowser(engine);
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 } });
    await ctx.addInitScript({ content: script });
    const page = await ctx.newPage();
    page.on('console', (m) => said.push(m.text()));
    await page.goto(url, { waitUntil: 'load' });
    return [browser, page];
}

/**
 * A decoder that never answers the probe, on the transport under test. The
 * client asks its own decoder before it asks the server for anything, so an
 * unanswered probe stands between the session and its first message: what
 * must not happen is the session waiting on it. Full color is dropped as it is
 * for a decoder that refuses outright, since an engine that will not say
 * cannot be shown a stream only its 4:4:4 profile could decode.
 */
async function driveStalled({ res, url }: Ctx, mode: string): Promise<void> {
    const started = Date.now();
    const [browser, page] = await open('chromium', initScript(mode) + STALL_PROBE_JS + WIRE_TAP_JS, url);
    try {
        for (const until = started + 45_000; Date.now() < until && !(await settingsSent(page));) await sleep(500);
        const sent = await settingsSent(page);
        res.check("[stalled] the session's settings reach the server", sent > 0, `${sent} sent in ${((Date.now() - started) / 1000).toFixed(0)}s`);
        const v = await video(page, mode, 60);
        res.check('[stalled] and the stream plays', Boolean(v), v);
        const kept = await stored(page);
        res.check('[stalled] full color is dropped, as for a decoder that refuses', kept === 'false', kept);
    } finally {
        await closeBrowser(browser);
    }
}

/**
 * OpenH264 cannot emit 4:4:4, so a locked full color on a software session of
 * an OpenH264 build reaches the client as 4:2:0, which every engine decodes:
 * nothing is refused, nothing is said, and the stream plays where it is.
 */
async function driveOpenh264({ res, host, url }: Ctx, tag: string): Promise<void> {
    const said: string[] = [];
    const [browser, page] = await open('webkit', initScript('websockets') + REFUSE_FULLCOLOR_JS, `${url}/?offscreen_worker=false`, said);
    try {
        const played = Boolean(await waitWsVideo(page, 45));
        const encoder = await stored(page, 'encoder');
        const log = await host.log();
        res.check(`[${tag}] OpenH264 streams 4:2:0 under a locked full color`, log.includes('Colorspace: I420'), log.slice(-200));
        res.check(`[${tag}] and the stream plays where it is`, played && encoder === 'h264enc', [played, encoder]);
        const refused = said.filter((t) => t.includes('has no decoder for') || t.includes('cannot decode'));
        res.check(`[${tag}] nothing is refused`, refused.length === 0, refused.slice(0, 2));
    } finally {
        await browser.close();
    }
}

/**
 * The server holding full color on, for an engine that cannot decode it. A
 * client cannot turn a locked setting off, so it walks the refusal ladder
 * instead: the next allowed video encoder whose stream it decodes at the
 * locked full color, or whose codec carries none, and JPEG only when no video
 * codec is left. What must not happen is the decoder being built and refused
 * for as long as the session runs, with nothing on the page to say why.
 */
async function driveLocked(c: Ctx, pinned: boolean): Promise<void> {
    if (serverSoftwareEncoder(await c.host.serverBinary()) === 'openh264') {
        await driveOpenh264(c, pinned ? 'pinned' : 'locked');
        return;
    }
    if (pinned) {
        await drivePinned(c);
        return;
    }
    const { res, url } = c;
    const said: string[] = [];
    const [browser, page] = await open('webkit', initScript('websockets') + REFUSE_FULLCOLOR_JS, `${url}/?offscreen_worker=false`, said);
    try {
        const played = Boolean(await waitWsVideo(page, 45));
        // The ladder may take more than one step; read the encoder once it holds still.
        let encoder: string | null = null;
        for (let since = Date.now(); Date.now() - since < 4000;) {
            const now = await stored(page, 'encoder');
            if (now !== encoder) {
                encoder = now;
                since = Date.now();
            }
            await sleep(500);
        }
        const switched = said.filter((t) => t.includes('has no decoder for avc1.F4'));
        res.check('[locked] the 4:4:4 the server insists on is named once', switched.length === 1, switched.length ? switched : said.slice(-2));
        res.check('[locked] the client steps to a video codec it decodes before JPEG', ['h265enc', 'vp8enc', 'vp9enc', 'av1enc'].includes(encoder ?? ''), encoder);
        res.check('[locked] and the stream plays there', played, played);
        const spam = said.filter((t) => t.includes('Error configuring VNC stripe decoder'));
        res.check('[locked] no decoder is left reporting the refusal per frame', spam.length === 0, spam.slice(0, 2));
    } finally {
        await closeBrowser(browser);
    }
}

/**
 * Full color and the encoder both held by the server: nothing the client may
 * change reaches a stream it can decode, so it says so on the page rather
 * than staying black and quiet.
 */
async function drivePinned({ res, url }: Ctx): Promise<void> {
    const said: string[] = [];
    const [browser, page] = await open('webkit', initScript('websockets') + REFUSE_FULLCOLOR_JS, `${url}/?offscreen_worker=false`, said);
    try {
        // Past the no-output watchdog, its software retry, and the reload the
        // fatal fallback ends in, none of which a stream the engine refused
        // may reach.
        await page.waitForTimeout(25_000);
        const told = said.filter((t) => t.includes('which this browser cannot decode'));
        res.check('[pinned] the stream it cannot decode is reported once', told.length === 1, told.length ? told : said.slice(-2));
        const starts = said.filter((t) => t.includes('Streaming mode determined'));
        res.check('[pinned] and the page stands rather than reloading into the same stream', starts.length === 1, starts.length);
        // A setting that restarts the capture has the server announce the same
        // encoder and full color again and then send a key frame the engine
        // refuses again; the refusal stands and is not reported a second time.
        await within(page.evaluate(() => { window.postMessage({ type: 'settings', settings: { cpu_encode_threading: false } }, window.location.origin); }));
        await page.waitForTimeout(8000);
        const again = said.filter((t) => t.includes('which this browser cannot decode'));
        res.check('[pinned] and not again when the server announces the same', again.length === told.length, again);
        const shown = await within(page.evaluate(() => {
            const e = document.getElementById('status-display');
            return e ? [e.className, (e.textContent ?? '').slice(0, 60)] : null;
        }));
        res.check('[pinned] and said on the page, not only in the console', shown !== null && !shown[0].includes('hidden'), shown);
        const spam = said.filter((t) => t.includes('Error configuring VNC stripe decoder'));
        res.check('[pinned] no decoder is left reporting the refusal per frame', spam.length === 0, spam.slice(0, 2));
    } finally {
        await closeBrowser(browser);
    }
}

/**
 * The server's own default holding full color on, unlocked, for an engine
 * whose decoder has no 4:4:4 H.264: the client turns the setting off for
 * itself and the stream comes back 4:2:0 on the same codec, not on JPEG and
 * not as a stream this browser paints nothing of.
 */
async function driveDefault({ res, host, url }: Ctx, engine: Engine, mode: string): Promise<void> {
    const tag = `${engine}-${mode}-default`;
    const said: string[] = [];
    const [browser, page] = await open(engine, initScript(mode, 'h264enc', false) + REFUSE_FULLCOLOR_JS,
        mode === 'websockets' ? `${url}/?offscreen_worker=false` : url, said);
    try {
        const v = await video(page, mode, 45);
        res.check(`[${tag}] the stream plays`, Boolean(v), v);
        if (mode === 'webrtc') {
            // The hello named no 4:4:4, so the server settled it before the offer.
            res.check(`[${tag}] the server turned full color off before the offer`, await host.saw('streams 4:2:0', 20), (await host.log()).slice(-300));
            const toggle = await within(page.evaluate(() => window.video_fullcolor));
            res.check(`[${tag}] and the page's toggle shows it off`, toggle === false, toggle);
        } else {
            const kept = await stored(page);
            res.check(`[${tag}] the client turned the server's full color off`, kept === 'false', kept);
            res.check(`[${tag}] and said so once`, said.filter((t) => t.includes('full color (4:4:4) is off')).length === 1, said.slice(-3));
        }
        const i420 = await host.saw('Colorspace: I420', 20);
        const log = await host.log();
        res.check(`[${tag}] the server streams 4:2:0 on the same codec`, i420 && log.slice(-4000).includes('Mode: H264'), log.slice(-300));
        const encoder = await stored(page, 'encoder');
        res.check(`[${tag}] no ladder step to JPEG`, encoder !== 'jpeg', encoder);
    } finally {
        await browser.close();
    }
}

/** One engine: what its decoder answers, and what the client then does. */
async function drive({ res, url }: Ctx, engine: Engine, mode: string, vp9: boolean): Promise<void> {
    const tag = `${engine}-${mode}${vp9 ? '-vp9' : ''}`;
    const warnings: string[] = [];
    const [browser, page] = await open(engine, initScript(mode, vp9 ? 'vp9enc' : 'h264enc'), url, warnings);
    try {
        const decodable = vp9 && mode === 'webrtc'
            ? await within(page.evaluate<boolean>(RTP_VP9_PROBE_JS))
            : await probeDecoder(page, vp9 ? VP9_FULLCOLOR_CODEC : FULLCOLOR_CODEC);
        res.check(`[${tag}] the decoder answers the profile probe`, decodable !== null, decodable);
        const v = await video(page, mode, 45);
        res.check(`[${tag}] the stream plays with full color asked for`, Boolean(v), v);
        const kept = await stored(page);
        res.check(`[${tag}] full color survives exactly where it decodes`, (kept === 'true') === Boolean(decodable),
            `stored=${kept} decodable=${decodable === null ? 'None' : decodable ? 'True' : 'False'}`);
        const said = warnings.filter((w) => w.includes('full color (4:4:4) is off'));
        res.check(`[${tag}] turning it off is said once, and only then`, (said.length > 0) === !decodable, said.length ? said.slice(0, 1) : decodable);
        const refused = warnings.filter((w) => w.includes('config not supported'));
        res.check(`[${tag}] no decoder is left asking for a profile it lacks`, refused.length === 0, refused.slice(0, 1));
    } finally {
        await closeBrowser(browser);
    }
}

// One engine on one transport per run, the way the browser matrix is driven:
// a WebRTC session left behind by one engine is still winding down when the
// next connects, and the second sees no video for that alone.
const SELECTORS = ['ws-chromium', 'ws-firefox', 'ws-webkit', 'wr-chromium', 'wr-webkit',
    'ws-locked', 'ws-pinned', 'ws-stalled', 'wr-stalled',
    'ws-default', 'wr-default', 'ws-chromium-vp9', 'wr-chromium-vp9'];

for (const selector of SELECTORS) {
    test(selector, async () => {
        const dash = selector.indexOf('-');
        const short = selector.slice(0, dash);
        let engine = selector.slice(dash + 1);
        const vp9 = engine.endsWith('-vp9');
        if (vp9) engine = engine.slice(0, -4);
        const mode = short === 'wr' ? 'webrtc' : 'websockets';
        const res = new Checks(`full-color-${selector}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            const locked = engine === 'locked' || engine === 'pinned';
            let env: Record<string, string> = {};
            if (locked) {
                env = { SELKIES_VIDEO_FULLCOLOR: 'true|locked' };
                if (engine === 'pinned') env.SELKIES_ENCODER = 'h264enc';
                if (serverSoftwareEncoder(await host.serverBinary()) === 'openh264') env.SELKIES_USE_CPU = 'true';
            }
            if (engine === 'default') env = { SELKIES_VIDEO_FULLCOLOR: 'true' };
            if (vp9) env = { SELKIES_ENCODER: 'vp9enc,h264enc,jpeg' };
            const { url } = await host.start({ label: selector, mode, env });
            const c: Ctx = { res, host, url };
            try {
                if (locked) await driveLocked(c, engine === 'pinned');
                else if (engine === 'stalled') await driveStalled(c, mode);
                else if (engine === 'default') await driveDefault(c, mode === 'websockets' ? 'webkit' : 'chromium', mode);
                else await drive(c, engine === 'firefox' ? 'firefox' : engine === 'webkit' ? 'webkit' : 'chromium', mode, vp9);
            } finally {
                await host.stop();
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}
