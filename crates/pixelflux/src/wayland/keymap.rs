/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Compositor-side keymap policy: one owner for the seat keymap.
//!
//! The seat keymap is BASE text (US by default, replaceable at runtime with a full
//! XKB_KEYMAP_FORMAT_TEXT_V1 string or RMLVO names) plus an OVERLAY of spare keycodes bound to
//! keysyms the base cannot produce (Unicode / IME output). All rebinds are batched: resolving N
//! new keysyms produces ONE keymap swap, and a keycode that is currently held down is never
//! recycled, so its release event always means the symbol its press meant.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;

use smithay::input::keyboard::xkb;

/// First overlay keycode. Sits above both the evdev/pc105 range and the legacy selkies
/// overlay range (257-272) so a base keymap carrying those legacy binds cannot collide.
pub const OVERLAY_FIRST_KEYCODE: u32 = 0x120;
/// Last overlay keycode (inclusive). Keycodes past the X11 255 ceiling are fine for
/// pure-Wayland clients: they look keycodes up in the delivered keymap via xkbcommon.
pub const OVERLAY_LAST_KEYCODE: u32 = 0x2ff;
/// Overlay slot count (keycodes `OVERLAY_FIRST..=OVERLAY_LAST`).
pub const OVERLAY_CAPACITY: usize = (OVERLAY_LAST_KEYCODE - OVERLAY_FIRST_KEYCODE + 1) as usize;

/// Keycodes every client types a character on, the seat's overlay takes first: F13 to F24,
/// then the JIS IntlRo and IntlYen keys, as selkies' own overlay does. A Wayland client may
/// read a key's code as the physical key it names before the keysym the keymap gives, and
/// Chromium drops a code naming no key it knows (every one past 255) and runs its browser,
/// media and launcher keys as commands, so Computer-Use text bound from 0x120 never reached
/// it. A text spare is taken only where the base leaves it unbound or bound to vendor keysyms
/// alone, and not while selkies' overlay holds it.
pub const TEXT_SPARE_KEYCODES: [u32; 14] = [
    191, 192, 193, 194, 195, 196, 197, 198, 199, 200, 201, 202, 97, 132,
];

/// The XF86 vendor keysyms: a base key carrying only these is free to shadow.
const VENDOR_KEYSYMS: std::ops::RangeInclusive<u32> = 0x1008_0000..=0x1008_ffff;

/// Highest shift level consulted when reverse-mapping the base keymap (plain, Shift,
/// AltGr, Shift+AltGr).
const MAX_LEVELS: u32 = 4;

/// Seat keymap state: the base text, its reverse keysym map, and the overlay slots.
pub struct KeymapPolicy {
    base_text: String,
    /// keysym -> (xkb keycode, level) in the base keymap; lowest level wins.
    base_map: HashMap<u32, (u32, u32)>,
    /// slot index -> bound keysym.
    slots: Vec<Option<u32>>,
    /// keysym -> slot index.
    by_sym: HashMap<u32, usize>,
    /// Slot recycle order, oldest bind first.
    lru: VecDeque<usize>,
    /// Overlay keycodes (xkb numbering) in the order free slots take them; slot i lives
    /// at `overlay_codes[i]`.
    overlay_codes: Vec<u32>,
    /// How many of the leading overlay codes a bind prefers, recycling the oldest of them
    /// before taking a free code past them: the seat's text spares, which Chromium types on.
    preferred: usize,
    /// The text spares the base binds to a keysym other than a vendor one, which an overlay
    /// slot must not shadow. (The virtual-keyboard client's own codes are base character
    /// keys it shadows on purpose, `unshadow` taking them back.)
    base_bound: HashSet<u32>,
    /// Externally-owned overlay binds (keycode -> keysym): selkies resolves its own
    /// keysyms and hands the compositor explicit assignments. Held here so every keymap
    /// the policy emits carries them, and a policy rebuild (computer-use bind, base-layout
    /// swap) keeps them live instead of dropping them until selkies re-sends.
    manual_overlay: BTreeMap<u32, u32>,
}

/// Keysym one literal character types as: Latin-1 printables map 1:1, `\n` types
/// Return (the raw utf32 table maps it to Linefeed, which no keymap binds), other
/// control characters their `0xffXX` function keysyms, everything else the
/// `0x01000000 | codepoint` Unicode form (0 = unmappable). The keysym then resolves
/// against an ACTIVE keymap, never a hardcoded layout table.
pub fn keysym_for_char(c: char) -> u32 {
    let c = if c == '\n' { '\r' } else { c };
    xkb::utf32_to_keysym(c as u32).raw()
}

/// Compile an XKB_KEYMAP_FORMAT_TEXT_V1 string, or `None` when it does not compile.
pub fn compile_keymap(text: &str) -> Option<xkb::Keymap> {
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    xkb::Keymap::new_from_string(
        &ctx,
        text.to_string(),
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
}

/// Compile RMLVO names to keymap text, or `None` when compilation fails. Empty strings
/// select the xkbcommon defaults for that component.
pub fn compile_rmlvo(
    rules: &str,
    model: &str,
    layout: &str,
    variant: &str,
    options: &str,
) -> Option<String> {
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let options = (!options.is_empty()).then(|| options.to_string());
    let keymap = xkb::Keymap::new_from_names(
        &ctx,
        rules,
        model,
        layout,
        variant,
        options,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )?;
    Some(keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
}

/// Level-0 keysym for every key of a compiled keymap (used to pre-bind a virtual-keyboard
/// client's keymap in one batch). Keys with zero or multiple level-0 syms are skipped.
pub fn level0_syms(keymap: &xkb::Keymap) -> HashMap<u32, u32> {
    let mut out = HashMap::new();
    let lo = keymap.min_keycode().raw();
    let hi = keymap.max_keycode().raw();
    for kc in lo..=hi {
        let syms = keymap.key_get_syms_by_level(xkb::Keycode::new(kc), 0, 0);
        if syms.len() == 1 {
            let sym = syms[0].raw();
            if sym != 0 {
                out.insert(kc, sym);
            }
        }
    }
    out
}

impl KeymapPolicy {
    /// Placeholder policy before the seat keymap is known; `rebuild_base` fills it in.
    pub fn empty() -> Self {
        Self::with_overlay_codes((OVERLAY_FIRST_KEYCODE..=OVERLAY_LAST_KEYCODE).collect())
    }

    /// The compositor seat's policy: the text spares (`TEXT_SPARE_KEYCODES`) first, then the
    /// above-255 range.
    pub fn seat() -> Self {
        let mut policy = Self::with_overlay_codes(
            TEXT_SPARE_KEYCODES
                .into_iter()
                .chain(OVERLAY_FIRST_KEYCODE..=OVERLAY_LAST_KEYCODE)
                .collect(),
        );
        policy.preferred = TEXT_SPARE_KEYCODES.len();
        policy
    }

    /// Policy with its own overlay keycodes (xkb numbering), taken in order. The seat
    /// uses `empty()`'s above-255 range; the virtual-keyboard client typing into a
    /// nested compositor uses the main block's character keys, which a base character
    /// the batch types takes back (`unshadow`).
    pub fn with_overlay_codes(overlay_codes: Vec<u32>) -> Self {
        Self {
            base_text: String::new(),
            base_map: HashMap::new(),
            slots: Vec::new(),
            by_sym: HashMap::new(),
            lru: VecDeque::new(),
            overlay_codes,
            preferred: 0,
            base_bound: HashSet::new(),
            manual_overlay: BTreeMap::new(),
        }
    }

    /// Replace the base keymap text and rebuild the reverse map. Overlay assignments are
    /// retained (same keycodes), so keycodes already handed out stay valid across the swap.
    /// Returns whether `base_text` compiled. A string that does not is rejected without
    /// touching any state, so the caller needs no separate validation pass — compiling a
    /// keymap is the expensive part of installing one and doing it twice is pure cost.
    pub fn rebuild_base(&mut self, base_text: String) -> bool {
        let Some(keymap) = compile_keymap(&base_text) else {
            return false;
        };
        self.base_map.clear();
        self.base_bound.clear();
        {
            let lo = keymap.min_keycode().raw();
            let hi = keymap.max_keycode().raw();
            // Lower levels win across ALL keys, so a keysym reachable unshifted never
            // resolves to a shifted position.
            for level in 0..MAX_LEVELS {
                for kc in lo..=hi {
                    let code = xkb::Keycode::new(kc);
                    if keymap.num_levels_for_key(code, 0) <= level {
                        continue;
                    }
                    for sym in keymap.key_get_syms_by_level(code, 0, level) {
                        let raw = sym.raw();
                        if raw != 0 {
                            self.base_map.entry(raw).or_insert((kc, level));
                            if TEXT_SPARE_KEYCODES.contains(&kc) && !VENDOR_KEYSYMS.contains(&raw) {
                                self.base_bound.insert(kc);
                            }
                        }
                    }
                }
            }
        }
        self.base_text = base_text;
        true
    }

    /// True once a base keymap has been installed.
    pub fn has_base(&self) -> bool {
        !self.base_text.is_empty()
    }

    /// Resolve `keysym` without binding: base first, then an existing overlay slot.
    pub fn resolve(&self, keysym: u32) -> Option<(u32, u32)> {
        if let Some(&hit) = self.base_map.get(&keysym)
            && self.slot_at(hit.0).is_none()
        {
            return Some(hit);
        }
        self.by_sym
            .get(&keysym)
            .map(|&slot| (self.overlay_codes[slot], 0))
    }

    /// The occupied overlay slot on `keycode`, whose keysym shadows the base's there.
    fn slot_at(&self, keycode: u32) -> Option<usize> {
        let slot = self.overlay_codes.iter().position(|&kc| kc == keycode)?;
        self.slots.get(slot)?.map(|_| slot)
    }

    /// Unbind every overlay slot shadowing a base keycode the batch presses, unless it
    /// is held, and return those keycodes so the batch's own binds skip them.
    fn unshadow(
        &mut self,
        keysyms: &[u32],
        pressed: &HashSet<u32>,
        plain_only: bool,
        changed: &mut bool,
    ) -> HashSet<u32> {
        let mut reserved = HashSet::new();
        for sym in keysyms {
            let Some(&(kc, level)) = self.base_map.get(sym) else {
                continue;
            };
            if (plain_only && level != 0) || !reserved.insert(kc) || pressed.contains(&kc) {
                continue;
            }
            if let Some(slot) = self.slot_at(kc)
                && let Some(old) = self.slots[slot].take()
            {
                self.by_sym.remove(&old);
                self.lru.retain(|&s| s != slot);
                *changed = true;
            }
        }
        reserved
    }

    /// True when `keysym` resolves at level 0 (base or overlay) — i.e. typable without
    /// synthetic modifiers.
    pub fn resolves_plain(&self, keysym: u32) -> bool {
        matches!(self.resolve(keysym), Some((_, 0)))
    }

    /// Resolve every keysym, overlay-binding the unresolvable ones. Returns one
    /// `(keycode, level)` per input keysym (`(0, 0)` when it cannot be bound) plus whether the
    /// keymap changed and must be re-applied — at most ONE swap per call, however many new
    /// keysyms were bound. Slots whose keycode is in `pressed` are never recycled.
    pub fn bind_many(
        &mut self,
        keysyms: &[u32],
        pressed: &HashSet<u32>,
    ) -> (Vec<(u32, u32)>, bool) {
        let mut out = Vec::with_capacity(keysyms.len());
        let mut changed = false;
        let mut reserved = self.unshadow(keysyms, pressed, false, &mut changed);
        for &sym in keysyms {
            let hit = self.bind_one(sym, pressed, &reserved, false, &mut changed);
            // A later keysym of the batch must not recycle the code this one now types on.
            reserved.insert(hit.0);
            out.push(hit);
        }
        (out, changed)
    }

    /// Like `bind_many` but only accepts level-0 resolutions: a keysym reachable in the
    /// base solely behind a modifier (e.g. `A` behind Shift) is overlay-bound instead, so the
    /// caller can inject it without synthesizing modifiers. Returns keycodes (0 = unbindable).
    pub fn bind_many_plain(&mut self, keysyms: &[u32], pressed: &HashSet<u32>) -> (Vec<u32>, bool) {
        let mut out = Vec::with_capacity(keysyms.len());
        let mut changed = false;
        let mut reserved = self.unshadow(keysyms, pressed, true, &mut changed);
        for &sym in keysyms {
            let kc = self.bind_one(sym, pressed, &reserved, true, &mut changed).0;
            reserved.insert(kc);
            out.push(kc);
        }
        (out, changed)
    }

    fn bind_one(
        &mut self,
        sym: u32,
        pressed: &HashSet<u32>,
        reserved: &HashSet<u32>,
        plain_only: bool,
        changed: &mut bool,
    ) -> (u32, u32) {
        if sym == 0 {
            return (0, 0);
        }
        if let Some(&(kc, level)) = self.base_map.get(&sym)
            && (!plain_only || level == 0)
            && self.slot_at(kc).is_none()
        {
            return (kc, level);
        }
        if let Some(&slot) = self.by_sym.get(&sym) {
            if let Some(at) = self.lru.iter().position(|&s| s == slot) {
                self.lru.remove(at);
            }
            self.lru.push_back(slot);
            return (self.overlay_codes[slot], 0);
        }
        let free_in = |me: &Self, range: std::ops::Range<usize>| {
            range.into_iter().find(|&i| {
                me.slots.get(i).is_none_or(Option::is_none)
                    && me.takes(me.overlay_codes[i], reserved)
            })
        };
        let preferred = self.preferred.min(self.overlay_codes.len());
        let slot = free_in(self, 0..preferred)
            .or_else(|| self.recycle_slot(pressed, reserved, preferred))
            .or_else(|| free_in(self, preferred..self.overlay_codes.len()))
            .or_else(|| self.recycle_slot(pressed, reserved, usize::MAX));
        let Some(slot) = slot else {
            return (0, 0);
        };
        if slot >= self.slots.len() {
            self.slots.resize(slot + 1, None);
        }
        if let Some(old) = self.slots[slot].replace(sym) {
            self.by_sym.remove(&old);
        }
        self.by_sym.insert(sym, slot);
        self.lru.push_back(slot);
        *changed = true;
        (self.overlay_codes[slot], 0)
    }

    /// Oldest slot below `below` whose keycode is neither held down nor pressed for its base
    /// keysym by this batch; a held keycode must keep its meaning until its release has been
    /// delivered.
    fn recycle_slot(
        &mut self,
        pressed: &HashSet<u32>,
        reserved: &HashSet<u32>,
        below: usize,
    ) -> Option<usize> {
        let at = self.lru.iter().position(|&slot| {
            let kc = self.overlay_codes[slot];
            slot < below && !pressed.contains(&kc) && self.takes(kc, reserved)
        })?;
        self.lru.remove(at)
    }

    /// Whether a slot may hold `keycode`: not one this batch presses for its base keysym,
    /// not a text spare the base binds to a real keysym, and not one selkies' overlay holds.
    fn takes(&self, keycode: u32, reserved: &HashSet<u32>) -> bool {
        !reserved.contains(&keycode)
            && !self.base_bound.contains(&keycode)
            && !self.manual_overlay.contains_key(&keycode)
    }

    /// Replace the externally-owned overlay binds (keycode -> keysym). selkies resolves its
    /// own keysyms and re-sends the whole set on every change, so a full replace is the
    /// contract. They then ride along in every `keymap_text`, so a base-layout swap or a
    /// computer-use policy bind re-applies them instead of dropping them.
    /// A slot on a keycode selkies now binds gives it up, so its keysym is bound afresh
    /// elsewhere when next typed.
    pub fn set_manual_overlay(&mut self, binds: &[(u32, u32)]) {
        self.manual_overlay = binds.iter().copied().collect();
        for slot in 0..self.slots.len() {
            if self.manual_overlay.contains_key(&self.overlay_codes[slot])
                && let Some(old) = self.slots[slot].take()
            {
                self.by_sym.remove(&old);
                self.lru.retain(|&s| s != slot);
            }
        }
    }

    /// The full seat keymap: the base text with every occupied policy overlay slot and every
    /// externally-owned bind spliced into the `xkb_keycodes` and `xkb_symbols` sections (and
    /// `maximum` raised to cover them). With neither, the base text verbatim. A manual bind
    /// sharing a keycode with a policy slot is emitted last, so its symbol wins.
    pub fn keymap_text(&self) -> String {
        let occupied: Vec<(usize, u32)> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.map(|sym| (i, sym)))
            .collect();
        if occupied.is_empty() && self.manual_overlay.is_empty() {
            return self.base_text.clone();
        }
        let base = &self.base_text;
        let Some(max_at) = base.find("maximum = ") else {
            return self.base_text.clone();
        };
        let num_at = max_at + "maximum = ".len();
        let Some(num_len) = base[num_at..].find(';') else {
            return self.base_text.clone();
        };
        let old_max: u32 = base[num_at..num_at + num_len].trim().parse().unwrap_or(255);
        let slot_max = occupied
            .iter()
            .map(|&(i, _)| self.overlay_codes[i])
            .max()
            .unwrap_or(0);
        let manual_max = self.manual_overlay.keys().copied().max().unwrap_or(0);
        let need_max = slot_max.max(manual_max);
        let mut text =
            String::with_capacity(base.len() + (occupied.len() + self.manual_overlay.len()) * 48);
        text.push_str(&base[..num_at]);
        text.push_str(&old_max.max(need_max).to_string());
        let rest = &base[num_at + num_len..];
        // First "};" after the maximum line closes xkb_keycodes.
        let Some(kc_end) = rest.find("};") else {
            return self.base_text.clone();
        };
        text.push_str(&rest[..kc_end]);
        for &(i, _) in &occupied {
            let _ = writeln!(text, "\t<P{:03}> = {};", i, self.overlay_codes[i]);
        }
        for &kc in self.manual_overlay.keys() {
            let _ = writeln!(text, "\t<X{kc:03}> = {kc};");
        }
        let rest = &rest[kc_end..];
        let Some(close_at) = rest
            .find("xkb_symbols")
            .and_then(|sym_at| Self::section_close(rest, sym_at))
        else {
            return self.base_text.clone();
        };
        text.push_str(&rest[..close_at]);
        for &(i, sym) in &occupied {
            let _ = writeln!(text, "\tkey <P{:03}> {{ [ {:#x} ] }};", i, sym);
        }
        for (&kc, &sym) in &self.manual_overlay {
            let _ = writeln!(text, "\tkey <X{kc:03}> {{ [ {sym:#x} ] }};");
        }
        text.push_str(&rest[close_at..]);
        text
    }

    /// Byte offset of the `}` closing the brace-block that starts at/after `from`.
    fn section_close(text: &str, from: usize) -> Option<usize> {
        let open = from + text[from..].find('{')?;
        let mut depth = 0usize;
        for (i, ch) in text[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(open + i);
                    }
                }
                _ => {}
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    //! Invariants: one `bind_many` call binds any number of new keysyms with a single
    //! keymap change; base keysyms resolve without consuming overlay slots; a pressed
    //! overlay keycode survives LRU pressure; the spliced keymap text compiles and
    //! resolves the overlay keysyms at their assigned keycodes.
    use super::*;

    fn us_base() -> String {
        compile_rmlvo("", "", "us", "", "").expect("us keymap")
    }

    fn policy() -> KeymapPolicy {
        let mut p = KeymapPolicy::empty();
        p.rebuild_base(us_base());
        p
    }

    #[test]
    fn base_keysyms_resolve_without_overlay() {
        let mut p = policy();
        // 'a' plain, 'A' shifted.
        let (out, changed) = p.bind_many(&[0x61, 0x41], &HashSet::new());
        assert!(!changed);
        assert_eq!(out[0].1, 0);
        assert_eq!(out[1].0, out[0].0);
        assert_eq!(out[1].1, 1);
    }

    #[test]
    fn batch_bind_is_one_swap_and_compiles() {
        let mut p = policy();
        let syms: Vec<u32> = (0..30).map(|i| 0x1004E00 + i).collect();
        let (out, changed) = p.bind_many(&syms, &HashSet::new());
        assert!(changed);
        let (_, changed_again) = p.bind_many(&syms, &HashSet::new());
        assert!(!changed_again, "re-binding bound keysyms must not swap");
        let text = p.keymap_text();
        let km = compile_keymap(&text).expect("overlay keymap compiles");
        for (i, &(kc, level)) in out.iter().enumerate() {
            assert_eq!(level, 0);
            let got = km.key_get_syms_by_level(xkb::Keycode::new(kc), 0, 0);
            assert_eq!(got.len(), 1, "keycode {kc} has one sym");
            assert_eq!(got[0].raw(), syms[i]);
        }
    }

    #[test]
    fn pressed_keycode_is_never_recycled() {
        let mut p = policy();
        let syms: Vec<u32> = (0..OVERLAY_CAPACITY as u32)
            .map(|i| 0x1005000 + i)
            .collect();
        let (out, _) = p.bind_many(&syms, &HashSet::new());
        let held_kc = out[0].0;
        let held_sym = syms[0];
        let pressed: HashSet<u32> = [held_kc].into_iter().collect();
        // Force full recycling pressure past capacity.
        let extra: Vec<u32> = (0..8).map(|i| 0x1006000 + i).collect();
        let (extra_out, changed) = p.bind_many(&extra, &pressed);
        assert!(changed);
        for &(kc, _) in &extra_out {
            assert_ne!(kc, held_kc, "held keycode must not be rebound");
        }
        assert_eq!(p.resolve(held_sym), Some((held_kc, 0)));
    }

    /// The seat binds Computer-Use text on the keys every client types on (`TEXT_SPARE_KEYCODES`)
    /// before any above 255, which Chromium drops: on each spare the US base leaves free, the
    /// spliced keymap carries the bound keysym, and only past them does a bind go above 255.
    #[test]
    fn the_seat_binds_text_on_the_text_spares_first() {
        let mut p = KeymapPolicy::seat();
        p.rebuild_base(us_base());
        let free: Vec<u32> = TEXT_SPARE_KEYCODES
            .into_iter()
            .filter(|kc| !p.base_bound.contains(kc))
            .collect();
        assert!(
            free.len() >= 10,
            "the US base leaves most text spares free: {free:?}"
        );
        let syms: Vec<u32> = (0..free.len() as u32 + 1).map(|i| 0x100AC00 + i).collect();
        let (out, changed) = p.bind_many(&syms, &HashSet::new());
        assert!(changed);
        let codes: Vec<u32> = out.iter().map(|&(kc, _)| kc).collect();
        assert_eq!(codes[..free.len()], free[..], "the text spares, in order");
        assert_eq!(codes[free.len()], OVERLAY_FIRST_KEYCODE, "then above 255");
        let km = compile_keymap(&p.keymap_text()).expect("seat keymap compiles");
        for (&kc, &sym) in codes.iter().zip(&syms) {
            let got = km.key_get_syms_by_level(xkb::Keycode::new(kc), 0, 0);
            assert_eq!(
                got.iter().map(|s| s.raw()).collect::<Vec<_>>(),
                vec![sym],
                "keycode {kc}"
            );
        }
    }

    /// A text spare selkies' overlay binds is selkies': the seat's slot there gives it up, its
    /// keysym binds afresh on another, and no seat bind takes it while selkies holds it.
    #[test]
    fn the_seat_gives_way_to_selkies_on_a_text_spare() {
        let mut p = KeymapPolicy::seat();
        p.rebuild_base(us_base());
        let (out, _) = p.bind_many(&[0x100AC00], &HashSet::new());
        let first = out[0].0;
        assert_eq!(first, TEXT_SPARE_KEYCODES[0]);
        p.set_manual_overlay(&[(first, 0x100D55C)]);
        assert_eq!(p.resolve(0x100AC00), None, "the slot gave its keycode up");
        let (out, changed) = p.bind_many(&[0x100AC00, 0x100AC01], &HashSet::new());
        assert!(changed);
        assert!(out.iter().all(|&(kc, _)| kc != first), "{out:?}");
        let km = compile_keymap(&p.keymap_text()).expect("shared keymap compiles");
        let at = |kc: u32| km.key_get_syms_by_level(xkb::Keycode::new(kc), 0, 0)[0].raw();
        assert_eq!(at(first), 0x100D55C);
        assert_eq!(at(out[0].0), 0x100AC00);
    }

    /// With the text spares full the seat recycles the oldest of them before it takes a code
    /// above 255, and never one its own batch has just bound.
    #[test]
    fn the_seat_recycles_a_text_spare_before_going_above_255() {
        let mut p = KeymapPolicy::seat();
        p.rebuild_base(us_base());
        let spares = TEXT_SPARE_KEYCODES
            .into_iter()
            .filter(|kc| !p.base_bound.contains(kc))
            .count();
        let first: Vec<u32> = (0..spares as u32).map(|i| 0x100AC00 + i).collect();
        let (out, _) = p.bind_many(&first, &HashSet::new());
        let (next, _) = p.bind_many(&[0x100B000, 0x100B001], &HashSet::new());
        assert_eq!(next[0].0, out[0].0, "the oldest text spare");
        assert_eq!(next[1].0, out[1].0, "then the next oldest");
        let batch: Vec<u32> = (0..spares as u32 + 1).map(|i| 0x100C000 + i).collect();
        let (out, _) = p.bind_many(&batch, &HashSet::new());
        let codes: HashSet<u32> = out.iter().map(|&(kc, _)| kc).collect();
        assert_eq!(
            codes.len(),
            batch.len(),
            "every keysym of a batch keeps its own code: {out:?}"
        );
    }

    #[test]
    fn character_key_overlay_wins_its_keys_and_gives_them_back() {
        // The virtual-keyboard client overlays the main block's character keys, which the
        // base already names (<AE01>...): the spliced definitions must win so overlay
        // keysyms resolve at their assigned keycodes, and a base character a batch types
        // takes its own key back.
        fn syms_at(km: &xkb::Keymap, kc: u32) -> Vec<u32> {
            km.key_get_syms_by_level(xkb::Keycode::new(kc), 0, 0)
                .iter()
                .map(|s| s.raw())
                .collect()
        }
        let mut p = KeymapPolicy::with_overlay_codes(vec![10, 11, 38]);
        p.rebuild_base(us_base());
        let (out, changed) = p.bind_many_plain(&[0x1004E2D, 0x61], &HashSet::new());
        assert!(changed);
        assert_eq!(out, vec![10, 38], "the overlay skips the key 'a' types on");
        let km = compile_keymap(&p.keymap_text()).expect("overlay keymap compiles");
        assert_eq!(syms_at(&km, 10), vec![0x1004E2D]);
        assert_eq!(syms_at(&km, 38), vec![0x61]);
        let (out, changed) = p.bind_many_plain(&[0x31, 0x1004E2D], &HashSet::new());
        assert!(changed);
        assert_eq!(
            out,
            vec![10, 11],
            "'1' takes its key back and the overlay moves on"
        );
        let km = compile_keymap(&p.keymap_text()).expect("given-back keymap compiles");
        assert_eq!(syms_at(&km, 10), vec![0x31]);
        assert_eq!(syms_at(&km, 11), vec![0x1004E2D]);
        assert_eq!(p.resolve(0x31), Some((10, 0)));
    }

    #[test]
    fn manual_overlay_survives_policy_rebind_and_layout_swap() {
        // selkies' explicit binds must keep resolving after a computer-use policy bind
        // (which re-applies keymap_text) and after a base-layout swap.
        let mut p = policy();
        // Two selkies-owned keycodes carrying emoji keysyms.
        let manual = [(220u32, 0x0101_F600u32), (221u32, 0x0101_F601u32)];
        p.set_manual_overlay(&manual);
        // A computer-use batch binds its own keysyms through the policy pool.
        let (_out, changed) = p.bind_many(&[0x1004E00, 0x1004E01], &HashSet::new());
        assert!(changed);
        let km = compile_keymap(&p.keymap_text()).expect("merged keymap compiles");
        for &(kc, sym) in &manual {
            let got = km.key_get_syms_by_level(xkb::Keycode::new(kc), 0, 0);
            assert_eq!(got.len(), 1, "manual keycode {kc} has one sym");
            assert_eq!(got[0].raw(), sym, "manual keycode {kc} keeps its keysym");
        }
        // A base-layout swap must not drop the manual binds either.
        let de = compile_rmlvo("", "", "de", "", "").expect("de keymap");
        assert!(p.rebuild_base(de));
        let km = compile_keymap(&p.keymap_text()).expect("post-swap keymap compiles");
        for &(kc, sym) in &manual {
            let got = km.key_get_syms_by_level(xkb::Keycode::new(kc), 0, 0);
            assert_eq!(got.first().map(|s| s.raw()), Some(sym));
        }
        // Clearing them removes the binds (the emoji keysym no longer resolves).
        p.set_manual_overlay(&[]);
        let km = compile_keymap(&p.keymap_text()).expect("cleared keymap compiles");
        let got = km.key_get_syms_by_level(xkb::Keycode::new(220), 0, 0);
        assert!(got.iter().all(|s| s.raw() != manual[0].1));
    }

    #[test]
    fn rebuild_base_keeps_overlay_assignments() {
        let mut p = policy();
        let (out, _) = p.bind_many(&[0x1004E2D], &HashSet::new());
        let de = compile_rmlvo("", "", "de", "", "").expect("de keymap");
        p.rebuild_base(de);
        assert_eq!(p.resolve(0x1004E2D), Some((out[0].0, 0)));
        // udiaeresis resolves in the German base without an overlay.
        let (u_out, changed) = p.bind_many(&[0xFC], &HashSet::new());
        assert!(!changed);
        assert_eq!(u_out[0].1, 0);
        assert!(compile_keymap(&p.keymap_text()).is_some());
    }

    /// What a typed character asks for: Latin-1 printables keep their legacy keysyms
    /// (the form a keymap names its keys by, so they resolve without an overlay slot),
    /// `\n` types Return rather than the Linefeed the raw table gives, other control
    /// characters their function keysyms, and everything else the Unicode form.
    #[test]
    fn typed_characters_map_to_the_keysyms_a_keymap_names() {
        assert_eq!(keysym_for_char('a'), 0x61);
        assert_eq!(keysym_for_char('~'), 0x7E);
        assert_eq!(keysym_for_char('\u{FC}'), 0xFC);
        assert_eq!(keysym_for_char('\n'), 0xFF0D);
        assert_eq!(keysym_for_char('\r'), 0xFF0D);
        assert_eq!(keysym_for_char('\t'), 0xFF09);
        assert_eq!(keysym_for_char('\u{1B}'), 0xFF1B);
        assert_eq!(keysym_for_char('\u{4E2D}'), 0x0100_4E2D);
        assert_eq!(keysym_for_char('\u{1F600}'), 0x0101_F600);
    }
}
