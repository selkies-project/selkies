/**
 * Conditional settings: settings whose default depends on other state (HiDPI
 * defers to whether a manual resolution is set, rate control to the encoder,
 * ...).
 *
 * Each is a declarative spec, and the precedence ladder, resolution, and
 * (through the dashboards' thin `useConditionalSetting` hook) initialization,
 * server sync and dependency re-derivation are generic; adding a setting is
 * one more spec. A spec fully describes both the read side and the write
 * side so the dashboards touch neither `postMessage` nor localStorage keys
 * directly.
 *
 * Resolution precedence, highest first:
 *  1. locked server value: the operator forces it, the client cannot override;
 *  2. explicit client choice, from localStorage, which must satisfy `isValid`;
 *  3. explicit server choice, a CLI or environment override, which must
 *     satisfy `isValid`;
 *  4. conditional default, derived from other state, which must satisfy
 *     `isValid`;
 *  5. built-in server default, the ground-truth fallback.
 * @module
 */

/**
 * @typedef {object} SettingSpec
 * @property {string} id Name of the setting in the dashboards.
 * @property {string} serverKey Key into `server_settings`.
 * @property {string} storageKey localStorage key for the client's choice.
 * @property {((stored: string) => *)=} parse Interprets the stored string;
 *     identity by default.
 * @property {((ctx: object) => *)=} conditional State-derived default, or
 *     `undefined` when the state implies nothing.
 * @property {((value: *, ctx: object) => boolean)=} isValid Rejects invalid
 *     candidates at every rung.
 * @property {*=} fallback Value used when nothing else resolves.
 * @property {((serverValue: *) => *)=} toUi Maps the server domain to the UI
 *     domain.
 * @property {((uiValue: *) => *)=} toServer Inverse of `toUi`; identity by
 *     default.
 * @property {((uiValue: *) => string)=} serialize localStorage form; `String`
 *     by default.
 * @property {((serverValue: *, ctx: object, io: {postSetting: Function, postToCore: Function}) => void)=} propagate
 *     Pushes a change to the server or the core.
 */

/**
 * Rate-control default per encoder for WebSocket streams when nothing
 * explicit is chosen: quality-driven (CRF).
 *
 * WebRTC streams default to CBR regardless of encoder (`RATE_CONTROL_SPEC`):
 * a congestion-controlled transport needs the encoder holding a bandwidth
 * target. So does OpenH264, the software H.264 encoder of a GPL-free
 * pixelflux build: a session known to encode on the CPU defaults to CBR when
 * the server reports that build (`softwareH264RcDefault`, the same rule as
 * the server's `resolve_rate_control_default`).
 */
export const ENCODER_RC_DEFAULTS = {
    "h264enc": "crf",
    "h264enc-striped": "crf",
    "jpeg": "crf",
};

/**
 * Whether a session with this encoder is known to encode H.264 on the CPU:
 * the striped encoder has no hardware path, and `h264enc` does when software
 * encoding is forced (without it `h264enc` may still land on the CPU, which
 * nothing here can know in advance).
 * @param {string} encoder Encoder wire value.
 * @param {boolean} useCpu Whether software encoding is forced.
 * @returns {boolean}
 */
export function softwareH264Path(encoder, useCpu) {
    return encoder === "h264enc-striped" || (encoder === "h264enc" && !!useCpu);
}

/**
 * The WebSocket rate-control default for an encoder.
 * @param {string} encoder Encoder wire value.
 * @param {string} softwareH264Encoder The server's software H.264 encoder
 *     from the settings payload, `x264` or `openh264`.
 * @param {boolean} useCpu Whether software encoding is forced.
 * @returns {string|undefined} `cbr` or `crf`; `undefined` for an unknown encoder.
 */
export function softwareH264RcDefault(encoder, softwareH264Encoder, useCpu) {
    if (softwareH264Encoder === "openh264" && softwareH264Path(encoder, useCpu)) return "cbr";
    return ENCODER_RC_DEFAULTS[encoder];
}

/**
 * Resolves one setting to its value in server terms through the module's
 * precedence ladder.
 * @param {object} input
 * @param {({value: *, locked?: boolean, overridden?: boolean}|undefined)} input.server
 *     The setting's entry in `server_settings`.
 * @param {(string|null|undefined)} input.stored The client's stored choice.
 * @param {((stored: string) => *)=} input.parse Interprets the stored string.
 * @param {(() => *)=} input.conditional State-derived default.
 * @param {((value: *) => boolean)=} input.isValid Rejects invalid candidates.
 * @returns {*} The resolved value; `undefined` without a server entry.
 */
export function resolveConditionalSetting({ server, stored, parse = (v) => v, conditional, isValid }) {
    const usable = (v) => v !== undefined && v !== null && (!isValid || isValid(v));
    if (server && server.locked) return server.value;
    if (stored !== null && stored !== undefined) {
        const v = parse(stored);
        if (usable(v)) return v;
    }
    if (server && server.overridden && usable(server.value)) return server.value;
    const conditionalValue = conditional ? conditional() : undefined;
    if (usable(conditionalValue)) return conditionalValue;
    return server ? server.value : undefined;
}

/**
 * Resolves a spec to its UI value.
 * @param {SettingSpec} spec The setting.
 * @param {object|null} serverSettings The `server_settings` payload.
 * @param {object} ctx State the spec's conditional and validator read.
 * @param {(key: string) => string|null} readStored localStorage reader.
 * @returns {*} The value in the UI domain.
 */
export function resolveSpec(spec, serverSettings, ctx, readStored) {
    const raw = resolveConditionalSetting({
        server: serverSettings ? serverSettings[spec.serverKey] : undefined,
        stored: readStored(spec.storageKey),
        parse: spec.parse,
        conditional: spec.conditional ? () => spec.conditional(ctx) : undefined,
        isValid: spec.isValid ? (v) => spec.isValid(v, ctx) : undefined,
    });
    const value = (raw !== undefined && raw !== null) ? raw : spec.fallback;
    return spec.toUi ? spec.toUi(value) : value;
}

/**
 * Whether a setting is explicitly pinned, so a dependency change must not
 * re-derive it: the client stored a choice, or the operator overrode or
 * locked it.
 * @param {SettingSpec} spec The setting.
 * @param {object|null} serverSettings The `server_settings` payload.
 * @param {(key: string) => string|null} readStored localStorage reader.
 * @returns {boolean}
 */
export function isSettingPinned(spec, serverSettings, readStored) {
    const server = serverSettings ? serverSettings[spec.serverKey] : undefined;
    return readStored(spec.storageKey) !== null || !!(server && (server.overridden || server.locked));
}

/**
 * HiDPI, shown as the inverse of `use_css_scaling`. A manual or preset
 * resolution wants CSS scaling on (HiDPI off). The core owns `useCssScaling`,
 * applying and persisting it on the propagated message.
 */
export const HIDPI_SPEC = {
    id: "hidpi",
    serverKey: "use_css_scaling",
    storageKey: "useCssScaling",
    parse: (v) => v === "true",
    conditional: (ctx) => (ctx.manualActive ? true : undefined),
    fallback: false,
    toUi: (cssScaling) => !cssScaling,
    toServer: (hidpi) => !hidpi,
    serialize: (hidpi) => String(!hidpi),
    propagate: (cssScaling, _ctx, io) => io.postToCore({ type: "setUseCssScaling", value: cssScaling }),
};

/** Rate control: CBR on WebRTC, else the per-encoder default. */
export const RATE_CONTROL_SPEC = {
    id: "rate_control_mode",
    serverKey: "rate_control_mode",
    storageKey: "rate_control_mode",
    conditional: (ctx) => (ctx.streamMode === "webrtc"
        ? "cbr"
        : softwareH264RcDefault(ctx.activeEncoder, ctx.softwareH264Encoder, ctx.useCpu)),
    isValid: (v, ctx) => ctx.allowedRateControl.includes(v),
    fallback: "crf",
    propagate: (mode, _ctx, io) => io.postSetting({ rate_control_mode: mode }),
};

/**
 * A spec for a plain boolean setting that carries a server truth. Routing it
 * through the ladder makes the displayed state track the real applied value,
 * so a locked or overridden operator value reaches the toggle. `serverKey`
 * and `storageKey` are the same key.
 * @param {string} key The server and storage key.
 * @param {boolean} fallback Value when nothing else resolves.
 * @param {SettingSpec['propagate']} propagate Pushes a change.
 * @returns {SettingSpec}
 */
function boolSpec(key, fallback, propagate) {
    return { id: key, serverKey: key, storageKey: key, parse: (v) => v === "true", fallback, propagate };
}

/**
 * The core owns `use_browser_cursors`, applying and persisting it on the
 * propagated message, so this spec posts to the core rather than a settings
 * message.
 */
export const USE_BROWSER_CURSORS_SPEC = boolSpec("use_browser_cursors", false,
    (value, _ctx, io) => io.postToCore({ type: "setUseBrowserCursors", value }));
export const VIDEO_FULLCOLOR_SPEC = boolSpec("video_fullcolor", false,
    (value, _ctx, io) => io.postSetting({ video_fullcolor: value }));
export const VIDEO_STREAMING_MODE_SPEC = boolSpec("video_streaming_mode", false,
    (value, _ctx, io) => io.postSetting({ video_streaming_mode: value }));
/**
 * Paint-over spends encoder effort a bandwidth-targeted stream budgets for
 * motion, so it defaults off under CBR and back on under CRF until someone
 * chooses (the same rule as the server's `resolve_paint_over_default`).
 */
export const USE_PAINT_OVER_QUALITY_SPEC = {
    ...boolSpec("use_paint_over_quality", true,
        (value, _ctx, io) => io.postSetting({ use_paint_over_quality: value })),
    conditional: (ctx) => (ctx.rateControlMode === "cbr" ? false : ctx.rateControlMode === "crf" ? true : undefined),
};
export const USE_CPU_SPEC = boolSpec("use_cpu", false,
    (value, _ctx, io) => io.postSetting({ use_cpu: value }));
export const FORCE_ALIGNED_RESOLUTION_SPEC = boolSpec("force_aligned_resolution", false,
    (value, _ctx, io) => io.postSetting({ force_aligned_resolution: value }));

const SETTING_SPECS = [
    HIDPI_SPEC, RATE_CONTROL_SPEC, USE_BROWSER_CURSORS_SPEC, VIDEO_FULLCOLOR_SPEC,
    VIDEO_STREAMING_MODE_SPEC, USE_PAINT_OVER_QUALITY_SPEC, USE_CPU_SPEC,
    FORCE_ALIGNED_RESOLUTION_SPEC,
];

/**
 * Server payload key to localStorage key, derived from the specs so the two
 * names cannot drift apart. Only HiDPI differs (`use_css_scaling` is stored
 * as the client-side `useCssScaling` flag); anything unregistered stores
 * under its own server key.
 */
const SERVER_TO_STORAGE_KEY = SETTING_SPECS.reduce((map, spec) => {
    if (spec.storageKey !== spec.serverKey) map[spec.serverKey] = spec.storageKey;
    return map;
}, {});

/**
 * The localStorage key a server setting's client choice lives under, which is
 * what the cores ask "has the user overridden this?" about.
 * @param {string} serverKey Key into `server_settings`.
 * @returns {string}
 */
export function storageKeyForServerKey(serverKey) {
    return SERVER_TO_STORAGE_KEY[serverKey] || serverKey;
}
