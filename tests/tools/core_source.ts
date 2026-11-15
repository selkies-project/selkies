/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The web core's sources as the browser is handed them, for audits that run a
 * worker, a worklet or a helper the core embeds as text rather than exports.
 * A module is read as the JavaScript its types strip to; a worker source is
 * cut out of its template and spliced the way the client splices it.
 * @module
 */
import { readFileSync } from 'node:fs';
import { stripTypeScriptTypes } from 'node:module';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';
import { createStripeClock } from '../../web/selkies-web-core/lib/stripe-clock.ts';
import { createChunkStamp } from '../../web/selkies-web-core/lib/chunk-stamp.ts';
import { createFrameFit } from '../../web/selkies-web-core/lib/fit-frame.ts';
import { createPresentMeter } from '../../web/selkies-web-core/lib/present-meter.ts';
import { isSkiaWebKit } from '../../web/selkies-web-core/lib/util.ts';

/** The web core's directory. */
export const WEB = join(dirname(fileURLToPath(import.meta.url)), '..', '..', 'web', 'selkies-web-core');
/** The WebSockets core, which holds the workers and worklets as templates. */
export const CORE = join(WEB, 'selkies-ws-core.ts');

/** A web-core source as the JavaScript its types strip to, which is what a worker runs. */
export const readSource = (file: string): string => stripTypeScriptTypes(readFileSync(file, 'utf8'));

/** Where `name` is declared in `text`: a `const` or a function, exported or
 *  not, with the offset of its value and of the declaration itself. */
function declarationAt(text: string, name: string): { from: number; value: number } {
    const m = new RegExp(`(?:export\\s+)?(?:const|function)\\s+${name}\\s*(=\\s*|\\()`).exec(text);
    if (!m) throw new Error(`${name} not found`);
    const from = m.index + (m[0].startsWith('export ') ? 'export '.length : 0);
    return { from, value: m[1].startsWith('=') ? m.index + m[0].length : from };
}

/** The declaration of `name`, to the brace that closes it, so a helper the
 *  page splices into a worker comes over whole and stays a valid expression. */
export function declaration(text: string, name: string): string {
    const { from } = declarationAt(text, name);
    let depth = 0, seen = false;
    for (let i = from; i < text.length; i++) {
        const c = text[i];
        if (c === '{') { depth++; seen = true; }
        else if (c === '}') { depth--; if (seen && depth === 0) return text.slice(from, i + 1); }
    }
    throw new Error(`${name} is unterminated`);
}

/** The value `name` is declared with, up to the semicolon that ends it. */
export function literal(text: string, name: string): string {
    const { value } = declarationAt(text, name);
    let depth = 0;
    for (let i = value; i < text.length; i++) {
        const c = text[i];
        if ('{[('.includes(c)) depth++;
        else if (')]}'.includes(c)) depth--;
        else if (c === ';' && depth === 0) return text.slice(value, i);
    }
    throw new Error(`${name} is unterminated`);
}

/** Helpers a worker source splices in that its own module imports. */
const IMPORTED: Record<string, (...args: never[]) => unknown> = { createStripeClock, createChunkStamp, createFrameFit, createPresentMeter, isSkiaWebKit };

/** Module sources a worker splices in whole, by their `?raw` import name. */
const RAW_SOURCES: Record<string, () => string> = {
    wireCodecsSource: () => readSource(join(WEB, 'lib', 'wire-codecs.ts')).replace(/^export /gm, ''),
    decodeGateSource: () => readSource(join(WEB, 'lib', 'decode-gate.ts')).replace(/^export /gm, ''),
    decodePaceSource: () => readSource(join(WEB, 'lib', 'decode-pace.ts')).replace(/^export /gm, ''),
};

/** Resolves one `${...}` the client would have interpolated. */
function splice(text: string, token: string): string {
    let m = token.match(/^(\w+)\.replace\(\/\^export \/gm, ''\)$/);
    if (m && RAW_SOURCES[m[1]]) return RAW_SOURCES[m[1]]();
    m = token.match(/^(\w+)\.toString\(\)$/);
    if (m) {
        return IMPORTED[m[1]]
            ? IMPORTED[m[1]].toString()
            : declaration(text, m[1]).replace(/^const \S+ = /, '');
    }
    // The encoder candidate table is spliced empty: the track path is what is
    // under test here, and the ladder that reads it has coverage of its own.
    if (/^JSON\.stringify\(\w+\)$/.test(token)) return '[]';
    if (/^\w+$/.test(token)) return literal(text, token);
    throw new Error(`no rule for \${${token}}`);
}

/**
 * The named worker source out of `text`, spliced the way the client splices it
 * before handing it to `new Worker`: every value it interpolates is read back
 * out of the same module, and the template's own escapes are resolved by
 * evaluating it as one.
 */
export function workerSource(text: string, name: string): string {
    const open = text.indexOf(`const ${name} = \``);
    if (open < 0) throw new Error(`${name} not found`);
    const start = text.indexOf('`', open) + 1;
    // The first backtick an even number of backslashes deep closes it; the
    // ones the worker code uses itself are escaped.
    let end = -1;
    for (let i = start; i < text.length; i++) {
        if (text[i] !== '`') continue;
        let slashes = 0;
        while (text[i - 1 - slashes] === '\\') slashes++;
        if (slashes % 2 === 0) { end = i; break; }
    }
    if (end < 0) throw new Error(`${name} is unterminated`);
    let body = text.slice(start, end);
    // Resolved against the module minus this template: the worker source
    // declares the same names it interpolates, and a search over the whole
    // file would answer with the placeholder instead of the value.
    const outside = text.slice(0, open) + text.slice(end);
    const values: string[] = [];
    body = body.replace(/(^|[^\\])\$\{([^{}]*(?:\{[^{}]*\}[^{}]*)*)\}/g, (whole, lead: string, token: string) => {
        values.push(splice(outside, token));
        return `${lead}__SPLICE${values.length - 1}__`;
    });
    // The module holds this as a template literal, so its `\$` and backtick
    // escapes are the outer template's; evaluating one resolves them exactly.
    const resolved: unknown = vm.runInThisContext('`' + body + '`');
    if (typeof resolved !== 'string') throw new Error(`${name} does not evaluate to text`);
    body = resolved;
    return values.reduce((out, value, i) => out.replace(`__SPLICE${i}__`, () => value), body);
}

/** A worker's global scope, as far as an audit drives it. */
export interface WorkerScope {
    postMessage: (message: unknown) => void;
    onmessage: ((event: { data: Record<string, unknown> }) => unknown) | null;
    close?: () => void;
}

/**
 * Starts a worker source over a scope, as `new Worker` would: the source runs
 * in a function of its own, so a second start declares its names afresh, and
 * it reads `self` and whatever other globals the audit set up before.
 * @param source The worker's source.
 * @param filename The name its stack traces carry.
 * @param scope The scope it takes as `self`.
 */
export function startWorker(source: string, filename: string, scope: WorkerScope): void {
    Object.assign(globalThis, { self: scope });
    const run: unknown = vm.runInThisContext(`(function () {\n${source}\n})`,
        { filename, importModuleDynamically: vm.constants.USE_MAIN_CONTEXT_DEFAULT_LOADER });
    if (typeof run !== 'function') throw new Error(`${filename} is not a script`);
    Reflect.apply(run, undefined, []);
}

/**
 * Hands a started worker a message on its own port.
 * @param scope The worker's scope.
 * @param data The message.
 * @returns What its handler returned, a promise for an async one.
 */
export function post(scope: WorkerScope, data: Record<string, unknown>): unknown {
    if (!scope.onmessage) throw new Error('the worker listens on no port');
    return scope.onmessage({ data });
}
