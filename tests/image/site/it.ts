/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Shared by the tester pages: report what a page saw, and follow the page the
 * harness names for this page's channel (`ch`, "main" unless the URL says).
 * The pages load it as the classic script `it.js`, which the image tier's
 * harness writes from this file with its types stripped
 * (`tests/image/image_lib.py` `install_site`).
 * @module
 */

/* exported IT */
const IT = {
    ch: new URLSearchParams(location.search).get('ch') || 'main',
    /**
     * Sends what the page saw to the site, which writes it where the harness reads it.
     * @param name The report's name.
     * @param data What the page saw, as JSON.
     * @returns The site's answer, or nothing where the site was not there.
     */
    report(name: string, data: unknown): Promise<Response | void> {
        return fetch('/report/' + name, { method: 'POST', body: JSON.stringify(data) }).catch(() => {});
    },
};
(() => {
    const here = location.pathname + location.search;
    const follow = async (): Promise<void> => {
        try {
            const next = (await (await fetch('/next/' + IT.ch, { cache: 'no-store' })).text()).trim();
            // The harness names a page of this site by its path.
            const page = next ? new URL(next, location.origin) : null;
            if (page && page.origin === location.origin && page.pathname + page.search !== here) {
                location.href = page.pathname + page.search;
            }
        } catch { /* the site restarts between cells */ }
    };
    setInterval(() => { void follow(); }, 500);
})();
