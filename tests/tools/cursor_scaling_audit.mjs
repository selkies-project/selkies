/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Cursor size and hotspot follow the displayed stream, independently of the
 * density requested from the server. Drives the shared Input used by both
 * transports, including the absolute and relative coordinate paths.
 * @module
 */
import { Input } from '../../addons/selkies-web-core/lib/input.js';

let failed = 0;
function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [cursor-scaling] ${label}  ${detail}`);
}
const near = (a, b) => Math.abs(a - b) < 1e-6;

/** An input overlay and a decoded sink, with mutable geometry. */
function fixture({ dpr, manual, canvas = false, width = 3840, height = 2160,
                   boxWidth = 1600, boxHeight = 900, left = 160 }) {
    globalThis.window = { devicePixelRatio: dpr, manualResolution: manual && !canvas,
        manual_resolution: manual && canvas };
    const rect = { left, top: 0, width: boxWidth, height: boxHeight };
    const sink = { tagName: canvas ? 'CANVAS' : 'VIDEO', width, height,
        videoWidth: canvas ? undefined : width, videoHeight: canvas ? undefined : height,
        style: { objectFit: 'contain' }, getBoundingClientRect: () => rect };
    const element = { style: { setProperty(k, v) { this[k] = v; } },
        getBoundingClientRect: () => ({ left: 0, top: 0, width: 1920, height: 900 }),
        offsetWidth: 1920, offsetHeight: 900 };
    globalThis.document = { getElementById: id => id === (canvas ? 'videoCanvas' : 'stream') ? sink : null,
        pointerLockElement: null };
    const input = Object.create(Input.prototype);
    Object.assign(input, { element, isSharedMode: false, useCssScaling: false,
        _streamDensity: dpr, _cursorImageBitmap: { width: 48, height: 48 },
        _cursorBase64Data: 'fixture', _rawHotspotX: 12, _rawHotspotY: 6,
        cursorDiv: { style: { display: 'block' } }, cursorImg: { clearRect() {}, drawImage() {} },
        cursorHotspot: {}, _latestMouseX: 660, _latestMouseY: 300 });
    Input._cursorImageSetFn = 'image-set';
    return { input, sink, rect };
}

for (const canvas of [false, true]) {
    for (const dpr of [1, 1.25, 2]) {
        for (const [mode, width, height, boxWidth, boxHeight, density] of [
            ['fit', 3840, 2160, 1600, 900, 2.4],
            ['fit small', 3840, 2160, 1280, 720, 3],
            ['exact', 3840, 2160, 3840 / dpr, 2160 / dpr, dpr],
            ['automatic', 1920 * dpr, 900 * dpr, 1920, 900, dpr],
            ['upscale', 1280, 720, 1920, 1080, 2 / 3],
        ]) {
            const { input } = fixture({ dpr, canvas, manual: mode !== 'automatic',
                width, height, boxWidth, boxHeight });
            const label = `${canvas ? 'WebSocket canvas' : 'WebRTC video'} ${mode} DPR ${dpr}`;
            for (const size of [24, 48, 96]) {
                input._cursorImageBitmap = { width: size, height: size };
                input._drawAndScaleCursor();
                check(`${label}: ${size}px canvas cursor and hotspot`,
                    near(parseFloat(input.cursorDiv.style.width), size / density) &&
                    near(input.cursorHotspot.x, 12 / density) && near(input.cursorHotspot.y, 6 / density),
                    JSON.stringify([input.cursorDiv.style.width, input.cursorHotspot]));
            }
            input._updateBrowserCursor();
            const css = input.element.style.cursor;
            check(`${label}: CSS density and hotspot`, density === 1
                ? css.endsWith('12 6, default') && !css.includes('image-set')
                : css.includes(` ${density}x) ${Math.round(12 / density)} ${Math.round(6 / density)}, default`), css);
            check(`${label}: input density stays independent`, input._inputDpr() === (mode === 'automatic' ? dpr : 1));
            if (mode !== 'automatic') {
                const box = input._streamBox();
                input._calculateTouchCoordinates({ clientX: box.left + 120 / density, clientY: box.top + 60 / density });
                check(`${label}: hotspot lands on the remote point`, input.x === 120 && input.y === 60,
                    JSON.stringify([input.x, input.y]));
                check(`${label}: relative motion has one scale`, near(input._pointerScale().x, density));
            }
        }
    }
}

{
    const { input, sink } = fixture({ dpr: 2, manual: true });
    sink.videoWidth = sink.videoHeight = 0;
    check('before video metadata, use the requested density', input._cursorDensity() === 2);
    sink.videoWidth = 3840; sink.videoHeight = 2160;
    check('first decoded dimensions replace the fallback', near(input._cursorDensity(), 2.4));
}
{
    const { input, rect } = fixture({ dpr: 1, manual: true, boxWidth: 1920, boxHeight: 900, left: 0 });
    check('object-fit contains the frame inside the element', near(input._cursorDensity(), 2.4));
    rect.width = 1280; rect.height = 720;
    input._drawAndScaleCursor();
    check('a resize rescales a cached bitmap', parseFloat(input.cursorDiv.style.width) === 16);
    input._cursorBase64Data = null;
    input._updateBrowserCursor();
    check('an empty cursor stays hidden', input.element.style.cursor === 'none');
}
{
    const { input } = fixture({ dpr: 1, manual: true });
    input.inputAttached = true;
    input.use_browser_cursors = true;
    input._cursorBitmapFromBase64 = async () => ({ width: 96, height: 96 });
    await input.updateServerCursor({ handle: 1, curdata: 'larger', hotx: 12, hoty: 6 });
    await input.setUseBrowserCursors(false);
    check('switching to canvas uses the latest application cursor size',
        parseFloat(input.cursorDiv.style.width) === 40, input.cursorDiv.style.width);
}
{
    const { input, sink } = fixture({ dpr: 2, manual: false });
    let reads = 0;
    sink.getBoundingClientRect = () => { reads++; return { left: 0, top: 0, width: 1600, height: 900 }; };
    for (let i = 0; i < 100; i++) input._inputDpr();
    check('input density does not measure cursor layout on pointer motion', reads === 0, reads);
}
process.exitCode = failed ? 1 : 0;
