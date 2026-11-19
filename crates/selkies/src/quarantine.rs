/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A quarantine for what the session writes (`--quarantine-xdg`), for a session on a machine
//! whose user already has a desktop: the configuration that desktop reads stays as the user
//! left it.
//!
//! A session keeps its desktop in the user's configuration: the window manager and the shell
//! write the GTK theme and the dark preference into `settings.ini`, an import of the desktop's
//! accent into `gtk.css`, the GSettings interface keys through dconf, the file manager's desktop
//! entries, the default folder handler into `mimeapps.list`, the window theme and `rc.xml` of an
//! openbox or labwc, and a density into `~/.Xresources`, `~/.xsettingsd`, or the desktop's own
//! settings. In a container the home is the session's own and that is what it is for. Run on a
//! machine whose user logs into a desktop of their own, the same writes land in that desktop's
//! configuration: its theme turns light or dark with the browser, and folders open in a file
//! manager that desktop does not have.
//!
//! The quarantine is a directory of Selkies' own, `selkies/quarantine` in the user's state home
//! ([`root`]), kept for the next session, in one of two forms ([`Mode`]):
//!
//! * **Targeted**: the applications keep the user's own XDG homes, and with them their profiles
//!   and settings; only what Selkies writes for the desktop moves. The quarantine's `config` and
//!   `data` lead `XDG_CONFIG_DIRS` and `XDG_DATA_DIRS`, so the session reads them beneath the
//!   user's own homes and above the system's: `settings.ini`, `mimeapps.list`, the window
//!   managers' `rc.xml`, the desktop entries, and the window theme are written there
//!   (`selkies_draw::dirs::session_config_home`), and what the user configured still wins.
//!   On a session bus of the session's own (`selkies session` without `--host`), the settings
//!   daemons it activates, dconf and xfconfd, are given a configuration home in the quarantine
//!   ([`SETTINGS_DAEMONS`], [`write_daemon_services`], [`settings_env`]): they alone write the
//!   stores the desktop's theme and density live in, so GSettings and XFCE's settings become
//!   the session's, starting from the user's (dconf reads the user's database beneath its own,
//!   xfconfd's channels are copied in the first time), and the theme and the density keep
//!   going there. What would still land in the user's own configuration is not written: the
//!   `gtk.css` import, which GTK reads from the configuration home alone, `lxqt.conf`, a file
//!   every LXQt application reads there, and GSettings and xfconf where the daemons are the
//!   user's, on the host's bus under `--host` or a server run alone
//!   (`selkies_draw::dirs::settings_stores_writable`). The scheme and the accent still reach
//!   libadwaita, Qt 6, Chromium, and Electron through the Settings portal, and a density the
//!   other desktops through the X resources.
//! * **Full**: the four homes, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, and
//!   `XDG_CACHE_HOME`, move into the quarantine, so everything the session writes through them,
//!   Selkies or not, lands there, the applications' profiles and GSettings included. The user's
//!   own configuration and data homes lead the system directories instead, so the session still
//!   reads what they hold beneath its own, and the XDG user directories, which are looked up in
//!   the configuration home alone, are carried in from the user's at every start ([`prepare`]).
//!
//! `HOME` stays the user's in both: their files, their shell, and everything an application keeps
//! in the home itself are the session's, which is what makes it the user's desktop rather than a
//! stranger's. The few files Selkies writes in the home itself are written in the quarantine's
//! `home` instead (`selkies_draw::dirs::dotfile_home`).
//!
//! The variables are applied once, by whichever process comes first: `selkies session` puts them
//! into the session's environment before it starts the buses, the sound server, the desktop, and
//! the server, so every one of them and everything they start shares the quarantine
//! ([`env`]); the server run alone puts them into its own environment before any thread starts
//! ([`apply_to_process`]), which every program its window manager starts inherits. Either
//! marks the environment with [`QUARANTINE_VAR`], so a process started inside a quarantined
//! session leaves it as it is.
//!
//! What another process writes on the session's behalf is outside it: under a full quarantine
//! with `selkies session --host`, or a server run alone on a session bus it inherited, the
//! GSettings writes go through that bus's dconf service, which writes the host's own database.

use std::path::{Component, Path, PathBuf};

pub use selkies_draw::dirs::QUARANTINE_VAR;

/// What the quarantine takes in (see the module docblock).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// No quarantine: the session writes into the user's XDG homes.
    Off,
    /// What Selkies writes for the desktop, the applications keeping the user's XDG homes.
    Targeted,
    /// The session's XDG homes entire.
    Full,
}

impl Mode {
    /// The mode a `--quarantine-xdg` value names, [`Mode::Off`] for any other.
    pub fn parse(value: &str) -> Mode {
        match value {
            "targeted" => Mode::Targeted,
            "full" => Mode::Full,
            _ => Mode::Off,
        }
    }

    /// The mode's name, as `--quarantine-xdg` takes it.
    pub fn name(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Targeted => "targeted",
            Mode::Full => "full",
        }
    }
}

/// The XDG homes the full quarantine moves, each with its default under the home and the directory
/// it becomes under the quarantine's root.
const HOMES: &[(&str, &str, &str)] = &[
    ("XDG_CONFIG_HOME", ".config", "config"),
    ("XDG_DATA_HOME", ".local/share", "data"),
    ("XDG_STATE_HOME", ".local/state", "state"),
    ("XDG_CACHE_HOME", ".cache", "cache"),
];

/// `dir` where it is absolute and names no parent, as the XDG Base Directory specification
/// takes a path in its variables.
fn usable(dir: PathBuf) -> Option<PathBuf> {
    (dir.is_absolute() && !dir.components().any(|c| c == Component::ParentDir)).then_some(dir)
}

/// The XDG home `var` names in the environment `get` reads, else its default under the home;
/// `None` without a usable home.
fn user_home_dir(
    get: &impl Fn(&str) -> Option<String>,
    var: &str,
    default: &str,
) -> Option<PathBuf> {
    get(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .and_then(usable)
        .or_else(|| {
            get("HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .and_then(usable)
                .filter(|home| home != Path::new("/"))
                .map(|home| home.join(default))
        })
}

/// The quarantine's root for the environment `get` reads: `selkies/quarantine` in the user's
/// own state home. `None` without a usable home, where there is nothing to keep apart from.
pub fn root(get: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    user_home_dir(&get, "XDG_STATE_HOME", ".local/state")
        .map(|state| state.join("selkies/quarantine"))
}

/// `first` ahead of the directories a search-path variable lists, else of its default, without
/// a second copy of it.
fn ahead_of(first: &Path, listed: Option<String>, default: &str) -> String {
    let listed = listed
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string());
    let first = first.to_string_lossy();
    std::iter::once(first.as_ref())
        .chain(
            listed
                .split(':')
                .filter(|dir| !dir.is_empty() && *dir != first),
        )
        .collect::<Vec<_>>()
        .join(":")
}

/// The variables that quarantine the environment `get` reads in `mode` (see the module
/// docblock), with [`QUARANTINE_VAR`] naming the root: under [`Mode::Targeted`] the
/// quarantine's `config` and `data` first among the system directories, under [`Mode::Full`]
/// the four XDG homes under [`root`] and the user's own configuration and data homes first among
/// the system directories. Empty for [`Mode::Off`], where the environment is quarantined
/// already, or where it has no usable home.
pub fn env(mode: Mode, get: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    if mode == Mode::Off || get(QUARANTINE_VAR).is_some_and(|v| !v.is_empty()) {
        return Vec::new();
    }
    let Some(root) = root(&get) else {
        return Vec::new();
    };
    let text = |path: &Path| path.to_string_lossy().into_owned();
    let (mut vars, config, data): (Vec<(String, String)>, _, _) = match mode {
        Mode::Targeted => (
            Vec::new(),
            Some(root.join("config")),
            Some(root.join("data")),
        ),
        _ => (
            HOMES
                .iter()
                .map(|(var, _, dir)| (var.to_string(), text(&root.join(dir))))
                .collect(),
            user_home_dir(&get, "XDG_CONFIG_HOME", ".config"),
            user_home_dir(&get, "XDG_DATA_HOME", ".local/share"),
        ),
    };
    if let Some(config) = config {
        vars.push((
            "XDG_CONFIG_DIRS".to_string(),
            ahead_of(&config, get("XDG_CONFIG_DIRS"), "/etc/xdg"),
        ));
    }
    if let Some(data) = data {
        vars.push((
            "XDG_DATA_DIRS".to_string(),
            ahead_of(&data, get("XDG_DATA_DIRS"), "/usr/local/share:/usr/share"),
        ));
    }
    vars.push((QUARANTINE_VAR.to_string(), text(&root)));
    vars
}

/// The files of the user's configuration home the session reads only from its own, copied into
/// the quarantine at every start: the XDG user directories (Desktop, Documents, Downloads, and
/// their localized names), which `xdg-user-dir`, GLib, and the shell look up in the
/// configuration home alone, never along `XDG_CONFIG_DIRS`.
const CARRIED: &[&str] = &["user-dirs.dirs", "user-dirs.locale"];

/// Make the quarantine's directories, its owner's alone as the XDG Base Directory specification
/// asks of them, from the variables [`env`] gave for the environment `get` reads, and under a
/// full quarantine carry the [`CARRIED`] files of the user's own configuration home into it, so
/// the session's Desktop and Downloads are the user's.
///
/// # Errors
/// A directory could not be made or its mode set, or a file could not be carried.
pub fn prepare(
    vars: &[(String, String)],
    get: impl Fn(&str) -> Option<String>,
) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let wanted = |name: &str| {
        vars.iter()
            .find(|(var, _)| var == name)
            .map(|(_, v)| PathBuf::from(v))
    };
    let Some(root) = wanted(QUARANTINE_VAR) else {
        return Ok(());
    };
    let mut dirs = vec![
        root.clone(),
        root.join("home"),
        root.join("config"),
        root.join("data"),
    ];
    dirs.extend(HOMES.iter().filter_map(|(var, _, _)| wanted(var)));
    for dir in dirs {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if let (Some(user), Some(config)) = (
        user_home_dir(&get, "XDG_CONFIG_HOME", ".config"),
        wanted("XDG_CONFIG_HOME"),
    ) {
        for name in CARRIED {
            let source = user.join(name);
            if source.is_file() {
                std::fs::copy(&source, config.join(name))?;
            }
        }
    }
    Ok(())
}

/// The settings daemons a targeted quarantine gives a configuration home of its own on a
/// session bus of the session's own, by the bus name each is activated under: dconf, which keeps
/// GSettings, and xfconfd, which keeps XFCE's settings. Only these processes write their stores,
/// and every other reads them through the bus or, for dconf, through the profile
/// ([`dconf_profile`]), so moving their home moves the stores and nothing an application keeps.
pub const SETTINGS_DAEMONS: &[&str] = &["ca.desrt.dconf", "org.xfce.Xfconf"];

/// A bus name only a service file in the session's runtime directory activates, whose presence
/// among the bus's activatable names shows the bus reads that directory.
pub const PROBE_NAME: &str = "org.selkies.QuarantineProbe";

/// The configuration home the session's settings daemons get: `settings` in the quarantine.
pub fn settings_home(root: &Path) -> PathBuf {
    root.join("settings")
}

/// `text` quoted as a word for the `Exec` line of a D-Bus service file, which the bus splits as
/// a shell does.
fn exec_word(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The service file that activates `name` as the system's `text` does, with the configuration
/// home `home` given to it through `env`; `None` where `text` names no command.
fn wrapped_service(name: &str, text: &str, env: &Path, home: &Path) -> Option<String> {
    let exec = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Exec="))
        .map(str::trim)
        .filter(|exec| !exec.is_empty())?;
    Some(format!(
        "[D-BUS Service]\nName={name}\nExec={} {} {exec}\n",
        exec_word(&env.to_string_lossy()),
        exec_word(&format!("XDG_CONFIG_HOME={}", home.display())),
    ))
}

/// Write the service files that give the [`SETTINGS_DAEMONS`] found along `data_dirs` the
/// quarantine's [`settings_home`] into `dbus-1/services` of the session's runtime directory,
/// which a session bus reads ahead of every other service directory, with the [`PROBE_NAME`]
/// beside them. Returns the names given a service file, the probe's not among them; none
/// without `env`.
///
/// # Errors
/// The directory or a file could not be written.
pub fn write_daemon_services(
    runtime_dir: &Path,
    root: &Path,
    data_dirs: &[PathBuf],
    env: Option<&Path>,
) -> std::io::Result<Vec<String>> {
    let Some(env) = env else {
        return Ok(Vec::new());
    };
    let dir = runtime_dir.join("dbus-1/services");
    std::fs::create_dir_all(&dir)?;
    let home = settings_home(root);
    let mut written = Vec::new();
    for name in SETTINGS_DAEMONS {
        let file = format!("{name}.service");
        let Some(text) = data_dirs
            .iter()
            .filter(|dir| !dir.starts_with(root))
            .find_map(|dir| std::fs::read_to_string(dir.join("dbus-1/services").join(&file)).ok())
        else {
            continue;
        };
        if let Some(service) = wrapped_service(name, &text, env, &home) {
            std::fs::write(dir.join(&file), service)?;
            written.push(name.to_string());
        }
    }
    std::fs::write(
        dir.join(format!("{PROBE_NAME}.service")),
        format!("[D-BUS Service]\nName={PROBE_NAME}\nExec=/bin/false\n"),
    )?;
    Ok(written)
}

/// Copy the user's own XFCE settings (`xfce4/xfconf/xfce-perchannel-xml` in `user_config`) into
/// the quarantine's [`settings_home`] the first time, so the session's XFCE starts from them and
/// then keeps its own. Returns whether they were copied.
///
/// # Errors
/// A file could not be copied.
pub fn seed_xfconf(user_config: &Path, root: &Path) -> std::io::Result<bool> {
    const CHANNELS: &str = "xfce4/xfconf/xfce-perchannel-xml";
    let target = settings_home(root).join(CHANNELS);
    let Ok(entries) = std::fs::read_dir(user_config.join(CHANNELS)) else {
        return Ok(false);
    };
    if target.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(&target)?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "xml") && path.is_file() {
            std::fs::copy(&path, target.join(entry.file_name()))?;
        }
    }
    Ok(true)
}

/// The dconf profile of a session whose dconf keeps GSettings in the quarantine: writes go to a
/// keyfile database the session's dconf service keeps under its configuration home, the
/// quarantine's, and reads fall through to the user's own database, `user_db` where it exists,
/// read-only, and to the system databases the profile that would otherwise apply names
/// (`base`), so the session starts from the user's settings and the user's database is never
/// written.
fn dconf_profile(user_db: Option<&Path>, base: &str) -> String {
    let mut lines = vec!["service-db:keyfile/user".to_string()];
    if let Some(db) = user_db {
        lines.push(format!("file-db:{}", db.display()));
    }
    lines.extend(
        base.lines()
            .map(str::trim)
            .filter(|line| line.starts_with("system-db:") || line.starts_with("file-db:"))
            .map(str::to_string),
    );
    lines.join("\n") + "\n"
}

/// The text of the dconf profile that applies to the environment `get` reads: the file
/// `DCONF_PROFILE` names (a path, or a name under `/etc/dconf/profile`), else
/// `/etc/dconf/profile/user`, else none, dconf's built-in `user-db:user`.
fn base_dconf_profile(get: &impl Fn(&str) -> Option<String>) -> String {
    let named = get("DCONF_PROFILE").filter(|v| !v.is_empty());
    let path = match named {
        Some(name) if name.starts_with('/') => PathBuf::from(name),
        Some(name) => Path::new("/etc/dconf/profile").join(name),
        None => PathBuf::from("/etc/dconf/profile/user"),
    };
    std::fs::read_to_string(path).unwrap_or_default()
}

/// The variables that hand the session to its settings daemons once the bus has shown it reads
/// their service files (`activatable` holds [`PROBE_NAME`]): `DCONF_PROFILE` naming a profile
/// written into the quarantine ([`dconf_profile`]) where dconf was given a service file, and
/// [`selkies_draw::dirs::SETTINGS_VAR`] naming the daemons' configuration home, with the user's
/// XFCE settings seeded into it the first time ([`seed_xfconf`]). Empty where the bus does not
/// read them, so the daemons stay the user's and their stores are left alone.
///
/// # Errors
/// The profile could not be written or the settings seeded.
pub fn settings_env(
    root: &Path,
    daemons: &[String],
    activatable: &[String],
    get: impl Fn(&str) -> Option<String>,
) -> std::io::Result<Vec<(String, String)>> {
    if daemons.is_empty() || !activatable.iter().any(|name| name == PROBE_NAME) {
        return Ok(Vec::new());
    }
    let home = settings_home(root);
    std::fs::DirBuilder::new().recursive(true).create(&home)?;
    let user_config = user_home_dir(&get, "XDG_CONFIG_HOME", ".config");
    let mut vars = Vec::new();
    if daemons.iter().any(|name| name == "ca.desrt.dconf") {
        let user_db = user_config
            .as_ref()
            .map(|config| config.join("dconf/user"))
            .filter(|db| db.is_file());
        let profile = home.join("dconf-profile");
        std::fs::write(
            &profile,
            dconf_profile(user_db.as_deref(), &base_dconf_profile(&get)),
        )?;
        vars.push((
            "DCONF_PROFILE".to_string(),
            profile.to_string_lossy().into_owned(),
        ));
    }
    if daemons.iter().any(|name| name == "org.xfce.Xfconf")
        && let Some(config) = &user_config
    {
        seed_xfconf(config, root)?;
    }
    vars.push((
        selkies_draw::dirs::SETTINGS_VAR.to_string(),
        home.to_string_lossy().into_owned(),
    ));
    Ok(vars)
}

/// The names the bus at `address` can activate, empty where it does not answer.
pub fn activatable_names(address: &str) -> Vec<String> {
    zbus::block_on(async {
        let connection = zbus::connection::Builder::address(address)
            .build()
            .await
            .ok()?;
        let reply = connection
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "ListActivatableNames",
                &(),
            )
            .await
            .ok()?;
        reply.body().deserialize::<Vec<String>>().ok()
    })
    .unwrap_or_default()
}

/// Quarantine this process's own environment in `mode`, for a server run alone: the variables
/// [`env`] gives, set where the environment is not quarantined already, with the directories
/// made. Returns the root where this call quarantined the environment.
///
/// Runs before any thread starts: setting a variable races every other reader of the
/// environment.
pub fn apply_to_process(mode: Mode) -> Option<PathBuf> {
    let get = |name: &str| std::env::var(name).ok();
    let vars = env(mode, get);
    let root = vars
        .iter()
        .find(|(var, _)| var == QUARANTINE_VAR)
        .map(|(_, root)| PathBuf::from(root))?;
    if let Err(e) = prepare(&vars, get) {
        log::warn!(target: "main", "the quarantine under {} was not prepared: {e}", root.display());
    }
    for (var, value) in &vars {
        // SAFETY: called from `main` before the runtime or any other thread starts.
        unsafe { std::env::set_var(var, value) };
    }
    Some(root)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn quarantined(mode: Mode, pairs: &[(&str, &str)]) -> HashMap<String, String> {
        let given: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        env(mode, |k| given.get(k).cloned()).into_iter().collect()
    }

    #[test]
    fn the_modes_are_named_as_the_setting_takes_them() {
        for mode in [Mode::Off, Mode::Targeted, Mode::Full] {
            assert_eq!(Mode::parse(mode.name()), mode);
        }
        assert_eq!(Mode::parse("true"), Mode::Off);
    }

    #[test]
    fn a_targeted_quarantine_leads_the_system_directories_and_keeps_the_users_homes() {
        let vars = quarantined(
            Mode::Targeted,
            &[
                ("HOME", "/home/u"),
                ("XDG_CONFIG_DIRS", "/etc/xdg/xdg-ubuntu:/etc/xdg"),
            ],
        );
        let root = "/home/u/.local/state/selkies/quarantine";
        assert_eq!(vars[QUARANTINE_VAR], root);
        assert_eq!(
            vars["XDG_CONFIG_DIRS"],
            format!("{root}/config:/etc/xdg/xdg-ubuntu:/etc/xdg")
        );
        assert_eq!(
            vars["XDG_DATA_DIRS"],
            format!("{root}/data:/usr/local/share:/usr/share")
        );
        for (var, _, _) in HOMES {
            assert!(!vars.contains_key(*var), "{var} stays the user's");
        }
        assert!(!vars.contains_key("HOME"));
    }

    #[test]
    fn a_full_quarantine_moves_the_homes_under_the_users_own_state_home_and_the_home_stays() {
        let vars = quarantined(Mode::Full, &[("HOME", "/home/u")]);
        let root = "/home/u/.local/state/selkies/quarantine";
        assert_eq!(vars["XDG_CONFIG_HOME"], format!("{root}/config"));
        assert_eq!(vars["XDG_DATA_HOME"], format!("{root}/data"));
        assert_eq!(vars["XDG_STATE_HOME"], format!("{root}/state"));
        assert_eq!(vars["XDG_CACHE_HOME"], format!("{root}/cache"));
        assert_eq!(vars[QUARANTINE_VAR], root);
        assert!(!vars.contains_key("HOME"), "the home is the user's");
    }

    #[test]
    fn a_full_quarantine_reads_the_users_own_configuration_and_data_beneath_its_own() {
        let vars = quarantined(Mode::Full, &[("HOME", "/home/u")]);
        assert_eq!(vars["XDG_CONFIG_DIRS"], "/home/u/.config:/etc/xdg");
        assert_eq!(
            vars["XDG_DATA_DIRS"],
            "/home/u/.local/share:/usr/local/share:/usr/share"
        );
        let vars = quarantined(
            Mode::Full,
            &[
                ("HOME", "/home/u"),
                ("XDG_CONFIG_HOME", "/cfg"),
                ("XDG_DATA_HOME", "/data"),
                ("XDG_STATE_HOME", "/state"),
                ("XDG_CONFIG_DIRS", "/etc/xdg/xdg-ubuntu:/cfg:/etc/xdg"),
                ("XDG_DATA_DIRS", "/usr/share/ubuntu:/usr/share"),
            ],
        );
        assert_eq!(vars[QUARANTINE_VAR], "/state/selkies/quarantine");
        assert_eq!(vars["XDG_CONFIG_DIRS"], "/cfg:/etc/xdg/xdg-ubuntu:/etc/xdg");
        assert_eq!(vars["XDG_DATA_DIRS"], "/data:/usr/share/ubuntu:/usr/share");
    }

    #[test]
    fn a_relative_xdg_home_is_ignored_as_the_specification_asks() {
        let vars = quarantined(
            Mode::Full,
            &[
                ("HOME", "/home/u"),
                ("XDG_STATE_HOME", "state"),
                ("XDG_CONFIG_HOME", "/home/u/../other"),
            ],
        );
        assert_eq!(
            vars[QUARANTINE_VAR],
            "/home/u/.local/state/selkies/quarantine"
        );
        assert!(vars["XDG_CONFIG_DIRS"].starts_with("/home/u/.config:"));
    }

    #[test]
    fn no_mode_a_quarantined_environment_or_one_without_a_home_is_left_as_it_is() {
        assert!(quarantined(Mode::Off, &[("HOME", "/home/u")]).is_empty());
        for mode in [Mode::Targeted, Mode::Full] {
            assert!(
                quarantined(
                    mode,
                    &[
                        ("HOME", "/home/u"),
                        (QUARANTINE_VAR, "/home/u/.local/state/selkies/quarantine"),
                    ],
                )
                .is_empty()
            );
            assert!(quarantined(mode, &[]).is_empty());
            assert!(quarantined(mode, &[("HOME", "/")]).is_empty());
            assert!(quarantined(mode, &[("HOME", "relative")]).is_empty());
        }
    }

    /// The toolkit settings, style sheets, and user directories of a desktop the user already
    /// has, as a scratch home holds them before a session starts.
    const USER_SETTINGS: &str = "[Settings]\ngtk-theme-name=Mine\ngtk-font-name=Sans 11\n";
    const USER_CSS: &str = "window { color: red; }\n";
    const USER_DIRS: &str = "XDG_DESKTOP_DIR=\"$HOME/Schreibtisch\"\n";

    #[test]
    #[ignore = "run by the_users_own_desktop_configuration_is_left_as_it_is, in a process of its own"]
    fn the_users_own_desktop_configuration_in_its_process() {
        if !crate::session_screens::testing::isolated() {
            return;
        }
        let home = selkies_draw::dirs::home();
        let user = home.join(".config");
        let check = std::env::var("QUARANTINE_CHECK").unwrap_or_default();
        let session_daemons = check == "targeted-daemons";
        let mode = Mode::parse(check.trim_end_matches("-daemons"));
        let gsettings_log = PathBuf::from(std::env::var("GSETTINGS_LOG").expect("the log"));
        let root = (mode != Mode::Off).then(|| {
            let root = apply_to_process(mode).expect("the environment is quarantined");
            assert_eq!(
                apply_to_process(mode),
                None,
                "a quarantined environment is left as it is"
            );
            root
        });
        if let Some(root) = root.as_ref().filter(|_| session_daemons) {
            // SAFETY: the test runs alone in its own process, before any thread it starts.
            unsafe {
                std::env::set_var(selkies_draw::dirs::SETTINGS_VAR, settings_home(root));
            }
        }
        let (decision, _) = selkies_draw::appearance::toolkits::apply(
            selkies_draw::style::Theme::default(),
            None,
            &[],
        );
        let read = |path: PathBuf| std::fs::read_to_string(path).unwrap_or_default();
        let theme = format!("gtk-theme-name={}", decision.gtk_theme);
        let set_gsettings = !read(gsettings_log).is_empty();
        let Some(root) = root else {
            assert!(
                read(user.join("gtk-3.0/settings.ini")).contains(&theme),
                "without a quarantine the session's theme lands in the user's own settings"
            );
            assert!(set_gsettings, "and its scheme in the user's own GSettings");
            return;
        };
        assert_eq!(selkies_draw::dirs::home(), home, "the home is the user's");
        assert_eq!(selkies_draw::dirs::dotfile_home(), root.join("home"));
        assert_eq!(
            selkies_draw::dirs::session_config_home(),
            root.join("config")
        );
        assert_eq!(selkies_draw::dirs::session_data_home(), root.join("data"));
        for gtk in ["gtk-3.0", "gtk-4.0"] {
            assert_eq!(
                read(user.join(gtk).join("settings.ini")),
                USER_SETTINGS,
                "{gtk}"
            );
            assert_eq!(read(user.join(gtk).join("gtk.css")), USER_CSS, "{gtk}");
            assert!(
                read(root.join("config").join(gtk).join("settings.ini")).contains(&theme),
                "{gtk}: the session's theme is in the quarantine"
            );
        }
        assert_eq!(
            selkies_draw::apps::desktop_dir(),
            home.join("Schreibtisch"),
            "the user's own Desktop folder is the session's"
        );
        if mode == Mode::Targeted {
            assert!(selkies_draw::dirs::keeps_user_homes());
            assert_eq!(selkies_draw::dirs::config_home(), user, "the apps keep it");
            assert_eq!(selkies_draw::dirs::config_dirs()[0], root.join("config"));
            assert_eq!(selkies_draw::dirs::data_dirs()[0], root.join("data"));
            assert_eq!(
                set_gsettings, session_daemons,
                "GSettings are written only where the session's own dconf keeps them"
            );
            assert!(
                !root.join("config/gtk-3.0/gtk.css").exists(),
                "GTK reads no style sheet from beneath the user's"
            );
            return;
        }
        assert!(!selkies_draw::dirs::keeps_user_homes());
        assert_eq!(selkies_draw::dirs::config_home(), root.join("config"));
        assert_eq!(selkies_draw::dirs::data_home(), root.join("data"));
        assert_eq!(selkies_draw::dirs::state_home(), root.join("state"));
        assert_eq!(selkies_draw::dirs::config_dirs()[0], user);
        assert_eq!(
            selkies_draw::dirs::data_dirs()[0],
            home.join(".local/share")
        );
        assert_eq!(
            selkies_draw::style::desktop_config_dir(),
            root.join("config/selkies-desktop")
        );
        assert!(
            set_gsettings,
            "GSettings go to the quarantine's own database"
        );
        assert!(
            !user.join("selkies-desktop").exists(),
            "nothing of the desktop's own"
        );
    }

    #[test]
    fn the_users_own_desktop_configuration_is_left_as_it_is() {
        for check in ["off", "targeted", "targeted-daemons", "full"] {
            let home = crate::session_screens::testing::Scratch::new("quarantine-home");
            for gtk in ["gtk-3.0", "gtk-4.0"] {
                let dir = home.path(".config").join(gtk);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("settings.ini"), USER_SETTINGS).unwrap();
                std::fs::write(dir.join("gtk.css"), USER_CSS).unwrap();
            }
            std::fs::write(home.path(".config/user-dirs.dirs"), USER_DIRS).unwrap();
            let bin = home.path("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let log = home.path("gsettings.log");
            crate::session_screens::testing::script(
                &bin.join("gsettings"),
                &format!("echo \"$@\" >> '{}'", log.display()),
            );
            let path = |name: &str| home.path(name).display().to_string();
            let search = format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            );
            crate::session_screens::testing::run_isolated(
                "quarantine::tests::the_users_own_desktop_configuration_in_its_process",
                &[
                    ("QUARANTINE_CHECK", check),
                    ("GSETTINGS_LOG", &log.display().to_string()),
                    ("PATH", &search),
                    ("HOME", &home.text()),
                    ("XDG_CONFIG_HOME", &path(".config")),
                    ("XDG_DATA_HOME", &path(".local/share")),
                    ("XDG_STATE_HOME", &path(".local/state")),
                    ("XDG_CACHE_HOME", &path(".cache")),
                    ("XDG_CONFIG_DIRS", &path("etc")),
                    ("XDG_DATA_DIRS", &path("share")),
                    (QUARANTINE_VAR, ""),
                    ("DISPLAY", ":9999"),
                ],
            );
        }
    }

    #[test]
    fn a_full_quarantine_carries_the_users_own_user_directories_in_at_every_start() {
        let scratch = crate::session_screens::testing::Scratch::new("quarantine-user-dirs");
        let home = scratch.text();
        std::fs::create_dir_all(scratch.path(".config")).unwrap();
        let user_dirs = scratch.path(".config/user-dirs.dirs");
        std::fs::write(&user_dirs, "XDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\n").unwrap();
        let get = |k: &str| (k == "HOME").then(|| home.clone());
        let carried = scratch.path(".local/state/selkies/quarantine/config/user-dirs.dirs");
        prepare(&env(Mode::Targeted, get), get).expect("prepared");
        assert!(
            !carried.exists(),
            "a targeted quarantine reads the user's own"
        );
        let vars = env(Mode::Full, get);
        prepare(&vars, get).expect("prepared");
        assert_eq!(
            std::fs::read_to_string(&carried).unwrap(),
            "XDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\n"
        );
        std::fs::write(&user_dirs, "XDG_DOWNLOAD_DIR=\"$HOME/Herunterladen\"\n").unwrap();
        prepare(&vars, get).expect("prepared again");
        assert_eq!(
            std::fs::read_to_string(&carried).unwrap(),
            "XDG_DOWNLOAD_DIR=\"$HOME/Herunterladen\"\n",
            "the user's own names win at the next start"
        );
        assert!(
            !scratch
                .path(".local/state/selkies/quarantine/config/user-dirs.locale")
                .exists()
        );
    }

    #[test]
    fn a_settings_daemon_is_activated_as_the_system_does_in_a_home_of_the_quarantines() {
        let system = "[D-BUS Service]\nName=org.xfce.Xfconf\nExec=/usr/lib/xfce4/xfconf/xfconfd\n";
        assert_eq!(
            wrapped_service(
                "org.xfce.Xfconf",
                system,
                Path::new("/usr/bin/env"),
                Path::new("/home/it's/q/settings"),
            )
            .as_deref(),
            Some(
                "[D-BUS Service]\nName=org.xfce.Xfconf\nExec='/usr/bin/env' \
                 'XDG_CONFIG_HOME=/home/it'\\''s/q/settings' /usr/lib/xfce4/xfconf/xfconfd\n"
            )
        );
        assert_eq!(
            wrapped_service(
                "x",
                "[D-BUS Service]\nName=x\n",
                Path::new("/e"),
                Path::new("/h")
            ),
            None
        );
    }

    #[test]
    fn the_settings_daemons_found_get_service_files_in_the_runtime_directory() {
        let scratch = crate::session_screens::testing::Scratch::new("quarantine-services");
        let root = scratch.path("q");
        let system = scratch.path("share/dbus-1/services");
        std::fs::create_dir_all(&system).unwrap();
        std::fs::write(
            system.join("ca.desrt.dconf.service"),
            "[D-BUS Service]\nName=ca.desrt.dconf\nExec=/usr/libexec/dconf-service\n",
        )
        .unwrap();
        let own = root.join("data/dbus-1/services");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(
            own.join("org.xfce.Xfconf.service"),
            "[D-BUS Service]\nName=org.xfce.Xfconf\nExec=/stale\n",
        )
        .unwrap();
        let runtime = scratch.path("run");
        let dirs = [root.join("data"), scratch.path("share")];
        assert_eq!(
            write_daemon_services(&runtime, &root, &dirs, None).unwrap(),
            Vec::<String>::new(),
            "no env, no service files"
        );
        let written =
            write_daemon_services(&runtime, &root, &dirs, Some(Path::new("/usr/bin/env"))).unwrap();
        assert_eq!(
            written,
            ["ca.desrt.dconf"],
            "the quarantine's own are no system's"
        );
        let services = runtime.join("dbus-1/services");
        let dconf = std::fs::read_to_string(services.join("ca.desrt.dconf.service")).unwrap();
        assert!(dconf.contains(&format!(
            "'XDG_CONFIG_HOME={}' /usr/libexec/dconf-service",
            settings_home(&root).display()
        )));
        assert!(services.join(format!("{PROBE_NAME}.service")).is_file());
        assert!(!services.join("org.xfce.Xfconf.service").exists());
    }

    #[test]
    fn the_users_xfce_settings_are_seeded_once() {
        let scratch = crate::session_screens::testing::Scratch::new("quarantine-xfconf");
        let user = scratch.path("config");
        let channels = user.join("xfce4/xfconf/xfce-perchannel-xml");
        std::fs::create_dir_all(&channels).unwrap();
        std::fs::write(channels.join("xsettings.xml"), "mine").unwrap();
        std::fs::write(channels.join("notes.txt"), "not a channel").unwrap();
        let root = scratch.path("q");
        assert!(seed_xfconf(&user, &root).unwrap());
        let seeded = settings_home(&root).join("xfce4/xfconf/xfce-perchannel-xml");
        assert_eq!(
            std::fs::read_to_string(seeded.join("xsettings.xml")).unwrap(),
            "mine"
        );
        assert!(!seeded.join("notes.txt").exists());
        std::fs::write(seeded.join("xsettings.xml"), "the session's").unwrap();
        std::fs::write(channels.join("xsettings.xml"), "changed").unwrap();
        assert!(!seed_xfconf(&user, &root).unwrap(), "seeded once");
        assert_eq!(
            std::fs::read_to_string(seeded.join("xsettings.xml")).unwrap(),
            "the session's"
        );
        assert_eq!(
            std::fs::read_to_string(channels.join("xsettings.xml")).unwrap(),
            "changed"
        );
    }

    #[test]
    fn the_dconf_profile_writes_the_quarantine_and_reads_the_users_and_the_systems() {
        assert_eq!(
            dconf_profile(
                Some(Path::new("/home/u/.config/dconf/user")),
                "user-db:user\nsystem-db:local\n# comment\nsystem-db:site\n"
            ),
            "service-db:keyfile/user\nfile-db:/home/u/.config/dconf/user\nsystem-db:local\nsystem-db:site\n"
        );
        assert_eq!(dconf_profile(None, ""), "service-db:keyfile/user\n");
    }

    #[test]
    fn the_session_takes_its_settings_daemons_only_once_the_bus_reads_their_service_files() {
        let scratch = crate::session_screens::testing::Scratch::new("quarantine-settings-env");
        let home = scratch.text();
        let root = scratch.path("q");
        let get = |k: &str| (k == "HOME").then(|| home.clone());
        let daemons = ["ca.desrt.dconf".to_string(), "org.xfce.Xfconf".to_string()];
        assert!(settings_env(&root, &daemons, &[], get).unwrap().is_empty());
        assert!(
            settings_env(&root, &[], &[PROBE_NAME.to_string()], get)
                .unwrap()
                .is_empty()
        );
        let channels = scratch.path(".config/xfce4/xfconf/xfce-perchannel-xml");
        std::fs::create_dir_all(&channels).unwrap();
        std::fs::write(channels.join("xsettings.xml"), "mine").unwrap();
        let vars: HashMap<String, String> =
            settings_env(&root, &daemons, &[PROBE_NAME.to_string()], get)
                .unwrap()
                .into_iter()
                .collect();
        let settings = settings_home(&root);
        assert_eq!(
            vars[selkies_draw::dirs::SETTINGS_VAR],
            settings.to_string_lossy()
        );
        let profile = std::fs::read_to_string(&vars["DCONF_PROFILE"]).unwrap();
        assert!(
            profile.starts_with("service-db:keyfile/user\n"),
            "{profile}"
        );
        assert!(
            !profile.contains("file-db:"),
            "the user has no database yet"
        );
        assert!(
            settings
                .join("xfce4/xfconf/xfce-perchannel-xml/xsettings.xml")
                .is_file()
        );
    }

    /// The user's own GSettings, written through a bus and a dconf that are the user's.
    fn user_gsettings(home: &Path, runtime: &Path, args: &[&str]) {
        let bus = Bus::start(home, runtime);
        assert!(
            gsettings(home, runtime, &bus.address, None, args)
                .status
                .success()
        );
    }

    /// A session bus in `runtime`, ended with this value.
    struct Bus {
        _daemon: crate::session_screens::testing::Owned,
        address: String,
    }

    impl Bus {
        fn start(home: &Path, runtime: &Path) -> Bus {
            std::fs::create_dir_all(runtime).unwrap();
            let socket = runtime.join("bus");
            let address = format!("unix:path={}", socket.display());
            let daemon = crate::session_screens::testing::Owned::spawn(
                std::process::Command::new("dbus-daemon")
                    .args(["--session", "--nofork", "--nopidfile"])
                    .arg(format!("--address={address}"))
                    .env_clear()
                    .envs(base_env(home, runtime)),
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !socket.exists() {
                assert!(std::time::Instant::now() < deadline, "the bus comes up");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Bus {
                _daemon: daemon,
                address,
            }
        }
    }

    fn base_env(home: &Path, runtime: &Path) -> Vec<(String, String)> {
        vec![
            ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
            ("HOME".into(), home.display().to_string()),
            (
                "XDG_CONFIG_HOME".into(),
                home.join(".config").display().to_string(),
            ),
            ("XDG_RUNTIME_DIR".into(), runtime.display().to_string()),
            ("XDG_DATA_DIRS".into(), "/usr/local/share:/usr/share".into()),
        ]
    }

    fn gsettings(
        home: &Path,
        runtime: &Path,
        address: &str,
        profile: Option<&str>,
        args: &[&str],
    ) -> std::process::Output {
        let mut command = std::process::Command::new("gsettings");
        command
            .args(args)
            .env_clear()
            .envs(base_env(home, runtime))
            .env("DBUS_SESSION_BUS_ADDRESS", address);
        if let Some(profile) = profile {
            command.env("DCONF_PROFILE", profile);
        }
        command.output().expect("gsettings runs")
    }

    #[test]
    fn a_session_bus_hands_gsettings_to_a_dconf_of_the_quarantines_own() {
        let schema = std::process::Command::new("gsettings")
            .args(["list-keys", "org.gnome.desktop.interface"])
            .output();
        let env = crate::commands::sessions::which("env");
        let dconf = PathBuf::from("/usr/share/dbus-1/services/ca.desrt.dconf.service");
        if crate::commands::sessions::which("dbus-daemon").is_none()
            || !schema.is_ok_and(|out| out.status.success())
            || env.is_none()
            || !dconf.is_file()
        {
            eprintln!("skipped: needs dbus-daemon, dconf, and gsettings with the GNOME schemas");
            return;
        }
        let scratch = crate::session_screens::testing::Scratch::new("quarantine-dconf");
        let home = scratch.0.clone();
        user_gsettings(
            &home,
            &scratch.path("user-run"),
            &["set", "org.gnome.desktop.interface", "font-name", "Mine 13"],
        );
        let user_db = home.join(".config/dconf/user");
        let before = std::fs::read(&user_db).expect("the user's database");

        let root = scratch.path(".local/state/selkies/quarantine");
        let runtime = scratch.path("session-run");
        let daemons = write_daemon_services(
            &runtime,
            &root,
            &[PathBuf::from("/usr/share")],
            env.as_deref(),
        )
        .unwrap();
        let bus = Bus::start(&home, &runtime);
        let home_text = home.display().to_string();
        let vars: HashMap<String, String> =
            settings_env(&root, &daemons, &activatable_names(&bus.address), |k| {
                (k == "HOME").then(|| home_text.clone())
            })
            .unwrap()
            .into_iter()
            .collect();
        let profile = vars
            .get("DCONF_PROFILE")
            .expect("the bus reads the runtime directory");
        let get = |key: &str| {
            let out = gsettings(
                &home,
                &runtime,
                &bus.address,
                Some(profile),
                &["get", "org.gnome.desktop.interface", key],
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        assert_eq!(
            get("font-name"),
            "'Mine 13'",
            "the session starts from the user's"
        );
        let set = gsettings(
            &home,
            &runtime,
            &bus.address,
            Some(profile),
            &[
                "set",
                "org.gnome.desktop.interface",
                "color-scheme",
                "prefer-dark",
            ],
        );
        assert!(
            set.status.success(),
            "{}",
            String::from_utf8_lossy(&set.stderr)
        );
        assert_eq!(get("color-scheme"), "'prefer-dark'");
        let keyfile = settings_home(&root).join("dconf/user.txt");
        assert!(
            std::fs::read_to_string(&keyfile)
                .unwrap_or_default()
                .contains("color-scheme='prefer-dark'"),
            "the write lands in the quarantine"
        );
        assert_eq!(
            std::fs::read(&user_db).unwrap(),
            before,
            "the user's database is untouched"
        );
        assert!(!home.join(".config/dconf/user.txt").exists());
    }

    #[test]
    fn the_quarantines_directories_are_its_owners_alone() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = crate::session_screens::testing::Scratch::new("quarantine-dirs");
        let home = scratch.text();
        let get = |k: &str| (k == "HOME").then(|| home.clone());
        let root = scratch.path(".local/state/selkies/quarantine");
        let mode_of = |dir: &str| {
            std::fs::metadata(root.join(dir))
                .expect(dir)
                .permissions()
                .mode()
                & 0o777
        };
        prepare(&env(Mode::Targeted, get), get).expect("the directories are made");
        for dir in ["", "home", "config", "data"] {
            assert_eq!(mode_of(dir), 0o700, "{dir}");
        }
        assert!(!root.join("state").exists());
        prepare(&env(Mode::Full, get), get).expect("the directories are made");
        for dir in ["state", "cache"] {
            assert_eq!(mode_of(dir), 0o700, "{dir}");
        }
    }
}
