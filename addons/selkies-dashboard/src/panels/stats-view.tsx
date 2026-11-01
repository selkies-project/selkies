/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The graph, row, and tile markup every stats view shares, which a layout
 * draws in its own skin by passing the class names (`GraphSkin`, `RowsSkin`,
 * `TilesSkin`) and its status mark. What they draw is read by
 * `stats-data.ts`.
 * @module
 */
import * as React from "react";
import { graphPath } from "../../../selkies-web-core/lib/stream-stats-view.js";
import type { GraphData, StatusMarkComponent, snapshotRows, snapshotTiles } from "@/panels/stats-data";
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
 */
export function Graph({ skin, label, unit, series, max }: GraphProps) {
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

/** The three graphs, in order. */
export function Graphs({ skin, graphs }: { skin: GraphSkin; graphs: Record<"fps" | "mbps" | "rtt", GraphData> }) {
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

/** What the stream runs on, one row per stage: its mark, name, value, and what explains it. */
export function StatRows({ skin, rows, Mark, children }: {
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

/** The figures as tiles: value, its spread where it has one, and its name. */
export function StatTiles({ skin, tiles, Mark }: { skin: TilesSkin; tiles: ReturnType<typeof snapshotTiles>; Mark: StatusMarkComponent }) {
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
