//! Desktop entries as the shell uses them: the scanner and launch helpers are
//! `selkies_draw::apps`, shared with the window manager's applications menu, and
//! this module adds what only the shell does with them: the translated category
//! names, the terminal setting, the recent list, the launches, and the file
//! manager's own entry.

use std::path::PathBuf;

pub use selkies_draw::apps::{
    Apps, Entry, applications_dirs, desktop_dir, exec_argv, exec_argv_files, icon_for_app_id, load_entry,
    resolve_program, spawn_detached,
};

use crate::config::{self, Config};

/// The translated name of a menu category.
///
/// # Arguments
/// * `name` - The category as the shared category map names it.
pub fn category_name(name: &str) -> &'static str {
    use crate::i18n::tr;
    match name {
        "Multimedia" => tr("Multimedia"),
        "Development" => tr("Development"),
        "Education" => tr("Education"),
        "Games" => tr("Games"),
        "Graphics" => tr("Graphics"),
        "Internet" => tr("Internet"),
        "Office" => tr("Office"),
        "Science" => tr("Science"),
        "Settings" => tr("Settings"),
        "System" => tr("System"),
        "Utilities" => tr("Utilities"),
        _ => tr("Other"),
    }
}

/// Launches the desktop keeps in the recent list.
const RECENT_LIMIT: usize = 12;

/// The terminal command to wrap entries in, as argv without the program to run,
/// the `terminal` setting taking precedence.
///
/// # Arguments
/// * `config` - The settings.
pub fn find_terminal(config: &Config) -> Option<Vec<String>> {
    selkies_draw::apps::find_terminal(config.terminal.as_deref())
}

/// Launches an entry on files, the way `launch` does.
///
/// # Arguments
/// * `entry` - The entry to launch.
/// * `config` - The settings, for the terminal command.
/// * `files` - The files to open.
pub fn launch_with(entry: &Entry, config: &Config, files: &[PathBuf]) {
    let mut argv = exec_argv_files(
        &entry.exec,
        entry.icon.as_deref(),
        &entry.name,
        &entry.path,
        files,
    );
    if argv.is_empty() {
        return;
    }
    if entry.terminal
        && let Some(mut terminal) = find_terminal(config)
    {
        terminal.append(&mut argv);
        argv = terminal;
    }
    spawn_detached(&argv, entry.working_dir.as_deref());
}

/// The desktop file ID of the built-in file manager.
pub const FILES_ID: &str = "selkies-desktop-files.desktop";
/// The app id the file manager's windows carry, the desktop entry's name.
pub const FILES_APP_ID: &str = "selkies-desktop-files";
/// The settings window's desktop entry, which lists it among the applications.
pub const SETTINGS_ID: &str = "selkies-desktop-settings.desktop";
/// The app id the settings window carries, the desktop entry's name.
pub const SETTINGS_APP_ID: &str = "selkies-desktop-settings";

/// Writes the file manager's desktop entry into the user's applications
/// directory when it is missing, so the menu lists it and `xdg-open` hands
/// folders to it, and makes it the folder handler when none is set.
pub fn ensure_files_entry() {
    let dir = config::data_home().join("applications");
    let path = dir.join(FILES_ID);
    if !path.is_file() && std::fs::create_dir_all(&dir).is_ok() {
        let mut text = String::from("[Desktop Entry]\nType=Application\nName=Files\n");
        for (code, name) in crate::i18n::all("Files") {
            text.push_str(&format!("Name[{code}]={name}\n"));
        }
        text.push_str("Comment=Browse and manage your files\n");
        for (code, comment) in crate::i18n::all("Browse and manage your files") {
            text.push_str(&format!("Comment[{code}]={comment}\n"));
        }
        text.push_str(
            "Exec=selkies-desktop --files %f\nIcon=system-file-manager\nTerminal=false\n\
             Categories=System;FileManager;\nMimeType=inode/directory;\nStartupNotify=false\n",
        );
        let _ = std::fs::write(&path, text);
    }
    let mimeapps = crate::files::mime::user_mimeapps();
    let text = std::fs::read_to_string(&mimeapps).unwrap_or_default();
    if !default_set(&text, "inode/directory")
        && let Some(parent) = mimeapps.parent()
        && std::fs::create_dir_all(parent).is_ok()
    {
        let _ = std::fs::write(&mimeapps, add_default(&text, "inode/directory", FILES_ID));
    }
}

/// Writes the settings window's desktop entry where none is, so the start menu, the launcher,
/// and the window manager's frames know the window by its entry: its name, its icon, and the
/// command that opens it.
pub fn ensure_settings_entry() {
    let dir = config::data_home().join("applications");
    let path = dir.join(SETTINGS_ID);
    if path.is_file() || std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let mut text = String::from("[Desktop Entry]\nType=Application\nName=Settings\n");
    for (code, name) in crate::i18n::all("Settings") {
        text.push_str(&format!("Name[{code}]={name}\n"));
    }
    text.push_str("Comment=Configure the desktop\n");
    for (code, comment) in crate::i18n::all("Configure the desktop") {
        text.push_str(&format!("Comment[{code}]={comment}\n"));
    }
    text.push_str(
        "Exec=selkies-desktop --settings\nIcon=preferences-system\nTerminal=false\n\
         Categories=Settings;System;\nStartupNotify=false\n",
    );
    let _ = std::fs::write(&path, text);
}

/// Whether a `mimeapps.list` names a default application for a type.
fn default_set(text: &str, mime: &str) -> bool {
    let mut in_defaults = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_defaults = line == "[Default Applications]";
            continue;
        }
        if in_defaults
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == mime
            && !value.trim().trim_end_matches(';').is_empty()
        {
            return true;
        }
    }
    false
}

/// A `mimeapps.list` with a default added for a type, the group created when
/// the file lacks it.
fn add_default(text: &str, mime: &str, id: &str) -> String {
    let line = format!("{mime}={id};");
    let mut out = String::new();
    let mut inserted = false;
    let mut in_defaults = false;
    for existing in text.lines() {
        let trimmed = existing.trim();
        if trimmed.starts_with('[') {
            if in_defaults && !inserted {
                out.push_str(&line);
                out.push('\n');
                inserted = true;
            }
            in_defaults = trimmed == "[Default Applications]";
        }
        if in_defaults && !inserted && trimmed.split_once('=').is_some_and(|(k, _)| k.trim() == mime) {
            out.push_str(&line);
            out.push('\n');
            inserted = true;
            continue;
        }
        out.push_str(existing);
        out.push('\n');
    }
    if !inserted {
        if !in_defaults {
            if !out.is_empty() && !out.ends_with("\n\n") {
                out.push('\n');
            }
            out.push_str("[Default Applications]\n");
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Launches an entry fully detached from the panel process, in a terminal when
/// it asks for one, and records it among the recent launches when the settings
/// keep them.
///
/// # Arguments
/// * `entry` - The entry to launch.
/// * `config` - The settings, for the terminal command and the recent list.
pub fn launch(entry: &Entry, config: &Config) {
    let mut argv = exec_argv(&entry.exec, entry.icon.as_deref(), &entry.name, &entry.path);
    if argv.is_empty() {
        return;
    }
    if entry.terminal
        && let Some(mut terminal) = find_terminal(config)
    {
        terminal.append(&mut argv);
        argv = terminal;
    }
    if config.track_recent {
        record_recent(&entry.id);
    }
    spawn_detached(&argv, entry.working_dir.as_deref());
}

/// The file the recent launches are kept in.
fn recent_file() -> PathBuf {
    config::state_dir().join("recent")
}

/// Desktop file IDs launched most recently, newest first.
pub fn recent() -> Vec<String> {
    std::fs::read_to_string(recent_file())
        .map(|text| {
            text.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Moves an ID to the front of the recent list and trims it.
fn record_recent(id: &str) {
    let mut ids = recent();
    ids.retain(|existing| existing != id);
    ids.insert(0, id.to_string());
    ids.truncate(RECENT_LIMIT);
    let dir = config::state_dir();
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(recent_file(), ids.join("\n") + "\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_mimeapps_default() {
        assert!(!default_set("", "inode/directory"));
        let text = add_default("", "inode/directory", FILES_ID);
        assert_eq!(
            text,
            "[Default Applications]\ninode/directory=selkies-desktop-files.desktop;\n"
        );
        assert!(default_set(&text, "inode/directory"));
        let text = add_default(
            "[Added Associations]\nimage/png=x.desktop;\n\n[Default Applications]\nimage/png=x.desktop;\n",
            "inode/directory",
            FILES_ID,
        );
        assert!(text.contains("image/png=x.desktop;\ninode/directory=selkies-desktop-files.desktop;\n"));
        let kept = add_default(
            "[Default Applications]\ninode/directory=\n",
            "inode/directory",
            FILES_ID,
        );
        assert!(default_set(&kept, "inode/directory"));
        assert_eq!(kept.matches("inode/directory").count(), 1);
    }
}
