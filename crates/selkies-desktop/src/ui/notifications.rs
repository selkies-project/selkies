//! The notifications the shell serves ([`crate::notify`]): the popups that
//! stack in the top right corner, and the notification center the bell on
//! the bar opens.
//!
//! Every notification is a [`Record`], newest first.  A record is live while
//! the protocol has it open, from its `Notify` until it expires, the user
//! dismisses it, or its sender withdraws it, each of which signals
//! `NotificationClosed` once; the last fifty that are not transient stay in
//! the center's history after they close.  A live record waits for a popup,
//! has one, or is kept from having one: by Do not disturb, by the center
//! being open, which lists it instead, or by its popup having been taken
//! down without closing it.
//!
//! The popups are cards, newest at the top, five at most with the rest
//! queued oldest first for the room a closing card leaves.  A card's
//! layout, the lines its texts wrap to and where its action chips go, is
//! measured once per content, and its body is drawn into a pixmap of its
//! own once per content and hover state; a frame composites those bodies at
//! their animated places and opacities and draws each countdown bar over
//! its card.  A card fades in sliding from the right, fades out in place,
//! and the cards around it then slide to their new places, every step a
//! redraw on the main loop's timer (`next_deadline`): one frame period while
//! anything moves, the countdown's step while only the bars shrink, and
//! otherwise the next expiry.  The bars alone are damaged on a countdown
//! step, so a stream encodes those strips rather than the stack.  Where the
//! display drops alpha, as X11 without a compositing manager does, a card
//! cannot fade: it slides in at full opacity and closes at once.
//!
//! The cards sit on a surface of their own sized to the stack, a layer
//! shell surface on the Overlay layer on Wayland and an override-redirect
//! window on X11, placed [`EDGE`] pixels from the top right corner, below the
//! bar when the bar is at the top; with no card the surface goes away, so
//! nothing covers a fullscreen window between notifications.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tiny_skia::{Color, PathBuilder, Pixmap, PixmapPaint, Transform};

use crate::desktop::{Desktop, Hover, Popup};
use crate::entries::{self, Apps};
use crate::event::{Button, Request};
use crate::geom::Rect;
use crate::i18n::{tr, trf};
use crate::icons::Icons;
use crate::notify::{CloseReason, Notification, Urgency};
use crate::render::{Align, Canvas, Fonts, rgb};
use crate::ui::{self, panel, popups, settings};

/// Width of a card.
pub const CARD_WIDTH: i32 = 360;
/// Space between the stack and the screen's edges.
pub const EDGE: i32 = 12;
/// Space between cards.
const GAP: i32 = 8;
/// Padding inside a card.
const PAD: i32 = 12;
/// Side of a card's icon.
const ICON: i32 = 32;
/// Space between the icon and the texts.
const ICON_GAP: i32 = 10;
/// Height of the summary line.
const SUMMARY_LINE: i32 = 19;
/// Height of a body line.
const BODY_LINE: i32 = 17;
/// Most lines of body a card shows.
const BODY_LINES: usize = 3;
/// Widest the application name grows at a card's top right.
const APP_NAME_WIDTH: i32 = 110;
/// Side of the close glyph a hovered card shows.
const CLOSE_SIZE: i32 = 16;
/// Height of an action chip.
const CHIP_HEIGHT: i32 = 26;
/// Padding beside a chip's label.
const CHIP_PAD: i32 = 10;
/// Space between chips.
const CHIP_GAP: i32 = 6;
/// Height of the progress bar a `value` hint draws.
const PROGRESS_HEIGHT: i32 = 6;
/// Height of the countdown bar along a card's bottom edge.
const COUNTDOWN_HEIGHT: i32 = 3;
/// Width of the red edge of a critical card.
const CRITICAL_EDGE: i32 = 4;
/// The color of that edge.
const CRITICAL: (u8, u8, u8) = (0xda, 0x44, 0x53);
/// Most cards shown at once.
pub const MAX_VISIBLE: usize = 5;
/// Most closed notifications the center keeps.
pub const MAX_HISTORY: usize = 50;
/// Most records kept, open or closed, so a flood of notifications that
/// never expire is bounded too.
const MAX_RECORDS: usize = 64;
/// How long a card takes to fade and slide in.
const FADE_IN: Duration = Duration::from_millis(160);
/// How far a card slides in from, to the right of its place.
const SLIDE: f32 = 24.0;
/// How long a closing card takes to fade out.
const FADE_OUT: Duration = Duration::from_millis(120);
/// How long a card takes to slide to a new place.
const MOVE: Duration = Duration::from_millis(160);
/// The redraw period while anything moves.
const FRAME: Duration = Duration::from_millis(16);
/// The redraw period while only countdown bars shrink.
const COUNTDOWN_STEP: Duration = Duration::from_millis(100);
/// How often the open center redraws its relative times.
const CENTER_REFRESH: Duration = Duration::from_secs(30);
/// Width of the notification center.
const CENTER_WIDTH: i32 = 380;
/// Height of the center's title row.
const CENTER_TITLE: i32 = 40;
/// Height of the center's Do not disturb row.
const CENTER_DND: i32 = 36;
/// Where the center's list starts below its top.
const CENTER_HEAD: i32 = CENTER_TITLE + CENTER_DND + 10;
/// Height of one row of the center's list.
const CENTER_ROW: i32 = 52;
/// Height of the center's empty state.
const CENTER_EMPTY: i32 = 72;
/// Tallest the center's list grows before it scrolls.
const CENTER_LIST_MAX: i32 = CENTER_ROW * 8;
/// Side of the icon of a center row.
const CENTER_ICON: i32 = 24;

/// Where a live record's popup stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Queued for a card.
    Waiting,
    /// On a card.
    Shown,
    /// Kept from a card: Do not disturb, the open center, or a card taken
    /// down without closing the notification.
    Held,
}

/// A notification the shell keeps, live or in the history.
#[derive(Debug)]
pub struct Record {
    /// The notification as last posted.
    pub note: Notification,
    /// When it was last posted.
    pub received: Instant,
    /// Whether the user has seen it in the center.
    pub read: bool,
    /// Whether the protocol has it open.
    pub live: bool,
    /// When it expires, while it is live.
    pub deadline: Option<Instant>,
    /// Whether the center lists it, which a transient notification is not.
    pub stored: bool,
    /// Where its popup stands.
    pub phase: Phase,
    /// Changes whenever the notification is replaced, so its card's layout
    /// and body are made again.
    version: u64,
    /// The resolved icon, with the pixel size it was resolved at.
    icon: Option<(u32, Option<Arc<Pixmap>>)>,
}

/// What a card's layout places, in the card's own coordinates.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CardLayout {
    /// The card's height.
    pub height: i32,
    /// The body's lines, the last ellipsized when the text runs on.
    pub body: Vec<String>,
    /// Each chip, with the index of its action in the notification's list.
    pub chips: Vec<(Rect, usize)>,
    /// The progress bar's track.
    pub progress: Option<Rect>,
}

/// What a pointer on a card is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardPart {
    /// The card itself, which invokes the default action.
    Body,
    /// The close glyph.
    Close,
    /// An action chip, by its action's index.
    Action(usize),
}

/// What a pointer in the center is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CenterPart {
    /// The Clear all button.
    Clear,
    /// The Do not disturb row.
    Dnd,
    /// A history row's close glyph, by the row's index in the list.
    RowClose(usize),
}

/// What a card's cached body was drawn for.
type BodyKey = (u64, Option<CardPart>, i32);

/// A popup on the stack.
#[derive(Debug)]
pub struct Card {
    /// The notification's id.
    pub id: u32,
    /// The record version the layout and body were made for.
    version: u64,
    /// The measured layout; `None` until measured.
    pub layout: Option<CardLayout>,
    /// When the card appeared.
    appeared: Instant,
    /// When the card began to fade out, once it is closing.
    closing: Option<Instant>,
    /// Where the card slides from.
    y_from: i32,
    /// Where the card slides to.
    y_to: i32,
    /// When the slide began.
    moved: Instant,
    /// The body drawn for a key.
    body: Option<(BodyKey, Pixmap)>,
}

impl Card {
    /// The card's height, zero until it is measured.
    fn height(&self) -> i32 {
        self.layout.as_ref().map_or(0, |l| l.height)
    }

    /// The card's top, partway through a slide.
    fn y(&self, now: Instant) -> f32 {
        let t = progress(now, self.moved, MOVE);
        self.y_from as f32 + (self.y_to - self.y_from) as f32 * ease(t)
    }

    /// The card's horizontal offset and opacity, from its fade in or out.
    fn fade(&self, now: Instant) -> (f32, f32) {
        if let Some(closing) = self.closing {
            return (0.0, 1.0 - progress(now, closing, FADE_OUT));
        }
        let t = ease(progress(now, self.appeared, FADE_IN));
        (SLIDE * (1.0 - t), t)
    }

    /// The card's rectangle on the surface, partway through its animations.
    pub fn rect(&self, now: Instant) -> Rect {
        let (dx, _) = self.fade(now);
        Rect::new(
            dx.round() as i32,
            self.y(now).round() as i32,
            CARD_WIDTH,
            self.height(),
        )
    }

    /// Whether the card is still moving or fading.
    fn animating(&self, now: Instant) -> bool {
        self.closing.is_some()
            || now < self.appeared + FADE_IN
            || (self.y_from != self.y_to && now < self.moved + MOVE)
    }
}

/// How far an animation of a length has run, from 0 to 1.
fn progress(now: Instant, start: Instant, length: Duration) -> f32 {
    (now.saturating_duration_since(start).as_secs_f32() / length.as_secs_f32()).clamp(0.0, 1.0)
}

/// An ease-out curve: fast at first, settling at the end.
fn ease(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// How long ago a notification arrived, as the center words it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ago {
    /// Under a minute.
    Now,
    /// Whole minutes, under an hour.
    Minutes(u64),
    /// Whole hours, under a day.
    Hours(u64),
    /// Whole days.
    Days(u64),
}

impl Ago {
    /// The span an elapsed time falls in.
    ///
    /// # Arguments
    /// * `elapsed` - Time since the notification arrived.
    pub fn of(elapsed: Duration) -> Ago {
        let minutes = elapsed.as_secs() / 60;
        match minutes {
            0 => Ago::Now,
            1..60 => Ago::Minutes(minutes),
            60..1440 => Ago::Hours(minutes / 60),
            _ => Ago::Days(minutes / 1440),
        }
    }

    /// The translated text, such as "2 min ago".
    pub fn label(self) -> String {
        match self {
            Ago::Now => tr("Just now").to_string(),
            Ago::Minutes(n) => trf("{n} min ago", &[("n", &n.to_string())]),
            Ago::Hours(n) => trf("{n} h ago", &[("n", &n.to_string())]),
            Ago::Days(n) => trf("{n} d ago", &[("n", &n.to_string())]),
        }
    }
}

/// Every notification, the stack of cards, and Do not disturb.
#[derive(Debug, Default)]
pub struct NotificationState {
    /// The records, newest first.
    pub records: Vec<Record>,
    /// The cards, top first.
    pub cards: Vec<Card>,
    /// Whether popups are held back.
    pub dnd: bool,
    /// Whether the center is open, which holds popups back too.
    center_open: bool,
    /// How far the center's list is scrolled.
    pub scroll: f64,
    /// When the open center next redraws its relative times.
    center_refresh: Option<Instant>,
    /// The signals to send, as requests for the main loop.
    signals: Vec<Request>,
    /// The last record version handed out.
    versions: u64,
    /// Whether the stack must be drawn again.
    dirty: bool,
    /// When the stack was last drawn.
    drawn: Option<Instant>,
    /// Whether the last frame was drawn mid-animation, so the settled one
    /// is still to draw.
    settling: bool,
    /// The surface size and top offset last requested.
    pub requested: (i32, i32, i32),
}

impl NotificationState {
    /// The next record version.
    fn version(&mut self) -> u64 {
        self.versions += 1;
        self.versions
    }

    /// The index of a record.
    fn find(&self, id: u32) -> Option<usize> {
        self.records.iter().position(|r| r.note.id == id)
    }

    /// The records the center lists, newest first.
    pub fn history(&self) -> impl Iterator<Item = &Record> {
        self.records.iter().filter(|r| r.stored)
    }

    /// How many listed records the user has not seen.
    pub fn unread(&self) -> usize {
        self.history().filter(|r| !r.read).count()
    }

    /// Whether anything is listed or open.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Takes the signals to send.
    pub fn take_signals(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.signals)
    }

    /// Marks the stack for drawing.
    pub fn touch(&mut self) {
        self.dirty = true;
    }

    /// Drops every layout and body, after a theme, font, or scale change.
    pub fn invalidate(&mut self) {
        for card in &mut self.cards {
            card.version = 0;
            card.body = None;
        }
        for record in &mut self.records {
            record.icon = None;
        }
        self.dirty = true;
    }

    /// Takes in a notification, replacing the record with its id, which
    /// moves to the top of the history.  A replaced notification keeps its
    /// card, which takes the new content in place; a new one waits for a
    /// card unless popups are held back.  The
    /// oldest records go once the history or the records overflow, closed
    /// as expired when they were still open.
    ///
    /// # Arguments
    /// * `note` - The notification.
    /// * `now` - The current time.
    pub fn receive(&mut self, note: Notification, now: Instant) {
        let version = self.version();
        let held = self.dnd || self.center_open;
        let deadline = note.timeout.map(|t| now + t);
        let stored = !note.transient;
        let index = self.find(note.id);
        let phase = match index.map(|i| self.records[i].phase) {
            Some(Phase::Shown) if self.cards.iter().any(|c| c.id == note.id && c.closing.is_none()) => {
                Phase::Shown
            }
            _ if held => Phase::Held,
            _ => Phase::Waiting,
        };
        let record = Record {
            note,
            received: now,
            read: self.center_open,
            live: true,
            deadline,
            stored,
            phase,
            version,
            icon: None,
        };
        if let Some(i) = index {
            self.records.remove(i);
        }
        self.records.insert(0, record);
        self.trim(now);
        self.dirty = true;
    }

    /// Drops the oldest records past the history's and the records' bounds.
    fn trim(&mut self, now: Instant) {
        loop {
            let stored = self.history().count();
            let over = if self.records.len() > MAX_RECORDS {
                self.records.len() - 1
            } else if stored > MAX_HISTORY {
                match self.records.iter().rposition(|r| r.stored) {
                    Some(i) => i,
                    None => return,
                }
            } else {
                return;
            };
            let id = self.records[over].note.id;
            self.close(id, CloseReason::Expired, now);
            if let Some(i) = self.find(id) {
                self.records.remove(i);
            }
        }
    }

    /// Closes a live notification: signals it, takes down its card, and
    /// forgets it unless the center lists it.  Nothing happens for one
    /// already closed.
    ///
    /// # Arguments
    /// * `id` - The notification.
    /// * `reason` - Why it closes.
    /// * `now` - The current time.
    pub fn close(&mut self, id: u32, reason: CloseReason, now: Instant) {
        let Some(index) = self.find(id) else {
            return;
        };
        let record = &mut self.records[index];
        if !record.live {
            return;
        }
        record.live = false;
        record.deadline = None;
        record.phase = Phase::Held;
        let stored = record.stored;
        self.signals.push(Request::NotificationClosed { id, reason });
        self.take_down(id, now);
        if !stored {
            self.records.remove(index);
        }
    }

    /// Starts the fade out of a notification's card.
    fn take_down(&mut self, id: u32, now: Instant) {
        for card in &mut self.cards {
            if card.id == id && card.closing.is_none() {
                card.closing = Some(now);
                self.dirty = true;
            }
        }
    }

    /// Invokes an action of a notification and closes it as dismissed,
    /// unless it is resident.
    ///
    /// # Arguments
    /// * `id` - The notification.
    /// * `key` - The action's key.
    /// * `now` - The current time.
    pub fn invoke(&mut self, id: u32, key: &str, now: Instant) {
        let Some(record) = self.find(id).map(|i| &self.records[i]) else {
            return;
        };
        if !record.live {
            return;
        }
        let resident = record.note.resident;
        self.signals.push(Request::ActionInvoked {
            id,
            key: key.to_string(),
        });
        if !resident {
            self.close(id, CloseReason::Dismissed, now);
        }
    }

    /// Removes a notification from the center, closing it as dismissed when
    /// it was still open.
    ///
    /// # Arguments
    /// * `id` - The notification.
    /// * `now` - The current time.
    pub fn remove(&mut self, id: u32, now: Instant) {
        self.close(id, CloseReason::Dismissed, now);
        if let Some(index) = self.find(id) {
            self.records.remove(index);
        }
    }

    /// Removes every notification, closing the open ones as dismissed.
    ///
    /// # Arguments
    /// * `now` - The current time.
    pub fn clear(&mut self, now: Instant) {
        let ids: Vec<u32> = self.records.iter().map(|r| r.note.id).collect();
        for id in ids {
            self.remove(id, now);
        }
        self.scroll = 0.0;
    }

    /// Notes the center opening: everything listed is read, and every card
    /// is taken down without closing its notification, which the center
    /// lists instead.
    ///
    /// # Arguments
    /// * `now` - The current time.
    pub fn open_center(&mut self, now: Instant) {
        self.center_open = true;
        self.scroll = 0.0;
        self.center_refresh = Some(now + CENTER_REFRESH);
        let ids: Vec<u32> = self.cards.iter().map(|c| c.id).collect();
        for id in ids {
            self.take_down(id, now);
        }
        for record in &mut self.records {
            record.read = true;
            if record.live {
                record.phase = Phase::Held;
            }
        }
    }

    /// Notes the center closing.
    pub fn close_center(&mut self) {
        self.center_open = false;
        self.center_refresh = None;
    }

    /// Whether the open center's relative times are due for a redraw, which
    /// schedules the next one.
    ///
    /// # Arguments
    /// * `now` - The current time.
    pub fn center_due(&mut self, now: Instant) -> bool {
        if self.center_refresh.is_some_and(|at| now >= at) {
            self.center_refresh = Some(now + CENTER_REFRESH);
            return true;
        }
        false
    }

    /// Moves time on: expires what ran out, drops cards that finished fading
    /// out, gives queued notifications the room left once no card is fading,
    /// so a queued card takes a closed one's place rather than pushing the
    /// stack down while that one fades, measures cards that need it, ends
    /// the slides that ran their course, and starts the slides of cards
    /// whose place changed.
    ///
    /// # Arguments
    /// * `now` - The current time.
    /// * `measure` - The layout of a record's card.
    pub fn advance(&mut self, now: Instant, mut measure: impl FnMut(&Record) -> CardLayout) {
        let expired: Vec<u32> = self
            .records
            .iter()
            .filter(|r| r.live && r.deadline.is_some_and(|d| now >= d))
            .map(|r| r.note.id)
            .collect();
        for id in expired {
            self.close(id, CloseReason::Expired, now);
        }
        let before = self.cards.len();
        self.cards
            .retain(|c| c.closing.is_none_or(|at| now < at + FADE_OUT));
        if self.cards.len() != before {
            self.dirty = true;
        }
        let fading = self.cards.iter().any(|c| c.closing.is_some());
        if !self.dnd && !self.center_open && !fading {
            while self.cards.len() < MAX_VISIBLE {
                let Some(index) = self
                    .records
                    .iter()
                    .rposition(|r| r.live && r.phase == Phase::Waiting)
                else {
                    break;
                };
                self.records[index].phase = Phase::Shown;
                self.cards.insert(
                    0,
                    Card {
                        id: self.records[index].note.id,
                        version: 0,
                        layout: None,
                        appeared: now,
                        closing: None,
                        y_from: 0,
                        y_to: 0,
                        moved: now,
                        body: None,
                    },
                );
                self.dirty = true;
            }
        }
        for card in &mut self.cards {
            if card.closing.is_some() {
                continue;
            }
            if let Some(record) = self.records.iter().find(|r| r.note.id == card.id)
                && record.version != card.version
            {
                card.layout = Some(measure(record));
                card.version = record.version;
                card.body = None;
                self.dirty = true;
            }
        }
        let mut y = 0;
        for card in &mut self.cards {
            if card.y_from != card.y_to && now >= card.moved + MOVE {
                card.y_from = card.y_to;
                self.dirty = true;
            }
            if card.y_to != y {
                let fresh = card.appeared == now;
                card.y_from = if fresh { y } else { card.y(now).round() as i32 };
                card.y_to = y;
                card.moved = now;
                self.dirty = true;
            }
            y += card.height() + GAP;
        }
    }

    /// Whether a card moves or fades at a time.
    pub fn animating(&self, now: Instant) -> bool {
        self.cards.iter().any(|c| c.animating(now))
    }

    /// Whether a card shows a countdown.
    fn counting_down(&self) -> bool {
        self.cards.iter().any(|c| self.countdown(c).is_some())
    }

    /// When the stack next needs time to move on: a frame period while
    /// anything moves, the countdown step while a bar shrinks, otherwise
    /// the next expiry, and the center's next redraw while it is open.
    ///
    /// # Arguments
    /// * `now` - The current time.
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let mut due = self.records.iter().filter_map(|r| r.deadline).min();
        let mut sooner = |at: Instant| due = Some(due.map_or(at, |d| d.min(at)));
        if self.dirty || self.settling || self.animating(now) {
            sooner(now + FRAME);
        } else if self.counting_down() {
            sooner(self.drawn.map_or(now, |at| at + COUNTDOWN_STEP));
        }
        if let Some(at) = self.center_refresh {
            sooner(at);
        }
        due
    }

    /// The share of a card's time left, for its countdown bar, when it
    /// counts down.
    fn countdown(&self, card: &Card) -> Option<(Instant, Duration)> {
        if card.closing.is_some() {
            return None;
        }
        let record = self.records.iter().find(|r| r.note.id == card.id)?;
        Some((record.deadline?, record.note.timeout?))
    }

    /// The surface the stack needs: its width and height, both zero when
    /// there is no card.  The height covers every card where it slides from
    /// as well as where it slides to, so a slide never resizes the surface.
    pub fn surface_size(&self) -> (i32, i32) {
        let height = self
            .cards
            .iter()
            .map(|c| c.y_from.max(c.y_to) + c.height())
            .max()
            .unwrap_or(0);
        if height <= 0 {
            return (0, 0);
        }
        (CARD_WIDTH + EDGE, height)
    }

    /// Whether the stack needs drawing at a time, and whether only its
    /// countdown bars changed.
    fn frame_due(&self, now: Instant) -> Option<bool> {
        if self.dirty || self.settling || self.animating(now) {
            return Some(false);
        }
        let step = self.drawn.is_none_or(|at| now >= at + COUNTDOWN_STEP);
        (self.counting_down() && step).then_some(true)
    }
}

/// Breaks a text into lines that fit a width, at word boundaries and at its
/// own newlines, blank lines dropped, at most a number of them; when text is
/// left over the last line ends in an ellipsis, which drawing keeps when it
/// cuts the line to fit.
///
/// # Arguments
/// * `fonts` - The font system.
/// * `text` - The text.
/// * `px` - Font size in physical pixels.
/// * `max_width` - Width limit in physical pixels.
/// * `max_lines` - Most lines returned.
pub fn wrap(fonts: &mut Fonts, text: &str, px: f32, max_width: f32, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut more = false;
    'paragraphs: for paragraph in text.split('\n').map(str::trim).filter(|p| !p.is_empty()) {
        if lines.len() == max_lines {
            more = true;
            break;
        }
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if line.is_empty() || fonts.shape(&candidate, px, false).width <= max_width {
                line = candidate;
                continue;
            }
            lines.push(std::mem::replace(&mut line, word.to_string()));
            if lines.len() == max_lines {
                more = true;
                break 'paragraphs;
            }
        }
        lines.push(line);
    }
    if more && let Some(last) = lines.last_mut() {
        last.push('…');
    }
    lines
}

/// The layout of a record's card at an output scale.
///
/// # Arguments
/// * `fonts` - The font system.
/// * `record` - The record.
/// * `scale` - The output scale the card is drawn at.
pub fn measure(fonts: &mut Fonts, record: &Record, scale: i32) -> CardLayout {
    let s = scale.max(1) as f32;
    let note = &record.note;
    let text_x = PAD + ICON + ICON_GAP;
    let text_w = CARD_WIDTH - text_x - PAD;
    let body = wrap(
        fonts,
        &note.body,
        ui::FONT_SIZE * s,
        text_w as f32 * s,
        BODY_LINES,
    );
    let mut y = PAD + SUMMARY_LINE + body.len() as i32 * BODY_LINE;
    let progress = note.progress.map(|_| {
        y += 6;
        let track = Rect::new(text_x, y, text_w, PROGRESS_HEIGHT);
        y += PROGRESS_HEIGHT;
        track
    });
    let mut chips = Vec::new();
    let mut x = text_x;
    for (index, (key, label)) in note.actions.iter().enumerate() {
        if key == "default" || label.is_empty() {
            continue;
        }
        let w = ui::text_width(fonts, label, ui::FONT_SMALL, true, scale.max(1)) + 2 * CHIP_PAD;
        if x + w > text_x + text_w {
            break;
        }
        chips.push((Rect::new(x, 0, w, CHIP_HEIGHT), index));
        x += w + CHIP_GAP;
    }
    if !chips.is_empty() {
        y += 8;
        for (chip, _) in &mut chips {
            chip.y = y;
        }
        y += CHIP_HEIGHT;
    }
    CardLayout {
        height: y.max(PAD + ICON) + PAD + COUNTDOWN_HEIGHT,
        body,
        chips,
        progress,
    }
}

/// The icon of a notification at a pixel size: the image it carries, else
/// the icon it names, else its desktop entry's, resolved once per size.
fn record_icon(record: &mut Record, icons: &mut Icons, apps: &Apps, px: u32) -> Option<Arc<Pixmap>> {
    if let Some((size, icon)) = &record.icon
        && *size == px
    {
        return icon.clone();
    }
    let note = &record.note;
    let icon = note
        .image
        .clone()
        .or_else(|| note.icon_name.as_deref().and_then(|name| icons.get(name, px)))
        .or_else(|| {
            let id = format!("{}.desktop", note.desktop_entry.as_deref()?);
            let name = apps
                .find(&id)
                .and_then(|i| apps.entries[i].icon.clone())
                .or_else(|| entries::load_entry(&id).and_then(|e| e.icon))?;
            icons.get(&name, px)
        });
    record.icon = Some((px, icon.clone()));
    icon
}

/// A bell, the glyph of the notifications, filled in a box, struck through
/// when popups are held back.
///
/// # Arguments
/// * `canvas` - The target.
/// * `rect` - The box the bell is centered in.
/// * `size` - The bell's height.
/// * `color` - Its color.
/// * `struck` - Whether to strike it through.
pub fn draw_bell(canvas: &mut Canvas<'_>, rect: Rect, size: f32, color: Color, struck: bool) {
    let u = size / 16.0;
    let cx = rect.x as f32 + rect.w as f32 / 2.0;
    let cy = rect.y as f32 + rect.h as f32 / 2.0 - u;
    let mut pb = PathBuilder::new();
    pb.move_to(cx - 6.5 * u, cy + 4.0 * u);
    pb.line_to(cx - 5.0 * u, cy + 1.5 * u);
    pb.line_to(cx - 5.0 * u, cy - 1.5 * u);
    pb.cubic_to(
        cx - 5.0 * u,
        cy - 5.5 * u,
        cx - 2.5 * u,
        cy - 7.0 * u,
        cx,
        cy - 7.0 * u,
    );
    pb.cubic_to(
        cx + 2.5 * u,
        cy - 7.0 * u,
        cx + 5.0 * u,
        cy - 5.5 * u,
        cx + 5.0 * u,
        cy - 1.5 * u,
    );
    pb.line_to(cx + 5.0 * u, cy + 1.5 * u);
    pb.line_to(cx + 6.5 * u, cy + 4.0 * u);
    pb.close();
    pb.push_circle(cx, cy + 6.0 * u, 1.8 * u);
    if let Some(path) = pb.finish() {
        canvas.fill_path(&path, color);
    }
    if struck {
        let mut pb = PathBuilder::new();
        pb.move_to(cx - 7.5 * u, cy - 6.5 * u);
        pb.line_to(cx + 7.5 * u, cy + 8.5 * u);
        if let Some(path) = pb.finish() {
            canvas.stroke_path(&path, 1.8 * u.max(1.0), color);
        }
    }
}

/// The unread count as the badge shows it.
fn badge_text(unread: usize) -> String {
    if unread > 99 {
        "99+".to_string()
    } else {
        unread.to_string()
    }
}

/// Draws the bell on the bar with its unread badge.
///
/// # Arguments
/// * `d` - The desktop.
/// * `canvas` - The panel canvas.
/// * `rect` - The bell's button.
pub fn draw_bell_button(d: &mut Desktop, canvas: &mut Canvas<'_>, rect: Rect) {
    let open = d.popup == Popup::Notifications;
    if d.hover == Hover::Bell || open {
        canvas.fill_rounded(rect, 5.0, if open { ui::active_bg() } else { ui::hover_bg() });
    }
    let dnd = d.notifications.dnd;
    let color = if dnd { ui::text_dim() } else { ui::text() };
    draw_bell(canvas, rect, 16.0, color, dnd);
    let unread = d.notifications.unread();
    if unread == 0 {
        return;
    }
    let text = badge_text(unread);
    let px = 9.5;
    let width = ui::text_width(&mut d.fonts, &text, px, true, canvas.scale()) + 6;
    let side = width.max(14);
    let badge = Rect::new(rect.right() - side + 2, rect.y - 1, side, 14);
    canvas.fill_rounded(badge, 7.0, ui::accent());
    ui::label(
        canvas,
        &mut d.fonts,
        &text,
        badge,
        px,
        true,
        Align::Center,
        ui::on_accent(),
    );
}

/// The tooltip of the bell.
pub fn bell_tooltip(d: &Desktop) -> String {
    if d.notifications.dnd {
        tr("Do not disturb").to_string()
    } else {
        tr("Notifications").to_string()
    }
}

/// Draws a card's body, everything but its countdown, at the card's origin.
fn draw_body(d: &mut Desktop, index: usize, hover: Option<CardPart>, canvas: &mut Canvas<'_>) {
    let Desktop {
        notifications,
        fonts,
        icons,
        apps,
        ..
    } = d;
    let card = &notifications.cards[index];
    let Some(layout) = card.layout.clone() else {
        return;
    };
    let Some(record) = notifications.records.iter_mut().find(|r| r.note.id == card.id) else {
        return;
    };
    let rect = Rect::new(0, 0, CARD_WIDTH, layout.height);
    let radius = ui::popup_radius();
    ui::fill_surface(canvas, rect, radius, ui::popup_bg());
    if record.note.urgency == Urgency::Critical {
        canvas.clip(Rect::new(0, 0, CRITICAL_EDGE, rect.h));
        let (r, g, b) = CRITICAL;
        canvas.fill_rounded(rect, radius, rgb(r, g, b));
        canvas.reset_clip();
    }
    canvas.stroke_rounded(rect, radius, 1.0, ui::text_alpha(0x40));

    let px = (ICON * canvas.scale()) as u32;
    let icon_rect = Rect::new(PAD, PAD, ICON, ICON);
    match record_icon(record, icons, apps, px) {
        Some(icon) => canvas.draw_image_fit(&icon, icon_rect),
        None => {
            canvas.fill_rounded(icon_rect, ICON as f32 / 2.0, ui::field_bg());
            draw_bell(canvas, icon_rect, 18.0, ui::text_dim(), false);
        }
    }
    let note = &record.note;
    let low = note.urgency == Urgency::Low;
    let (strong, weak) = if low {
        (ui::text_dim(), ui::text_alpha(0x80))
    } else {
        (ui::text(), ui::text_dim())
    };
    let text_x = PAD + ICON + ICON_GAP;
    let mut right = CARD_WIDTH - PAD;
    if hover.is_some() {
        let close = Rect::new(right - CLOSE_SIZE, PAD + 1, CLOSE_SIZE, CLOSE_SIZE);
        if hover == Some(CardPart::Close) {
            canvas.fill_rounded(close.inflate(3), (CLOSE_SIZE / 2 + 3) as f32, ui::hover_bg());
        }
        if let Some(path) = ui::cross_path(close, 4.5) {
            canvas.stroke_path(&path, 1.6, strong);
        }
        right = close.x - 6;
    }
    let app_w = if note.app_name.is_empty() {
        0
    } else {
        ui::text_width(fonts, &note.app_name, ui::FONT_SMALL, false, canvas.scale()).min(APP_NAME_WIDTH)
    };
    if app_w > 0 {
        let app_rect = Rect::new(right - app_w, PAD, app_w, SUMMARY_LINE);
        ui::label(
            canvas,
            fonts,
            &note.app_name,
            app_rect,
            ui::FONT_SMALL,
            false,
            Align::Right,
            weak,
        );
        right -= app_w + 8;
    }
    let summary = if note.summary.is_empty() {
        &note.app_name
    } else {
        &note.summary
    };
    let summary_rect = Rect::new(text_x, PAD, right - text_x, SUMMARY_LINE);
    ui::label(
        canvas,
        fonts,
        summary,
        summary_rect,
        ui::FONT_SIZE,
        true,
        Align::Left,
        strong,
    );
    let text_w = CARD_WIDTH - text_x - PAD;
    for (i, line) in layout.body.iter().enumerate() {
        let line_rect = Rect::new(
            text_x,
            PAD + SUMMARY_LINE + i as i32 * BODY_LINE,
            text_w,
            BODY_LINE,
        );
        ui::label(
            canvas,
            fonts,
            line,
            line_rect,
            ui::FONT_SIZE,
            false,
            Align::Left,
            weak,
        );
    }
    if let (Some(track), Some(value)) = (layout.progress, note.progress) {
        let radius = PROGRESS_HEIGHT as f32 / 2.0;
        canvas.fill_rounded(track, radius, ui::text_alpha(0x30));
        let fill = Rect::new(track.x, track.y, track.w * value as i32 / 100, track.h);
        canvas.fill_rounded(fill, radius, ui::accent());
    }
    for (chip, action) in &layout.chips {
        let hovered = hover == Some(CardPart::Action(*action));
        let fill = if hovered {
            ui::shifted(ui::accent(), 18)
        } else {
            ui::accent()
        };
        canvas.fill_rounded(*chip, 6.0, fill);
        let label = &note.actions[*action].1;
        ui::label(
            canvas,
            fonts,
            label,
            *chip,
            ui::FONT_SMALL,
            true,
            Align::Center,
            ui::on_accent(),
        );
    }
}

/// What a point on the stack's surface is over: a card that is not
/// closing, by id, and its part.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X on the surface.
/// * `y` - Pointer Y on the surface.
pub fn hit(d: &Desktop, x: f64, y: f64) -> Option<(u32, CardPart)> {
    let now = Instant::now();
    let card = d
        .notifications
        .cards
        .iter()
        .find(|c| c.closing.is_none() && c.rect(now).contains(x, y))?;
    let rect = card.rect(now);
    let (lx, ly) = (x - rect.x as f64, y - rect.y as f64);
    let close = Rect::new(CARD_WIDTH - PAD - CLOSE_SIZE, PAD + 1, CLOSE_SIZE, CLOSE_SIZE).inflate(4);
    if close.contains(lx, ly) {
        return Some((card.id, CardPart::Close));
    }
    let chip = card
        .layout
        .as_ref()
        .and_then(|l| l.chips.iter().find(|(r, _)| r.contains(lx, ly)));
    if let Some((_, action)) = chip {
        return Some((card.id, CardPart::Action(*action)));
    }
    Some((card.id, CardPart::Body))
}

/// Handles a press on the stack: the close glyph or a right click dismisses
/// the card, a chip invokes its action, and the card itself invokes the
/// default action when there is one, each then closing the notification
/// unless it is resident; with no default action the card just closes.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X on the surface.
/// * `y` - Pointer Y on the surface.
/// * `button` - The button.
pub fn click(d: &mut Desktop, x: f64, y: f64, button: Button) {
    let Some((id, part)) = hit(d, x, y) else {
        return;
    };
    let now = Instant::now();
    let state = &mut d.notifications;
    match (button, part) {
        (Button::Right, _) | (Button::Left, CardPart::Close) => state.close(id, CloseReason::Dismissed, now),
        (Button::Left, CardPart::Action(index)) => {
            let key = state
                .find(id)
                .and_then(|i| state.records[i].note.actions.get(index))
                .map(|(key, _)| key.clone());
            if let Some(key) = key {
                state.invoke(id, &key, now);
            }
        }
        (Button::Left, CardPart::Body) => {
            let has_default = state
                .find(id)
                .is_some_and(|i| state.records[i].note.default_action().is_some());
            if has_default {
                state.invoke(id, "default", now);
            } else {
                state.close(id, CloseReason::Dismissed, now);
            }
        }
        (Button::Middle, _) => {}
    }
}

/// Moves the stack on and draws it when it needs drawing, into a pixmap of
/// the surface's size.
///
/// # Arguments
/// * `d` - The desktop.
/// * `pix` - The surface's pixels.
/// * `scale` - The surface's output scale.
/// * `now` - The current time.
/// * `opaque` - Whether the display drops alpha, so cards never fade.
///
/// # Returns
/// The rectangles drawn on, the ones that take input, and the damage; `None`
/// when nothing changed.
pub fn render(
    d: &mut Desktop,
    pix: &mut Pixmap,
    scale: i32,
    now: Instant,
    opaque: bool,
) -> Option<(Vec<Rect>, Vec<Rect>, Vec<Rect>)> {
    let bars_only = d.notifications.frame_due(now)?;
    d.notifications.dirty = false;
    d.notifications.drawn = Some(now);
    d.notifications.settling = d.notifications.animating(now);
    let surface = Rect::new(0, 0, pix.width() as i32 / scale, pix.height() as i32 / scale);
    let hover = match d.hover {
        Hover::Card(id, part) => Some((id, part)),
        _ => None,
    };
    pix.fill(Color::TRANSPARENT);
    let mut visible = Vec::new();
    let mut input = Vec::new();
    let mut bars = Vec::new();
    for index in 0..d.notifications.cards.len() {
        let card = &d.notifications.cards[index];
        if card.layout.is_none() {
            continue;
        }
        let rect = card.rect(now);
        let (_, fade) = card.fade(now);
        let opacity = if opaque {
            if card.closing.is_some() { 0.0 } else { 1.0 }
        } else {
            fade
        };
        let part = hover.filter(|(id, _)| *id == card.id).map(|(_, part)| part);
        let countdown = d.notifications.countdown(card);
        let closing = card.closing.is_some();
        {
            let key = (card.version, part, scale);
            if d.notifications.cards[index]
                .body
                .as_ref()
                .is_none_or(|(k, _)| *k != key)
            {
                let mut body = Pixmap::new((CARD_WIDTH * scale) as u32, (rect.h * scale).max(1) as u32)?;
                {
                    let mut canvas = Canvas::new(body.as_mut(), scale);
                    draw_body(d, index, part, &mut canvas);
                }
                d.notifications.cards[index].body = Some((key, body));
            }
            if let Some((_, body)) = &d.notifications.cards[index].body
                && opacity > 0.0
            {
                let paint = PixmapPaint {
                    opacity,
                    ..PixmapPaint::default()
                };
                pix.draw_pixmap(
                    rect.x * scale,
                    rect.y * scale,
                    body.as_ref(),
                    &paint,
                    Transform::identity(),
                    None,
                );
            }
        }
        if opacity > 0.0 {
            visible.push(rect.intersect(&surface));
        }
        if !closing {
            input.push(rect.intersect(&surface));
        }
        if let Some((deadline, total)) = countdown {
            let left = deadline.saturating_duration_since(now).as_secs_f32() / total.as_secs_f32().max(0.001);
            let inset = (ui::popup_radius() as i32).max(PAD / 2);
            let track = Rect::new(
                rect.x + inset,
                rect.bottom() - COUNTDOWN_HEIGHT - 2,
                rect.w - 2 * inset,
                COUNTDOWN_HEIGHT,
            );
            let bar = Rect::new(
                track.x,
                track.y,
                (track.w as f32 * left.clamp(0.0, 1.0)).round() as i32,
                track.h,
            );
            let mut canvas = Canvas::new(pix.as_mut(), scale);
            canvas.fill_rounded(
                bar,
                COUNTDOWN_HEIGHT as f32 / 2.0,
                ui::with_alpha(ui::accent(), (opacity * 255.0) as u8),
            );
            bars.push(track.inflate(1));
        }
    }
    let damage = if bars_only { bars } else { vec![surface] };
    Some((visible, input, damage))
}

/// The center's rectangle above the bell, when the bell is on the bar.
fn center_rect(d: &mut Desktop) -> Option<Rect> {
    let anchor = panel::layout(d).notifications?;
    let rows = d.notifications.history().count() as i32;
    let list = if rows == 0 {
        CENTER_EMPTY
    } else {
        (rows * CENTER_ROW).min(CENTER_LIST_MAX)
    };
    Some(popups::anchor_above(
        d,
        anchor,
        CENTER_WIDTH,
        CENTER_HEAD + list + 6,
    ))
}

/// The parts of the center: the Clear all button, the Do not disturb row
/// with its switch, and the list's viewport.
fn center_parts(d: &mut Desktop, frame: Rect) -> (Rect, Rect, Rect, Rect) {
    let width = ui::text_width(&mut d.fonts, tr("Clear all"), ui::FONT_SMALL, true, d.panel.scale) + 20;
    let clear = Rect::new(frame.right() - 12 - width, frame.y + 8, width, 26);
    let dnd = Rect::new(frame.x + 6, frame.y + CENTER_TITLE, frame.w - 12, CENTER_DND);
    let switch = Rect::new(
        dnd.right() - 8 - settings::SWITCH_WIDTH,
        dnd.y + (dnd.h - settings::SWITCH_HEIGHT) / 2,
        settings::SWITCH_WIDTH,
        settings::SWITCH_HEIGHT,
    );
    let list = Rect::new(
        frame.x + 6,
        frame.y + CENTER_HEAD,
        frame.w - 12,
        frame.bottom() - 6 - (frame.y + CENTER_HEAD),
    );
    (clear, dnd, switch, list)
}

/// The rectangle of a history row and of its close glyph, the row leaving
/// the scrollbar its band when the list scrolls.
fn center_row(d: &Desktop, list: Rect, index: usize) -> (Rect, Rect) {
    let y = list.y + index as i32 * CENTER_ROW - d.notifications.scroll.round() as i32;
    let band = if center_max_scroll(d, list) > 0.0 {
        ui::SCROLLBAR - 2
    } else {
        0
    };
    let row = Rect::new(list.x, y, list.w - band, CENTER_ROW);
    let close = Rect::new(row.right() - 30, row.y + 8, 22, 22);
    (row, close)
}

/// The largest scroll of the center's list.
fn center_max_scroll(d: &Desktop, list: Rect) -> f64 {
    (d.notifications.history().count() as i32 * CENTER_ROW - list.h).max(0) as f64
}

/// Draws the notification center; returns its rectangle.
///
/// # Arguments
/// * `d` - The desktop.
/// * `canvas` - The panel canvas.
pub fn draw_center(d: &mut Desktop, canvas: &mut Canvas<'_>) -> Option<Rect> {
    let frame = center_rect(d)?;
    let (clear, dnd, switch, list) = center_parts(d, frame);
    ui::draw_popup_frame(canvas, frame);
    let title = Rect::new(frame.x + 14, frame.y + 6, clear.x - frame.x - 20, 30);
    ui::label(
        canvas,
        &mut d.fonts,
        tr("Notifications"),
        title,
        ui::FONT_SIZE,
        true,
        Align::Left,
        ui::text(),
    );
    let empty = d.notifications.is_empty();
    let hover = d.hover;
    let hovered = |part| hover == Hover::Center(part);
    let clear_bg = if hovered(CenterPart::Clear) && !empty {
        0x2a
    } else {
        0x16
    };
    canvas.fill_rounded(clear, 7.0, ui::text_alpha(clear_bg));
    let clear_color = if empty { ui::text_dim() } else { ui::text() };
    ui::label(
        canvas,
        &mut d.fonts,
        tr("Clear all"),
        clear,
        ui::FONT_SMALL,
        true,
        Align::Center,
        clear_color,
    );
    if hovered(CenterPart::Dnd) {
        canvas.fill_rounded(dnd, 6.0, ui::hover_bg());
    }
    let bell = Rect::new(dnd.x + 6, dnd.y, 20, dnd.h);
    draw_bell(canvas, bell, 14.0, ui::text_dim(), true);
    let label = Rect::new(bell.right() + 8, dnd.y, switch.x - bell.right() - 16, dnd.h);
    ui::label(
        canvas,
        &mut d.fonts,
        tr("Do not disturb"),
        label,
        ui::FONT_SIZE,
        false,
        Align::Left,
        ui::text(),
    );
    settings::draw_pill_switch(canvas, switch, d.notifications.dnd);
    canvas.fill_rect(
        Rect::new(frame.x + 12, list.y - 5, frame.w - 24, 1),
        ui::separator(),
    );

    if empty || d.notifications.history().next().is_none() {
        ui::label(
            canvas,
            &mut d.fonts,
            tr("No notifications"),
            list,
            ui::FONT_SIZE,
            false,
            Align::Center,
            ui::text_dim(),
        );
        return Some(frame);
    }
    let max = center_max_scroll(d, list);
    d.notifications.scroll = d.notifications.scroll.clamp(0.0, max);
    canvas.clip(list);
    let now = Instant::now();
    let scale = canvas.scale();
    let count = d.notifications.history().count();
    for index in 0..count {
        let (row, close) = center_row(d, list, index);
        if row.bottom() < list.y || row.y > list.bottom() {
            continue;
        }
        let Desktop {
            notifications,
            fonts,
            icons,
            apps,
            ..
        } = &mut *d;
        let Some(record) = notifications.records.iter_mut().filter(|r| r.stored).nth(index) else {
            continue;
        };
        let icon_rect = Rect::new(
            row.x + 8,
            row.y + (row.h - CENTER_ICON) / 2,
            CENTER_ICON,
            CENTER_ICON,
        );
        match record_icon(record, icons, apps, (CENTER_ICON * scale) as u32) {
            Some(icon) => canvas.draw_image_fit(&icon, icon_rect),
            None => draw_bell(canvas, icon_rect, 15.0, ui::text_dim(), false),
        }
        let note = &record.note;
        let text_x = icon_rect.right() + 10;
        let when = Ago::of(now.saturating_duration_since(record.received)).label();
        let when_w = ui::text_width(fonts, &when, ui::FONT_SMALL, false, scale);
        let when_rect = Rect::new(close.x - 6 - when_w, row.y + 7, when_w, 19);
        ui::label(
            canvas,
            fonts,
            &when,
            when_rect,
            ui::FONT_SMALL,
            false,
            Align::Right,
            ui::text_dim(),
        );
        let summary = if note.summary.is_empty() {
            &note.app_name
        } else {
            &note.summary
        };
        let summary_rect = Rect::new(text_x, row.y + 7, when_rect.x - 8 - text_x, 19);
        ui::label(
            canvas,
            fonts,
            summary,
            summary_rect,
            ui::FONT_SIZE,
            true,
            Align::Left,
            ui::text(),
        );
        let body = note.body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let body_rect = Rect::new(text_x, row.y + 27, close.right() - text_x, 18);
        ui::label(
            canvas,
            fonts,
            body,
            body_rect,
            ui::FONT_SMALL,
            false,
            Align::Left,
            ui::text_dim(),
        );
        if hovered(CenterPart::RowClose(index)) {
            canvas.fill_rounded(close, 11.0, ui::hover_bg());
        }
        if let Some(path) = ui::cross_path(close, 4.0) {
            canvas.stroke_path(&path, 1.5, ui::text_dim());
        }
    }
    canvas.reset_clip();
    if let Some(bar) = ui::Scrollbar::new(list, count as i32 * CENTER_ROW, d.notifications.scroll) {
        bar.draw(canvas, false);
    }
    Some(frame)
}

/// What a point in the center is over, `Some(Hover::None)` for the center's
/// empty space, or `None` outside it.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn center_hit(d: &mut Desktop, x: f64, y: f64) -> Option<Hover> {
    let frame = center_rect(d)?;
    if !frame.contains(x, y) {
        return None;
    }
    let (clear, dnd, _, list) = center_parts(d, frame);
    if clear.contains(x, y) {
        return Some(Hover::Center(CenterPart::Clear));
    }
    if dnd.contains(x, y) {
        return Some(Hover::Center(CenterPart::Dnd));
    }
    if !list.contains(x, y) {
        return Some(Hover::None);
    }
    for index in 0..d.notifications.history().count() {
        let (row, close) = center_row(d, list, index);
        if row.contains(x, y) && close.inflate(2).contains(x, y) {
            return Some(Hover::Center(CenterPart::RowClose(index)));
        }
    }
    Some(Hover::None)
}

/// Handles a press; returns whether it landed in the center.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `button` - The button.
pub fn center_click(d: &mut Desktop, x: f64, y: f64, button: Button) -> bool {
    let Some(hover) = center_hit(d, x, y) else {
        return false;
    };
    if button != Button::Left {
        return true;
    }
    match hover {
        Hover::Center(CenterPart::Clear) => d.clear_notifications(),
        Hover::Center(CenterPart::Dnd) => d.set_dnd(!d.notifications.dnd),
        Hover::Center(CenterPart::RowClose(index)) => {
            let id = d.notifications.history().nth(index).map(|r| r.note.id);
            if let Some(id) = id {
                d.notifications.remove(id, Instant::now());
                d.set_hover(Hover::None);
                d.redraw_panel();
            }
        }
        _ => {}
    }
    true
}

/// Scrolls the center's list; returns whether the wheel was over the center.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `delta` - Scroll amount, positive downwards.
pub fn center_scroll(d: &mut Desktop, x: f64, y: f64, delta: f64) -> bool {
    let Some(frame) = center_rect(d) else {
        return false;
    };
    if !frame.contains(x, y) {
        return false;
    }
    let (_, _, _, list) = center_parts(d, frame);
    let max = center_max_scroll(d, list);
    let step = delta.signum() * ui::SCROLL_STEP;
    d.notifications.scroll = (d.notifications.scroll + step).clamp(0.0, max);
    d.redraw_panel();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: u32, timeout: Option<Duration>) -> Notification {
        Notification {
            id,
            app_name: "App".to_string(),
            summary: format!("Summary {id}"),
            body: String::new(),
            actions: Vec::new(),
            urgency: Urgency::Normal,
            image: None,
            icon_name: None,
            desktop_entry: None,
            transient: false,
            resident: false,
            progress: None,
            timeout,
        }
    }

    fn layout(_: &Record) -> CardLayout {
        CardLayout {
            height: 80,
            ..CardLayout::default()
        }
    }

    fn ids(state: &NotificationState) -> Vec<(u32, bool)> {
        state.cards.iter().map(|c| (c.id, c.closing.is_some())).collect()
    }

    fn closed(state: &mut NotificationState) -> Vec<(u32, CloseReason)> {
        state
            .take_signals()
            .into_iter()
            .filter_map(|r| match r {
                Request::NotificationClosed { id, reason } => Some((id, reason)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn five_cards_show_newest_first_and_the_rest_queue_until_a_closed_card_makes_room() {
        let mut state = NotificationState::default();
        let start = Instant::now();
        for id in 1..=6 {
            state.receive(note(id, None), start);
        }
        state.advance(start, layout);
        assert_eq!(
            ids(&state),
            [(5, false), (4, false), (3, false), (2, false), (1, false)]
        );
        assert_eq!(state.records[0].phase, Phase::Waiting, "the sixth is queued");
        let tops: Vec<i32> = state.cards.iter().map(|c| c.y_to).collect();
        assert_eq!(tops, [0, 88, 176, 264, 352]);
        assert_eq!(state.surface_size(), (CARD_WIDTH + EDGE, 432));

        state.close(3, CloseReason::Dismissed, start);
        assert_eq!(closed(&mut state), [(3, CloseReason::Dismissed)]);
        state.advance(start, layout);
        assert_eq!(
            ids(&state),
            [(5, false), (4, false), (3, true), (2, false), (1, false)],
            "the queued card waits while the closed one fades"
        );

        let later = start + FADE_OUT + Duration::from_millis(10);
        state.advance(later, layout);
        assert_eq!(
            ids(&state),
            [(6, false), (5, false), (4, false), (2, false), (1, false)]
        );
        let above = &state.cards[1];
        assert_eq!((above.y_from, above.y_to), (0, 88), "the cards above make room");
        let below = &state.cards[3];
        assert_eq!((below.y_from, below.y_to), (264, 264), "the cards below stay");
        assert!(state.animating(later));
        let settled = later + MOVE;
        assert!(!state.animating(settled));
        state.advance(settled, layout);
        let tops: Vec<i32> = state.cards.iter().map(|c| c.y_from).collect();
        assert_eq!(tops, [0, 88, 176, 264, 352]);
        assert_eq!(state.surface_size(), (CARD_WIDTH + EDGE, 432));

        state.close(6, CloseReason::Dismissed, settled);
        let gone = settled + FADE_OUT;
        state.advance(gone, layout);
        assert_eq!(ids(&state), [(5, false), (4, false), (2, false), (1, false)]);
        let last = &state.cards[3];
        assert_eq!((last.y_from, last.y_to), (352, 264), "the cards below slide up");
        assert_eq!(
            state.surface_size().1,
            432,
            "the surface keeps room until the slide ends"
        );
        state.advance(gone + MOVE, layout);
        assert_eq!(state.surface_size().1, 344);
    }

    #[test]
    fn an_expired_notification_closes_as_expired_and_stays_in_the_history() {
        let mut state = NotificationState::default();
        let start = Instant::now();
        state.receive(note(1, Some(Duration::from_secs(1))), start);
        state.advance(start, layout);
        assert_eq!(
            state.next_deadline(start),
            Some(start + FRAME),
            "a fresh card animates"
        );
        state.advance(start + Duration::from_secs(2), layout);
        assert_eq!(closed(&mut state), [(1, CloseReason::Expired)]);
        assert!(!state.records[0].live);
        assert_eq!(state.history().count(), 1);
        state.close(1, CloseReason::Closed, start + Duration::from_secs(3));
        assert!(
            closed(&mut state).is_empty(),
            "a closed notification is closed once"
        );
    }

    #[test]
    fn do_not_disturb_holds_popups_back_but_still_stores_and_signals() {
        let mut state = NotificationState {
            dnd: true,
            ..NotificationState::default()
        };
        let start = Instant::now();
        state.receive(note(1, Some(Duration::from_secs(1))), start);
        state.advance(start, layout);
        assert!(state.cards.is_empty());
        assert_eq!(state.unread(), 1);
        state.advance(start + Duration::from_secs(2), layout);
        assert_eq!(closed(&mut state), [(1, CloseReason::Expired)]);
        assert_eq!(state.history().count(), 1);
    }

    #[test]
    fn opening_the_center_marks_everything_read_and_takes_the_cards_down_without_closing() {
        let mut state = NotificationState::default();
        let start = Instant::now();
        state.receive(note(1, None), start);
        state.receive(note(2, None), start);
        state.advance(start, layout);
        assert_eq!(state.unread(), 2);
        state.open_center(start);
        assert_eq!(state.unread(), 0);
        assert!(state.cards.iter().all(|c| c.closing.is_some()));
        assert!(closed(&mut state).is_empty());
        state.receive(note(3, None), start);
        state.advance(start + FADE_OUT * 2, layout);
        assert!(state.cards.is_empty(), "nothing pops up over the open center");
        assert_eq!(state.unread(), 0);
        state.close_center();
        state.advance(start + FADE_OUT * 3, layout);
        assert!(
            state.cards.is_empty(),
            "what the center listed does not pop up afterwards"
        );
    }

    #[test]
    fn a_transient_notification_is_never_listed() {
        let mut state = NotificationState::default();
        let start = Instant::now();
        let mut transient = note(1, None);
        transient.transient = true;
        state.receive(transient, start);
        state.advance(start, layout);
        assert_eq!(ids(&state), [(1, false)]);
        assert_eq!(state.history().count(), 0);
        state.close(1, CloseReason::Dismissed, start);
        assert!(state.records.is_empty());
    }

    #[test]
    fn a_replacement_keeps_its_card_and_takes_the_new_content() {
        let mut state = NotificationState::default();
        let start = Instant::now();
        state.receive(note(1, None), start);
        state.advance(start, layout);
        let mut update = note(1, None);
        update.summary = "Updated".to_string();
        update.progress = Some(40);
        state.receive(update, start + Duration::from_secs(1));
        state.advance(start + Duration::from_secs(1), |_| CardLayout {
            height: 100,
            ..CardLayout::default()
        });
        assert_eq!(ids(&state), [(1, false)]);
        assert_eq!(state.cards[0].height(), 100);
        assert_eq!(state.cards[0].appeared, start, "the card does not appear again");
        assert_eq!(state.records.len(), 1);
        assert_eq!(state.records[0].note.summary, "Updated");
    }

    #[test]
    fn the_history_keeps_fifty_and_closes_what_it_drops() {
        let mut state = NotificationState {
            dnd: true,
            ..NotificationState::default()
        };
        let start = Instant::now();
        for id in 1..=55 {
            state.receive(note(id, None), start);
        }
        assert_eq!(state.history().count(), MAX_HISTORY);
        let dropped: Vec<u32> = closed(&mut state).into_iter().map(|(id, _)| id).collect();
        assert_eq!(dropped, [1, 2, 3, 4, 5]);
        assert_eq!(state.records.last().map(|r| r.note.id), Some(6));
    }

    #[test]
    fn invoking_an_action_signals_it_and_closes_unless_resident() {
        let mut state = NotificationState::default();
        let start = Instant::now();
        state.receive(note(1, None), start);
        let mut resident = note(2, None);
        resident.resident = true;
        state.receive(resident, start);
        state.invoke(1, "default", start);
        state.invoke(2, "open", start);
        let signals = state.take_signals();
        assert!(matches!(
            signals.as_slice(),
            [
                Request::ActionInvoked { id: 1, key: a },
                Request::NotificationClosed { id: 1, reason: CloseReason::Dismissed },
                Request::ActionInvoked { id: 2, key: b },
            ] if a == "default" && b == "open"
        ));
        assert!(state.records.iter().any(|r| r.note.id == 2 && r.live));
    }

    #[test]
    fn relative_times_round_down_to_minutes_hours_and_days() {
        assert_eq!(Ago::of(Duration::from_secs(59)), Ago::Now);
        assert_eq!(Ago::of(Duration::from_secs(150)), Ago::Minutes(2));
        assert_eq!(Ago::of(Duration::from_secs(59 * 60 + 59)), Ago::Minutes(59));
        assert_eq!(Ago::of(Duration::from_secs(3600)), Ago::Hours(1));
        assert_eq!(Ago::of(Duration::from_secs(23 * 3600 + 3599)), Ago::Hours(23));
        assert_eq!(Ago::of(Duration::from_secs(3 * 86400 + 10)), Ago::Days(3));
    }

    #[test]
    fn a_body_wraps_to_at_most_three_lines_at_its_own_newlines_and_words() {
        let mut fonts = Fonts::new();
        let px = ui::FONT_SIZE;
        let long = "word ".repeat(200);
        let lines = wrap(&mut fonts, &long, px, 200.0, 3);
        assert_eq!(lines.len(), 3);
        assert!(lines[2].ends_with('…'));
        assert!(lines.iter().all(|l| !l.starts_with(' ')));
        let lines = wrap(&mut fonts, "one\n\ntwo\nthree", px, 400.0, 3);
        assert_eq!(lines, ["one", "two", "three"]);
        let lines = wrap(&mut fonts, "one\ntwo\nthree\nfour", px, 400.0, 3);
        assert_eq!(lines, ["one", "two", "three…"]);
        assert!(wrap(&mut fonts, "  ", px, 400.0, 3).is_empty());
    }
}
