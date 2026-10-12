/**
 * Conditional settings: settings whose default depends on other state (HiDPI
 * defers to whether a manual resolution is set, paint-over to Turbo, ...), and
 * the plain ones that share the ladder so a locked or overridden server value
 * reaches the UI.
 *
 * Each is a declarative spec, and the precedence ladder, resolution, and
 * (through the dashboards' thin `useConditionalSetting` hook) initialization,
 * server sync, and dependency re-derivation are generic; adding a setting is
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

import { codecOfEncoder } from "./wire-codecs.js";

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
 * Whether the software-encoding switch changes anything for an encoder: its
 * codec has both a hardware backend on the server's encode node and a
 * software encoder in its pixelflux build, so the switch moves the session
 * between them. Never for the CPU-only encoders. Without a backend table (a
 * server whose hardware side could not be probed) only H.264 is read as
 * served both ways.
 * @param {string} encoder Encoder wire value.
 * @param {Object<string, {hardware: (string|null), software: (string|null)}>|undefined} encoderBackends
 *     The server's `encoder_backends` payload entry, keyed by codec name.
 * @returns {boolean}
 */
export function softwareChoiceAvailable(encoder, encoderBackends) {
    if (encoder === "jpeg" || encoder === "h264enc-striped") return false;
    if (!encoderBackends) return encoder === "h264enc";
    const backends = encoderBackends[codecOfEncoder(encoder)];
    return !!(backends && backends.hardware && backends.software);
}

/**
 * The 10-bit stream an encoder would run on this server, or `null` where it
 * would stream 8 bits. A session runs on the encode node's engine where the
 * codec has one and software is not forced, and the server hands it to its
 * software encoder where the engine lacks the 4:4:4 a full-color session asks
 * for, or the 10 bits this one does while the software encoder codes them.
 * Each side answers at the chroma it would run: its 4:4:4 where full color is
 * on and it carries that, else 4:2:0. `null` without a backend table, so the
 * switch is not offered on a guess.
 * @param {string} encoder Encoder wire value.
 * @param {Object<string, {hardware: (string|null), software: (string|null),
 *     fullcolor: Object<string, ?boolean>, ten_bit: Object<string, ?Object<string, boolean>>}>|undefined|null} encoderBackends
 *     The server's `encoder_backends` payload entry, keyed by codec name.
 * @param {boolean} useCpu Whether software encoding is forced.
 * @param {boolean} fullColor Whether full color is on.
 * @returns {?{fullcolor: boolean, software: boolean}} The stream's chroma, and
 *     whether the software encoder codes it.
 */
export function tenBitStream(encoder, encoderBackends, useCpu, fullColor) {
    if (encoder === "jpeg" || !encoderBackends) return null;
    const backends = encoderBackends[codecOfEncoder(encoder)];
    if (!backends || !backends.ten_bit) return null;
    const carries = (side) => {
        const table = backends.ten_bit[side];
        const fullcolor = !!(fullColor && backends.fullcolor && backends.fullcolor[side]);
        return table && table[fullcolor ? "444" : "420"] ? { fullcolor, software: side === "software" } : null;
    };
    if (useCpu || encoder === "h264enc-striped" || !backends.hardware) return carries("software");
    if (fullColor && backends.fullcolor && backends.fullcolor.hardware === false && backends.fullcolor.software) {
        return carries("software");
    }
    return carries("hardware") || carries("software");
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

/** Rate control: the server's, CBR unless an operator set it, on both transports. */
export const RATE_CONTROL_SPEC = {
    id: "rate_control_mode",
    serverKey: "rate_control_mode",
    storageKey: "rate_control_mode",
    isValid: (v, ctx) => ctx.allowedRateControl.includes(v),
    fallback: "cbr",
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
export const VIDEO_10BIT_SPEC = boolSpec("video_10bit", false,
    (value, _ctx, io) => io.postSetting({ video_10bit: value }));
export const VIDEO_STREAMING_MODE_SPEC = boolSpec("video_streaming_mode", false,
    (value, _ctx, io) => io.postSetting({ video_streaming_mode: value }));
/**
 * Paint-over cleans up a still screen, which a video encoder under Turbo never
 * leaves (it sends every frame), so it defaults off under Turbo and on without
 * it, JPEG included, whatever the rate control; a stored choice or an
 * operator's value overrides it. `ctx.encoder` and `ctx.videoStreamingMode`
 * carry the encoder and Turbo.
 */
export const USE_PAINT_OVER_QUALITY_SPEC = {
    ...boolSpec("use_paint_over_quality", true,
        (value, _ctx, io) => {
            io.postToCore({ type: "setLosslessParentState", enabled: value });
            io.postSetting({ use_paint_over_quality: value });
        }),
    conditional: (ctx) => (ctx.videoStreamingMode === undefined ? undefined
        : ctx.encoder === "jpeg" || !ctx.videoStreamingMode),
};
/** An explicit preference or operator override opts in; an echoed default cannot. */
export const LOSSLESS_STATIC_REFINEMENT_SPEC = {
    ...boolSpec("lossless_static_refinement", false,
        (value, _ctx, io) => io.postToCore({ type: "setLosslessStaticRefinement", enabled: value })),
    conditional: () => false,
};
export const USE_CPU_SPEC = boolSpec("use_cpu", false,
    (value, _ctx, io) => io.postSetting({ use_cpu: value }));
export const FORCE_ALIGNED_RESOLUTION_SPEC = boolSpec("force_aligned_resolution", false,
    (value, _ctx, io) => io.postSetting({ force_aligned_resolution: value }));
/**
 * Raw pointer motion under pointer lock: whether the client asks the engine
 * for the deltas ahead of the OS acceleration curve. Off until chosen on macOS
 * (`ctx.macDesktop`), where the engine grants the option and the curve it
 * removes is what carried a slow hand across the remote screen; a stored
 * choice or an operator value overrides the platform. The core owns it,
 * applying and persisting it on the propagated message.
 */
export const RAW_POINTER_MOTION_SPEC = {
    ...boolSpec("raw_pointer_motion", true,
        (value, _ctx, io) => io.postToCore({ type: "setRawPointerMotion", value })),
    conditional: (ctx) => (ctx.macDesktop ? false : undefined),
};

/**
 * Whether a macOS Command chord is sent as its Control chord. On unless chosen
 * or set otherwise; only macOS clients read it (`ctx.macDesktop` decides
 * whether a panel offers it at all). The core owns it, applying and persisting
 * it on the propagated message.
 */
export const MAC_CMD_AS_CTRL_SPEC = boolSpec("mac_cmd_as_ctrl", true,
    (value, _ctx, io) => io.postToCore({ type: "setMacCmdAsCtrl", value }));

const SETTING_SPECS = [
    HIDPI_SPEC, RATE_CONTROL_SPEC, USE_BROWSER_CURSORS_SPEC, VIDEO_FULLCOLOR_SPEC, VIDEO_10BIT_SPEC,
    VIDEO_STREAMING_MODE_SPEC, USE_PAINT_OVER_QUALITY_SPEC, USE_CPU_SPEC,
    FORCE_ALIGNED_RESOLUTION_SPEC, RAW_POINTER_MOTION_SPEC, MAC_CMD_AS_CTRL_SPEC,
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
