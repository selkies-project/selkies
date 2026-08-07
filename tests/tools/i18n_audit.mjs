// Resolve every translation key the dashboards ask for, the way they resolve it.
//
// Both translators return the key itself on a miss, so a wrong or absent key is
// invisible until it renders as "clipboard.uploadImage" on screen. Emits JSON:
// {dashboard: {unresolved: {key: [site]}, gaps: {locale: [key]}, keys: n}}.
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

const ADDONS = process.argv[2];
const LOCALES = "en es zh hi pt fr ru de tr it nl ar ko ja vi th fil da".split(" ");
const CLASSIC = `${ADDONS}/selkies-dashboard`;
const WISH = `${ADDONS}/selkies-dashboard-wish`;

function walk(dir, out = []) {
  for (const e of readdirSync(dir)) {
    if (e === "node_modules" || e === "dist") continue;
    const p = join(dir, e);
    if (statSync(p).isDirectory()) walk(p, out);
    // selkies-core.js is the bundled core copied in by the prebuild step, not
    // dashboard source: its minified internals have their own unrelated t().
    else if (/\.(ts|tsx|js|jsx)$/.test(e) && e !== "selkies-core.js") out.push(p);
  }
  return out;
}

// The dictionaries are plain object literals, so read them without a bundler.
function literal(src, name) {
  const start = src.indexOf(`const ${name} = {`);
  if (start < 0) return null;
  const open = src.indexOf("{", start);
  let depth = 0, end = -1;
  for (let j = open; j < src.length; j++) {
    if (src[j] === "{") depth++;
    else if (src[j] === "}" && --depth === 0) { end = j + 1; break; }
  }
  return end < 0 ? null : eval("(" + src.slice(open, end) + ")");
}

const classicSrc = readFileSync(`${CLASSIC}/src/translations.js`, "utf8");
const extraSrc = readFileSync(`${WISH}/src/translations-extra.ts`, "utf8");
const classic = Object.fromEntries(LOCALES.map((l) => [l, literal(classicSrc, l)]));
const extra = Object.fromEntries(LOCALES.map((l) => [l, literal(extraSrc, l)]));

const has = (dict, key) => {
  let o = dict;
  for (const k of key.split(".")) {
    if (!o || typeof o !== "object" || !Object.prototype.hasOwnProperty.call(o, k)) return false;
    o = o[k];
  }
  return typeof o === "string";
};

const flatten = (dict, prefix = "", out = []) => {
  for (const [k, v] of Object.entries(dict || {})) {
    if (v && typeof v === "object") flatten(v, prefix + k + ".", out);
    else out.push(prefix + k);
  }
  return out;
};

// Dotted and single-segment keys alike; runtime-built keys cannot be resolved
// statically and are counted separately rather than reported as failures.
const KEY = /\b(?:t|tl)\(\s*(['"])([^'"\n]+)\1/g;

const report = {};
for (const [name, dir, dicts] of [
  ["classic", `${CLASSIC}/src`, [classic.en]],
  ["wish", `${WISH}/src`, [extra.en, classic.en]],
]) {
  const unresolved = {};
  let keys = 0;
  for (const file of walk(dir)) {
    const src = readFileSync(file, "utf8");
    const rel = file.slice(ADDONS.length + 1);
    for (const m of src.matchAll(KEY)) {
      keys++;
      const key = m[2];
      if (dicts.some((d) => has(d, key))) continue;
      const line = src.slice(0, m.index).split("\n").length;
      (unresolved[key] ||= []).push(`${rel}:${line}`);
    }
  }
  // A key defined in English but absent elsewhere silently serves English, so
  // it never surfaces as a raw key while still being an untranslated string.
  const dict = name === "classic" ? classic : extra;
  const enKeys = flatten(dict.en);
  const gaps = {};
  for (const l of LOCALES.filter((x) => x !== "en")) {
    const missing = enKeys.filter((k) => !has(dict[l], k));
    if (missing.length) gaps[l] = missing;
  }
  report[name] = { keys, locales: LOCALES.length, enKeys: enKeys.length, unresolved, gaps };
}
process.stdout.write(JSON.stringify(report));
