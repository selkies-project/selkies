/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The X11 keyboard injector over XTEST.
//!
//! Keysyms the layout lacks (Unicode, exotic symbols) bind on demand to spare keycodes, so they
//! inject in-process through XTEST instead of forking xdotool. The bindings are round-robin
//! recycled; the reverse of TigerVNC's `XkbAddKeyKeysym`, done through core
//! `ChangeKeyboardMapping`.
//!
//! On a multi-group layout (`setxkbmap us,ru`) a keysym of a later group sits on the same
//! keycode as its group-1 glyph, and the server translates an injected keycode under its
//! current group, so the keysym's group is locked around the injection and the previous lock
//! restored once the last key that needed it is up. The physical keycode is kept (`Cyrillic_ef`
//! presses the 'a' key, as a Russian keyboard does), so scancode-driven clients see the key they
//! expect. Keysyms group 1 carries never switch: the server's own group then decides, exactly as
//! for a single-group layout.
//!
//! Everything here runs on the X worker thread: the settle a bind owes clients is slept out
//! there, never on the runtime.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use log::debug;

use super::keysyms::{
    KEYSYM_ISO_LEVEL3_SHIFT, KEYSYM_LEFT_ARROW, KEYSYM_MODE_SWITCH, KEYSYM_SHIFT_L, KEYSYM_SHIFT_R,
    is_unicode_plane, overlay_bind_keysym,
};
use super::wayland_keymap::TEXT_SPARES;
use super::x11_link::{XLink, XkbPlacement};

/// Settle after binding a keycode, before pressing it: toolkits refetch the keymap
/// asynchronously, and one that has not caught up reads the keycode by its previous symbol,
/// NoSymbol on a spare, which drops the key outright. A Chrome whose main thread is starved of
/// its core needs about 25 ms.
pub const BIND_SETTLE: Duration = Duration::from_millis(25);
/// A group lock outlives the last key that needed it by this long: one switch per run of
/// keystrokes, and the desktop's layout indicator stays put.
pub const GROUP_LINGER: Duration = Duration::from_millis(500);
/// `XK_Left` at evdev's keycode for it, which tells the server's keycodes apart.
const EVDEV_LEFT: (u32, u8) = (KEYSYM_LEFT_ARROW, 113);

/// A group lock this shim holds: the group locked before the switch, the group locked now, and
/// the keysyms down under it with the group each was pressed under.
struct GroupHold {
    before: u8,
    now: u8,
    keys: HashMap<u32, u8>,
}

/// The XTEST keyboard over an `XLink`; see the module docstring.
pub struct XTestKeyboard<L: XLink> {
    link: L,
    /// Keysym to `(keycode, group, level)` from XKB; None until a lookup needs it or after a
    /// keymap change, and never built on a link without XKB.
    placement: Option<HashMap<u32, XkbPlacement>>,
    group_hold: Option<GroupHold>,
    /// When a lingering group lock is to be restored.
    group_restore_at: Option<Instant>,
    /// `XK_Shift_L`; 0 on an exotic keymap (capitals then skip Shift).
    shift_kc: u8,
    /// `XK_Shift_R`; a client-held Shift may be on either keycode.
    shift_r_kc: u8,
    /// `ISO_Level3_Shift`, else `Mode_switch`, for glyphs bound above the Shift level.
    altgr_kc: u8,
    /// Keycodes the modifier map actually binds; None where it could not be read.
    effective_mod_keycodes: Option<HashSet<u8>>,
    /// Keysym to the modifier keycodes `press` synthesized for it; `release` undoes only these.
    synth_mods: HashMap<u32, Vec<u8>>,
    /// Overlay pool, discovered lazily, the text spares first.
    spare_keycodes: Option<Vec<u8>>,
    spare_set: HashSet<u8>,
    /// How many of the pool are text spares a bind is kept to; 0 on evdev keycodes.
    text_codes: usize,
    /// Keysym to overlay keycode.
    overlay: HashMap<u32, u8>,
    /// Bound value (`overlay_bind_keysym`) to keycode, in step with `overlay`.
    overlay_value_kc: HashMap<u32, u8>,
    /// Overlay keysyms least-recently-used first, the recycle order.
    overlay_order: Vec<u32>,
    settle_until: Option<Instant>,
    /// Keysym to the keycode injected at press; release replays it and never re-resolves (the
    /// layout may shift mid-keystroke).
    pressed_kc: HashMap<u32, u8>,
}

impl<L: XLink> XTestKeyboard<L> {
    /// Resolve the synth-modifier keycodes and the modifier map on `link`.
    pub fn new(link: L) -> Self {
        let shift_kc = link.keysym_to_keycode(KEYSYM_SHIFT_L);
        let shift_r_kc = link.keysym_to_keycode(KEYSYM_SHIFT_R);
        let altgr_kc = match link.keysym_to_keycode(KEYSYM_ISO_LEVEL3_SHIFT) {
            0 => link.keysym_to_keycode(KEYSYM_MODE_SWITCH),
            kc => kc,
        };
        let effective = Self::read_effective_modifiers(&link);
        Self {
            link,
            placement: None,
            group_hold: None,
            group_restore_at: None,
            shift_kc,
            shift_r_kc,
            altgr_kc,
            effective_mod_keycodes: effective,
            synth_mods: HashMap::new(),
            spare_keycodes: None,
            spare_set: HashSet::new(),
            text_codes: 0,
            overlay: HashMap::new(),
            overlay_value_kc: HashMap::new(),
            overlay_order: Vec::new(),
            settle_until: None,
            pressed_kc: HashMap::new(),
        }
    }

    /// The connection the shim injects on.
    pub fn link(&self) -> &L {
        &self.link
    }

    /// Keycodes the server actually treats as modifiers.
    ///
    /// Holding a key only selects a shifted level when that keycode is bound in the modifier
    /// map. A keymap can carry the Shift keysym without binding it (a bare Xvfb with no keymap
    /// is the common case), and injecting Shift there types the level-0 glyph instead: every
    /// capital arrives lowercase.
    fn read_effective_modifiers(link: &L) -> Option<HashSet<u8>> {
        match link.modifier_mapping() {
            Ok(codes) => Some(codes.into_iter().collect()),
            Err(e) => {
                debug!(target: "input", "modifier map unreadable ({e}); assuming it binds what it names");
                None
            }
        }
    }

    /// Every keycode free to repurpose for the overlay.
    ///
    /// Spare means all levels NoSymbol, or carrying a previous overlay bind: every level the
    /// same Unicode-plane keysym, a shape no real layout produces (the server echoes a two-sym
    /// bind back expanded across all levels). Overlay binds persist on the X server across a
    /// handler restart, so without reclaiming them each restart would shrink the pool until
    /// typing beyond the layout starves. Modifier-mapped keycodes are never spare: pressing one
    /// would toggle its modifier under the typed character. The full range is scanned, since
    /// more slots make recycling, the only case where a slow app can mistranslate a rebound
    /// keycode, rare. The text spares lead the pool, and on keycodes other than evdev's they are
    /// all a bind recycles before it takes another (`text_codes`).
    pub fn find_spare_keycodes(&mut self) -> Vec<u8> {
        let (lo, hi) = self.link.keycode_range();
        let mapping = self
            .link
            .keyboard_mapping(lo, hi - lo + 1)
            .unwrap_or_default();
        let mod_keycodes: HashSet<u8> = self
            .link
            .modifier_mapping()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let mut spares: Vec<u8> = Vec::new();
        for (i, syms) in mapping.iter().enumerate() {
            let kc = lo.wrapping_add(i as u8);
            if mod_keycodes.contains(&kc) {
                continue;
            }
            let bound: HashSet<u32> = syms.iter().copied().filter(|s| *s != 0).collect();
            if bound.is_empty() {
                spares.push(kc);
            } else if bound.len() == 1 && bound.iter().all(|s| is_unicode_plane(*s)) {
                // An overlay bind, this handler's or a previous one's.
                spares.push(kc);
            }
        }
        self.spare_set = spares.iter().copied().collect();
        let text: Vec<u8> = TEXT_SPARES
            .iter()
            .map(|kc| *kc as u8)
            .filter(|kc| self.spare_set.contains(kc))
            .collect();
        let (keysym, evdev_kc) = EVDEV_LEFT;
        self.text_codes = if self.link.keysym_to_keycode(keysym) == evdev_kc {
            0
        } else {
            text.len()
        };
        let mut pool = text.clone();
        pool.extend(spares.iter().copied().filter(|kc| !text.contains(kc)));
        pool
    }

    fn spares(&mut self) -> &[u8] {
        if self.spare_keycodes.is_none() {
            let pool = self.find_spare_keycodes();
            self.spare_keycodes = Some(pool);
        }
        self.spare_keycodes.as_deref().unwrap_or(&[])
    }

    /// Spare keycodes not currently bound, in pool order.
    fn free_spares(&mut self) -> Vec<u8> {
        let used: HashSet<u8> = self.overlay.values().copied().collect();
        self.spares()
            .iter()
            .copied()
            .filter(|kc| !used.contains(kc))
            .collect()
    }

    /// `keysym_to_keycode`, distrusting hits on spare-pool keycodes: the cached lookup can name
    /// a keycode whose bind belongs to a previous handler, or to a different keysym after this
    /// pool re-purposed it, so on a pool keycode only this shim's own live bind carrying
    /// exactly this value counts. Only keysym forms an overlay bind can carry (Latin-1 high
    /// half, Unicode plane) can hit the pool, so everything else skips the discovery.
    fn layout_keycode(&mut self, keysym: u32) -> u8 {
        let kc = self.link.keysym_to_keycode(keysym);
        if kc == 0 {
            return 0;
        }
        if is_unicode_plane(keysym) || (0xA0..=0xFF).contains(&keysym) {
            self.spares();
            if self.spare_set.contains(&kc) {
                return self.overlay_value_kc.get(&keysym).copied().unwrap_or(0);
            }
        }
        kc
    }

    /// How long the newest overlay bind still owes clients before a press on it reads
    /// correctly; the caller awaits this off the runtime (`bind_ahead` first), and injection
    /// itself waits out any remainder.
    pub fn settle_delay(&self) -> Duration {
        self.settle_until
            .map(|t| t.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::ZERO)
    }

    fn settle(&self) {
        let remaining = self.settle_delay();
        if !remaining.is_zero() {
            std::thread::sleep(remaining);
        }
    }

    /// Resolve any overlay bind the keysym needs now, so the settle can be awaited before the
    /// injecting call re-resolves it from cache.
    pub fn bind_ahead(&mut self, keysym: u32) {
        let _ = self.resolve(keysym);
    }

    /// Index into `overlay_order` of the oldest binding whose keycode is not physically down:
    /// rebound while held, a keycode's eventual release would be read under the new symbol and
    /// leave the old one stuck. The bindings of `keep`, keysyms about to be typed, are passed
    /// over too.
    fn recycle_index(&self, keep: &[u32]) -> usize {
        let mut held: HashSet<u8> = self.pressed_kc.values().copied().collect();
        held.extend(keep.iter().filter_map(|k| self.overlay.get(k).copied()));
        self.overlay_order
            .iter()
            .position(|ks| !held.contains(&self.overlay[ks]))
            .unwrap_or(0)
    }

    /// How many text spares are not physically down: how many keysyms a run can bind on them
    /// at once; 0 where binds are not kept to them.
    pub fn text_room(&mut self) -> usize {
        self.spares();
        let held: HashSet<u8> = self.pressed_kc.values().copied().collect();
        let text_codes = self.text_codes;
        self.spares()
            .iter()
            .take(text_codes)
            .filter(|kc| !held.contains(kc))
            .count()
    }

    /// Reserve a spare keycode for `keysym` and record the binding.
    ///
    /// A free text spare comes first. Once every text spare is bound, the oldest bind on one
    /// that is not down, nor one of `keep`, is recycled before any other keycode is taken; then
    /// a free spare, then the oldest binding anywhere. The mapping request itself is the
    /// caller's (single or batched).
    fn alloc_overlay_keycode(&mut self, keysym: u32, keep: &[u32]) -> u8 {
        let free = self.free_spares();
        let text_codes = self.text_codes;
        let text: Vec<u8> = self.spares().iter().take(text_codes).copied().collect();
        let mut held: HashSet<u8> = self.pressed_kc.values().copied().collect();
        held.extend(keep.iter().filter_map(|k| self.overlay.get(k).copied()));
        let mut victim = None;
        if !(free
            .first()
            .is_some_and(|f| text.contains(f) || text.is_empty()))
        {
            victim =
                self.overlay_order.iter().copied().find(|ks| {
                    text.contains(&self.overlay[ks]) && !held.contains(&self.overlay[ks])
                });
            if victim.is_none() && free.is_empty() {
                victim = self.overlay_order.get(self.recycle_index(keep)).copied();
            }
        }
        let kc = match victim {
            None => free[0],
            Some(v) => {
                self.overlay_order.retain(|k| *k != v);
                let kc = self.overlay.remove(&v).unwrap_or(0);
                self.overlay_value_kc.remove(&overlay_bind_keysym(v));
                kc
            }
        };
        self.overlay.insert(keysym, kc);
        self.overlay_value_kc
            .insert(overlay_bind_keysym(keysym), kc);
        self.overlay_order.push(keysym);
        kc
    }

    /// Bind an unmapped keysym to a spare keycode (recycling the least recently used) and
    /// return it, or None if no spare keycode exists.
    pub fn overlay_keycode(&mut self, keysym: u32) -> Option<u8> {
        if let Some(kc) = self.overlay.get(&keysym).copied() {
            // Freshly used, so it goes to the back of the recycle queue: a pool this small (tens
            // of slots against a CJK composition's few keysyms per syllable) would otherwise
            // rebind a keycode a client may still be holding the previous symbol for.
            self.overlay_order.retain(|k| *k != keysym);
            self.overlay_order.push(keysym);
            return Some(kc);
        }
        if self.spares().is_empty() {
            return None;
        }
        let kc = self.alloc_overlay_keycode(keysym, &[]);
        // Bound at levels 0 and 1 so an accidental Shift cannot change it.
        let value = overlay_bind_keysym(keysym);
        if let Err(e) = self.link.change_keyboard_mapping(kc, &[vec![value, value]]) {
            debug!(target: "input", "overlay bind of keysym {keysym:#x} failed: {e}");
        }
        let _ = self.link.sync();
        self.settle_until = Some(Instant::now() + BIND_SETTLE);
        Some(kc)
    }

    /// The overlay keycode a keysym is bound to, without binding.
    pub fn overlay_binding(&self, keysym: u32) -> Option<u8> {
        self.overlay.get(&keysym).copied()
    }

    /// Overlay-bind every unmapped keysym in as few requests as possible.
    ///
    /// One `ChangeKeyboardMapping` per contiguous spare-keycode run, one sync, so a CJK
    /// composition commit broadcasts O(1) `MappingNotify` events instead of one per new
    /// character. The keycodes come as a single bind takes them (`alloc_overlay_keycode`), the
    /// text spares first, and never one a keysym of the batch is bound to.
    ///
    /// Returns false (nothing bound) when more new keysyms than slots exist, since filling
    /// would recycle bindings made earlier in the same batch and corrupt the text; the caller
    /// then falls back without partial typing.
    pub fn prebind(&mut self, keysyms: &[u32]) -> bool {
        let mut batch: Vec<u32> = Vec::new();
        for ks in keysyms {
            if !batch.contains(ks) {
                batch.push(*ks);
            }
        }
        let mut missing = Vec::new();
        for ks in &batch {
            if !self.overlay.contains_key(ks) && self.layout_keycode(*ks) == 0 {
                missing.push(*ks);
            }
        }
        if missing.is_empty() {
            return true;
        }
        if missing.len() > self.spares().len() {
            return false;
        }
        let mut assigns: Vec<(u8, u32)> = missing
            .iter()
            .map(|ks| (self.alloc_overlay_keycode(*ks, &batch), *ks))
            .collect();
        assigns.sort_unstable();
        let mut i = 0;
        while i < assigns.len() {
            let mut j = i;
            while j + 1 < assigns.len() && assigns[j + 1].0 == assigns[j].0.wrapping_add(1) {
                j += 1;
            }
            let rows: Vec<Vec<u32>> = assigns[i..=j]
                .iter()
                .map(|(_, ks)| {
                    let v = overlay_bind_keysym(*ks);
                    vec![v, v]
                })
                .collect();
            if let Err(e) = self.link.change_keyboard_mapping(assigns[i].0, &rows) {
                debug!(target: "input", "overlay batch bind failed: {e}");
            }
            i = j + 1;
        }
        let _ = self.link.sync();
        // One settle deadline for the whole batch, since one sync covers it.
        self.settle_until = Some(Instant::now() + BIND_SETTLE);
        true
    }

    /// True when every overlay binding in a notify's keycode range still resolves to its keysym
    /// in the server's map. Distinguishes this shim's own `MappingNotify` from a foreign layout
    /// change by semantics: servers vary in how many notifies one `ChangeKeyboardMapping` emits
    /// and report the full keycode range, so neither counting nor range matching works, but a
    /// self-bind leaves the bindings intact and a foreign change wipes them. Read from the
    /// keymap cache the notify just refreshed for its range, not a request per binding; a
    /// binding outside the range is one whose own notify is still on its way.
    pub fn bindings_intact(&self, first_keycode: u8, count: u8) -> bool {
        let last = first_keycode as u32 + count as u32;
        self.overlay
            .iter()
            .filter(|(_, kc)| (first_keycode as u32..last).contains(&(**kc as u32)))
            .all(|(ks, kc)| self.link.keycode_to_keysym(*kc, 0) == overlay_bind_keysym(*ks))
    }

    /// Re-resolve the synth-modifier keycodes from the (already refreshed) cache: a modifier
    /// remap moves them without touching the overlay.
    pub fn refresh_modifier_keycodes(&mut self) {
        self.shift_kc = self.link.keysym_to_keycode(KEYSYM_SHIFT_L);
        self.shift_r_kc = self.link.keysym_to_keycode(KEYSYM_SHIFT_R);
        self.altgr_kc = match self.link.keysym_to_keycode(KEYSYM_ISO_LEVEL3_SHIFT) {
            0 => self.link.keysym_to_keycode(KEYSYM_MODE_SWITCH),
            kc => kc,
        };
        self.effective_mod_keycodes = Self::read_effective_modifiers(&self.link);
    }

    /// A foreign keymap change (setxkbmap, desktop layout switcher) wiped the overlay bindings
    /// and may have moved modifier keycodes: drop the overlay bookkeeping, rediscover spares
    /// lazily, and re-resolve the modifier keycodes. Held keys are kept: release replays the
    /// exact press-time keycode.
    pub fn invalidate_mapping(&mut self) {
        self.overlay.clear();
        self.overlay_value_kc.clear();
        self.overlay_order.clear();
        self.spare_keycodes = None;
        self.spare_set.clear();
        self.placement = None;
        self.refresh_modifier_keycodes();
    }

    /// A keyboard `MappingNotify` arrived for this keycode range: refetch the XKB placement on
    /// the next lookup unless the range lies inside the spare pool, where only overlay binds
    /// live and the placement never trusts them anyway, so this shim's own binds cost no
    /// refetch.
    pub fn note_mapping_change(&mut self, first_keycode: u8, count: u8) {
        if !self.link.has_xkb() {
            return;
        }
        if self.spare_keycodes.is_some()
            && (first_keycode as u32..first_keycode as u32 + count as u32)
                .all(|kc| self.spare_set.contains(&(kc as u8)))
        {
            return;
        }
        self.placement = None;
    }

    /// Handle one keymap event the link reported: refresh the cache and keep the overlay coherent
    /// with server-side keymap changes. A modifier remap only re-resolves the Shift/AltGr keycodes:
    /// it never touches the overlay, and this shim's own overlay binds surface as modifier notifies
    /// on some servers, so clearing the overlay there would loop bind, notify, clear forever.
    pub fn keymap_changed(&mut self, event: super::x11_link::KeymapEvent) {
        use super::x11_link::KeymapEvent;
        match event {
            KeymapEvent::Modifier => self.refresh_modifier_keycodes(),
            KeymapEvent::Keyboard {
                first_keycode,
                count,
            } => {
                let refreshed = match self.link.refresh_keyboard_mapping(first_keycode, count) {
                    Ok(()) => true,
                    Err(e) => {
                        log::warn!(target: "input", "keymap cache refresh failed: {e}");
                        false
                    }
                };
                self.note_mapping_change(first_keycode, count);
                // `bindings_intact` reads the cache, which a failed refresh left stale.
                if refreshed && self.bindings_intact(first_keycode, count) {
                    return;
                }
                debug!(target: "input", "Foreign keymap change detected (keycodes {first_keycode}+{count}): invalidating XTEST overlay state.");
                self.invalidate_mapping();
            }
        }
    }

    fn xkb_locate(&mut self, keysym: u32) -> Result<Option<XkbPlacement>, String> {
        if !self.link.has_xkb() {
            return Ok(None);
        }
        if self.placement.is_none() {
            self.placement = Some(self.link.xkb_placement()?);
        }
        Ok(self
            .placement
            .as_ref()
            .and_then(|p| p.get(&keysym).copied()))
    }

    /// Whether this keysym must inject through `press`/`release` rather than a bare keycode:
    /// it is down under a group lock of this shim's, or the layout carries it only in a group
    /// past the first.
    pub fn outside_base_group(&mut self, keysym: u32) -> bool {
        if self.pressed_kc.contains_key(&keysym) && self.group_hold.is_some() {
            return true;
        }
        if !self.link.has_xkb() {
            return false;
        }
        match self.xkb_locate(keysym) {
            Ok(placed) => placed.is_some_and(|(_, group, _)| group != 0),
            Err(e) => {
                debug!(target: "input", "XKB placement lookup failed ({e}); core keymap only");
                false
            }
        }
    }

    /// Whether the layout itself (not an overlay bind) carries the keysym.
    pub fn layout_carries(&mut self, keysym: u32) -> bool {
        if self.link.has_xkb() {
            match self.placement_of(keysym) {
                Ok(placed) => return placed.is_some(),
                Err(e) => {
                    debug!(target: "input", "XKB placement lookup failed ({e}); core keymap only")
                }
            }
        }
        self.layout_keycode(keysym) != 0
    }

    /// `(keycode, group, level)` from the XKB map, with the spare-pool distrust of
    /// `layout_keycode` applied; None when XKB is unavailable or the keymap lacks the keysym.
    fn placement_of(&mut self, keysym: u32) -> Result<Option<XkbPlacement>, String> {
        let Some((kc, group, level)) = self.xkb_locate(keysym)? else {
            return Ok(None);
        };
        if is_unicode_plane(keysym) || (0xA0..=0xFF).contains(&keysym) {
            self.spares();
            if self.spare_set.contains(&kc) {
                return Ok(self
                    .overlay_value_kc
                    .get(&keysym)
                    .copied()
                    .filter(|kc| *kc != 0)
                    .map(|kc| (kc, 0, 0)));
            }
        }
        Ok(Some((kc, group, level)))
    }

    /// `(keycode, modifier keycodes, group)` to inject this keysym.
    ///
    /// The modifiers are the Shift/AltGr keycodes whose held state selects the keymap level the
    /// keysym sits at, so a glyph bound above the Shift level (AltGr '@') types correctly
    /// instead of falling through to its level-0 glyph. The group is the layout group the
    /// keycode carries the keysym in; None for an overlay keycode, which carries one group and
    /// so types the same under any lock, and when XKB is unavailable and the core keymap's
    /// flattened columns are all there is.
    ///
    /// A keysym the layout lacks binds to a spare keycode in-process; overlay keysyms sit at
    /// level 0 and never need modifiers. The same bind is used when the level a glyph sits at
    /// is unreachable because its modifier keycode is not in the modifier map: the glyph then
    /// carries its own case instead of depending on a modifier the server will not act on.
    /// Fails when no keycode exists and no spare keycode can be bound; the caller falls back to
    /// xdotool.
    pub fn resolve(&mut self, keysym: u32) -> Result<(u8, Vec<u8>, Option<u8>), String> {
        let mut xkb_answered = self.link.has_xkb();
        let mut placed = None;
        if xkb_answered {
            match self.placement_of(keysym) {
                Ok(p) => placed = p,
                Err(e) => {
                    debug!(target: "input", "XKB placement lookup failed ({e}); core keymap only");
                    xkb_answered = false;
                }
            }
        }
        let (kc, group, level) = if let Some((kc, group, level)) = placed {
            (kc, Some(group), level)
        } else if xkb_answered {
            (0, None, 0)
        } else {
            let kc = self.layout_keycode(keysym);
            // Lowest column carrying this glyph: 0 base, 1 Shift, 2 AltGr, 3 Shift+AltGr.
            let level = (0..4u8)
                .find(|lvl| self.link.keycode_to_keysym(kc, *lvl as usize) == keysym)
                .unwrap_or(0);
            (kc, None, level)
        };
        if kc == 0 {
            let kc = self
                .overlay_keycode(keysym)
                .ok_or_else(|| format!("no keycode for keysym {keysym:#x}"))?;
            return Ok((kc, Vec::new(), None));
        }
        let mut mods = Vec::new();
        if level & 1 != 0 && self.shift_kc != 0 {
            mods.push(self.shift_kc);
        }
        if level & 2 != 0 && self.altgr_kc != 0 {
            mods.push(self.altgr_kc);
        }
        if !mods.is_empty()
            && !self.modifiers_engage(&mods)
            && let Some(overlay_kc) = self.overlay_keycode(keysym)
        {
            return Ok((overlay_kc, Vec::new(), None));
        }
        Ok((kc, mods, group))
    }

    /// Whether holding these keycodes actually selects a shifted level.
    fn modifiers_engage(&self, mods: &[u8]) -> bool {
        match &self.effective_mod_keycodes {
            None => true,
            Some(effective) => mods.iter().all(|kc| effective.contains(kc)),
        }
    }

    /// Shift/AltGr keycodes currently down: the client-held keysyms the handler tracks, plus
    /// this shim's own synthesized holds. Tracked state only, so no press ever pays a
    /// `QueryKeymap` round trip.
    pub fn down_mod_keycodes(&self, held_keysyms: &[u32]) -> HashSet<u8> {
        let mut down: HashSet<u8> = self.synth_mods.values().flatten().copied().collect();
        for ks in held_keysyms {
            match *ks {
                KEYSYM_SHIFT_L => down.insert(self.shift_kc),
                KEYSYM_SHIFT_R => down.insert(self.shift_r_kc),
                KEYSYM_ISO_LEVEL3_SHIFT | KEYSYM_MODE_SWITCH => down.insert(self.altgr_kc),
                _ => false,
            };
        }
        down.remove(&0);
        down
    }

    /// Down Shift/AltGr keycodes the target level does not want. A required Shift is satisfied
    /// by either side, so neither is lifted then.
    pub fn mods_to_lift(&self, required: &[u8], down: &HashSet<u8>) -> Vec<u8> {
        let mut lift = Vec::new();
        if !required.contains(&self.shift_kc) {
            lift.extend(
                [self.shift_kc, self.shift_r_kc]
                    .into_iter()
                    .filter(|kc| down.contains(kc)),
            );
        }
        if self.altgr_kc != 0 && !required.contains(&self.altgr_kc) && down.contains(&self.altgr_kc)
        {
            lift.push(self.altgr_kc);
        }
        lift
    }

    /// Press a keysym through XTEST, synthesizing the modifiers its level needs.
    ///
    /// Only modifiers not already down are synthesized (a required Shift held on either side
    /// counts), and only those are undone by `release`. With `neutralize`, a held Shift/AltGr
    /// the level does not want is lifted around the press: it would select a different glyph,
    /// or push an overlay bind onto its empty AltGr levels; chords pass false so Ctrl+Shift+X
    /// keeps its held modifiers. `held_keysyms` are the level-selecting modifier keysyms the
    /// client itself holds, consulted instead of a per-press server query.
    pub fn press(
        &mut self,
        keysym: u32,
        neutralize: bool,
        held_keysyms: &[u32],
    ) -> Result<(), String> {
        let (kc, mods, group) = self.resolve(keysym)?;
        self.settle();
        self.enter_group(keysym, group);
        let down = self.down_mod_keycodes(held_keysyms);
        let lifted = if neutralize {
            self.mods_to_lift(&mods, &down)
        } else {
            Vec::new()
        };
        for m in &lifted {
            self.link.fake_key(*m, false);
        }
        let synth: Vec<u8> = mods
            .iter()
            .copied()
            .filter(|m| {
                !down.contains(m) && !(*m == self.shift_kc && down.contains(&self.shift_r_kc))
            })
            .collect();
        for m in &synth {
            self.link.fake_key(*m, true);
        }
        if !synth.is_empty() {
            self.synth_mods.insert(keysym, synth);
        }
        self.link.fake_key(kc, true);
        self.pressed_kc.insert(keysym, kc);
        for m in lifted.iter().rev() {
            self.link.fake_key(*m, true);
        }
        self.link.flush();
        Ok(())
    }

    /// Release a keysym, replaying its press-time keycode. Only an untracked press is
    /// re-resolved: the layout may have changed mid-keystroke, so a re-resolve of a tracked
    /// one could miss the key.
    pub fn release(&mut self, keysym: u32) -> Result<(), String> {
        let kc = match self.pressed_kc.remove(&keysym) {
            Some(kc) => kc,
            None => {
                let (kc, _, _) = self.resolve(keysym)?;
                self.settle();
                kc
            }
        };
        self.leave_group(keysym);
        self.link.fake_key(kc, false);
        for m in self
            .synth_mods
            .remove(&keysym)
            .unwrap_or_default()
            .iter()
            .rev()
        {
            self.link.fake_key(*m, false);
        }
        self.settle_group();
        self.link.flush();
        Ok(())
    }

    /// Whether `keysym` is down through this shim.
    pub fn is_pressed(&self, keysym: u32) -> bool {
        self.pressed_kc.contains_key(&keysym)
    }

    /// Lock the group a press needs, queued ahead of the press itself.
    ///
    /// Without a hold in force, a group-1 keysym injects under the server's own lock (a later
    /// group locked by the user's desktop switcher then decides, as for any single-group
    /// layout) and only a keysym of a later group reads the locked group, one round trip, and
    /// switches when it differs. While a hold is in force every key is tracked under it, so a
    /// group-1 key typed during a Cyrillic run switches back and the original lock returns once
    /// the last tracked key is up.
    fn enter_group(&mut self, keysym: u32, group: Option<u8>) {
        let Some(group) = group else { return };
        if !self.link.has_xkb() {
            return;
        }
        match &mut self.group_hold {
            None => {
                if group == 0 {
                    return;
                }
                let Ok(before) = self.link.xkb_locked_group() else {
                    return;
                };
                if before == group {
                    return;
                }
                self.link.xkb_lock_group(group);
                let mut keys = HashMap::new();
                keys.insert(keysym, group);
                self.group_hold = Some(GroupHold {
                    before,
                    now: group,
                    keys,
                });
            }
            Some(hold) => {
                self.group_restore_at = None;
                if hold.now != group {
                    self.link.xkb_lock_group(group);
                    hold.now = group;
                }
                hold.keys.insert(keysym, group);
            }
        }
    }

    /// Take a key out of the group hold ahead of its release, putting its press-time group back
    /// if a later key moved the lock on, so the release carries the keysym the press did.
    fn leave_group(&mut self, keysym: u32) {
        let Some(hold) = &mut self.group_hold else {
            return;
        };
        if let Some(group) = hold.keys.remove(&keysym)
            && group != hold.now
        {
            self.link.xkb_lock_group(group);
            hold.now = group;
        }
    }

    /// With no tracked key left down, schedule the restore of the lock found before the switch.
    fn settle_group(&mut self) {
        if self.group_hold.as_ref().is_some_and(|h| h.keys.is_empty()) {
            self.group_restore_at = Some(Instant::now() + GROUP_LINGER);
        }
    }

    /// When the lingering group restore is due, for the worker's wait.
    pub fn group_restore_due(&self) -> Option<Instant> {
        self.group_restore_at
    }

    /// The linger elapsed: put the group lock back unless a key of this shim's is still down
    /// under it.
    pub fn restore_group_if_due(&mut self) {
        let Some(at) = self.group_restore_at else {
            return;
        };
        if Instant::now() < at {
            return;
        }
        self.group_restore_at = None;
        if self.group_hold.as_ref().is_some_and(|h| h.keys.is_empty()) {
            self.release_group_lock();
        }
    }

    /// Put back the group lock found before this shim switched it, now: after a keyboard reset,
    /// or before the connection closes, no keystroke follows that a lingering lock would serve,
    /// and a lock left behind would have the desktop translate every later key under it.
    pub fn release_group_lock(&mut self) {
        self.group_restore_at = None;
        let Some(hold) = self.group_hold.take() else {
            return;
        };
        if hold.before == hold.now || !self.link.has_xkb() {
            return;
        }
        self.link.xkb_lock_group(hold.before);
        self.link.flush();
    }

    /// Type a string in-process, the shim half of `Dispatcher::type_text_xtest`.
    ///
    /// Each character is a press and release of its keysym (mapped characters with shift
    /// synthesis, unmapped ones through the spare-keycode overlay). A character prefers its
    /// canonical layout keysym, as the Wayland owner's `type_text` does, so a layout that
    /// carries the script types on its own keys instead of spending overlay slots on every
    /// letter. With `neutralize`, conflicting held Shift/AltGr are lifted around the whole run.
    /// Unmapped characters are bound in one batch, and nothing is typed on failure. Where binds
    /// are kept to the text spares (`text_room`), a text with more of them than those hold goes
    /// out in runs that fit them, each bound behind the keys before it. The settle each bind owes
    /// is slept out here, on the worker thread.
    ///
    /// Returns true on full success; false (having typed nothing) if any character cannot be
    /// resolved, so the caller can fall back to xdotool without double-typing.
    pub fn type_text(&mut self, text: &str, neutralize: bool, held_keysyms: &[u32]) -> bool {
        use super::keysyms::{character_to_layout_keysym, universal_text_keysym};
        if text.is_empty() {
            return false;
        }
        let mut keysyms = Vec::new();
        let mut overlaid = HashSet::new();
        for ch in text.chars() {
            let mut ks = character_to_layout_keysym(ch);
            if !self.layout_carries(ks) {
                match universal_text_keysym(ch) {
                    Some(u) => ks = u,
                    None => continue,
                }
                overlaid.insert(ks);
            }
            keysyms.push(ks);
        }
        let room = self.text_room();
        let mut runs: Vec<Vec<u32>> = vec![Vec::new()];
        let mut missing: HashSet<u32> = HashSet::new();
        for ks in keysyms {
            if room > 0 && overlaid.contains(&ks) && !missing.contains(&ks) {
                if missing.len() >= room {
                    runs.push(Vec::new());
                    missing.clear();
                }
                missing.insert(ks);
            }
            if let Some(run) = runs.last_mut() {
                run.push(ks);
            }
        }
        if !self.prebind(&runs[0]) {
            return false;
        }
        self.settle();
        let lifted = if neutralize {
            let down = self.down_mod_keycodes(held_keysyms);
            self.mods_to_lift(&[], &down)
        } else {
            Vec::new()
        };
        for m in &lifted {
            self.link.fake_key(*m, false);
        }
        let mut ok = true;
        for (i, run) in runs.iter().enumerate() {
            // A later run that cannot bind as a batch binds key by key.
            if i > 0 && self.prebind(run) {
                self.settle();
            }
            for ks in run {
                if self.press(*ks, false, &[]).is_err() || self.release(*ks).is_err() {
                    ok = false;
                    break;
                }
            }
            if !ok {
                break;
            }
        }
        for m in lifted.iter().rev() {
            self.link.fake_key(*m, true);
        }
        if !lifted.is_empty() {
            self.link.flush();
        }
        ok
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A core keymap from 8 to 255 answering the shim's queries, recording binds and presses.

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use super::super::x11_link::{KeymapEvent, XCursorImage, XLink, XkbPlacement};

    /// What a fake link records.
    #[derive(Default)]
    pub struct Recorded {
        /// Every XTEST key as `(keycode, down, seconds since start)`.
        pub keys: Vec<(u8, bool, f64)>,
        /// The keysym each pressed keycode mapped to at the moment of its press.
        pub typed: Vec<u32>,
        /// `(first keycode, rows)` of every `ChangeKeyboardMapping`.
        pub changes: Vec<(u8, usize)>,
        /// Keymap requests made.
        pub requests: usize,
        pub flushes: usize,
        pub motions: Vec<(i16, i16, bool)>,
        pub buttons: Vec<(u8, bool)>,
    }

    /// A fake X connection over a core keymap.
    pub struct FakeLink {
        pub map: Mutex<HashMap<u8, Vec<u32>>>,
        pub modmap: Vec<u8>,
        pub recorded: Arc<Mutex<Recorded>>,
        pub start: Mutex<Instant>,
        pub pointer: Mutex<(i16, i16, u16)>,
    }

    impl FakeLink {
        /// A keymap with `bound` mapped and `modmap` as the modifier map's keycodes.
        pub fn new(bound: HashMap<u8, Vec<u32>>, modmap: Vec<u8>) -> Self {
            let mut map = HashMap::new();
            for kc in 8u8..=255 {
                map.insert(kc, bound.get(&kc).cloned().unwrap_or_default());
            }
            Self {
                map: Mutex::new(map),
                modmap,
                recorded: Arc::new(Mutex::new(Recorded::default())),
                start: Mutex::new(Instant::now()),
                pointer: Mutex::new((0, 0, 0)),
            }
        }

        /// The keysym the server mapped each pressed keycode to at the moment of its press.
        pub fn typed(&self) -> Vec<u32> {
            self.recorded.lock().unwrap().typed.clone()
        }

        pub fn presses(&self) -> Vec<(u8, bool)> {
            self.recorded
                .lock()
                .unwrap()
                .keys
                .iter()
                .map(|(kc, d, _)| (*kc, *d))
                .collect()
        }

        pub fn clear(&self) {
            let mut r = self.recorded.lock().unwrap();
            r.keys.clear();
            r.typed.clear();
            r.changes.clear();
            *self.start.lock().unwrap() = Instant::now();
        }
    }

    impl XLink for FakeLink {
        fn keycode_range(&self) -> (u8, u8) {
            (8, 255)
        }

        fn keysym_to_keycode(&self, keysym: u32) -> u8 {
            let map = self.map.lock().unwrap();
            let mut best: Option<(usize, u8)> = None;
            for (kc, syms) in map.iter() {
                if let Some(col) = syms.iter().position(|s| *s == keysym)
                    && best.is_none_or(|(c, k)| (col, *kc) < (c, k))
                {
                    best = Some((col, *kc));
                }
            }
            best.map(|(_, kc)| kc).unwrap_or(0)
        }

        fn keycode_to_keysym(&self, keycode: u8, level: usize) -> u32 {
            self.map
                .lock()
                .unwrap()
                .get(&keycode)
                .and_then(|s| s.get(level).copied())
                .unwrap_or(0)
        }

        fn keyboard_mapping(&self, first: u8, count: u8) -> Result<Vec<Vec<u32>>, String> {
            self.recorded.lock().unwrap().requests += 1;
            let map = self.map.lock().unwrap();
            Ok((first as u32..first as u32 + count as u32)
                .map(|kc| {
                    let mut row = map.get(&(kc as u8)).cloned().unwrap_or_default();
                    row.resize(4, 0);
                    row
                })
                .collect())
        }

        fn modifier_mapping(&self) -> Result<Vec<u8>, String> {
            Ok(self.modmap.clone())
        }

        fn change_keyboard_mapping(&self, first: u8, rows: &[Vec<u32>]) -> Result<(), String> {
            self.recorded
                .lock()
                .unwrap()
                .changes
                .push((first, rows.len()));
            let mut map = self.map.lock().unwrap();
            for (i, row) in rows.iter().enumerate() {
                map.insert(first + i as u8, row.clone());
            }
            Ok(())
        }

        fn refresh_keyboard_mapping(&self, _first: u8, _count: u8) -> Result<(), String> {
            Ok(())
        }

        fn fake_key(&self, keycode: u8, down: bool) {
            let at = self.start.lock().unwrap().elapsed().as_secs_f64();
            let sym = self.keycode_to_keysym(keycode, 0);
            let mut r = self.recorded.lock().unwrap();
            r.keys.push((keycode, down, at));
            if down {
                r.typed.push(sym);
            }
        }

        fn fake_motion(&self, x: i16, y: i16, relative: bool) {
            self.recorded.lock().unwrap().motions.push((x, y, relative));
        }

        fn fake_button(&self, button: u8, down: bool) {
            self.recorded.lock().unwrap().buttons.push((button, down));
        }

        fn flush(&self) {
            self.recorded.lock().unwrap().flushes += 1;
        }

        fn sync(&self) -> Result<(), String> {
            Ok(())
        }

        fn has_xkb(&self) -> bool {
            false
        }

        fn xkb_placement(&self) -> Result<HashMap<u32, XkbPlacement>, String> {
            Err("no XKB".into())
        }

        fn xkb_locked_group(&self) -> Result<u8, String> {
            Err("no XKB".into())
        }

        fn xkb_lock_group(&self, _group: u8) {}

        fn query_pointer(&self) -> Result<(i16, i16, u16), String> {
            Ok(*self.pointer.lock().unwrap())
        }

        fn cursor_image(&self) -> Result<XCursorImage, String> {
            Err("no cursor".into())
        }

        fn screen_dpi(&self) -> Option<f64> {
            None
        }

        fn autorepeat(&self) -> Option<(u16, u16)> {
            None
        }

        fn reads_kernel_input(&self) -> bool {
            false
        }

        fn wait_keymap_event(&self) -> Option<KeymapEvent> {
            None
        }

        fn broken(&self) -> bool {
            false
        }
    }

    /// Latin letters on keycodes 38 to 63.
    pub fn latin_letters() -> HashMap<u8, Vec<u32>> {
        (38u8..64)
            .map(|kc| (kc, vec![0x61 + kc as u32 - 38]))
            .collect()
    }

    /// The modifier map the IME and text-spare suites use.
    pub fn pc105_modmap() -> Vec<u8> {
        vec![50, 62, 66, 37, 105, 64, 108, 77, 133, 134, 92]
    }

    /// The xfree86 keycodes the GLX desktop's Xorg uses: Home at 97, Print at 111, Alt_R at 113,
    /// Super_R at 116, and 101, 114, 118-131 and 133-146 left unbound beside the text spares.
    pub fn xfree86() -> HashMap<u8, Vec<u32>> {
        let mut m: HashMap<u8, Vec<u32>> = (8u8..101)
            .map(|kc| (kc, vec![0x61 + (kc as u32 % 26)]))
            .collect();
        for (kc, ks) in [
            (97u8, 0xFF50u32),
            (111, 0xFF61),
            (113, 0xFFEA),
            (116, 0xFFEC),
            (115, 0xFFEB),
            (108, 0xFE03),
            (100, 0xFF51),
            (102, 0xFF53),
            (103, 0xFF57),
            (104, 0xFF54),
            (105, 0xFF56),
            (106, 0xFF63),
            (107, 0xFFFF),
        ] {
            m.insert(kc, vec![ks]);
        }
        m.insert(XFREE86_SPACE, vec![0x20]);
        m
    }

    pub const XFREE86_SPACE: u8 = 65;

    /// An evdev keymap, Left at 113, binds most of F13-F24 and the JIS keys, and leaves a few
    /// others free.
    pub fn evdev() -> HashMap<u8, Vec<u32>> {
        let mut m: HashMap<u8, Vec<u32>> = (8u8..248)
            .map(|kc| (kc, vec![0x61 + (kc as u32 % 26)]))
            .collect();
        m.insert(113, vec![0xFF51]);
        for kc in [93u8, 132, 157, 202] {
            m.remove(&kc);
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    const HANGUL: [u32; 5] = [0x100D55C, 0x100AD6D, 0x100C5B4, 0x100C785, 0x100B825];

    fn keyboard(bound: HashMap<u8, Vec<u32>>) -> XTestKeyboard<FakeLink> {
        XTestKeyboard::new(FakeLink::new(bound, pc105_modmap()))
    }

    /// The pool's order on both keycode sets.
    #[test]
    fn text_spares_lead_the_pool() {
        let mut kb = keyboard(xfree86());
        let pool = kb.find_spare_keycodes();
        let mut want: Vec<u8> = (191u8..=202).collect();
        want.push(132);
        assert_eq!(
            &pool[..13],
            &want[..],
            "the text spares lead an xfree86 keymap's pool"
        );
        let mut kb = keyboard(evdev());
        let pool = kb.find_spare_keycodes();
        let mut want = vec![202u8, 132, 93, 157];
        want.extend(248u8..=255);
        assert_eq!(
            pool, want,
            "an evdev keymap's free text spares lead its pool too"
        );
        let placed: Vec<u8> = HANGUL
            .iter()
            .map(|ks| kb.overlay_keycode(*ks).unwrap())
            .collect();
        assert_eq!(
            placed,
            vec![202, 132, 93, 157, 248],
            "on evdev keycodes the whole pool serves before a bind is recycled"
        );
        assert_eq!(kb.text_room(), 0, "with no run kept to the text spares");
    }

    #[test]
    fn binds_land_on_text_spares() {
        let mut kb = keyboard(xfree86());
        assert_eq!(
            kb.overlay_keycode(HANGUL[0]),
            Some(191),
            "a single bind takes a text spare"
        );
        let mut kb = keyboard(xfree86());
        assert!(kb.prebind(&HANGUL), "a composition's batch binds");
        let placed: Vec<u8> = HANGUL
            .iter()
            .map(|ks| kb.overlay_binding(*ks).unwrap())
            .collect();
        assert!(
            placed.iter().all(|kc| (191..=202).contains(kc)),
            "on text spares, not on the unbound arrow, Delete or Copy codes: {placed:?}"
        );
        assert_eq!(
            kb.link().recorded.lock().unwrap().changes,
            vec![(191, 5)],
            "in one request"
        );
        let mut kb = keyboard(xfree86());
        let many: Vec<u32> = (0..20).map(|i| 0x1004E00 + i).collect();
        assert!(kb.prebind(&many), "a batch past the text spares binds");
        let placed: HashSet<u8> = many
            .iter()
            .map(|ks| kb.overlay_binding(*ks).unwrap())
            .collect();
        let text: HashSet<u8> = (191u8..=202).chain([132]).collect();
        assert!(
            text.is_subset(&placed),
            "filling every text spare before another keycode"
        );
    }

    #[test]
    fn recycling_keeps_to_the_text_spares() {
        let mut kb = keyboard(xfree86());
        let first: Vec<u8> = (0..13)
            .map(|i| kb.overlay_keycode(0x100AC00 + i).unwrap())
            .collect();
        let mut sorted = first.clone();
        sorted.sort_unstable();
        let mut want = vec![132u8];
        want.extend(191u8..=202);
        assert_eq!(sorted, want, "thirteen syllables fill the text spares");
        assert_eq!(
            kb.overlay_keycode(0x100AC00 + 13),
            Some(191),
            "the next recycles the oldest text spare rather than take a free arrow or Delete code"
        );
        assert_eq!(
            kb.text_room(),
            13,
            "a free text spare counts toward a run's room"
        );
        kb.link().recorded.lock().unwrap().changes.clear();
        let reused = 0x100AC00 + 1;
        let mut batch = vec![reused];
        batch.extend(HANGUL);
        assert!(kb.prebind(&batch), "a batch binds on full text spares");
        let placed: Vec<u8> = HANGUL
            .iter()
            .map(|ks| kb.overlay_binding(*ks).unwrap())
            .collect();
        assert!(
            placed
                .iter()
                .all(|kc| *kc == 132 || (191..=202).contains(kc)),
            "by recycling the oldest of them"
        );
        assert_eq!(
            kb.overlay_binding(reused),
            Some(first[1]),
            "and not the bind a keysym of the batch still needs"
        );
    }

    #[test]
    fn a_long_text_types_whole_on_the_text_spares() {
        let mut kb = keyboard(xfree86());
        let text = "가나다라마바사아자차카타파하 한국어 입력";
        assert!(
            kb.type_text(text, false, &[]),
            "a text of more syllables than the text spares hold is typed"
        );
        let typed = kb.link().typed();
        let want: Vec<u32> = text
            .chars()
            .map(|c| if c == ' ' { 0x20 } else { 0x1000000 + c as u32 })
            .collect();
        assert_eq!(typed, want, "whole");
        let codes: HashSet<u8> = kb
            .link()
            .recorded
            .lock()
            .unwrap()
            .keys
            .iter()
            .map(|(kc, _, _)| *kc)
            .collect();
        let allowed: HashSet<u8> = (191u8..=202).chain([132, XFREE86_SPACE]).collect();
        assert!(
            codes.is_subset(&allowed),
            "on text spares and the space key alone: {codes:?}"
        );
    }

    /// Telling this shim's notify from a foreign change.
    #[test]
    fn own_notify_reads_as_intact() {
        let mut kb = keyboard(latin_letters());
        let first = kb.overlay_keycode(0x100D55C).unwrap();
        let second = kb.overlay_keycode(0x100AD6D).unwrap();
        kb.link().recorded.lock().unwrap().requests = 0;
        assert!(
            kb.bindings_intact(first, 1),
            "its own notify reads as intact"
        );
        assert_eq!(
            kb.link().recorded.lock().unwrap().requests,
            0,
            "with no keymap request"
        );
        let stale = kb.link().map.lock().unwrap().clone();
        kb.link().map.lock().unwrap().insert(second, vec![]);
        assert!(
            kb.bindings_intact(first, 1),
            "a binding outside the notify's range is left to its own notify"
        );
        *kb.link().map.lock().unwrap() = stale;
        kb.link().map.lock().unwrap().insert(first, vec![0x61]);
        assert!(
            !kb.bindings_intact(8, 248),
            "a foreign change in range reads as one"
        );
    }

    /// Each unbound letter waits out its bind's settle on the worker, never the runtime.
    #[test]
    fn binds_settle_before_the_press() {
        let mut kb = keyboard(latin_letters());
        let start = Instant::now();
        for ks in &HANGUL[..3] {
            kb.press(*ks, true, &[]).unwrap();
            kb.release(*ks).unwrap();
        }
        let took = start.elapsed();
        assert!(
            took >= BIND_SETTLE.mul_f64(3.0 * 0.9),
            "each waits out its bind's settle: {took:?}"
        );
        let typed = kb.link().typed();
        assert_eq!(
            typed,
            HANGUL[..3].to_vec(),
            "three unbound letters are typed whole"
        );
    }
}
