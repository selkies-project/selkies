/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What reaches the screen, counted by the sink that shows the stream while a
 * dashboard has its stats open: the frames handed to the compositor, how long
 * after they arrived, and the frames that never were -- replaced by a newer
 * one before a display refresh took them, or refused by a sink that could
 * not take more.
 *
 * A `<video>` sink says so itself: requestVideoFrameCallback reports the
 * frames it hands on (`presentationTime`), and getVideoPlaybackQuality counts
 * the frames it showed and those it dropped because a newer one replaced them
 * first (`watchVideo`). A canvas says nothing, so a draw counts as handed on at
 * the next animation frame of the thread that drew it, and every draw before
 * the last one ahead of that frame as replaced. Times are epoch milliseconds
 * (`performance.timeOrigin` plus `performance.now()`), so an arrival stamped
 * on the socket's thread compares with a presentation on whichever thread
 * showed the frame.
 * @module
 */

/**
 * @typedef {Object} PresentFigures What a meter counted since its last take.
 * @property {number} presented Frames handed to the compositor.
 * @property {number} delaySum Their receive-to-present delays, summed, in ms,
 *     over the `delays` of them whose arrival was known.
 * @property {number} delays
 * @property {number} superseded Frames replaced before a refresh took them.
 * @property {number} refused Frames the sink refused.
 */

/**
 * @typedef {Object} PresentMeter
 * @property {function(number): void} drawn A canvas draw of a frame that
 *     arrived at the given epoch time, NaN where that is unknown.
 * @property {function(): void} land Counts the draws so far as handed on now;
 *     for a thread that draws inside its own animation frame.
 * @property {function(number, number): void} presented Frames a sink handed
 *     on, and the receive-to-present delay of the newest in ms (NaN where
 *     unknown).
 * @property {function(number): void} superseded
 * @property {function(): void} refused
 * @property {function(): PresentFigures} take
 * @property {function(): void} reset
 */

/**
 * The counters one sink keeps. Self-contained by design: the video worker
 * embeds this factory by `toString()`, so it references nothing in module
 * scope.
 * @param {?function(function(): void): void} nextFrame Runs a callback at the
 *     thread's next animation frame, or null where the thread has none, in
 *     which case a draw counts as handed on as it is made.
 * @returns {PresentMeter}
 */
export function createPresentMeter(nextFrame) {
  const epoch = () => performance.timeOrigin + performance.now();
  let presented = 0, delaySum = 0, delays = 0, superseded = 0, refused = 0;
  let drawn = 0, drawnArrival = NaN, waiting = false;
  const hand = (count, delay) => {
    presented += count;
    if (delay >= 0) { delaySum += delay; delays++; }
  };
  const land = () => {
    waiting = false;
    if (!drawn) return;
    hand(1, epoch() - drawnArrival);
    superseded += drawn - 1;
    drawn = 0;
  };
  return {
    drawn(arrival) {
      drawn++;
      drawnArrival = arrival;
      if (!nextFrame) land();
      else if (!waiting) { waiting = true; nextFrame(land); }
    },
    land,
    presented: hand,
    superseded(count) { superseded += count; },
    refused() { refused++; },
    take() {
      const figures = { presented, delaySum, delays, superseded, refused };
      presented = 0; delaySum = 0; delays = 0; superseded = 0; refused = 0;
      return figures;
    },
    reset() {
      presented = 0; delaySum = 0; delays = 0; superseded = 0; refused = 0; drawn = 0;
    },
  };
}

/**
 * Follows a `<video>` sink until stopped. Each frame it hands on is reported by
 * requestVideoFrameCallback, whose receive-to-present delay goes to the meter;
 * what it showed and dropped is read from getVideoPlaybackQuality, where a
 * frame replaced by a newer one before a refresh counts as dropped, since
 * Chromium counts such a frame in `presentedFrames` as well. Firefox keeps no
 * playback quality for a live stream, so there the frames the callback
 * reports stand in for the frames shown. A new source restarts the element's
 * counters, which is read as a restart rather than a count going backwards.
 * @param {HTMLVideoElement} video
 * @param {PresentMeter} meter
 * @param {function(VideoFrameCallbackMetadata): number} delayOf The
 *     receive-to-present delay, in ms, of the frame a callback reports; NaN
 *     where its arrival is unknown.
 * @returns {{reported: function(): boolean, stop: function(): void,
 *     read: function(): {shown: number, dropped: number}}} `reported` is
 *     whether the element has reported a frame since the call, which an engine
 *     without the callback never does; `read` gives the frames shown and
 *     dropped since the last read.
 */
export function watchVideo(video, meter, delayOf) {
  const quality = () => (typeof video.getVideoPlaybackQuality === 'function'
    ? video.getVideoPlaybackQuality() : { totalVideoFrames: 0, droppedVideoFrames: 0 });
  const follows = typeof video.requestVideoFrameCallback === 'function';
  let handle = 0, last = -1, handed = 0, handedRead = 0, reported = false;
  const onFrame = (now, metadata) => {
    handed += last >= 0 && metadata.presentedFrames > last ? metadata.presentedFrames - last : 1;
    last = metadata.presentedFrames;
    reported = true;
    meter.presented(0, delayOf(metadata));
    handle = video.requestVideoFrameCallback(onFrame);
  };
  if (follows) handle = video.requestVideoFrameCallback(onFrame);
  const start = quality();
  let total = start.totalVideoFrames, dropped = start.droppedVideoFrames;
  return {
    reported: () => reported,
    stop: () => { if (follows) video.cancelVideoFrameCallback(handle); },
    read() {
      const now = quality();
      const restarted = now.totalVideoFrames < total || now.droppedVideoFrames < dropped;
      const totalDelta = restarted ? now.totalVideoFrames : now.totalVideoFrames - total;
      const droppedDelta = restarted ? now.droppedVideoFrames : now.droppedVideoFrames - dropped;
      total = now.totalVideoFrames;
      dropped = now.droppedVideoFrames;
      const handedDelta = handed - handedRead;
      handedRead = handed;
      return { shown: totalDelta > 0 ? Math.max(0, totalDelta - droppedDelta) : handedDelta, dropped: droppedDelta };
    },
  };
}
