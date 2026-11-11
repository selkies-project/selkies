/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The light or dark color scheme of the session's desktop, as the primary display's page asks
//! for it (the `color_scheme` setting).
//!
//! No one key reaches every toolkit, so the scheme goes to each settings store a session's
//! applications read, and every store is detected rather than assumed:
//!
//! - Every session bus of the session's applications (the bus each of this user's processes on
//!   the session's displays carries, read from `/proc`, plus `$XDG_RUNTIME_DIR/bus`, the bus GLib
//!   and libdbus connect to where a process names none) has GSettings
//!   `org.gnome.desktop.interface color-scheme` set through it. That is the key the settings
//!   portal serves as `org.freedesktop.appearance color-scheme` (xdg-desktop-portal-gtk and
//!   -gnome read it), which GTK 4 on Wayland, libadwaita, Qt 6, and the browsers follow, and
//!   libadwaita reads it directly where no portal answers. Its `gtk-theme`, which GTK 3 on
//!   Wayland and GNOME's XSETTINGS daemon serve, moves to the current theme's dark or light
//!   counterpart. dconf keeps one database per home but notifies only the bus a change came
//!   through, so each bus is written, not one. A bus whose GSettings falls back to the memory
//!   backend stores nothing another process sees, and counts for nothing.
//! - The desktop a bus belongs to (its `XDG_CURRENT_DESKTOP`) takes the scheme its own way
//!   too: XFCE as its xfconf `/Net/ThemeName`, which xfsettingsd serves over XSETTINGS; MATE as
//!   `org.mate.interface gtk-theme`, which mate-settings-daemon serves; KDE Plasma as its color
//!   scheme (`plasma-apply-colorscheme`), which its own portal and GTK integration follow.
//! - The xsettingsd serving this display (the base image's XSETTINGS manager) serves the theme's
//!   counterpart as `Net/ThemeName` ([`super::xsettings`]). GTK 3 on X11 has no XSETTING for a
//!   dark preference, only the theme's name, and it restyles running windows when the name
//!   changes.
//!
//! A theme's counterpart is `<name>-dark` (or `-Dark`) and back, and only one GTK 3 can load:
//! built in, or installed with a `gtk-3.0` directory under a themes directory. A theme without
//! one is left as it is, so a scheme never swaps a user's theme for an unrelated one, and a
//! light page asking a light theme for light writes nothing. A theme that is named but not
//! installed counts as GTK's default, Adwaita, which GTK 3 shows in its place: a distribution's
//! GTK settings name its own theme (Ubuntu's name Yaru) whether or not the image carries it.
//!
//! Applying is idempotent and latest-wins: pages assert the scheme again with every full
//! `SETTINGS`, a run repeats only when the scheme or the set of stores changed, and of
//! overlapping requests only the newest runs. Everything here is blocking or spawns tools, so it
//! runs on its own task, never under a lock a stream waits on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use log::{debug, info};
use parking_lot::Mutex;

use super::dpi::{home, pids_on_display};
use super::xrandr::{run_tool, which};
use super::xsettings;
use crate::input::command::{AppSession, dbus_address_live, session_processes};

/// The schema both GNOME keys live in.
const GNOME_INTERFACE: &str = "org.gnome.desktop.interface";
/// GTK 3's built-in themes, which no themes directory carries.
const BUILTIN_GTK3_THEMES: &[&str] = &["Adwaita", "HighContrast", "HighContrastInverse"];
/// What GTK 3 loads where nothing names a theme.
const DEFAULT_GTK3_THEME: &str = "Adwaita";
/// Plasma's light and dark Breeze schemes; `Breeze` is the older light one.
const PLASMA_LIGHT: &str = "BreezeLight";
const PLASMA_DARK: &str = "BreezeDark";

/// A color scheme the desktop is asked to follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorScheme {
    Light,
    Dark,
}

impl ColorScheme {
    /// The scheme a sanitized `color_scheme` value names; `None` for `off`, which leaves the
    /// desktop alone, and for anything else.
    pub fn from_setting(value: &str) -> Option<ColorScheme> {
        match value {
            "light" => Some(ColorScheme::Light),
            "dark" => Some(ColorScheme::Dark),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ColorScheme::Light => "light",
            ColorScheme::Dark => "dark",
        }
    }

    /// The `org.gnome.desktop.interface color-scheme` value.
    fn gnome_value(self) -> &'static str {
        match self {
            ColorScheme::Light => "prefer-light",
            ColorScheme::Dark => "prefer-dark",
        }
    }
}

/// The GTK 3 theme `scheme` asks for in place of `current`, or `None` where the theme GTK shows
/// already is one or has no counterpart `installed` names. A `current` that is not installed
/// is shown as GTK's default, the one GTK 3 falls back to (distributions name themes in their
/// GTK settings that their minimal installs leave out).
pub fn theme_for(
    scheme: ColorScheme,
    current: &str,
    installed: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let current = if installed(current) {
        current
    } else {
        DEFAULT_GTK3_THEME
    };
    let base = ["-dark", "-Dark"]
        .iter()
        .find_map(|suffix| current.strip_suffix(suffix));
    match (scheme, base) {
        (ColorScheme::Dark, None) => [format!("{current}-dark"), format!("{current}-Dark")]
            .into_iter()
            .find(|name| installed(name)),
        (ColorScheme::Light, Some(base)) => Some(base.to_string()).filter(|name| installed(name)),
        _ => None,
    }
}

/// The directories GTK 3 looks for themes in, as this process's environment names them.
fn theme_dirs() -> Vec<PathBuf> {
    let home = home();
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let mut dirs = vec![data_home.join("themes"), home.join(".themes")];
    dirs.extend(
        data_dirs
            .split(':')
            .filter(|d| !d.is_empty())
            .map(|d| Path::new(d).join("themes")),
    );
    dirs
}

/// Whether GTK 3 can load the theme `name`: built in, or a `gtk-3.0` directory in `dirs`.
pub fn gtk3_theme_installed(name: &str, dirs: &[PathBuf]) -> bool {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return false;
    }
    BUILTIN_GTK3_THEMES.contains(&name)
        || dirs
            .iter()
            .any(|dir| dir.join(name).join("gtk-3.0").is_dir())
}

/// A GSettings string as `gsettings get` prints it (`'Adwaita'`), unquoted.
fn gsettings_string(printed: &str) -> String {
    let text = printed.trim();
    text.strip_prefix('\'')
        .and_then(|t| t.strip_suffix('\''))
        .unwrap_or(text)
        .to_string()
}

/// Whether a GSettings tool's stderr says it fell back to the memory backend: a write there
/// reaches no other process.
fn memory_backend(stderr: &str) -> bool {
    stderr.contains("'memory' GSettings backend")
}

/// One session bus and the environment of a process on it, which the tools writing through it
/// run with.
#[derive(Clone, Debug)]
pub struct SessionBus {
    pub address: String,
    pub env: BTreeMap<String, String>,
}

impl SessionBus {
    /// Whether the desktop this bus belongs to names itself `desktop` (case-insensitive).
    fn desktop_is(&self, desktop: &str) -> bool {
        self.env
            .get("XDG_CURRENT_DESKTOP")
            .is_some_and(|d| d.split(':').any(|part| part.eq_ignore_ascii_case(desktop)))
    }
}

/// Whether two D-Bus addresses name one bus: the same transports once the `guid` a daemon
/// appends to the address it hands its clients is set aside.
fn same_bus(a: &str, b: &str) -> bool {
    let transports = |address: &str| -> Vec<String> {
        address
            .split(';')
            .map(|transport| {
                transport
                    .split(',')
                    .filter(|kv| !kv.starts_with("guid="))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect()
    };
    transports(a) == transports(b)
}

/// Every live session bus the session's applications reach, each once: the buses its processes
/// carry, then `$XDG_RUNTIME_DIR/bus` where it is a socket of its own. Blocking: it walks `/proc`.
pub fn session_buses(session: &AppSession) -> Vec<SessionBus> {
    let mut buses: Vec<SessionBus> = Vec::new();
    let known =
        |buses: &[SessionBus], address: &str| buses.iter().any(|b| same_bus(&b.address, address));
    for env in session_processes(
        session.x11_display.as_deref(),
        session.wayland_display.as_deref(),
    ) {
        let address = env["DBUS_SESSION_BUS_ADDRESS"].clone();
        if known(&buses, &address) || !dbus_address_live(&address) {
            continue;
        }
        buses.push(SessionBus {
            address,
            env: env.into_iter().collect(),
        });
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|r| !r.is_empty()) {
        let socket = Path::new(&runtime).join("bus");
        let address = format!("unix:path={}", socket.display());
        let ours = std::fs::metadata(&socket).is_ok_and(|m| {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            m.file_type().is_socket() && m.uid() == nix::unistd::getuid().as_raw()
        });
        if ours && !known(&buses, &address) && dbus_address_live(&address) {
            let mut env: BTreeMap<String, String> = std::env::vars().collect();
            env.insert("DBUS_SESSION_BUS_ADDRESS".into(), address.clone());
            buses.push(SessionBus { address, env });
        }
    }
    buses
}

/// Set GSettings `schema key` to `value` (a string) through `bus`; whether it was stored where
/// other processes read it.
async fn gsettings_set(bus: &SessionBus, schema: &str, key: &str, value: &str) -> bool {
    let out = run_tool(
        "gsettings",
        &["set", schema, key, value],
        Some(&bus.env),
        None,
    )
    .await;
    if memory_backend(&out.stderr) {
        debug!(target: "display", "GSettings on {} has no persistent backend; {schema} {key} is not stored.", bus.address);
        return false;
    }
    if !out.success {
        debug!(target: "display", "gsettings set {schema} {key} on {} failed: {}", bus.address, out.stderr.trim());
    }
    out.success
}

/// GSettings `schema key` as `bus` reads it, `None` where the schema or key is missing.
async fn gsettings_get(bus: &SessionBus, schema: &str, key: &str) -> Option<String> {
    let out = run_tool("gsettings", &["get", schema, key], Some(&bus.env), None).await;
    out.success.then(|| gsettings_string(&out.stdout))
}

/// Move a GSettings theme key to the counterpart `scheme` asks for; the theme set, if any.
async fn gsettings_theme(
    bus: &SessionBus,
    schema: &str,
    scheme: ColorScheme,
    dirs: &[PathBuf],
) -> Option<String> {
    let current = gsettings_get(bus, schema, "gtk-theme").await?;
    let wanted = theme_for(scheme, &current, &|n| gtk3_theme_installed(n, dirs))?;
    gsettings_set(bus, schema, "gtk-theme", &wanted)
        .await
        .then_some(wanted)
}

/// The scheme through one bus: the GNOME keys, and the stores of the desktop the bus belongs
/// to. What took it, for the log line.
async fn apply_on_bus(bus: &SessionBus, scheme: ColorScheme, dirs: &[PathBuf]) -> Vec<String> {
    let mut took = Vec::new();
    if which("gsettings")
        && gsettings_get(bus, GNOME_INTERFACE, "color-scheme")
            .await
            .is_some()
    {
        if gsettings_set(bus, GNOME_INTERFACE, "color-scheme", scheme.gnome_value()).await {
            took.push(format!("color-scheme {}", scheme.gnome_value()));
        }
        if let Some(theme) = gsettings_theme(bus, GNOME_INTERFACE, scheme, dirs).await {
            took.push(format!("GTK theme {theme}"));
        }
    }
    if bus.desktop_is("MATE")
        && which("gsettings")
        && let Some(theme) = gsettings_theme(bus, "org.mate.interface", scheme, dirs).await
    {
        took.push(format!("MATE theme {theme}"));
    }
    if bus.desktop_is("XFCE")
        && which("xfconf-query")
        && let Some(theme) = xfconf_theme(bus, scheme, dirs).await
    {
        took.push(format!("XFCE theme {theme}"));
    }
    if bus.desktop_is("KDE")
        && let Some(colors) = plasma_scheme(bus, scheme).await
    {
        took.push(format!("Plasma colors {colors}"));
    }
    took
}

/// XFCE's `/Net/ThemeName` moved to the counterpart `scheme` asks for.
async fn xfconf_theme(bus: &SessionBus, scheme: ColorScheme, dirs: &[PathBuf]) -> Option<String> {
    let path = ["-c", "xsettings", "-p", "/Net/ThemeName"];
    let out = run_tool("xfconf-query", &path, Some(&bus.env), None).await;
    let current = if out.success {
        out.stdout.trim().to_string()
    } else {
        DEFAULT_GTK3_THEME.to_string()
    };
    let wanted = theme_for(scheme, &current, &|n| gtk3_theme_installed(n, dirs))?;
    let mut args = path.to_vec();
    args.extend(["-s", &wanted, "--create", "-t", "string"]);
    let out = run_tool("xfconf-query", &args, Some(&bus.env), None).await;
    if !out.success {
        debug!(target: "display", "xfconf-query /Net/ThemeName on {} failed: {}", bus.address, out.stderr.trim());
    }
    out.success.then_some(wanted)
}

/// Plasma's color scheme moved between Breeze's light and dark ones; a scheme of another family
/// is the user's and left alone.
async fn plasma_scheme(bus: &SessionBus, scheme: ColorScheme) -> Option<String> {
    if !which("plasma-apply-colorscheme") {
        return None;
    }
    let reader = ["kreadconfig6", "kreadconfig5"]
        .into_iter()
        .find(|tool| which(tool))?;
    let out = run_tool(
        reader,
        &[
            "--file",
            "kdeglobals",
            "--group",
            "General",
            "--key",
            "ColorScheme",
        ],
        Some(&bus.env),
        None,
    )
    .await;
    let current = out.stdout.trim();
    let current = if current.is_empty() {
        PLASMA_LIGHT
    } else {
        current
    };
    let wanted = match (scheme, current) {
        (ColorScheme::Dark, PLASMA_LIGHT | "Breeze") => PLASMA_DARK,
        (ColorScheme::Light, PLASMA_DARK) => PLASMA_LIGHT,
        _ => return None,
    };
    let out = run_tool("plasma-apply-colorscheme", &[wanted], Some(&bus.env), None).await;
    if !out.success {
        debug!(target: "display", "plasma-apply-colorscheme {wanted} on {} failed: {}", bus.address, out.stderr.trim());
    }
    out.success.then(|| wanted.to_string())
}

/// The GTK 3 theme the X11 session is served: the xsettingsd file's, else the one the first of
/// GTK's own `settings_files` names, else GTK's default.
fn served_gtk3_theme(xsettingsd: &str, settings_files: &[String]) -> String {
    if let Some(theme) = xsettings::entry(xsettingsd, "Net/ThemeName").filter(|t| !t.is_empty()) {
        return theme;
    }
    settings_files
        .iter()
        .find_map(|text| {
            text.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "gtk-theme-name")
                    .then(|| value.trim().trim_matches('"').to_string())
                    .filter(|v| !v.is_empty())
            })
        })
        .unwrap_or_else(|| DEFAULT_GTK3_THEME.to_string())
}

/// GTK 3's settings files, the user's first, as far as they can be read. Blocking.
fn gtk3_settings_files() -> Vec<String> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    [
        config_home.join("gtk-3.0/settings.ini"),
        PathBuf::from("/etc/gtk-3.0/settings.ini"),
    ]
    .iter()
    .filter_map(|path| std::fs::read_to_string(path).ok())
    .collect()
}

/// The theme's counterpart as the xsettingsd serving this display's `Net/ThemeName`; the theme
/// served, if it moved.
async fn xsettingsd_theme(scheme: ColorScheme, dirs: &[PathBuf]) -> Option<String> {
    let path = xsettings::xsettingsd_path();
    let read = path.clone();
    let current = tokio::task::spawn_blocking(move || {
        served_gtk3_theme(
            &std::fs::read_to_string(read).unwrap_or_default(),
            &gtk3_settings_files(),
        )
    })
    .await
    .unwrap_or_else(|_| DEFAULT_GTK3_THEME.to_string());
    let wanted = theme_for(scheme, &current, &|n| gtk3_theme_installed(n, dirs))?;
    let entries = [("Net/ThemeName", format!("\"{wanted}\""))];
    let written = tokio::task::spawn_blocking(move || xsettings::write_entries(&path, &entries))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| e.to_string()));
    if let Err(e) = written {
        debug!(target: "display", "The xsettingsd theme was not written: {e}");
        return None;
    }
    (xsettings::reload().await > 0).then_some(wanted)
}

/// What a run reached: the scheme, and which buses and XSETTINGS manager it found.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Reach {
    scheme: ColorScheme,
    buses: Vec<String>,
    xsettingsd: Vec<i32>,
}

/// The newest request no run has taken yet.
static WANTED: Mutex<Option<(ColorScheme, AppSession)>> = Mutex::new(None);
/// Held by the one run in flight, with what the last run reached.
static APPLYING: tokio::sync::Mutex<Option<Reach>> = tokio::sync::Mutex::const_new(None);

/// Hand `scheme` to the desktop the session's applications run on (module docblock). A request
/// overtaken by a newer one while it waits does nothing.
pub async fn apply(scheme: ColorScheme, session: AppSession) {
    *WANTED.lock() = Some((scheme, session));
    let mut last = APPLYING.lock().await;
    let Some((scheme, session)) = WANTED.lock().take() else {
        return;
    };
    let (buses, xsettingsd) = {
        let session = session.clone();
        tokio::task::spawn_blocking(move || {
            let xsettingsd = if session.x11_display.is_some() {
                pids_on_display("xsettingsd")
            } else {
                Vec::new()
            };
            (session_buses(&session), xsettingsd)
        })
        .await
        .unwrap_or_default()
    };
    let reach = Reach {
        scheme,
        buses: buses.iter().map(|b| b.address.clone()).collect(),
        xsettingsd: xsettingsd.clone(),
    };
    if last.as_ref() == Some(&reach) {
        debug!(target: "display", "Desktop color scheme {} already applied to the same stores.", scheme.name());
        return;
    }
    let changed = last.as_ref().is_none_or(|l| l.scheme != scheme);
    let dirs = tokio::task::spawn_blocking(theme_dirs)
        .await
        .unwrap_or_default();
    let mut took: Vec<String> = Vec::new();
    for bus in &buses {
        let on_bus = apply_on_bus(bus, scheme, &dirs).await;
        debug!(target: "display", "Color scheme {} through {}: {}.", scheme.name(), bus.address,
            if on_bus.is_empty() { "nothing took it".to_string() } else { on_bus.join(", ") });
        for item in on_bus {
            if !took.contains(&item) {
                took.push(item);
            }
        }
    }
    if !xsettingsd.is_empty()
        && let Some(theme) = xsettingsd_theme(scheme, &dirs).await
    {
        took.push(format!("XSETTINGS theme {theme}"));
    }
    let summary = if took.is_empty() {
        "no settings store took it (no session bus with persistent settings, no XSETTINGS manager on this display, or no theme counterpart installed)".to_string()
    } else {
        took.join(", ")
    };
    if changed {
        info!(target: "display", "Desktop color scheme set to {}: {summary}.", scheme.name());
    } else {
        debug!(target: "display", "Desktop color scheme {} reapplied to the session's current stores: {summary}.", scheme.name());
    }
    *last = Some(reach);
}

#[cfg(test)]
mod tests;
