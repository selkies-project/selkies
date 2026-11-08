/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The floating stats, in the kit's skin over the shared data
 * (`stats-data.ts`): the stream's figures as half-circle
 * gauges, what the stream runs on, the graphs that grow while it stays open,
 * the figures under them, and the host's meters. It is a compact strip or the
 * full view, dragged by its header: the strip reads at a glance, at a
 * toolbar's height, the stream's three figures against their own scales, then
 * after a divider every meter of the host's load as shares. The strip is one
 * row, and wraps to a second only where the window is too narrow to hold it,
 * as on a phone, where the divider goes. The full view
 * keeps the same gauges, every meter among them, above an accordion of the
 * pipeline rows, the histories, and the figures, one open at a time, with the
 * uplinks under it.
 *
 * Every stats component a layout draws is in this file, so a new layout
 * takes one of these and writes none:
 * - `FloatingStats`: the card floating over the stream, for a layout with
 *   no place of its own for it (the registry's `stats` entry in
 *   `layouts/index.ts`).
 * - `StreamStats`: the card itself, to place in a panel.
 * - `StreamStats` with `bar`: the strip a layout's bar carries in place of
 *   the card, the status marks and the figures a glance needs
 *   (`streamStrip`), as a row or a column.
 * - `StatsSection` and `StreamStrip`: the sidebar's section and the strip
 *   over its stream, in the sidebar's own skin (`sidebar.css`), for a
 *   layout that styles with those classes.
 * What the stats hold and read (the snapshot, the gauges, the graphs) is
 * `stats-data.ts`. The markup that lays the rows, graphs, and tiles out is
 * the same for every skin, so a skin is only the class names it passes
 * (`GraphSkin`, `RowsSkin`, `TilesSkin`) and its status mark.
 *
 * In the kit's skin every color is a token (`bg-muted`,
 * `text-muted-foreground`) or the theme's `--stat-*` variable, so the views
 * follow the theme without a stylesheet. The rows and tiles take no pointer
 * input, so a button belongs beside them, not inside.
 * @module
 */
import * as React from "react";
import { Check, ChevronDown, ChevronUp, CircleCheck, Copy, Dot, Mic, TriangleAlert, Video } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { FloatingPanel } from "@/panels/floating-panel";
import { SectionAccordion, SectionItem } from "@/panels/section-accordion";
import { meterLabel, snapshotGauges, snapshotMeters, snapshotRows, snapshotStrip, snapshotTiles, useCopyReport, useStatsFramerate, useStatsGraphs, useStreamSnapshot, type GaugeSpec, type GraphData, type Snapshot, type Status, type StatusMarkComponent } from "@/panels/stats-data";
import { graphPath } from "../../../selkies-web-core/lib/stream-stats-view.ts";
import { getPrefixedKey } from "@/utils";
import { t } from "@/i18n";

const GRAPH_WIDTH = 240;
const GRAPH_HEIGHT = 44;

/** The class names a layout draws a graph in. */
export interface GraphSkin {
  root: string;
  head: string;
  label: string;
  values: string;
  value?: string;
  number: string;
  /** The key beside a series' value where there are two; its color is the series'. */
  key: string;
  plot: string;
  /** A line across the middle of the plot, where the skin has one. */
  grid?: string;
  area?: string;
  line?: string;
  cross?: string;
  max: string;
}

interface GraphProps extends GraphData {
  skin: GraphSkin;
  /** The graph's name. */
  label: string;
  /** The unit of its axis. */
  unit: string;
}

/**
 * One graph: its name, the value under the pointer or else the newest, and up
 * to two series against one axis. The plot's strokes are presentation
 * attributes, which a skin's class names override.
 * @param props The graph's skin, name, unit, series, and axis.
 */
function Graph({ skin, label, unit, series, max }: GraphProps) {
  const [hover, setHover] = React.useState<number | null>(null);
  const paths = React.useMemo(() => series.map((s) => graphPath(s.values, GRAPH_WIDTH, GRAPH_HEIGHT, max)), [series, max]);
  const count = series[0].values.length;
  const at = hover !== null && hover < count ? hover : count - 1;
  const onMove = (e: React.PointerEvent<SVGSVGElement>) => {
    const box = e.currentTarget.getBoundingClientRect();
    const xs = paths[0].xs;
    if (!xs.length || box.width <= 0) return;
    const x = ((e.clientX - box.left) / box.width) * GRAPH_WIDTH;
    setHover(xs.reduce((best: number, px: number, i: number) => (Math.abs(px - x) < Math.abs(xs[best] - x) ? i : best), 0));
  };
  const stroke = (i: number) => `var(--stat-series-${i + 1})`;
  return (
    <div className={skin.root}>
      <div className={skin.head}>
        <span className={skin.label}>{label}</span>
        <span className={skin.values}>
          {series.map((s, i) => (
            <span key={s.name || i} className={skin.value}>
              {series.length > 1 && <i className={`${skin.key} series-${i + 1}`} style={{ background: stroke(i) }} />}
              <b className={skin.number}>{at >= 0 ? s.values[at] : 0}</b>
              {series.length > 1 ? ` ${s.name}` : ` ${unit}`}
            </span>
          ))}
        </span>
      </div>
      <svg className={skin.plot} viewBox={`0 0 ${GRAPH_WIDTH} ${GRAPH_HEIGHT}`} preserveAspectRatio="none"
        onPointerMove={onMove} onPointerLeave={() => setHover(null)} role="img"
        aria-label={`${label}: ${series.map((s) => `${at >= 0 ? s.values[at] : 0} ${s.name || unit}`).join(", ")}`}>
        {skin.grid && <line className={skin.grid} x1="0" x2={GRAPH_WIDTH} y1={GRAPH_HEIGHT / 2} y2={GRAPH_HEIGHT / 2} />}
        {paths.map((p, i) => (
          <g key={i}>
            {i === 0 && <path className={skin.area} d={p.area} fill={stroke(0)} opacity={0.12} />}
            <path className={skin.line && `${skin.line} series-${i + 1}`} d={p.line} fill="none" stroke={stroke(i)} strokeWidth={2}
              strokeLinejoin="round" strokeLinecap="round" vectorEffect="non-scaling-stroke" />
          </g>
        ))}
        {hover !== null && at >= 0 && (
          <line className={skin.cross} x1={paths[0].xs[at]} x2={paths[0].xs[at]} y1={0} y2={GRAPH_HEIGHT}
            stroke="var(--muted-foreground)" strokeWidth={1} vectorEffect="non-scaling-stroke" />
        )}
      </svg>
      <span className={skin.max}>{`${Math.round(max)} ${unit}`}</span>
    </div>
  );
}

/**
 * The three graphs, in order.
 * @param props The skin they are drawn in, and their series.
 */
function Graphs({ skin, graphs }: { skin: GraphSkin; graphs: Record<"fps" | "mbps" | "rtt", GraphData> }) {
  return (
    <>
      <Graph skin={skin} label={t("sections.stats.fpsLabel")} unit="fps" {...graphs.fps} />
      <Graph skin={skin} label={t("sections.stats.bandwidthLabel")} unit="Mbps" {...graphs.mbps} />
      <Graph skin={skin} label={t("sections.stats.latencyLabel")} unit="ms" {...graphs.rtt} />
    </>
  );
}

/** The class names a layout draws the rows in. */
export interface RowsSkin {
  root: string;
  /** A row; its status is added as a class of its own. */
  row: string;
  label: string;
  body: string;
  value: string;
  detail: string;
  reason: string;
}

/**
 * What the stream runs on, one row per stage: its mark, name, value, and what explains it.
 * @param props The skin, the rows, the status mark, and anything the skin adds after the rows.
 */
function StatRows({ skin, rows, Mark, children }: {
  skin: RowsSkin; rows: ReturnType<typeof snapshotRows>; Mark: StatusMarkComponent; children?: React.ReactNode;
}) {
  return (
    <div className={skin.root}>
      {rows.map((row) => (
        <div key={row.key} className={`${skin.row} ${row.status}`}>
          <Mark status={row.status} />
          <span className={skin.label}>{t(`sections.stats.${row.key}Label`)}</span>{" "}
          <span className={skin.body}>
            <span className={skin.value}>{row.value || t("sections.stats.tooltipMemoryNA")}</span>
            {row.detail && <span className={skin.detail}>{row.detail}</span>}
            {row.reason && <span className={skin.reason}>{row.reason}</span>}
          </span>
        </div>
      ))}
      {children}
    </div>
  );
}

/** The class names a layout draws the figure tiles in. */
export interface TilesSkin {
  root: string;
  tile: string;
  /** Added to a tile whose figure did not fall short. */
  calm?: string;
  /** Added to a tile whose figure fell short. */
  warn: string;
  value: string;
  detail: string;
  label: string;
}

/**
 * The figures as tiles: value, its spread where it has one, and its name.
 * @param props The skin, the tiles, and the status mark.
 */
function StatTiles({ skin, tiles, Mark }: { skin: TilesSkin; tiles: ReturnType<typeof snapshotTiles>; Mark: StatusMarkComponent }) {
  return (
    <div className={skin.root}>
      {tiles.map((tile) => (
        <div key={tile.key} className={`${skin.tile} ${tile.warn ? skin.warn : skin.calm ?? ""}`} title={tile.warn ? t("sections.stats.overshoot") : undefined}>
          <b className={skin.value}>{tile.warn && <Mark status="warn" />}{tile.value}</b>
          {tile.detail && <small className={skin.detail}>{tile.detail}</small>}
          <span className={skin.label}>{t(`sections.stats.tiles.${tile.key}`, tile.label)}</span>
        </div>
      ))}
    </div>
  );
}

/**
 * The mark beside a row or a warned figure: its state as a shape, so color
 * never carries it alone.
 * @param props The state it shows.
 */
function StatusMark({ status }: { status: Status }) {
  if (status === "good") return <CircleCheck className="h-3.5 w-3.5 shrink-0 text-[var(--stat-good)]" aria-hidden />;
  if (status === "warn") return <TriangleAlert className="h-3.5 w-3.5 shrink-0 text-[var(--stat-warn)]" aria-hidden />;
  return <Dot className="h-3.5 w-3.5 shrink-0 text-muted-foreground" aria-hidden />;
}

const CARD_GRAPHS: GraphSkin = {
  root: "relative",
  head: "mb-0.5 flex items-baseline justify-between gap-2",
  label: "text-[11px] uppercase tracking-wide text-muted-foreground",
  values: "flex gap-2.5 text-xs text-muted-foreground",
  number: "text-sm text-foreground",
  key: "mr-1 inline-block h-0.5 w-2.5 rounded-sm align-middle",
  plot: "block h-11 w-full rounded-md bg-muted/50 pointer-events-auto",
  max: "pointer-events-none absolute bottom-7 right-1 rounded-sm bg-muted px-1 text-[11px] text-muted-foreground",
};

const CARD_ROWS: RowsSkin = {
  root: "flex flex-col gap-1.5 pointer-events-none",
  row: "grid grid-cols-[14px_76px_1fr] items-start gap-1.5",
  label: "text-[11px] uppercase leading-4 tracking-wide text-muted-foreground",
  body: "flex min-w-0 flex-col [overflow-wrap:anywhere]",
  value: "font-semibold",
  detail: "text-muted-foreground",
  reason: "mt-0.5 border-l-2 border-[var(--stat-warn)] pl-1.5 text-muted-foreground",
};

const CARD_TILES: TilesSkin = {
  root: "grid grid-cols-3 gap-1.5 pointer-events-none",
  tile: "flex flex-col rounded-md border bg-muted/40 px-1.5 py-1",
  calm: "border-transparent",
  warn: "border-[var(--stat-warn)]",
  value: "flex items-center gap-1 text-[13px]",
  detail: "text-[10.5px] tabular-nums text-muted-foreground",
  label: "text-[11px] text-muted-foreground",
};

interface GaugeProps extends Omit<GaugeSpec, "key"> {
  /** Draws the gauge at the size of a toolbar's controls, for a strip. */
  compact?: boolean;
}

/**
 * One half-circle gauge: the arc fills to `percent` of the half circle, the
 * figure sits in the bowl, and the name sits under the base line. A gauge
 * reads a figure against its scale in one glance, where a graph has to be
 * read; the scales themselves belong to the caller (`snapshotGauges`).
 * @param props The gauge's name, figure, fill, and warn state.
 * @returns The gauge element.
 */
function Gauge({ label, value, percent, warn, title, compact }: GaugeProps) {
  const fill = Math.max(0, Math.min(100, percent));
  const angle = ((180 + fill * 1.8) * Math.PI) / 180;
  const endX = (20 + 16 * Math.cos(angle)).toFixed(2);
  const endY = (20 + 16 * Math.sin(angle)).toFixed(2);
  return (
    <div title={title} className="flex w-12 flex-col items-center">
      <div className="relative w-12">
        <svg viewBox="0 0 40 22" className={`block w-full ${compact ? "h-6" : "h-6.5"}`} role="img" aria-label={`${label}: ${value}`}>
          <path d="M 4 20 A 16 16 0 0 1 36 20" fill="none" stroke="var(--muted)" strokeWidth={4} strokeLinecap="round" />
          {fill > 0.5 && (
            <path d={`M 4 20 A 16 16 0 0 1 ${endX} ${endY}`} fill="none"
              stroke={warn ? "var(--stat-warn)" : "var(--primary)"} strokeWidth={4} strokeLinecap="round" />
          )}
        </svg>
        <b className="absolute inset-x-0 bottom-0 text-center text-xs leading-none text-foreground">{value}</b>
      </div>
      <span className="mt-1 whitespace-nowrap text-[10px] leading-none tracking-wide text-muted-foreground uppercase">{label}</span>
    </div>
  );
}

/**
 * The strip a bar shows: the sidebar's "Show over the stream" content
 * (`StreamStrip` below) in the kit's skin — the four status
 * marks, then the figures a glance needs (`streamStrip`), each its value
 * over its caption.
 * @param props The latest read.
 */
function BarStrip({ snapshot }: { snapshot: Snapshot }) {
  const figures = snapshotStrip(snapshot);
  return (
    <div className="flex items-center gap-2 pointer-events-none" role="status" aria-live="off">
      <span className="stream-strip-marks flex items-center gap-1">
        {snapshotRows(snapshot).map((row) => <StatusMark key={row.key} status={row.status} />)}
      </span>
      {figures.map((figure) => (
        <span key={figure.key} className="flex flex-col items-center whitespace-nowrap font-mono text-[9px] leading-tight tabular-nums tracking-tight text-muted-foreground">
          <b className={figure.warn ? "text-[var(--stat-warn)]" : "text-foreground"}>
            {figure.warn && <TriangleAlert className="mb-px inline h-2.5 w-2.5" aria-hidden />}{figure.value}
          </b>
          <span className="text-[7px] leading-none uppercase tracking-wide">{figure.caption}</span>
        </span>
      ))}
    </div>
  );
}

/**
 * The microphone and webcam uplinks, each a line with its mark; nothing while
 * neither is running.
 * @param props The latest read.
 */
function Uplinks({ latest }: { latest: Snapshot["latest"] }) {
  if (!latest || !(latest.mic || latest.webcam)) return null;
  return (
    <div className="flex flex-col gap-1 text-muted-foreground pointer-events-none">
      {latest.mic && <div className="flex items-center gap-1.5"><Mic className="h-3.5 w-3.5" aria-label="mic" />{latest.mic}</div>}
      {latest.webcam && <div className="flex items-center gap-1.5"><Video className="h-3.5 w-3.5" aria-label="webcam" />{latest.webcam}</div>}
    </div>
  );
}

/** A header button with its tooltip. */
function HeaderButton({ label, onClick, children }: { label: string; onClick: () => void; children: React.ReactNode }) {
  return (
    <Tooltip>
      <TooltipTrigger render={
        <Button variant="ghost" size="sm" className="h-7 w-7 p-0 min-w-0 pointer-events-auto" onClick={onClick} aria-label={label} />
      }>
        {children}
      </TooltipTrigger>
      <TooltipContent side="bottom"><p>{label}</p></TooltipContent>
    </Tooltip>
  );
}

/**
 * Renders the stats, compact or detailed by its own toggle, telling the core
 * the stats are on screen for as long as it is mounted in a visible tab.
 * @param props `bar` draws the stats for a layout's bar instead: the sidebar
 * strip's own content, the status marks and the figures a glance needs
 * (`streamStrip`), value over caption. It draws nothing until the core has
 * published its first reading.
 */
export function StreamStats({ bar }: { bar?: boolean }) {
  const [detailed, setDetailed] = React.useState(() => localStorage.getItem(getPrefixedKey("stats_detailed")) === "true");
  const framerate = useStatsFramerate();
  const snapshot = useStreamSnapshot(true);
  const graphs = useStatsGraphs(snapshot, framerate);
  const { copied, copy } = useCopyReport(snapshot);
  const { stream: streamGauges, meters: meterGauges } = snapshotGauges(snapshot, graphs, framerate);

  if (bar) {
    if (!snapshot) return null;
    return <BarStrip snapshot={snapshot} />;
  }

  const toggle = (
    <HeaderButton label={detailed ? t("stats.compactView") : t("stats.detailedView")} onClick={() => {
      localStorage.setItem(getPrefixedKey("stats_detailed"), String(!detailed));
      setDetailed(!detailed);
    }}>
      {detailed ? <ChevronUp className="h-3 w-3" /> : <ChevronDown className="h-3 w-3" />}
    </HeaderButton>
  );

  if (!detailed) {
    return (
      <div data-drag-handle className="flex max-w-[calc(100vw-1rem)] items-start gap-2 rounded-lg border bg-background px-2 py-2 text-xs shadow-lg tabular-nums cursor-grab active:cursor-grabbing select-none chrome-panel">
        <div className="flex min-w-0 flex-wrap items-end gap-x-1.5 gap-y-2 pointer-events-none">
          {streamGauges.map(({ key, ...gauge }) => <Gauge key={key} compact {...gauge} />)}
          {meterGauges.length > 0 && <span className="mb-1 h-8 self-end border-s max-sm:hidden" aria-hidden />}
          {meterGauges.map(({ key, ...gauge }) => <Gauge key={key} compact {...gauge} />)}
        </div>
        {toggle}
      </div>
    );
  }

  return (
    <div className="stream-stats chrome-panel flex min-h-0 w-[min(92vw,22rem)] flex-col rounded-lg border bg-background py-2 text-xs shadow-lg tabular-nums">
      {/* The scroll is an inner box with no background, square corners, and rows that skip rendering
          while scrolled out of view. Chromium composites an opaque scroller (on a high-density screen,
          any) as a second layer redrawn over the panel with every video frame, and a rounded one through
          an offscreen pass; Firefox renders the rows a scroller hides with every frame. The panel's
          padding keeps the box clear of its corners. */}
      <div className="flex min-h-0 flex-col gap-2.5 overflow-y-auto px-3 py-1 [&>*]:[content-visibility:auto] [&>*]:[contain-intrinsic-size:auto_2.75rem]">
        <div data-drag-handle className="flex items-center justify-between select-none cursor-grab active:cursor-grabbing">
          <h3 className="text-sm font-semibold text-card-foreground pointer-events-none">{t("sections.stats.title")}</h3>
          <div className="ms-auto flex items-center gap-1">
            <HeaderButton label={t("sections.stats.copyLabel")} onClick={copy}>
              {copied ? <Check className="h-3 w-3 text-[var(--stat-good)]" /> : <Copy className="h-3 w-3" />}
            </HeaderButton>
            {toggle}
          </div>
        </div>

        {/* One row of seven gauges where the panel is its full width; a narrower one wraps them evenly. */}
        <div className="grid grid-cols-[repeat(auto-fit,minmax(3rem,1fr))] justify-items-center gap-y-1.5 border-b border-border pb-2">
          {[...streamGauges, ...meterGauges].map(({ key, ...gauge }) => (
            <Gauge key={key} {...gauge} />
          ))}
        </div>

        <SectionAccordion defaultValue={["pipeline"]}>
          <SectionItem value="pipeline" title={t("stats.pipeline")}>
            <StatRows skin={CARD_ROWS} rows={snapshotRows(snapshot)} Mark={StatusMark} />
          </SectionItem>
          <SectionItem value="graphs" title={t("stats.graphs")}>
            <div className="flex flex-col gap-2.5"><Graphs skin={CARD_GRAPHS} graphs={graphs} /></div>
          </SectionItem>
          <SectionItem value="figures" title={t("stats.figures")}>
            <StatTiles skin={CARD_TILES} tiles={snapshotTiles(snapshot)} Mark={StatusMark} />
          </SectionItem>
        </SectionAccordion>

        <Uplinks latest={snapshot ? snapshot.latest : null} />
      </div>
    </div>
  );
}

/** The stats floating over the stream, where a layout has no place of its own for them. */
export function FloatingStats() {
  return (
    <FloatingPanel className="stats-overlay" initial={{ x: 0, y: 0.06 }} storageKey="stats_panel_position">
      <StreamStats />
    </FloatingPanel>
  );
}

const STATUS_ICONS = {
  good: <path d="M9 16.2 4.8 12l-1.4 1.4L9 19 21 7l-1.4-1.4z" />,
  warn: <path d="M1 21h22L12 2zm12-3h-2v-2h2zm0-4h-2v-4h2z" />,
  neutral: <circle cx="12" cy="12" r="4" />,
};

/**
 * The sidebar's mark beside a row: its state as a shape, so color never
 * carries it alone. Styled by `.stream-status-icon` in `sidebar.css`.
 * @param props The state it shows.
 */
function StatusIcon({ status }: { status: Status }) {
  return (
    <svg className={`stream-status-icon ${status}`} viewBox="0 0 24 24" width="14" height="14" aria-hidden="true">
      {STATUS_ICONS[status]}
    </svg>
  );
}

const SIDEBAR_GRAPHS: GraphSkin = {
  root: "stream-graph", head: "stream-graph-head", label: "stream-graph-label", values: "stream-graph-values",
  value: "stream-graph-value", number: "", key: "stream-key", plot: "stream-graph-plot", grid: "stream-graph-grid",
  area: "stream-graph-area", line: "stream-graph-line", cross: "stream-graph-cross", max: "stream-graph-max",
};

const SIDEBAR_ROWS: RowsSkin = {
  root: "stream-rows", row: "stream-row", label: "stream-row-label", body: "stream-row-text",
  value: "stream-row-value", detail: "stream-row-detail", reason: "stream-row-reason",
};

const SIDEBAR_TILES: TilesSkin = {
  root: "stream-tiles", tile: "stream-tile", warn: "warn", value: "", detail: "stream-tile-detail", label: "",
};

/**
 * The stats section of the sidebar's drawer, in the sidebar's own skin
 * (`sidebar.css`): what the stream runs on as rows with a status mark, the
 * graphs, the figures as tiles, the host's meters as bars, and the uplinks.
 * A layout that has no skin of its own takes `StreamStats` instead.
 * @param props `active` is whether the section is unfolded in an open drawer, which is what turns the numbers on.
 */
export function StatsSection({ active }: { active: boolean }) {
  const snapshot = useStreamSnapshot(active);
  const graphs = useStatsGraphs(snapshot, useStatsFramerate());
  const { copied, copy } = useCopyReport(snapshot);
  if (!snapshot) return null;
  const { latest } = snapshot;
  const meters = snapshotMeters(snapshot);

  return (
    <div className="stream-stats-section">
      <StatRows skin={SIDEBAR_ROWS} rows={snapshotRows(snapshot)} Mark={StatusIcon}>
        <button type="button" className={`stream-copy ${copied ? "done" : ""}`} onClick={copy} aria-label={t("sections.stats.copyLabel")}>
          <svg viewBox="0 0 24 24" width="16" height="16" fill="currentColor" aria-hidden="true">
            <path d="M16 1H4a2 2 0 0 0-2 2v14h2V3h12zm3 4H8a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h11a2 2 0 0 0 2-2V7a2 2 0 0 0-2-2m0 16H8V7h11z" />
          </svg>
        </button>
      </StatRows>
      <Graphs skin={SIDEBAR_GRAPHS} graphs={graphs} />
      <StatTiles skin={SIDEBAR_TILES} tiles={snapshotTiles(snapshot)} Mark={StatusIcon} />
      {meters.length > 0 && (
        <div className="stream-meters">
          {meters.map((meter) => (
            <div key={meter.key} className={`stream-meter${meter.bar ? "" : " amounts"}`} title={meter.detail || undefined}>
              <span className="stream-meter-label">{meterLabel(meter.key)}</span>
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

/**
 * The strip over the stream that the sidebar's "Show over the stream" switch
 * lays out, in the sidebar's skin: the status marks and the figures a glance
 * needs (`snapshotStrip`). It takes no pointer input and stays on with the
 * drawer shut. `StreamStats` with `bar` is the same content in the kit's skin.
 */
export function StreamStrip() {
  const snapshot = useStreamSnapshot(true);
  if (!snapshot) return null;
  const figures = snapshotStrip(snapshot);
  return (
    <div className="stream-strip" role="status" aria-live="off">
      <span className="stream-strip-marks">
        {snapshotRows(snapshot).map((row) => <StatusIcon key={row.key} status={row.status} />)}
      </span>
      {figures.map((figure) => (
        <span key={figure.key} className={`stream-strip-figure${figure.warn ? " warn" : ""}`}>
          <b>{figure.warn && <StatusIcon status="warn" />}{figure.value}</b>
          <small>{figure.caption}</small>
        </span>
      ))}
    </div>
  );
}
