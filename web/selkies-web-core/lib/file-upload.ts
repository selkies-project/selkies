/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * File uploads, shared by both transports.
 *
 * Every upload is HTTP POSTs to `/api/upload` rather than a stream of chunks
 * over the WebSocket or a data channel: the browser's native HTTP path
 * saturates the link, where chunks over the session pay framing and dispatch
 * for each message, and an upload cannot stall or kill the realtime session
 * socket. The destination path rides URL-encoded in the `X-Upload-Path`
 * header, the secure-mode session token as a Bearer header, and progress is
 * reported to the dashboards as `{type: 'fileUpload'}` window messages with
 * the statuses `start`, `progress`, `end`, `error`, and `warning`.
 *
 * Files at or under `UPLOAD_CHUNK_BYTES` go up as one plain POST (the whole
 * Blob, no extra headers, the shape every server accepts). Larger files are
 * sliced with `Blob.slice`, never read into memory, into sequential POSTs of
 * at most `UPLOAD_CHUNK_BYTES` so no single request body exceeds a fronting
 * proxy's per-request cap (Cloudflare rejects bodies over 100 MB). Each slice
 * carries the same `X-Upload-Path` plus `X-Upload-Id` (an opaque per-file
 * transfer id), `X-Upload-Offset` (the slice's absolute byte offset),
 * `X-Upload-Total` (the final file size in bytes), and, on the last slice,
 * `X-Upload-Final: 1`. The server appends slices to a `.part` file and
 * atomically renames it into place on the final one. Progress is cumulative
 * across slices, so the dashboards render one smooth bar per file.
 *
 * One upload operation (a file-picker set or a dropped tree) runs at a time:
 * sequential POSTs keep server-side writes ordered and the progress UI
 * coherent.
 * @module
 */
import { sessionAuthHeaders } from './session-token.ts';

const UPLOAD_CHUNK_BYTES = 64 * 1024 * 1024;

/** The upload handlers a core wires to its page. */
export interface FileUploader {
  /** Uploads one file to the given destination path, reporting progress. */
  uploadFileObject: (file: File, pathToSend: string) => Promise<void>;
  /** Opens the hidden file picker; a `requestFileUpload` event whose `detail.directory` names a directory under the file-manager root sends the picked files there rather than to the root. */
  handleRequestFileUpload: (event?: CustomEvent) => void;
  /** Uploads the picker's files sequentially. */
  handleFileInputChange: (event: Event) => Promise<void>;
  /** Sets the drop effect. */
  handleDragOver: (ev: DragEvent) => void;
  /** Uploads dropped files and directory trees sequentially. */
  handleDrop: (ev: DragEvent) => Promise<void>;
}

/** Options for a file uploader. */
export interface FileUploaderOptions {
  /** Per-core gate; a shared or viewer session returns `false` and every entry point refuses. */
  canUpload?: () => boolean;
}

/** A filesystem entry, as the drop API hands it over. */
interface FileSystemLikeEntry {
  name: string;
  fullPath?: string;
  isFile: boolean;
  isDirectory: boolean;
  file: (success: (file: File) => void, error: (error: DOMException) => void) => void;
  createReader: () => FileSystemDirectoryReaderLike;
}

/** A directory reader, as the drop API hands it over. */
interface FileSystemDirectoryReaderLike {
  readEntries: (success: (entries: FileSystemLikeEntry[]) => void, error: (error: DOMException) => void) => void;
}

/**
 * Creates the upload handlers a core wires to its page.
 * @param options Per-core gate.
 * @returns The upload handlers.
 */
export function createFileUploader({ canUpload = () => true }: FileUploaderOptions = {}): FileUploader {
    let operationInFlight = false;
    // Where the picker's files go, relative to the file-manager root: set by
    // the request that opened the picker, so a dashboard browsing a folder can
    // upload into it, and taken by the one change it opened the picker for, so
    // no later upload inherits it. The server confines the path either way.
    let pickerDirectory = '';

    function post(payload: Record<string, unknown>): void {
        window.postMessage({ type: 'fileUpload', payload }, window.location.origin);
    }

    function beginOperation(): boolean {
        if (operationInFlight) {
            console.warn("Simultaneous uploading of files with distinct upload operations is not supported yet");
            post({ status: 'warning', fileName: '_N/A_', message: "Please let the ongoing upload complete." });
            return false;
        }
        operationInFlight = true;
        return true;
    }

    /**
     * One POST of a File or Blob slice. Resolves on 2xx and rejects with the
     * dashboard-facing message otherwise; upload progress is relayed to
     * `onProgress` raw so the caller can accumulate across slices.
     * @param url The upload endpoint.
     * @param pathToSend Destination path.
     * @param body The bytes.
     * @param extraHeaders Slice headers, if any.
     * @param onProgress Upload progress listener.
     * @returns Resolves when the server answers 2xx.
     */
    function postUploadBody(url: string, pathToSend: string, body: Blob, extraHeaders: Record<string, string> | null, onProgress: (e: ProgressEvent) => void): Promise<void> {
        return new Promise<void>((resolve, reject) => {
            const xhr = new XMLHttpRequest();
            xhr.open('POST', url, true);
            xhr.withCredentials = true;
            xhr.setRequestHeader('Content-Type', 'application/octet-stream');
            xhr.setRequestHeader('X-Upload-Path', encodeURIComponent(pathToSend));
            for (const [name, value] of Object.entries(sessionAuthHeaders(extraHeaders || undefined))) {
                xhr.setRequestHeader(name, value);
            }
            xhr.upload.onprogress = onProgress;
            xhr.onload = (): void => {
                if (xhr.status >= 200 && xhr.status < 300) {
                    resolve();
                } else {
                    reject(new Error(`upload failed (${xhr.status}): ${String(xhr.responseText || '').slice(0, 160)}`));
                }
            };
            xhr.onerror = (): void => {
                reject(new Error(`network error uploading ${pathToSend}`));
            };
            xhr.send(body);
        });
    }

    /**
     * Uploads one file, as a single POST or as slices by size, reporting
     * `start`, cumulative `progress`, and `end` or `error`.
     * @param file The file.
     * @param pathToSend Destination path.
     * @throws The upload failure, after reporting it.
     */
    async function uploadFileObject(file: File, pathToSend: string): Promise<void> {
        post({ status: 'start', fileName: pathToSend, fileSize: file.size });
        const report = (status: string, extra?: Record<string, unknown>): void =>
            post({ status, fileName: pathToSend, fileSize: file.size, ...extra });
        const percentOf = (sentBytes: number): number => (file.size > 0)
            ? Math.min(100, Math.round((sentBytes / file.size) * 100)) : 0;
        try {
            const url = new URL('api/upload', window.location.href).href;
            if (file.size <= UPLOAD_CHUNK_BYTES) {
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
                    const headers: Record<string, string> = {
                        'X-Upload-Id': transferId,
                        'X-Upload-Offset': String(offset),
                        'X-Upload-Total': String(file.size),
                    };
                    if (end >= file.size) headers['X-Upload-Final'] = '1';
                    const sentBefore = offset;
                    await postUploadBody(url, pathToSend, file.slice(offset, end), headers, (e) => {
                        if (!e.lengthComputable) return;
                        report('progress', { progress: percentOf(sentBefore + e.loaded) });
                    });
                    offset = end;
                    report('progress', { progress: percentOf(offset) });
                }
            }
            report('progress', { progress: 100 });
            report('end');
        } catch (error) {
            const errorAsError = error as Error | undefined;
            const msg = (errorAsError && errorAsError.message) ? errorAsError.message : `error during upload of ${pathToSend}: ${String(error)}`;
            report('error', { message: msg });
            throw error;
        }
    }

    function getFileFromEntry(fileEntry: FileSystemLikeEntry): Promise<File> {
        return new Promise<File>((resolve, reject) => fileEntry.file(resolve, reject));
    }

    /**
     * Uploads a dropped file or, recursively, a dropped directory. The
     * entry's `fullPath` preserves a dropped directory's internal structure;
     * the fallback rebuilds it from the recursion for browsers without one.
     * @param entry The dropped entry.
     * @param basePathFallback Parent path when `fullPath` is absent.
     */
    async function handleDroppedEntry(entry: FileSystemLikeEntry, basePathFallback = ""): Promise<void> {
        let pathToSend: string;
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
                const errWithMessage = err as { message?: string } | undefined;
                console.error(`Error processing file ${pathToSend}: ${String(err)}`);
                post({ status: 'error', fileName: pathToSend, message: `Error processing file: ${errWithMessage?.message || String(errWithMessage || err)}` });
            }
        } else if (entry.isDirectory) {
            const dirReader = entry.createReader();
            let entries: FileSystemLikeEntry[];
            do {
                entries = await new Promise<FileSystemLikeEntry[]>((resolve, reject) => dirReader.readEntries(resolve, reject));
                for (const subEntry of entries) {
                    await handleDroppedEntry(subEntry, pathToSend);
                }
            } while (entries.length > 0);
        }
    }

    function handleRequestFileUpload(event?: CustomEvent): void {
        if (!canUpload()) {
            console.log("File upload blocked (shared/viewer session).");
            return;
        }
        const detail = (event as CustomEvent<Record<string, unknown>> | undefined)?.detail as { directory?: unknown } | undefined;
        const directory = detail?.directory;
        pickerDirectory = typeof directory === 'string' ? directory.replace(/^\/+|\/+$/g, '') : '';
        const hiddenInput = document.getElementById('globalFileInput') as HTMLInputElement | null;
        if (!hiddenInput) {
            console.error("Global file input not found!");
            return;
        }
        hiddenInput.click();
    }

    async function handleFileInputChange(event: Event): Promise<void> {
        const target = event.target as HTMLInputElement;
        const files = target.files;
        const directory = pickerDirectory;
        pickerDirectory = '';
        if (!canUpload() || !files || files.length === 0) {
            target.value = '';
            return;
        }
        if (!beginOperation()) {
            target.value = '';
            return;
        }
        console.log(`File input changed, processing ${files.length} files sequentially.`);
        try {
            for (let i = 0; i < files.length; i++) {
                const file = files[i];
                await uploadFileObject(file, directory ? `${directory}/${file.name}` : file.name);
            }
        } catch (error) {
            const errorWithMessage = error as { message?: string };
            const errorMsg = `An error occurred during the file input upload process: ${errorWithMessage.message || String(errorWithMessage || error)}`;
            console.error(errorMsg);
            post({ status: 'error', fileName: 'N/A', message: errorMsg });
        } finally {
            target.value = '';
            operationInFlight = false;
        }
    }

    function handleDragOver(ev: DragEvent): void {
        ev.preventDefault();
        ev.dataTransfer!.dropEffect = canUpload() ? 'copy' : 'none';
    }

    async function handleDrop(ev: DragEvent): Promise<void> {
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
            const entriesToProcess: FileSystemLikeEntry[] = [];
            const transfer = ev.dataTransfer!;
            if (transfer.items) {
                for (let i = 0; i < transfer.items.length; i++) {
                    const item = transfer.items[i];
                    if (item.kind !== 'file') continue;
                    let entry: FileSystemLikeEntry | null = null;
                    const dataTransferItemWithEntry = item as unknown as { webkitGetAsEntry?: () => FileSystemLikeEntry; getAsEntry?: () => FileSystemLikeEntry };
                    if (typeof dataTransferItemWithEntry.webkitGetAsEntry === 'function') entry = dataTransferItemWithEntry.webkitGetAsEntry();
                    else if (typeof dataTransferItemWithEntry.getAsEntry === 'function') entry = dataTransferItemWithEntry.getAsEntry();
                    if (entry) entriesToProcess.push(entry);
                }
            } else if (transfer.files.length > 0) {
                for (let i = 0; i < transfer.files.length; i++) {
                    await uploadFileObject(transfer.files[i], transfer.files[i].name);
                }
                return;
            }
            try {
                for (const entry of entriesToProcess) await handleDroppedEntry(entry);
            } catch (error) {
                const errorWithMessage = error as { message?: string };
                const errorMsg = `Error during sequential upload: ${errorWithMessage.message || String(errorWithMessage || error)}`;
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
