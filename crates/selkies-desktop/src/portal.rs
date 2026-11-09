//! The desktop Settings portal, served by the shell itself so applications
//! flip between light and dark and take the accent the moment the shell's
//! theme changes.
//!
//! Toolkits that follow the desktop's scheme ask
//! `org.freedesktop.portal.Settings` at `/org/freedesktop/portal/desktop` on
//! the name `org.freedesktop.portal.Desktop`, and listen there for its
//! `SettingChanged` signal:
//!
//! * libadwaita reads `org.freedesktop.appearance`'s `color-scheme`,
//!   `accent-color`, and `contrast` there before GSettings, and GTK 4 its
//!   dark preference.
//! * Qt 6 takes its color scheme from `org.freedesktop.appearance`, and its
//!   gtk3 platform theme the GTK theme name from
//!   `org.gnome.desktop.interface`.
//! * Chromium and Electron take `prefers-color-scheme` and the accent from
//!   `org.freedesktop.appearance`.
//! * A sandboxed GTK, which cannot read the user's GSettings, reads the
//!   `org.gnome.desktop.interface` keys through the portal instead.
//!
//! xdg-desktop-portal is a frontend that relays this interface from a
//! backend, and the images ship neither, so the shell owns the frontend's
//! name itself rather than installing a portal to be the backend of: the
//! values are the shell's own [`Decision`], the one [`crate::toolkits`]
//! writes to the GTK files and GSettings, so every path a toolkit takes
//! reads the same scheme.  The name is requested without replacing its owner
//! and without queuing, so an image that runs a portal of its own keeps it
//! and the shell serves nothing.  Only the Settings interface is served: a
//! toolkit that probes another portal interface's `version` finds none and
//! keeps its own dialogs.
//!
//! zbus serves calls on its own executor thread, so the decision sits behind
//! a mutex the interface reads and [`Portal::update`] replaces; each call
//! from the shell's loop runs to completion with [`zbus::block_on`].  The
//! bus is the session's, through `DBUS_SESSION_BUS_ADDRESS`, which the
//! window manager exports to the shell; without one, or with the name
//! taken, nothing is served and a line says why under
//! `SELKIES_DESKTOP_DEBUG`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use zbus::connection::Builder;
use zbus::fdo::RequestNameFlags;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::{OwnedVariant, Variant};

use crate::toolkits::Decision;

/// The well-known name of the portal frontend.
const NAME: &str = "org.freedesktop.portal.Desktop";
/// The object the portal interfaces sit on.
const PATH: &str = "/org/freedesktop/portal/desktop";
/// The version of the Settings interface served, the one with `ReadOne`.
const VERSION: u32 = 2;
/// The toolkit-neutral namespace of the scheme and the accent.
const APPEARANCE: &str = "org.freedesktop.appearance";
/// The GSettings schema the portal relays for GTK.
const INTERFACE: &str = "org.gnome.desktop.interface";

/// A setting's value, in the D-Bus type the portal gives it.
#[derive(Clone, Debug, PartialEq)]
enum Setting {
    /// A `u`.
    U32(u32),
    /// A `b`.
    Bool(bool),
    /// An `s`.
    Str(String),
    /// An sRGB color as the portal's `(ddd)`, each channel in 0..1.
    Rgb(f64, f64, f64),
}

impl Setting {
    /// The value as a D-Bus variant.
    fn variant(&self) -> Variant<'static> {
        match self {
            Setting::U32(value) => Variant::from(*value),
            Setting::Bool(value) => Variant::from(*value),
            Setting::Str(value) => Variant::from(value.clone()),
            Setting::Rgb(r, g, b) => Variant::from((*r, *g, *b)),
        }
    }
}

/// A namespace, a key in it, and its value.
type Entry = (&'static str, &'static str, Setting);

/// Every setting a decision serves, by namespace and key.
fn settings(decision: &Decision) -> Vec<Entry> {
    let accent = decision.accent;
    let (scheme, gnome_scheme) = if decision.dark {
        (1, "prefer-dark")
    } else {
        (2, "prefer-light")
    };
    let mut entries = vec![
        (APPEARANCE, "color-scheme", Setting::U32(scheme)),
        (
            APPEARANCE,
            "accent-color",
            Setting::Rgb(accent.red().into(), accent.green().into(), accent.blue().into()),
        ),
        (APPEARANCE, "contrast", Setting::U32(0)),
        (INTERFACE, "color-scheme", Setting::Str(gnome_scheme.to_string())),
        (INTERFACE, "gtk-theme", Setting::Str(decision.gtk_theme.clone())),
    ];
    if let Some(icons) = &decision.icon_theme {
        entries.push((INTERFACE, "icon-theme", Setting::Str(icons.clone())));
    }
    if let Some(cursors) = &decision.cursor_theme {
        entries.push((INTERFACE, "cursor-theme", Setting::Str(cursors.clone())));
    }
    entries.push((INTERFACE, "enable-animations", Setting::Bool(true)));
    entries
}

/// Whether a namespace is one `ReadAll` was asked for, as the portal
/// defines it: an empty list or an empty pattern matches every namespace, a
/// pattern ending in `*` matches by the prefix before it, and any other
/// pattern matches itself.
fn namespace_matches(namespace: &str, patterns: &[String]) -> bool {
    patterns.is_empty()
        || patterns.iter().any(|pattern| {
            pattern.is_empty()
                || namespace == pattern
                || pattern
                    .strip_suffix('*')
                    .is_some_and(|prefix| namespace.starts_with(prefix))
        })
}

/// The errors the interface answers with, under the portal's error names.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.portal.Error")]
enum PortalError {
    /// A failure of the bus itself.
    #[zbus(error)]
    ZBus(zbus::Error),
    /// The namespace has no such key.
    NotFound(String),
}

/// The `org.freedesktop.portal.Settings` interface over the shared decision.
struct Settings {
    /// The decision served, replaced by [`Portal::update`].
    decision: Arc<Mutex<Decision>>,
}

impl Settings {
    /// The settings of the current decision.
    fn current(&self) -> Vec<Entry> {
        settings(&self.decision.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// The value of a key, or the portal's `NotFound` error.
    fn lookup(&self, namespace: &str, key: &str) -> Result<Variant<'static>, PortalError> {
        self.current()
            .into_iter()
            .find(|(ns, k, _)| *ns == namespace && *k == key)
            .map(|(_, _, setting)| setting.variant())
            .ok_or_else(|| PortalError::NotFound(format!("Requested setting {namespace}.{key} not found")))
    }
}

#[zbus::interface(name = "org.freedesktop.portal.Settings")]
impl Settings {
    /// The interface version.
    #[zbus(property(emits_changed_signal = "const"), name = "version")]
    fn version(&self) -> u32 {
        VERSION
    }

    /// Every setting of the namespaces the patterns match.
    fn read_all(
        &self,
        namespaces: Vec<String>,
    ) -> Result<HashMap<String, HashMap<String, OwnedVariant>>, PortalError> {
        let mut all: HashMap<String, HashMap<String, OwnedVariant>> = HashMap::new();
        for (namespace, key, setting) in self.current() {
            if namespace_matches(namespace, &namespaces) {
                let value = setting.variant().try_into_owned()?;
                all.entry(namespace.to_string())
                    .or_default()
                    .insert(key.to_string(), value);
            }
        }
        Ok(all)
    }

    /// The value of one setting.
    fn read_one(&self, namespace: &str, key: &str) -> Result<OwnedVariant, PortalError> {
        Ok(self.lookup(namespace, key)?.try_into_owned()?)
    }

    /// The value of one setting wrapped in a second variant, as the
    /// deprecated version 1 method returns it.
    fn read(&self, namespace: &str, key: &str) -> Result<OwnedVariant, PortalError> {
        let value = self.lookup(namespace, key)?;
        Ok(Variant::Variant(Box::new(value)).try_into_owned()?)
    }

    /// A setting took a new value.
    #[zbus(signal)]
    async fn setting_changed(
        emitter: &SignalEmitter<'_>,
        namespace: &str,
        key: &str,
        value: Variant<'_>,
    ) -> zbus::Result<()>;
}

/// The Settings portal on the session bus, served while this value lives.
pub struct Portal {
    /// The served interface, through which its signals are emitted; it
    /// holds the connection, and with it the name.
    interface: InterfaceRef<Settings>,
    /// The decision the interface reads.
    decision: Arc<Mutex<Decision>>,
}

impl Portal {
    /// Serves the portal on the session bus, or returns `None` when there is
    /// no bus or another process holds the portal's name.
    ///
    /// # Arguments
    /// * `decision` - The decision to serve first.
    pub fn start(decision: &Decision) -> Option<Portal> {
        match Portal::serve(Builder::session(), decision) {
            Ok(portal) => Some(portal),
            Err(error) => {
                if std::env::var_os("SELKIES_DESKTOP_DEBUG").is_some() {
                    eprintln!("selkies-desktop: not serving the settings portal: {error}");
                }
                None
            }
        }
    }

    /// Serves the portal on the bus a builder connects to.
    ///
    /// # Errors
    /// The connection's failure, or [`zbus::Error::NameTaken`] when the
    /// portal's name has an owner.
    fn serve(builder: Builder<'_>, decision: &Decision) -> zbus::Result<Portal> {
        let decision = Arc::new(Mutex::new(decision.clone()));
        let settings = Settings {
            decision: decision.clone(),
        };
        zbus::block_on(async {
            let connection = builder.serve_at(PATH, settings).build().await?;
            connection
                .request_name_with_flags(NAME, RequestNameFlags::DO_NOT_QUEUE)
                .await?;
            let interface = connection.object_server().interface::<_, Settings>(PATH).await?;
            Ok(Portal { interface, decision })
        })
    }

    /// Serves a new decision and signals every setting whose value changed.
    ///
    /// # Arguments
    /// * `decision` - The decision the shell's settings now imply.
    pub fn update(&mut self, decision: &Decision) {
        let before = {
            let mut shared = self.decision.lock().unwrap_or_else(PoisonError::into_inner);
            let before = settings(&shared);
            *shared = decision.clone();
            before
        };
        let changed: Vec<Entry> = settings(decision)
            .into_iter()
            .filter(|entry| !before.contains(entry))
            .collect();
        if changed.is_empty() {
            return;
        }
        let result = zbus::block_on(async {
            for (namespace, key, setting) in &changed {
                self.interface
                    .setting_changed(namespace, key, setting.variant())
                    .await?;
            }
            Ok::<(), zbus::Error>(())
        });
        if let Err(error) = result
            && std::env::var_os("SELKIES_DESKTOP_DEBUG").is_some()
        {
            eprintln!("selkies-desktop: cannot signal the settings portal: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::BufRead;
    use std::process::{Child, Command, Stdio};

    use zbus::export::futures_core::Stream;
    use zbus::{Connection, MatchRule, Message, MessageStream};

    fn decision(dark: bool) -> Decision {
        Decision {
            dark,
            gtk_theme: if dark { "Adwaita-dark" } else { "Adwaita" }.to_string(),
            icon_theme: Some("hicolor".to_string()),
            cursor_theme: None,
            accent: crate::render::rgb(0xff, 0x00, 0x80),
        }
    }

    fn patterns(list: &[&str]) -> Vec<String> {
        list.iter().map(|p| p.to_string()).collect()
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

    fn call<B>(client: &Connection, interface: &str, method: &str, body: &B) -> zbus::Result<Message>
    where
        B: zbus::export::serde::Serialize + zbus::DynamicType,
    {
        zbus::block_on(client.call_method(Some(NAME), PATH, Some(interface), method, body))
    }

    const SETTINGS: &str = "org.freedesktop.portal.Settings";

    #[test]
    fn a_dark_decision_serves_the_dark_scheme_in_both_namespaces() {
        let entries = settings(&decision(true));
        assert!(entries.contains(&(APPEARANCE, "color-scheme", Setting::U32(1))));
        assert!(entries.contains(&(INTERFACE, "color-scheme", Setting::Str("prefer-dark".into()))));
        assert!(entries.contains(&(INTERFACE, "gtk-theme", Setting::Str("Adwaita-dark".into()))));
        let accent = entries.iter().find(|(_, key, _)| *key == "accent-color");
        let Some((APPEARANCE, _, Setting::Rgb(r, g, b))) = accent else {
            panic!("no accent color: {accent:?}");
        };
        assert_eq!((*r, *g), (1.0, 0.0));
        assert!((b - 128.0 / 255.0).abs() < 1e-6, "{b}");
        assert!(entries.contains(&(INTERFACE, "icon-theme", Setting::Str("hicolor".into()))));
        assert!(!entries.iter().any(|(_, key, _)| *key == "cursor-theme"));
        let light = settings(&decision(false));
        assert!(light.contains(&(APPEARANCE, "color-scheme", Setting::U32(2))));
        assert!(light.contains(&(INTERFACE, "color-scheme", Setting::Str("prefer-light".into()))));
    }

    #[test]
    fn namespaces_match_as_the_portal_globs_them() {
        assert!(namespace_matches(APPEARANCE, &[]));
        assert!(namespace_matches(APPEARANCE, &patterns(&[""])));
        assert!(namespace_matches(APPEARANCE, &patterns(&["org.freedesktop.*"])));
        assert!(namespace_matches(
            APPEARANCE,
            &patterns(&["org.gnome.*", APPEARANCE])
        ));
        assert!(!namespace_matches(APPEARANCE, &patterns(&["org.gnome.*"])));
        assert!(!namespace_matches(APPEARANCE, &patterns(&["org.freedesktop"])));
    }

    #[test]
    fn the_portal_answers_reads_and_signals_a_changed_scheme_on_a_session_bus() {
        let Some(bus) = Bus::start() else {
            eprintln!("skipped: no dbus-daemon");
            return;
        };
        let mut portal = Portal::serve(bus.builder(), &decision(true)).unwrap();
        let client = zbus::block_on(bus.builder().build()).unwrap();

        let reply = call(&client, SETTINGS, "ReadOne", &(APPEARANCE, "color-scheme")).unwrap();
        let value: OwnedVariant = reply.body().deserialize().unwrap();
        assert_eq!(*value, Variant::U32(1));

        let reply = call(&client, SETTINGS, "Read", &(INTERFACE, "gtk-theme")).unwrap();
        let value: OwnedVariant = reply.body().deserialize().unwrap();
        assert_eq!(*value, Variant::Variant(Box::new(Variant::from("Adwaita-dark"))));

        let reply = call(&client, SETTINGS, "ReadAll", &(vec!["org.freedesktop.*"],)).unwrap();
        let all: HashMap<String, HashMap<String, OwnedVariant>> = reply.body().deserialize().unwrap();
        assert_eq!(all.keys().collect::<Vec<_>>(), [APPEARANCE]);
        assert_eq!(*all[APPEARANCE]["contrast"], Variant::U32(0));

        let error = call(&client, SETTINGS, "ReadOne", &(APPEARANCE, "missing")).unwrap_err();
        assert!(
            matches!(&error, zbus::Error::MethodError(name, _, _)
                if name.as_str() == "org.freedesktop.portal.Error.NotFound"),
            "{error}"
        );

        let reply = call(
            &client,
            "org.freedesktop.DBus.Properties",
            "Get",
            &(SETTINGS, "version"),
        )
        .unwrap();
        let value: OwnedVariant = reply.body().deserialize().unwrap();
        assert_eq!(*value, Variant::U32(VERSION));

        assert!(matches!(
            Portal::serve(bus.builder(), &decision(true)),
            Err(zbus::Error::NameTaken)
        ));

        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(SETTINGS)
            .member("SettingChanged")
            .build()
            .unwrap();
        let mut stream = zbus::block_on(MessageStream::for_match_rule(rule, &client, None)).unwrap();
        portal.update(&decision(false));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            zbus::block_on(async {
                while let Some(Ok(message)) =
                    std::future::poll_fn(|cx| std::pin::Pin::new(&mut stream).poll_next(cx)).await
                {
                    let (namespace, key, value): (String, String, OwnedVariant) =
                        message.body().deserialize().unwrap();
                    if tx.send((namespace, key, value)).is_err() {
                        break;
                    }
                }
            })
        });
        let mut changed = HashMap::new();
        while changed.len() < 3
            && let Ok((namespace, key, value)) = rx.recv_timeout(std::time::Duration::from_secs(5))
        {
            changed.insert(format!("{namespace} {key}"), value);
        }
        if let Ok((namespace, key, _)) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
            panic!("an unchanged key was signaled: {namespace} {key}");
        }
        assert_eq!(*changed[&format!("{APPEARANCE} color-scheme")], Variant::U32(2));
        assert_eq!(
            *changed[&format!("{INTERFACE} color-scheme")],
            Variant::from("prefer-light")
        );
        assert_eq!(
            *changed[&format!("{INTERFACE} gtk-theme")],
            Variant::from("Adwaita")
        );
    }
}
