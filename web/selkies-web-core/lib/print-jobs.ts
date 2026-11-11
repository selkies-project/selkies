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
import { sessionAuthHeaders } from './session-token.ts';
import { isMobileClient } from './util.ts';

/** The blob URLs of the documents this page fetched, the only ones it prints. */
const fetchedDocuments = new Set<string>();

/**
 * `url` when it names a document this page fetched, else null: no other
 * address reaches a frame or a tab.
 * @param url The address a print request names.
 * @returns The blob URL, or null.
 */
function ownBlobUrl(url: unknown): string | null {
    if (typeof url !== 'string' || !fetchedDocuments.has(url)) return null;
    return url;
}

/**
 * Prints the PDF at `url`: on a desktop through a frame of no size, so the
 * document prints rather than the page around it, and the frame goes once
 * the dialog closes; on a touch-first client by opening it in a new tab,
 * which reaches the browser's own viewer only from within a user gesture.
 * @param url Blob URL of the PDF.
 */
export function printDocument(url: string): void {
    const own = ownBlobUrl(url);
    if (own === null) {
        console.warn('Print request ignored: not a document this page fetched.');
        return;
    }
    url = own;
    if (isMobileClient) {
        window.open(url, '_blank');
        return;
    }
    const frame = document.createElement('iframe');
    frame.style.cssText = 'position:fixed;right:0;bottom:0;width:0;height:0;border:0';
    frame.addEventListener('load', () => {
        const target = frame.contentWindow as Window & { print: () => void };
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

/** Options for a print jobs handler. */
export interface PrintJobsOptions {
  /** Whether an announced document is printed at once. */
  automatic: boolean;
}

/** A print jobs handler. */
export interface PrintJobs {
  announce: (name: string, sizeBytes: number) => Promise<void>;
  setAutomatic: (value: boolean) => void;
}

/**
 * Creates a print jobs handler.
 * @param options Whether printing is automatic.
 * @returns The handler.
 */
export function createPrintJobs({ automatic }: PrintJobsOptions): PrintJobs {
    let auto = !!automatic;
    return {
        /**
         * Fetches the document the server announced and hands it to the
         * dashboards, printing it when automatic printing is on and the
         * client is not touch-first, where no gesture backs the print.
         * @param name File name in the spool.
         * @param sizeBytes Its size, as announced.
         */
        async announce(name: string, sizeBytes: number): Promise<void> {
            const url = new URL('api/print/' + encodeURIComponent(name), window.location.href).href;
            let blob: Blob;
            try {
                const response = await fetch(url, { headers: sessionAuthHeaders(), credentials: 'same-origin' });
                if (!response.ok) throw new Error(`HTTP ${response.status}`);
                blob = await response.blob();
            } catch (e) {
                console.warn(`Printed document ${name} (${sizeBytes} bytes) was not fetched: ${(e as Error).message}`);
                return;
            }
            const objectUrl = URL.createObjectURL(new Blob([blob], { type: 'application/pdf' }));
            fetchedDocuments.add(objectUrl);
            window.postMessage({ type: 'printDocument', name, size: blob.size, url: objectUrl }, window.location.origin);
            if (auto && !isMobileClient) printDocument(objectUrl);
        },
        setAutomatic(value: boolean): void {
            auto = !!value;
        },
    };
}
