/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The browser tab's title.
 *
 * The tab is named after the web root's `manifest.json` until the server's
 * settings arrive. An operator who set `ui_title` names it from then on, and a
 * manifest answering late does not take that back. Without an explicit
 * `ui_title` the manifest's name stays, so a web root that renames the app in
 * its manifest keeps doing so.
 * @module
 */

let operatorTitle = '';

/**
 * Names the tab after the manifest, unless `ui_title` already named it.
 */
export function titleFromManifest() {
    document.title = 'Selkies';
    fetch('manifest.json')
        .then(response => response.json())
        .then(manifest => {
            if (manifest.name && !operatorTitle) {
                document.title = manifest.name;
            }
        })
        .catch(() => {
        });
}

/**
 * Names the tab after `ui_title` when the operator set it.
 * @param {Object<string, {value: *, overridden: boolean}>} settings The
 *     server's settings payload.
 */
export function titleFromSettings(settings) {
    const title = settings.ui_title;
    if (!title || !title.overridden || !title.value) return;
    operatorTitle = String(title.value);
    document.title = operatorTitle;
}
