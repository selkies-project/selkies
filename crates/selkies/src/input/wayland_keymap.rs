/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Keysym policy owner for the Wayland compositor seat (`_WaylandKeymapOwner`).
//!
//! Resolves keysyms to keycodes against the compositor's own keymap (synthesizing Shift and
//! AltGr for leveled glyphs) and binds unmapped keysyms, Unicode codepoints and IME output, to
//! spare overlay keycodes by swapping in a rebuilt keymap. Everything is delivered through
//! `inject_key`/`inject_keys` and `set_keymap_overlay` on the one compositor channel, so key
//! ordering holds. A failure is returned so the caller can fall back to the next injection rung.
//!
//! Overlay keycodes are chosen from the live keymap, not a fixed range: an X11 keycode is a
//! byte, so a bind above 255 reaches Wayland apps only and XWayland clients never see it. The
//! sub-256 range is nearly full on a pc105 keymap, so only unbound keycodes and keycodes
//! carrying only XF86 vendor keysyms (media and browser keys a streamed session does not need)
//! are taken; everything else down there is load bearing (modifiers, F-keys, punctuation,
//! Print) and never touched. Of those, the text spares (`TEXT_SPARES`) come first and are
//! recycled before any other is taken, and `type_text` types a longer run in chunks that fit
//! them. An overflow band past the ceiling keeps a layout with no room working for Wayland
//! clients instead of failing outright.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use xkbcommon::xkb;

use super::keysyms::{
    KEYSYM_ISO_LEVEL3_SHIFT, KEYSYM_SHIFT_L, KEYSYM_SHIFT_R, character_to_layout_keysym,
    overlay_bind_keysym, universal_text_keysym,
};
use super::seat::WaylandSeat;

/// Spare keycodes every client types a character on: F13 to F24, then the JIS IntlRo and
/// IntlYen keys. A Wayland client may read a key's code as the physical key it names before the
/// keysym the keymap gives, and Chromium drops a code naming no key it knows and runs its
/// browser, media, and launcher keys as commands, so a character on another spare reaches every
/// client but Chromium.
pub const TEXT_SPARES: [u32; 14] = [
    191, 192, 193, 194, 195, 196, 197, 198, 199, 200, 201, 202, 97, 132,
];

/// Keycodes below this reach XWayland clients.
const SUB256_CEILING: u32 = 256;
/// Where the overflow band starts.
const OVERFLOW_BASE_KEYCODE: u32 = 257;
/// How many overflow keycodes serve Wayland clients when the sub-256 pool is spent.
const OVERFLOW_SLOTS: u32 = 64;
/// XF86 vendor keysym block, matched numerically: a name lookup per keysym doubled the cost of
/// building the map.
const VENDOR_FIRST: u32 = 0x1008_0000;
const VENDOR_LAST: u32 = 0x1008_FFFF;

/// A keysym's place in the base keymap: keycode and level.
type Placement = (u32, u32);

/// The seat's keysym policy; see the module docstring.
pub struct WaylandKeymapOwner {
    seat: Arc<dyn WaylandSeat>,
    /// Keysym to `(keycode, level)` in the base keymap.
    map: HashMap<u32, Placement>,
    /// Keysym to overlay keycode.
    overlay: HashMap<u32, u32>,
    /// Overlay keysyms in bind order, for round-robin recycling.
    overlay_order: Vec<u32>,
    /// Overlay keycode pool in preference order, the text spares first.
    overlay_codes: Vec<u32>,
    /// How many of `overlay_codes` are text spares.
    text_codes: usize,
    /// Held keysym to `(keycode, synthesized modifier keycodes)`.
    pressed: HashMap<u32, (u32, Vec<u32>)>,
    /// Synthesized modifier keycode to holder count.
    mod_refs: HashMap<u32, u32>,
    /// Every keycode currently injected down; the one live view that synth-skip and
    /// conflict-lift decisions read (no compositor query).
    down: HashSet<u32>,
    shift_kc: u32,
    shift_r_kc: u32,
    altgr_kc: u32,
}

impl WaylandKeymapOwner {
    /// Compile `base_keymap_text` and index every keysym plus the overlay pool.
    ///
    /// One walk over the keymap fills the keysym-to-placement map, collects overlay
    /// candidates, and resolves the synth-modifier keycodes.
    pub fn new(seat: Arc<dyn WaylandSeat>, base_keymap_text: &str) -> Result<Self, String> {
        if base_keymap_text.is_empty() {
            return Err("empty compositor keymap".into());
        }
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let km = xkb::Keymap::new_from_string(
            &ctx,
            base_keymap_text.to_string(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| "keymap compile failed".to_string())?;
        let lo = km.min_keycode().raw();
        let hi = km.max_keycode().raw();
        let mut map: HashMap<u32, Placement> = HashMap::new();
        let mut unbound = Vec::new();
        let mut shadowable = Vec::new();
        for kc in lo..=hi {
            let key = xkb::Keycode::new(kc);
            let levels = km.num_levels_for_key(key, 0).min(4);
            let (mut seen, mut spare) = (0u32, 0u32);
            for level in 0..levels {
                for sym in km.key_get_syms_by_level(key, 0, level) {
                    let sym = sym.raw();
                    if sym == 0 {
                        continue;
                    }
                    seen += 1;
                    if (VENDOR_FIRST..=VENDOR_LAST).contains(&sym) {
                        spare += 1;
                    }
                    map.entry(sym).or_insert((kc, level));
                }
            }
            if kc < SUB256_CEILING && kc >= lo.max(9) {
                if seen == 0 {
                    unbound.push(kc);
                } else if seen == spare {
                    shadowable.push(kc);
                }
            }
        }
        let spares: Vec<u32> = unbound.into_iter().chain(shadowable).collect();
        let text: Vec<u32> = TEXT_SPARES
            .iter()
            .copied()
            .filter(|kc| spares.contains(kc))
            .collect();
        let text_codes = text.len();
        let mut overlay_codes = text.clone();
        overlay_codes.extend(spares.iter().copied().filter(|kc| !text.contains(kc)));
        overlay_codes.extend(OVERFLOW_BASE_KEYCODE..OVERFLOW_BASE_KEYCODE + OVERFLOW_SLOTS);
        // Fallbacks are the conventional evdev+8 keycodes.
        let kc_of = |ks: u32, dflt: u32| map.get(&ks).map(|p| p.0).unwrap_or(dflt);
        Ok(Self {
            shift_kc: kc_of(KEYSYM_SHIFT_L, 50),
            shift_r_kc: kc_of(KEYSYM_SHIFT_R, 62),
            altgr_kc: kc_of(KEYSYM_ISO_LEVEL3_SHIFT, 108),
            seat,
            map,
            overlay: HashMap::new(),
            overlay_order: Vec::new(),
            overlay_codes,
            text_codes,
            pressed: HashMap::new(),
            mod_refs: HashMap::new(),
            down: HashSet::new(),
        })
    }

    /// Whether the base keymap carries this keysym on some key and level, so it can be
    /// injected without an overlay bind.
    pub fn resolves(&self, keysym: u32) -> bool {
        self.map.contains_key(&keysym)
    }

    /// The base keycode of a keysym the layout carries.
    pub fn base_keycode(&self, keysym: u32) -> Option<u32> {
        self.map.get(&keysym).map(|p| p.0)
    }

    /// How many keysyms the base keymap carries.
    pub fn map_len(&self) -> usize {
        self.map.len()
    }

    /// How many keys are held through this owner.
    pub fn held_count(&self) -> usize {
        self.pressed.len()
    }

    /// The keycode a held keysym went down at, and whether that keycode is down.
    pub fn held_keycode(&self, keysym: u32) -> Option<u32> {
        self.pressed.get(&keysym).map(|(kc, _)| *kc)
    }

    /// Whether `keycode` is injected down.
    pub fn is_down(&self, keycode: u32) -> bool {
        self.down.contains(&keycode)
    }

    /// The overlay keycode a keysym is bound to.
    pub fn overlay_keycode(&self, keysym: u32) -> Option<u32> {
        self.overlay.get(&keysym).copied()
    }

    /// How many overlay binds exist.
    pub fn overlay_len(&self) -> usize {
        self.overlay.len()
    }

    /// Shift/AltGr keycodes currently down that the target level rejects.
    ///
    /// Held (client-pressed or synthesized), they would shift the injected key onto a different
    /// glyph. A required Shift is satisfied by either side, so neither Shift keycode is lifted
    /// then.
    fn held_conflicts(&self, required: &[u32]) -> Vec<u32> {
        let shift_wanted = required.contains(&self.shift_kc);
        [self.shift_kc, self.shift_r_kc, self.altgr_kc]
            .into_iter()
            .filter(|kc| *kc != 0 && self.down.contains(kc) && !required.contains(kc))
            .filter(|kc| !(shift_wanted && (*kc == self.shift_kc || *kc == self.shift_r_kc)))
            .collect()
    }

    fn inject(&mut self, kc: u32, state: u32) -> Result<(), String> {
        if state != 0 {
            self.down.insert(kc);
        } else {
            self.down.remove(&kc);
        }
        self.seat.inject_key(kc, state).map_err(|e| e.to_string())
    }

    /// The Shift/AltGr keycodes whose held state selects this keymap level.
    fn mods_for_level(&self, level: u32) -> Vec<u32> {
        let mut mods = Vec::new();
        if level & 1 != 0 {
            mods.push(self.shift_kc);
        }
        if level & 2 != 0 {
            mods.push(self.altgr_kc);
        }
        mods
    }

    /// Assign overlay keycodes to a batch of keysyms in one keymap swap.
    ///
    /// A swap costs milliseconds on the compositor thread (which also drives input and
    /// rendering), so binding a burst one at a time would stall it proportionally. Once the
    /// text spares are bound, the oldest bind on one not held down is recycled before any
    /// other keycode is taken, and a full pool recycles the oldest slot not held down:
    /// rebinding a pressed keycode would make its release report a different symbol than its
    /// press did. The swap rides the same command channel as the key events and never awaits a
    /// reply, so it drains before the keys that need it while this thread is never blocked on
    /// the compositor.
    ///
    /// Returns keysym to keycode for every requested keysym; a keysym that could not be bound
    /// (every slot held down) maps to 0.
    pub fn overlay_bind_many(&mut self, keysyms: &[u32]) -> Result<HashMap<u32, u32>, String> {
        let mut unique: Vec<u32> = Vec::new();
        for ks in keysyms {
            if !unique.contains(ks) {
                unique.push(*ks);
            }
        }
        // The batch's own binds are as untouchable as a held key: one swap binds them all, so a
        // recycled one would type the other keysym.
        let mut held: HashSet<u32> = self.pressed.values().map(|(kc, _)| *kc).collect();
        held.extend(unique.iter().filter_map(|k| self.overlay.get(k).copied()));
        let mut out = HashMap::new();
        let mut fresh = false;
        for keysym in unique {
            let kc = match self.overlay.get(&keysym) {
                Some(kc) => *kc,
                None => {
                    let mut victim = None;
                    if self.overlay.len() >= self.text_codes {
                        let text = &self.overlay_codes[..self.text_codes];
                        victim = self.overlay_order.iter().copied().find(|s| {
                            text.contains(&self.overlay[s]) && !held.contains(&self.overlay[s])
                        });
                        if victim.is_none() && self.overlay.len() >= self.overlay_codes.len() {
                            victim = self
                                .overlay_order
                                .iter()
                                .copied()
                                .find(|s| !held.contains(&self.overlay[s]));
                            if victim.is_none() {
                                out.insert(keysym, 0);
                                continue;
                            }
                        }
                    }
                    let kc = match victim {
                        Some(v) => {
                            self.overlay_order.retain(|s| *s != v);
                            self.overlay.remove(&v).unwrap_or(0)
                        }
                        None => self.overlay_codes[self.overlay.len()],
                    };
                    self.overlay.insert(keysym, kc);
                    self.overlay_order.push(keysym);
                    held.insert(kc);
                    fresh = true;
                    kc
                }
            };
            out.insert(keysym, kc);
        }
        if fresh {
            let binds: Vec<(u32, u32)> = self
                .overlay
                .iter()
                .map(|(sym, kc)| (*kc, overlay_bind_keysym(*sym)))
                .collect();
            self.seat
                .set_keymap_overlay(binds)
                .map_err(|e| e.to_string())?;
        }
        Ok(out)
    }

    /// Bind one keysym (`overlay_bind_many`).
    pub fn overlay_bind(&mut self, keysym: u32) -> Result<u32, String> {
        Ok(self
            .overlay_bind_many(&[keysym])?
            .get(&keysym)
            .copied()
            .unwrap_or(0))
    }

    /// Momentary press and release with refcounted modifier synthesis appended to `into`; a
    /// modifier the client itself holds down is left alone. A caller typing a run of characters
    /// hands the compositor one ordered batch, since an event at a time costs a channel send
    /// and a calloop wake each, on the thread that also renders.
    fn tap(&mut self, kc: u32, mods: &[u32], into: &mut Vec<(u32, u32)>) {
        let mut synthed = Vec::new();
        for m in mods {
            if self.down.contains(m) && !self.mod_refs.contains_key(m) {
                continue;
            }
            let refs = self.mod_refs.entry(*m).or_insert(0);
            *refs += 1;
            if *refs == 1 {
                into.push((*m, 1));
            }
            synthed.push(*m);
        }
        into.push((kc, 1));
        into.push((kc, 0));
        for m in synthed.iter().rev() {
            let refs = self.mod_refs.get(m).copied().unwrap_or(0).saturating_sub(1);
            if refs == 0 {
                self.mod_refs.remove(m);
                into.push((*m, 0));
            } else {
                self.mod_refs.insert(*m, refs);
            }
        }
    }

    /// Deliver an ordered run of `(keycode, state)` events as one batch.
    fn inject_run(&mut self, events: Vec<(u32, u32)>) -> Result<(), String> {
        if events.is_empty() {
            return Ok(());
        }
        for (kc, state) in &events {
            if *state != 0 {
                self.down.insert(*kc);
            } else {
                self.down.remove(kc);
            }
        }
        self.seat.inject_keys(events).map_err(|e| e.to_string())
    }

    /// Type text as momentary taps, with one keymap swap per run.
    ///
    /// Every missing keysym of a run resolves in a single swap (no per-character swap storm).
    /// A run holds as many missing keysyms as there are text spares not held down, so a longer
    /// text goes out in runs that fit them, each swap riding the channel behind the taps before
    /// it. Each character prefers its canonical layout keysym (a ru layout types ф on its own
    /// key) before falling to the overlay. With `neutralize`, conflicting held Shift/AltGr are
    /// lifted around the whole run so the taps land on their resolved levels.
    ///
    /// Returns false, having typed nothing, when a character cannot be bound at all (every
    /// overlay keycode is held down, which only the first run can meet); true once the full
    /// text is injected.
    pub fn type_text(&mut self, text: &str, neutralize: bool) -> Result<bool, String> {
        let mut keysyms = Vec::new();
        for ch in text.chars() {
            let mut ks = character_to_layout_keysym(ch);
            if !self.map.contains_key(&ks) {
                match universal_text_keysym(ch) {
                    Some(u) => ks = u,
                    None => continue,
                }
            }
            keysyms.push(ks);
        }
        let held: HashSet<u32> = self.pressed.values().map(|(kc, _)| *kc).collect();
        let room = self.overlay_codes[..self.text_codes]
            .iter()
            .filter(|kc| !held.contains(kc))
            .count();
        let mut runs: Vec<(Vec<u32>, Vec<u32>)> = vec![(Vec::new(), Vec::new())];
        for ks in keysyms {
            let needs_bind = !self.map.contains_key(&ks);
            let last = runs.len() - 1;
            if needs_bind && !runs[last].1.contains(&ks) {
                if room > 0 && runs[last].1.len() >= room {
                    runs.push((vec![ks], vec![ks]));
                    continue;
                }
                runs[last].1.push(ks);
            }
            runs[last].0.push(ks);
        }
        let mut lifted = Vec::new();
        let last = runs.len() - 1;
        for (i, (run, missing)) in runs.into_iter().enumerate() {
            let overlay = if missing.is_empty() {
                HashMap::new()
            } else {
                self.overlay_bind_many(&missing)?
            };
            // Resolve the run before touching state: the false path must have typed nothing
            // and charged no modifier refs.
            let mut resolved_keys = Vec::with_capacity(run.len());
            for ks in &run {
                let resolved = match self.map.get(ks) {
                    Some(p) => *p,
                    None => match overlay.get(ks) {
                        Some(kc) if *kc != 0 => (*kc, 0),
                        _ => return Ok(false),
                    },
                };
                resolved_keys.push(resolved);
            }
            if i == 0 && neutralize {
                // Lift conflicts before building the taps so `down` reflects the lift and a
                // shifted character inside the run synthesizes its Shift normally.
                lifted = self.held_conflicts(&[]);
                for kc in lifted.clone() {
                    self.inject(kc, 0)?;
                }
            }
            let mut events = Vec::new();
            for (kc, level) in resolved_keys {
                let mods = self.mods_for_level(level);
                self.tap(kc, &mods, &mut events);
            }
            if i == last {
                events.extend(lifted.iter().rev().map(|kc| (*kc, 1)));
            }
            self.inject_run(events)?;
        }
        Ok(true)
    }

    /// Press a keysym, overlay-binding it first when the base layout lacks it.
    ///
    /// Only modifiers not already down are synthesized; a modifier the client holds as its own
    /// key is neither charged nor released here. With `neutralize`, a conflicting held
    /// Shift/AltGr is lifted around the press and restored after: a client layout's Shift
    /// pairing rarely matches the seat layout's, so the held modifier would move the key onto a
    /// different glyph. Chords pass false so Ctrl+Shift+X passes through untouched.
    pub fn press(&mut self, keysym: u32, neutralize: bool) -> Result<(), String> {
        if let Some((kc, _)) = self.pressed.get(&keysym).cloned() {
            // Auto-repeat re-press: the first press already charged the refcounts.
            return self.inject(kc, 1);
        }
        let (kc, mods) = match self.map.get(&keysym) {
            Some((kc, level)) => (*kc, self.mods_for_level(*level)),
            None => {
                let kc = self.overlay_bind(keysym)?;
                if kc == 0 {
                    // Pool exhausted (every slot held down); keycode 0 is not a key.
                    return Ok(());
                }
                (kc, Vec::new())
            }
        };
        let lifted = if neutralize {
            self.held_conflicts(&mods)
        } else {
            Vec::new()
        };
        for m in &lifted {
            self.inject(*m, 0)?;
        }
        let mut synthed = Vec::new();
        for m in &mods {
            let already = self.down.contains(m)
                || (*m == self.shift_kc && self.down.contains(&self.shift_r_kc));
            if already && !self.mod_refs.contains_key(m) {
                continue;
            }
            let refs = self.mod_refs.entry(*m).or_insert(0);
            *refs += 1;
            if *refs == 1 {
                self.inject(*m, 1)?;
            }
            synthed.push(*m);
        }
        self.pressed.insert(keysym, (kc, synthed));
        self.inject(kc, 1)?;
        for m in lifted.iter().rev() {
            self.inject(*m, 1)?;
        }
        Ok(())
    }

    /// Release a pressed keysym and un-refcount the modifiers its press synthesized.
    pub fn release(&mut self, keysym: u32) -> Result<(), String> {
        let Some((kc, mods)) = self.pressed.remove(&keysym) else {
            return Ok(());
        };
        self.inject(kc, 0)?;
        for m in mods.iter().rev() {
            let refs = self.mod_refs.get(m).copied().unwrap_or(0).saturating_sub(1);
            if refs == 0 {
                self.mod_refs.remove(m);
                self.inject(*m, 0)?;
            } else {
                self.mod_refs.insert(*m, refs);
            }
        }
        Ok(())
    }

    /// Release every held key and synthetic modifier.
    pub fn reset(&mut self) {
        for keysym in self.pressed.keys().copied().collect::<Vec<_>>() {
            if self.release(keysym).is_err() {
                self.pressed.remove(&keysym);
            }
        }
        for m in self.mod_refs.keys().copied().collect::<Vec<_>>() {
            let _ = self.inject(m, 0);
        }
        self.mod_refs.clear();
        self.down.clear();
    }

    /// Carry what a previous owner left down across a base-layout change.
    ///
    /// Held keys and synthesized modifiers are physical keycodes the compositor still has
    /// pressed, valid under any base keymap, so the releases that follow the change must reach
    /// them: a fresh owner would drop them as no-ops and leave the keys held for good. Overlay
    /// binds are not carried; the compositor rebuilds its keymap from the new base without them.
    pub fn adopt_held(&mut self, previous: &WaylandKeymapOwner) {
        self.pressed
            .extend(previous.pressed.iter().map(|(k, v)| (*k, v.clone())));
        self.mod_refs
            .extend(previous.mod_refs.iter().map(|(k, v)| (*k, *v)));
        self.down.extend(previous.down.iter().copied());
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Stand-ins the Wayland suites share: a compiled layout's text and a recording seat.

    use std::sync::Mutex;

    use pixelflux::Error;
    use xkbcommon::xkb;

    use super::super::seat::WaylandSeat;

    /// The XKB text of `layout` on evdev/pc105 from libxkbcommon, or empty when the xkb data
    /// files are missing.
    pub fn keymap_text(layout: &str) -> String {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        match xkb::Keymap::new_from_names(
            &ctx,
            "evdev",
            "pc105",
            layout,
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        ) {
            Some(km) => km.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1),
            None => String::new(),
        }
    }

    /// One `set_xkb_layout` push: layout, variant, options, model, and rules.
    pub type Rmlvo = (String, String, String, String, String);

    /// Compositor keymap plus key channel: serves whichever base text is current and records
    /// every injected `(keycode, state)`.
    #[derive(Default)]
    pub struct FakeSeat {
        pub text: Mutex<String>,
        pub injected: Mutex<Vec<(u32, u32)>>,
        /// When each injected event went in, beside `injected`.
        pub stamps: Mutex<Vec<std::time::Instant>>,
        pub down: Mutex<Vec<u32>>,
        pub layouts: Mutex<Vec<Rmlvo>>,
        pub typed: Mutex<Vec<Vec<u32>>>,
        pub scrolls: Mutex<Vec<(f64, f64)>>,
        pub finger: Mutex<Vec<Option<(f64, f64)>>>,
        pub moves: Mutex<Vec<(&'static str, f64, f64)>>,
        pub buttons: Mutex<Vec<(u32, u32)>>,
        pub typer_unavailable: Mutex<bool>,
        pub location: Mutex<Option<(f64, f64, f64, u64)>>,
    }

    impl FakeSeat {
        pub fn new(text: &str) -> Self {
            FakeSeat {
                text: Mutex::new(text.to_string()),
                ..Default::default()
            }
        }

        pub fn set_text(&self, text: &str) {
            *self.text.lock().unwrap() = text.to_string();
        }

        pub fn injected(&self) -> Vec<(u32, u32)> {
            self.injected.lock().unwrap().clone()
        }

        pub fn clear(&self) {
            self.injected.lock().unwrap().clear();
            self.stamps.lock().unwrap().clear();
        }

        pub fn pressed(&self) -> Vec<u32> {
            let mut d = self.down.lock().unwrap().clone();
            d.sort_unstable();
            d
        }
    }

    impl WaylandSeat for FakeSeat {
        fn inject_key(&self, kc: u32, state: u32) -> Result<(), Error> {
            self.injected.lock().unwrap().push((kc, state));
            self.stamps.lock().unwrap().push(std::time::Instant::now());
            let mut down = self.down.lock().unwrap();
            if state != 0 {
                if !down.contains(&kc) {
                    down.push(kc);
                }
            } else {
                down.retain(|k| *k != kc);
            }
            Ok(())
        }

        fn inject_keys(&self, events: Vec<(u32, u32)>) -> Result<(), Error> {
            for (kc, state) in events {
                self.inject_key(kc, state)?;
            }
            Ok(())
        }

        fn set_keymap_overlay(&self, _binds: Vec<(u32, u32)>) -> Result<(), Error> {
            Ok(())
        }

        fn get_xkb_keymap_string(&self) -> Result<String, Error> {
            Ok(self.text.lock().unwrap().clone())
        }

        fn set_xkb_layout(
            &self,
            l: String,
            v: String,
            o: String,
            m: String,
            r: String,
        ) -> Result<bool, Error> {
            self.layouts.lock().unwrap().push((l, v, o, m, r));
            Ok(true)
        }

        fn get_keyboard_state(&self) -> Result<(Vec<u32>, u32), Error> {
            Ok((self.pressed(), 0))
        }

        fn type_keysyms_wayland(&self, _display: String, keysyms: Vec<u32>) -> Result<(), Error> {
            if *self.typer_unavailable.lock().unwrap() {
                return Err(Error::Unavailable(
                    "zwp_virtual_keyboard_manager_v1 missing".into(),
                ));
            }
            self.typed.lock().unwrap().push(keysyms);
            Ok(())
        }

        fn inject_mouse_move(&self, x: f64, y: f64) -> Result<Option<u64>, Error> {
            self.moves.lock().unwrap().push(("absolute", x, y));
            Ok(None)
        }

        fn inject_relative_mouse_move(&self, dx: f64, dy: f64) -> Result<Option<u64>, Error> {
            self.moves.lock().unwrap().push(("relative", dx, dy));
            Ok(None)
        }

        fn inject_mouse_button(&self, button: u32, state: u32) -> Result<(), Error> {
            self.buttons.lock().unwrap().push((button, state));
            Ok(())
        }

        fn inject_mouse_scroll(&self, x: f64, y: f64) -> Result<(), Error> {
            self.scrolls.lock().unwrap().push((x, y));
            Ok(())
        }

        fn inject_finger_scroll(&self, dx: f64, dy: f64) -> Result<(), Error> {
            self.finger.lock().unwrap().push(Some((dx, dy)));
            Ok(())
        }

        fn inject_finger_scroll_end(&self) -> Result<(), Error> {
            self.finger.lock().unwrap().push(None);
            Ok(())
        }

        fn pointer_location(&self) -> Option<(f64, f64, f64, u64)> {
            *self.location.lock().unwrap()
        }

        fn set_app_wayland_display(&self, _display: String) {}

        fn clipboard_unwatch_app(&self, _display: String) {}
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{FakeSeat, keymap_text};
    use super::*;

    #[test]
    fn overlay_binds_take_text_spares_first_and_recycle_them() {
        let us = keymap_text("us");
        if us.is_empty() {
            eprintln!("skipped: xkb data files unavailable");
            return;
        }
        let seat = Arc::new(FakeSeat::new(&us));
        let mut owner = WaylandKeymapOwner::new(seat.clone(), &us).unwrap();
        assert!(owner.resolves(0x61));
        assert!(!owner.resolves(0x0100_D55C));
        let text = owner.text_codes;
        assert!(text > 0, "a pc105 us keymap leaves F13-F24 free");
        let first: Vec<u32> = (0..text as u32)
            .map(|i| owner.overlay_bind(0x0100_AC00 + i).unwrap())
            .collect();
        assert_eq!(first, owner.overlay_codes[..text].to_vec());
        let next = owner.overlay_bind(0x0100_AC00 + text as u32).unwrap();
        assert_eq!(
            next, first[0],
            "the oldest text spare is recycled before any other keycode"
        );
        assert!(owner.type_text("한국어 입력 abc", false).unwrap());
        let typed = seat.injected();
        assert!(typed.iter().filter(|(_, s)| *s == 1).count() >= 10);
    }
}
