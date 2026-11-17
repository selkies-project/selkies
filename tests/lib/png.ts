/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// PNG images made for the suites, the size a PNG says it has, and the
// pixels of one a page's screenshot gives.
import { crc32, deflateSync, inflateSync } from 'node:zlib';

function chunk(kind: string, body: Buffer): Buffer {
    const head = Buffer.alloc(4);
    head.writeUInt32BE(body.length);
    const payload = Buffer.concat([Buffer.from(kind, 'latin1'), body]);
    const tail = Buffer.alloc(4);
    tail.writeUInt32BE(crc32(payload) >>> 0);
    return Buffer.concat([head, payload, tail]);
}

/**
 * An 8-bit RGB PNG of `rgb`, `height` rows of `width * 3` bytes each.
 * @param width Its width.
 * @param height Its height.
 * @param rgb The pixels, row after row.
 * @returns The file.
 */
export function encodePng(width: number, height: number, rgb: Buffer): Buffer {
    const rows: Buffer[] = [];
    for (let y = 0; y < height; y++) rows.push(Buffer.from([0]), rgb.subarray(y * width * 3, (y + 1) * width * 3));
    const header = Buffer.alloc(13);
    header.writeUInt32BE(width, 0);
    header.writeUInt32BE(height, 4);
    header.set([8, 2, 0, 0, 0], 8);
    return Buffer.concat([Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]), chunk('IHDR', header), chunk('IDAT', deflateSync(Buffer.concat(rows))), chunk('IEND', Buffer.alloc(0))]);
}

/**
 * An 8x8 PNG whose pixels follow `seed`, so two of them never compare equal.
 * @param seed The pattern.
 * @returns The file.
 */
export function seededPng(seed: number): Buffer {
    const side = 8;
    const rgb = Buffer.alloc(side * side * 3);
    for (let y = 0; y < side; y++) {
        for (let x = 0; x < side; x++) rgb.set([(seed + y) % 256, (seed * 3) % 256, (y * 7) % 256], (y * side + x) * 3);
    }
    return encodePng(side, side, rgb);
}

/**
 * A PNG of noise: incompressible, so its size is what crosses a link.
 * @param width Its width.
 * @param height Its height.
 * @param seed The noise.
 * @returns The file.
 */
export function noisePng(width: number, height: number, seed: number): Buffer {
    // mulberry32: a stream of the same bytes on every run.
    let state = seed >>> 0;
    const rgb = Buffer.alloc(width * height * 3);
    for (let i = 0; i < rgb.length; i += 4) {
        state = (state + 0x6d2b79f5) >>> 0;
        let t = state;
        t = Math.imul(t ^ (t >>> 15), t | 1);
        t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
        const word = (t ^ (t >>> 14)) >>> 0;
        for (let b = 0; b < 4 && i + b < rgb.length; b++) rgb[i + b] = (word >>> (b * 8)) & 0xff;
    }
    return encodePng(width, height, rgb);
}

/**
 * The width and height a PNG's header gives.
 * @param data The bytes.
 * @returns The size, or null when the bytes are no PNG.
 */
export function pngSize(data: Buffer | null): [number, number] | null {
    if (data === null || data.length < 24 || data.readUInt32BE(0) !== 0x89504e47 || data.toString('latin1', 12, 16) !== 'IHDR') return null;
    return [data.readUInt32BE(16), data.readUInt32BE(20)];
}

/** A decoded image: `channels` bytes a pixel, row after row. */
export interface Decoded {
    width: number;
    height: number;
    channels: number;
    data: Buffer;
}

/**
 * The pixels of an 8-bit, non-interlaced grey, RGB or RGBA PNG, such as a
 * page's screenshot.
 * @param png The file.
 * @returns The image.
 * @throws {Error} For a PNG of another kind.
 */
export function decodePng(png: Buffer): Decoded {
    const size = pngSize(png);
    if (size === null) throw new Error('not a PNG');
    const [width, height] = size;
    const depth = png[24];
    const kind = png[25];
    const channels = { 0: 1, 2: 3, 4: 2, 6: 4 }[kind];
    if (depth !== 8 || channels === undefined || png[28] !== 0) throw new Error(`PNG of depth ${depth}, colour type ${kind}, interlace ${png[28]}`);
    const idat: Buffer[] = [];
    for (let at = 8; at + 8 <= png.length;) {
        const length = png.readUInt32BE(at);
        const type = png.toString('latin1', at + 4, at + 8);
        if (type === 'IDAT') idat.push(png.subarray(at + 8, at + 8 + length));
        if (type === 'IEND') break;
        at += 12 + length;
    }
    const raw = inflateSync(Buffer.concat(idat));
    const stride = width * channels;
    const data = Buffer.alloc(stride * height);
    for (let y = 0; y < height; y++) {
        const filter = raw[y * (stride + 1)];
        const row = raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1));
        const out = y * stride;
        for (let x = 0; x < stride; x++) {
            const a = x >= channels ? data[out + x - channels] : 0;
            const b = y > 0 ? data[out - stride + x] : 0;
            const c = x >= channels && y > 0 ? data[out - stride + x - channels] : 0;
            let pred = 0;
            if (filter === 1) pred = a;
            else if (filter === 2) pred = b;
            else if (filter === 3) pred = (a + b) >> 1;
            else if (filter === 4) {
                const p = a + b - c;
                const pa = Math.abs(p - a);
                const pb = Math.abs(p - b);
                const pc = Math.abs(p - c);
                pred = pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
            }
            data[out + x] = (row[x] + pred) & 0xff;
        }
    }
    return { width, height, channels, data };
}

/**
 * The RGB of one pixel of a decoded image.
 * @param image The image.
 * @param x Its column.
 * @param y Its row.
 * @returns Red, green and blue.
 */
export function rgbAt(image: Decoded, x: number, y: number): [number, number, number] {
    const at = (y * image.width + x) * image.channels;
    if (image.channels < 3) return [image.data[at], image.data[at], image.data[at]];
    return [image.data[at], image.data[at + 1], image.data[at + 2]];
}
