/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The strip over the stream: the stats section's four status marks and the
 * figures a glance needs (`streamStrip` in `lib/stream-stats-view.js`, which
 * the wish dashboard's floating overlay is the counterpart of), kept on while
 * the sidebar's switch says so, the sidebar open or not.
 *
 * It holds the numbers on (`holdStats`) while it is on screen in a visible tab,
 * reads the core's stream state each time the core announces a change, and
 * takes no pointer input, so nothing under it stops reaching the stream.
 * @module
 */
import { useEffect, useMemo, useState } from "react";
import { streamRows, streamStrip } from "../../../selkies-web-core/lib/stream-stats-view.js";
import { STATS_EVENT } from "../../../selkies-web-core/lib/stream-stats.js";
import StatusIcon from "./StatusIcon.jsx";
import { holdStats } from "./stats-hold.js";

/** The label each figure is captioned with, the tile's or graph's that shows it in the section. */
const CAPTIONS = { fps: "sections.stats.fpsLabel", rtt_ms: "sections.stats.latencyLabel" };

/**
 * @param {{t: function(string, (Object|string)=): string}} props
 */
export default function StreamStrip({ t }) {
  const [visible, setVisible] = useState(!document.hidden);
  const [snapshot, setSnapshot] = useState(null);

  useEffect(() => {
    const onVisibility = () => setVisible(!document.hidden);
    document.addEventListener("visibilitychange", onVisibility);
    return () => document.removeEventListener("visibilitychange", onVisibility);
  }, []);

  useEffect(() => {
    if (!visible) return undefined;
    holdStats(1);
    const read = () => {
      const stats = window.stream_stats || { latest: null };
      setSnapshot({
        info: window.stream_info || null,
        client: window.stream_client ? { ...window.stream_client } : null,
        latest: stats.latest,
      });
    };
    read();
    window.addEventListener(STATS_EVENT, read);
    return () => {
      window.removeEventListener(STATS_EVENT, read);
      holdStats(-1);
    };
  }, [visible]);

  const words = useMemo(() => ({
    hardware: t("sections.stats.hardware"),
    software: t("sections.stats.software"),
    unknown: t("sections.stats.tooltipMemoryNA"),
    throttled: t("sections.stats.throttled"),
    hardware_available: t("sections.stats.hardwareAvailable"),
    software_preferred: t("sections.stats.softwarePreferred"),
  }), [t]);

  if (!snapshot) return null;
  const { info, client, latest } = snapshot;
  const rows = streamRows(info, client, latest, words);
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
