/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What every stats view reads: the core's stream state, the rows and tiles
 * worded from it, the graphs' series and scales, the gauges' readings, and the
 * copied report. The views that draw them are in `stats.tsx`; what a row says
 * and when it warns is `lib/stream-stats-view.ts`.
 *
 * The state is the core's `window.stream_info`, `window.stream_client`, and
 * `window.stream_stats` (`selkies-web-core/lib/stream-stats.ts`), read each
 * time the core announces a change (`STATS_EVENT`) while a view is on screen
 * in a visible tab. Being on screen is what turns the numbers on: each view
 * holds them through `holdStats`, so the core is told the stats shut only when
 * the last view goes.
 * @module
 */
import * as React from "react";
import { STATS_EVENT, type StreamClient, type StreamInfo, type StreamSample } from "../../../selkies-web-core/lib/stream-stats.ts";
import { seriesOf, streamMeters, streamReport, streamRows, streamStrip, streamTiles } from "../../../selkies-web-core/lib/stream-stats-view.ts";
import { useSettings } from "@/state/settings-context";
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

/** How many views are showing the stream's numbers: a section, the strip, a bar, or the floating panel. */
let holders = 0;

/**
 * Counts one view in or out. The core is told the stats are open when the
 * first view comes and shut only when the last one goes.
 * @param delta 1 as a view mounts, -1 as it goes.
 */
function holdStats(delta: 1 | -1): void {
  const before = holders;
  holders += delta;
  if ((before === 0) !== (holders === 0)) {
    window.postMessage({ type: "statsOpen", open: holders > 0 }, window.location.origin);
  }
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

/** The host's meters (`streamMeters`): the load of the CPU, memory, GPU, and its memory, as shares where the host reports them. */
export function snapshotMeters(snapshot: Snapshot | null) {
  return streamMeters(snapshot?.latest ?? null);
}

/** The label each strip figure is captioned with, the tile's or graph's that shows it in the views. */
const STRIP_CAPTIONS: Record<string, string> = { fps: "sections.stats.fpsLabel", rtt_ms: "sections.stats.latencyLabel" };

/**
 * The figures a glance needs (`streamStrip`), each with the caption the views
 * name it by, for a strip over the stream or on a bar.
 * @param snapshot The latest read.
 * @returns The figures, each carrying its translated `caption`.
 */
export function snapshotStrip(snapshot: Snapshot | null) {
  return streamStrip(snapshot?.latest ?? null, snapshot?.client ? snapshot.client.transport : "websockets")
    .map((figure) => ({ ...figure, caption: t(STRIP_CAPTIONS[figure.key] || `sections.stats.tiles.${figure.key}`) }));
}

/** One half-circle gauge's reading: its identity, the figure in its bowl, and how full its arc is. */
export interface GaugeSpec {
  /** Tells the gauges apart in their row. */
  key: string;
  /** The gauge's short name, drawn under the base line. */
  label: string;
  /** The figure drawn in the bowl. */
  value: string;
  /** The share of the half circle the arc fills, 0-100. */
  percent: number;
  /** Draws the arc in the warn color; set by the figure that fell short. */
  warn?: boolean;
  /** A hover note carrying what the arc alone omits, the memory amounts. */
  title?: string;
}

/**
 * The short name a host meter is drawn under, in a gauge or on a bar.
 * @param key The meter's key from `streamMeters`: `cpu`, `mem`, `gpu`, or `gpumem`.
 * @returns `CPU`, `MEM`, `GPU`, or `VRAM`.
 */
export function meterLabel(key: string): string {
  return key === "gpumem" ? "VRAM" : key.toUpperCase();
}

/**
 * The gauges of a snapshot: the stream's three, then the host's meters. The
 * fps arc fills to the configured frame rate and warns under nine tenths of
 * it, the one figure with a configured value to fall short of; the latency
 * arc fills as the reading leaves its axis, so a full arc is the best
 * reading; the bitrate arc is the reading against the graph's own top, a
 * scale with no good or bad end, and reads the graph's newest point so the
 * two always agree (the graph is bucketed by maximum once the history is
 * long). The meters fill as shares of their wholes.
 * @param snapshot The latest read.
 * @param graphs The graphs' series and scales (`useStatsGraphs`).
 * @param framerate The configured frame rate, the fps arc's whole.
 * @returns The stream's gauges and the host's.
 */
export function snapshotGauges(snapshot: Snapshot | null, graphs: Record<"fps" | "mbps" | "rtt", GraphData>, framerate: number) {
  const latest = snapshot ? snapshot.latest : null;
  const number = (key: string): number => (latest && typeof latest[key] === "number" ? (latest[key] as number) : 0);
  const mbpsValues = graphs.mbps.series[0].values;
  const mbpsNow = mbpsValues.length ? mbpsValues[mbpsValues.length - 1] : 0;
  const stream: GaugeSpec[] = [
    { key: "fps", label: "FPS", value: `${Math.round(number("fps"))}`, percent: (number("fps") / framerate) * 100,
      warn: number("fps") > 0 && number("fps") < framerate * 0.9 },
    { key: "mbps", label: "Mbps", value: mbpsNow.toFixed(1), percent: (mbpsNow / graphs.mbps.max) * 100 },
    { key: "rtt_ms", label: "RTT", value: `${Math.round(number("rtt_ms"))}`, percent: 100 - (number("rtt_ms") / graphs.rtt.max) * 100 },
  ];
  const meters: GaugeSpec[] = snapshotMeters(snapshot).map((meter) => ({
    key: meter.key,
    label: meterLabel(meter.key),
    value: `${Math.round(meter.percent)}`,
    percent: meter.percent,
    title: meter.text,
  }));
  return { stream, meters };
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

/**
 * The frame rate the stats read their fps against: the configured one, else
 * 60 where the setting names none.
 * @returns The frame rate in frames per second.
 */
export function useStatsFramerate(): number {
  const { framerate } = useSettings();
  return framerate > 0 ? framerate : 60;
}

/** A row's state; its mark carries it as a shape as well as a color. */
export type Status = "good" | "warn" | "neutral";

/** A layout's mark beside a row or a warned figure. */
export type StatusMarkComponent = React.ComponentType<{ status: Status }>;

