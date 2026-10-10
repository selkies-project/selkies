/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The dashboard's sectioned accordion: the kit's accordion drawn as separate
 * bordered cards, each with an uppercase header and a divided body, so the
 * Settings panel and the System Monitoring view look the same. The open and
 * active accents come from the rules the accent script writes into
 * `index.css`, not from here.
 * @module
 */

import * as React from "react";

import { Accordion, AccordionContent, AccordionItem, AccordionTrigger } from "@/components/ui/accordion";

/** Classes that draw the accordion as separate cards rather than one list. */
const GROUP = "gap-1 overflow-visible rounded-none border-0";
const CARD = "rounded-lg border";
const HEADER = "items-center px-3 py-2.5 text-xs font-semibold uppercase tracking-wide text-muted-foreground hover:no-underline";
const BODY = "space-y-4 border-t px-3 pt-3";

/**
 * The accordion that holds `SectionItem` cards, one open at a time unless the
 * props say otherwise.
 * @param props The kit accordion's own props.
 * @returns The accordion.
 */
export function SectionAccordion({ className, ...props }: React.ComponentProps<typeof Accordion>) {
    return <Accordion className={[GROUP, className].filter(Boolean).join(" ")} {...props} />;
}

/** Props of `SectionItem`. */
interface SectionItemProps {
    /** Identifies the card to the accordion's `value` and `defaultValue`. */
    value: string;
    /** Heading in the header button, already translated. */
    title: React.ReactNode;
    /** The card's body. */
    children: React.ReactNode;
}

/**
 * One card of a `SectionAccordion`: a header button and a body of controls.
 * @param props The card's props.
 * @returns The card.
 */
export function SectionItem({ value, title, children }: SectionItemProps) {
    return (
        <AccordionItem value={value} className={CARD}>
            <AccordionTrigger className={HEADER}>{title}</AccordionTrigger>
            <AccordionContent className={BODY}>{children}</AccordionContent>
        </AccordionItem>
    );
}
