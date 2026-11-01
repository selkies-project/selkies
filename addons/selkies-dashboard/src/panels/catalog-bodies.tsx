/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The bodies of the catalog's panels (`catalog.ts`) that are no panel of
 * their own: the files' actions, the apps button, the sharing and gamepad
 * panels as each place draws them, and a settings panel's cards as a list.
 * @module
 */
import { FileUp, FolderOpen, LayoutGrid } from "lucide-react";
import { Button } from "@/components/ui/button";
import type { PanelCard, PanelPlace } from "@/panels/catalog";
import { Clip } from "@/panels/controls";
import { GamepadControls } from "@/panels/gamepads";
import { Sharing } from "@/panels/sharing";
import { useDashboard } from "@/state/dashboard";
import { t } from "@/i18n";

/** The files: an upload, and the file manager's dialog. */
export function FilesActions() {
  const d = useDashboard();
  return (
    <div className="files-actions">
      {d.renderable.fileUpload && (
        <Button variant="outline" className="min-w-0" title={t("sections.files.uploadButton")} onClick={() => window.dispatchEvent(new CustomEvent("requestFileUpload"))}>
          <FileUp /><Clip>{t("sections.files.uploadButton")}</Clip>
        </Button>
      )}
      {d.renderable.fileDownload && (
        <Button variant="outline" className="min-w-0" title={t("sections.files.downloadButtonTitle")} onClick={() => d.openDialog("files")}>
          <FolderOpen /><Clip>{t("sections.files.downloadButtonTitle")}</Clip>
        </Button>
      )}
    </div>
  );
}

/** The button that opens the apps catalog. */
export function AppsAction() {
  const d = useDashboard();
  return (
    <Button variant="outline" className="w-full min-w-0" title={t("sections.apps.openButton")} onClick={() => d.openDialog("apps")}>
      <LayoutGrid /><Clip>{t("sections.apps.openButton")}</Clip>
    </Button>
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

/** A settings panel's cards one after the other, as a section lists them. */
export function CardList({ cards }: { cards: PanelCard[] }) {
  return (
    <div className="space-y-3">
      {cards.map(({ value, Body }) => <Body key={value} />)}
    </div>
  );
}
