/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { useState, useEffect, useMemo, useRef, useSyncExternalStore } from "react";
import { Button } from "@/components/ui/button";
import { Clip } from "@/panels/controls";
import { Card, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from "@/components/ui/card";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";
import {
    Ban, Check, ChevronLeft, CircleAlert, Clapperboard, Clock, Code, Cpu, Download, FileText, FlaskConical,
    Gamepad2, Globe, GraduationCap, Info, LayoutGrid, Loader2, Network, PackageCheck, Palette, Play, RotateCw,
    Search, SearchX, Shapes, Sparkles, Trash2, Wrench, X,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";
import * as yaml from "js-yaml";
import { t } from "@/i18n";
import { getLastServerSettings, onCoreMessage, type ServerSettings } from "@/utils";
import { withSessionToken } from "../../../selkies-web-core/lib/session-token.ts";
import {
    appsCatalog,
    appsSnapshot,
    jobFinished,
    jobForApp,
    sendAppsRequest,
    subscribeApps,
    type AppJob,
} from "../../../selkies-web-core/lib/app-commands.ts";

/**
 * The apps modal: the proot-apps catalog by category, with install, remove,
 * update, and launch actions and the console of every job the server runs.
 *
 * Requests go through the apps contract of the core
 * (`selkies-web-core/lib/app-commands.ts`), whose store holds what the server
 * reports: what is installed, read from the session's disk when the modal
 * opens and after every job, and the jobs with their consoles, shared by every
 * page of the session. Nothing about the session is kept in this browser
 * beyond the tab last chosen. Requests run server-side only while the
 * `command_enabled` server setting is on; without it the core drops them, so
 * the modal says why. Its catalog location is `appsCatalog`'s answer (the
 * remote repository's metadata, or this server's `api/apps/` for a local
 * repository). Listens for `serverSettings` messages.
 * @module
 */

const METADATA_FETCH_TIMEOUT_MS = 10000;
/** Where the tab last chosen is remembered, a convenience of this browser's. */
const TAB_STORAGE_KEY = "selkiesAppsTab";
/** The tab of every app. */
const ALL_TAB = "__all";
/** The width the catalog grid is given, and the narrower one a single app's view takes. */
const GRID_WIDTH = "sm:w-[min(90vw,72rem)]!";
const DETAIL_WIDTH = "sm:w-[min(90vw,38rem)]!";
/** The margin a single app's view keeps from the viewport's top and bottom edges together. */
const DETAIL_MARGIN_PX = 64;
/**
 * The grid's height: four fifths of the viewport less its margins, but never less than the
 * whole of that or 36rem, whichever is smaller, so a short screen keeps every row it has.
 */
const GRID_HEIGHT = "max(calc((100dvh - 4rem) * 0.8), min(calc(100dvh - 4rem), 36rem))";
/** The categories a first visit opens on, the first the catalog has. */
const DEFAULT_CATEGORIES = ["WebBrowser", "Network"];

/** One catalog entry of the proot-apps metadata. */
interface App {
    /** Package name, the argument of every request. */
    name: string;
    /** Display name. */
    full_name: string;
    description: string;
    /** Icon file name under the catalog's image directory. */
    icon: string;
    /** The one category the catalog files it under (`WebBrowser`, `Game`, `AudioVideo`), absent from a catalog that names none. */
    category?: string;
    /** Listed but not installable. */
    disabled?: boolean;
}

/** The icon of each category proot-apps files apps under; another gets a generic one. */
const CATEGORY_ICONS: Record<string, LucideIcon> = {
    WebBrowser: Globe,
    Network,
    Game: Gamepad2,
    Creative: Palette,
    AudioVideo: Clapperboard,
    Office: FileText,
    Development: Code,
    Utility: Wrench,
    AI: Sparkles,
    System: Cpu,
    Science: FlaskConical,
    Education: GraduationCap,
};

/**
 * The name a category is shown by: the catalog's own value, with a space
 * where a camel-cased word runs on (`AudioVideo` reads `Audio Video`). The
 * names come from the catalog, as an app's name and description do, so a
 * category it adds later lists without a change here.
 * @param category A catalog `category` value.
 * @returns The label for its tab.
 */
function categoryLabel(category: string): string {
    return category.replace(/([a-z])([A-Z])/g, '$1 $2');
}

/**
 * The tab remembered from an earlier visit, nothing where storage is refused.
 * @returns The tab's value, or null.
 */
function rememberedTab(): string | null {
    try {
        return localStorage.getItem(TAB_STORAGE_KEY);
    } catch {
        return null;
    }
}

/**
 * Remembers the tab chosen, where storage allows.
 * @param tab The tab's value.
 */
function rememberTab(tab: string): void {
    try {
        localStorage.setItem(TAB_STORAGE_KEY, tab);
    } catch {
        // A refused storage only forgets the tab.
    }
}

/**
 * The label a job is shown by: its operation on its app.
 * @param job The job.
 * @returns The label.
 */
function jobLabel(job: AppJob): string {
    const key = { install: 'apps.installApp', remove: 'apps.removeApp', update: 'apps.updateApp' }[job.op];
    return t(key, { name: job.app });
}

/**
 * Session cache of the fetched catalog, keyed by where it was read from: the
 * modal is conditionally mounted by its parent, so each open is a fresh mount;
 * a hit here skips the network.
 */
let cachedAppData: { include: App[] } | null = null;
let cachedCatalogUrl: string | null = null;

interface AppsProps {
    /** Whether the dialog is shown; the parent controls it. */
    isOpen?: boolean;
    /** Called when the dialog asks to close. */
    onClose?: () => void;
}

/**
 * The catalog icon, or the app's initial on a muted tile when the image does
 * not load, so an unreachable image host never leaves the tile blank.
 * @param props The catalog entry whose icon is shown, the icon's URL from
 *     the catalog the modal reads, and the classes sizing it.
 * @returns The icon image, or the fallback tile carrying the app's initial.
 */
function AppIcon({ app, src, className }: { app: App; src: string; className?: string }) {
    const [failed, setFailed] = useState(false);
    if (failed) {
        return (
            <div
                aria-hidden="true"
                className={cn("flex items-center justify-center rounded-lg bg-muted font-medium text-muted-foreground", className)}
            >
                {app.full_name.charAt(0).toUpperCase()}
            </div>
        );
    }
    return (
        <img
            src={src}
            alt=""
            loading="lazy"
            onError={() => setFailed(true)}
            className={className}
        />
    );
}

/**
 * The icon of a job's state: a spinner while it runs, a clock while it waits,
 * and how it ended after.
 * @param props The job.
 * @returns The icon.
 */
function JobStateIcon({ job }: { job: AppJob }) {
    switch (job.state) {
        case 'queued': return <Clock className="size-4 shrink-0 text-muted-foreground" />;
        case 'running': return <Loader2 className="size-4 shrink-0 animate-spin text-primary" />;
        case 'done': return <Check className="size-4 shrink-0 text-primary" />;
        case 'failed': return <CircleAlert className="size-4 shrink-0 text-destructive" />;
        default: return <Ban className="size-4 shrink-0 text-muted-foreground" />;
    }
}

/**
 * A job's header: its state, what it does, its download progress, and the
 * button that cancels it while pending or clears it once finished.
 * @param props The job.
 * @returns The header row.
 */
function JobHeader({ job }: { job: AppJob }) {
    const finished = jobFinished(job);
    return (
        <div className="flex min-w-0 items-center gap-2">
            <JobStateIcon job={job} />
            <div className="min-w-0 flex-1">
                <div className="flex min-w-0 items-center gap-2">
                    <Clip className="text-sm font-medium">{jobLabel(job)}</Clip>
                    {job.state === 'failed' && job.code !== null && (
                        <span className="shrink-0 font-mono text-[0.625rem] text-destructive">{job.code}</span>
                    )}
                </div>
                {job.progress !== null && !finished && (
                    <div className="mt-1 h-1 overflow-hidden rounded-full bg-muted">
                        <div className="h-full bg-primary transition-[width]" style={{ width: `${job.progress}%` }} />
                    </div>
                )}
            </div>
            <Button
                variant="ghost"
                size="icon-sm"
                className={cn("app-job-button shrink-0", finished ? "clear" : "cancel")}
                aria-label={finished ? t('common.close') : t('files.cancel')}
                onClick={(event) => {
                    event.stopPropagation();
                    sendAppsRequest(finished ? 'clear' : 'cancel', undefined, job.id);
                }}
            >
                {finished ? <X /> : <Ban />}
            </Button>
        </div>
    );
}

/**
 * A job's console: the header, then everything it printed, the line being
 * written last, kept scrolled to the end unless the reader scrolled up.
 * @param props The job.
 * @returns The console.
 */
function JobConsole({ job }: { job: AppJob }) {
    const scroller = useRef<HTMLPreElement>(null);
    const pinned = useRef(true);
    const text = job.current ? [...job.lines, job.current].join('\n') : job.lines.join('\n');
    useEffect(() => {
        const node = scroller.current;
        if (node && pinned.current) node.scrollTop = node.scrollHeight;
    }, [text]);
    return (
        <div className="app-job-console space-y-2 rounded-lg border bg-muted/30 p-3" data-state={job.state}>
            <JobHeader job={job} />
            {text && (
                <pre
                    ref={scroller}
                    onScroll={(event) => {
                        const node = event.currentTarget;
                        pinned.current = node.scrollHeight - node.scrollTop - node.clientHeight < 8;
                    }}
                    className="thin-scroll max-h-56 overflow-auto rounded-md bg-background/60 p-2 font-mono text-[0.6875rem] leading-relaxed whitespace-pre-wrap break-all text-muted-foreground"
                >
                    {text}
                </pre>
            )}
        </div>
    );
}

/**
 * Renders the catalog grid under a category picker, the search box and the
 * installed filter that both look through every category, and the per-app
 * view with its job's console, inside a dialog controlled by the parent. The
 * categories are the ones the catalog's entries carry (`category` in the
 * proot-apps metadata), plus one of every app, as rows of chips wrapped and
 * centered under the header; a first visit opens on web browsers where the
 * catalog files any, a later one on the category last chosen.
 *
 * The popup holds no fixed width: it spans the viewport on small screens and,
 * on larger ones, tracks 90% of it up to 72rem for the grid, which reflows
 * between three and eight columns, and up to 38rem for a single app. Its
 * height is always definite, because the scroll region sizes itself as a
 * percentage of the popup and a browser resolves that only against a definite
 * height: four fifths of the viewport less 2rem at each edge for the grid,
 * which the categories leave more than enough, but no shorter than that whole
 * or 36rem, so a short screen keeps its rows; and for a single app the height
 * its content measures, up to the viewport less 2rem at each edge. Both are set as
 * lengths rather than `auto`, so the popup glides between the two sizes, and
 * it stays centered while it does; a reader who asked for reduced motion gets
 * the change at once. The header and the category picker stay outside the
 * scroll region.
 *
 * The catalog is fetched once per modal open, plus explicit Retry presses; a
 * failure settles into the error view rather than refetching. The fetch is
 * aborted after a timeout and on close or unmount. Each open asks the server
 * what is installed.
 */
export function Apps({ isOpen = false, onClose }: AppsProps = {}) {
    const [serverSettings, setServerSettings] = useState(() => getLastServerSettings());
    const catalog = appsCatalog(serverSettings, withSessionToken);
    const [appData, setAppData] = useState<{ include: App[] } | null>(
        () => (cachedCatalogUrl === catalog.metadata ? cachedAppData : null));
    const [isLoading, setIsLoading] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [fetchAttempt, setFetchAttempt] = useState(0);
    const [searchTerm, setSearchTerm] = useState('');
    const [chosenTab, setChosenTab] = useState<string | null>(rememberedTab);
    const [installedOnly, setInstalledOnly] = useState(false);
    const [selectedApp, setSelectedApp] = useState<App | null>(null);
    const apps = useSyncExternalStore(subscribeApps, appsSnapshot);
    // A single app's view is as tall as its content: the header and the view
    // are measured, and the popup takes their sum as a length it can glide to.
    // While that sum fits the viewport the view never scrolls, so content that
    // grows ahead of the glide (a console appearing) shows no scrollbar.
    const headerRef = useRef<HTMLDivElement>(null);
    const bodyRef = useRef<HTMLDivElement>(null);
    const [detailHeight, setDetailHeight] = useState<number | null>(null);
    const detailFits = detailHeight !== null && detailHeight <= window.innerHeight - DETAIL_MARGIN_PX;
    useEffect(() => {
        if (!selectedApp) return;
        const header = headerRef.current;
        const body = bodyRef.current;
        if (!header || !body) return;
        const measure = () => setDetailHeight(header.offsetHeight + body.scrollHeight);
        const observer = new ResizeObserver(measure);
        observer.observe(header);
        observer.observe(body);
        measure();
        return () => observer.disconnect();
    }, [selectedApp]);

    useEffect(() => onCoreMessage((message) => {
        if (message.type === 'serverSettings') setServerSettings(message.payload as ServerSettings | null);
    }), []);
    const commandsKnown = serverSettings != null;
    const commandsAvailable = serverSettings?.command_enabled?.value === true;

    // What is installed is read from the session's disk each time it is shown.
    useEffect(() => {
        if (isOpen && commandsAvailable) sendAppsRequest('list');
    }, [isOpen, commandsAvailable]);

    const handleModalClose = (open: boolean) => {
        if (!open && onClose) {
            onClose();
        }
    };

    useEffect(() => {
        if (!isOpen || appData) return;
        const controller = new AbortController();
        const timeoutId = window.setTimeout(() => controller.abort(), METADATA_FETCH_TIMEOUT_MS);
        // Suppresses any setState landing after cleanup.
        let active = true;
        // Not derivable during render: this is the transition into a fetch,
        // and a Retry has to re-enter it.
        // eslint-disable-next-line react-hooks/set-state-in-effect
        setIsLoading(true);
        setError(null);
        (async () => {
            try {
                const response = await fetch(catalog.metadata, { signal: controller.signal });
                if (!response.ok) {
                    throw new Error(`HTTP error! status: ${response.status}`);
                }
                const yamlText = await response.text();
                const parsedData = yaml.load(yamlText) as { include: App[] };
                if (!active) return;
                cachedAppData = parsedData;
                cachedCatalogUrl = catalog.metadata;
                setAppData(parsedData);
            } catch (e) {
                if (!active) return;
                console.error("Failed to fetch or parse app data:", e);
                setError(t('appsModal.errorLoading'));
            } finally {
                clearTimeout(timeoutId);
                if (active) setIsLoading(false);
            }
        })();
        return () => {
            active = false;
            clearTimeout(timeoutId);
            controller.abort();
        };
    }, [isOpen, appData, fetchAttempt, catalog.metadata]);

    const installable = useMemo(() => (appData?.include ?? []).filter(app => !app.disabled), [appData]);
    const installed = useMemo(() => new Set(apps.installed ?? []), [apps.installed]);
    const isAppInstalled = (appName: string) => installed.has(appName);

    // The categories the installable apps are filed under, sorted by their
    // shown name.
    const categories = useMemo(() => {
        const names = new Set<string>();
        for (const app of installable) if (app.category) names.add(app.category);
        return [...names].sort((a, b) => categoryLabel(a).localeCompare(categoryLabel(b)));
    }, [installable]);
    const tabs = useMemo(() => {
        const list: { value: string; label: string; icon: LucideIcon }[] = categories.map(category => (
            { value: category, label: categoryLabel(category), icon: CATEGORY_ICONS[category] ?? Shapes }));
        list.push({ value: ALL_TAB, label: t('appsModal.allCategories'), icon: LayoutGrid });
        return list;
    }, [categories]);
    const installedCount = installable.filter(app => installed.has(app.name)).length;
    const tab = (chosenTab && tabs.some(each => each.value === chosenTab))
        ? chosenTab
        : DEFAULT_CATEGORIES.find(category => categories.includes(category)) ?? categories[0] ?? ALL_TAB;
    const chooseTab = (value: string) => {
        setChosenTab(value);
        rememberTab(value);
    };

    // The search and the installed filter look through every category.
    const needle = searchTerm.trim().toLowerCase();
    const acrossCategories = !!needle || installedOnly;
    const filteredApps = installable.filter(app =>
        (!installedOnly || installed.has(app.name)) &&
        (needle
            ? (app.full_name?.toLowerCase().includes(needle) ||
               app.name?.toLowerCase().includes(needle) ||
               app.description?.toLowerCase().includes(needle))
            : installedOnly || tab === ALL_TAB || app.category === tab)
    );

    const request = (op: 'install' | 'remove' | 'update' | 'launch', appName: string) => {
        if (commandsAvailable) sendAppsRequest(op, appName);
    };
    const selectedJob = selectedApp ? jobForApp(apps, selectedApp.name) : null;

    return (
        <Dialog open={isOpen} onOpenChange={handleModalClose}>
            <DialogContent
                showCloseButton={false}
                style={{
                    height: selectedApp && detailHeight !== null
                        ? `min(${detailHeight}px, calc(100dvh - ${DETAIL_MARGIN_PX}px))`
                        : GRID_HEIGHT,
                }}
                className={cn(
                    "allow-native-input flex! flex-col gap-0! p-0! overflow-hidden max-w-none! w-[calc(100vw-2rem)]!",
                    // Transition properties of their own, so the dialog's open and close animations keep their timing.
                    "transition-[width,height] [transition-duration:300ms] [transition-timing-function:cubic-bezier(0.2,0,0,1)] motion-reduce:transition-none",
                    selectedApp ? DETAIL_WIDTH : GRID_WIDTH,
                )}
            >
                <DialogHeader
                    ref={headerRef}
                    className={cn(
                        "gap-4 border-b p-5",
                        // Without the search, the title and the close button share a row at every width.
                        selectedApp ? "flex-row items-center justify-between" : "sm:flex-row sm:items-center sm:justify-between",
                    )}
                >
                    <div className="min-w-0">
                        <DialogTitle>{t('sections.apps.title')}</DialogTitle>
                        <DialogDescription>{t('apps.subtitle')}</DialogDescription>
                    </div>
                    <div className="flex shrink-0 items-center gap-2">
                        {!selectedApp && appData && installedCount > 0 && (
                            <Button
                                variant={installedOnly ? 'default' : 'outline'}
                                onClick={() => setInstalledOnly(on => !on)}
                                aria-pressed={installedOnly}
                                aria-label={t('appsModal.installedBadge')}
                                className="app-installed-filter shrink-0"
                            >
                                <PackageCheck />
                                <span className="tabular-nums">{installedCount}</span>
                            </Button>
                        )}
                        <div className={cn("relative min-w-0 flex-1", selectedApp && "hidden")}>
                            <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
                            <Input
                                type="text"
                                placeholder={t('appsModal.searchPlaceholder')}
                                value={searchTerm}
                                onChange={(event) => setSearchTerm(event.target.value)}
                                className="w-full pl-8 sm:w-56"
                            />
                        </div>
                        <Button
                            variant="secondary"
                            size="icon"
                            onClick={() => handleModalClose(false)}
                            aria-label={t('appsModal.closeAlt')}
                        >
                            <X />
                        </Button>
                    </div>
                </DialogHeader>

                {appData && !selectedApp && !acrossCategories && tabs.length > 1 && (
                    <div className="flex flex-wrap justify-center gap-1.5 border-b px-4 py-3 animate-in fade-in-0 duration-300">
                        {tabs.map(({ value, label, icon: Icon }) => (
                            <button
                                key={value}
                                type="button"
                                aria-pressed={tab === value}
                                data-tab={value}
                                onClick={() => chooseTab(value)}
                                className={cn(
                                    "app-tab inline-flex h-7 items-center gap-1.5 rounded-full border px-3 text-xs font-medium whitespace-nowrap transition-colors",
                                    "focus-visible:ring-[3px] focus-visible:ring-ring focus-visible:outline-none [&_svg]:size-3.5 [&_svg]:shrink-0",
                                    tab === value
                                        ? "border-primary bg-primary text-primary-foreground"
                                        : "border-border bg-muted/40 text-muted-foreground hover:bg-accent hover:text-foreground",
                                )}
                            >
                                <Icon />
                                {label}
                            </button>
                        ))}
                    </div>
                )}

                {/* A native scroller: the catalog runs to hundreds of tiles, and
                    each is laid out and painted only as it scrolls into view. */}
                <div className={cn("thin-scroll min-h-0 flex-1", selectedApp && detailFits ? "overflow-y-hidden" : "overflow-y-auto")}>
                    <div ref={bodyRef} className="p-5">
                        {commandsKnown && !commandsAvailable && (
                            <div className="mb-4 flex items-start gap-2.5 rounded-lg border bg-muted/40 p-3.5 text-muted-foreground">
                                <Info className="mt-0.5 size-4 shrink-0" />
                                <p>{t('appsModal.commandsDisabled')}</p>
                            </div>
                        )}
                        {isLoading && (
                            <div
                                role="status"
                                aria-label={t('appsModal.loading')}
                                className="grid grid-cols-3 gap-3 sm:grid-cols-4 md:grid-cols-5 lg:grid-cols-6 xl:grid-cols-8"
                            >
                                {Array.from({ length: 12 }, (_, i) => (
                                    <div key={i} className="h-28 animate-pulse rounded-lg bg-muted" />
                                ))}
                            </div>
                        )}
                        {error && (
                            <div className="flex flex-col items-center justify-center gap-4 py-16 text-center">
                                <span className="flex size-11 items-center justify-center rounded-full bg-destructive/10">
                                    <CircleAlert className="size-5 text-destructive" />
                                </span>
                                <p className="max-w-md text-muted-foreground">{error}</p>
                                <Button variant="outline" onClick={() => setFetchAttempt(n => n + 1)}>
                                    <RotateCw />
                                    {t('appsModal.retryButton')}
                                </Button>
                            </div>
                        )}
                        {!isLoading && !error && appData && (
                            selectedApp ? (
                                <div key="detail" className="space-y-4 animate-in fade-in-0 duration-300">
                                    <Button variant="ghost" size="sm" onClick={() => setSelectedApp(null)} className="-ml-2">
                                        <ChevronLeft />
                                        {t('appsModal.backButton')}
                                    </Button>
                                    <Card>
                                        <CardHeader>
                                            <div className="flex flex-col items-start gap-4 sm:flex-row sm:items-center">
                                                <AppIcon app={selectedApp} src={catalog.icon(selectedApp.icon)} className="size-16 shrink-0 object-contain" />
                                                <div className="min-w-0 space-y-1.5">
                                                    <div className="flex flex-wrap items-center gap-2">
                                                        <CardTitle className="text-base">{selectedApp.full_name}</CardTitle>
                                                        {isAppInstalled(selectedApp.name) && (
                                                            <span className="inline-flex items-center gap-1 rounded-full bg-primary/10 px-2 py-0.5 text-[0.625rem] font-medium text-primary">
                                                                <Check className="size-2.5" />
                                                                {t('appsModal.installedBadge')}
                                                            </span>
                                                        )}
                                                        <span className="rounded bg-muted px-1.5 py-0.5 font-mono text-[0.625rem] text-muted-foreground">
                                                            {selectedApp.name}
                                                        </span>
                                                    </div>
                                                    <CardDescription>{selectedApp.description}</CardDescription>
                                                </div>
                                            </div>
                                        </CardHeader>
                                        <CardFooter className="flex-wrap justify-end gap-2 border-t">
                                            {(() => {
                                                const name = selectedApp.name;
                                                const pending = selectedJob && !jobFinished(selectedJob) ? selectedJob.op : null;
                                                const held = !commandsAvailable || !!pending;
                                                const spin = (op: string) =>
                                                    pending === op ? <Loader2 className="animate-spin" /> : null;
                                                // A pending job keeps the buttons it was started from, the pressed one spinning.
                                                const shownInstalled = pending === 'install' ? false
                                                    : pending === 'remove' ? true : isAppInstalled(name);
                                                const button = (op: 'install' | 'remove' | 'update' | 'launch', label: string,
                                                    icon: React.ReactNode, variant: 'default' | 'outline' | 'destructive') => (
                                                    <Button
                                                        variant={variant}
                                                        onClick={() => request(op, name)}
                                                        disabled={held}
                                                        className={cn("app-action-button min-w-0", op, pending === op && "running")}
                                                        title={label}
                                                    >
                                                        {spin(op)}
                                                        {icon}
                                                        <Clip>{label}</Clip>
                                                    </Button>
                                                );
                                                return shownInstalled ? (
                                                    <>
                                                        {button('launch', t('apps.launchApp', { name }), <Play />, 'default')}
                                                        {button('update', t('apps.updateApp', { name }), <RotateCw />, 'outline')}
                                                        {button('remove', t('apps.removeApp', { name }), <Trash2 />, 'destructive')}
                                                    </>
                                                ) : button('install', t('apps.installApp', { name }), <Download />, 'default');
                                            })()}
                                        </CardFooter>
                                    </Card>
                                    {selectedJob && <JobConsole key={selectedJob.id} job={selectedJob} />}
                                </div>
                            ) : (
                                <div key="grid" className="grid grid-cols-3 gap-3 animate-in fade-in-0 duration-300 sm:grid-cols-4 md:grid-cols-5 lg:grid-cols-6 xl:grid-cols-8">
                                    {filteredApps.length > 0 ? (
                                        filteredApps.map(app => {
                                            const job = jobForApp(apps, app.name);
                                            const pending = job && !jobFinished(job) ? job : null;
                                            return (
                                                <Card
                                                    key={app.name}
                                                    size="sm"
                                                    className="group/tile relative cursor-pointer transition-colors hover:bg-accent/50 hover:ring-primary/30 [content-visibility:auto] [contain-intrinsic-size:auto_7rem]"
                                                    onClick={() => setSelectedApp(app)}
                                                >
                                                    {pending ? (
                                                        <span className="absolute top-1.5 right-1.5">
                                                            <JobStateIcon job={pending} />
                                                        </span>
                                                    ) : isAppInstalled(app.name) && (
                                                        <span className="absolute top-1.5 right-1.5 flex size-4 items-center justify-center rounded-full bg-primary text-primary-foreground">
                                                            <Check className="size-2.5" />
                                                            <span className="sr-only">{t('appsModal.installedBadge')}</span>
                                                        </span>
                                                    )}
                                                    <CardContent className="flex flex-col items-center gap-2">
                                                        <AppIcon app={app} src={catalog.icon(app.icon)} className="size-10 object-contain" />
                                                        <CardTitle className="line-clamp-2 text-center text-xs group-hover/tile:text-primary">
                                                            {app.full_name}
                                                        </CardTitle>
                                                    </CardContent>
                                                    {pending?.progress != null && (
                                                        <div className="absolute inset-x-2 bottom-1.5 h-0.5 overflow-hidden rounded-full bg-muted">
                                                            <div className="h-full bg-primary transition-[width]" style={{ width: `${pending.progress}%` }} />
                                                        </div>
                                                    )}
                                                </Card>
                                            );
                                        })
                                    ) : (
                                        <div className="col-span-full flex flex-col items-center justify-center gap-3 py-16 text-center">
                                            <SearchX className="size-8 text-muted-foreground/50" />
                                            <p className="text-muted-foreground">{t('appsModal.noAppsFound')}</p>
                                        </div>
                                    )}
                                </div>
                            )
                        )}
                    </div>
                </div>

            </DialogContent>
        </Dialog>
    );
}
