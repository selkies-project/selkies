/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Experimental RGB still overlay for the static-refinement probe.
 *
 * This is a laboratory fixture, not a streaming-core feature. It accepts a
 * caller-supplied lossless snapshot and puts it over one visible HTML video.
 * Client-observed frames, input, and geometry changes invalidate the overlay
 * and pending work. This does not establish server-side scene ordering: a
 * capture can already be stale before another video frame reaches the client.
 * The screenshot API also includes the cursor and captures the X11 root.
 * Those limitations must be resolved before integration into either core.
 * @module
 */

/**
 * Install an opt-in overlay without modifying the video's pixels or settings.
 *
 * @param {HTMLVideoElement} video Visible, already-playing video sink.
 * @param {(signal: AbortSignal) => Promise<Blob>} capture Lossless snapshot provider.
 * @returns {{refine: Function, invalidate: Function, dispose: Function,
 *   status: Function, canvas: HTMLCanvasElement}} Probe controls and readback canvas.
 */
export function createStaticRefinement(video, capture) {
    if (!(video instanceof HTMLVideoElement) || !video.requestVideoFrameCallback) {
        throw new Error('The experiment requires an HTML video with frame callbacks');
    }
    const canvas = document.createElement('canvas');
    canvas.style.cssText = 'position:fixed;pointer-events:none;z-index:2;max-width:none;max-height:none;display:none';
    document.body.appendChild(canvas);
    const context = canvas.getContext('2d', { alpha: false });
    let generation = 0, frames = 0, disposed = false, shown = false;
    let pending = null, reason = 'video', lastFrame = performance.now();

    /** Geometry and filter are checked again after asynchronous decoding. */
    const geometry = () => {
        const r = video.getBoundingClientRect();
        return [r.x, r.y, r.width, r.height, video.videoWidth, video.videoHeight,
            getComputedStyle(video).imageRendering, devicePixelRatio];
    };

    /** Cancel pending work and reveal the video immediately. */
    const invalidate = (why = 'invalidated') => {
        generation++;
        pending?.abort();
        pending = null;
        shown = false;
        reason = why;
        canvas.style.display = 'none';
    };

    /** Observe presentation without copying any video frame. */
    const onFrame = () => {
        if (disposed) return;
        frames++;
        lastFrame = performance.now();
        if (shown || pending) invalidate('new-video-frame');
        callback = video.requestVideoFrameCallback(onFrame);
    };
    let callback = video.requestVideoFrameCallback(onFrame);
    const onEvent = (event) => invalidate(event.type);
    const events = ['pointerdown', 'pointermove', 'keydown', 'wheel', 'scroll',
        'resize', 'blur', 'pagehide', 'visibilitychange'];
    for (const event of events) window.addEventListener(event, onEvent, true);
    const videoEvents = ['emptied', 'ended', 'loadstart'];
    for (const event of videoEvents) video.addEventListener(event, onEvent);
    const observer = new ResizeObserver(() => invalidate('video-resized'));
    observer.observe(video);
    const styles = new MutationObserver(() => invalidate('video-style'));
    styles.observe(video, { attributes: true, attributeFilter: ['style', 'class', 'width', 'height'] });

    /** Request one bounded snapshot and reject work superseded while in flight. */
    const refine = async () => {
        if (disposed) return { accepted: false, reason: 'disposed' };
        invalidate('requesting');
        const before = { generation, frames, geometry: geometry() };
        const controller = new AbortController();
        pending = controller;
        const timer = setTimeout(() => controller.abort(), 10000);
        const started = performance.now();
        let bitmap;
        try {
            const blob = await capture(controller.signal);
            const received = performance.now();
            bitmap = await createImageBitmap(blob, { colorSpaceConversion: 'none' });
            const decoded = performance.now();
            const current = geometry();
            if (disposed || controller.signal.aborted || before.generation !== generation
                || before.frames !== frames || !video.isConnected || video.readyState < 2
                || !current.every((value, index) => value === before.geometry[index])) {
                return { accepted: false, reason: 'scene-changed' };
            }
            if (bitmap.width !== video.videoWidth || bitmap.height !== video.videoHeight) {
                reason = 'snapshot-size-mismatch';
                return { accepted: false, reason };
            }
            if (current[2] <= 0 || current[3] <= 0 || getComputedStyle(video).display === 'none') {
                reason = 'video-hidden';
                return { accepted: false, reason };
            }
            canvas.width = bitmap.width;
            canvas.height = bitmap.height;
            context.drawImage(bitmap, 0, 0);
            Object.assign(canvas.style, {
                left: current[0] + 'px', top: current[1] + 'px',
                width: current[2] + 'px', height: current[3] + 'px',
                imageRendering: current[6], display: 'block',
            });
            shown = true;
            reason = 'refined';
            return { accepted: true, bytes: blob.size, size: [bitmap.width, bitmap.height],
                box: current.slice(0, 4), dpr: current[7], fetchMs: received - started,
                decodeMs: decoded - received, readyMs: performance.now() - started };
        } catch (error) {
            if (before.generation === generation) reason = controller.signal.aborted ? 'aborted' : 'capture-failed';
            return { accepted: false, reason: controller.signal.aborted ? 'aborted' : 'capture-failed' };
        } finally {
            bitmap?.close();
            clearTimeout(timer);
            if (pending === controller) pending = null;
        }
    };

    /** Remove the overlay, observers, callback, and input listeners. */
    const dispose = () => {
        if (disposed) return;
        disposed = true;
        invalidate('disposed');
        video.cancelVideoFrameCallback(callback);
        observer.disconnect();
        styles.disconnect();
        for (const event of events) window.removeEventListener(event, onEvent, true);
        for (const event of videoEvents) video.removeEventListener(event, onEvent);
        canvas.remove();
    };
    return { refine, invalidate, dispose, canvas,
        status: () => ({ frames, generation, shown, pending: pending !== null,
            reason, idleMs: performance.now() - lastFrame }) };
}
