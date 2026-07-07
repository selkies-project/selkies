// Conditional settings: settings whose default depends on other state (HiDPI
// defers to whether a manual resolution is set; rate control defers to the
// encoder; ...). Each is a declarative SPEC; the precedence ladder, resolution,
// and (via the dashboards' thin useConditionalSetting hook) init + server-sync +
// dependency re-derivation are all generic. Adding a setting is one more spec.

// Rate-control default per encoder when nothing explicit is chosen: the striped
// software encoder and jpeg are quality-driven (CRF); the single-slice software
// encoders target a bandwidth (CBR).
export const ENCODER_RC_DEFAULTS = {
    "h264enc": "cbr",
    "openh264enc": "cbr",
    "h264enc-striped": "crf",
    "jpeg": "crf",
};

// Resolve one setting to its value in SERVER terms. Precedence, highest first:
//   1. locked server value       - operator forces it; the client can't override
//   2. explicit client choice     - localStorage; must satisfy isValid
//   3. explicit server choice     - CLI/env override; must satisfy isValid
//   4. conditional default        - derived from other state; must satisfy isValid
//   5. built-in server default    - the ground-truth fallback
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

// Resolve a SPEC (below) to its UI value, given the server_settings payload, a
// context object (state the conditionals read), and a localStorage reader.
// A spec's fields:
//   serverKey     key into server_settings                 (required)
//   storageKey    localStorage key for the client's choice  (required)
//   parse         (string)=>value  interpret the stored string          (default: identity)
//   conditional   (ctx)=>value|undefined  state-derived default          (optional)
//   isValid       (value,ctx)=>bool  reject invalid candidates           (optional)
//   fallback      value used when nothing else resolves                  (optional)
//   toUi          (serverValue)=>uiValue  map server domain to UI domain (optional)
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

// Is this setting explicitly pinned (so a dependency change must NOT re-derive
// it)? True when the client stored a choice or the operator overrode/locked it.
export function isSettingPinned(spec, serverSettings, readStored) {
    const server = serverSettings ? serverSettings[spec.serverKey] : undefined;
    return readStored(spec.storageKey) !== null || !!(server && (server.overridden || server.locked));
}

// The registry of conditional settings. Each spec fully describes both READ
// (parse/conditional/isValid/fallback/toUi) and WRITE (toServer/serialize/
// propagate) so the dashboards touch neither postMessage nor localStorage keys
// directly. WRITE fields:
//   toServer   (uiValue)=>serverValue   inverse of toUi (default: identity)
//   serialize  (uiValue)=>string        localStorage form (default: String)
//   propagate  (serverValue, ctx, io)   push to server/core; io = {postSetting, postToCore}
export const HIDPI_SPEC = {
    id: "hidpi",
    serverKey: "use_css_scaling",
    storageKey: "useCssScaling",
    parse: (v) => v === "true",
    // A manual/preset resolution wants CSS scaling on (HiDPI off).
    conditional: (ctx) => (ctx.manualActive ? true : undefined),
    fallback: false,
    // UI shows HiDPI, the inverse of use_css_scaling.
    toUi: (cssScaling) => !cssScaling,
    toServer: (hidpi) => !hidpi,
    serialize: (hidpi) => String(!hidpi),
    // The core owns useCssScaling: it applies + persists on this message.
    propagate: (cssScaling, _ctx, io) => io.postToCore({ type: "setUseCssScaling", value: cssScaling }),
};

export const RATE_CONTROL_SPEC = {
    id: "rate_control_mode",
    serverKey: "rate_control_mode",
    storageKey: "rate_control_mode",
    conditional: (ctx) => ENCODER_RC_DEFAULTS[ctx.activeEncoder],
    isValid: (v, ctx) => ctx.allowedRateControl.includes(v),
    fallback: "crf",
    propagate: (mode, _ctx, io) => io.postSetting({ rate_control_mode: mode }),
};
