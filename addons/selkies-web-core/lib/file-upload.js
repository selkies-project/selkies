/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * File uploads, shared by both transports.
 *
 * Every upload is HTTP POSTs to /api/upload rather than a stream of chunks
 * over the WebSocket or a data channel: the browser's native HTTP path and
 * the server's C-accelerated aiohttp saturate the link, whereas per-message
 * chunk processing is bounded by pure-Python per-chunk work — and an upload
 * cannot stall or kill the realtime session socket. The destination path rides
 * URL-encoded in the X-Upload-Path header; progress is reported to the
 * dashboards as `{type: 'fileUpload'}` window messages (statuses: start /
 * progress / end / error / warning).
 *
 * Files at or under UPLOAD_CHUNK_BYTES go up as ONE plain POST (the whole
 * Blob, no extra headers — the shape every server accepts). Larger files are
 * sliced with Blob.slice (never read into memory) into sequential POSTs of at
 * most UPLOAD_CHUNK_BYTES so no single request body exceeds a fronting
 * proxy's per-request cap (e.g. Cloudflare rejects bodies over 100 MB). Each
 * slice carries the same X-Upload-Path plus:
 *   X-Upload-Id:     opaque per-file transfer id
 *   X-Upload-Offset: absolute byte offset of the slice
 *   X-Upload-Total:  final file size in bytes
 *   X-Upload-Final:  "1" on the last slice
 * The server appends slices to a .part file and atomically renames it into
 * place on the final one. Progress is cumulative across slices, so the
 * dashboards render one smooth bar per file.
 *
 * One upload OPERATION (a file-picker set or a dropped tree) runs at a time:
 * sequential POSTs keep server-side writes ordered and the progress UI
 * coherent. `canUpload` is a per-core gate (e.g. shared/viewer sessions must
 * not upload).
 */
const UPLOAD_CHUNK_BYTES = 64 * 1024 * 1024;

export function createFileUploader({ canUpload = () => true } = {}) {
    let operationInFlight = false;

    function post(payload) {
        window.postMessage({ type: 'fileUpload', payload }, window.location.origin);
    }

    function beginOperation() {
        if (operationInFlight) {
            console.warn("Simultaneous uploading of files with distinct upload operations is not supported yet");
            post({ status: 'warning', fileName: '_N/A_', message: "Please let the ongoing upload complete." });
            return false;
        }
        operationInFlight = true;
        return true;
    }

    // One POST of `body` (a File or Blob slice) to `url`. Resolves on 2xx,
    // rejects with the dashboard-facing message otherwise; upload progress is
    // relayed to `onProgress` raw so the caller can accumulate across slices.
    function postUploadBody(url, pathToSend, body, extraHeaders, onProgress) {
        return new Promise((resolve, reject) => {
            const xhr = new XMLHttpRequest();
            xhr.open('POST', url, true);
            xhr.withCredentials = true;
            xhr.setRequestHeader('Content-Type', 'application/octet-stream');
            xhr.setRequestHeader('X-Upload-Path', encodeURIComponent(pathToSend));
            for (const [name, value] of Object.entries(extraHeaders || {})) {
                xhr.setRequestHeader(name, value);
            }
            xhr.upload.onprogress = onProgress;
            xhr.onload = () => {
                if (xhr.status >= 200 && xhr.status < 300) {
                    resolve();
                } else {
                    reject(new Error(`upload failed (${xhr.status}): ${String(xhr.responseText || '').slice(0, 160)}`));
                }
            };
            xhr.onerror = () => {
                reject(new Error(`network error uploading ${pathToSend}`));
            };
            xhr.send(body);
        });
    }

    async function uploadFileObject(file, pathToSend) {
        post({ status: 'start', fileName: pathToSend, fileSize: file.size });
        const report = (status, extra) =>
            post({ status, fileName: pathToSend, fileSize: file.size, ...extra });
        const percentOf = (sentBytes) => (file.size > 0)
            ? Math.min(100, Math.round((sentBytes / file.size) * 100)) : 0;
        try {
            // Same-origin URL resolves any subfolder prefix.
            const url = new URL('api/upload', window.location.href).href;
            if (file.size <= UPLOAD_CHUNK_BYTES) {
                // Single plain POST — no chunk headers.
                await postUploadBody(url, pathToSend, file, null, (e) => {
                    const progress = (e.lengthComputable && file.size > 0)
                        ? Math.min(100, Math.round((e.loaded / e.total) * 100)) : 0;
                    report('progress', { progress });
                });
            } else {
                const transferId = (window.crypto && crypto.randomUUID)
                    ? crypto.randomUUID()
                    : `${Date.now()}-${Math.random().toString(36).slice(2)}`;
                for (let offset = 0; offset < file.size;) {
                    const end = Math.min(offset + UPLOAD_CHUNK_BYTES, file.size);
                    const headers = {
                        'X-Upload-Id': transferId,
                        'X-Upload-Offset': String(offset),
                        'X-Upload-Total': String(file.size),
                    };
                    if (end >= file.size) headers['X-Upload-Final'] = '1';
                    const sentBefore = offset;
                    await postUploadBody(url, pathToSend, file.slice(offset, end), headers, (e) => {
                        if (!e.lengthComputable) return;
                        // Cumulative across slices: one smooth bar per file.
                        report('progress', { progress: percentOf(sentBefore + e.loaded) });
                    });
                    offset = end;
                    report('progress', { progress: percentOf(offset) });
                }
            }
            report('progress', { progress: 100 });
            report('end');
        } catch (error) {
            const msg = (error && error.message) ? error.message : `error during upload of ${pathToSend}: ${error}`;
            report('error', { message: msg });
            throw error;
        }
    }

    function getFileFromEntry(fileEntry) {
        return new Promise((resolve, reject) => fileEntry.file(resolve, reject));
    }

    async function handleDroppedEntry(entry, basePathFallback = "") {
        let pathToSend;
        // entry.fullPath preserves a dropped directory's internal structure; the
        // fallback rebuilds it from the recursion for browsers without fullPath.
        if (entry.fullPath && typeof entry.fullPath === 'string' && entry.fullPath !== entry.name &&
            (entry.fullPath.includes('/') || entry.fullPath.includes('\\'))) {
            pathToSend = entry.fullPath;
            if (pathToSend.startsWith('/')) {
                pathToSend = pathToSend.substring(1);
            }
        } else {
            pathToSend = basePathFallback ? `${basePathFallback}/${entry.name}` : entry.name;
        }

        if (entry.isFile) {
            try {
                const file = await getFileFromEntry(entry);
                await uploadFileObject(file, pathToSend);
            } catch (err) {
                console.error(`Error processing file ${pathToSend}: ${err}`);
                post({ status: 'error', fileName: pathToSend, message: `Error processing file: ${err.message || err}` });
            }
        } else if (entry.isDirectory) {
            const dirReader = entry.createReader();
            let entries;
            do {
                entries = await new Promise((resolve, reject) => dirReader.readEntries(resolve, reject));
                for (const subEntry of entries) {
                    await handleDroppedEntry(subEntry, pathToSend);
                }
            } while (entries.length > 0);
        }
    }

    function handleRequestFileUpload() {
        if (!canUpload()) {
            console.log("File upload blocked (shared/viewer session).");
            return;
        }
        const hiddenInput = document.getElementById('globalFileInput');
        if (!hiddenInput) {
            console.error("Global file input not found!");
            return;
        }
        hiddenInput.click();
    }

    async function handleFileInputChange(event) {
        const files = event.target.files;
        if (!canUpload() || !files || files.length === 0) {
            event.target.value = null;
            return;
        }
        if (!beginOperation()) {
            event.target.value = null;
            return;
        }
        console.log(`File input changed, processing ${files.length} files sequentially.`);
        try {
            for (let i = 0; i < files.length; i++) {
                const file = files[i];
                await uploadFileObject(file, file.name);
            }
        } catch (error) {
            const errorMsg = `An error occurred during the file input upload process: ${error.message || error}`;
            console.error(errorMsg);
            post({ status: 'error', fileName: 'N/A', message: errorMsg });
        } finally {
            event.target.value = null;
            operationInFlight = false;
        }
    }

    function handleDragOver(ev) {
        ev.preventDefault();
        ev.dataTransfer.dropEffect = canUpload() ? 'copy' : 'none';
    }

    async function handleDrop(ev) {
        ev.preventDefault();
        ev.stopPropagation();
        if (!canUpload()) {
            console.log("File upload via drag-drop blocked (shared/viewer session).");
            return;
        }
        if (!beginOperation()) {
            return;
        }
        try {
            const entriesToProcess = [];
            if (ev.dataTransfer.items) {
                for (let i = 0; i < ev.dataTransfer.items.length; i++) {
                    const item = ev.dataTransfer.items[i];
                    if (item.kind !== 'file') continue;
                    let entry = null;
                    if (typeof item.webkitGetAsEntry === 'function') entry = item.webkitGetAsEntry();
                    else if (typeof item.getAsEntry === 'function') entry = item.getAsEntry();
                    if (entry) entriesToProcess.push(entry);
                }
            } else if (ev.dataTransfer.files.length > 0) {
                for (let i = 0; i < ev.dataTransfer.files.length; i++) {
                    await uploadFileObject(ev.dataTransfer.files[i], ev.dataTransfer.files[i].name);
                }
                return;
            }
            try {
                for (const entry of entriesToProcess) await handleDroppedEntry(entry);
            } catch (error) {
                const errorMsg = `Error during sequential upload: ${error.message || error}`;
                post({ status: 'error', fileName: 'N/A', message: errorMsg });
            }
        } finally {
            operationInFlight = false;
        }
    }

    return {
        uploadFileObject,
        handleRequestFileUpload,
        handleFileInputChange,
        handleDragOver,
        handleDrop,
    };
}
