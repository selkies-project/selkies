/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What every stats view reads: the core's stream state, the rows and tiles
 * worded from it, the graphs' series and scales, and the copied report. The
 * views themselves are `stats-view.tsx`; what a row says and when it warns is
 * `lib/stream-stats-view.js`.
 *
 * The state is the core's `window.stream_info`, `window.stream_client`, and
 * `window.stream_stats` (`selkies-web-core/lib/stream-stats.js`), read each
 * time the core announces a change (`STATS_EVENT`) while a view is on screen
 * in a visible tab. Being on screen is what turns the numbers on: each view
 * holds them through `holdStats`, so the core is told the stats shut only when
 * the last view goes.
 * @module
 */
import * as React from "react";
import { STATS_EVENT, type StreamClient, type StreamInfo, type StreamSample } from "../../../selkies-web-core/lib/stream-stats.js";
import { seriesOf, streamReport, streamRows, streamTiles } from "../../../selkies-web-core/lib/stream-stats-view.js";
import { holdStats } from "@/panels/stats-hold";
import { t } from "@/i18n";

/** The stream state the cores publish on `window`. */
declare global {
  interface Window {
    stream_info?: StreamInfo | null;
    stream_client?: StreamClient;
    stream_stats?: { open: boolean; latest: StreamSample | null; history: StreamSample[] };
    fps?: number;
    currentAudioBufferSize?: number;
    /**
     * Set by the dashboard around a transport switch so the active core
     * leaves the old peer's teardown to the reload the switch makes.
     */
    __selkiesModeSwitching?: boolean;
  }
}

/** What one read of the core's state holds. */
export interface Snapshot {
  /** The server's description of the stream, null until it arrives. */
  info: StreamInfo | null;
  /** This page's description of the stream. */
  client: StreamClient | null;
  /** The last second's figures. */
  latest: StreamSample | null;
  /** Every second since the stats opened, appended to in place. */
  history: StreamSample[];
  /** The history's length at the read, which is what changes between reads. */
  length: number;
}

/**
 * Reads the core's stream state on every announcement while `on` in a
 * visible tab, holding the numbers on meanwhile.
 * @param on Whether the view is on screen.
 * @returns The latest read, or null before the first.
 */
export function useStreamSnapshot(on: boolean): Snapshot | null {
  const [visible, setVisible] = React.useState(!document.hidden);
  const [snapshot, setSnapshot] = React.useState<Snapshot | null>(null);
  React.useEffect(() => {
    const onVisibility = () => setVisible(!document.hidden);
    document.addEventListener("visibilitychange", onVisibility);
    return () => document.removeEventListener("visibilitychange", onVisibility);
  }, []);
  React.useEffect(() => {
    if (!on || !visible) return undefined;
    holdStats(1);
    const read = () => {
      const stats = window.stream_stats || { open: false, latest: null, history: [] };
      setSnapshot({
        info: window.stream_info || null,
        client: window.stream_client ? { ...window.stream_client } : null,
        latest: stats.latest,
        history: stats.history,
        length: stats.history.length,
      });
    };
    read();
    window.addEventListener(STATS_EVENT, read);
    return () => {
      window.removeEventListener(STATS_EVENT, read);
      holdStats(-1);
    };
  }, [on, visible]);
  return snapshot;
}

/** The words the rows are worded with. */
function rowWords() {
  return {
    hardware: t("sections.stats.hardware"),
    software: t("sections.stats.software"),
    unknown: t("sections.stats.tooltipMemoryNA"),
    throttled: t("sections.stats.throttled"),
    hardware_available: t("sections.stats.hardwareAvailable"),
    software_preferred: t("sections.stats.softwarePreferred"),
  };
}

/** The rows of what the stream runs on (`streamRows`), worded. */
export function snapshotRows(snapshot: Snapshot | null) {
  return streamRows(snapshot?.info ?? null, snapshot?.client ?? null, snapshot?.latest ?? null, rowWords());
}

/** The figures as tiles (`streamTiles`). */
export function snapshotTiles(snapshot: Snapshot | null) {
  return streamTiles(snapshot?.latest ?? null, snapshot?.client ? snapshot.client.transport : "websockets", snapshot?.history ?? null);
}

/** The top of a graph's axis: the largest value drawn with a little headroom, never under `floor`. */
const axisTop = (floor: number, ...lists: number[][]): number => Math.max(floor, Math.ceil(Math.max(0, ...lists.flat()) * 1.1));

/** One graph's series against one axis, and the value at its top edge. */
export interface GraphData {
  series: Array<{ name: string; values: number[] }>;
  max: number;
}

/**
 * The three graphs' series: frame rate (the client's, and the server's where it
 * reports one) on an axis from the configured frame rate up, bitrate, and
 * round trip.
 * @param snapshot The latest read.
 * @param framerate The configured frame rate, the floor of the fps axis.
 */
export function useStatsGraphs(snapshot: Snapshot | null, framerate: number): Record<"fps" | "mbps" | "rtt", GraphData> {
  const history = snapshot ? snapshot.history : null;
  const length = snapshot ? snapshot.length : 0;
  return React.useMemo(() => {
    const samples = history || [];
    const fps = seriesOf(samples, "fps");
    const encoded = seriesOf(samples, "encoded_fps");
    const mbps = seriesOf(samples, "mbps");
    const rtt = seriesOf(samples, "rtt_ms");
    const hasEncoded = samples.some((s) => typeof s.encoded_fps === "number");
    return {
      fps: {
        series: hasEncoded
          ? [{ name: t("sections.stats.client"), values: fps }, { name: t("sections.stats.server"), values: encoded }]
          : [{ name: "", values: fps }],
        max: axisTop(framerate > 0 ? framerate : 60, fps, encoded),
      },
      mbps: { series: [{ name: "", values: mbps }], max: axisTop(1, mbps) },
      rtt: { series: [{ name: "", values: rtt }], max: axisTop(20, rtt) },
    };
    // The history array is appended to in place; its length is what changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [history, length, framerate]);
}

/**
 * Copies the stats as text (`streamReport`) and says so for a moment.
 * @param snapshot The latest read.
 * @returns Whether a copy just landed, and the copy action.
 */
export function useCopyReport(snapshot: Snapshot | null): { copied: boolean; copy: () => Promise<void> } {
  const [copied, setCopied] = React.useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(streamReport(snapshot?.info ?? null, snapshot?.client ?? null, snapshot?.latest ?? null));
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch (e) {
      console.warn("Could not copy the stats:", e);
    }
  };
  return { copied, copy };
}

/** A row's state; its mark carries it as a shape as well as a color. */
export type Status = "good" | "warn" | "neutral";

/** A layout's mark beside a row or a warned figure. */
export type StatusMarkComponent = React.ComponentType<{ status: Status }>;

