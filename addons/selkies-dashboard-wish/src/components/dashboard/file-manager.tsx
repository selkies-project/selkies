/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

"use client"

import * as React from "react"
import {
  ChevronRight,
  Download,
  Eye,
  EyeOff,
  File as FileIcon,
  Folder,
  FolderPlus,
  Grid2X2,
  List,
  Rows3,
  UploadCloud,
} from "lucide-react"

import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { ScrollArea } from "@/components/ui/scroll-area"
import { cn } from "@/lib/utils"
import { toast } from "sonner"

import {
  sessionAuthHeaders,
  withSessionToken,
} from "../../../../selkies-web-core/lib/session-token.js"

/** Bytes above which an upload is posted in slices rather than in one request. */
const UPLOAD_CHUNK_BYTES = 64 * 1024 * 1024

/**
 * Posts one file to `api/upload`, sliced when it is over `UPLOAD_CHUNK_BYTES`.
 * The slices carry the transfer id and byte offset the server reassembles from,
 * and the browser's cached Basic credentials ride the request like any other.
 *
 * @param file The file to send.
 * @param pathToSend Its path relative to the file-manager root.
 * @param onProgress Called with a percentage as the transfer advances.
 */
async function uploadFile(file: File, pathToSend: string, onProgress?: (progress: number) => void) {
  const url = new URL('api/upload', window.location.href).href

  async function postChunk(body: Blob, headers: Record<string, string> = {}) {
    return new Promise<void>((resolve, reject) => {
      const xhr = new XMLHttpRequest()
      xhr.open('POST', url, true)
      xhr.withCredentials = true
      xhr.setRequestHeader('Content-Type', 'application/octet-stream')
      xhr.setRequestHeader('X-Upload-Path', encodeURIComponent(pathToSend))

      // Secure mode accepts the session token as a Bearer header, so an upload
      // does not depend on the session cookie having been written.
      for (const [name, value] of Object.entries(sessionAuthHeaders(headers))) {
        xhr.setRequestHeader(name, value)
      }

      xhr.upload.onprogress = (e) => {
        if (e.lengthComputable && onProgress) {
          onProgress(Math.round((e.loaded / e.total) * 100))
        }
      }

      xhr.onload = () => {
        if (xhr.status >= 200 && xhr.status < 300) {
          resolve()
        } else {
          reject(new Error(`Upload failed (${xhr.status})`))
        }
      }

      xhr.onerror = () => reject(new Error(`Network error uploading ${pathToSend}`))
      xhr.send(body)
    })
  }

  if (file.size <= UPLOAD_CHUNK_BYTES) {
    await postChunk(file)
    onProgress?.(100)
  } else {
    // randomUUID is only exposed in a secure context, and a session reached
    // over plain HTTP on a LAN address has none, so the id falls back the way
    // the core uploader does rather than throwing out of the whole loop.
    const transferId = (window.crypto && crypto.randomUUID)
      ? crypto.randomUUID()
      : `${Date.now()}-${Math.random().toString(36).slice(2)}`

    for (let offset = 0; offset < file.size; offset += UPLOAD_CHUNK_BYTES) {
      const end = Math.min(offset + UPLOAD_CHUNK_BYTES, file.size)
      const chunk = file.slice(offset, end)

      const headers: Record<string, string> = {
        'X-Upload-Id': transferId,
        'X-Upload-Offset': String(offset),
        'X-Upload-Total': String(file.size),
      }

      if (end >= file.size) {
        headers['X-Upload-Final'] = '1'
      }

      await postChunk(chunk, headers)

      onProgress?.(Math.min(100, Math.round((end / file.size) * 100)))
    }
    onProgress?.(100)
  }
}

/** One entry of a directory listing, as the server's file-manager listing returns it. */
type FileManagerItem = {
  /** Entry name, the segment on its own. */
  name: string
  /** Path relative to the file-manager root, which the client sends back verbatim. */
  path: string
  /** Whether the entry is a directory, following a symlink the server resolved. */
  is_dir: boolean
  /** Size in bytes, or null for a directory. */
  size: number | null
  /** Modification time as an ISO 8601 string. */
  mtime: string
}

/** How the pane lays a directory out, chosen by the view switcher. */
type FileManagerView = "details" | "list" | "cards"

/** Listings by directory path, `""` being the file-manager root. */
type Listings = Record<string, FileManagerItem[]>

function normalizePath(path: string): string {
  if (!path || path === "/") return ""
  return path.replace(/^\/+|\/+$/g, "")
}

function getParentPath(path: string): string {
  const normalized = normalizePath(path)
  if (!normalized) return ""
  const parts = normalized.split("/")
  return parts.slice(0, -1).join("/")
}

function getPathParts(path: string): string[] {
  const normalized = normalizePath(path)
  return normalized ? normalized.split("/") : []
}

function formatBytes(bytes: number | null): string {
  if (bytes === null) return "--"
  const units = ["B", "KB", "MB", "GB", "TB"]
  let value = bytes
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  return `${value.toFixed(unit ? 1 : 0)} ${units[unit]}`
}

function formatDate(iso: string): string {
  try {
    return new Date(iso).toLocaleString()
  } catch {
    return "--"
  }
}

interface FileManagerProps {
  /** Whether the session may change the listing (upload, mkdir). */
  canUpload?: boolean
  /** Whether files may be fetched. */
  allowDownload?: boolean
  className?: string
  /** Translator for the row labels, dialog text and toasts. */
  t?: (key: string, vars?: Record<string, unknown>) => string
}

/**
 * The file manager over the JSON file API: one directory at a time, in whichever
 * of the three layouts the user picked, with the upload and mkdir actions in the
 * header and a download on every row. The breadcrumb and the back button move
 * between directories; `cwd` is the one on screen and the one uploads and new
 * folders land in, and its listing is fetched the first time it is opened.
 *
 * A row is one truncating line with its controls beside it, so a phone's width
 * stays usable: the details table drops its size column below `sm` and its
 * modification time below `lg`, and the row's actions become icon-only buttons
 * below `sm`. Nothing here needs a fixed-width grid track, which is what used to
 * make the columns collide.
 *
 * Those columns are hidden with `max-sm:hidden` and `max-lg:hidden` rather than
 * `hidden sm:block`. Both cores inject a stylesheet of their own that carries an
 * unscoped `.hidden { display: none !important }`, which outranks every Tailwind
 * variant that would reveal the column; a `max-*` variant never puts the `hidden`
 * class on the element, so nothing here depends on that rule being scoped.
 *
 * @param props The permissions, translator and class name.
 */
export function FileManager({
  canUpload = true,
  allowDownload = true,
  className,
  t = (key) => key,
}: FileManagerProps) {
  const [listings, setListings] = React.useState<Listings>({ "": [] })
  const [loading, setLoading] = React.useState(true)
  const [cwd, setCwd] = React.useState("")
  const [view, setView] = React.useState<FileManagerView>("details")
  const [showHidden, setShowHidden] = React.useState(false)
  const [error, setError] = React.useState("")

  // Dialogs
  const [uploadOpen, setUploadOpen] = React.useState(false)
  const [newFolderOpen, setNewFolderOpen] = React.useState(false)

  // Form states
  const [newFolderName, setNewFolderName] = React.useState("")
  const [uploadFiles, setUploadFiles] = React.useState<File[]>([])
  const [uploading, setUploading] = React.useState(false)
  const fileInputRef = React.useRef<HTMLInputElement>(null)

  const apiBase = React.useMemo(() => new URL("api/file-manager", window.location.href).href, [])

  const browse = React.useCallback(
    async (dir: string): Promise<FileManagerItem[]> => {
      const url = new URL(`${apiBase}/browse`)
      if (dir) url.searchParams.set("path", dir)
      const resp = await fetch(url.href, {
        credentials: "same-origin",
        headers: sessionAuthHeaders(),
      })
      const data = await resp.json().catch(() => null)
      if (!resp.ok || !data || data.status !== "ok") {
        throw new Error((data && data.message) || `HTTP ${resp.status}`)
      }
      return (data.entries || []) as FileManagerItem[]
    },
    [apiBase]
  )

  const load = React.useCallback(
    async (dir: string) => {
      const entries = await browse(dir)
      setListings((prev) => ({ ...prev, [dir]: entries }))
    },
    [browse]
  )

  React.useEffect(() => {
    let live = true
    browse("").then(
      (entries) => {
        if (!live) return
        setListings((prev) => ({ ...prev, "": entries }))
        setLoading(false)
      },
      (e: any) => {
        if (!live) return
        setError(e?.message || String(e))
        setLoading(false)
      }
    )
    return () => {
      live = false
    }
  }, [browse])

  /** Opens a directory's listing the first time it is asked for. */
  const ensureLoaded = React.useCallback(
    (dir: string) => {
      if (dir in listings) return
      load(dir).catch((e: any) => setError(e?.message || String(e)))
    },
    [listings, load]
  )

  /** Re-reads the root and the directory the pane is showing. */
  const refresh = React.useCallback(async () => {
    setError("")
    await Promise.all(["", cwd].map((dir) => load(dir)))
  }, [cwd, load])

  /** Shows the directory at `path`, fetching its listing the first time. */
  const navigateTo = React.useCallback(
    (path: string) => {
      setCwd(path)
      ensureLoaded(path)
    },
    [ensureLoaded]
  )

  // A download is a navigation rather than a fetch, so it cannot carry a
  // header; the query token is what reaches the route in that case. A folder
  // has no single file to fetch, so it goes to the archive route instead.
  const fileUrl = React.useCallback(
    (entryPath: string) => {
      const segments = entryPath.split("/").map((seg) => encodeURIComponent(seg))
      return withSessionToken(
        new URL(["api/files", ...segments].join("/"), window.location.href).href,
      )
    },
    []
  )

  const archiveUrl = React.useCallback(
    (entryPath: string) => {
      const url = new URL(`${apiBase}/zip`, window.location.href)
      url.searchParams.set("path", entryPath)
      return withSessionToken(url.href)
    },
    [apiBase]
  )

  async function post(action: string, body: unknown) {
    const resp = await fetch(`${apiBase}/${action}`, {
      method: "POST",
      credentials: "same-origin",
      headers: sessionAuthHeaders({ "Content-Type": "application/json" }),
      body: JSON.stringify(body),
    })
    const data = await resp.json().catch(() => null)
    if (!resp.ok || !data || data.status !== "ok") {
      throw new Error((data && data.message) || `HTTP ${resp.status}`)
    }
    return data
  }

  // A file's name is not its download: the row's Download button is, so a
  // click on a file does nothing rather than saving it, and a click on a
  // folder is what opens it.
  const handleOpenItem = React.useCallback(
    (item: FileManagerItem) => {
      if (item.is_dir) navigateTo(item.path)
    },
    [navigateTo]
  )

  const handleDownload = React.useCallback(
    (item: FileManagerItem) => {
      window.open(item.is_dir ? archiveUrl(item.path) : fileUrl(item.path), "_self")
    },
    [archiveUrl, fileUrl]
  )

  const handleCreateFolder = async () => {
    const name = newFolderName.trim()
    if (!name) return

    const target = cwd ? `${cwd}/${name}` : name
    try {
      await post("mkdir", { path: target })
      setNewFolderOpen(false)
      setNewFolderName("")
      await refresh()
      toast.success(t("files.folderCreated", { name }))
    } catch (e: any) {
      toast.error(t("files.folderCreateFailed", { error: e?.message || String(e) }))
    }
  }

  const handleUpload = async () => {
    if (uploadFiles.length === 0 || uploading) return

    setUploading(true)
    let successCount = 0

    for (const file of uploadFiles) {
      const dest = cwd ? `${cwd}/${file.name}` : file.name
      try {
        await uploadFile(file, dest)
        successCount++
      } catch (e: any) {
        toast.error(t("files.uploadFailed", { file: file.name, error: e?.message || String(e) }))
      }
    }

    if (successCount > 0) {
      toast.success(
        t(successCount === 1 ? "files.uploadedCountOne" : "files.uploadedCount", {
          count: successCount,
        }),
      )
    }

    setUploading(false)
    setUploadOpen(false)
    setUploadFiles([])
    await refresh()
  }

  /**
   * A directory's entries with the hidden ones dropped, unless the header's
   * toggle has asked for them. The leading dot is the filesystem's own name for
   * an entry nobody meant to see, so the manager hides them by default and says
   * so on the toggle; a session rooted at a home directory would otherwise open
   * on its dotfiles rather than its documents.
   */
  const visible = React.useCallback(
    (items: FileManagerItem[] | undefined): FileManagerItem[] =>
      (items || []).filter((item) => showHidden || !item.name.startsWith(".")),
    [showHidden]
  )

  const pathParts = getPathParts(cwd)
  const entries = visible(listings[cwd])

  return (
    <div className={cn("flex h-full flex-col overflow-hidden rounded-md border bg-card", className)}>
      {/* Header */}
      <div className="shrink-0 border-b p-3">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="flex min-w-0 items-center gap-2">
            <Button
              type="button"
              variant="ghost"
              size="icon"
              disabled={!cwd}
              onClick={() => navigateTo(getParentPath(cwd))}
            >
              <ChevronRight className="h-4 w-4 rotate-180" />
              <span className="sr-only">{t("files.goBack")}</span>
            </Button>
            <nav aria-label={t("files.rootLabel")} className="flex min-w-0 flex-wrap items-center gap-1 text-sm font-medium">
              <button
                type="button"
                className="shrink-0 rounded px-1 py-0.5 hover:bg-muted"
                onClick={() => navigateTo("")}
              >
                {t("files.rootLabel")}
              </button>
              {pathParts.map((part, index) => (
                <React.Fragment key={`${part}-${index}`}>
                  <span className="text-muted-foreground">/</span>
                  <button
                    type="button"
                    className="min-w-0 truncate rounded px-1 py-0.5 hover:bg-muted"
                    onClick={() => navigateTo(pathParts.slice(0, index + 1).join("/"))}
                  >
                    {part}
                  </button>
                </React.Fragment>
              ))}
            </nav>
          </div>
          <div className="flex items-center gap-2">
            {canUpload && (
              <>
                {/* Icon-only below sm, where the label would cost more width
                    than the row has; aria-label keeps the name either way. */}
                <Button
                  size="sm"
                  variant="outline"
                  aria-label={t("files.newFolder")}
                  onClick={() => setNewFolderOpen(true)}
                >
                  <FolderPlus className="h-4 w-4" />
                  <span className="max-sm:hidden">{t("files.newFolder")}</span>
                </Button>
                <Button
                  size="sm"
                  aria-label={t("files.upload")}
                  onClick={() => setUploadOpen(true)}
                >
                  <UploadCloud className="h-4 w-4" />
                  <span className="max-sm:hidden">{t("files.upload")}</span>
                </Button>
              </>
            )}
          </div>
        </div>
        {/* The count and the two view controls share the second row, so the
            first row keeps only the path and the actions that change it and a
            narrow viewport has one row of each to wrap. */}
        <div className="mt-2 flex items-center justify-between gap-3">
          <span className="pl-10 text-xs text-muted-foreground">
            {t(entries.length === 1 ? "files.itemCountOne" : "files.itemCount", {
              count: entries.length,
            })}
          </span>
          <div className="flex items-center gap-1">
            <Button
              type="button"
              variant="ghost"
              size="icon-sm"
              aria-pressed={showHidden}
              onClick={() => setShowHidden((shown) => !shown)}
            >
              {showHidden ? <Eye className="h-4 w-4" /> : <EyeOff className="h-4 w-4" />}
              <span className="sr-only">
                {showHidden ? t("files.hideHidden") : t("files.showHidden")}
              </span>
            </Button>
            <div className="flex rounded-md border bg-background p-0.5">
              {(
                [
                  { value: "details" as const, icon: Rows3, label: t("files.viewDetails") },
                  { value: "list" as const, icon: List, label: t("files.viewList") },
                  { value: "cards" as const, icon: Grid2X2, label: t("files.viewCards") },
                ]
              ).map((item) => {
                const Icon = item.icon
                return (
                  <Button
                    key={item.value}
                    variant={view === item.value ? "secondary" : "ghost"}
                    size="icon-sm"
                    onClick={() => setView(item.value)}
                  >
                    <Icon className="h-4 w-4" />
                    <span className="sr-only">{item.label}</span>
                  </Button>
                )
              })}
            </div>
          </div>
        </div>
      </div>

      {/* Error Display */}
      {error && (
        <div className="shrink-0 border-b bg-destructive/10 px-4 py-2 text-sm text-destructive">
          {error}
        </div>
      )}

      <ScrollArea className="min-h-0 flex-1">
        {/* The column is what centres the loading and empty states when the
            listing is short, and what grows with it when it is not. */}
        <div className="flex min-h-full flex-col p-3">
        {loading ? (
          <div className="flex flex-1 items-center justify-center text-muted-foreground">
            {t("files.loading")}
          </div>
        ) : entries.length === 0 ? (
          <div className="flex flex-1 items-center justify-center rounded-md border border-dashed px-4 py-10 text-center text-sm text-muted-foreground">
            {t("files.emptyFolder")}
          </div>
        ) : view === "details" ? (
          <div className="overflow-hidden rounded-md border">
            <div className="grid grid-cols-[minmax(0,1fr)_2rem] gap-3 border-b bg-muted/30 px-3 py-2 text-xs font-medium text-muted-foreground sm:grid-cols-[minmax(0,1fr)_5rem_2rem] lg:grid-cols-[minmax(0,1fr)_5rem_10rem_2rem]">
              <div>{t("files.name")}</div>
              <div className="max-sm:hidden">{t("files.size")}</div>
              <div className="max-lg:hidden">{t("files.modified")}</div>
              <div />
            </div>
            {entries.map((file) => (
              <FileRow
                key={file.path}
                file={file}
                view="details"
                allowDownload={allowDownload}
                t={t}
                onOpen={handleOpenItem}
                onDownload={handleDownload}
              />
            ))}
          </div>
        ) : (
          <div className={cn("grid gap-2", view === "cards" && "sm:grid-cols-2 lg:grid-cols-3")}>
            {entries.map((file) => (
              <FileRow
                key={file.path}
                file={file}
                view={view}
                allowDownload={allowDownload}
                t={t}
                onOpen={handleOpenItem}
                onDownload={handleDownload}
              />
            ))}
          </div>
        )}
        </div>
      </ScrollArea>

      {/* Upload Dialog */}
      <Dialog open={uploadOpen} onOpenChange={setUploadOpen}>
        <DialogContent className="allow-native-input sm:max-w-md">
          <DialogHeader>
            <DialogTitle>{t("files.uploadTitle")}</DialogTitle>
            <DialogDescription>
              {t("files.uploadDescription", { path: cwd || t("files.rootLabel") })}
            </DialogDescription>
          </DialogHeader>
          <div className="flex flex-col gap-4">
            <div
              className={cn(
                "rounded-md border-2 border-dashed p-8 text-center transition-colors",
                uploadFiles.length > 0 ? "border-primary bg-primary/5" : "border-muted"
              )}
            >
              <input
                ref={fileInputRef}
                type="file"
                multiple
                className="hidden"
                onChange={(e) => {
                  if (e.target.files) {
                    setUploadFiles(Array.from(e.target.files))
                  }
                }}
              />
              <Button
                type="button"
                variant="outline"
                onClick={() => fileInputRef.current?.click()}
              >
                <UploadCloud className="h-4 w-4" />
                {t("files.chooseFiles")}
              </Button>
              <p className="mt-2 text-sm text-muted-foreground">
                {uploadFiles.length > 0
                  ? t("files.filesSelected", { count: uploadFiles.length })
                  : t("files.selectFiles")}
              </p>
            </div>
            {uploadFiles.length > 0 && (
              <div className="max-h-40 flex flex-col gap-1 overflow-auto">
                {uploadFiles.map((file, i) => (
                  <div key={i} className="flex items-center justify-between gap-2 rounded-md bg-muted px-3 py-2 text-sm">
                    <span className="truncate">{file.name}</span>
                    <span className="shrink-0 text-xs text-muted-foreground">{formatBytes(file.size)}</span>
                  </div>
                ))}
              </div>
            )}
          </div>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setUploadOpen(false)}>
              {t("files.cancel")}
            </Button>
            <Button
              type="button"
              onClick={handleUpload}
              disabled={uploadFiles.length === 0 || uploading}
            >
              {uploading ? t("files.uploading") : t("files.upload")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* New Folder Dialog */}
      <Dialog open={newFolderOpen} onOpenChange={setNewFolderOpen}>
        <DialogContent className="allow-native-input sm:max-w-md">
          <DialogHeader>
            <DialogTitle>{t("files.newFolderTitle")}</DialogTitle>
            <DialogDescription>
              {t("files.newFolderDescription", { path: cwd || t("files.rootLabel") })}
            </DialogDescription>
          </DialogHeader>
          <div className="flex flex-col gap-2">
            <Label htmlFor="folder-name">{t("files.folderName")}</Label>
            <Input
              id="folder-name"
              autoFocus
              value={newFolderName}
              onChange={(e) => setNewFolderName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault()
                  handleCreateFolder()
                }
              }}
              placeholder={t("files.folderNamePlaceholder")}
            />
          </div>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setNewFolderOpen(false)}>
              {t("files.cancel")}
            </Button>
            <Button
              type="button"
              onClick={handleCreateFolder}
              disabled={!newFolderName.trim()}
            >
              {t("files.create")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

    </div>
  )
}
interface FileRowProps {
  /** The entry the row shows. */
  file: FileManagerItem
  /** Which layout the row is drawn in. */
  view: FileManagerView
  /** Whether files may be fetched. */
  allowDownload: boolean
  /** Translator for the row's labels. */
  t: (key: string, vars?: Record<string, unknown>) => string
  /** Called with the entry: a folder opens it; a file's name is not its download. */
  onOpen: (item: FileManagerItem) => void
  /** Called with the entry to download it: a file as itself, a folder as an archive. */
  onDownload: (item: FileManagerItem) => void
}

/**
 * One entry of the pane's directory, in the details table or as a card.
 *
 * The details table's columns are per-breakpoint rather than fixed: the name
 * truncates and the size appears from `sm` and the modification time from `lg`,
 * so the row never needs more width than the viewport has. Below `sm` it is the
 * name and the download button, which is all a phone can usefully show.
 *
 * The trailing column is pinned to the download button's width, and the header
 * repeats the same template with an empty cell there: left `auto`, the header's
 * empty cell would collapse the track and every label would sit a button's width
 * right of the values under it.
 *
 * @param props The entry, the layout, whether files may be fetched, and the
 *     two things the row can be asked to do.
 */
function FileRow({ file, view, allowDownload, t, onOpen, onDownload }: FileRowProps) {
  const Icon = file.is_dir ? Folder : FileIcon

  // Every row carries the button: a file's downloads that file, a folder's
  // downloads the folder as an archive. The row's own click is the navigation,
  // so the button is what keeps the download from depending on knowing a row
  // is clickable.
  const download = allowDownload ? (
    <Button
      variant="ghost"
      size="icon-sm"
      aria-label={t("files.download")}
      title={t("files.download")}
      className="relative"
      onClick={() => onDownload(file)}
    >
      <Download className="h-4 w-4" />
    </Button>
  ) : (
    <div />
  )

  if (view === "details") {
    return (
      <div className="relative grid grid-cols-[minmax(0,1fr)_2rem] items-center gap-3 border-b px-3 py-2 last:border-b-0 hover:bg-muted/50 sm:grid-cols-[minmax(0,1fr)_5rem_2rem] lg:grid-cols-[minmax(0,1fr)_5rem_10rem_2rem]">
        {/* The overlay is what makes the whole row the target: the name button is
            the only focusable control, but it reaches past its own column to the
            row's edges, so the size and modification columns are not dead. The
            download button sits above it and keeps its own clicks. */}
        <button
          type="button"
          onClick={() => onOpen(file)}
          className="flex min-w-0 items-center gap-2 text-left after:absolute after:inset-0 after:content-['']"
        >
          <Icon className="h-4 w-4 shrink-0 text-muted-foreground" />
          <span className="truncate text-sm font-medium">{file.name}</span>
        </button>
        <div className="max-sm:hidden text-xs text-muted-foreground">{formatBytes(file.size)}</div>
        <div className="max-lg:hidden text-xs text-muted-foreground">{formatDate(file.mtime)}</div>
        {download}
      </div>
    )
  }

  return (
    <div className="flex items-center gap-3 rounded-md border bg-background p-3 hover:bg-muted/50">
      <button
        type="button"
        onClick={() => onOpen(file)}
        className="flex min-w-0 flex-1 items-center gap-3 text-left"
      >
        <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md border bg-muted/40">
          <Icon className="h-4 w-4 text-muted-foreground" />
        </div>
        <div className="min-w-0">
          <div className="truncate text-sm font-medium">{file.name}</div>
          <div className="mt-1 text-xs text-muted-foreground">
            {file.is_dir ? t("files.folderLabel") : formatBytes(file.size)} · {formatDate(file.mtime)}
          </div>
        </div>
      </button>
      {download}
    </div>
  )
}





