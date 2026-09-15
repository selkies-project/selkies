/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Documents printed in the session, delivered as PDFs.
 *
 * The server announces each document that lands in its print spool. It is
 * fetched once, which takes it out of the spool, handed to the dashboards
 * as a blob URL on the `printDocument` window message, and opened in the
 * browser's print dialog when automatic printing is on. A dashboard's
 * `printRequest` message prints one again.
 *
 * The dialog is reached through a frame showing the PDF, which a desktop
 * engine renders with a viewer that prints it. A phone or tablet browser
 * has no such viewer in a frame: WebKit rasterises the first page into an
 * image, Chrome for Android loads nothing, and neither throws, so printing
 * there would print a blank frame or nothing at all. On a touch-first client
 * the document is instead opened as a tab of its own, where the browser's
 * own PDF viewer prints and saves it. A new tab needs a user gesture, which
 * an announcement arriving over the socket has none of, so automatic printing
 * does nothing there and the dashboards raise a notice with an open link per
 * document, since their menus are closed while an application prints.
 * @module
 */
import { sessionAuthHeaders } from './session-token.js';
import { isMobileClient } from './util.js';

/**
 * Prints the PDF at `url`: on a desktop through a frame of no size, so the
 * document prints rather than the page around it, and the frame goes once
 * the dialog closes; on a touch-first client by opening it in a new tab,
 * which reaches the browser's own viewer only from within a user gesture.
 * @param {string} url Blob URL of the PDF.
 */
export function printDocument(url) {
    if (isMobileClient) {
        window.open(url, '_blank');
        return;
    }
    const frame = document.createElement('iframe');
    frame.style.cssText = 'position:fixed;right:0;bottom:0;width:0;height:0;border:0';
    frame.addEventListener('load', () => {
        const target = frame.contentWindow;
        target.addEventListener('afterprint', () => frame.remove(), { once: true });
        try {
            target.focus();
            target.print();
        } catch (e) {
            frame.remove();
            window.open(url, '_blank');
        }
    });
    frame.src = url;
    document.body.appendChild(frame);
}

/**
 * @param {Object} options
 * @param {boolean} options.automatic Whether an announced document is printed at once.
 * @returns {{announce: function(string, number): Promise<void>, setAutomatic: function(boolean): void}}
 */
export function createPrintJobs({ automatic }) {
    let auto = !!automatic;
    return {
        /**
         * Fetches the document the server announced and hands it to the
         * dashboards, printing it when automatic printing is on and the
         * client is not touch-first, where no gesture backs the print.
         * @param {string} name File name in the spool.
         * @param {number} sizeBytes Its size, as announced.
         */
        async announce(name, sizeBytes) {
            const url = new URL('api/print/' + encodeURIComponent(name), window.location.href).href;
            let blob;
            try {
                const response = await fetch(url, { headers: sessionAuthHeaders(), credentials: 'same-origin' });
                if (!response.ok) throw new Error(`HTTP ${response.status}`);
                blob = await response.blob();
            } catch (e) {
                console.warn(`Printed document ${name} (${sizeBytes} bytes) was not fetched: ${e.message}`);
                return;
            }
            const objectUrl = URL.createObjectURL(new Blob([blob], { type: 'application/pdf' }));
            window.postMessage({ type: 'printDocument', name, size: blob.size, url: objectUrl }, window.location.origin);
            if (auto && !isMobileClient) printDocument(objectUrl);
        },
        setAutomatic(value) {
            auto = !!value;
        },
    };
}
