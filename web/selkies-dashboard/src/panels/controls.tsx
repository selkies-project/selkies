/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The form controls every panel is built from: a labeled row with a switch,
 * a labeled field, a native select, and a native range slider, each drawn
 * with the kit's tokens. Native elements where the kit has a composite: a
 * select or a range the browser owns works with every screen reader, opens
 * the platform's own picker on a phone, and takes its value through the
 * keyboard without a script.
 * @module
 */
import * as React from "react";
import { ChevronDown } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { cn } from "@/lib/utils";
import { hint } from "@/utils";

/**
 * A label cut with an ellipsis where its row leaves it no room; the row's
 * hover text carries it whole. A skin that would rather wrap it (the
 * sidebar's, read on touch screens that have no hover) addresses `clip`.
 */
export function Clip({ className, children }: { className?: string; children: React.ReactNode }) {
  return <span className={cn("clip min-w-0 truncate", className)}>{children}</span>;
}

interface SwitchRowProps {
  /** The element id, which tests and labels address. */
  id: string;
  label: React.ReactNode;
  /** The control's detail, shown on hover after the label. */
  title?: string;
  checked: boolean;
  onToggle: () => void;
  disabled?: boolean;
}

/** A label with a switch at the row's end; the id names the switch button itself, as a native control's would. */
export function SwitchRow({ id, label, title, checked, onToggle, disabled }: SwitchRowProps) {
  return (
    <div className="setting-item toggle-item flex items-center justify-between gap-3">
      <Label htmlFor={id} title={hint(label, title)} className="min-w-0"><Clip>{label}</Clip></Label>
      <Switch id={id} nativeButton checked={checked} onCheckedChange={onToggle} disabled={disabled} />
    </div>
  );
}

/**
 * A panel's action button, at the layout's field metrics (`--control-h`, `--control-font`) like the fields beside it,
 * its label wrapping rather than clipping. Outlined unless asked otherwise.
 */
export function ActionButton({ className, variant = "outline", ...props }: React.ComponentProps<typeof Button>) {
  return (
    <Button variant={variant} {...props}
      className={cn("h-auto min-h-(--control-h) min-w-0 py-1 text-(length:--control-font) whitespace-normal", className)} />
  );
}

interface FieldProps {
  /** The id of the control the label is for. */
  htmlFor: string;
  label: React.ReactNode;
  title?: string;
  className?: string;
  children: React.ReactNode;
}

/** A label above its control. */
export function Field({ htmlFor, label, title, className, children }: FieldProps) {
  return (
    <div className={cn("setting-item flex flex-col gap-(--field-gap)", className)}>
      <Label htmlFor={htmlFor} title={hint(label, title)} className="min-w-0"><Clip>{label}</Clip></Label>
      {children}
    </div>
  );
}

/** One option of a `NativeSelect`. */
export interface SelectOption {
  value: string;
  label: string;
  disabled?: boolean;
}

interface NativeSelectProps extends Omit<React.SelectHTMLAttributes<HTMLSelectElement>, "onChange"> {
  options: SelectOption[];
  /** Called with the picked value. */
  onValueChange: (value: string) => void;
  /** A first, unpickable entry naming what the list is for. */
  placeholder?: string;
}

/** The browser's own select, drawn as the kit's outline button. */
export function NativeSelect({ options, onValueChange, placeholder, className, ...props }: NativeSelectProps) {
  return (
    <div className="relative">
      <select
        {...props}
        onChange={(event) => onValueChange(event.target.value)}
        className={cn(
          "h-(--control-h) w-full cursor-pointer appearance-none truncate rounded-md border border-input bg-background pr-8 pl-2.5 text-(length:--control-font) text-foreground shadow-xs outline-none",
          "hover:bg-muted focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50",
          "dark:border-input dark:bg-input/30 dark:hover:bg-input/50",
          className,
        )}
      >
        {placeholder !== undefined && <option value="" disabled>{placeholder}</option>}
        {options.map((option) => (
          <option key={option.value} value={option.value} disabled={option.disabled}>{option.label}</option>
        ))}
      </select>
      <ChevronDown className="pointer-events-none absolute top-1/2 right-2 size-3 -translate-y-1/2 text-muted-foreground" aria-hidden />
    </div>
  );
}

interface RangeSliderProps extends Omit<React.InputHTMLAttributes<HTMLInputElement>, "onChange" | "value"> {
  value: number;
  /** Called with the slider's number. */
  onValueChange: (value: number) => void;
}

/** The browser's own range input, filled to its value in the accent color. */
export function RangeSlider({ value, onValueChange, min = 0, max = 100, className, ...props }: RangeSliderProps) {
  const span = Number(max) - Number(min);
  const percent = span > 0 ? ((value - Number(min)) / span) * 100 : 0;
  return (
    <input
      type="range"
      {...props}
      min={min}
      max={max}
      value={value}
      onChange={(event) => onValueChange(Number(event.target.value))}
      style={{ "--range-fill": `${percent}%` } as React.CSSProperties}
      className={cn("range-slider w-full", className)}
    />
  );
}
