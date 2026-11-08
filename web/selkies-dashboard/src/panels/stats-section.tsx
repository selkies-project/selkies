/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The sidebar's stats, in its own skin (`sidebar.css`) over the shared view
 * (`stats-data.ts`, `stats-view.tsx`): what the stream runs on as rows with a status mark, the
 * graphs that grow while the section stays open, the figures as tiles, the
 * host's meters as bars, and the uplinks; and the strip over the stream the
 * section's switch lays out, which shows the status marks and the figures a
 * glance needs (`streamStrip` in `lib/stream-stats-view.ts`), takes no
 * pointer input, and stays on with the drawer shut.
 * @module
 */
import { streamMeters, streamStrip } from "../../../selkies-web-core/lib/stream-stats-view.ts";
import { snapshotRows, snapshotTiles, useCopyReport, useStatsGraphs, useStreamSnapshot, type Status } from "@/panels/stats-data";
import { Graphs, StatRows, StatTiles, type GraphSkin, type RowsSkin, type TilesSkin } from "@/panels/stats-view";
import { t } from "@/i18n";

const STATUS_ICONS = {
  good: <path d="M9 16.2 4.8 12l-1.4 1.4L9 19 21 7l-1.4-1.4z" />,
  warn: <path d="M1 21h22L12 2zm12-3h-2v-2h2zm0-4h-2v-4h2z" />,
  neutral: <circle cx="12" cy="12" r="4" />,
};

/** The mark beside a row: its state as a shape, so color never carries it alone. */
function StatusIcon({ status }: { status: Status }) {
  return (
    <svg className={`stream-status-icon ${status}`} viewBox="0 0 24 24" width="14" height="14" aria-hidden="true">
      {STATUS_ICONS[status]}
    </svg>
  );
}

const GRAPH_SKIN: GraphSkin = {
  root: "stream-graph", head: "stream-graph-head", label: "stream-graph-label", values: "stream-graph-values",
  value: "stream-graph-value", number: "", key: "stream-key", plot: "stream-graph-plot", grid: "stream-graph-grid",
  area: "stream-graph-area", line: "stream-graph-line", cross: "stream-graph-cross", max: "stream-graph-max",
};

const ROWS_SKIN: RowsSkin = {
  root: "stream-rows", row: "stream-row", label: "stream-row-label", body: "stream-row-text",
  value: "stream-row-value", detail: "stream-row-detail", reason: "stream-row-reason",
};

const TILES_SKIN: TilesSkin = {
  root: "stream-tiles", tile: "stream-tile", warn: "warn", value: "", detail: "stream-tile-detail", label: "",
};

const METER_LABELS: Record<string, string> = { cpu: "CPU", mem: "MEM", gpu: "GPU", gpumem: "VRAM" };

interface StatsSectionProps {
  /** Whether the section is unfolded in an open drawer, which is what turns the numbers on. */
  active: boolean;
  /** The configured frame rate, the floor of the fps axis. */
  framerate: number;
}

/** The section's body: rows, graphs, tiles, meters, and uplinks. */
export function StatsSection({ active, framerate }: StatsSectionProps) {
  const snapshot = useStreamSnapshot(active);
  const graphs = useStatsGraphs(snapshot, framerate);
  const { copied, copy } = useCopyReport(snapshot);
  if (!snapshot) return null;
  const { latest } = snapshot;
  const meters = streamMeters(latest);

  return (
    <div className="stream-stats-section">
      <StatRows skin={ROWS_SKIN} rows={snapshotRows(snapshot)} Mark={StatusIcon}>
        <button type="button" className={`stream-copy ${copied ? "done" : ""}`} onClick={copy} aria-label={t("sections.stats.copyLabel")}>
          <svg viewBox="0 0 24 24" width="16" height="16" fill="currentColor" aria-hidden="true">
            <path d="M16 1H4a2 2 0 0 0-2 2v14h2V3h12zm3 4H8a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h11a2 2 0 0 0 2-2V7a2 2 0 0 0-2-2m0 16H8V7h11z" />
          </svg>
        </button>
      </StatRows>
      <Graphs skin={GRAPH_SKIN} graphs={graphs} />
      <StatTiles skin={TILES_SKIN} tiles={snapshotTiles(snapshot)} Mark={StatusIcon} />
      {meters.length > 0 && (
        <div className="stream-meters">
          {meters.map((meter) => (
            <div key={meter.key} className={`stream-meter${meter.bar ? "" : " amounts"}`} title={meter.detail || undefined}>
              <span className="stream-meter-label">{METER_LABELS[meter.key] ?? meter.key}</span>
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
  const figures = streamStrip(snapshot.latest, snapshot.client ? snapshot.client.transport : "websockets");
  return (
    <div className="stream-strip" role="status" aria-live="off">
      <span className="stream-strip-marks">
        {snapshotRows(snapshot).map((row) => <StatusIcon key={row.key} status={row.status} />)}
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
