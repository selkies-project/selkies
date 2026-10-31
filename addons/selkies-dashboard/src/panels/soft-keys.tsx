/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The soft keys a touch client types with: the held modifiers and the keys a
 * phone keyboard lacks, the key palette with the user's own chords, the
 * trackpad switch with its speed, and the button that pops the system's
 * on-screen keyboard. Shown from the first touch the session sees; the
 * layout decides where the bar sits. The palette opens above the keys and
 * scrolls within what the viewport leaves, so the keys stay in reach on a
 * phone in either orientation.
 * @module
 */
import * as React from "react";
import { Keyboard, Touchpad } from "lucide-react";
import { Button } from "@/components/ui/button";
import { NativeSelect } from "@/panels/controls";
import { PALETTE_CHORDS, PALETTE_KEYS, TRACKPAD_SPEEDS, useDashboard } from "@/state/dashboard";
import { t } from "@/i18n";

/** A soft key: a mouse press leaves the stream's focus where it is. */
function Key({ active, className, ...props }: React.ComponentProps<typeof Button> & { active?: boolean }) {
  return (
    <Button variant={active ? "default" : "secondary"} size="sm" className={`mobile-key-button h-(--soft-key-h) ${className ?? ""}`}
      onMouseDown={(event) => event.preventDefault()} {...props} />
  );
}

interface SoftKeysProps {
  /** Draws the trackpad switch in the row; off where the layout has a tile for it. */
  showTrackpad?: boolean;
  /** Draws the button that pops the on-screen keyboard; off where the layout has one. */
  showKeyboardButton?: boolean;
}

/** The modifier row and the palette, the trackpad controls beside them. */
export function SoftKeys({ showTrackpad: trackpadButton = true, showKeyboardButton = true }: SoftKeysProps) {
  const d = useDashboard();
  const [paletteOpen, setPaletteOpen] = React.useState(false);
  const [draft, setDraft] = React.useState("");
  const [refused, setRefused] = React.useState(false);
  const showKeys = d.renderable.softButtons;
  const showTrackpad = d.renderable.trackpad;
  if (!d.isTouch || (!showKeys && !showTrackpad)) return null;

  const addChord = (event: React.FormEvent) => {
    event.preventDefault();
    const added = d.addChord(draft);
    setRefused(!added);
    if (added) setDraft("");
  };

  return (
    <div data-soft-keys className="flex min-h-0 flex-col gap-2 [--soft-key-h:2.5rem]">
      {showKeys && paletteOpen && (
        <div className="key-palette flex min-h-0 max-w-[22rem] flex-col gap-2 overflow-y-auto">
          <div className="grid grid-cols-[repeat(auto-fill,minmax(2.5rem,1fr))] gap-1">
            {PALETTE_KEYS.map(([label, key, code]: string[]) => (
              <Key key={code} className="palette-grid-key min-w-0 px-0.5 text-xs" data-code={code} onClick={() => d.pressKey(key, code)}>{label}</Key>
            ))}
          </div>
          <div className="flex flex-wrap gap-1 border-t border-border/60 pt-2">
            {PALETTE_CHORDS.map((chord: string) => (
              <Key key={chord} className="flex-auto px-3" data-chord={chord} onClick={() => d.playChord(chord)}>{chord}</Key>
            ))}
          </div>
          <div className="flex flex-wrap gap-1 empty:hidden">
            {d.userChords.map((chord) => (
              <span key={chord} className="flex gap-0.5">
                <Key data-chord={chord} onClick={() => d.playChord(chord)}>{chord}</Key>
                <Button variant="outline" size="sm" className="h-(--soft-key-h)" aria-label={t('keyPalette.remove', { chord })} title={t('keyPalette.remove', { chord })}
                  onClick={() => d.removeChord(chord)} onMouseDown={(event) => event.preventDefault()}>×</Button>
              </span>
            ))}
          </div>
          <form className="flex gap-1" onSubmit={addChord}>
            <input type="text" className="key-palette-input allow-native-input min-w-0 flex-1 h-(--soft-key-h) rounded border bg-background px-3 text-base"
              value={draft} onChange={(event) => { setDraft(event.target.value); setRefused(false); }}
              placeholder={t('keyPalette.addPlaceholder')} aria-label={t('keyPalette.addPlaceholder')}
              autoCapitalize="off" autoCorrect="off" spellCheck={false} />
            <Button type="submit" variant="secondary" size="sm" className="h-(--soft-key-h)">{t('keyPalette.add')}</Button>
          </form>
          {refused && <p className="text-xs text-muted-foreground">{t('keyPalette.refused')}</p>}
        </div>
      )}
      <div className="flex shrink-0 flex-col items-start gap-1.5">
        <div className="flex w-full flex-wrap items-center gap-1">
          {showKeys && (
            <>
              <Key active={d.heldKeys.Control} className="flex-auto px-1.5 text-[clamp(0.6875rem,3.4vw,0.875rem)]" onClick={() => d.holdKey('Control', 'ControlLeft')}>CTRL</Key>
              <Key active={d.heldKeys.Alt} className="flex-auto px-1.5 text-[clamp(0.6875rem,3.4vw,0.875rem)]" onClick={() => d.holdKey('Alt', 'AltLeft')}>ALT</Key>
              <Key active={d.heldKeys.Meta} className="flex-auto px-1.5 text-[clamp(0.6875rem,3.4vw,0.875rem)]" onClick={() => d.holdKey('Meta', 'MetaLeft')}>WIN</Key>
              <Key className="flex-auto px-1.5 text-[clamp(0.6875rem,3.4vw,0.875rem)]" onClick={() => d.pressKey('Tab', 'Tab')}>TAB</Key>
              <Key className="flex-auto px-1.5 text-[clamp(0.6875rem,3.4vw,0.875rem)]" onClick={() => d.pressKey('Escape', 'Escape')}>ESC</Key>
              {showKeyboardButton && d.renderable.keyboardButton && (
                <Button variant="secondary" size="sm" className="h-(--soft-key-h) shrink-0 aspect-square px-0" aria-label={t('topMenu.virtualKeyboard')} onClick={d.showVirtualKeyboard}>
                  <Keyboard className="size-4" />
                </Button>
              )}
            </>
          )}
          {showTrackpad && trackpadButton && (
            <Button variant={d.trackpad ? "default" : "secondary"} size="sm" className="trackpad-mode-button h-(--soft-key-h) shrink-0 aspect-square px-0" aria-pressed={d.trackpad}
              aria-label={t('trackpadModeTitle')} title={t('trackpadModeTitle')} onClick={d.toggleTrackpad}>
              <Touchpad className="size-4" />
            </Button>
          )}
        </div>
        <div className="flex flex-wrap items-center gap-1.5">
          {showKeys && (
            <Key active={paletteOpen} className="key-palette-toggle" aria-expanded={paletteOpen} onClick={() => setPaletteOpen((open) => !open)}>
              {paletteOpen ? t('keyPalette.less') : t('keyPalette.more')}
            </Key>
          )}
          {showTrackpad && d.trackpad && (
            <label className="flex items-center gap-1 text-xs">
              <span>{t('trackpadSpeedLabel')}</span>
              <NativeSelect id="trackpadSpeedSelect" className="w-16" value={String(d.trackpadSpeed)}
                onValueChange={(value) => d.setTrackpadSpeed(Number(value))}
                options={TRACKPAD_SPEEDS.map((v: number) => ({ value: String(v), label: `${v}×` }))} />
            </label>
          )}
        </div>
      </div>
    </div>
  );
}
