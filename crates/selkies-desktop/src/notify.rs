//! The notification server: `org.freedesktop.Notifications` on the session
//! bus, served by the shell itself so an application's notification pops up
//! in the shell's colors and gathers in the panel's notification center.
//!
//! The interface follows the Desktop Notifications Specification 1.2:
//! `Notify` takes the application's name and icon, a summary, a body, the
//! actions as key and label pairs, the hints, and the timeout, and answers
//! the notification's id; `CloseNotification` withdraws one;
//! `GetCapabilities` names `body`, `actions`, `icon-static`, and
//! `persistence`; and `NotificationClosed` and `ActionInvoked` tell the
//! sender what became of it.  The hints read are `urgency`, `image-data`,
//! `image-path`, `desktop-entry`, `transient`, `resident`, and `value`, each
//! also under the spelling the specification deprecated where it has one.
//! `body-markup` is not advertised, so a body is plain text, but senders that
//! send markup anyway are common: tags are stripped, keeping their text, and
//! entities decoded ([`strip_markup`]).
//!
//! Everything a caller sends is bounded before it leaves the call: texts
//! are cut to a length, actions to a count, an `image-data` whose header does
//! not describe the bytes it carries is dropped ([`image_from_data`]), an
//! image file is read only below a size and decoded under the image crate's
//! allocation limits, and every image is shrunk to the largest icon the cards
//! draw.  The hints are read into typed fields rather than generic variants,
//! so an image's bytes arrive as one buffer rather than a variant per byte,
//! and a hint of an unexpected type is ignored rather than failing the call.
//!
//! zbus serves calls on its own executor thread, so a call is validated and
//! converted there, and the result handed to the shell's loop through a
//! channel, with a byte on a pipe whose read end is one of the descriptors
//! the loop polls ([`Server::fd`]).  The ids `Notify` answers come from a
//! counter that thread owns, since the call must answer before the loop sees
//! the notification.  The signals are emitted from the loop, each call run to
//! completion with [`zbus::block_on`] as the Settings portal's are.  The name
//! is requested without replacing its owner and without queuing; the shell
//! holds it from its start, so the bus never activates another server, and
//! where another one already runs the shell serves nothing.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use tiny_skia::Pixmap;
use zbus::connection::Builder;
use zbus::export::serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use zbus::fdo::RequestNameFlags;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::{OwnedVariant, Signature, Type, Variant};

/// The well-known name of the notification server.
pub const NAME: &str = "org.freedesktop.Notifications";
/// The object the interface sits on.
pub const PATH: &str = "/org/freedesktop/Notifications";
/// The version of the specification served.
const SPEC_VERSION: &str = "1.2";
/// The optional features served, as `GetCapabilities` names them.
const CAPABILITIES: [&str; 4] = ["body", "actions", "icon-static", "persistence"];
/// Longest application name kept, in characters.
const MAX_APP_NAME: usize = 64;
/// Longest summary kept, in characters.
const MAX_SUMMARY: usize = 256;
/// Longest body kept, in characters, after its markup is stripped.
const MAX_BODY: usize = 2048;
/// Longest action key or label kept, in characters.
const MAX_ACTION_TEXT: usize = 64;
/// Most actions kept.
const MAX_ACTIONS: usize = 8;
/// Largest side an `image-data` image may have.
const MAX_IMAGE_SIDE: i32 = 1024;
/// Largest image file read for an `image-path` or an icon path, in bytes.
const MAX_IMAGE_FILE: u64 = 16 << 20;
/// Longer side of the images kept: the cards' icon at the largest output
/// scale.
pub const IMAGE_SIZE: u32 = 128;
/// The default timeout of a low urgency notification.
const LOW_TIMEOUT: Duration = Duration::from_secs(6);
/// The default timeout of a normal urgency notification.
const NORMAL_TIMEOUT: Duration = Duration::from_secs(8);

/// How urgent a notification is, from its `urgency` hint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Urgency {
    /// Shown dim, and gone sooner.
    Low,
    /// The default.
    #[default]
    Normal,
    /// Marked red, and never expires.
    Critical,
}

/// Why a notification closed, as `NotificationClosed` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// Its timeout ran out.
    Expired = 1,
    /// The user dismissed it.
    Dismissed = 2,
    /// The sender withdrew it with `CloseNotification`.
    Closed = 3,
}

/// A notification as a sender posted it, validated and converted.
#[derive(Clone, Debug)]
pub struct Notification {
    /// The id `Notify` answered.
    pub id: u32,
    /// The sending application's name.
    pub app_name: String,
    /// One line of summary.
    pub summary: String,
    /// The body as plain text, its newlines kept.
    pub body: String,
    /// The actions as key and label pairs, the `default` one among them.
    pub actions: Vec<(String, String)>,
    /// The urgency.
    pub urgency: Urgency,
    /// The image the sender sent or named by path, shrunk to [`IMAGE_SIZE`].
    pub image: Option<Arc<Pixmap>>,
    /// An icon name to look up in the icon theme when there is no image.
    pub icon_name: Option<String>,
    /// The desktop file ID of the sender's entry, whose icon is the last
    /// resort, without its `.desktop` suffix.
    pub desktop_entry: Option<String>,
    /// Whether it bypasses the notification center.
    pub transient: bool,
    /// Whether invoking an action leaves it open.
    pub resident: bool,
    /// Progress in percent, drawn as a bar.
    pub progress: Option<u8>,
    /// How long it shows before it expires; `None` never.
    pub timeout: Option<Duration>,
}

impl Notification {
    /// The label of the action invoked by clicking the card, when it has one.
    pub fn default_action(&self) -> Option<&str> {
        self.actions
            .iter()
            .find(|(key, _)| key == "default")
            .map(|(_, label)| label.as_str())
    }
}

/// What a sender asked of the shell, as the loop receives it.
#[derive(Debug)]
pub enum Incoming {
    /// Show a notification, or replace the one with its id.
    Notify(Box<Notification>),
    /// Withdraw a notification.
    Close(u32),
}

/// The duration a notification shows for: the `expire_timeout` in
/// milliseconds, `-1` choosing the urgency's default and `0` never expiring;
/// a critical notification never expires, as the specification has it.
///
/// # Arguments
/// * `expire_timeout` - The timeout the sender passed.
/// * `urgency` - The notification's urgency.
pub fn timeout(expire_timeout: i32, urgency: Urgency) -> Option<Duration> {
    match (urgency, expire_timeout) {
        (Urgency::Critical, _) | (_, 0) => None,
        (Urgency::Low, ..0) => Some(LOW_TIMEOUT),
        (_, ..0) => Some(NORMAL_TIMEOUT),
        (_, ms) => Some(Duration::from_millis(ms as u64)),
    }
}

/// Plain text from a body that may carry markup: every tag is dropped and
/// its text kept, so `<b>`, `<i>`, and `<a href>` read as their words, and
/// the five named entities and numeric ones are decoded.  A `<` that cannot
/// open a tag, one not followed by a letter, `/`, or `!`, is text, so a
/// plain body comparing two values survives; an unknown entity is text too.
///
/// # Arguments
/// * `text` - The body as sent.
pub fn strip_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if c == '<'
            && rest[1..]
                .chars()
                .next()
                .is_some_and(|n| n.is_ascii_alphabetic() || n == '/' || n == '!')
            && let Some(end) = rest.find('>')
        {
            let tag = rest[1..end].trim_start_matches('/').to_ascii_lowercase();
            if tag == "br" || tag.starts_with("br ") || tag.starts_with("br/") {
                out.push('\n');
            }
            rest = &rest[end + 1..];
            continue;
        }
        if c == '&'
            && let Some((end, _)) = rest.char_indices().take(12).find(|(_, c)| *c == ';')
            && let Some(decoded) = entity(&rest[1..end])
        {
            out.push(decoded);
            rest = &rest[end + 1..];
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// The character an entity names, without its `&` and `;`.
fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let number = name.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            char::from_u32(code).filter(|c| !c.is_control() || *c == '\n')
        }
    }
}

/// A text cut to a number of characters, its control characters other than
/// newlines dropped, and its newlines too unless kept.
fn clean(text: &str, max: usize, newlines: bool) -> String {
    text.chars()
        .map(|c| match c {
            '\t' => ' ',
            '\n' if !newlines => ' ',
            c => c,
        })
        .filter(|c| *c == '\n' || !c.is_control())
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

/// The pixels of an `image-data` hint as a premultiplied pixmap, or `None`
/// when the header does not describe the bytes: both sides must be 1 to
/// [`MAX_IMAGE_SIDE`], the samples 8 bits, the channels 3, or 4 with alpha
/// (a fourth channel without `has_alpha` is padding), the row stride at least
/// a row's bytes, and the rows the stride spans inside the data.  The last
/// row need not be padded to the stride, as GdkPixbuf sends it.
///
/// # Arguments
/// * `width` - Pixels per row.
/// * `height` - Rows.
/// * `rowstride` - Bytes from one row to the next.
/// * `has_alpha` - Whether the fourth channel is alpha.
/// * `bits` - Bits per sample.
/// * `channels` - Samples per pixel.
/// * `data` - The rows.
pub fn image_from_data(
    width: i32,
    height: i32,
    rowstride: i32,
    has_alpha: bool,
    bits: i32,
    channels: i32,
    data: &[u8],
) -> Option<Pixmap> {
    if !(1..=MAX_IMAGE_SIDE).contains(&width)
        || !(1..=MAX_IMAGE_SIDE).contains(&height)
        || bits != 8
        || !(channels == 4 || channels == 3 && !has_alpha)
    {
        return None;
    }
    let (width, height, channels) = (width as usize, height as usize, channels as usize);
    let row = width * channels;
    let stride = usize::try_from(rowstride).ok().filter(|s| *s >= row)?;
    if (height - 1) * stride + row > data.len() {
        return None;
    }
    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        let line = &data[y * stride..y * stride + row];
        for px in line.chunks_exact(channels) {
            let alpha = if has_alpha { px[3] } else { 0xff };
            rgba.extend_from_slice(&[px[0], px[1], px[2], alpha]);
        }
    }
    crate::render::pixmap_from_rgba(width as u32, height as u32, &rgba)
}

/// An image shrunk so its longer side is at most [`IMAGE_SIZE`].
fn shrink(image: Pixmap) -> Pixmap {
    if image.width().max(image.height()) <= IMAGE_SIZE {
        return image;
    }
    crate::render::resize_pixmap(&image, IMAGE_SIZE).unwrap_or(image)
}

/// The path a `file://` URI names, its percent escapes decoded, or `None`
/// for another scheme or a remote host.
fn uri_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let path = if rest.starts_with('/') {
        rest
    } else {
        rest.strip_prefix("localhost")?
    };
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = path
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// An image or icon reference: a file to load when it is an absolute path
/// or a `file://` URI, else an icon name.
enum Reference {
    /// An image file.
    File(String),
    /// A name for the icon theme.
    Name(String),
}

/// Reads an `image-path` hint or an `app_icon` argument.
fn reference(text: &str) -> Option<Reference> {
    let text = text.trim();
    if text.is_empty() || text.len() > 4096 {
        return None;
    }
    if text.starts_with("file://") {
        return uri_path(text).map(Reference::File);
    }
    if text.starts_with('/') {
        return Some(Reference::File(text.to_string()));
    }
    (!text.contains('/') && !text.contains("..")).then(|| Reference::Name(text.to_string()))
}

/// Loads an image file of a bounded size, shrunk to [`IMAGE_SIZE`].
fn load_image(path: &str) -> Option<Pixmap> {
    let path = std::path::Path::new(path);
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_IMAGE_FILE {
        return None;
    }
    let is_svg = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg") || e.eq_ignore_ascii_case("svgz"));
    if is_svg {
        return crate::render::load_svg(path, IMAGE_SIZE);
    }
    crate::render::load_raster(path, 0).map(shrink)
}

/// The fields of `image-data`: width, height, row stride, whether it has
/// alpha, bits per sample, channels, and the rows.
type RawImage = (i32, i32, i32, bool, i32, i32, Vec<u8>);

/// The `image-data` hint read as its typed fields when the variant carries
/// them, and as nothing when it carries anything else.
#[derive(Debug, Default)]
struct ImageHint(Option<RawImage>);

impl Type for ImageHint {
    const SIGNATURE: &'static Signature = &Signature::Variant;
}

impl<'de> zbus::export::serde::Deserialize<'de> for ImageHint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<ImageHint, D::Error> {
        deserializer.deserialize_struct("Variant", &["signature", "value"], ImageVisitor)
    }
}

/// Reads a variant's signature, then its value as [`RawImage`] when the
/// signature is that one's and skips it otherwise.
struct ImageVisitor;

impl<'de> Visitor<'de> for ImageVisitor {
    type Value = ImageHint;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a variant")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<ImageHint, A::Error> {
        let signature: Signature = seq
            .next_element()?
            .ok_or_else(|| de::Error::invalid_length(0, &self))?;
        if &signature == <RawImage as Type>::SIGNATURE {
            return Ok(ImageHint(seq.next_element()?));
        }
        seq.next_element::<IgnoredAny>()?;
        Ok(ImageHint(None))
    }
}

/// The hints the server reads.  The scalar ones are taken as variants and
/// read leniently, an integer of any width or a boolean where one is meant;
/// every other key is skipped unread.
#[derive(Debug, Default)]
struct Hints {
    urgency: Option<OwnedVariant>,
    image_data: Option<RawImage>,
    image_path: Option<OwnedVariant>,
    desktop_entry: Option<OwnedVariant>,
    transient: Option<OwnedVariant>,
    resident: Option<OwnedVariant>,
    value: Option<OwnedVariant>,
}

impl Type for Hints {
    const SIGNATURE: &'static Signature = <HashMap<String, OwnedVariant> as Type>::SIGNATURE;
}

impl<'de> zbus::export::serde::Deserialize<'de> for Hints {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Hints, D::Error> {
        deserializer.deserialize_map(HintsVisitor)
    }
}

/// Reads the hints dictionary key by key, each key's current spelling
/// winning over the deprecated one.
struct HintsVisitor;

impl<'de> Visitor<'de> for HintsVisitor {
    type Value = Hints;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a dictionary of hints")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Hints, A::Error> {
        let mut hints = Hints::default();
        let mut legacy_image = None;
        let mut legacy_path = None;
        while let Some(key) = map.next_key::<String>()? {
            let slot = match key.as_str() {
                "image-data" => {
                    hints.image_data = map.next_value::<ImageHint>()?.0;
                    continue;
                }
                "image_data" => {
                    legacy_image = map.next_value::<ImageHint>()?.0;
                    continue;
                }
                "urgency" => &mut hints.urgency,
                "image-path" => &mut hints.image_path,
                "image_path" => &mut legacy_path,
                "desktop-entry" => &mut hints.desktop_entry,
                "transient" => &mut hints.transient,
                "resident" => &mut hints.resident,
                "value" => &mut hints.value,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                    continue;
                }
            };
            *slot = Some(map.next_value::<OwnedVariant>()?);
        }
        hints.image_data = hints.image_data.or(legacy_image);
        hints.image_path = hints.image_path.or(legacy_path);
        Ok(hints)
    }
}

/// An integer hint of any width.
fn int(value: &Variant<'_>) -> Option<i64> {
    match value {
        Variant::U8(v) => Some((*v).into()),
        Variant::I16(v) => Some((*v).into()),
        Variant::U16(v) => Some((*v).into()),
        Variant::I32(v) => Some((*v).into()),
        Variant::U32(v) => Some((*v).into()),
        Variant::I64(v) => Some(*v),
        Variant::U64(v) => i64::try_from(*v).ok(),
        Variant::Variant(inner) => int(inner),
        _ => None,
    }
}

/// A boolean hint, or an integer standing for one.
fn flag(value: &Variant<'_>) -> bool {
    match value {
        Variant::Bool(v) => *v,
        Variant::Variant(inner) => flag(inner),
        other => int(other).is_some_and(|v| v != 0),
    }
}

/// A string hint.
fn text(value: &Variant<'_>) -> Option<String> {
    match value {
        Variant::Str(s) => Some(s.as_str().to_string()),
        Variant::Variant(inner) => text(inner),
        _ => None,
    }
}

/// Builds the notification a `Notify` call describes.
///
/// # Arguments
/// * `id` - The id the call answers.
/// * `call` - The call's arguments.
fn build(id: u32, call: NotifyCall) -> Notification {
    let hints = call.hints;
    let urgency = match hints.urgency.as_deref().and_then(int) {
        Some(0) => Urgency::Low,
        Some(2) => Urgency::Critical,
        _ => Urgency::Normal,
    };
    let image = hints
        .image_data
        .and_then(|(w, h, stride, alpha, bits, channels, data)| {
            image_from_data(w, h, stride, alpha, bits, channels, &data)
        })
        .map(shrink);
    let mut icon_name = None;
    let mut image = image.map(Arc::new);
    let path = hints.image_path.as_deref().and_then(text);
    for candidate in [path, Some(call.app_icon)].into_iter().flatten() {
        if image.is_some() || icon_name.is_some() {
            break;
        }
        match reference(&candidate) {
            Some(Reference::File(path)) => image = load_image(&path).map(Arc::new),
            Some(Reference::Name(name)) => icon_name = Some(name),
            None => {}
        }
    }
    let desktop_entry = hints
        .desktop_entry
        .as_deref()
        .and_then(text)
        .map(|entry| entry.trim_end_matches(".desktop").to_string())
        .filter(|entry| {
            !entry.is_empty() && entry.len() <= 255 && !entry.starts_with('.') && !entry.contains(['/', '\0'])
        });
    let actions = call
        .actions
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[key, label]| {
            (
                clean(key, MAX_ACTION_TEXT, false),
                clean(label, MAX_ACTION_TEXT, false),
            )
        })
        .filter(|(key, _)| !key.is_empty())
        .take(MAX_ACTIONS)
        .collect();
    Notification {
        id,
        app_name: clean(&call.app_name, MAX_APP_NAME, false),
        summary: clean(&call.summary, MAX_SUMMARY, false),
        body: clean(&strip_markup(&call.body), MAX_BODY, true),
        actions,
        urgency,
        image,
        icon_name,
        desktop_entry,
        transient: hints.transient.as_deref().is_some_and(flag),
        resident: hints.resident.as_deref().is_some_and(flag),
        progress: hints
            .value
            .as_deref()
            .and_then(int)
            .map(|v| v.clamp(0, 100) as u8),
        timeout: timeout(call.expire_timeout, urgency),
    }
}

/// The arguments of a `Notify` call that [`build`] reads.
struct NotifyCall {
    app_name: String,
    app_icon: String,
    summary: String,
    body: String,
    actions: Vec<String>,
    hints: Hints,
    expire_timeout: i32,
}

/// The `org.freedesktop.Notifications` interface, handing what it is sent
/// to the shell's loop.
struct Notifications {
    /// The channel to the loop.
    sender: Sender<Incoming>,
    /// The write end of the pipe that wakes the loop.
    wake: File,
    /// The last id answered.
    last_id: AtomicU32,
}

impl Notifications {
    /// Hands something to the loop and wakes it.  A full pipe already wakes
    /// the loop, so a write that would block is dropped.
    fn send(&self, incoming: Incoming) {
        if self.sender.send(incoming).is_ok() {
            let _ = (&self.wake).write(&[1]);
        }
    }

    /// The id for a call: the one it replaces, else the next unused one,
    /// never zero.
    fn assign(&self, replaces_id: u32) -> u32 {
        if replaces_id != 0 {
            self.last_id.fetch_max(replaces_id, Ordering::Relaxed);
            return replaces_id;
        }
        loop {
            let id = self.last_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            if id != 0 {
                return id;
            }
        }
    }
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    /// The optional features served.
    fn get_capabilities(&self) -> Vec<String> {
        CAPABILITIES.iter().map(|c| c.to_string()).collect()
    }

    /// Shows a notification, or replaces the one `replaces_id` names.
    // The Desktop Notifications Specification fixes Notify's eight arguments.
    #[allow(clippy::too_many_arguments)]
    #[zbus(out_args("id"))]
    fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: Hints,
        expire_timeout: i32,
    ) -> u32 {
        let id = self.assign(replaces_id);
        let call = NotifyCall {
            app_name,
            app_icon,
            summary,
            body,
            actions,
            hints,
            expire_timeout,
        };
        self.send(Incoming::Notify(Box::new(build(id, call))));
        id
    }

    /// Withdraws a notification; the shell signals its closing when it was
    /// still open.
    fn close_notification(&self, id: u32) {
        self.send(Incoming::Close(id));
    }

    /// The server's name, vendor, version, and the specification's version.
    #[zbus(out_args("name", "vendor", "version", "spec_version"))]
    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "selkies-desktop".to_string(),
            "Selkies".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
            SPEC_VERSION.to_string(),
        )
    }

    /// A notification closed, for a [`CloseReason`].
    #[zbus(signal)]
    async fn notification_closed(emitter: &SignalEmitter<'_>, id: u32, reason: u32) -> zbus::Result<()>;

    /// The user invoked one of a notification's actions.
    #[zbus(signal)]
    async fn action_invoked(emitter: &SignalEmitter<'_>, id: u32, action_key: &str) -> zbus::Result<()>;
}

/// The notification server on the session bus, served while this value
/// lives.
pub struct Server {
    /// The served interface, through which its signals are emitted; it
    /// holds the connection, and with it the name.
    interface: InterfaceRef<Notifications>,
    /// What the interface handed over.
    receiver: Receiver<Incoming>,
    /// The read end of the pipe that wakes the loop.
    wake: File,
}

impl Server {
    /// Serves the notification interface on the session bus, or returns
    /// `None`, with a line under `SELKIES_DESKTOP_DEBUG`, when there is no
    /// bus or another process holds the name.
    pub fn start() -> Option<Server> {
        match Server::serve(Builder::session()) {
            Ok(server) => Some(server),
            Err(error) => {
                if std::env::var_os("SELKIES_DESKTOP_DEBUG").is_some() {
                    eprintln!("selkies-desktop: not serving notifications: {error}");
                }
                None
            }
        }
    }

    /// Serves the interface on the bus a builder connects to.
    ///
    /// # Errors
    /// The pipe's or the connection's failure, or
    /// [`zbus::Error::NameTaken`] when the name has an owner.
    fn serve(builder: Builder<'_>) -> zbus::Result<Server> {
        let mut fds = [0 as RawFd; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
            return Err(zbus::Error::InputOutput(
                Arc::new(std::io::Error::last_os_error()),
            ));
        }
        // SAFETY: pipe2 succeeded, so both descriptors are open and owned by nothing else.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let (sender, receiver) = channel();
        let notifications = Notifications {
            sender,
            wake: File::from(write),
            last_id: AtomicU32::new(0),
        };
        zbus::block_on(async {
            let connection = builder.serve_at(PATH, notifications).build().await?;
            connection
                .request_name_with_flags(NAME, RequestNameFlags::DO_NOT_QUEUE)
                .await?;
            let interface = connection
                .object_server()
                .interface::<_, Notifications>(PATH)
                .await?;
            Ok(Server {
                interface,
                receiver,
                wake: File::from(read),
            })
        })
    }

    /// The descriptor that becomes readable when something arrived.
    pub fn fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// Takes everything that arrived, clearing the wake pipe.
    pub fn drain(&self) -> Vec<Incoming> {
        let mut buf = [0u8; 64];
        while (&self.wake).read(&mut buf).is_ok_and(|n| n > 0) {}
        self.receiver.try_iter().collect()
    }

    /// Signals that a notification closed.
    ///
    /// # Arguments
    /// * `id` - The notification.
    /// * `reason` - Why it closed.
    pub fn closed(&self, id: u32, reason: CloseReason) {
        let result = zbus::block_on(self.interface.notification_closed(id, reason as u32));
        report(result);
    }

    /// Signals that the user invoked an action.
    ///
    /// # Arguments
    /// * `id` - The notification.
    /// * `key` - The action's key.
    pub fn invoked(&self, id: u32, key: &str) {
        let result = zbus::block_on(self.interface.action_invoked(id, key));
        report(result);
    }
}

/// Reports a signal that could not be sent, under `SELKIES_DESKTOP_DEBUG`.
fn report(result: zbus::Result<()>) {
    if let Err(error) = result
        && std::env::var_os("SELKIES_DESKTOP_DEBUG").is_some()
    {
        eprintln!("selkies-desktop: cannot signal a notification: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::BufRead;
    use std::process::{Child, Command, Stdio};

    use zbus::{Connection, Message};

    #[test]
    fn markup_is_stripped_to_its_text_with_entities_decoded_and_newlines_kept() {
        assert_eq!(
            strip_markup("12 files copied to <b>Downloads</b>\n<i>in</i> <a href=\"file:///x\">2 s</a>"),
            "12 files copied to Downloads\nin 2 s"
        );
        assert_eq!(
            strip_markup("Tom &amp; Jerry &lt;3 &#65;&#x42;"),
            "Tom & Jerry <3 AB"
        );
        assert_eq!(strip_markup("a < b and c > d"), "a < b and c > d");
        assert_eq!(strip_markup("AT&T &bogus; &amp"), "AT&T &bogus; &amp");
        assert_eq!(strip_markup("one<br/>two<br>three"), "one\ntwo\nthree");
        assert_eq!(strip_markup("<img src=\"x.png\" alt=\"pic\"/>done"), "done");
        assert_eq!(strip_markup("&ü;ünïcode"), "&ü;ünïcode");
    }

    #[test]
    fn image_data_is_accepted_as_rgb_and_rgba_and_premultiplied() {
        let rgb = image_from_data(2, 1, 6, false, 8, 3, &[255, 0, 0, 0, 255, 0]).unwrap();
        assert_eq!((rgb.width(), rgb.height()), (2, 1));
        assert_eq!(&rgb.data()[..8], &[255, 0, 0, 255, 0, 255, 0, 255]);
        let rgba = image_from_data(1, 2, 4, true, 8, 4, &[200, 100, 0, 128, 0, 0, 255, 0]).unwrap();
        assert_eq!(&rgba.data()[..4], &[100, 50, 0, 128]);
        assert_eq!(&rgba.data()[4..], &[0, 0, 0, 0]);
        let padded = image_from_data(1, 1, 4, false, 8, 4, &[1, 2, 3, 0]).unwrap();
        assert_eq!(padded.data(), &[1, 2, 3, 255]);
        let unpadded_last_row = vec![9; 8 + 6];
        assert!(image_from_data(2, 2, 8, false, 8, 3, &unpadded_last_row).is_some());
    }

    #[test]
    fn image_data_whose_rows_run_past_the_data_or_whose_header_is_wrong_is_rejected() {
        let data = vec![0; 16 * 4 - 1];
        assert!(image_from_data(4, 4, 16, true, 8, 4, &data).is_none());
        assert!(image_from_data(4, 4, 16, true, 8, 4, &[data.as_slice(), &[0]].concat()).is_some());
        assert!(
            image_from_data(4, 4, 15, true, 8, 4, &[0; 64]).is_none(),
            "stride under a row"
        );
        assert!(image_from_data(4, 4, -16, true, 8, 4, &[0; 64]).is_none());
        assert!(image_from_data(0, 4, 16, true, 8, 4, &[0; 64]).is_none());
        assert!(image_from_data(1025, 1, 4100, true, 8, 4, &[0; 4100]).is_none());
        assert!(image_from_data(4, 4, 16, true, 16, 4, &[0; 64]).is_none());
        assert!(
            image_from_data(4, 4, 12, true, 8, 3, &[0; 48]).is_none(),
            "alpha needs 4 channels"
        );
        assert!(image_from_data(4, 4, 8, false, 8, 2, &[0; 32]).is_none());
        assert!(image_from_data(i32::MAX, i32::MAX, i32::MAX, true, 8, 4, &[0; 64]).is_none());
    }

    #[test]
    fn timeouts_follow_the_urgency_unless_the_sender_names_one() {
        assert_eq!(timeout(-1, Urgency::Low), Some(Duration::from_secs(6)));
        assert_eq!(timeout(-1, Urgency::Normal), Some(Duration::from_secs(8)));
        assert_eq!(timeout(-1, Urgency::Critical), None);
        assert_eq!(timeout(0, Urgency::Normal), None);
        assert_eq!(timeout(0, Urgency::Low), None);
        assert_eq!(timeout(2500, Urgency::Normal), Some(Duration::from_millis(2500)));
        assert_eq!(timeout(2500, Urgency::Low), Some(Duration::from_millis(2500)));
        assert_eq!(timeout(2500, Urgency::Critical), None);
        assert_eq!(timeout(-7, Urgency::Normal), Some(Duration::from_secs(8)));
    }

    #[test]
    fn image_references_are_files_or_icon_names() {
        assert!(
            matches!(reference("file:///tmp/a%20b.png"), Some(Reference::File(p)) if p == "/tmp/a b.png")
        );
        assert!(matches!(reference("file://localhost/x.png"), Some(Reference::File(p)) if p == "/x.png"));
        assert!(reference("file://otherhost/x.png").is_none());
        assert!(matches!(reference("/usr/share/x.svg"), Some(Reference::File(_))));
        assert!(
            matches!(reference("dialog-information"), Some(Reference::Name(n)) if n == "dialog-information")
        );
        assert!(reference("../etc/passwd").is_none());
        assert!(reference("  ").is_none());
    }

    /// A private session bus, ended with the test.
    struct Bus {
        daemon: Child,
        address: String,
    }

    impl Bus {
        fn start() -> Option<Bus> {
            let mut daemon = Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(Stdio::piped())
                .spawn()
                .ok()?;
            let mut address = String::new();
            std::io::BufReader::new(daemon.stdout.take()?)
                .read_line(&mut address)
                .ok()?;
            Some(Bus {
                daemon,
                address: address.trim().to_string(),
            })
        }

        fn builder(&self) -> Builder<'static> {
            Builder::address(self.address.as_str())
        }
    }

    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.daemon.kill();
            let _ = self.daemon.wait();
        }
    }

    fn call<B>(client: &Connection, method: &str, body: &B) -> zbus::Result<Message>
    where
        B: zbus::export::serde::Serialize + zbus::DynamicType,
    {
        zbus::block_on(client.call_method(Some(NAME), PATH, Some(NAME), method, body))
    }

    /// Waits for the server's pipe and takes what arrived.
    fn receive(server: &Server) -> Vec<Incoming> {
        let mut fd = libc::pollfd {
            fd: server.fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe { libc::poll(&mut fd, 1, 5000) };
        server.drain()
    }

    type NotifyArgs<'a> = (
        &'a str,
        u32,
        &'a str,
        &'a str,
        &'a str,
        Vec<&'a str>,
        HashMap<&'a str, Variant<'a>>,
        i32,
    );

    #[test]
    fn the_server_answers_notify_with_ids_and_hands_over_validated_notifications() {
        let Some(bus) = Bus::start() else {
            eprintln!("skipped: no dbus-daemon");
            return;
        };
        let server = Server::serve(bus.builder()).unwrap();
        let client = zbus::block_on(bus.builder().build()).unwrap();

        let reply = call(&client, "GetServerInformation", &()).unwrap();
        let info: (String, String, String, String) = reply.body().deserialize().unwrap();
        assert_eq!(info.0, "selkies-desktop");
        assert_eq!(info.3, "1.2");
        let reply = call(&client, "GetCapabilities", &()).unwrap();
        let capabilities: Vec<String> = reply.body().deserialize().unwrap();
        assert_eq!(capabilities, CAPABILITIES);

        let pixels: Vec<u8> = [10u8, 20, 30, 255].repeat(4);
        let mut hints = HashMap::new();
        hints.insert("urgency", Variant::from(2u8));
        hints.insert(
            "image-data",
            Variant::from((2i32, 2i32, 8i32, true, 8i32, 4i32, pixels)),
        );
        hints.insert("desktop-entry", Variant::from("org.example.App.desktop"));
        hints.insert("value", Variant::from(140i32));
        hints.insert("x-unknown", Variant::from(7u64));
        let args: NotifyArgs = (
            "Files",
            0,
            "folder",
            "Copy finished",
            "12 files copied to <b>Downloads</b>",
            vec!["default", "Open", "undo", "Undo", "dangling"],
            hints,
            -1,
        );
        let id: u32 = call(&client, "Notify", &args)
            .unwrap()
            .body()
            .deserialize()
            .unwrap();
        assert_eq!(id, 1);
        let Some(Incoming::Notify(note)) = receive(&server).pop() else {
            panic!("no notification arrived");
        };
        assert_eq!(note.id, 1);
        assert_eq!(note.body, "12 files copied to Downloads");
        assert_eq!(note.urgency, Urgency::Critical);
        assert_eq!(note.timeout, None);
        assert_eq!(note.image.as_ref().map(|i| (i.width(), i.height())), Some((2, 2)));
        assert_eq!(note.icon_name, None, "the image wins over the app icon");
        assert_eq!(note.desktop_entry.as_deref(), Some("org.example.App"));
        assert_eq!(note.progress, Some(100));
        assert_eq!(
            note.actions,
            vec![("default".into(), "Open".into()), ("undo".into(), "Undo".into())]
        );
        assert_eq!(note.default_action(), Some("Open"));

        let mut hints = HashMap::new();
        hints.insert("urgency", Variant::from(0i32));
        hints.insert("image-data", Variant::from("not an image"));
        hints.insert("transient", Variant::from(true));
        let args: NotifyArgs = ("App", 1, "dialog-information", "Again", "", vec![], hints, 0);
        let id: u32 = call(&client, "Notify", &args)
            .unwrap()
            .body()
            .deserialize()
            .unwrap();
        assert_eq!(id, 1, "a replacement keeps its id");
        let Some(Incoming::Notify(note)) = receive(&server).pop() else {
            panic!("no replacement arrived");
        };
        assert_eq!(note.urgency, Urgency::Low);
        assert!(note.image.is_none() && note.transient);
        assert_eq!(note.icon_name.as_deref(), Some("dialog-information"));
        assert_eq!(note.timeout, None);

        let args: NotifyArgs = ("App", 0, "", "Next", "", vec![], HashMap::new(), 100);
        let id: u32 = call(&client, "Notify", &args)
            .unwrap()
            .body()
            .deserialize()
            .unwrap();
        assert_eq!(id, 2);
        receive(&server);

        call(&client, "CloseNotification", &(2u32,)).unwrap();
        assert!(matches!(receive(&server).as_slice(), [Incoming::Close(2)]));

        assert!(matches!(
            Server::serve(bus.builder()),
            Err(zbus::Error::NameTaken)
        ));
    }
}
