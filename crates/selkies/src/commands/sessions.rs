/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The desktop a session runs, found as a display manager finds one.
//!
//! The session files of the XDG data directories are the menu: `xsessions` on X11,
//! `wayland-sessions` on Wayland, where the session nests in Selkies' compositor. A session is
//! named by file, desktop, or program name; unset, the desktop `XDG_CURRENT_DESKTOP` names
//! comes first, then the default the host's display manager or session alternative records
//! ([`configured_desktops`]), then the session named after its own desktop, then the first by
//! name. A name only the other backend's sessions carry stands for their desktop, as `plasma`
//! does for KDE's Wayland session on Plasma 5, and a name no file carries runs as a command.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The `[Desktop Entry]` keys of a session file.
pub type Entry = BTreeMap<String, String>;

/// The `[Desktop Entry]` keys of a desktop file, the first value of a repeated key kept.
pub fn read_entry(path: &Path) -> Entry {
    let mut entry = Entry::new();
    let Ok(text) = fs::read(path) else {
        return entry;
    };
    let mut in_group = false;
    for line in String::from_utf8_lossy(&text).lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
        } else if in_group
            && !line.starts_with('#')
            && let Some((key, value)) = line.split_once('=')
        {
            entry
                .entry(key.trim().to_string())
                .or_insert_with(|| value.trim().to_string());
        }
    }
    entry
}

/// The first executable `name` resolves to: as given when it carries a directory, else along
/// `PATH`.
pub fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |path: &Path| {
        fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if name.contains('/') {
        let path = PathBuf::from(name);
        return executable(&path).then_some(path);
    }
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|path| executable(path))
}

/// The XDG data directories, the user's first: `XDG_DATA_HOME` or `~/.local/share`, then
/// `XDG_DATA_DIRS` or `/usr/local/share:/usr/share`.
pub fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| Path::new(&h).join(".local/share"))
        });
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    home.into_iter()
        .chain(
            system
                .split(':')
                .filter(|d| !d.is_empty())
                .map(PathBuf::from),
        )
        .collect()
}

/// The sessions a display manager would offer for the backend, by file name.
///
/// The user's data directory comes first, so a session file there adds or replaces one
/// without root; the first directory holding a name wins. A hidden session and one whose
/// `TryExec` is not installed are left out.
pub fn installed_sessions(data_dirs: &[PathBuf], wayland: bool) -> BTreeMap<String, Entry> {
    let kind = if wayland {
        "wayland-sessions"
    } else {
        "xsessions"
    };
    let mut found: BTreeMap<String, Entry> = BTreeMap::new();
    for folder in data_dirs.iter().map(|d| d.join(kind)) {
        let Ok(names) = fs::read_dir(&folder) else {
            continue;
        };
        let mut names: Vec<String> = names
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect();
        names.sort();
        for name in names {
            if let Some(sid) = name.strip_suffix(".desktop") {
                found
                    .entry(sid.to_string())
                    .or_insert_with(|| read_entry(&folder.join(&name)));
            }
        }
    }
    found.retain(|_, entry| {
        entry.get("Exec").is_some_and(|e| !e.is_empty())
            && entry.get("Hidden").map(String::as_str) != Some("true")
            && entry
                .get("TryExec")
                .filter(|t| !t.is_empty())
                .is_none_or(|t| which(t).is_some())
    });
    found
}

/// The desktops an entry's `DesktopNames` lists, lowercased.
pub fn desktop_names(entry: &Entry) -> Vec<String> {
    entry
        .get("DesktopNames")
        .map(|names| {
            names
                .to_lowercase()
                .replace(':', ";")
                .split(';')
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The last path component, as a program name.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The sessions `hints` name by file, desktop, or program: one named by file first, then one
/// named after its own desktop (`xfce` for XFCE, not `gnome-xorg` for GNOME), then name order.
/// A command line names none.
pub fn matching(sessions: &BTreeMap<String, Entry>, hints: &[&str]) -> Vec<String> {
    let keys: Vec<String> = hints
        .iter()
        .filter(|h| !h.contains(' '))
        .map(|h| {
            let name = basename(h).to_lowercase();
            name.strip_suffix(".desktop").unwrap_or(&name).to_string()
        })
        .collect();
    let mut found: Vec<&String> = sessions
        .iter()
        .filter(|(sid, entry)| {
            keys.contains(&sid.to_lowercase())
                || desktop_names(entry).iter().any(|n| keys.contains(n))
                || entry.get("Exec").is_some_and(|exec| {
                    exec.split_whitespace()
                        .any(|word| keys.contains(&basename(word).to_lowercase()))
                })
        })
        .map(|(sid, _)| sid)
        .collect();
    found.sort_by_key(|sid| {
        let lower = sid.to_lowercase();
        (
            !keys.contains(&lower),
            !desktop_names(&sessions[*sid]).contains(&lower),
        )
    });
    found.into_iter().cloned().collect()
}

/// What the last of `paths` to set `key` sets it to, quotes stripped.
pub fn config_value(paths: &[PathBuf], key: &str) -> String {
    let mut value = String::new();
    for path in paths {
        let Ok(text) = fs::read(path) else {
            continue;
        };
        for line in String::from_utf8_lossy(&text).lines() {
            if let Some((name, rest)) = line.split_once('=')
                && name.trim() == key
            {
                value = rest.trim().trim_matches(['"', '\'']).to_string();
            }
        }
    }
    value
}

/// Files matching `dir/*.conf`, sorted.
fn conf_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "conf"))
        .collect();
    found.sort();
    found
}

/// The desktop the host starts by default, the user's last choice first: LightDM's and GDM's
/// defaults, Debian's session manager alternative, and Red Hat's `/etc/sysconfig/desktop`.
/// The system's files are read under `root`, the user's under `home`.
pub fn configured_desktops(root: &Path, home: &Path) -> Vec<String> {
    let mut lightdm: Vec<PathBuf> = ["usr/share", "usr/local/share", "etc/xdg", "etc"]
        .iter()
        .flat_map(|d| conf_files(&root.join(d).join("lightdm/lightdm.conf.d")))
        .collect();
    lightdm.push(root.join("etc/lightdm/lightdm.conf"));
    let gdm: Vec<PathBuf> = ["gdm3/daemon.conf", "gdm3/custom.conf", "gdm/custom.conf"]
        .iter()
        .map(|name| root.join("etc").join(name))
        .collect();
    let manager = root.join("etc/alternatives/x-session-manager");
    let sysconfig = [root.join("etc/sysconfig/desktop")];
    let manager = if manager.is_symlink() {
        fs::canonicalize(&manager)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        String::new()
    };
    vec![
        config_value(&[home.join(".dmrc")], "Session"),
        config_value(&lightdm, "user-session"),
        config_value(&gdm, "FallbackSession"),
        manager,
        config_value(&sysconfig, "DESKTOP"),
        config_value(&sysconfig, "PREFERRED"),
    ]
}

/// What the host offers and prefers, read once per launch.
#[derive(Clone, Debug, Default)]
pub struct Desktops {
    /// Where the session files are looked for.
    pub data_dirs: Vec<PathBuf>,
    /// The desktops `XDG_CURRENT_DESKTOP` names.
    pub current_desktop: Vec<String>,
    /// The host's defaults ([`configured_desktops`]).
    pub configured: Vec<String>,
}

impl Desktops {
    /// The host as the environment describes it.
    pub fn from_env() -> Desktops {
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
        Desktops {
            data_dirs: data_dirs(),
            current_desktop: std::env::var("XDG_CURRENT_DESKTOP")
                .unwrap_or_default()
                .split(':')
                .filter(|d| !d.is_empty())
                .map(str::to_string)
                .collect(),
            configured: configured_desktops(Path::new("/"), &home),
        }
    }

    /// The sessions installed for the backend.
    pub fn installed(&self, wayland: bool) -> BTreeMap<String, Entry> {
        installed_sessions(&self.data_dirs, wayland)
    }

    /// The session file `wanted` names by file, desktop, or program name, else `wanted` run
    /// as a command. Unset, the desktop `XDG_CURRENT_DESKTOP` names, then the host's default,
    /// then one named after its own desktop, then the first installed. `("", {})` when
    /// nothing is installed.
    ///
    /// # Errors
    /// `wanted` names only the other backend's sessions and their desktop has none for this
    /// one (Plasma 6.8 ships no X11 session): the message says which backend has it, where
    /// running the name as a command would start a program that does not exist.
    pub fn pick(&self, wanted: Option<&str>, wayland: bool) -> Result<(String, Entry), String> {
        let sessions = self.installed(wayland);
        let others = self.installed(!wayland);
        let hints: Vec<String> = match wanted {
            Some(w) => vec![w.to_string()],
            None => self
                .current_desktop
                .iter()
                .chain(self.configured.iter())
                .cloned()
                .collect(),
        };
        for hint in hints.iter().filter(|h| !h.is_empty()) {
            let mut found = matching(&sessions, &[hint]);
            if found.is_empty() {
                let names: Vec<String> = matching(&others, &[hint])
                    .iter()
                    .flat_map(|sid| desktop_names(&others[sid]))
                    .collect();
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                found = matching(&sessions, &names);
            }
            if let Some(sid) = found.into_iter().next() {
                let entry = sessions[&sid].clone();
                return Ok((sid, entry));
            }
        }
        if let Some(wanted) = wanted {
            let elsewhere = matching(&others, &[wanted]);
            if !elsewhere.is_empty() {
                let (have, other) = if wayland {
                    ("Wayland", "X11")
                } else {
                    ("X11", "Wayland")
                };
                return Err(format!(
                    "{wanted} names a {other} session here ({}), and its desktop has no {have} \
                     session installed: start it with --wayland={}",
                    elsewhere.join(", "),
                    !wayland
                ));
            }
            return Ok((
                String::new(),
                Entry::from([("Exec".to_string(), wanted.to_string())]),
            ));
        }
        let mut ranked: Vec<&String> = sessions.keys().collect();
        ranked.sort_by_key(|sid| !desktop_names(&sessions[*sid]).contains(&sid.to_lowercase()));
        Ok(match ranked.first() {
            Some(sid) => ((*sid).clone(), sessions[*sid].clone()),
            None => (String::new(), Entry::new()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn entry(name: &str, exec: &str, extra: &str) -> String {
        format!(
            "[Desktop Entry]\nName={name}\nExec={exec}\n{extra}\n[Desktop Action other]\nExec=wrong\n"
        )
    }

    fn scratch(tag: &str) -> PathBuf {
        tempfile::Builder::new()
            .prefix(&format!("selkies-sessions-{tag}-"))
            .tempdir()
            .unwrap()
            .keep()
    }

    fn host(root: &Path) -> Desktops {
        let (home, system) = (root.join("home"), root.join("system"));
        write(
            &system.join("xsessions/plasmax11.desktop"),
            &entry("Plasma (X11)", "startplasma-x11", "DesktopNames=KDE"),
        );
        write(
            &system.join("xsessions/xfce.desktop"),
            &entry("Xfce", "startxfce4 --flag", "DesktopNames=XFCE;"),
        );
        write(
            &system.join("xsessions/gone.desktop"),
            &entry("Gone", "gone", "TryExec=/nonexistent/gone"),
        );
        write(
            &system.join("xsessions/hidden.desktop"),
            &entry("Hidden", "hidden", "Hidden=true"),
        );
        write(&system.join("xsessions/notes.txt"), "not a session");
        write(
            &system.join("wayland-sessions/plasma.desktop"),
            &entry("Plasma", "startplasma-wayland", "DesktopNames=KDE"),
        );
        write(
            &system.join("wayland-sessions/sway.desktop"),
            &entry("Sway", "sway", "DesktopNames=sway"),
        );
        write(
            &home.join("xsessions/xfce.desktop"),
            &entry("Xfce mine", "my-xfce", "DesktopNames=XFCE"),
        );
        Desktops {
            data_dirs: vec![home, system],
            current_desktop: Vec::new(),
            configured: Vec::new(),
        }
    }

    fn keys(sessions: &BTreeMap<String, Entry>) -> Vec<&str> {
        sessions.keys().map(String::as_str).collect()
    }

    #[test]
    fn sessions_are_the_files_a_display_manager_lists() {
        let host = host(&scratch("installed"));
        let x11 = host.installed(false);
        assert_eq!(
            keys(&x11),
            ["plasmax11", "xfce"],
            "hidden ones and missing TryExec left out"
        );
        assert_eq!(
            x11["xfce"]["Exec"], "my-xfce",
            "the user's data directory shadows a system session"
        );
        assert_eq!(
            x11["plasmax11"]["Exec"], "startplasma-x11",
            "keys come from the [Desktop Entry] group alone"
        );
        assert_eq!(keys(&host.installed(true)), ["plasma", "sway"]);
    }

    #[test]
    fn a_session_is_picked_by_file_desktop_or_program() {
        let mut host = host(&scratch("pick"));
        assert_eq!(
            host.pick(Some("plasmax11"), false).unwrap().0,
            "plasmax11",
            "by file name"
        );
        assert_eq!(
            host.pick(Some("kde"), false).unwrap().0,
            "plasmax11",
            "by desktop name, any case"
        );
        assert_eq!(
            host.pick(Some("KDE"), true).unwrap().0,
            "plasma",
            "following the backend"
        );
        assert_eq!(
            host.pick(Some("/usr/bin/my-xfce"), false).unwrap().0,
            "xfce",
            "by the program it runs"
        );
        assert_eq!(
            host.pick(Some("plasma"), false).unwrap().0,
            "plasmax11",
            "a name only the other backend carries stands for its desktop"
        );
        assert_eq!(
            host.pick(Some("startplasma-x11"), true).unwrap().0,
            "plasma"
        );
        let refused = host.pick(Some("sway"), false).unwrap_err();
        assert!(
            refused.contains("sway names a Wayland session here (sway)")
                && refused.contains("--wayland=true"),
            "a name only the other backend carries, whose desktop has none here, is refused with the \
             backend that has it, not run as a command: {refused}"
        );
        assert_eq!(
            host.pick(Some("sway"), true).unwrap().0,
            "sway",
            "and found on that backend"
        );
        let command = Entry::from([("Exec".to_string(), "firefox --kiosk".to_string())]);
        assert_eq!(
            host.pick(Some("firefox --kiosk"), false).unwrap(),
            (String::new(), command),
            "a name no file carries runs as a command"
        );
        let command = Entry::from([("Exec".to_string(), "my-xfce --x".to_string())]);
        assert_eq!(
            host.pick(Some("my-xfce --x"), false).unwrap(),
            (String::new(), command),
            "a command line runs as given"
        );
        assert_eq!(
            host.pick(None, false).unwrap().0,
            "xfce",
            "unset, a session named after its own desktop comes first"
        );
        host.configured = ["", "plasma", "", "/usr/bin/startplasma-x11"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            host.pick(None, false).unwrap().0,
            "plasmax11",
            "unset, the host's default"
        );
        assert_eq!(host.pick(None, true).unwrap().0, "plasma");
        host.current_desktop = vec!["ubuntu".to_string(), "XFCE".to_string()];
        assert_eq!(
            host.pick(None, false).unwrap().0,
            "xfce",
            "XDG_CURRENT_DESKTOP names the desktop ahead of the host's default"
        );
        assert_eq!(
            host.pick(Some("KDE"), false).unwrap().0,
            "plasmax11",
            "--session outranks both"
        );
        host.configured.clear();
        host.current_desktop = vec!["GNOME".to_string()];
        assert_eq!(
            host.pick(None, false).unwrap().0,
            "xfce",
            "a desktop none installed falls through"
        );
        let empty = Desktops {
            data_dirs: vec![scratch("empty")],
            ..Desktops::default()
        };
        assert_eq!(
            empty.pick(None, true),
            Ok((String::new(), Entry::new())),
            "nothing installed, nothing wanted"
        );
    }

    #[test]
    fn the_hosts_defaults_are_read_in_order() {
        let root = scratch("configured");
        let (etc, home) = (root.join("host"), root.join("home"));
        write(
            &etc.join("usr/share/lightdm/lightdm.conf.d/50-packaged.conf"),
            "[Seat:*]\nuser-session=packaged\n",
        );
        write(
            &etc.join("etc/lightdm/lightdm.conf.d/10-site.conf"),
            "[Seat:*]\nuser-session=site\n",
        );
        write(
            &etc.join("etc/lightdm/lightdm.conf"),
            "[Seat:*]\n#user-session=commented\n",
        );
        write(
            &etc.join("etc/gdm/custom.conf"),
            "[daemon]\nFallbackSession=gnome-xorg\n",
        );
        write(&etc.join("usr/bin/startxfce4"), "");
        write(
            &etc.join("etc/sysconfig/desktop"),
            "DESKTOP=\"KDE\"\nPREFERRED=/usr/bin/startkde\n",
        );
        fs::create_dir_all(etc.join("etc/alternatives")).unwrap();
        symlink(
            etc.join("usr/bin/startxfce4"),
            etc.join("etc/alternatives/x-session-manager"),
        )
        .unwrap();
        write(&home.join(".dmrc"), "[Desktop]\nSession=xfce\n");
        let found = configured_desktops(&etc, &home);
        let manager = fs::canonicalize(etc.join("usr/bin/startxfce4"))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            found,
            [
                "xfce",
                "site",
                "gnome-xorg",
                &manager,
                "KDE",
                "/usr/bin/startkde"
            ]
        );
        assert!(
            configured_desktops(&root.join("empty"), &root.join("nohome"))
                .iter()
                .all(String::is_empty),
            "a host that records none gives none"
        );
    }

    #[test]
    fn which_resolves_paths_and_names() {
        assert!(which("/nonexistent/gone").is_none());
        assert!(which("sh").is_some());
        assert!(which("surely-not-a-program-here").is_none());
    }
}
