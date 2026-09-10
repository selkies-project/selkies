/**
 * The density a display page streams at: stream pixels per CSS pixel.
 *
 * A page asks for its CSS size at the density of the screen it is on, so the
 * stream is one pixel per device pixel there. The primary streams at the
 * device pixel ratio, or under CSS scaling at the ratio divided by the
 * UI-scaling pick, which the desktop then does not apply as a DPI: windows
 * keep the same proportion of the screen either way. A manual resolution is
 * the exception both ways round: it is the framebuffer the operator asked
 * for, so there is nothing for the pick to divide and it governs the desktop
 * DPI instead, and a page dividing here as well would apply it twice --
 * publishing a density its own box does not draw at. On Wayland each screen
 * takes its own page's DPI; on X11 the desktop has one, the primary's, so a
 * secondary on a screen of another density shows the UI at another physical
 * size rather than at another resolution.
 * @param {{useCssScaling: boolean, localScale: (number|undefined),
 *     manual: (boolean|undefined)}} page `localScale` is the UI-scaling pick
 *     as a factor (1 is 100%); `manual` is whether this page's resolution is
 *     the operator's own.
 * @returns {number}
 */
export function streamDensity({ useCssScaling, localScale, manual }) {
    const dpr = window.devicePixelRatio || 1;
    const stretch = (Number.isFinite(localScale) && localScale > 0) ? localScale : 1;
    return (useCssScaling && !manual) ? dpr / stretch : dpr;
}

/** `scaling_dpi` stops in 25% steps from 96; densities between them snap to the nearest. */
export const DPI_STOPS = [96, 120, 144, 168, 192, 216, 240, 264, 288];
/** The rows a 96 DPI desktop is for, which a resolution is read against. */
export const DPI_UNITY_ROWS = 1080;

/** The stop nearest a density, clamped at both ends. */
function snapDpi(target) {
    return DPI_STOPS.reduce((prev, cur) =>
        Math.abs(cur - target) < Math.abs(prev - target) ? cur : prev);
}

/**
 * The default `scaling_dpi`: the local display scaling as a desktop DPI, so
 * the remote UI comes out the size of the local one — 1.5 is 144, 2 is 192,
 * snapped to the nearest stop and clamped at both ends.
 * @returns {number} One of `DPI_STOPS`.
 */
export function autoScalingDpi() {
    const dpr = window.devicePixelRatio || 1;
    return snapDpi(Math.round(dpr * 4) * 24);
}

/**
 * The default `scaling_dpi` for a resolution the operator set: that
 * framebuffer read as a desktop DPI, so one asked for in 4K is not drawn with
 * the UI of a 1080p desktop, nor a small one with a dense laptop's.
 *
 * Read off the shorter side, against the rows 96 DPI is for: an ultrawide is
 * wide rather than dense, and a screen turned portrait is the same screen. The
 * local display scaling says nothing about it — the operator's number is the
 * framebuffer whatever screen shows it — so it stands in only for a size not
 * yet known.
 * @param {number} width Pixels the operator asked for.
 * @param {number} height
 * @returns {number} One of `DPI_STOPS`.
 */
export function resolutionScalingDpi(width, height) {
    const rows = Math.min(Number(width) || 0, Number(height) || 0);
    if (!(rows > 0)) return autoScalingDpi();
    return snapDpi(96 * rows / DPI_UNITY_ROWS);
}

/**
 * The scale a page publishes with the layout: the remote pixels per CSS pixel
 * of the stream box it presents.
 *
 * Measured off that box, since a manual resolution or the pixel cap makes the
 * ratio something other than the density. Only while the box is showing the
 * stream the server realized, though: a box still holding the stream from
 * before a resize measures that one's ratio instead, and a neighbouring
 * display would take the answer for this page's density and stream at it. The
 * density the request was built with stands in until the two agree.
 * @param {{stream: (number[]|null), css: (number[]|null),
 *     realized: (number[]|null), density: number}} box Sink pixels, the CSS
 *     box drawing them, the size the server realized, and the density.
 * @returns {number}
 */
export function publishedScale({ stream, css, realized, density }) {
    if (!stream || !css || !realized || !(css[0] > 0) || !(css[1] > 0)) return density;
    if (stream[0] !== realized[0] || stream[1] !== realized[1]) return density;
    // The tighter dimension is the one a fitted box scales by.
    return Math.max(stream[0] / css[0], stream[1] / css[1]);
}
