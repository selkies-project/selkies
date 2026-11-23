/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The bodies of the catalog's panels (`catalog.ts`) that are no panel of
 * their own: the files' actions, the sharing and gamepad
 * panels as each place draws them, the Pelorus chat, and a settings panel's
 * cards as a list.
 * @module
 */
import { FileUp, FolderOpen } from "lucide-react";
import type { PanelCard, PanelPlace } from "@/panels/catalog";
import { ActionButton, Clip } from "@/panels/controls";
import { GamepadControls } from "@/panels/gamepads";
import { PelorusCompact } from "@/pelorus/compact";
import { Sharing } from "@/panels/sharing";
import { useDashboard } from "@/state/dashboard";
import { t } from "@/i18n";
import { getRoutePrefix } from "@/utils";
import { readUrlFragment } from "../../../selkies-web-core/lib/page-url.ts";
import { withSessionToken } from "../../../selkies-web-core/lib/session-token.ts";

/** The files: an upload, and the file manager's dialog. */
export function FilesActions() {
  const d = useDashboard();
  return (
    <div className="files-actions">
      {d.renderable.fileUpload && (
        <ActionButton title={t("sections.files.uploadButton")} onClick={() => window.dispatchEvent(new CustomEvent("requestFileUpload"))}>
          <FileUp /><Clip>{t("sections.files.uploadButton")}</Clip>
        </ActionButton>
      )}
      {d.renderable.fileDownload && (
        <ActionButton title={t("sections.files.downloadButtonTitle")} onClick={() => d.openDialog("files")}>
          <FolderOpen /><Clip>{t("sections.files.downloadButtonTitle")}</Clip>
        </ActionButton>
      )}
    </div>
  );
}

/** The sharing links, written out under their names in a section. */
export function SharingBody({ place }: { place: PanelPlace }) {
  return <Sharing show showUrl={place === "section"} />;
}

/** The gamepads: floating, with the input switch a layout with sections has as a toggle of its own; in a section, rumble as a pressed button. */
export function GamepadsBody({ place }: { place: PanelPlace }) {
  return <GamepadControls showInput={place === "float"} rumbleButton={place === "section"} />;
}

/**
 * The Pelorus page this page pops out: on this page's origin, written out so a
 * prefix starting with `//` names no other host, and carrying this page's
 * session token the way it came, in the query or in the fragment, since the
 * session cookie may not reach a window of its own.
 * @param prefix The route prefix the client is served under.
 * @returns The URL to open.
 */
function pelorusPageURL(prefix: string): string {
  const page = withSessionToken(`${window.location.origin}${prefix}/pelorus/`);
  const { token } = readUrlFragment();
  return token ? `${page}#token=${encodeURIComponent(token)}` : page;
}

/** The Pelorus prompt against this server's API, with the page of its own one press away. */
export function PelorusBody() {
  const prefix = getRoutePrefix();
  return <PelorusCompact base={`${prefix}/api/pelorus`} popoutHref={pelorusPageURL(prefix)} />;
}

/** A settings panel's cards one after the other, as a section lists them. */
export function CardList({ cards }: { cards: PanelCard[] }) {
  return (
    <div className="flex flex-col gap-(--stack-gap)">
      {cards.map(({ value, Body }) => <Body key={value} />)}
    </div>
  );
}
