/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { useState, useEffect } from "react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from "@/components/ui/card";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { ScrollArea } from "@/components/ui/scroll-area";
import { cn } from "@/lib/utils";
import { Check, ChevronLeft, CircleAlert, Download, Info, Loader2, Play, RotateCw, Search, SearchX, Trash2, X } from "lucide-react";
import * as yaml from "js-yaml";
import { t } from "@/i18n";
import { getLastServerSettings } from "@/utils";
import { withSessionToken } from "../../../../selkies-web-core/lib/session-token.js";
import {
    APP_COMMAND_STATE_EVENT,
    INSTALLED_APPS_ROLLBACK_EVENT,
    INSTALLED_APPS_SERVER_EVENT,
    applyServerInstalledApps,
    appsCatalog,
    pendingAppAction,
    postAppCommand,
    readInstalledApps,
    writeInstalledApps,
} from "../../../../selkies-web-core/lib/app-commands.js";

/**
 * The apps modal: the proot-apps catalog, with install, remove, update, and
 * launch actions.
 *
 * Actions go through the apps command contract both dashboards share
 * (`selkies-web-core/lib/app-commands.js`): it posts the selkies-proot wrapper
 * commands to the core and tracks them for rollback, and its `appsCatalog`
 * says where the catalog is read from (the remote repository's metadata, or
 * this server's `api/apps/` for a local repository). Which apps are installed
 * comes from the server (`apps_installed` in the settings payload, and an
 * `appsInstalled` message when a command changes it), with localStorage holding
 * the last answer. Commands run server-side only while the `command_enabled`
 * server setting is on; without it the core suppresses every `cmd,` send, so
 * the modal says why instead of pretending the install happened. Listens for
 * `serverSettings` and `appsInstalled` messages and the rollback event the
 * contract dispatches on a failed command.
 * @module
 */

const METADATA_FETCH_TIMEOUT_MS = 10000;

/** One catalog entry of the proot-apps metadata. */
interface App {
    /** Package name, the argument of every command. */
    name: string;
    /** Display name. */
    full_name: string;
    description: string;
    /** Icon file name under the catalog's image directory. */
    icon: string;
    /** Listed but not installable. */
    disabled?: boolean;
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
 * Renders the catalog grid, its search box, and the per-app detail view
 * inside a dialog controlled by the parent.
 *
 * The popup holds no fixed width: it spans the viewport on small screens and
 * tracks 90% of it up to 72rem on larger ones, and the grid reflows between
 * two and six columns to the width it is given. Its height is the viewport
 * minus 2rem at each edge rather than content-capped: the scroll region
 * sizes itself as a percentage of the popup, and a browser resolves that
 * percentage only against a definite height, so a content-capped popup
 * leaves the region the height of its content with nothing to scroll. The
 * header stays outside the scroll region.
 *
 * The catalog is fetched once per modal open, plus explicit Retry presses; a
 * failure settles into the error view rather than refetching. The fetch is
 * aborted after a timeout and on close or unmount.
 */
export function Apps({ isOpen = false, onClose }: AppsProps = {}) {
    const [serverSettings, setServerSettings] = useState<any>(() => getLastServerSettings());
    const catalog = appsCatalog(serverSettings, withSessionToken);
    const [appData, setAppData] = useState<{ include: App[] } | null>(
        () => (cachedCatalogUrl === catalog.metadata ? cachedAppData : null));
    const [isLoading, setIsLoading] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [fetchAttempt, setFetchAttempt] = useState(0);
    const [searchTerm, setSearchTerm] = useState('');
    const [selectedApp, setSelectedApp] = useState<App | null>(null);
    const [installedApps, setInstalledApps] = useState<string[]>(readInstalledApps);
    const [commandTick, setCommandTick] = useState(0);

    useEffect(() => {
        writeInstalledApps(installedApps);
    }, [installedApps]);

    // The running set lives in app-commands.js; this only re-reads it.
    useEffect(() => {
        const onCommandState = () => setCommandTick((tick) => tick + 1);
        window.addEventListener(APP_COMMAND_STATE_EVENT, onCommandState);
        return () => window.removeEventListener(APP_COMMAND_STATE_EVENT, onCommandState);
    }, []);

    // A failed command already rolled the stored list back; mirror it here so
    // the badge flips without a remount.
    useEffect(() => {
        const onRollback = (event: Event) => {
            const { app, action } = (event as CustomEvent).detail || {};
            if (action === 'install')
                setInstalledApps(prev => prev.filter(name => name !== app));
            else if (action === 'remove')
                setInstalledApps(prev => prev.includes(app) ? prev : [...prev, app]);
        };
        window.addEventListener(INSTALLED_APPS_ROLLBACK_EVENT, onRollback);
        return () => window.removeEventListener(INSTALLED_APPS_ROLLBACK_EVENT, onRollback);
    }, []);

    useEffect(() => {
        const handleWindowMessage = (event: MessageEvent) => {
            if (event.origin !== window.location.origin) return;
            const message = event.data;
            if (typeof message !== 'object' || message === null) return;
            if (message.type === 'serverSettings') setServerSettings(message.payload);
        };
        window.addEventListener('message', handleWindowMessage);
        return () => window.removeEventListener('message', handleWindowMessage);
    }, []);
    const commandsKnown = serverSettings != null;
    const commandsAvailable = serverSettings?.command_enabled?.value === true;

    // The runner's answer replaces what this browser remembered, which a private
    // window or cleared site data leaves empty while the session has apps.
    const installedFromServer = serverSettings?.apps_installed?.value;
    useEffect(() => {
        const adopt = (apps: string[] | undefined) => {
            if (applyServerInstalledApps(apps)) setInstalledApps(readInstalledApps());
        };
        adopt(installedFromServer);
        const onServerList = (event: Event) =>
            setInstalledApps((event as CustomEvent).detail?.apps || []);
        const onWindowMessage = (event: MessageEvent) => {
            if (event.origin !== window.location.origin) return;
            const message = event.data;
            if (typeof message !== 'object' || message === null) return;
            if (message.type === 'appsInstalled') adopt(message.apps);
        };
        window.addEventListener(INSTALLED_APPS_SERVER_EVENT, onServerList);
        window.addEventListener('message', onWindowMessage);
        return () => {
            window.removeEventListener(INSTALLED_APPS_SERVER_EVENT, onServerList);
            window.removeEventListener('message', onWindowMessage);
        };
    }, [installedFromServer]);

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

    const handleSearchChange = (event: React.ChangeEvent<HTMLInputElement>) => {
        setSearchTerm(event.target.value);
    };

    const handleAppClick = (app: App) => {
        setSelectedApp(app);
    };

    const handleBackToGrid = () => {
        setSelectedApp(null);
    };

    const handleInstall = (appName: string) => {
        if (!commandsAvailable) return;
        postAppCommand('install', appName);
        setInstalledApps(prev => prev.includes(appName) ? prev : [...prev, appName]);
    };

    const handleRemove = (appName: string) => {
        if (!commandsAvailable) return;
        postAppCommand('remove', appName);
        setInstalledApps(prev => prev.filter(name => name !== appName));
    };

    const handleUpdate = (appName: string) => {
        if (!commandsAvailable) return;
        postAppCommand('update', appName);
    };

    const handleLaunch = (appName: string) => {
        if (!commandsAvailable) return;
        postAppCommand('launch', appName);
    };

    const needle = searchTerm.toLowerCase();
    const filteredApps = appData?.include?.filter(app =>
        !app.disabled &&
        (app.full_name?.toLowerCase().includes(needle) ||
         app.name?.toLowerCase().includes(needle) ||
         app.description?.toLowerCase().includes(needle))
    ) || [];

    const isAppInstalled = (appName: string) => installedApps.includes(appName);

    return (
        <Dialog open={isOpen} onOpenChange={handleModalClose}>
            <DialogContent
                showCloseButton={false}
                className="flex! flex-col gap-0! h-[calc(100dvh-4rem)]! p-0! overflow-hidden sm:max-w-[min(90vw,72rem)]!"
            >
                <DialogHeader className="gap-4 border-b p-5 sm:flex-row sm:items-center sm:justify-between">
                    <div className="min-w-0">
                        <DialogTitle>{t('sections.apps.title')}</DialogTitle>
                        <DialogDescription>{t('apps.subtitle')}</DialogDescription>
                    </div>
                    <div className="flex shrink-0 items-center gap-2">
                        <div className="relative">
                            <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
                            <Input
                                type="text"
                                placeholder={t('appsModal.searchPlaceholder')}
                                value={searchTerm}
                                onChange={handleSearchChange}
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

                <ScrollArea className="min-h-0 flex-1">
                    <div className="p-5">
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
                                <div className="space-y-4">
                                    <Button variant="ghost" size="sm" onClick={handleBackToGrid} className="-ml-2">
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
                                        <CardFooter className="justify-end gap-2 border-t">
                                            {(() => {
                                                const running = commandTick >= 0 && pendingAppAction(selectedApp.name);
                                                const held = !commandsAvailable || !!running;
                                                const spin = (action: string) =>
                                                    running === action ? <Loader2 className="animate-spin" /> : null;
                                                return isAppInstalled(selectedApp.name) ? (
                                                    <>
                                                        <Button
                                                            variant="default"
                                                            onClick={() => handleLaunch(selectedApp.name)}
                                                            disabled={held}
                                                        >
                                                            {spin('launch')}
                                                            <Play />
                                                            {t('apps.launchApp', { name: selectedApp.name })}
                                                        </Button>
                                                        <Button
                                                            variant="outline"
                                                            onClick={() => handleUpdate(selectedApp.name)}
                                                            disabled={held}
                                                        >
                                                            {spin('update')}
                                                            <RotateCw />
                                                            {t('apps.updateApp', { name: selectedApp.name })}
                                                        </Button>
                                                        <Button
                                                            variant="destructive"
                                                            onClick={() => handleRemove(selectedApp.name)}
                                                            disabled={held}
                                                        >
                                                            {spin('remove')}
                                                            <Trash2 />
                                                            {t('apps.removeApp', { name: selectedApp.name })}
                                                        </Button>
                                                    </>
                                                ) : (
                                                    <Button
                                                        variant="default"
                                                        onClick={() => handleInstall(selectedApp.name)}
                                                        disabled={held}
                                                    >
                                                        {spin('install')}
                                                        <Download />
                                                        {t('apps.installApp', { name: selectedApp.name })}
                                                    </Button>
                                                );
                                            })()}
                                        </CardFooter>
                                    </Card>
                                </div>
                            ) : (
                                <div className="grid grid-cols-3 gap-3 sm:grid-cols-4 md:grid-cols-5 lg:grid-cols-6 xl:grid-cols-8">
                                    {filteredApps.length > 0 ? (
                                        filteredApps.map(app => (
                                            <Card
                                                key={app.name}
                                                size="sm"
                                                className="group/tile relative cursor-pointer transition-colors hover:bg-accent/50 hover:ring-primary/30"
                                                onClick={() => handleAppClick(app)}
                                            >
                                                {isAppInstalled(app.name) && (
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
                                            </Card>
                                        ))
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
                </ScrollArea>
            </DialogContent>
        </Dialog>
    );
}
