import { motion, AnimatePresence } from "framer-motion";
import { Settings2, X, Gauge, Hand, Share2 } from "lucide-react"; 
import { Button } from "@/components/ui/button";
import * as React from "react";
import { SystemMonitoring } from "./system-monitoring";
import { Settings } from "./settings";
import { Sharing } from "@/components/dashboard/sharing";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";

interface StatsTopBarProps {
    toggleStats: () => void;
}

export function StatsTopBar({ toggleStats }: StatsTopBarProps) {
    const [showDetails, setShowDetails] = React.useState(false);
    const [showSystemMonitoring, setShowSystemMonitoring] = React.useState(false);
    const [showSharing, setShowSharing] = React.useState(false);
    const [isDragging, setIsDragging] = React.useState(false);
    const [position, setPosition] = React.useState(() => {
        // Calculate center position on mount
        const x = window.innerWidth / 2 - 150;
        return { x, y: 0 };
    });
    const [copiedId, setCopiedId] = React.useState<string | null>(null);
    const dragRef = React.useRef<HTMLDivElement>(null);
    const startPosRef = React.useRef({ x: 0, y: 0 });

    const sharingLinks = [
        {
            id: "shared",
            label: "Read only viewer",
            tooltip: "Read only client for viewing, as many clients as needed can connect to this endpoint and see the live session",
            hash: "#shared",
        },
        {
            id: "player2",
            label: "Controller 2",
            tooltip: "Player 2 gamepad input, this endpoint has full control over the player 2 gamepad",
            hash: "#player2",
        },
        {
            id: "player3",
            label: "Controller 3",
            tooltip: "Player 3 gamepad input, this endpoint has full control over the player 3 gamepad",
            hash: "#player3",
        },
        {
            id: "player4",
            label: "Controller 4",
            tooltip: "Player 4 gamepad input, this endpoint has full control over the player 4 gamepad",
            hash: "#player4",
        },
    ];
    const baseUrl = typeof window !== "undefined" ? window.location.href.split("#")[0] : "";

    const toggleDetails = () => {
        setShowDetails((prev) => {
            const next = !prev;
            if (next) {
                setShowSystemMonitoring(false);
                setShowSharing(false);
            }
            return next;
        });
    };

    const toggleSystemMonitoring = () => {
        setShowSystemMonitoring((prev) => {
            const next = !prev;
            if (next) {
                setShowDetails(false);
                setShowSharing(false);
            }
            return next;
        });
    };

    const handleMouseDown = (e: React.MouseEvent) => {
        e.preventDefault();
        setIsDragging(true);
        startPosRef.current = {
            x: e.clientX - position.x,
            y: e.clientY - position.y
        };
    };

    const handleCopyLink = async (fullUrl: string, id: string) => {
        if (!navigator.clipboard) return;
        try {
            await navigator.clipboard.writeText(fullUrl);
            setCopiedId(id);
            setTimeout(() => setCopiedId(null), 2000);
        } catch (err) {}
    };

    React.useEffect(() => {
        const handleMouseMove = (e: MouseEvent) => {
            if (!isDragging) return;
            
            const newX = e.clientX - startPosRef.current.x;
            const newY = e.clientY - startPosRef.current.y;
            setPosition({ x: newX, y: newY });
        };

        const handleMouseUp = () => {
            setIsDragging(false);
        };

        if (isDragging) {
            document.addEventListener('mousemove', handleMouseMove);
            document.addEventListener('mouseup', handleMouseUp);
        }

        return () => {
            document.removeEventListener('mousemove', handleMouseMove);
            document.removeEventListener('mouseup', handleMouseUp);
        };
    }, [isDragging]);

    return (
        <>
            <motion.div
                ref={dragRef}
                initial={{ y: "-100%" }}
                animate={{ y: 0 }}
                exit={{ y: "-100%" }}
                transition={{ type: "spring", stiffness: 300, damping: 30 }}
                className="absolute z-50 w-fit rounded-lg border bg-background/95 backdrop-blur-sm shadow-lg"
                style={{
                    left: position.x,
                    top: position.y,
                }}
            >
                <div className="flex items-center space-x-4 px-4 py-2">
                    <div className="flex items-center space-x-1">
                        <Button
                            variant={showDetails ? "default" : "secondary"}
                            size="icon"
                            className="h-6 w-6"
                            onClick={toggleDetails}
                        >
                            <Settings2 className="h-4 w-4" />
                        </Button>
                        <Button
                            variant={showSystemMonitoring ? "default" : "secondary"}
                            size="icon"
                            className="h-6 w-6"
                            onClick={toggleSystemMonitoring}
                        >
                            <Gauge className="h-4 w-4" />
                        </Button>
                        <Button
                            variant={showSharing ? "default" : "secondary"}
                            size="icon"
                            className="h-6 w-6"
                            aria-label="Share session links"
                            onClick={() => {
                                setShowSharing((prev) => {
                                    const next = !prev;
                                    if (next) {
                                        setShowSystemMonitoring(false);
                                        setShowDetails(false);
                                    }
                                    return next;
                                });
                            }}
                        >
                            <Share2 className="h-4 w-4" />
                        </Button>
                        <Button
                            variant="destructive"
                            size="icon"
                            className="h-6 w-6"
                            onClick={toggleStats}
                        >
                            <X className="h-4 w-4" />
                        </Button>
                        <Button
                            variant="secondary"
                            size="icon"
                            className="h-6 w-6 cursor-grab active:cursor-grabbing select-none"
                            onMouseDown={handleMouseDown}
                        >
                            <Hand className="h-4 w-4" />
                        </Button>
                    </div>
                </div>
            </motion.div>

            <AnimatePresence>
                {showDetails && (
                    <motion.div
                        initial={{ opacity: 0, y: -20 }}
                        animate={{ opacity: 1, y: 0 }}
                        exit={{ opacity: 0, y: -20 }}
                        className="absolute z-20 w-fit"
                        style={{
                            left: position.x,
                            top: position.y + 48,
                        }}
                    >
                        <Settings />
                    </motion.div>
                )}
            </AnimatePresence>

            <AnimatePresence>
                {showSystemMonitoring && (
                    <motion.div
                        initial={{ opacity: 0, y: -20 }}
                        animate={{ opacity: 1, y: 0 }}
                        exit={{ opacity: 0, y: -20 }}
                        className="absolute z-20 w-fit"
                        style={{
                            left: position.x,
                            top: position.y + 48,
                        }}
                    >
                        <SystemMonitoring scale={0.8} />
                    </motion.div>
                )}
            </AnimatePresence>

            <AnimatePresence>
                {showSharing && (
                    <motion.div
                        initial={{ opacity: 0, y: -20 }}
                        animate={{ opacity: 1, y: 0 }}
                        exit={{ opacity: 0, y: -20 }}
                        className="absolute z-20 w-fit"
                        style={{
                            left: position.x,
                            top: position.y + 48,
                        }}
                    >
                        <Sharing show={showSharing} onClose={() => setShowSharing(false)} />
                    </motion.div>
                )}
            </AnimatePresence>
        </>
    );
}