// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Shared translator: the classic dashboard's dictionary (imported straight
// from its addon, like the touch-gamepad import in main.jsx) overlaid with the
// wish-only strings in translations-extra.ts. Extras are consulted first, then
// the classic translator with its built-in English fallback. Language follows
// navigator.language, resolved once at module load (same policy as classic).
import { getTranslator } from "../../selkies-dashboard/src/translations.js";
import { extras } from "./translations-extra";

const lang = ((typeof navigator !== "undefined" && navigator.language) || "en")
    .split("-")[0]
    .toLowerCase();
const base = getTranslator(lang);
const extra = extras[lang] || extras.en;

const lookup = (dict: any, key: string): unknown =>
    key.split(".").reduce((o: any, k: string) => (o && typeof o === "object" ? o[k] : undefined), dict);

const interpolate = (text: string, vars?: Record<string, unknown>): string =>
    vars ? text.replace(/\{(\w+)\}/g, (m, k) => (vars[k] !== undefined ? String(vars[k]) : m)) : text;

export const t = (key: string, vars?: Record<string, unknown>): string => {
    const hit = lookup(extra, key) ?? lookup(extras.en, key);
    if (typeof hit === "string") return interpolate(hit, vars);
    return base.t(key, vars);
};

// Label variant: classic labels often end in ":" (or fullwidth "："); wish
// renders its own label styling, so strip a trailing colon.
export const tl = (key: string, vars?: Record<string, unknown>): string =>
    t(key, vars).replace(/[:：]\s*$/, "");

export const raw = base.raw;
