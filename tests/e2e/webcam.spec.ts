/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The webcam uplink end to end: a browser's camera, switched on through the
// dashboard's pipeline control, must come out of the virtual V4L2 device on
// the server -- over the WebSocket (WebCodecs frames) and over WebRTC (the
// camera track on the reserved sendonly transceiver) -- and switch off again.
//
// Both engines capture the same camera: pixelflux's VirtualCamera published
// through the interposer, so the device's pixels are checked rather than just
// their presence and neither engine is measured on content the other never
// sees. Which codec an engine settles on is left to what it measured it could
// sustain and is never asserted -- that moves with the browser -- while the
// camera reaching the device at its own rate has to hold whatever was chosen.
// The operator lock (`webcam_enabled=false|locked`) must withhold the uplink
// on both transports.
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import type { Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host, runtimeDir } from '../lib/host.ts';
import { wireHookJs } from '../lib/page.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';
import {
    BLACK, build, CAMERA_FPS, camStatus, DETAIL_RATE_FLOOR, Device, frames, GREEN, launchCam, LEFT_HALF, near, PublishedCamera, RATE_FLOOR, RIGHT_HALF, sampleAt, samplesText, toggle, waitStatus,
    type CamPage, type Ycc,
} from '../lib/webcam.ts';

// A half turn stamped onto every uplink frame's flags byte, which is what a
// client on an engine that hands the camera's own orientation over sends.
const HALF_TURN_JS = wireHookJs(`
  if (data instanceof ArrayBuffer && data.byteLength > 3) {
    const b = new Uint8Array(data);
    if (b[0] === 0x06) b[2] = (b[2] & ~0x06) | (2 << 1);
  }
`);

// Every WebCodecs global removed, the way an engine without the API presents;
// the uplink then takes the JPEG rung and the screen arrives as striped JPEG.
const NO_WEBCODECS_JS = `
  (() => {
    for (const name of ['VideoDecoder', 'VideoEncoder', 'VideoFrame', 'EncodedVideoChunk', 'ImageDecoder']) {
      try { Object.defineProperty(window, name, { value: undefined, configurable: true, writable: true }); } catch (e) {}
    }
  })();
`;

// And OffscreenCanvas, as Safari before 16.4 and Firefox before 105 lack it:
// the JPEG rung then draws on a page canvas.
const NO_OFFSCREEN_JS = `
  (() => {
    try { Object.defineProperty(window, 'OffscreenCanvas', { value: undefined, configurable: true, writable: true }); } catch (e) {}
    try { delete HTMLCanvasElement.prototype.transferControlToOffscreen; } catch (e) {}
  })();
`;

// Keeps every peer connection the page opens reachable, so the camera sender
// can be steered to AV1 through its encoding's `codec` once the track is on.
const PC_JS = `
(() => {
  const Real = window.RTCPeerConnection;
  window.__pcs = [];
  window.RTCPeerConnection = new Proxy(Real, {
    construct(target, args) { const pc = Reflect.construct(target, args); window.__pcs.push(pc); return pc; },
  });
})();
`;
// A browser answering the server's offer sends with the offer's first codec
// whatever its own answer lists first, so the send codec is set on the sender
// itself, from the codecs the answer negotiated.
const SEND_AV1_JS = `(async () => {
  const out = [];
  for (const pc of window.__pcs) {
    for (const t of pc.getTransceivers()) {
      if (!(t.sender.track && t.sender.track.kind === 'video')) continue;
      const params = t.sender.getParameters();
      const av1 = (params.codecs || []).find((c) => /video\\/av1/i.test(c.mimeType));
      if (!av1) { out.push('no AV1 negotiated'); continue; }
      params.encodings[0].codec = { mimeType: av1.mimeType, clockRate: av1.clockRate, sdpFmtpLine: av1.sdpFmtpLine };
      try { await t.sender.setParameters(params); out.push('av1'); } catch (e) { out.push(String(e)); }
    }
  }
  return out;
})()`;
// The H.264 profiles of the offer's two video sections as the browser holds
// them: the display's own stream, and the webcam section it is asked to encode.
const OFFER_PROFILES_JS = `(() => {
  const out = {display: [], webcam: []};
  for (const section of window.__pcs[0].remoteDescription.sdp.split(/(?=^m=)/m)) {
    if (!section.startsWith('m=video')) continue;
    const key = section.includes('a=recvonly') ? 'webcam' : 'display';
    for (const m of section.matchAll(/profile-level-id=([0-9A-Fa-f]{6})/g)) out[key].push(m[1].toLowerCase());
  }
  return out;
})()`;
// Stops the transceiver the camera rides, the state a rejected webcam section
// leaves behind: its sender then refuses every track.
const STOP_WEBCAM_SENDER_JS = `(() => {
  const t = window.__pcs[0].getTransceivers().find((t) =>
    t.direction === 'sendonly' && t.receiver.track && t.receiver.track.kind === 'video');
  if (!t) return 'no webcam transceiver';
  t.stop();
  return 'stopped';
})()`;
const LIVE_VIDEO_SENDER_JS = "window.__pcs[0].getSenders().some((s) => s.track && s.track.kind === 'video')";

// Uplink codec ids, as the frame header carries them.
const CODEC_IDS: Record<string, number> = { mjpeg: 0, jpeg: 0, h264: 1, avc: 1, vp8: 2, vp9: 3, av1: 4, h265: 5 };
const WIRE_CODEC_JS = `window.__codecs = {};\n${wireHookJs(`
  if (data instanceof ArrayBuffer && data.byteLength > 3) {
    const b = new Uint8Array(data);
    if (b[0] === 0x06) window.__codecs[b[1]] = (window.__codecs[b[1]] || 0) + 1;
  }
`)}`;

// Long enough for the floor between the checks that ask whether anything still
// reads the camera to pass, plus a frame to act on the answer.
const REFORMAT_SETTLE_MS = 8000;

const CENTER = [640, 360] as const;
const BAR = [20, 20] as const;
const PICTURE: readonly (readonly [readonly [number, number], Ycc])[] = [[CENTER, GREEN], [BAR, BLACK]];

const r = (v: unknown): string => JSON.stringify(v);
const js = (page: Page, code: string): Promise<unknown> => within(page.evaluate(code));
const waitVideo = (page: Page, mode: string, timeoutS?: number): Promise<unknown> => (mode === 'websockets' ? waitWsVideo(page, timeoutS) : waitWrVideo(page, timeoutS));
const statusText = async (page: Page): Promise<string> => r(await camStatus(page));
const errorsText = (c: CamPage): string => c.errors.join('; ').slice(0, 200);
const env = (extra: Record<string, string>): Record<string, string> => ({ SELKIES_WEBCAM_ENABLED: 'false', ...extra });
const field = (p: { fields: Record<string, string> }, k: string): string => p.fields[k] ?? 'None';
const fps = (p: { fields: Record<string, string> }): number => Number(p.fields.fps ?? '0') || 0;
const framesText = (p: { rc: number; fields: Record<string, string> }): string => `rc=${p.rc} frames=${field(p, 'frames')} err=${p.fields.error ?? ''}`;

async function start(host: Host, label: string, mode: string, extra: Record<string, string>): Promise<{ url: string; device: Device }> {
    const { url, pid } = await host.start({ label, mode, env: extra });
    return { url, device: new Device(runtimeDir(pid)) };
}

/** Size, center RGB, and RGB at x=20 on the center row of a dumped JPEG, decoded in `page`. */
async function jpegCenter(page: Page, path: string): Promise<unknown> {
    let b64: string;
    try {
        b64 = readFileSync(path).toString('base64');
    } catch {
        return null;
    }
    return js(page, `(async () => {
      try {
        const bytes = Uint8Array.from(atob(${r(b64)}), (c) => c.charCodeAt(0));
        const bmp = await createImageBitmap(new Blob([bytes], { type: 'image/jpeg' }));
        // A page canvas, not an OffscreenCanvas: the page-canvas block takes
        // that global away from the page this runs in.
        const c = document.createElement('canvas');
        c.width = bmp.width;
        c.height = bmp.height;
        const g = c.getContext('2d');
        g.drawImage(bmp, 0, 0);
        const at = (x, y) => Array.from(g.getImageData(x, y, 1, 1).data.slice(0, 3));
        return [[bmp.width, bmp.height], at(bmp.width >> 1, bmp.height >> 1), at(20, bmp.height >> 1)];
      } catch (e) { return null; }
    })()`);
}

/**
 * Chromium with every WebCodecs global removed: the screen degrades to
 * striped JPEG, the camera goes up the JPEG rung, and the server's default
 * device format follows that uplink into an MJPEG device whose frames carry
 * the camera picture. Without `offscreen` OffscreenCanvas goes too, and the
 * rung draws on a page canvas.
 */
async function nowebcodecsBlock(host: Host, res: Checks, offscreen: boolean): Promise<void> {
    const cam = await new PublishedCamera(await frames({ kind: 'flat', color: GREEN })).start();
    const dump = join(cam.sockDir, 'frame.jpg');
    try {
        const { url, device } = await start(host, res.block, 'websockets', env({}));
        const c = await launchCam('chromium', url, cam.sockDir, 'websockets', NO_WEBCODECS_JS + (offscreen ? '' : NO_OFFSCREEN_JS));
        try {
            const { page } = c;
            const logs: string[] = [];
            page.on('console', (m) => { if (m.text().includes('[Webcam]')) logs.push(m.text()); });
            const video = await waitWsVideo(page, 30);
            res.check('screen stream up without WebCodecs', Boolean(video), r(video).slice(0, 100));
            const stored = await js(page, "localStorage.getItem((location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_') + '_encoder')");
            res.check('screen encoder pinned to jpeg', stored === 'jpeg', stored);
            await toggle(page, true);
            res.check('webcam reports active', await waitStatus(page, true), await statusText(page));
            for (const until = Date.now() + 25_000; Date.now() < until && (await device.probe(2, 1500)).rc !== 0;) await sleep(500);
            const p2 = await device.probe(30, 4000, [], dump);
            res.check('30 frames reach /dev/video0', p2.rc === 0 && p2.fields.frames === '30', `rc=${p2.rc} ${field(p2, 'frames')} ${p2.fields.error ?? ''}`);
            res.check('device follows the JPEG uplink: MJPEG at 1280x720', p2.fields.format === 'MJPG' && p2.fields.width === '1280' && p2.fields.height === '720', `${field(p2, 'format')} ${field(p2, 'width')}x${field(p2, 'height')}`);
            res.check('frames flow at camera rate', fps(p2) >= 12, p2.fields.fps ?? null);
            const px = await jpegCenter(page, dump);
            const ok = Array.isArray(px) && r(px[0]) === r([1280, 720]) && Array.isArray(px[1]) && Number(px[1][1]) > 200 && Number(px[1][0]) < 60 && Number(px[1][2]) < 60
                && Array.isArray(px[2]) && Math.max(...px[2].map(Number)) < 40;
            res.check('device frame is a 1280x720 JPEG of the green camera, pillarboxed', ok, r(px));
            res.check('client took the JPEG rung', logs.some((l) => l.includes('JPEG')), logs.join('; ').slice(0, 200));
            const drawn = offscreen ? 'OffscreenCanvas' : 'a page canvas';
            res.check(`drawn on ${drawn}`, logs.some((l) => l.includes(`encoder: JPEG through ${drawn}`)), logs.join('; ').slice(0, 200));
            await toggle(page, false);
            res.check('webcam reports inactive', await waitStatus(page, false), await statusText(page));
            res.check('no page errors', c.errors.length === 0, errorsText(c));
        } finally {
            await c.close();
        }
    } finally {
        await host.stop();
        await cam.stop();
    }
}

/**
 * The camera comes out of /dev/video0 over `mode` from both engines and stops
 * with the uplink. The device is pinned to I420 whatever the client sends: by
 * default its format follows the uplink, which would make the picture
 * readable only on some rungs, and which rung an engine takes is not this
 * block's to fix -- the reformat selector exercises that default. The
 * process-wide camera outlives whichever uplink brought it up and is decoded
 * into the same device on either transport.
 */
async function transportBlock(host: Host, res: Checks, mode: string): Promise<void> {
    const cam = await new PublishedCamera(await frames({ kind: 'flat', color: GREEN })).start();
    try {
        const { url, device } = await start(host, res.block, mode, env({ SELKIES_WEBCAM_PIXEL_FORMAT: 'I420' }));
        for (const engine of ['chromium', 'firefox'] as const) {
            const c = await launchCam(engine, url, cam.sockDir, mode);
            try {
                const { page } = c;
                const video = await waitVideo(page, mode);
                if (!video && engine === 'firefox' && mode === 'webrtc') {
                    // Playwright's Firefox ships no OpenH264, so the session's
                    // H.264 stream never plays there; the uplink cannot be
                    // driven without it.
                    res.skip('firefox: webrtc webcam uplink', 'no H.264 decode in this Firefox build (seed OpenH264 with tests/tools/fetch-openh264.sh)');
                    continue;
                }
                res.check(`${engine}: stream up`, Boolean(video), r(video).slice(0, 100));
                await toggle(page, true);
                res.check(`${engine}: webcam reports active`, await waitStatus(page, true), await statusText(page));
                const got = await device.waitForPicture(PICTURE);
                res.check(`${engine}: 30 frames reach /dev/video0`, got.rc === 0 && got.fields.frames === '30', framesText(got));
                res.check(`${engine}: device is the configured 1280x720 I420`, got.fields.format === 'YU12' && got.fields.width === '1280' && got.fields.height === '720', `${field(got, 'format')} ${field(got, 'width')}x${field(got, 'height')}`);
                res.check(`${engine}: frames flow at the camera's rate`, fps(got) >= CAMERA_FPS * RATE_FLOOR, `${got.fields.fps ?? 'None'} of ${CAMERA_FPS}`);
                // The 4:3 picture sits pillarboxed in the 16:9 device: center
                // green, the bar at x=20 black.
                res.check(`${engine}: center is the camera's green`, near(sampleAt(got, ...CENTER), GREEN), samplesText(got));
                res.check(`${engine}: pillarbox is black`, near(sampleAt(got, ...BAR), BLACK), samplesText(got));
                await toggle(page, false);
                res.check(`${engine}: webcam reports inactive`, await waitStatus(page, false), await statusText(page));
                // Frames already in flight (RTP jitter buffer, decoder queue)
                // land for a moment after the stop; the device must then go quiet.
                await sleep(1500);
                const after = await device.probe(10, 3000);
                res.check(`${engine}: no frames after disable`, after.rc !== 0 && (Number(after.fields.frames ?? '0') || 0) < 3, after.fields.frames ?? null);
                res.check(`${engine}: no page errors`, c.errors.length === 0, errorsText(c));
            } finally {
                await c.close();
            }
        }
    } finally {
        await host.stop();
        await cam.stop();
    }
}

/**
 * The camera comes out of /dev/video0 over WebRTC while the screen streams
 * 4:4:4: the High 4:4:4 profile the offer is rewritten to describes the
 * display's own stream, and the webcam section the browser encodes keeps the
 * profiles it can. A sender the browser has stopped leaves the camera
 * released, so the control reports the webcam off and a retry asks again.
 */
async function fullcolorBlock(host: Host, res: Checks): Promise<void> {
    const cam = await new PublishedCamera(await frames({ kind: 'flat', color: GREEN })).start();
    try {
        const { url, device } = await start(host, res.block, 'webrtc', env({ SELKIES_WEBCAM_PIXEL_FORMAT: 'I420', SELKIES_VIDEO_FULLCOLOR: 'true' }));
        const c = await launchCam('chromium', url, cam.sockDir, 'webrtc', PC_JS);
        try {
            const { page } = c;
            const logs: string[] = [];
            page.on('console', (m) => { if (m.text().includes('Webcam capture error')) logs.push(m.text()); });
            const video = await waitWrVideo(page);
            res.check('stream up', Boolean(video), r(video).slice(0, 100));
            const offered = await js(page, OFFER_PROFILES_JS);
            const list = (k: string): string[] => {
                const got: unknown = offered !== null && typeof offered === 'object' ? Reflect.get(offered, k) : undefined;
                return Array.isArray(got) ? got.map(String) : [];
            };
            res.check("the display's stream is offered as High 4:4:4", list('display').length > 0 && new Set(list('display')).size === 1 && list('display')[0] === 'f4001f', offered);
            res.check('the webcam section keeps the profiles the browser can encode', list('webcam').includes('42e01f'), offered);
            await toggle(page, true);
            res.check('webcam reports active', await waitStatus(page, true), await statusText(page));
            res.check('the uplink carries H.264', await host.saw('Webcam uplink carries h264.', 10), '');
            const got = await device.waitForPicture(PICTURE);
            res.check('30 frames reach /dev/video0', got.rc === 0 && got.fields.frames === '30', framesText(got));
            res.check("center is the camera's green", near(sampleAt(got, ...CENTER), GREEN), samplesText(got));
            await toggle(page, false);
            res.check('webcam reports inactive', await waitStatus(page, false), await statusText(page));
            res.check("the camera's transceiver is stopped", (await js(page, STOP_WEBCAM_SENDER_JS)) === 'stopped');
            for (const attempt of ['first', 'second']) {
                await js(page, 'window.__camStatus = []');
                await toggle(page, true);
                const stayedOff = !(await waitStatus(page, true, 6)) && r((await camStatus(page)).slice(-1)) === r([false]);
                res.check(`${attempt} attempt on a stopped sender: the webcam stays reported off`, stayedOff, await statusText(page));
            }
            res.check('the refused attempts left no camera track on a sender', (await js(page, LIVE_VIDEO_SENDER_JS)) === false);
            res.check('each refusal is logged, none escapes the page', logs.length === 2 && c.errors.length === 0, `${r(logs.slice(0, 2))} ${r(c.errors.slice(0, 2))}`);
        } finally {
            await c.close();
        }
    } finally {
        await host.stop();
        await cam.stop();
    }
}

/**
 * The camera comes out of /dev/video0 over WebRTC when the browser sends it
 * as AV1, steered onto that codec once its track is on: one OBU's fragments
 * span RTP packets, so the frame the virtual camera decodes is reassembled
 * from the frame's packets rather than joined.
 */
async function av1Block(host: Host, res: Checks): Promise<void> {
    const cam = await new PublishedCamera(await frames({ kind: 'flat', color: GREEN })).start();
    try {
        const { url, device } = await start(host, res.block, 'webrtc', env({ SELKIES_WEBCAM_PIXEL_FORMAT: 'I420' }));
        const c = await launchCam('chromium', url, cam.sockDir, 'webrtc', PC_JS);
        try {
            const { page } = c;
            const video = await waitWrVideo(page);
            res.check('stream up', Boolean(video), r(video).slice(0, 100));
            await toggle(page, true);
            res.check('webcam reports active', await waitStatus(page, true), await statusText(page));
            const switched = await js(page, SEND_AV1_JS);
            res.check('the camera sender takes AV1', r(switched) === r(['av1']), switched);
            res.check('the uplink carries AV1', await host.saw('Webcam uplink carries av1.', 10), '');
            const got = await device.waitForPicture(PICTURE);
            res.check('30 frames reach /dev/video0', got.rc === 0 && got.fields.frames === '30', framesText(got));
            res.check("frames flow at the camera's rate", fps(got) >= CAMERA_FPS * RATE_FLOOR, `${got.fields.fps ?? 'None'} of ${CAMERA_FPS}`);
            res.check("center is the camera's green", near(sampleAt(got, ...CENTER), GREEN), samplesText(got));
            res.check('pillarbox is black', near(sampleAt(got, ...BAR), BLACK), samplesText(got));
            await toggle(page, false);
            res.check('webcam reports inactive', await waitStatus(page, false), await statusText(page));
            res.check('no page errors', c.errors.length === 0, errorsText(c));
        } finally {
            await c.close();
        }
    } finally {
        await host.stop();
        await cam.stop();
    }
}

async function lockedBlock(host: Host, res: Checks): Promise<void> {
    const cam = await new PublishedCamera(await frames({ kind: 'flat', color: GREEN })).start();
    try {
        for (const mode of ['websockets', 'webrtc']) {
            const { url, device } = await start(host, `locked-${mode}`, mode, { SELKIES_WEBCAM_ENABLED: 'false|locked' });
            try {
                const c = await launchCam('chromium', url, cam.sockDir, mode);
                try {
                    const { page } = c;
                    res.check(`${mode}: stream up`, Boolean(await waitVideo(page, mode)));
                    await toggle(page, true);
                    await sleep(4000);
                    const got = await device.probe(3, 2500);
                    res.check(`${mode}: locked-off webcam feeds no frames`, (got.fields.frames ?? '0') === '0' || got.rc !== 0, got.fields.frames ?? null);
                    const seen = await camStatus(page);
                    res.check(`${mode}: client settles on webcam off`, seen.length === 0 || seen.at(-1) === false, r(seen));
                } finally {
                    await c.close();
                }
            } finally {
                await host.stop();
            }
        }
    } finally {
        await cam.stop();
    }
}

/**
 * What the flags byte says about a frame's orientation is what the device
 * shows. The client leaves the transform out of the bitstream and relays it,
 * so the picture is only upright if the server bakes it in: the same clip sent
 * with and without a half turn must come out of /dev/video0 mirrored against
 * each other.
 */
async function rotationBlock(host: Host, res: Checks): Promise<void> {
    const cam = await new PublishedCamera(await frames({ kind: 'split' })).start();
    // Well inside each half of the 4:3 picture as the server letterboxes it.
    const leftAt = [400, 360] as const;
    const rightAt = [880, 360] as const;
    try {
        // Pinned to JPEG so the frames pass through the page, which is where
        // the flags byte can be stamped: a video codec is encoded in a worker
        // and handed straight to the socket worker. The server's handling of
        // the flag is the subject, not the codec that carries it.
        const { url, device } = await start(host, res.block, 'websockets', env({ SELKIES_WEBCAM_PIXEL_FORMAT: 'I420', SELKIES_WEBCAM_ENCODER: 'mjpeg' }));
        for (const [label, initJs, want] of [['upright', '', [LEFT_HALF, RIGHT_HALF]], ['half turn', HALF_TURN_JS, [RIGHT_HALF, LEFT_HALF]]] as const) {
            const c = await launchCam('chromium', url, cam.sockDir, 'websockets', initJs);
            try {
                const { page } = c;
                res.check(`${label}: stream up`, Boolean(await waitWsVideo(page)));
                await toggle(page, true);
                res.check(`${label}: webcam reports active`, await waitStatus(page, true));
                const got = await device.waitForPicture([[leftAt, want[0]], [rightAt, want[1]]]);
                res.check(`${label}: frames reach /dev/video0`, got.rc === 0, got.fields.error ?? '');
                res.check(`${label}: left of the device picture`, near(sampleAt(got, ...leftAt), want[0]), `${r(sampleAt(got, ...leftAt) ?? null)} want ${r(want[0])}`);
                res.check(`${label}: right of the device picture`, near(sampleAt(got, ...rightAt), want[1]), `${r(sampleAt(got, ...rightAt) ?? null)} want ${r(want[1])}`);
                res.check(`${label}: no page errors`, c.errors.length === 0, errorsText(c));
                await toggle(page, false);
                await waitStatus(page, false);
            } finally {
                await c.close();
            }
            await sleep(1500);
        }
    } finally {
        await host.stop();
        await cam.stop();
    }
}

/**
 * The `auto` device follows the uplink that is actually there. The camera
 * outlives the session that started it, so an uplink of the other kind finds a
 * device it does not fit: a video uplink in an MJPEG device is decoded and
 * re-encoded per frame, an MJPEG uplink in a raw device decoded where it could
 * have passed through. It is re-created for the newcomer -- but only while
 * nothing reads it, which an attached interposer client proves by keeping the
 * device exactly as it was.
 */
async function reformatBlock(host: Host, res: Checks): Promise<void> {
    const cam = await new PublishedCamera(await frames({ kind: 'flat', color: GREEN })).start();
    let reader: ReturnType<Device['reader']> | null = null;
    try {
        const { url, device } = await start(host, res.block, 'websockets', env({}));
        let c = await launchCam('chromium', url, cam.sockDir, 'websockets');
        try {
            await waitWsVideo(c.page, 30);
            await toggle(c.page, true);
            await waitStatus(c.page, true);
            res.check('a video uplink brings the device up raw', await device.waitFormat('YU12'), await device.format());
        } finally {
            await c.close();
        }
        reader = device.reader();
        c = await launchCam('chromium', url, cam.sockDir, 'websockets', NO_WEBCODECS_JS);
        try {
            await waitWsVideo(c.page, 30);
            await toggle(c.page, true);
            await waitStatus(c.page, true);
            await sleep(REFORMAT_SETTLE_MS);
            res.check('a JPEG uplink does not re-create a device being read', (await device.format()) === 'YU12', await device.format());
            const held = reader;
            await new Promise<void>((resolve) => {
                held.once('exit', () => { resolve(); });
                held.kill('SIGTERM');
            });
            reader = null;
            res.check('and re-creates it once the reader is gone', await device.waitFormat('MJPG', 30), await device.format());
            await toggle(c.page, false);
            await waitStatus(c.page, false);
        } finally {
            await c.close();
        }
    } finally {
        reader?.kill('SIGTERM');
        await host.stop();
        await cam.stop();
    }
}

/**
 * An HD camera showing real detail, which is where the encoder ladder makes a
 * decision it can get wrong. Whichever rung an engine settles on has to carry
 * the camera at close to its own rate: a codec the engine cannot sustain
 * spends a core to deliver a fraction of the frames, and the device shows it.
 * Nothing here asserts which rung was taken, so an engine whose codecs get
 * faster keeps passing on the rung it earns.
 */
async function detailBlock(host: Host, res: Checks): Promise<void> {
    const cam = await new PublishedCamera(await frames({ kind: 'detail', count: 24 }, 1280, 720), 1280, 720).start();
    try {
        const { url, device } = await start(host, res.block, 'websockets', env({}));
        for (const engine of ['chromium', 'firefox'] as const) {
            const c = await launchCam(engine, url, cam.sockDir, 'websockets');
            try {
                const { page } = c;
                res.check(`${engine}: stream up`, Boolean(await waitWsVideo(page)), '');
                await toggle(page, true);
                res.check(`${engine}: webcam reports active`, await waitStatus(page, true), await statusText(page));
                for (const until = Date.now() + 30_000; Date.now() < until && (await device.probe(2, 1500)).rc !== 0;) await sleep(500);
                const got = await device.probe(60, 8000);
                res.check(`${engine}: the detailed camera reaches the device`, got.rc === 0 && got.fields.frames === '60', framesText(got));
                res.check(`${engine}: the chosen rung sustains the camera`, fps(got) >= CAMERA_FPS * DETAIL_RATE_FLOOR, `${got.fields.fps ?? 'None'} of ${CAMERA_FPS}`);
                res.check(`${engine}: no page errors`, c.errors.length === 0, errorsText(c));
                await toggle(page, false);
            } finally {
                await c.close();
            }
        }
    } finally {
        await host.stop();
        await cam.stop();
    }
}

/** The codec ids the page sent more than five frames of, or its own name for a codec it encodes in a worker. */
async function wireCodecs(page: Page): Promise<[Set<number>, string]> {
    const counts = await js(page, 'window.__codecs');
    const entries = counts !== null && typeof counts === 'object' ? Object.entries(counts) : [];
    const got = new Set(entries.filter(([, v]) => Number(v) > 5).map(([k]) => Number(k)));
    const text = entries.length ? r(Object.fromEntries(entries)) : '';
    if (got.size === 0) {
        // JPEG frames go out through the page, so their codec byte is on the
        // wire here; a video codec is encoded in a worker and handed to the
        // socket worker over a port, which the page never sees, so the
        // client's own name for it stands in.
        const named = String(await js(page, 'window.webcamCodec'));
        if (named in CODEC_IDS) got.add(CODEC_IDS[named]);
        return [got, text || named];
    }
    return [got, text];
}

/**
 * The `webcam_encoder` setting, proven by the uplink's codec bytes on the
 * engine whose rung it decides (Firefox, no MediaStreamTrackProcessor): the
 * default keeps it on MJPEG, an explicit codec re-opens the ladder with that
 * codec alone, and a forced codec never yields another codec's id -- though it
 * may degrade to JPEG (Firefox H.264 emits nothing through its plugin's cold
 * start). Every mode lands the same green on the device.
 */
async function encoderprefBlock(host: Host, res: Checks): Promise<void> {
    const cam = await new PublishedCamera(await frames({ kind: 'flat', color: GREEN })).start();
    try {
        const sorted = (s: Iterable<number>): number[] => [...s].sort((a, b) => a - b);
        for (const [pref, want, need] of [['auto', [0], 0], ['vp8', [2], 2], ['h264', [0, 1], null]] as const) {
            const extra: Record<string, string> = { SELKIES_WEBCAM_PIXEL_FORMAT: 'I420', ...(pref === 'auto' ? {} : { SELKIES_WEBCAM_ENCODER: pref }) };
            const { url, device } = await start(host, `pref-${pref}`, 'websockets', env(extra));
            try {
                const c = await launchCam('firefox', url, cam.sockDir, 'websockets', WIRE_CODEC_JS);
                try {
                    const { page } = c;
                    res.check(`${pref}: stream up`, Boolean(await waitWsVideo(page)), '');
                    await toggle(page, true);
                    res.check(`${pref}: webcam reports active`, await waitStatus(page, true), await statusText(page));
                    const got = await device.waitForPicture([[CENTER, GREEN]]);
                    res.check(`${pref}: device shows the camera's green`, near(sampleAt(got, ...CENTER), GREEN), samplesText(got));
                    const [codecs, text] = await wireCodecs(page);
                    res.check(`${pref}: wire codecs within [${[...want].sort((x, y) => x - y).join(', ')}]`, codecs.size > 0 && [...codecs].every((x) => (want as readonly number[]).includes(x)) && (need === null || codecs.has(need)), text);
                    res.check(`${pref}: no page errors`, c.errors.length === 0, errorsText(c));
                } finally {
                    await c.close();
                }
            } finally {
                await host.stop();
            }
        }
        // The codecs past VP8 come from Chromium's own WebCodecs encoders, so
        // a pin to one of them must come out of the device as that codec's id alone.
        for (const [pref, want] of [['vp9', [3]], ['av1', [4]]] as const) {
            const { url, device } = await start(host, `pref-chromium-${pref}`, 'websockets', env({ SELKIES_WEBCAM_PIXEL_FORMAT: 'I420', SELKIES_WEBCAM_ENCODER: pref }));
            try {
                const c = await launchCam('chromium', url, cam.sockDir, 'websockets', WIRE_CODEC_JS);
                try {
                    const { page } = c;
                    res.check(`chromium ${pref}: stream up`, Boolean(await waitWsVideo(page)), '');
                    await toggle(page, true);
                    res.check(`chromium ${pref}: webcam reports active`, await waitStatus(page, true), await statusText(page));
                    const got = await device.waitForPicture([[CENTER, GREEN]]);
                    res.check(`chromium ${pref}: device shows the camera's green`, near(sampleAt(got, ...CENTER), GREEN), samplesText(got));
                    const [codecs, text] = await wireCodecs(page);
                    res.check(`chromium ${pref}: wire codec is ${r(want)}`, r(sorted(codecs)) === r(want), text);
                    res.check(`chromium ${pref}: no page errors`, c.errors.length === 0, errorsText(c));
                } finally {
                    await c.close();
                }
            } finally {
                await host.stop();
            }
        }
    } finally {
        await cam.stop();
    }
}

/** `webcam_encoder` over WebRTC: the camera sender is set to the named codec among the ones the answer negotiated, so the server's uplink carries it. */
async function webrtcprefBlock(host: Host, res: Checks): Promise<void> {
    const cam = await new PublishedCamera(await frames({ kind: 'flat', color: GREEN })).start();
    try {
        for (const pref of ['vp9', 'av1']) {
            const { url, device } = await start(host, `wrpref-${pref}`, 'webrtc', env({ SELKIES_WEBCAM_PIXEL_FORMAT: 'I420', SELKIES_WEBCAM_ENCODER: pref }));
            try {
                const c = await launchCam('chromium', url, cam.sockDir, 'webrtc');
                try {
                    const { page } = c;
                    res.check(`${pref}: stream up`, Boolean(await waitWrVideo(page)), '');
                    await toggle(page, true);
                    res.check(`${pref}: webcam reports active`, await waitStatus(page, true), await statusText(page));
                    res.check(`${pref}: the uplink carries ${pref}`, await host.saw(`Webcam uplink carries ${pref}.`, 10), '');
                    const got = await device.waitForPicture([[CENTER, GREEN]]);
                    res.check(`${pref}: device shows the camera's green`, near(sampleAt(got, ...CENTER), GREEN), samplesText(got));
                    res.check(`${pref}: no page errors`, c.errors.length === 0, errorsText(c));
                } finally {
                    await c.close();
                }
            } finally {
                await host.stop();
            }
        }
    } finally {
        await cam.stop();
    }
}

/**
 * The page is asked for its camera only while something in the session reads
 * the device. On both transports and every engine: an idle session is never
 * asked, an application opening the device has the page asked and, where the
 * engine captures the published camera (Chromium and Firefox; Playwright's
 * WebKit captures from its own mock device), is served frames from it, and
 * closing the device releases the camera inside the hold-off. The reader is
 * the probe, so what turns the camera on is an application opening the device.
 */
async function ondemandBlock(host: Host, res: Checks): Promise<void> {
    const cam = await new PublishedCamera(await frames({ kind: 'flat', color: GREEN })).start();
    try {
        for (const mode of ['websockets', 'webrtc']) {
            const { url, device } = await start(host, `ondemand-${mode}`, mode, { SELKIES_WEBCAM_ENABLED: 'true', SELKIES_WEBCAM_ON_START: 'demand', SELKIES_WEBCAM_PIXEL_FORMAT: 'I420' });
            try {
                for (const engine of ['chromium', 'firefox', 'webkit'] as Engine[]) {
                    const c = await launchCam(engine, url, cam.sockDir, mode);
                    try {
                        const { page } = c;
                        const video = await waitVideo(page, mode);
                        if (!video && engine === 'firefox' && mode === 'webrtc') {
                            res.skip(`${engine}/${mode}: on-demand camera`, 'no H.264 decode in this Firefox build (seed OpenH264 with tests/tools/fetch-openh264.sh)');
                            continue;
                        }
                        res.check(`${engine}/${mode}: stream up`, Boolean(video), r(video).slice(0, 100));
                        await sleep(6000);
                        const idle = await camStatus(page);
                        res.check(`${engine}/${mode}: an idle session is never asked`, !idle.some(Boolean), r(idle));
                        const started = Date.now();
                        if (engine === 'webkit') {
                            const reader = device.reader();
                            try {
                                res.check(`${engine}/${mode}: an application opening the device has the page asked`, await waitStatus(page, true), await statusText(page));
                            } finally {
                                await new Promise<void>((resolve) => {
                                    reader.once('exit', () => { resolve(); });
                                    reader.kill('SIGTERM');
                                });
                            }
                        } else {
                            const got = await device.probe(30, 20_000);
                            res.check(`${engine}/${mode}: an application opening the device is served 30 frames`, got.rc === 0 && got.fields.frames === '30', framesText(got));
                            process.stdout.write(`  ${engine}/${mode}: 30 frames in ${((Date.now() - started) / 1000).toFixed(1)}s from the open\n`);
                            res.check(`${engine}/${mode}: the page reported the capture`, (await camStatus(page)).includes(true));
                        }
                        res.check(`${engine}/${mode}: closing the device releases the camera`, await waitStatus(page, false, 25), await statusText(page));
                        res.check(`${engine}/${mode}: no page errors`, c.errors.length === 0, errorsText(c));
                    } finally {
                        await c.close();
                    }
                }
            } finally {
                await host.stop();
            }
        }
    } finally {
        await cam.stop();
    }
}

const BLOCKS: Record<string, [string, (host: Host, res: Checks) => Promise<void>]> = {
    websockets: ['webcam-websockets', (h, res) => transportBlock(h, res, 'websockets')],
    webrtc: ['webcam-webrtc', (h, res) => transportBlock(h, res, 'webrtc')],
    ondemand: ['webcam-ondemand', ondemandBlock],
    av1: ['webcam-av1', av1Block],
    webrtcpref: ['webcam-webrtcpref', webrtcprefBlock],
    locked: ['webcam-locked', lockedBlock],
    nowebcodecs: ['webcam-nowebcodecs', (h, res) => nowebcodecsBlock(h, res, true)],
    'nowebcodecs-canvas': ['webcam-nowebcodecs-canvas', (h, res) => nowebcodecsBlock(h, res, false)],
    rotation: ['webcam-rotation', rotationBlock],
    reformat: ['webcam-reformat', reformatBlock],
    detail: ['webcam-detail', detailBlock],
    encoderpref: ['webcam-encoderpref', encoderprefBlock],
    fullcolor: ['webcam-fullcolor', fullcolorBlock],
};

for (const [which, [block, run]] of Object.entries(BLOCKS)) {
    test(which, async () => {
        build();
        const res = new Checks(block);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            await run(host, res);
        } finally {
            await host.close();
        }
        res.finish();
    });
}
