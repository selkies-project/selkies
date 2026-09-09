/**
 * The density a display page streams at: stream pixels per CSS pixel.
 *
 * The desktop renders its UI at one DPI, the primary page's, so streaming
 * every page at the primary's density is what shows that UI at the same
 * physical size on screens of different pixel densities: a page asks for its
 * CSS size at that density and the browser resamples the stream by the ratio
 * to its own. The primary streams at its own density (1 under CSS scaling); a
 * secondary at the primary's, which the primary reports as its display scale
 * and the server carries in every layout broadcast, and at its own until that
 * arrives. A shared viewer follows the controller and keeps its own.
 * @param {{displayId: string, layouts: (Object|null), useCssScaling: boolean,
 *     shared: boolean}} page
 * @returns {number}
 */
export function streamDensity({ displayId, layouts, useCssScaling, shared }) {
    const own = useCssScaling ? 1 : (window.devicePixelRatio || 1);
    if (shared || !displayId || displayId === 'primary') return own;
    const primary = layouts && layouts.primary;
    const scale = primary ? Number(primary.scale) : NaN;
    if (!Number.isFinite(scale) || scale <= 0) return own;
    // A measured scale carries the rounding of the primary's aligned buffer;
    // the request is sized on the density it stands for.
    return Math.round(scale * 100) / 100;
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
