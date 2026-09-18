/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The sidebar's stats: what the stream runs on, the graphs that grow while the
 * section stays open, the figures under them and the host's meters.
 *
 * Everything drawn comes from the core's `window.stream_info`,
 * `window.stream_client` and `window.stream_stats`
 * (`selkies-web-core/lib/stream-stats.js`), read once a second and only while
 * the section is on screen; what a row says and when it warns is
 * `lib/stream-stats-view.js`, shared with the wish dashboard. Being on screen
 * is what turns the numbers on: the component posts `statsOpen` to the core,
 * which asks the server for them, and posts it again with `open: false` when
 * the section folds, the sidebar shuts or the tab hides.
 * @module
 */
import { useEffect, useMemo, useState } from "react";
import {
  streamRows,
  streamTiles,
  streamMeters,
  streamReport,
  seriesOf,
  graphPath,
} from "../../../selkies-web-core/lib/stream-stats-view.js";

const READ_INTERVAL_MS = 1000;
const GRAPH_WIDTH = 240;
const GRAPH_HEIGHT = 44;

const STATUS_ICONS = {
  good: <path d="M9 16.2 4.8 12l-1.4 1.4L9 19 21 7l-1.4-1.4z" />,
  warn: <path d="M1 21h22L12 2zm12-3h-2v-2h2zm0-4h-2v-4h2z" />,
  neutral: <circle cx="12" cy="12" r="4" />,
};

/** The mark beside a row: its state as a shape, so color never carries it alone. */
const StatusIcon = ({ status }) => (
  <svg className={`stream-status-icon ${status}`} viewBox="0 0 24 24" width="14" height="14" aria-hidden="true">
    {STATUS_ICONS[status]}
  </svg>
);

const CopyIcon = () => (
  <svg viewBox="0 0 24 24" width="16" height="16" fill="currentColor" aria-hidden="true">
    <path d="M16 1H4a2 2 0 0 0-2 2v14h2V3h12zm3 4H8a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h11a2 2 0 0 0 2-2V7a2 2 0 0 0-2-2m0 16H8V7h11z" />
  </svg>
);

const UPLINK_ICONS = {
  mic: <path d="M12 14a3 3 0 0 0 3-3V5a3 3 0 0 0-6 0v6a3 3 0 0 0 3 3m5-3a5 5 0 0 1-10 0H5a7 7 0 0 0 6 6.9V21h2v-3.1a7 7 0 0 0 6-6.9z" />,
  webcam: <path d="M17 10.5V7a1 1 0 0 0-1-1H4a1 1 0 0 0-1 1v10a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-3.5l4 4v-11z" />,
};

/** Names an uplink row by its device, as the sidebar's own toggles do. */
const UplinkIcon = ({ kind }) => (
  <svg viewBox="0 0 24 24" width="14" height="14" fill="currentColor" role="img" aria-label={kind}>
    {UPLINK_ICONS[kind]}
  </svg>
);

/**
 * One graph: its name, the value under the pointer or else the newest, and up
 * to two series against one axis whose top is `max`.
 * @param {{label: string, unit: string, series: Array<{name: string, values: number[]}>, max: number}} props
 */
function Graph({ label, unit, series, max }) {
  const [hover, setHover] = useState(null);
  const paths = useMemo(
    () => series.map((s) => graphPath(s.values, GRAPH_WIDTH, GRAPH_HEIGHT, max)),
    [series, max]
  );
  const count = series[0].values.length;
  const at = hover !== null && hover < count ? hover : count - 1;
  const onMove = (e) => {
    const box = e.currentTarget.getBoundingClientRect();
    const xs = paths[0].xs;
    if (!xs.length || box.width <= 0) return;
    const x = ((e.clientX - box.left) / box.width) * GRAPH_WIDTH;
    setHover(xs.reduce((best, px, i) => (Math.abs(px - x) < Math.abs(xs[best] - x) ? i : best), 0));
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
      <svg
        className="stream-graph-plot"
        viewBox={`0 0 ${GRAPH_WIDTH} ${GRAPH_HEIGHT}`}
        preserveAspectRatio="none"
        onPointerMove={onMove}
        onPointerLeave={() => setHover(null)}
        role="img"
        aria-label={`${label}: ${series.map((s) => `${at >= 0 ? s.values[at] : 0} ${s.name || unit}`).join(", ")}`}
      >
        <line className="stream-graph-grid" x1="0" x2={GRAPH_WIDTH} y1={GRAPH_HEIGHT / 2} y2={GRAPH_HEIGHT / 2} />
        {paths.map((p, i) => (
          <g key={i}>
            {i === 0 && <path className="stream-graph-area" d={p.area} />}
            <path className={`stream-graph-line series-${i + 1}`} d={p.line} />
          </g>
        ))}
        {hover !== null && at >= 0 && (
          <line className="stream-graph-cross" x1={paths[0].xs[at]} x2={paths[0].xs[at]} y1="0" y2={GRAPH_HEIGHT} />
        )}
      </svg>
      <span className="stream-graph-max">{`${Math.round(max)} ${unit}`}</span>
    </div>
  );
}

/** The top of a graph's axis: the largest value drawn with a little headroom, never under `floor`. */
const axisTop = (floor, ...lists) => Math.max(floor, Math.ceil(Math.max(0, ...lists.flat()) * 1.1));

/**
 * @param {{t: function(string, (Object|string)=): string, active: boolean, framerate: number}} props
 *     `active` is whether the section is unfolded in an open sidebar;
 *     `framerate` is the configured rate, the floor of the fps axis.
 */
export default function StreamStats({ t, active, framerate }) {
  const [visible, setVisible] = useState(!document.hidden);
  const [snapshot, setSnapshot] = useState(null);
  const [copied, setCopied] = useState(false);
  const shown = active && visible;

  useEffect(() => {
    const onVisibility = () => setVisible(!document.hidden);
    document.addEventListener("visibilitychange", onVisibility);
    return () => document.removeEventListener("visibilitychange", onVisibility);
  }, []);

  useEffect(() => {
    if (!shown) return undefined;
    window.postMessage({ type: "statsOpen", open: true }, window.location.origin);
    const read = () => {
      const stats = window.stream_stats || { latest: null, history: [] };
      setSnapshot({
        info: window.stream_info || null,
        client: window.stream_client ? { ...window.stream_client } : null,
        latest: stats.latest,
        history: stats.history,
        length: stats.history.length,
      });
    };
    read();
    const id = setInterval(read, READ_INTERVAL_MS);
    return () => {
      clearInterval(id);
      window.postMessage({ type: "statsOpen", open: false }, window.location.origin);
    };
  }, [shown]);

  const words = useMemo(() => ({
    hardware: t("sections.stats.hardware"),
    software: t("sections.stats.software"),
    unknown: t("sections.stats.tooltipMemoryNA"),
    throttled: t("sections.stats.throttled"),
  }), [t]);

  const graphs = useMemo(() => {
    const history = snapshot ? snapshot.history : [];
    const fps = seriesOf(history, "fps");
    const encoded = seriesOf(history, "encoded_fps");
    const mbps = seriesOf(history, "mbps");
    const rtt = seriesOf(history, "rtt_ms");
    const hasEncoded = history.some((s) => typeof s.encoded_fps === "number");
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
  }, [snapshot && snapshot.length, snapshot && snapshot.history, framerate, t]);

  if (!snapshot) return null;
  const { info, client, latest } = snapshot;
  const rows = streamRows(info, client, latest, words);
  const tiles = streamTiles(latest, client ? client.transport : "websockets");
  const meters = streamMeters(latest);
  const meterLabels = {
    cpu: t("sections.stats.cpuLabel"),
    mem: t("sections.stats.sysMemLabel"),
    gpu: t("sections.stats.gpuLabel"),
    gpumem: t("sections.stats.gpuMemLabel"),
  };
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
    <div className="stream-stats">
      <div className="stream-rows">
        {rows.map((row) => (
          <div key={row.key} className={`stream-row ${row.status}`}>
            <StatusIcon status={row.status} />
            <span className="stream-row-text">
              <span>
                <span className="stream-row-label">{t(`sections.stats.${row.key}Label`)}</span>
                <span className="stream-row-value">{row.value || t("sections.stats.tooltipMemoryNA")}</span>
              </span>
              {row.detail && <span className="stream-row-detail">{row.detail}</span>}
              {row.reason && <span className="stream-row-reason">{row.reason}</span>}
            </span>
          </div>
        ))}
        <button
          type="button"
          className={`stream-copy ${copied ? "done" : ""}`}
          onClick={copy}
          aria-label={t("sections.stats.copyLabel")}
        >
          <CopyIcon />
        </button>
      </div>

      <Graph label={t("sections.stats.fpsLabel")} unit="fps" {...graphs.fps} />
      <Graph label={t("sections.stats.bandwidthLabel")} unit="Mbps" {...graphs.mbps} />
      <Graph label={t("sections.stats.latencyLabel")} unit="ms" {...graphs.rtt} />

      {(
        <div className="stream-tiles">
          {tiles.map((tile) => (
            <div key={tile.key} className="stream-tile">
              <b>{tile.value}</b>
              <span>{tile.label}</span>
            </div>
          ))}
        </div>
      )}

      {meters.length > 0 && (
        <div className="stream-meters">
          {meters.map((meter) => (
            <div key={meter.key} className={`stream-meter${meter.bar ? "" : " amounts"}`}
              title={meter.detail || undefined}>
              <span className="stream-meter-label">{meterLabels[meter.key]}</span>
              {meter.bar && (
                <span className="stream-meter-track">
                  <span className="stream-meter-fill" style={{ width: `${meter.percent}%` }} />
                </span>
              )}
              <span className="stream-meter-text">{meter.text}</span>
            </div>
          ))}
        </div>
      )}

      {latest && (latest.mic || latest.webcam) && (
        <div className="stream-uplink">
          {latest.mic && <div><UplinkIcon kind="mic" />{latest.mic}</div>}
          {latest.webcam && <div><UplinkIcon kind="webcam" />{latest.webcam}</div>}
        </div>
      )}
    </div>
  );
}
