/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { useState, useEffect } from "react";
import { Button } from "@/components/ui/button";
import { X } from "lucide-react";
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription } from "@/components/ui/dialog";
import { toast } from "sonner";

// Helper function to format bytes
function formatBytes(bytes: number, decimals = 2): string {
    if (bytes === 0) return '0 Bytes';
    const k = 1024;
    const dm = decimals < 0 ? 0 : decimals;
    const sizes = ['Bytes', 'KB', 'MB', 'GB', 'TB', 'PB', 'EB', 'ZB', 'YB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(dm)) + ' ' + sizes[i];
}

interface FilesProps {
    // No props needed for direct content rendering
}

export function Files({}: FilesProps = {}) {
    const [isFilesModalOpen, setIsFilesModalOpen] = useState(false);

    const handleUploadClick = () => {
        window.dispatchEvent(new CustomEvent('requestFileUpload'));
    };

    const toggleFilesModal = () => {
        setIsFilesModalOpen(!isFilesModalOpen);
    };

    // Listen for file upload events
    useEffect(() => {
        const handleWindowMessage = (event: MessageEvent) => {
            if (event.origin !== window.location.origin) return;
            const message = event.data;
            
            if (typeof message === 'object' && message !== null && message.type === 'fileUpload') {
                const { status, fileName, progress, message: errMsg } = message.payload;

                if (status === 'start') {
                    toast.loading(`Uploading ${fileName}...`, {
                        id: fileName,
                    });
                } else if (status === 'progress') {
                    toast.loading(`Uploading ${fileName}: ${progress}%`, {
                        id: fileName,
                    });
                } else if (status === 'end') {
                    toast.success(`Successfully uploaded ${fileName}`, {
                        id: fileName,
                    });
                } else if (status === 'error') {
                    const errorMessage = errMsg ? `Error: ${errMsg}` : 'Unknown error';
                    toast.error(`Failed to upload ${fileName}: ${errorMessage}`, {
                        id: fileName,
                    });
                }
            }
        };

        window.addEventListener('message', handleWindowMessage);
        return () => window.removeEventListener('message', handleWindowMessage);
    }, []);

    return (
        <>
            <div className="w-auto p-4 flex flex-col gap-2">
                <Button 
                    variant="outline" 
                    className="mb-2"
                    onClick={handleUploadClick}
                >
                    Upload Files
                </Button>
                <Button 
                    variant="outline" 
                    className="mb-2"
                    onClick={toggleFilesModal}
                >
                    Download Files
                </Button>
            </div>

            <Dialog open={isFilesModalOpen} onOpenChange={setIsFilesModalOpen}>
                <DialogContent className="max-h-[90vh] sm:max-w-[80vw] p-0">
                    <DialogHeader className="sticky top-0 z-10 bg-background p-6 border-b">
                        <div className="flex flex-col space-y-6">
                            <div className="flex flex-col lg:flex-row lg:items-center lg:justify-between gap-6">
                                <div>
                                    <DialogTitle>Files</DialogTitle>
                                    <DialogDescription>
                                        Download and manage files
                                    </DialogDescription>
                                </div>
                                <div className="flex items-center gap-2">
                                    <Button
                                        variant="secondary"
                                        size="icon"
                                        onClick={() => setIsFilesModalOpen(false)}
                                        className="h-10 w-10"
                                    >
                                        <X className="h-4 w-4" />
                                        <span className="sr-only">Close</span>
                                    </Button>
                                </div>
                            </div>
                        </div>
                    </DialogHeader>

                    <div className="flex-1 overflow-hidden">
                        <iframe 
                            src="/files" 
                            title="Downloadable Files"
                            className="w-full h-[calc(90vh-8rem)] border-0"
                        />
                    </div>
                </DialogContent>
            </Dialog>
        </>
    );
}