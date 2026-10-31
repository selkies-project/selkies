/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What the dashboard says beside the stream whatever the layout: the toasts
 * for the core's `fileUpload` channel (upload progress and failures,
 * clipboard-image refusals, app commands that did not run), the
 * `printDocument` notice a touch-first client gets, the notice the WebRTC
 * core's `transportAdvice` raises while its media path keeps failing beside a
 * working signaling socket (withdrawn once a session connects, offering the
 * switch to WebSockets where the settings would: dual mode on, not a viewer),
 * and the mark over the stream while the server finds this page's connection
 * poor (`connection` in `window.stream_client`), which takes no pointer
 * input and sits above the soft keys (`--soft-keys-top`).
 *
 * These listen from the page's root rather than from a panel: a listener that
 * lives in a menu is unmounted whenever that menu is closed, and these
 * arrive while the user is somewhere else in the interface.
 * @module
 */
import { useEffect, useState } from "react";
import { toast, Toaster as Sonner } from "sonner";
import { Printer, TriangleAlert, CircleCheck, Info, OctagonX, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { useTheme } from "@/components/ui/theme-provider";
import { useDashboard } from "@/state/dashboard";
import { computeRenderableSettings, getLastServerSettings, isMobileClient, isViewerUrlMode } from "@/utils";
import { resolveFailedAppCommand } from "../../../selkies-web-core/lib/app-commands.js";
import { switchStreamMode } from "../../../selkies-web-core/lib/mode-switch.js";
import { STATS_EVENT } from "../../../selkies-web-core/lib/stream-stats.js";
import { t } from "@/i18n";

const TRANSPORT_TOAST_ID = "transport-advice";

/** The toaster, following the theme, with the kit's icons. */
export function Toaster() {
  const { theme } = useTheme();
  return (
    <Sonner position="bottom-right" richColors closeButton theme={theme} className="toaster group"
      icons={{
        success: <CircleCheck className="size-4" />,
        info: <Info className="size-4" />,
        warning: <TriangleAlert className="size-4" />,
        error: <OctagonX className="size-4" />,
        loading: <Loader2 className="size-4 animate-spin" />,
      }}
      style={{ "--normal-bg": "var(--popover)", "--normal-text": "var(--popover-foreground)", "--normal-border": "var(--border)", "--border-radius": "var(--radius)" } as React.CSSProperties}
    />
  );
}

/**
 * Renders nothing; it only listens. A warning carries an optional translation
 * code (`clipboard*`, `commandFailed`, `keyboardLock*`); the translator returns
 * the key itself for an unknown code, since a future core may ship new ones,
 * so the raw message is the fallback.
 */
export function Notices() {
  useEffect(() => {
    let viewer = isViewerUrlMode;
    const onMessage = (event: MessageEvent) => {
      if (event.origin !== window.location.origin) return;
      const message = event.data;
      if (typeof message !== 'object' || message === null) return;
      if (message.type === "clientRoleUpdate") {
        viewer = message.role === "viewer";
        return;
      }
      if (message.type === "transportAdvice") {
        if (message.offer !== "websockets") {
          toast.dismiss(TRANSPORT_TOAST_ID);
          return;
        }
        const dualMode = computeRenderableSettings(getLastServerSettings()).enableDualMode || !!(window as any).__SELKIES_DUAL_MODE__;
        const canSwitch = dualMode && !viewer;
        toast.warning(t("notifications.webrtcFailedTitle"), {
          id: TRANSPORT_TOAST_ID,
          duration: Infinity,
          className: "transport-advice",
          description: canSwitch ? t("notifications.webrtcFailedSwitch") : t("notifications.webrtcFailedNoSwitch"),
          action: canSwitch ? { label: t("notifications.switchToWebsockets"), onClick: () => { switchStreamMode("websockets"); } } : undefined,
        });
        return;
      }
      if (message.type === 'printDocument' && isMobileClient && typeof message.url === 'string') {
        const url: string = message.url;
        // One row in place of a title: the link, the name, the printer mark at the end.
        toast(
          <div className="flex w-full items-center gap-2">
            <Button variant="outline" size="sm" nativeButton={false} render={<a href={url} target="_blank" onClick={() => toast.dismiss(url)} />}>
              {t('sections.printing.openButton')}
            </Button>
            <span className="min-w-0 flex-1 truncate" title={message.name}>{message.name}</span>
            <Printer className="size-4 shrink-0 text-muted-foreground" aria-hidden="true" />
          </div>,
          { id: url, duration: Infinity, classNames: { content: 'min-w-0 flex-1', title: 'w-full' } },
        );
        return;
      }
      if (message.type !== 'fileUpload') return;
      const { status, fileName, progress, message: errMsg, code } = message.payload;
      if (status === 'start') {
        toast.loading(t('uploads.uploadingFile', { fileName }), { id: fileName });
      } else if (status === 'progress') {
        toast.loading(t('uploads.uploadingFileProgress', { fileName, progress }), { id: fileName });
      } else if (status === 'end') {
        toast.success(t('uploads.uploadSuccessFile', { fileName }), { id: fileName });
      } else if (status === 'error') {
        const errorMessage = errMsg ? `${t('notifications.errorPrefix')} ${errMsg}` : t('notifications.unknownError');
        toast.error(t('uploads.uploadFailedFile', { fileName, errorMessage }), { id: fileName });
      } else if (status === 'warning') {
        // A failed apps command settles its pending optimistic update first;
        // a stale launch match is lifecycle noise, not a notice.
        if (code === 'commandFailed' && !resolveFailedAppCommand(errMsg)) return;
        const codeKey = (typeof code === 'string' && (code.startsWith('clipboard') || code === 'commandFailed' || code.startsWith('keyboardLock')))
          ? `notifications.${code}` : null;
        const codeMsg = codeKey ? t(codeKey, { detail: errMsg }) : null;
        toast.warning((codeMsg && codeMsg !== codeKey) ? codeMsg : (errMsg || t('notifications.unknownError')), { id: fileName });
      }
    };
    window.addEventListener('message', onMessage);
    return () => window.removeEventListener('message', onMessage);
  }, []);
  return null;
}

const isPoor = () => (window as any).stream_client?.connection === "poor";

/** The poor-connection mark; `ui_show_connection_indicator` hides it. */
export function ConnectionIndicator() {
  const { renderable } = useDashboard();
  const [poor, setPoor] = useState(isPoor);
  useEffect(() => {
    const read = () => setPoor(isPoor());
    read();
    window.addEventListener(STATS_EVENT, read);
    return () => window.removeEventListener(STATS_EVENT, read);
  }, []);
  if (!poor || !renderable.connectionIndicator) return null;
  return (
    <div role="status" className="connection-indicator pointer-events-none fixed left-5 z-[2040] flex select-none items-center gap-1.5 rounded-md bg-black/55 px-2.5 py-1 text-xs text-white"
      style={{ bottom: "min(calc(var(--soft-keys-top, 0.75rem) + 0.5rem), calc(100% - 2.5rem))" }}>
      <TriangleAlert className="size-3.5 shrink-0 text-[var(--stat-warn)]" aria-hidden />
      {t("notifications.poorConnection")}
    </div>
  );
}
