/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Helpers shared by the streaming cores and the dashboard: a small FIFO
 * queue, the human-readable labels for wire values, the decodability checks
 * that keep a client from asking for a stream it cannot play, the route
 * prefix and localStorage namespace every caller derives the same way, and
 * whether the server answers at all before a page reloads into it.
 * @module
 */

import { H264_ANNEXB_SAMPLE, PROBE_CODEC_STRINGS, PROBE_FULLCOLOR_STRINGS, TEN_BIT_SAMPLES, codecOfEncoder, codecCarriesFullColor, codecCarriesTenBit, codecStringFor } from './wire-codecs.ts';
export { codecOfEncoder, codecCarriesFullColor, codecCarriesTenBit };

/** A FIFO queue over an array. */
export class Queue<T> {
    items: T[] = [];

    /** @param elements Initial items, enqueued in order. */
    constructor(...elements: T[]) {
        this.enqueue(...elements);
    }

    /** @param elements Items appended in order. */
    enqueue(...elements: T[]): void {
        elements.forEach(element => this.items.push(element));
    }

    /**
     * Removes the oldest `count` items.
     * @param count How many items to drop.
     * @returns The oldest of the removed items.
     */
    dequeue(count = 1): T | undefined {
        return this.items.splice(0, count)[0];
    }

    /** @returns */
    size(): number {
        return this.items.length;
    }

    /** @returns */
    isEmpty(): boolean {
        return this.items.length===0;
    }

    /** @returns A copy of the items, oldest first. */
    toArray(): T[] {
        return [...this.items]
    }

    /** Removes the first occurrence of `element`. */
    remove(element: T): void {
        const index = this.items.indexOf(element)
        this.items.splice(index, 1)
    }

    /** @returns Whether `element` is queued. */
    find(element: T): boolean {
        return this.items.indexOf(element) == -1 ? false: true;
    }

    /** Drops every item. */
    clear(): void {
        this.items.length = 0;
    }
}

/**
 * Human-readable names for the wire values surfaced in UIs (transport modes,
 * encoders, rate-control modes). The raw values are what the server APIs
 * speak and stay untouched; unknown values fall through unchanged so new wire
 * values render as-is. Locale-invariant technical terms, so they live here
 * once rather than in every dashboard's translation dictionaries.
 */
export const DISPLAY_LABELS: Record<string, string> = {
    websockets: "WebSockets",
    webrtc: "WebRTC",
    h264enc: "H.264",
    h265enc: "H.265",
    vp8enc: "VP8",
    vp9enc: "VP9",
    av1enc: "AV1",
    jpeg: "JPEG (Striped)",
    cbr: "CBR (Constant Bitrate)",
    crf: "CRF (Constant Quality)",
    auto: "Auto",
    h264: "H.264",
    h265: "H.265",
    vp8: "VP8",
    vp9: "VP9",
    av1: "AV1",
    mjpeg: "MJPEG",
};

/**
 * @param value A wire value.
 * @returns Its display label, or the value itself when it has none.
 */
export const displayLabel = (value: string): string => DISPLAY_LABELS[value] ?? value;

/**
 * How long an engine is given to answer a decoder probe, and a page's first
 * stream decoder configuration before the core's no-output watchdog counts
 * the wait. A decoder that stalls instead of answering must not hold
 * the session up: the answer settles one setting, while a session waits on it
 * to start. Measured answers are immediate, the first in a page costing a
 * media stack its warm-up.
 */
export const DECODER_PROBE_TIMEOUT_MS = 10000;

/**
 * Whether a `VideoDecoder` here accepts `codec` at `width` x `height`. False
 * without WebCodecs, and when the engine throws on the question; null when it
 * gives no answer within `DECODER_PROBE_TIMEOUT_MS`, which is not a refusal.
 * @param codec A WebCodecs codec string.
 * @param width
 * @param height
 * @returns
 */
async function decoderAccepts(codec: string, width: number, height: number): Promise<boolean | null> {
    if (typeof VideoDecoder === "undefined") return false;
    for (const accel of [undefined, "prefer-software"] as Array<string | undefined>) {
        try {
            const config = { codec, codedWidth: width, codedHeight: height } as VideoDecoderConfig;
            if (accel) config.hardwareAcceleration = accel as NonNullable<VideoDecoderConfig["hardwareAcceleration"]>;
            const support = await Promise.race([
                VideoDecoder.isConfigSupported(config),
                new Promise<undefined>((resolve) => setTimeout(resolve, DECODER_PROBE_TIMEOUT_MS)),
            ]) as VideoDecoderSupport | undefined;
            if (support && support.supported) return true;
            if (support === undefined) return null;
        } catch (err) {
            // Fall through to the next acceleration preference.
        }
    }
    return false;
}

/**
 * What this engine's `VideoDecoder` said to each codec's representative
 * configuration, once the probe has run; `null` until then. A decoder can
 * accept a configuration and still fail at `decode()`, which the core's
 * fallback ladder answers; this only keeps the settings from offering what the
 * engine refuses outright, so a codec it gave no answer on (`null`) stays
 * offered, as every codec is before the probe has run.
 */
let decoderSupport: Record<string, boolean | null> | null = null;

/**
 * Resolves once every codec has been asked of the decoder. Menus built before
 * it resolves offer every codec an engine with WebCodecs might play, and are
 * rebuilt from the answer.
 */
export const decoderSupportReady: Promise<Record<string, boolean | null>> = (async () => {
    const answers: Record<string, boolean | null> = {};
    for (const [codec, string] of Object.entries(PROBE_CODEC_STRINGS)) {
        answers[codec] = await decoderAccepts(string, 1280, 720);
    }
    decoderSupport = answers;
    return answers;
})();

/**
 * How this engine's `VideoDecoder` takes H.264: `annexb` where a key frame in
 * Annex B form decodes (Chromium, Firefox), `avcc` where it is refused and an
 * `avcC` description with length-prefixed NAL units is needed instead (WebKit).
 * Asked once, of a 16x16 key frame, in a worker so a slow refusal never holds
 * the page; `annexb` without WebCodecs or when nothing answers.
 */
let h264FramingAnswer = "annexb";
export const h264FramingReady: Promise<string> = (async () => {
    if (typeof VideoDecoder === "undefined") return "annexb";
    const probe = `self.onmessage = async (e) => {
        let outputs = 0;
        try {
            const dec = new VideoDecoder({ output: (f) => { outputs++; f.close(); }, error: () => {} });
            dec.configure({ codec: 'avc1.42000A', codedWidth: 16, codedHeight: 16, optimizeForLatency: true });
            dec.decode(new EncodedVideoChunk({ type: 'key', timestamp: 0, data: e.data }));
            await dec.flush();
            dec.close();
        } catch (err) { /* refused: the count says so */ }
        self.postMessage(outputs > 0 ? 'annexb' : 'avcc');
    };`;
    const sample = Uint8Array.from(atob(H264_ANNEXB_SAMPLE), (c) => c.charCodeAt(0));
    const answer = await new Promise<string>((resolve) => {
        let worker: Worker | null = null;
        const settle = (value: string): void => { resolve(value); if (worker) { try { worker.terminate(); } catch (err) { /* gone */ } } };
        const timer = setTimeout(() => settle("annexb"), 5000);
        try {
            worker = new Worker(URL.createObjectURL(new Blob([probe], { type: "text/javascript" })));
            worker.onmessage = (e: MessageEvent) => { clearTimeout(timer); settle(e.data as string); };
            worker.onerror = () => { clearTimeout(timer); settle("annexb"); };
            worker.postMessage(sample.buffer, [sample.buffer as ArrayBuffer]);
        } catch (err) {
            clearTimeout(timer);
            settle("annexb");
        }
    });
    h264FramingAnswer = answer;
    return answer;
})();
/** @returns The framing `h264FramingReady` settled on; `annexb` until it has. */
export const h264Framing = (): string => h264FramingAnswer;

/**
 * Whether this engine can play an encoder on the WebSocket transport: every
 * video mode decodes through WebCodecs' `VideoDecoder`, whose answer per codec
 * is read once it is in, while `jpeg` (striped JPEG painted through
 * `createImageBitmap`) needs nothing. An engine without WebCodecs therefore
 * still streams: the core's pre-flight pins `jpeg` instead of failing, and the
 * settings offer nothing it cannot play. The WebRTC transport decodes in the
 * browser's media stack and is not subject to this.
 * @param encoder An encoder wire value.
 * @returns
 */
export const canDecodeEncoder = (encoder: string): boolean => {
    if (encoder === "jpeg") return true;
    if (typeof VideoDecoder === "undefined") return false;
    return decoderSupport === null || decoderSupport[codecOfEncoder(encoder)] !== false;
};

/** RTP MIME type each WebRTC encoder streams. */
const RTC_MIME_TYPES: Record<string, string> = {
    h264enc: "video/h264", h265enc: "video/h265", vp8enc: "video/vp8", vp9enc: "video/vp9", av1enc: "video/av1",
};

/**
 * Whether this engine's own WebRTC stack receives an encoder's codec, from
 * `RTCRtpReceiver.getCapabilities`; an engine that cannot answer the question
 * is given the benefit of the doubt, and the server's negotiation fallback
 * answers a codec it declines.
 * @param encoder An encoder wire value.
 * @returns
 */
export const canReceiveEncoder = (encoder: string): boolean => {
    const mime = RTC_MIME_TYPES[encoder];
    if (!mime) return false;
    try {
        const caps = RTCRtpReceiver.getCapabilities("video");
        if (!caps || !Array.isArray(caps.codecs)) return true;
        return caps.codecs.some((c) => typeof c.mimeType === "string" && c.mimeType.toLowerCase() === mime);
    } catch (err) {
        return true;
    }
};
/**
 * Whether this engine plays an encoder on a transport: `canDecodeEncoder` on
 * WebSockets, `canReceiveEncoder` on WebRTC. A menu lists what it cannot play
 * disabled and marked, so a missing option reads as the browser's limit and
 * never as the server's.
 * @param encoder An encoder wire value.
 * @param webrtc Whether the transport is WebRTC.
 * @returns
 */
export const canPlayEncoder = (encoder: string, webrtc: boolean): boolean => (webrtc ? canReceiveEncoder(encoder) : canDecodeEncoder(encoder));

/** Cached answers of `canDecodeFullColor`, one probe per codec. */
const fullColorProbes: Record<string, Promise<boolean>> = {};

/**
 * Whether this engine's `VideoDecoder` will take `codec` at full color (4:4:4).
 *
 * Engines differ on the 4:4:4 profiles (High 4:4:4 Predictive, HEVC RExt, VP9 profile 1), and
 * one whose decoder lacks them cannot show the stream at all rather than
 * showing it worse, so full color is asked of the decoder before it is asked
 * of the server. Which engines have them changes with their releases, which is
 * why this probes instead of naming them. The profile is the whole question,
 * so it is asked with the constraint bits the encoders emit, at the smallest
 * frame the level allows, since a level is a ceiling and a size beyond it is a
 * pair an implementation may reject on its own. What a real stream is decoded
 * with comes from its key frame's parameter sets instead.
 * @param codec The codec name, `h264` by default.
 * @returns False for a codec without 4:4:4, and where there is no `VideoDecoder` at all.
 */
export function canDecodeFullColor(codec = "h264"): Promise<boolean> {
    const string = PROBE_FULLCOLOR_STRINGS[codec];
    if (!string) return Promise.resolve(false);
    if (!fullColorProbes[codec]) {
        fullColorProbes[codec] = decoderAccepts(string, 320, 240).then((answer) => {
            const ok = answer === true;
            fullColorAnswers[codec] = ok;
            return ok;
        });
    }
    return fullColorProbes[codec];
}

/** The answers `canDecodeFullColor` has settled, by codec. */
const fullColorAnswers: Record<string, boolean> = {};

/**
 * What `canDecodeFullColor` answered for `codec`, for a decision that cannot
 * wait on the probe: `undefined` while it is still out or was never asked.
 * @param codec The codec name.
 * @returns
 */
export const fullColorDecoded = (codec: string): boolean | undefined => fullColorAnswers[codec];

/** The decodes `canDecodeTenBit` has asked for, by format. */
const tenBitProbes: Record<string, Promise<boolean>> = {};
/** The answers `canDecodeTenBit` has settled, by format. */
const tenBitAnswers: Record<string, boolean> = {};

/**
 * The name a 10-bit format goes by in `TEN_BIT_SAMPLES`, in `tenBitDecoded`, and
 * in a WebRTC hello: the codec name for 4:2:0, with `444` after it for 4:4:4.
 * @param codec The codec name.
 * @param fullcolor Whether the format is 4:4:4.
 * @returns
 */
export const tenBitFormat = (codec: string, fullcolor = false): string => `${codec}${fullcolor ? "444" : ""}`;

/**
 * Whether this engine's `VideoDecoder` shows a picture of `codec` at 10 bits.
 *
 * The decoder is handed a real 10-bit key frame (`TEN_BIT_SAMPLES`) and has to
 * give a frame back, configured as a stream's decoder is, from the key frame's
 * own codec string: engines accept 10-bit configurations they then fail to
 * decode (Firefox and VP9 profile 2), and whether a profile decodes turns on
 * the client's hardware where the engine has no software decoder for the codec
 * (H.265 everywhere), so nothing short of a decode answers it.
 * @param codec The codec name.
 * @param fullcolor Whether to ask for the codec's 4:4:4 at 10 bits.
 * @returns False for a format without a sample, where there is no `VideoDecoder`,
 *     and where no frame comes back within `DECODER_PROBE_TIMEOUT_MS`.
 */
export function canDecodeTenBit(codec: string, fullcolor = false): Promise<boolean> {
    const format = tenBitFormat(codec, fullcolor);
    const sample = TEN_BIT_SAMPLES[format];
    if (!sample || typeof VideoDecoder === "undefined") return Promise.resolve(false);
    if (!tenBitProbes[format]) {
        tenBitProbes[format] = (async (): Promise<boolean> => {
            const data = Uint8Array.from(atob(sample), (c) => c.charCodeAt(0));
            let outputs = 0;
            let decoder: VideoDecoder | null = null;
            try {
                decoder = new VideoDecoder({ output: (frame) => { outputs++; frame.close(); }, error: () => {} });
                decoder.configure({
                    codec: codecStringFor(codec, data, 320, 240, 30, fullcolor, true),
                    codedWidth: 320, codedHeight: 240, optimizeForLatency: true,
                });
                decoder.decode(new EncodedVideoChunk({ type: "key", timestamp: 0, data }));
                await Promise.race([
                    decoder.flush(),
                    new Promise<void>((resolve) => setTimeout(resolve, DECODER_PROBE_TIMEOUT_MS)),
                ]);
            } catch (err) {
                // Refused: the count says so.
            }
            try { if (decoder && decoder.state !== "closed") decoder.close(); } catch (err) { /* closed by its error */ }
            tenBitAnswers[format] = outputs > 0;
            return outputs > 0;
        })();
    }
    return tenBitProbes[format];
}

/**
 * What `canDecodeTenBit` answered, for a decision that cannot wait on the
 * decode: `undefined` while it is still out or was never asked.
 * @param codec The codec name.
 * @param fullcolor Whether the format is 4:4:4.
 * @returns
 */
export const tenBitDecoded = (codec: string, fullcolor = false): boolean | undefined => tenBitAnswers[tenBitFormat(codec, fullcolor)];

/**
 * Directory this document is served from, without a trailing slash (`''` at
 * the server root). Every request the client builds hangs off it, so a
 * deployment reverse-proxied under a subfolder reaches its own routes, and an
 * iframed client reads its own path instead of the frame's.
 * @returns The path prefix, e.g. `/desk`.
 */
export function getRoutePrefix(): string {
    const pathname = window.location.pathname;
    const dirPath = pathname.substring(0, pathname.lastIndexOf('/') + 1);
    return dirPath.replace(/\/$/, '');
}

/** How long `serverAnswers` and `entryPageTag` wait for an answer. */
const SERVER_ANSWER_MS = 5000;

/**
 * The server's answer to a plain GET on one of its endpoints, or null where it
 * gave none: nothing answered within SERVER_ANSWER_MS (a path that swallows
 * packets holds a fetch open for minutes), or the answer says it is not up
 * (502, 503, 504: a gateway in front of a stopped server, or the server itself
 * still starting). A page reloaded into a server stopping or starting lands on
 * an error page with nothing left to retry, so a core reloads only once this
 * returns an answer.
 * @param url
 * @param headers
 * @returns
 */
export async function serverAnswers(url: string, headers?: Record<string, string>): Promise<Response | null> {
    try {
        const signal = typeof AbortSignal.timeout === 'function' ? AbortSignal.timeout(SERVER_ANSWER_MS) : undefined;
        const res = await fetch(url, { cache: 'no-store', headers, signal });
        return [502, 503, 504].includes(res.status) ? null : res;
    } catch (e) {
        return null;
    }
}

/**
 * The validators (`ETag`, `Last-Modified`) the server answers for the page's
 * own entry document now: two answers differ when a new build is served.
 * @param headers Credentials the page's API calls carry.
 * @returns Null when the server does not answer.
 */
export async function entryPageTag(headers?: Record<string, string>): Promise<string | null> {
    try {
        const signal = typeof AbortSignal.timeout === 'function' ? AbortSignal.timeout(SERVER_ANSWER_MS) : undefined;
        const res = await fetch(window.location.href, { method: 'HEAD', cache: 'no-store', headers, signal });
        if (!res.ok) return null;
        return `${res.headers.get('etag') || ''}|${res.headers.get('last-modified') || ''}`;
    } catch (e) {
        return null;
    }
}

/**
 * The localStorage namespace every stored key is prefixed with.
 *
 * Origin and pathname only, not the full URL: a per-session `?token=` must
 * not mint a new namespace on each connect. Cores and dashboards share one
 * prefix, so this derivation is the single one they all call.
 * @returns Sanitized namespace, empty outside a browser.
 */
export function getStorageAppName(): string {
    if (typeof window === 'undefined') return '';
    const urlForKey = window.location.origin + window.location.pathname;
    return urlForKey.replace(/[^a-zA-Z0-9._-]/g, '_');
}

/**
 * macOS proper, a narrower question than the Mac family `navigator.platform`
 * names: an iPad in its desktop-class default reports `MacIntel` like a Mac
 * does, and its touch points are what tell the two apart. `navigator.platform`
 * decides and the client hint is only a fallback for an engine that has
 * dropped the deprecated field, since the question is which pointer
 * acceleration curve the OS applies, not which brand the browser reports.
 * @returns
 */
export function isMacDesktop(): boolean {
    if (typeof navigator === 'undefined') return false;
    const platform = navigator.platform
        || ((navigator as Navigator & { userAgentData?: { platform?: string } }).userAgentData?.platform) || '';
    return /^mac/i.test(platform) && (navigator.maxTouchPoints || 0) <= 1;
}

/**
 * This browser tab's id, kept in its sessionStorage: a reload of the page keeps
 * it and another tab has its own, which is how the server tells a page taking
 * its own display back from another page joining it.
 * @returns Null where the tab has no storage.
 */
export function pageTabId(): string | null {
  const key = 'selkies_tab_id';
  try {
    let id = window.sessionStorage.getItem(key);
    if (!id) {
      id = (window.crypto && window.crypto.randomUUID) ? window.crypto.randomUUID()
        : `${Date.now().toString(36)}${Math.random().toString(36).slice(2)}`;
      window.sessionStorage.setItem(key, id);
    }
    return id;
  } catch (e) {
    return null;
  }
}

/** How long a remembered start rate stands: a path's capacity a day later is anyone's guess. */
const CC_START_TTL_MS = 24 * 60 * 60 * 1000;

/** The localStorage key of a display's remembered start rate, per origin as every key is. */
function ccStartKey(displayId: string | null | undefined): string {
  return `${getStorageAppName()}_cc_start${displayId && displayId !== 'primary' ? `_${displayId}` : ''}`;
}

/**
 * Keep the rate congestion control held a display at, as its server reports it, so a
 * restarted server starts the page's next stream there rather than at the configured rate.
 * @param displayId
 * @param kbps
 */
export function rememberCcStart(displayId: string | null | undefined, kbps: number): void {
  if (!Number.isFinite(kbps) || kbps <= 0) return;
  try {
    window.localStorage.setItem(ccStartKey(displayId), JSON.stringify({ kbps: Math.round(kbps), at: Date.now() }));
  } catch (e) { /* no storage: every start is the configured rate */ }
}

/** A remembered start rate. */
interface CcStart {
  kbps: number;
  at: number;
}

/**
 * The display's remembered start rate, or null: none kept, a day old, or unreadable.
 * @param displayId
 * @returns
 */
export function recalledCcStart(displayId: string | null | undefined): number | null {
  try {
    const kept = JSON.parse(window.localStorage.getItem(ccStartKey(displayId)) || 'null') as CcStart | null;
    if (kept && Number.isFinite(kept.kbps) && kept.kbps > 0 && Date.now() - kept.at < CC_START_TTL_MS) {
      return kept.kbps;
    }
  } catch (e) { /* unreadable reads as none */ }
  return null;
}

/**
 * Drop the display's remembered start rate, once the user sets a bitrate of their own.
 * @param displayId
 */
export function forgetCcStart(displayId: string | null | undefined): void {
  try {
    window.localStorage.removeItem(ccStartKey(displayId));
  } catch (e) { /* nothing kept */ }
}

/**
 * WebKit's Linux ports (WPE, WebKitGTK), which draw a canvas through Skia's GPU
 * context, as against Safari, which draws through CoreGraphics. No capability
 * tells the two apart and WebKit on Linux can carry Safari's macOS user agent,
 * so the engine is read from the user agent and the OS from the platform, both
 * of which a worker has too.
 * @param userAgent `navigator.userAgent`.
 * @param platform `navigator.platform`.
 * @returns
 */
export function isSkiaWebKit(userAgent: string | null | undefined, platform: string | null | undefined): boolean {
    return /AppleWebKit\//.test(userAgent || '') && !/Chrom(e|ium)\/|Edg\/|OPR\//.test(userAgent || '')
        && /^Linux/.test(platform || '');
}

/**
 * Whether the client is touch-first: its primary pointer is coarse. True on
 * phones and tablets, false on desktops -- including touch-screen laptops,
 * whose primary pointer is still the pointing device. The form factor is
 * fixed for the life of the document, so it is resolved once and available
 * to every first render; a touch-capable device this misses is still caught
 * by the first touchstart.
 */
export const isMobileClient: boolean =
    typeof window !== 'undefined' &&
    typeof window.matchMedia === 'function' &&
    window.matchMedia('(pointer: coarse)').matches;

/**
 * Whether a capture the server asked for failed for good: permission withheld, or no
 * getUserMedia at all. A busy camera or an unmet constraint may clear by the next demand.
 *
 * @param error The rejection from getUserMedia, or the capture's own error.
 * @returns True where asking again could not succeed.
 */
export function isCaptureRefusal(error: unknown): boolean {
	const name = error && (error as { name?: string }).name;
	return name === 'NotAllowedError' || name === 'SecurityError' || name === 'NotSupportedError';
}
