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
 * @module
 */
import { sessionAuthHeaders } from './session-token.js';

/**
 * Opens the PDF at `url` in the browser's print dialog through a frame of no
 * size, so the document prints rather than the page around it. The frame
 * goes once the dialog closes.
 * @param {string} url Blob URL of the PDF.
 */
export function printDocument(url) {
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
         * dashboards, printing it when automatic printing is on.
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
            if (auto) printDocument(objectUrl);
        },
        setAutomatic(value) {
            auto = !!value;
        },
    };
}
