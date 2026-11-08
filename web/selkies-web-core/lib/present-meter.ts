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
 * first (`watchVideo`). Where the playback quality does the counting, the
 * callback is asked for once every `DELAY_SAMPLE_MS`: each one makes the
 * engine run the page's rendering steps for a frame it would otherwise
 * composite without the page's thread. A canvas says nothing, so a draw
 * counts as handed on at the next animation frame of the thread that drew it,
 * and every draw before the last one ahead of that frame as replaced. Times
 * are epoch milliseconds (`performance.timeOrigin` plus `performance.now()`),
 * so an arrival stamped on the socket's thread compares with a presentation
 * on whichever thread showed the frame.
 * @module
 */

/**
 * Spacing, in ms, of the frames whose receive-to-present delay a `<video>` is
 * sampled at once its playback quality counts what it shows.
 */
export const DELAY_SAMPLE_MS = 100;

/** What a meter counted since its last take. */
export interface PresentFigures {
  /** Frames handed to the compositor. */
  presented: number;
  /** Their receive-to-present delays, summed, in ms, over the `delays` of them whose arrival was known. */
  delaySum: number;
  delays: number;
  /** Frames replaced before a refresh took them. */
  superseded: number;
  /** Frames the sink refused. */
  refused: number;
}

/** The counters one sink keeps. */
export interface PresentMeter {
  /** A canvas draw of a frame that arrived at the given epoch time, NaN where that is unknown. */
  drawn: (arrival: number) => void;
  /** Counts the draws so far as handed on now; for a thread that draws inside its own animation frame. */
  land: () => void;
  /** Frames a sink handed on, and the receive-to-present delay of the newest in ms (NaN where unknown). */
  presented: (count: number, delay: number) => void;
  superseded: (count: number) => void;
  refused: () => void;
  take: () => PresentFigures;
  reset: () => void;
}

/**
 * The counters one sink keeps. Self-contained by design: the video worker
 * embeds this factory by `toString()`, so it references nothing in module
 * scope.
 * @param nextFrame Runs a callback at the thread's next animation frame, or null where the thread has none, in which case a draw counts as handed on as it is made.
 * @returns The meter.
 */
export function createPresentMeter(nextFrame: ((cb: () => void) => void) | null): PresentMeter {
  const epoch = (): number => performance.timeOrigin + performance.now();
  let presented = 0, delaySum = 0, delays = 0, superseded = 0, refused = 0;
  let drawn = 0, drawnArrival = NaN, waiting = false;
  const hand = (count: number, delay: number): void => {
    presented += count;
    if (delay >= 0) { delaySum += delay; delays++; }
  };
  const land = (): void => {
    waiting = false;
    if (!drawn) return;
    hand(1, epoch() - drawnArrival);
    superseded += drawn - 1;
    drawn = 0;
  };
  return {
    drawn(arrival: number): void {
      drawn++;
      drawnArrival = arrival;
      if (!nextFrame) land();
      else if (!waiting) { waiting = true; nextFrame(land); }
    },
    land,
    presented: hand,
    superseded(count: number): void { superseded += count; },
    refused(): void { refused++; },
    take(): PresentFigures {
      const figures: PresentFigures = { presented, delaySum, delays, superseded, refused };
      presented = 0; delaySum = 0; delays = 0; superseded = 0; refused = 0;
      return figures;
    },
    reset(): void {
      presented = 0; delaySum = 0; delays = 0; superseded = 0; refused = 0; drawn = 0;
    },
  };
}

/** What a video watch read. */
export interface VideoPlaybackRead {
  shown: number;
  dropped: number;
}

/** A video watch. */
export interface VideoWatch {
  /** Whether the element has reported a frame since the call, which an engine without the callback never does. */
  reported: () => boolean;
  stop: () => void;
  /** The frames shown and dropped since the last read. */
  read: () => VideoPlaybackRead;
}

/**
 * Follows a `<video>` sink until stopped. The frames it hands on are reported by
 * requestVideoFrameCallback, whose receive-to-present delay goes to the meter;
 * what it showed and dropped is read from getVideoPlaybackQuality, where a
 * frame replaced by a newer one before a refresh counts as dropped, since
 * Chromium counts such a frame in `presentedFrames` as well. Once that count
 * moves, the delay is sampled from one frame every `DELAY_SAMPLE_MS` rather
 * than every frame. Firefox keeps no playback quality for a live stream, so
 * there every frame is followed and the frames the callback reports stand in
 * for the frames shown. A new source restarts the element's counters, which
 * is read as a restart rather than a count going backwards.
 * @param video The video element to follow.
 * @param meter The meter to feed.
 * @param delayOf The receive-to-present delay, in ms, of the frame a callback reports; NaN where its arrival is unknown.
 * @returns The watch.
 */
export function watchVideo(video: HTMLVideoElement, meter: PresentMeter, delayOf: (metadata: VideoFrameCallbackMetadata) => number): VideoWatch {
  const quality = (): { totalVideoFrames: number; droppedVideoFrames: number } => (typeof video.getVideoPlaybackQuality === 'function'
    ? video.getVideoPlaybackQuality() : { totalVideoFrames: 0, droppedVideoFrames: 0 });
  const follows = typeof (video as unknown as { requestVideoFrameCallback?: unknown }).requestVideoFrameCallback === 'function';
  const start = quality();
  let total = start.totalVideoFrames, dropped = start.droppedVideoFrames;
  let handle = 0, timer = 0, last = -1, handed = 0, handedRead = 0, reported = false, counted = false;
  const follow = (): void => {
    timer = 0;
    handle = (video as HTMLVideoElement & { requestVideoFrameCallback: (cb: (now: number, metadata: VideoFrameCallbackMetadata) => void) => number }).requestVideoFrameCallback(onFrame);
  };
  const onFrame = (_now: number, metadata: VideoFrameCallbackMetadata): void => {
    handed += last >= 0 && metadata.presentedFrames > last ? metadata.presentedFrames - last : 1;
    last = metadata.presentedFrames;
    reported = true;
    meter.presented(0, delayOf(metadata));
    counted = counted || quality().totalVideoFrames !== start.totalVideoFrames;
    if (counted) timer = setTimeout(follow, DELAY_SAMPLE_MS);
    else follow();
  };
  if (follows) follow();
  return {
    reported: () => reported,
    stop: (): void => {
      clearTimeout(timer);
      if (follows) (video as unknown as { cancelVideoFrameCallback: (h: number) => void }).cancelVideoFrameCallback(handle);
    },
    read(): VideoPlaybackRead {
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
