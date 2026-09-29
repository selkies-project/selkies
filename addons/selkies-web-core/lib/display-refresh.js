/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The refresh rate of the display a page is shown on, measured from its
 * animation frames, and the frame rate a stream matching it runs at.
 *
 * A stream whose rate differs from the display's even slightly drifts against
 * its refresh: at 60 frames per second on a display refreshing at 59.94, one
 * frame in a thousand has no refresh of its own and is never shown, a skip
 * every 17 s, and a stream slower than its display shows a frame twice as
 * often. Matched, every frame lands on a refresh of its own. No web API names
 * the refresh rate, so the page measures it: the timestamps
 * requestAnimationFrame hands a callback fall on the display's refreshes, a
 * frame the page was too busy for costing a whole refresh, so a straight line
 * through them against the count of refreshes has the refresh period for its
 * slope, finer than the timer's resolution. The median of the short intervals
 * (those near the tenth percentile, which a page busy for most of its frames
 * still leaves at one refresh) sets the refresh count of each step for a first
 * fit, whose period sets it for the second; an interval off that cadence or
 * longer than `GAP_MS` (a page hidden or throttled meanwhile) starts a new run
 * of the fit, and frames mostly off any cadence measure nothing. The fit
 * stands once its standard error is within `PRECISION` of the period, a tenth
 * of the step between 60 and 59.94.
 *
 * A measurement within 0.1% of a whole number or of an NTSC rate (N * 1000 /
 * 1001, 59.94 being 60000/1001) is that rate exactly, since displays run at
 * those and the measurement's own error is smaller; any other is kept to a
 * hundredth of a frame per second. The server carries the rate as the
 * fraction it names down to every encoder.
 *
 * The display is measured for two seconds, longer where the frames are too
 * rough for the fit to stand by then (up to `MAX_MEASURE_MS`), when the page
 * loads, again whenever the page becomes visible, and whenever the window may
 * have moved to another display: its device pixel ratio or, where the engine
 * reports it, its screen changed, or it entered or left full screen.
 * @module
 */

/** The stored frame rate that asks for the display's own. */
export const FRAMERATE_DISPLAY = 'display';

/** An interval at least this long is a gap in the frames, not a refresh, in ms. */
export const GAP_MS = 250;
/** The fewest refreshes a measurement stands on. */
export const MIN_FRAMES = 30;
/** How far a measurement may lie from a standard rate and be it, as a share of it. */
export const SNAP_TOLERANCE = 0.001;
/** How long one measurement collects frames for at least, and at most, in ms. */
export const MEASURE_MS = 2000;
export const MAX_MEASURE_MS = 8000;
/** The standard error a measurement stands at, as a share of the period. */
export const PRECISION = 0.0001;

/** The refresh rates a measurement may name, in Hz. */
const MIN_HZ = 8;
const MAX_HZ = 1000;
/** How far from a whole count of refreshes an interval may be, as a share of one. */
const OFF_CADENCE = 0.4;
/** The share of intervals that must lie on the cadence. */
const ON_CADENCE = 0.8;
/** How far the frames may stray from the fitted cadence, RMS, as a share of a refresh. */
const MAX_SCATTER = 0.15;
/** How far a new measurement must move from the last to replace it, as a share of it. */
const CHANGE = 0.0005;
/** How often a measurement past `MEASURE_MS` tries the fit, in ms. */
const RETRY_MS = 250;

/**
 * The display's refresh rate from the timestamps of consecutive animation
 * frames, or null where they say too little: fewer than `MIN_FRAMES` refreshes
 * on a steady cadence, a cadence no display runs at, or a fit short of
 * `PRECISION`.
 * @param {number[]} stamps Callback timestamps in ms, ascending.
 * @returns {?number} Refreshes per second.
 */
export function estimateRefresh(stamps) {
  const steady = [];
  for (let i = 1; i < stamps.length; i++) {
    const d = stamps[i] - stamps[i - 1];
    if (d > 0 && d < GAP_MS) steady.push(d);
  }
  if (steady.length < MIN_FRAMES) return null;
  const sorted = steady.slice().sort((a, b) => a - b);
  const short = sorted.filter((d) => d <= 1.5 * sorted[Math.floor(sorted.length / 10)]);
  let period = short[short.length >> 1];
  for (let pass = 0; pass < 2; pass++) {
    if (!(period >= 1000 / MAX_HZ && period <= 1000 / MIN_HZ)) return null;
    period = fitPeriod(stamps, period, steady.length);
    if (period === null) return null;
  }
  const hz = 1000 / period;
  return hz >= MIN_HZ && hz <= MAX_HZ ? hz : null;
}

/**
 * The refresh period of `stamps` fitted against their refresh counts, each
 * step counted in whole periods of `coarse`, or null where fewer than
 * `ON_CADENCE` of the `steady` intervals or `MIN_FRAMES` refreshes lie on the
 * cadence, the frames stray from it by more than `MAX_SCATTER`, or the fit's
 * standard error is past `PRECISION`.
 * @param {number[]} stamps
 * @param {number} coarse
 * @param {number} steady The intervals shorter than `GAP_MS`.
 * @returns {?number}
 */
function fitPeriod(stamps, coarse, steady) {
  const runs = [];
  let run = [[0, stamps[0]]];
  let onCadence = 0;
  for (let i = 1; i < stamps.length; i++) {
    const d = stamps[i] - stamps[i - 1];
    const k = Math.round(d / coarse);
    if (d <= 0 || d >= GAP_MS || k < 1 || Math.abs(d - k * coarse) > OFF_CADENCE * coarse) {
      runs.push(run);
      run = [[0, stamps[i]]];
    } else {
      run.push([run[run.length - 1][0] + k, stamps[i]]);
      onCadence++;
    }
  }
  runs.push(run);
  if (onCadence < ON_CADENCE * steady) return null;
  const fitted = runs.filter((r) => r.length >= 3).map((r) => ({
    r,
    n: r.reduce((s, p) => s + p[0], 0) / r.length,
    t: r.reduce((s, p) => s + p[1], 0) / r.length,
  }));
  let sxy = 0, sxx = 0, refreshes = 0;
  for (const { r, n, t } of fitted) {
    for (const [x, y] of r) {
      sxy += (x - n) * (y - t);
      sxx += (x - n) * (x - n);
    }
    refreshes += r[r.length - 1][0];
  }
  if (refreshes < MIN_FRAMES || sxx === 0) return null;
  const period = sxy / sxx;
  let scatter = 0, points = 0;
  for (const { r, n, t } of fitted) {
    for (const [x, y] of r) {
      scatter += (y - t - (x - n) * period) ** 2;
      points++;
    }
  }
  const rms = Math.sqrt(scatter / points);
  if (rms > MAX_SCATTER * period || rms / Math.sqrt(sxx) > PRECISION * period) return null;
  return period;
}

/**
 * The rate a measured refresh names: the whole number or the NTSC rate nearest
 * it where one lies within `SNAP_TOLERANCE`, else the measurement to a
 * hundredth.
 * @param {number} hz
 * @returns {number}
 */
export function snapRefresh(hz) {
  const whole = Math.round(hz);
  const ntsc = Math.round(hz * 1.001) * 1000 / 1001;
  const nearest = Math.abs(ntsc - hz) < Math.abs(whole - hz) ? ntsc : whole;
  return Math.abs(nearest - hz) <= SNAP_TOLERANCE * nearest ? nearest : Math.round(hz * 100) / 100;
}

/**
 * The frame rate that matches a display refreshing at `hz` inside a server's
 * span: the refresh itself, else its largest whole division the span holds, so
 * each frame still lasts a whole number of refreshes, else the span's nearer
 * end.
 * @param {number} hz
 * @param {number} min
 * @param {number} max
 * @returns {number}
 */
export function matchDisplay(hz, min, max) {
  for (let k = 1; hz / k >= min; k++) {
    if (hz / k <= max) return hz / k;
  }
  return hz < min ? min : max;
}

/**
 * A frame rate as the dashboards show it: to a hundredth, so 60000/1001 reads
 * 59.94.
 * @param {number} rate
 * @returns {number}
 */
export function framerateLabel(rate) {
  return Math.round(rate * 100) / 100;
}

/**
 * Whether a page's frame rate follows its display: where the stored choice is
 * `FRAMERATE_DISPLAY` and the server does not lock the rate. With nothing
 * chosen the page asks for nothing, since the server's pacing strays from the
 * rate asked of it by more than a matched rate would remove.
 * @param {?string} stored The stored value, null where none is.
 * @param {?{min: number, max: number}} span
 *     The server's framerate setting, null before it arrives.
 * @returns {boolean}
 */
export function followsDisplay(stored, span) {
  return stored === FRAMERATE_DISPLAY && !(span && span.min === span.max);
}

/**
 * The frame rate a page asks the server for: the one stored, or where the
 * page follows its display (`followsDisplay`) the display's own inside the
 * server's span; null asks for nothing, as does a display not measured yet.
 * @param {?string} stored The stored value, null where none is.
 * @param {?number} displayRate The measured refresh, null where unknown.
 * @param {?{min: number, max: number}} span
 *     The server's framerate setting, null before it arrives.
 * @returns {?number}
 */
export function requestedFramerate(stored, displayRate, span) {
  if (stored !== null && stored !== FRAMERATE_DISPLAY) {
    const rate = parseFloat(stored);
    return Number.isFinite(rate) ? rate : null;
  }
  if (!span || !displayRate || !followsDisplay(stored, span)) return null;
  return matchDisplay(displayRate, span.min, span.max);
}

/**
 * Measures the display while the page runs, calling `onRate` with the first
 * rate and each that differs from the last by more than `CHANGE`.
 * @param {function(number): void} onRate
 * @returns {{rate: function(): ?number, measure: function(): void, stop: function(): void}}
 */
export function watchDisplayRefresh(onRate) {
  let rate = null;
  let stamps = [];
  let handle = 0;
  let measuring = false;
  let tryAt = MEASURE_MS;
  let density = null;
  const done = (hz) => {
    measuring = false;
    stamps = [];
    if (hz === null) return;
    const snapped = snapRefresh(hz);
    if (rate === null || Math.abs(snapped - rate) > CHANGE * rate) {
      rate = snapped;
      onRate(rate);
    }
  };
  const frame = (t) => {
    if (!measuring) return;
    stamps.push(t);
    const span = t - stamps[0];
    if (span >= tryAt) {
      tryAt = span + RETRY_MS;
      const hz = estimateRefresh(stamps);
      if (hz !== null || span >= MAX_MEASURE_MS) {
        done(hz);
        return;
      }
    }
    handle = requestAnimationFrame(frame);
  };
  const measure = () => {
    if (measuring || document.visibilityState !== 'visible') return;
    measuring = true;
    stamps = [];
    tryAt = MEASURE_MS;
    handle = requestAnimationFrame(frame);
  };
  const onVisibility = () => {
    if (document.visibilityState === 'visible') {
      measure();
    } else if (measuring) {
      cancelAnimationFrame(handle);
      measuring = false;
      stamps = [];
    }
  };
  const watchDensity = () => {
    density = window.matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`);
    density.addEventListener('change', onDensity, { once: true });
  };
  const onDensity = () => {
    watchDensity();
    measure();
  };
  const screen = window.screen;
  const screenEvents = !!(screen && typeof screen.addEventListener === 'function');
  document.addEventListener('visibilitychange', onVisibility);
  document.addEventListener('fullscreenchange', measure);
  if (screenEvents) screen.addEventListener('change', measure);
  watchDensity();
  measure();
  return {
    rate: () => rate,
    measure,
    stop() {
      if (measuring) cancelAnimationFrame(handle);
      measuring = false;
      document.removeEventListener('visibilitychange', onVisibility);
      document.removeEventListener('fullscreenchange', measure);
      if (screenEvents) screen.removeEventListener('change', measure);
      density.removeEventListener('change', onDensity);
    },
  };
}
