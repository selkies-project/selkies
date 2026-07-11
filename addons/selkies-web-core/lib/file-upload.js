/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * File uploads, shared by both transports.
 *
 * Every upload is a plain HTTP POST to /api/upload rather than a stream of
 * chunks over the WebSocket or a data channel: the browser's native HTTP path
 * and the server's C-accelerated aiohttp saturate the link, whereas per-message
 * chunk processing is bounded by pure-Python per-chunk work — and an upload
 * cannot stall or kill the realtime session socket. The destination path rides
 * URL-encoded in the X-Upload-Path header; progress is reported to the
 * dashboards as `{type: 'fileUpload'}` window messages (statuses: start /
 * progress / end / error / warning).
 *
 * One upload OPERATION (a file-picker set or a dropped tree) runs at a time:
 * sequential POSTs keep server-side writes ordered and the progress UI
 * coherent. `canUpload` is a per-core gate (e.g. shared/viewer sessions must
 * not upload).
 */
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

    function uploadFileObject(file, pathToSend) {
        return new Promise((resolve, reject) => {
            post({ status: 'start', fileName: pathToSend, fileSize: file.size });
            const report = (status, extra) =>
                post({ status, fileName: pathToSend, fileSize: file.size, ...extra });
            try {
                // Same-origin URL resolves any subfolder prefix.
                const url = new URL('api/upload', window.location.href).href;
                const xhr = new XMLHttpRequest();
                xhr.open('POST', url, true);
                xhr.withCredentials = true;
                xhr.setRequestHeader('Content-Type', 'application/octet-stream');
                xhr.setRequestHeader('X-Upload-Path', encodeURIComponent(pathToSend));
                xhr.upload.onprogress = (e) => {
                    const progress = (e.lengthComputable && file.size > 0)
                        ? Math.min(100, Math.round((e.loaded / e.total) * 100)) : 0;
                    report('progress', { progress });
                };
                xhr.onload = () => {
                    if (xhr.status >= 200 && xhr.status < 300) {
                        report('progress', { progress: 100 });
                        report('end');
                        resolve();
                    } else {
                        const msg = `upload failed (${xhr.status}): ${String(xhr.responseText || '').slice(0, 160)}`;
                        report('error', { message: msg });
                        reject(new Error(msg));
                    }
                };
                xhr.onerror = () => {
                    const msg = `network error uploading ${pathToSend}`;
                    report('error', { message: msg });
                    reject(new Error(msg));
                };
                xhr.send(file);
            } catch (error) {
                const msg = `error during upload of ${pathToSend}: ${error.message || error}`;
                post({ status: 'error', fileName: pathToSend, message: msg });
                reject(error);
            }
        });
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
