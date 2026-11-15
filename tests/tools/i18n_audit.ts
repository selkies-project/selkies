/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Resolve every translation key the dashboard asks for, the way it resolves it.
 *
 * The translator returns the key itself on a miss, so a wrong or absent key is
 * invisible until it renders as "clipboard.uploadImage" on screen. Reads the web
 * directory named on the command line, else the one in this tree.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import type { Dictionary } from "../../web/selkies-dashboard/src/translations.ts";

// The web directory, resolved, and refused where it holds a NUL or names a parent.
const WEB = resolve(process.argv[2] ?? join(import.meta.dirname, "..", "..", "web"));
if (!/^\/[^\0]*$/.test(WEB) || WEB.split("/").includes("..")) throw new Error(`i18n_audit: ${WEB} is not a usable path`);
const LOCALES = "en es zh_cn zh_tw hi pt fr ru de tr it nl ar ko ja vi th fil da".split(" ");

// A browser tag has to read the dictionary written for it: Chinese is two
// written languages under one base language, told apart by script and region.
const ROUTES: Record<string, string> = {
  "en": "en", "en-US": "en", "pt-BR": "pt", "ja-JP": "ja", "fil": "fil",
  "zh": "zh_cn", "zh-CN": "zh_cn", "zh-SG": "zh_cn", "zh-Hans": "zh_cn", "zh-Hans-SG": "zh_cn",
  "zh-TW": "zh_tw", "zh-HK": "zh_tw", "zh-MO": "zh_tw", "zh-Hant": "zh_tw", "zh-Hant-TW": "zh_tw", "zh-Hant-HK": "zh_tw",
};
const DASHBOARD = `${WEB}/selkies-dashboard`;

/** The source files under `dir`, build output and dependencies left out. */
function walk(dir: string, out: string[] = []): string[] {
  for (const e of readdirSync(dir)) {
    if (e === "node_modules" || e === "dist") continue;
    const p = join(dir, e);
    if (statSync(p).isDirectory()) walk(p, out);
    else if (/\.(ts|tsx|js|jsx)$/.test(e)) out.push(p);
  }
  return out;
}

// The modules are those of the web directory named, so they are imported by its path.
const statsView = await import(pathToFileURL(resolve(`${WEB}/selkies-web-core/lib/stream-stats-view.ts`)).href) as
  typeof import("../../web/selkies-web-core/lib/stream-stats-view.ts");
const { resolveLocale } = await import(pathToFileURL(resolve(`${DASHBOARD}/src/i18n.ts`)).href) as
  typeof import("../../web/selkies-dashboard/src/i18n.ts");
const { translations } = await import(pathToFileURL(resolve(`${DASHBOARD}/src/translations.ts`)).href) as
  typeof import("../../web/selkies-dashboard/src/translations.ts");
const dict = Object.fromEntries(LOCALES.map((l) => [l, translations[l]]));

/** A dictionary, or what one of its keys holds; a list is walked by its indices like a dictionary. */
type Entry = Dictionary[string] | undefined;

const has = (table: Dictionary, key: string): boolean => {
  let o: Entry = table;
  for (const k of key.split(".")) {
    if (!o || typeof o !== "object" || !Object.prototype.hasOwnProperty.call(o, k)) return false;
    o = (o as Dictionary)[k];
  }
  return typeof o === "string";
};

const read = (table: Dictionary, key: string): Entry => {
  let o: Entry = table;
  for (const k of key.split(".")) {
    if (!o || typeof o !== "object") return undefined;
    o = (o as Dictionary)[k];
  }
  return o;
};

const trailingColon = (value: Entry): boolean => typeof value === "string" && /[:：]\s*$/.test(value);

const flatten = (table: Dictionary, prefix = "", out: string[] = []): string[] => {
  for (const [k, v] of Object.entries(table || {})) {
    if (v && typeof v === "object") flatten(v as Dictionary, prefix + k + ".", out);
    else out.push(prefix + k);
  }
  return out;
};

// Dotted and single-segment keys alike; runtime-built keys cannot be resolved
// statically and are counted separately rather than reported as failures.
const KEY = /\b(?:t|tl)\(\s*(['"])([^'"\n]+)\1/g;

const unresolved: Record<string, string[]> = {};
let keys = 0;
for (const file of walk(`${DASHBOARD}/src`)) {
  const text = readFileSync(file, "utf8");
  const rel = file.slice(WEB.length + 1);
  for (const m of text.matchAll(KEY)) {
    keys++;
    const key = m[2];
    if (has(dict.en, key)) continue;
    const line = text.slice(0, m.index).split("\n").length;
    (unresolved[key] ||= []).push(`${rel}:${line}`);
  }
}
// A key defined in English but absent elsewhere silently serves English, so
// it never surfaces as a raw key while still being an untranslated string.
const enKeys = flatten(dict.en);
const gaps: Record<string, string[]> = {};
for (const l of LOCALES.filter((x) => x !== "en")) {
  const missing = enKeys.filter((k) => !has(dict[l], k));
  if (missing.length) gaps[l] = missing;
}
// A label's punctuation belongs to the phrase, not to the locale: the call
// site decides whether a colon is followed by a value, so a translation that
// adds one English does not have renders it dangling on its own.
const punctuation: Record<string, string[]> = {};
for (const key of enKeys) {
  const enColon = trailingColon(read(dict.en, key));
  const off = LOCALES.filter((l) => l !== "en" && typeof read(dict[l], key) === "string" &&
                                    trailingColon(read(dict[l], key)) !== enColon);
  if (off.length) punctuation[key] = off;
}
// The stats figures are worded by their key at render time, which the scan
// above cannot see, so every figure either transport shows needs its label.
const shown = new Set((["websockets", "webrtc"] as const).flatMap((transport) =>
  statsView.streamTiles(null, transport).map((tile) => tile.key)));
const tiles = { shown: shown.size, unlabeled: [...shown].filter((key) => !has(dict.en, `sections.stats.tiles.${key}`)) };
// A key the resolver names must exist.
const misrouted: Record<string, string> = {};
for (const [tag, want] of Object.entries(ROUTES)) {
  const got = resolveLocale(tag);
  if (got !== want || !dict[got]) misrouted[tag] = got;
}

let passed = 0;
let failed = 0;

/** Prints one check's line and counts it. */
function check(label: string, ok: boolean, detail: string): void {
  if (ok) passed++;
  else failed++;
  console.log(`${ok ? "PASS" : "FAIL"}  [i18n] dashboard: ${label}  ${detail}`);
}

/** The first four entries of a report map, each worded by `entry`. */
function first<T>(map: Record<string, T>, entry: (key: string, value: T) => string): string {
  return Object.entries(map).slice(0, 4).map(([key, value]) => entry(key, value)).join(", ");
}

check("every key resolves", !Object.keys(unresolved).length,
  first(unresolved, (key, sites) => `${key} (${sites[0]})`) || `${keys} keys`);
check("no locale is missing a key English has", !Object.keys(gaps).length,
  first(gaps, (locale, missing) => `${locale}: ${missing.length} missing (${missing[0]})`) ||
    `${enKeys.length} keys x ${LOCALES.length} locales`);
check("a label ends in a colon in every locale or in none", !Object.keys(punctuation).length,
  first(punctuation, (key, locales) => `${key} (${locales.slice(0, 4).join(", ")})`) ||
    `${enKeys.length} keys x ${LOCALES.length} locales`);
check("every stats figure has a label to translate", !tiles.unlabeled.length,
  tiles.unlabeled.slice(0, 4).join(", ") || `${tiles.shown} figures`);
check("a browser tag reads the dictionary written for it", !Object.keys(misrouted).length,
  first(misrouted, (tag, got) => `${tag} -> ${got}`) || `${Object.keys(ROUTES).length} tags`);

console.log(`[i18n] ${passed}/${passed + failed} passed`);
process.exit(failed ? 1 : 0);
