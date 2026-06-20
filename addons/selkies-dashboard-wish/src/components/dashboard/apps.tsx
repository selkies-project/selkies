/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { useState, useEffect } from "react";
import { Button } from "@/components/ui/button";
import { ChevronLeft, X } from "lucide-react";
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Card, CardContent, CardHeader, CardTitle, CardDescription, CardFooter } from "@/components/ui/card";
import yaml from "js-yaml";

const REPO_BASE_URL = 'https://raw.githubusercontent.com/linuxserver/proot-apps/master/metadata/';
const METADATA_URL = `${REPO_BASE_URL}metadata.yml`;
const IMAGE_BASE_URL = `${REPO_BASE_URL}img/`;
const INSTALLED_APPS_STORAGE_KEY = 'prootInstalledApps';

interface App {
    name: string;
    full_name: string;
    description: string;
    icon: string;
    disabled?: boolean;
}

interface AppsProps {
    isOpen?: boolean;
    onClose?: () => void;
}

export function Apps({ isOpen = false, onClose }: AppsProps = {}) {
    const [isAppsModalOpen, setIsAppsModalOpen] = useState(isOpen);
    const [appData, setAppData] = useState<{ include: App[] } | null>(null);
    const [isLoading, setIsLoading] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [searchTerm, setSearchTerm] = useState('');
    const [selectedApp, setSelectedApp] = useState<App | null>(null);
    const [installedApps, setInstalledApps] = useState<string[]>(() => {
        const savedApps = localStorage.getItem(INSTALLED_APPS_STORAGE_KEY);
        if (savedApps) {
            try {
                const parsedApps = JSON.parse(savedApps);
                if (Array.isArray(parsedApps) && parsedApps.every(item => typeof item === 'string')) {
                    return parsedApps;
                }
                console.warn("Invalid data found in localStorage for installed apps. Resetting.");
                localStorage.removeItem(INSTALLED_APPS_STORAGE_KEY);
            } catch (e) {
                console.error("Failed to parse installed apps from localStorage:", e);
                localStorage.removeItem(INSTALLED_APPS_STORAGE_KEY);
            }
        }
        return [];
    });

    useEffect(() => {
        localStorage.setItem(INSTALLED_APPS_STORAGE_KEY, JSON.stringify(installedApps));
    }, [installedApps]);

    // Sync with external isOpen prop
    useEffect(() => {
        setIsAppsModalOpen(isOpen);
    }, [isOpen]);

    // Handle modal close
    const handleModalClose = (open: boolean) => {
        setIsAppsModalOpen(open);
        if (!open && onClose) {
            onClose();
        }
    };

    useEffect(() => {
        if (isAppsModalOpen && !appData && !isLoading) {
            const fetchAppData = async () => {
                setIsLoading(true);
                setError(null);
                try {
                    const response = await fetch(METADATA_URL);
                    if (!response.ok) {
                        throw new Error(`HTTP error! status: ${response.status}`);
                    }
                    const yamlText = await response.text();
                    const parsedData = yaml.load(yamlText) as { include: App[] };
                    setAppData(parsedData);
                } catch (e) {
                    console.error("Failed to fetch or parse app data:", e);
                    setError("Failed to load app data. Please try again.");
                } finally {
                    setIsLoading(false);
                }
            };
            fetchAppData();
        }
    }, [isAppsModalOpen, appData, isLoading]);

    const handleSearchChange = (event: React.ChangeEvent<HTMLInputElement>) => {
        setSearchTerm(event.target.value.toLowerCase());
    };

    const handleAppClick = (app: App) => {
        setSelectedApp(app);
    };

    const handleBackToGrid = () => {
        setSelectedApp(null);
    };

    const handleInstall = (appName: string) => {
        console.log(`Install app: ${appName}`);
        window.postMessage({ type: 'command', value: `st ~/.local/bin/proot-apps install ${appName}` }, window.location.origin);
        setInstalledApps(prev => prev.includes(appName) ? prev : [...prev, appName]);
    };

    const handleRemove = (appName: string) => {
        console.log(`Remove app: ${appName}`);
        window.postMessage({ type: 'command', value: `st ~/.local/bin/proot-apps remove ${appName}` }, window.location.origin);
        setInstalledApps(prev => prev.filter(name => name !== appName));
    };

    const handleUpdate = (appName: string) => {
        console.log(`Update app: ${appName}`);
        window.postMessage({ type: 'command', value: `st ~/.local/bin/proot-apps update ${appName}` }, window.location.origin);
    };

    const handleLaunch = (appName: string) => {
        console.log(`Launch app: ${appName}`);
        window.postMessage({ type: 'command', value: `st ~/.local/bin/${appName}-pa` }, window.location.origin);
    };

    const filteredApps = appData?.include?.filter(app =>
        !app.disabled &&
        (app.full_name?.toLowerCase().includes(searchTerm) ||
         app.name?.toLowerCase().includes(searchTerm) ||
         app.description?.toLowerCase().includes(searchTerm))
    ) || [];

    const isAppInstalled = (appName: string) => installedApps.includes(appName);

    return (
        <>
            <Dialog open={isAppsModalOpen} onOpenChange={handleModalClose}>
                <DialogContent className="max-h-screen sm:max-w-[50vw] p-0">
                    <DialogHeader className="sticky top-0 z-10 bg-background p-6 border-b">
                        <div className="flex flex-col space-y-6">
                            <div className="flex flex-col lg:flex-row lg:items-center lg:justify-between gap-6">
                                <div>
                                    <DialogTitle>Apps</DialogTitle>
                                    <DialogDescription>
                                        Install and manage applications
                                    </DialogDescription>
                                </div>
                                <div className="flex items-center gap-2">
                                    <Input
                                        type="text"
                                        placeholder="Search apps..."
                                        value={searchTerm}
                                        onChange={handleSearchChange}
                                        className="w-full sm:w-[300px]"
                                    />
                                    <Button
                                        variant="secondary"
                                        size="icon"
                                        onClick={() => setIsAppsModalOpen(false)}
                                        className="h-10 w-10"
                                    >
                                        <X className="h-4 w-4" />
                                        <span className="sr-only">Close</span>
                                    </Button>
                                </div>
                            </div>
                        </div>
                    </DialogHeader>

                    <ScrollArea className="h-[calc(98vh-8rem)]">
                        {isLoading && (
                            <div className="flex justify-center items-center w-full h-full p-6">
                                <div className="flex flex-col items-center gap-4">
                                    <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-primary"></div>
                                    <span className="text-muted-foreground">Loading apps...</span>
                                </div>
                            </div>
                        )}
                        {error && (
                            <div className="p-6">
                                <Card className="border-destructive">
                                    <CardContent className="p-6">
                                        <div className="flex items-center gap-2 text-destructive">
                                            <X className="h-4 w-4" />
                                            <p>{error}</p>
                                        </div>
                                    </CardContent>
                                </Card>
                            </div>
                        )}
                        {!isLoading && !error && appData && (
                            <div className="p-6">
                                {selectedApp ? (
                                    <div className="space-y-6">
                                        <Button 
                                            variant="outline" 
                                            onClick={handleBackToGrid} 
                                            className="mb-4"
                                        >
                                            <ChevronLeft className="mr-2 h-4 w-4" />
                                            Back to list
                                        </Button>
                                        <Card className="bg-background/95 backdrop-blur-sm">
                                            <CardHeader className="space-y-4">
                                                <section className="flex items-center gap-4">
                                                    <img 
                                                        src={`${IMAGE_BASE_URL}${selectedApp.icon}`} 
                                                        alt={selectedApp.full_name} 
                                                        className="w-16 h-16 object-contain"
                                                        onError={(e) => { e.currentTarget.style.display = 'none'; }}
                                                    />
                                                    <header className="space-y-1">
                                                        <CardTitle className="text-xl">{selectedApp.full_name}</CardTitle>
                                                        <CardDescription className="text-base">{selectedApp.description}</CardDescription>
                                                    </header>
                                                </section>
                                            </CardHeader>
                                            <CardFooter className="flex gap-2 justify-end">
                                                {isAppInstalled(selectedApp.name) ? (
                                                    <>
                                                        <Button 
                                                            variant="default" 
                                                            onClick={() => handleLaunch(selectedApp.name)}
                                                            className="w-auto"
                                                        >
                                                            Launch {selectedApp.name}
                                                        </Button>
                                                        <Button 
                                                            variant="outline" 
                                                            onClick={() => handleUpdate(selectedApp.name)}
                                                            className="w-auto"
                                                        >
                                                            Update {selectedApp.name}
                                                        </Button>
                                                        <Button 
                                                            variant="destructive" 
                                                            onClick={() => handleRemove(selectedApp.name)}
                                                            className="w-auto"
                                                        >
                                                            Remove {selectedApp.name}
                                                        </Button>
                                                    </>
                                                ) : (
                                                    <Button 
                                                        variant="default" 
                                                        onClick={() => handleInstall(selectedApp.name)}
                                                        className="w-auto"
                                                    >
                                                        Install {selectedApp.name}
                                                    </Button>
                                                )}
                                            </CardFooter>
                                        </Card>
                                    </div>
                                ) : (
                                    <div className="grid grid-cols-1 sm:grid-cols-2 md:grid-cols-3 lg:grid-cols-4 xl:grid-cols-6 gap-4">
                                        {filteredApps.length > 0 ? (
                                            filteredApps.map(app => (
                                                <Card 
                                                    key={app.name} 
                                                    className="cursor-pointer hover:bg-accent/50 transition-colors bg-background/95 backdrop-blur-sm relative aspect-square group"
                                                    onClick={() => handleAppClick(app)}
                                                >
                                                    {isAppInstalled(app.name) && (
                                                        <div className="absolute top-2 right-2 w-2 h-2 rounded-full bg-green-500" />
                                                    )}
                                                    <CardContent className="p-4 h-full flex flex-col items-center justify-center">
                                                        <div className="flex flex-col items-center text-center">
                                                            <img 
                                                                src={`${IMAGE_BASE_URL}${app.icon}`} 
                                                                alt={app.full_name} 
                                                                className="w-12 h-12 object-contain mb-2"
                                                                loading="lazy"
                                                                onError={(e) => { e.currentTarget.style.visibility = 'hidden'; }}
                                                            />
                                                            <div>
                                                                <CardTitle className="text-sm line-clamp-2 group-hover:text-primary transition-colors">
                                                                    {app.full_name}
                                                                </CardTitle>
                                                            </div>
                                                        </div>
                                                    </CardContent>
                                                </Card>
                                            ))
                                        ) : (
                                            <Card className="col-span-full">
                                                <CardContent className="p-6">
                                                    <div className="flex flex-col items-center justify-center text-center">
                                                        <p className="text-muted-foreground">
                                                            No apps found matching your search.
                                                        </p>
                                                    </div>
                                                </CardContent>
                                            </Card>
                                        )}
                                    </div>
                                )}
                            </div>
                        )}
                    </ScrollArea>
                </DialogContent>
            </Dialog>
        </>
    );
}