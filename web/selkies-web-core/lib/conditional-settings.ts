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

import { codecOfEncoder } from './wire-codecs.ts';

/** One side's backend description. */
export interface CodecBackends {
  hardware: string | null;
  software: string | null;
  fullcolor?: Record<string, boolean | null | undefined>;
  ten_bit?: Record<string, Record<string, boolean>>;
}

/** The server's `encoder_backends` payload entry, keyed by codec name. */
export type EncoderBackends = Record<string, CodecBackends>;

/** A setting's IO for `propagate`. */
export interface SettingIO {
  postSetting: (value: Record<string, unknown>) => void;
  postToCore: (message: Record<string, unknown>) => void;
}

/** A conditional setting's declarative spec. */
export interface SettingSpec {
  id: string;
  serverKey: string;
  storageKey: string;
  parse?: (stored: string) => unknown;
  conditional?: (ctx: Record<string, unknown>) => unknown;
  isValid?: (value: unknown, ctx: Record<string, unknown>) => boolean;
  fallback?: unknown;
  toUi?: (value: unknown) => unknown;
  toServer?: (uiValue: unknown) => unknown;
  serialize?: (uiValue: unknown) => string;
  propagate?: (serverValue: unknown, ctx: Record<string, unknown>, io: SettingIO) => void;
}

/**
 * Whether the CPU encode threading switch does anything for a session on an
 * encoder: the session codes in software, on JPEG (whose stripes it codes
 * one after another), where software is forced on a codec that has a software
 * encoder, where the host encodes the codec in software alone, or where the
 * hardware engine hands a full color or 10-bit stream to
 * its software encoder. A hardware engine has no such threads. Without a
 * backend table it cannot be told, and reads as applying.
 * @param encoder Encoder wire value.
 * @param encoderBackends The server's `encoder_backends` payload entry, keyed by codec name.
 * @param useCpu Whether software encoding is forced.
 * @param fullColor Whether full color is asked for.
 * @param tenBit Whether 10-bit is asked for.
 * @returns
 */
export function cpuEncodeThreadingApplies(encoder: string, encoderBackends: EncoderBackends | undefined, useCpu: boolean, fullColor: boolean, tenBit: boolean): boolean {
    if (encoder === "jpeg" || !encoderBackends) return true;
    const backends = encoderBackends[codecOfEncoder(encoder)];
    if (!backends || !backends.hardware || (useCpu && backends.software)) return true;
    if (fullColor && backends.fullcolor && backends.fullcolor.hardware === false && backends.fullcolor.software) {
        return true;
    }
    const stream = tenBit ? tenBitStream(encoder, encoderBackends, useCpu, fullColor) : null;
    return !!(stream && stream.software);
}

/**
 * Whether the software-encoding switch changes anything for an encoder: its
 * codec has both a hardware backend on the server's encode node and a
 * software encoder in its pixelflux build, so the switch moves the session
 * between them. Never for JPEG, which codes on the CPU alone. Without a
 * backend table (a server whose hardware side could not be probed) only H.264
 * is read as served both ways.
 * @param encoder Encoder wire value.
 * @param encoderBackends The server's `encoder_backends` payload entry, keyed by codec name.
 * @returns
 */
export function softwareChoiceAvailable(encoder: string, encoderBackends: EncoderBackends | undefined): boolean {
    if (encoder === "jpeg") return false;
    if (!encoderBackends) return encoder === "h264enc";
    const backends = encoderBackends[codecOfEncoder(encoder)];
    return !!(backends && backends.hardware && backends.software);
}

/** The 10-bit stream an encoder would run as. */
export interface TenBitStream {
  fullcolor: boolean;
  software: boolean;
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
 * @param encoder Encoder wire value.
 * @param encoderBackends The server's `encoder_backends` payload entry, keyed by codec name.
 * @param useCpu Whether software encoding is forced.
 * @param fullColor Whether full color is on.
 * @returns The stream's chroma, and whether the software encoder codes it.
 */
export function tenBitStream(encoder: string, encoderBackends: EncoderBackends | undefined | null, useCpu: boolean, fullColor: boolean): TenBitStream | null {
    if (encoder === "jpeg" || !encoderBackends) return null;
    const backends = encoderBackends[codecOfEncoder(encoder)];
    if (!backends || !backends.ten_bit) return null;
    const carries = (side: string): TenBitStream | null => {
        const table = backends.ten_bit![side];
        const fullcolor = !!(fullColor && backends.fullcolor && backends.fullcolor[side]);
        return table && table[fullcolor ? "444" : "420"] ? { fullcolor, software: side === "software" } : null;
    };
    if (useCpu || !backends.hardware) return carries("software");
    if (fullColor && backends.fullcolor && backends.fullcolor.hardware === false && backends.fullcolor.software) {
        return carries("software");
    }
    return carries("hardware") || carries("software");
}

/** A `server_settings` entry. */
export interface ServerSettingEntry {
  value: unknown;
  locked?: boolean;
  overridden?: boolean;
}

/** Input to `resolveConditionalSetting`. */
export interface ResolveInput {
  server: ServerSettingEntry | undefined;
  stored: string | null | undefined;
  parse?: (stored: string) => unknown;
  conditional?: () => unknown;
  isValid?: (value: unknown) => boolean;
}

/**
 * Resolves one setting to its value in server terms through the module's
 * precedence ladder.
 * @param input
 * @returns The resolved value; `undefined` without a server entry.
 */
export function resolveConditionalSetting({ server, stored, parse = (v: string) => v, conditional, isValid }: ResolveInput): unknown {
    const usable = (v: unknown): boolean => v !== undefined && v !== null && (!isValid || isValid(v));
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
 * @param spec The setting.
 * @param serverSettings The `server_settings` payload.
 * @param ctx State the spec's conditional and validator read.
 * @param readStored localStorage reader.
 * @returns The value in the UI domain.
 */
export function resolveSpec(spec: SettingSpec, serverSettings: Record<string, ServerSettingEntry> | null, ctx: Record<string, unknown>, readStored: (key: string) => string | null): unknown {
    const raw = resolveConditionalSetting({
        server: serverSettings ? serverSettings[spec.serverKey] : undefined,
        stored: readStored(spec.storageKey),
        parse: spec.parse,
        conditional: spec.conditional ? () => spec.conditional?.(ctx) : undefined,
        isValid: spec.isValid ? (v: unknown) => spec.isValid?.(v, ctx) ?? false : undefined,
    });
    const value = (raw !== undefined && raw !== null) ? raw : spec.fallback;
    return spec.toUi ? spec.toUi(value) : value;
}

/**
 * Whether a setting is explicitly pinned, so a dependency change must not
 * re-derive it: the client stored a choice, or the operator overrode or
 * locked it.
 * @param spec The setting.
 * @param serverSettings The `server_settings` payload.
 * @param readStored localStorage reader.
 * @returns
 */
export function isSettingPinned(spec: SettingSpec, serverSettings: Record<string, ServerSettingEntry> | null, readStored: (key: string) => string | null): boolean {
    const server = serverSettings ? serverSettings[spec.serverKey] : undefined;
    return readStored(spec.storageKey) !== null || !!(server && (server.overridden || server.locked));
}

/**
 * HiDPI, shown as the inverse of `use_css_scaling`. A manual or preset
 * resolution wants CSS scaling on (HiDPI off). The core owns `useCssScaling`,
 * applying and persisting it on the propagated message.
 */
export const HIDPI_SPEC: SettingSpec & Required<Pick<SettingSpec, "toServer" | "serialize">> = {
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
export const RATE_CONTROL_SPEC: SettingSpec = {
    id: "rate_control_mode",
    serverKey: "rate_control_mode",
    storageKey: "rate_control_mode",
    isValid: (v, ctx) => (ctx.allowedRateControl as string[]).includes(v as string),
    fallback: "cbr",
    propagate: (mode, _ctx, io) => io.postSetting({ rate_control_mode: mode }),
};

/**
 * A spec for a plain boolean setting that carries a server truth. Routing it
 * through the ladder makes the displayed state track the real applied value,
 * so a locked or overridden operator value reaches the toggle. `serverKey`
 * and `storageKey` are the same key.
 * @param key The server and storage key.
 * @param fallback Value when nothing else resolves.
 * @param propagate Pushes a change.
 * @returns
 */
function boolSpec(key: string, fallback: boolean, propagate: NonNullable<SettingSpec['propagate']>): SettingSpec {
    return { id: key, serverKey: key, storageKey: key, parse: (v) => v === "true", fallback, propagate };
}

/**
 * The core owns `use_browser_cursors`, applying and persisting it on the
 * propagated message, so this spec posts to the core rather than a settings
 * message.
 */
export const USE_BROWSER_CURSORS_SPEC: SettingSpec = boolSpec("use_browser_cursors", false,
    (value, _ctx, io) => io.postToCore({ type: "setUseBrowserCursors", value }));
export const VIDEO_FULLCOLOR_SPEC: SettingSpec = boolSpec("video_fullcolor", false,
    (value, _ctx, io) => io.postSetting({ video_fullcolor: value }));
export const VIDEO_10BIT_SPEC: SettingSpec = boolSpec("video_10bit", false,
    (value, _ctx, io) => io.postSetting({ video_10bit: value }));
export const VIDEO_STREAMING_MODE_SPEC: SettingSpec = boolSpec("video_streaming_mode", false,
    (value, _ctx, io) => io.postSetting({ video_streaming_mode: value }));
/**
 * Paint-over cleans up a still screen at its own quality; it defaults off under
 * Turbo and on without it, JPEG included (a constant rate cleans a still screen
 * up without it too, to the session's quality); a stored choice or an
 * operator's value overrides it. `ctx.encoder` and `ctx.videoStreamingMode`
 * carry the encoder and Turbo.
 */
export const USE_PAINT_OVER_QUALITY_SPEC: SettingSpec = {
    ...boolSpec("use_paint_over_quality", true,
        (value, _ctx, io) => io.postSetting({ use_paint_over_quality: value })),
    conditional: (ctx) => (ctx.videoStreamingMode === undefined ? undefined
        : ctx.encoder === "jpeg" || !ctx.videoStreamingMode),
};
export const USE_CPU_SPEC: SettingSpec = boolSpec("use_cpu", false,
    (value, _ctx, io) => io.postSetting({ use_cpu: value }));
export const CPU_ENCODE_THREADING_SPEC: SettingSpec = boolSpec("cpu_encode_threading", true,
    (value, _ctx, io) => io.postSetting({ cpu_encode_threading: value }));
export const FORCE_ALIGNED_RESOLUTION_SPEC: SettingSpec = boolSpec("force_aligned_resolution", false,
    (value, _ctx, io) => io.postSetting({ force_aligned_resolution: value }));
/**
 * Raw pointer motion under pointer lock: whether the client asks the engine
 * for the deltas ahead of the OS acceleration curve. Off until chosen on macOS
 * (`ctx.macDesktop`), where the engine grants the option and the curve it
 * removes is what carried a slow hand across the remote screen; a stored
 * choice or an operator value overrides the platform. The core owns it,
 * applying and persisting it on the propagated message.
 */
export const RAW_POINTER_MOTION_SPEC: SettingSpec = {
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
export const MAC_CMD_AS_CTRL_SPEC: SettingSpec = boolSpec("mac_cmd_as_ctrl", true,
    (value, _ctx, io) => io.postToCore({ type: "setMacCmdAsCtrl", value }));

const SETTING_SPECS: SettingSpec[] = [
    HIDPI_SPEC, RATE_CONTROL_SPEC, USE_BROWSER_CURSORS_SPEC, VIDEO_FULLCOLOR_SPEC, VIDEO_10BIT_SPEC,
    VIDEO_STREAMING_MODE_SPEC, USE_PAINT_OVER_QUALITY_SPEC, USE_CPU_SPEC, CPU_ENCODE_THREADING_SPEC,
    FORCE_ALIGNED_RESOLUTION_SPEC, RAW_POINTER_MOTION_SPEC, MAC_CMD_AS_CTRL_SPEC,
];

/**
 * Server payload key to localStorage key, derived from the specs so the two
 * names cannot drift apart. Only HiDPI differs (`use_css_scaling` is stored
 * as the client-side `useCssScaling` flag); anything unregistered stores
 * under its own server key.
 */
const SERVER_TO_STORAGE_KEY: Record<string, string> = SETTING_SPECS.reduce((map, spec) => {
    if (spec.storageKey !== spec.serverKey) map[spec.serverKey] = spec.storageKey;
    return map;
}, {} as Record<string, string>);

/**
 * The localStorage key a server setting's client choice lives under, which is
 * what the cores ask "has the user overridden this?" about.
 * @param serverKey Key into `server_settings`.
 * @returns
 */
export function storageKeyForServerKey(serverKey: string): string {
    return SERVER_TO_STORAGE_KEY[serverKey] || serverKey;
}
