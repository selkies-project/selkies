/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The translator: one dictionary per locale (`translations.ts`), the locale
 * resolved once at module load from `navigator.language`, and English as the
 * fallback for a key the locale lacks. A key no dictionary has is returned as
 * written, or as the literal a caller passes in place of its variables, so a
 * misspelled key shows its English text rather than nothing.
 * @module
 */
import { translations, type Dictionary } from "./translations.ts";

/**
 * The dictionary key a browser language tag reads: its base language, so
 * `en-US` reads `en`, except Chinese, which is two written languages under
 * one base: a tag naming the Traditional script or a region that writes it
 * (Taiwan, Hong Kong, Macao) reads `zh_tw`, any other Chinese tag `zh_cn`.
 * @param langCode A BCP 47 tag.
 * @returns The dictionary key.
 */
export function resolveLocale(langCode: string = "en"): string {
    const parts = String(langCode).toLowerCase().split(/[-_]/);
    if (parts[0] === "zh") {
        return parts.slice(1).some((p) => ["hant", "tw", "hk", "mo"].includes(p)) ? "zh_tw" : "zh_cn";
    }
    return parts[0];
}

const locale = resolveLocale((typeof navigator !== "undefined" && navigator.language) || "en");
const dict: Dictionary = translations[locale] || translations.en;

/** Resolves a dotted key against a dictionary, a string or nothing. */
function lookup(table: Dictionary, key: string): string | undefined {
    let node: unknown = table;
    for (const part of key.split(".")) {
        if (!node || typeof node !== "object" || !Object.prototype.hasOwnProperty.call(node, part)) return undefined;
        node = (node as Dictionary)[part];
    }
    return typeof node === "string" ? node : undefined;
}

/** Substitutes `{name}` placeholders from `vars`, leaving unknown ones intact. */
function interpolate(text: string, vars?: Record<string, unknown>): string {
    if (!vars) return text;
    return text.replace(/\{(\w+)\}/g, (match, name) => (vars[name] !== undefined ? String(vars[name]) : match));
}

/**
 * Translates a dotted key.
 * @param key Dotted lookup key.
 * @param vars Values for `{name}` placeholders, or a literal to show where
 *     no dictionary has the key.
 * @returns The translated string.
 */
export function t(key: string, vars?: Record<string, unknown> | string): string {
    const literal = typeof vars === "string" ? vars : null;
    const values = typeof vars === "string" ? undefined : vars;
    const hit = lookup(dict, key) ?? lookup(translations.en, key);
    if (hit !== undefined) return interpolate(hit, values);
    console.warn(`Translation key not found: ${key}`);
    return literal ?? key;
}

/**
 * Label variant of `t`: a label that ends in `:` (or fullwidth `：`) in the
 * dictionary is shown without it, since the controls draw their own labels.
 * @param key Dotted lookup key.
 * @param vars Values for `{name}` placeholders.
 * @returns The translated label without its trailing colon.
 */
export function tl(key: string, vars?: Record<string, unknown>): string {
    return t(key, vars).replace(/[:：]\s*$/, "");
}
