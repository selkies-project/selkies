/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The sidebar's stats: what the stream runs on as rows with a status mark,
 * the graphs that grow while the section stays open, the figures as tiles,
 * the host's meters as bars, and the uplinks; and the strip over the stream
 * the section's switch lays out, which shows the status marks and the
 * figures a glance needs (`streamStrip` in `lib/stream-stats-view.js`),
 * takes no pointer input, and stays on with the drawer shut.
 *
 * Both read the core's `window.stream_info`, `window.stream_client`, and
 * `window.stream_stats` each time the core announces a change (`STATS_EVENT`)
 * while on screen in a visible tab, and hold the numbers on through
 * `holdStats`, so the core is told the stats shut only when the last view goes.
 * @module
 */
import { useEffect, useMemo, useState } from "react";
import { STATS_EVENT, type StreamClient, type StreamInfo, type StreamSample } from "../../../selkies-web-core/lib/stream-stats.js";
import { graphPath, seriesOf, streamMeters, streamReport, streamRows, streamStrip, streamTiles } from "../../../selkies-web-core/lib/stream-stats-view.js";
import { holdStats } from "@/panels/stats-hold";
import { t } from "@/i18n";

const GRAPH_WIDTH = 240;
const GRAPH_HEIGHT = 44;

const STATUS_ICONS = {
  good: <path d="M9 16.2 4.8 12l-1.4 1.4L9 19 21 7l-1.4-1.4z" />,
  warn: <path d="M1 21h22L12 2zm12-3h-2v-2h2zm0-4h-2v-4h2z" />,
  neutral: <circle cx="12" cy="12" r="4" />,
};

/** The mark beside a row: its state as a shape, so color never carries it alone. */
function StatusIcon({ status }: { status: "good" | "warn" | "neutral" }) {
  return (
    <svg className={`stream-status-icon ${status}`} viewBox="0 0 24 24" width="14" height="14" aria-hidden="true">
      {STATUS_ICONS[status]}
    </svg>
  );
}

/** What one read of the core's state holds. */
interface Snapshot {
  info: StreamInfo | null;
  client: StreamClient | null;
  latest: StreamSample | null;
  history: StreamSample[];
  /** The history's length at the read, which is what changes between reads. */
  length: number;
}

/** Reads the core's stream state on every announcement while `on`, holding the numbers on meanwhile. */
function useStreamSnapshot(on: boolean): Snapshot | null {
  const [visible, setVisible] = useState(!document.hidden);
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  useEffect(() => {
    const onVisibility = () => setVisible(!document.hidden);
    document.addEventListener("visibilitychange", onVisibility);
    return () => document.removeEventListener("visibilitychange", onVisibility);
  }, []);
  useEffect(() => {
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

interface GraphProps {
  label: string;
  unit: string;
  series: Array<{ name: string; values: number[] }>;
  max: number;
}

/** One graph: its name, the value under the pointer or else the newest, and up to two series against one axis. */
function Graph({ label, unit, series, max }: GraphProps) {
  const [hover, setHover] = useState<number | null>(null);
  const paths = useMemo(() => series.map((s) => graphPath(s.values, GRAPH_WIDTH, GRAPH_HEIGHT, max)), [series, max]);
  const count = series[0].values.length;
  const at = hover !== null && hover < count ? hover : count - 1;
  const onMove = (e: React.PointerEvent<SVGSVGElement>) => {
    const box = e.currentTarget.getBoundingClientRect();
    const xs = paths[0].xs;
    if (!xs.length || box.width <= 0) return;
    const x = ((e.clientX - box.left) / box.width) * GRAPH_WIDTH;
    setHover(xs.reduce((best: number, px: number, i: number) => (Math.abs(px - x) < Math.abs(xs[best] - x) ? i : best), 0));
  };
  return (
    <div className="stream-graph">
      <div className="stream-graph-head">
        <span className="stream-graph-label">{label}</span>
        <span className="stream-graph-values">
          {series.map((s, i) => (
            <span key={s.name || i} className="stream-graph-value">
              {series.length > 1 && <i className={`stream-key series-${i + 1}`} />}
              <b>{at >= 0 ? s.values[at] : 0}</b>
              {series.length > 1 ? ` ${s.name}` : ` ${unit}`}
            </span>
          ))}
        </span>
      </div>
      <svg className="stream-graph-plot" viewBox={`0 0 ${GRAPH_WIDTH} ${GRAPH_HEIGHT}`} preserveAspectRatio="none"
        onPointerMove={onMove} onPointerLeave={() => setHover(null)} role="img"
        aria-label={`${label}: ${series.map((s) => `${at >= 0 ? s.values[at] : 0} ${s.name || unit}`).join(", ")}`}>
        <line className="stream-graph-grid" x1="0" x2={GRAPH_WIDTH} y1={GRAPH_HEIGHT / 2} y2={GRAPH_HEIGHT / 2} />
        {paths.map((p, i) => (
          <g key={i}>
            {i === 0 && <path className="stream-graph-area" d={p.area} />}
            <path className={`stream-graph-line series-${i + 1}`} d={p.line} />
          </g>
        ))}
        {hover !== null && at >= 0 && <line className="stream-graph-cross" x1={paths[0].xs[at]} x2={paths[0].xs[at]} y1="0" y2={GRAPH_HEIGHT} />}
      </svg>
      <span className="stream-graph-max">{`${Math.round(max)} ${unit}`}</span>
    </div>
  );
}

/** The top of a graph's axis: the largest value drawn with a little headroom, never under `floor`. */
const axisTop = (floor: number, ...lists: number[][]) => Math.max(floor, Math.ceil(Math.max(0, ...lists.flat()) * 1.1));

interface StatsSectionProps {
  /** Whether the section is unfolded in an open drawer, which is what turns the numbers on. */
  active: boolean;
  /** The configured frame rate, the floor of the fps axis. */
  framerate: number;
}

/** The section's body: rows, graphs, tiles, meters, and uplinks. */
export function StatsSection({ active, framerate }: StatsSectionProps) {
  const snapshot = useStreamSnapshot(active);
  const [copied, setCopied] = useState(false);
  const history = snapshot ? snapshot.history : null;
  const length = snapshot ? snapshot.length : 0;
  const graphs = useMemo(() => {
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
        max: axisTop(framerate || 60, fps, encoded),
      },
      mbps: { series: [{ name: "", values: mbps }], max: axisTop(1, mbps) },
      rtt: { series: [{ name: "", values: rtt }], max: axisTop(20, rtt) },
    };
    // The history array is appended to in place; its length is what changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [history, length, framerate]);

  if (!snapshot) return null;
  const { info, client, latest } = snapshot;
  const rows = streamRows(info, client, latest, rowWords());
  const tiles = streamTiles(latest, client ? client.transport : "websockets", snapshot.history);
  const meters = streamMeters(latest);
  const meterLabels: Record<string, string> = { cpu: "CPU", mem: "MEM", gpu: "GPU", gpumem: "VRAM" };
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(streamReport(info, client, latest));
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch (e) {
      console.warn("Could not copy the stats:", e);
    }
  };

  return (
    <div className="stream-stats-section">
      <div className="stream-rows">
        {rows.map((row) => (
          <div key={row.key} className={`stream-row ${row.status}`}>
            <StatusIcon status={row.status} />
            <span className="stream-row-text">
              <span>
                <span className="stream-row-label">{t(`sections.stats.${row.key}Label`)}</span>{" "}
                <span className="stream-row-value">{row.value || t("sections.stats.tooltipMemoryNA")}</span>
              </span>
              {row.detail && <span className="stream-row-detail">{row.detail}</span>}
              {row.reason && <span className="stream-row-reason">{row.reason}</span>}
            </span>
          </div>
        ))}
        <button type="button" className={`stream-copy ${copied ? "done" : ""}`} onClick={copy} aria-label={t("sections.stats.copyLabel")}>
          <svg viewBox="0 0 24 24" width="16" height="16" fill="currentColor" aria-hidden="true">
            <path d="M16 1H4a2 2 0 0 0-2 2v14h2V3h12zm3 4H8a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h11a2 2 0 0 0 2-2V7a2 2 0 0 0-2-2m0 16H8V7h11z" />
          </svg>
        </button>
      </div>
      <Graph label={t("sections.stats.fpsLabel")} unit="fps" {...graphs.fps} />
      <Graph label={t("sections.stats.bandwidthLabel")} unit="Mbps" {...graphs.mbps} />
      <Graph label={t("sections.stats.latencyLabel")} unit="ms" {...graphs.rtt} />
      <div className="stream-tiles">
        {tiles.map((tile) => (
          <div key={tile.key} className={`stream-tile${tile.warn ? " warn" : ""}`} title={tile.warn ? t("sections.stats.overshoot") : undefined}>
            <b>{tile.warn && <StatusIcon status="warn" />}{tile.value}</b>
            {tile.detail && <small className="stream-tile-detail">{tile.detail}</small>}
            <span>{t(`sections.stats.tiles.${tile.key}`, tile.label)}</span>
          </div>
        ))}
      </div>
      {meters.length > 0 && (
        <div className="stream-meters">
          {meters.map((meter) => (
            <div key={meter.key} className={`stream-meter${meter.bar ? "" : " amounts"}`} title={meter.detail || undefined}>
              <span className="stream-meter-label">{meterLabels[meter.key] ?? meter.key}</span>
              {meter.bar && (
                <span className="stream-meter-track">
                  <span className="stream-meter-fill" style={{ transform: `translateX(${meter.percent - 100}%)` }} />
                </span>
              )}
              <span className="stream-meter-text">{meter.text}</span>
            </div>
          ))}
        </div>
      )}
      {latest && (latest.mic || latest.webcam) && (
        <div className="stream-uplink">
          {latest.mic && <div>{t("topMenu.microphone")}: {latest.mic}</div>}
          {latest.webcam && <div>{t("topMenu.webcam")}: {latest.webcam}</div>}
        </div>
      )}
    </div>
  );
}

/** The label each strip figure is captioned with, the tile's or graph's that shows it in the section. */
const CAPTIONS: Record<string, string> = { fps: "sections.stats.fpsLabel", rtt_ms: "sections.stats.latencyLabel" };

/** The strip over the stream. */
export function StreamStrip() {
  const snapshot = useStreamSnapshot(true);
  if (!snapshot) return null;
  const { info, client, latest } = snapshot;
  const rows = streamRows(info, client, latest, rowWords());
  const figures = streamStrip(latest, client ? client.transport : "websockets");
  return (
    <div className="stream-strip" role="status" aria-live="off">
      <span className="stream-strip-marks">
        {rows.map((row) => <StatusIcon key={row.key} status={row.status} />)}
      </span>
      {figures.map((figure) => (
        <span key={figure.key} className={`stream-strip-figure${figure.warn ? " warn" : ""}`}>
          <b>{figure.warn && <StatusIcon status="warn" />}{figure.value}</b>
          <small>{t(CAPTIONS[figure.key] || `sections.stats.tiles.${figure.key}`)}</small>
        </span>
      ))}
    </div>
  );
}
