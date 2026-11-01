/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The floating stats, in the kit's skin over the shared view
 * (`stats-data.ts`, `stats-view.tsx`): the stream's figures as half-circle gauges, what the
 * stream runs on, the graphs that grow while it stays open, the figures under
 * them, and the host's meters. It is a compact strip or the full view, dragged
 * by its header: the strip reads at a glance, at a toolbar's height, the
 * stream's three figures against their own scales, then after a divider every
 * meter of the host's load as shares. The full view keeps the same gauges,
 * every meter among them, above an accordion of the pipeline rows, the
 * histories, and the figures, one open at a time, with the uplinks under it.
 * @module
 */
import { useState } from "react";
import { Check, ChevronDown, ChevronUp, CircleCheck, Copy, Dot, Mic, TriangleAlert, Video } from "lucide-react";
import { streamMeters } from "../../../selkies-web-core/lib/stream-stats-view.js";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { FloatingPanel } from "@/panels/floating-panel";
import { SectionAccordion, SectionItem } from "@/panels/section-accordion";
import { snapshotRows, snapshotTiles, useCopyReport, useStatsGraphs, useStreamSnapshot, type Status } from "@/panels/stats-data";
import { Graphs, StatRows, StatTiles, type GraphSkin, type RowsSkin, type TilesSkin } from "@/panels/stats-view";
import { useSettings } from "@/state/settings-context";
import { getPrefixedKey } from "@/utils";
import { t } from "@/i18n";

/** The mark beside a row. */
function StatusMark({ status }: { status: Status }) {
  if (status === "good") return <CircleCheck className="h-3.5 w-3.5 shrink-0 text-[var(--stat-good)]" aria-hidden />;
  if (status === "warn") return <TriangleAlert className="h-3.5 w-3.5 shrink-0 text-[var(--stat-warn)]" aria-hidden />;
  return <Dot className="h-3.5 w-3.5 shrink-0 text-muted-foreground" aria-hidden />;
}

const GRAPH_SKIN: GraphSkin = {
  root: "relative",
  head: "mb-0.5 flex items-baseline justify-between gap-2",
  label: "text-[11px] uppercase tracking-wide text-muted-foreground",
  values: "flex gap-2.5 text-xs text-muted-foreground",
  number: "text-sm text-foreground",
  key: "mr-1 inline-block h-0.5 w-2.5 rounded-sm align-middle",
  plot: "block h-11 w-full rounded-md bg-muted/50 pointer-events-auto",
  max: "pointer-events-none absolute bottom-7 right-1 rounded-sm bg-muted px-1 text-[11px] text-muted-foreground",
};

const ROWS_SKIN: RowsSkin = {
  root: "flex flex-col gap-1.5 pointer-events-none",
  row: "grid grid-cols-[14px_76px_1fr] items-start gap-1.5",
  label: "text-[11px] uppercase leading-4 tracking-wide text-muted-foreground",
  body: "flex min-w-0 flex-col [overflow-wrap:anywhere]",
  value: "font-semibold",
  detail: "text-muted-foreground",
  reason: "mt-0.5 border-l-2 border-[var(--stat-warn)] pl-1.5 text-muted-foreground",
};

const TILES_SKIN: TilesSkin = {
  root: "grid grid-cols-3 gap-1.5 pointer-events-none",
  tile: "flex flex-col rounded-md border bg-muted/40 px-1.5 py-1",
  calm: "border-transparent",
  warn: "border-[var(--stat-warn)]",
  value: "flex items-center gap-1 text-[13px]",
  detail: "text-[9px] tabular-nums text-muted-foreground",
  label: "text-[10.5px] text-muted-foreground",
};

interface GaugeProps {
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
  /** Draws the gauge at the size of the toolbar's controls, for the strip. */
  compact?: boolean;
}

/** One gauge of a row: its identity beside what it draws. */
interface GaugeSpec extends GaugeProps {
  /** Tells the gauges apart in their row. */
  key: string;
}

/**
 * One half-circle gauge: the arc fills to `percent` of the half circle, the
 * figure sits in the bowl, and the name sits under the base line. A gauge
 * reads a figure against its scale in one glance, where a graph has to be
 * read; the scales themselves belong to the caller.
 * @param props The gauge's name, figure, fill, and warn state.
 * @returns The gauge element.
 */
function Gauge({ label, value, percent, warn, title, compact }: GaugeProps) {
  const fill = Math.max(0, Math.min(100, percent));
  const angle = ((180 + fill * 1.8) * Math.PI) / 180;
  const endX = (20 + 16 * Math.cos(angle)).toFixed(2);
  const endY = (20 + 16 * Math.sin(angle)).toFixed(2);
  return (
    <div title={title} className={`flex flex-col items-center ${compact ? "w-9" : "w-10"}`}>
      <div className={`relative ${compact ? "w-9" : "w-10"}`}>
        <svg viewBox="0 0 40 22" className={`block w-full ${compact ? "h-4" : "h-5.5"}`} role="img" aria-label={`${label}: ${value}`}>
          <path d="M 4 20 A 16 16 0 0 1 36 20" fill="none" stroke="var(--muted)" strokeWidth={4} strokeLinecap="round" />
          {fill > 0.5 && (
            <path d={`M 4 20 A 16 16 0 0 1 ${endX} ${endY}`} fill="none"
              stroke={warn ? "var(--stat-warn)" : "var(--primary)"} strokeWidth={4} strokeLinecap="round" />
          )}
        </svg>
        <b className={`absolute inset-x-0 bottom-0 text-center leading-none text-foreground ${compact ? "text-[7px]" : "text-[10px]"}`}>{value}</b>
      </div>
      <span className={`mt-0.5 whitespace-nowrap uppercase tracking-wide text-muted-foreground ${compact ? "text-[7px] leading-none" : "text-[9px] leading-none"}`}>{label}</span>
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
 */
export function StreamStats() {
  const [detailed, setDetailed] = useState(() => localStorage.getItem(getPrefixedKey("stats_detailed")) === "true");
  const { framerate: configured } = useSettings();
  const framerate = configured > 0 ? configured : 60;
  const snapshot = useStreamSnapshot(true);
  const graphs = useStatsGraphs(snapshot, framerate);
  const { copied, copy } = useCopyReport(snapshot);
  const latest = snapshot ? snapshot.latest : null;
  const number = (key: string): number => (latest && typeof latest[key] === "number" ? (latest[key] as number) : 0);

  // The gauges both views share. The fps arc fills to the configured
  // framerate and warns under nine tenths of it — the one figure with a
  // configured value to fall short of; the latency arc fills as the reading
  // leaves its axis, so a full arc is the best reading; the bitrate arc is
  // the reading against the graph's own top, a scale with no good or bad
  // end. The host meters fill as shares of their wholes.
  // The bitrate gauge reads the graph's own newest point, so the two always
  // agree: the graph is bucketed by maximum once the history is long.
  const mbpsValues = graphs.mbps.series[0].values;
  const mbpsNow = mbpsValues.length ? mbpsValues[mbpsValues.length - 1] : 0;
  const streamGauges: GaugeSpec[] = [
    { key: "fps", label: "FPS", value: `${Math.round(number("fps"))}`, percent: (number("fps") / framerate) * 100,
      warn: number("fps") > 0 && number("fps") < framerate * 0.9 },
    { key: "mbps", label: "Mbps", value: mbpsNow.toFixed(1), percent: (mbpsNow / graphs.mbps.max) * 100 },
    { key: "rtt_ms", label: "RTT", value: `${Math.round(number("rtt_ms"))}`, percent: 100 - (number("rtt_ms") / graphs.rtt.max) * 100 },
  ];
  const meterGauges: GaugeSpec[] = streamMeters(latest).map((meter) => ({
    key: meter.key,
    label: meter.key === "gpumem" ? "VRAM" : meter.key.toUpperCase(),
    value: `${Math.round(meter.percent)}`,
    percent: meter.percent,
    title: meter.text,
  }));

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
      <div data-drag-handle className="flex h-[42px] items-center gap-2 rounded-lg border bg-background px-2 text-xs shadow-lg tabular-nums cursor-grab active:cursor-grabbing select-none">
        <div className="flex items-center gap-1 pointer-events-none">
          {streamGauges.map(({ key, ...gauge }) => <Gauge key={key} compact {...gauge} />)}
          {meterGauges.length > 0 && (
            <>
              <div className="mx-1 h-6 w-px bg-border" aria-hidden />
              {meterGauges.map(({ key, ...gauge }) => <Gauge key={key} compact {...gauge} />)}
            </>
          )}
        </div>
        {toggle}
      </div>
    );
  }

  return (
    <div className="stream-stats flex min-h-0 w-[min(92vw,22rem)] flex-col rounded-lg border bg-background py-2 text-xs shadow-lg tabular-nums">
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
        <div className="grid grid-cols-[repeat(auto-fit,minmax(2.5rem,1fr))] justify-items-center gap-y-1.5 border-b border-border pb-2">
          {[...streamGauges, ...meterGauges].map(({ key, ...gauge }) => (
            <Gauge key={key} {...gauge} />
          ))}
        </div>

        <SectionAccordion defaultValue={["pipeline"]}>
          <SectionItem value="pipeline" title={t("stats.pipeline")}>
            <StatRows skin={ROWS_SKIN} rows={snapshotRows(snapshot)} Mark={StatusMark} />
          </SectionItem>
          <SectionItem value="graphs" title={t("stats.graphs")}>
            <div className="flex flex-col gap-2.5"><Graphs skin={GRAPH_SKIN} graphs={graphs} /></div>
          </SectionItem>
          <SectionItem value="figures" title={t("stats.figures")}>
            <StatTiles skin={TILES_SKIN} tiles={snapshotTiles(snapshot)} Mark={StatusMark} />
          </SectionItem>
        </SectionAccordion>

        {latest && (latest.mic || latest.webcam) && (
          <div className="flex flex-col gap-1 text-muted-foreground pointer-events-none">
            {latest.mic && <div className="flex items-center gap-1.5"><Mic className="h-3.5 w-3.5" aria-label="mic" />{latest.mic}</div>}
            {latest.webcam && <div className="flex items-center gap-1.5"><Video className="h-3.5 w-3.5" aria-label="webcam" />{latest.webcam}</div>}
          </div>
        )}
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
