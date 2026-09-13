/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { useState, useEffect } from "react";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { getPrefixedKey, getPrintJobs, PrintJob } from "@/utils";
import { t } from "@/i18n";

/**
 * The printing panel: the switch that prints each document as it arrives,
 * and the documents printed so far, each with a print and a save button.
 *
 * The documents are the core's `printDocument` messages, kept in utils so
 * none is missed while the panel is closed; the switch travels on the
 * `settings` message and a print on `printRequest`.
 * @module
 */

/** Renders the automatic-printing switch and the printed documents. */
export function Printing() {
    const [jobs, setJobs] = useState<PrintJob[]>(() => getPrintJobs());
    const [automatic, setAutomatic] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("print_auto"));
        return saved !== null ? saved === 'true' : true;
    });

    useEffect(() => {
        const refresh = () => setJobs(getPrintJobs());
        window.addEventListener('printJobsChanged', refresh);
        return () => window.removeEventListener('printJobsChanged', refresh);
    }, []);

    const toggle = () => {
        const value = !automatic;
        setAutomatic(value);
        localStorage.setItem(getPrefixedKey("print_auto"), String(value));
        window.postMessage({ type: 'settings', settings: { print_auto: value } }, window.location.origin);
    };

    return (
        <div className="w-[300px] p-4 flex flex-col gap-3">
            <div className="flex items-center justify-between">
                <Label className="text-sm font-medium" title={t('sections.printing.automaticDetails')}>{t('sections.printing.automaticLabel')}</Label>
                <Switch checked={automatic} onCheckedChange={toggle} />
            </div>
            {jobs.map((job) => (
                <div key={job.url} className="flex items-center gap-2">
                    <span className="flex-1 truncate text-sm" title={job.name}>{job.name}</span>
                    <Button variant="outline" size="sm"
                        onClick={() => window.postMessage({ type: 'printRequest', url: job.url }, window.location.origin)}>
                        {t('sections.printing.printButton')}
                    </Button>
                    <Button variant="outline" size="sm" asChild>
                        <a href={job.url} download={job.name}>{t('sections.printing.saveButton')}</a>
                    </Button>
                </div>
            ))}
        </div>
    );
}
