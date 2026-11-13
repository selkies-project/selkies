/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `selkies` with no arguments: a desktop of its own on this machine, reached in a browser at
//! `https://localhost:8080`.
//!
//! What a `pip install selkies`, an unpacked bundle, or the AppImage should do when run as it
//! comes is what a container does: bring up an isolated session (`selkies session`, its own
//! runtime directory, buses, sound server, and display, the user's home its only share) on
//! Selkies' own desktop, the selkies-desktop shell over the window manager on the Wayland
//! backend, and serve it over HTTPS behind a login. So a bare `selkies` re-executes itself as
//! `selkies session` with [`defaults`] added to its environment: the backend, the shell, HTTPS
//! on the port, which the server gives a self-signed certificate it keeps under the state
//! directory, and a password generated once and kept in the user's configuration directory
//! readable by them alone ([`password`]). A login stays on by default even though the server
//! listens on the loopback addresses alone: every account on the machine reaches those, and the
//! session is the user's whole home directory. Each default is the environment's own `SELKIES_`
//! setting, set only where the environment leaves it unset, so a variable the user exports still
//! wins and the server's settings parser stays the one place they are read.
//!
//! Any argument is a request for the server alone (attached to the display and sound server the
//! environment names) or one of its commands; so is a bare `selkies` started inside a
//! session, which is how a session starts its server ([`super::session::SESSION_ID_VAR`]).

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::settings::Settings;

use super::session::SESSION_ID_VAR;

/// The characters of a generated password: letters and digits, so it survives being typed.
const PASSWORD_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";

/// The length of a generated password, about 93 bits from [`PASSWORD_ALPHABET`].
const PASSWORD_LENGTH: usize = 16;

/// Whether `args` (the command line after the program name) asks for the standalone desktop.
pub fn wanted(args: &[String], get: impl Fn(&str) -> Option<String>) -> bool {
    args.is_empty() && get(SESSION_ID_VAR).is_none()
}

/// The variables the standalone session adds to an environment `get` reads, each only where
/// that environment sets neither it nor the name it was read under before `SELKIES_`.
/// `password` is the login's, used only where the environment names none of its own and keeps
/// the login on.
pub fn defaults(get: impl Fn(&str) -> Option<String>, password: &str) -> Vec<(String, String)> {
    let set = |names: &[&str]| names.iter().any(|name| get(name).is_some());
    let mut env = Vec::new();
    let mut default = |names: &[&str], value: &str| {
        if !set(names) {
            env.push((names[0].to_string(), value.to_string()));
        }
    };
    default(&["SELKIES_WAYLAND", "PIXELFLUX_WAYLAND"], "true");
    default(&["SELKIES_DESKTOP"], "true");
    default(&["SELKIES_ENABLE_HTTPS"], "true");
    let login_off = get("SELKIES_ENABLE_BASIC_AUTH")
        .is_some_and(|v| !crate::settings::parse_bool(Some(&v), true));
    if !login_off {
        default(
            &["SELKIES_BASIC_AUTH_PASSWORD", "PASSWORD", "PASSWD"],
            password,
        );
    }
    env
}

/// The login password kept at `path`, generated there readable by its owner alone the first
/// time; a user edits the file to choose their own.
///
/// # Errors
/// The file cannot be read or written, or it is empty.
pub fn password(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let password = text.trim().to_string();
            if password.is_empty() {
                return Err(io::Error::other(format!("{} is empty", path.display())));
            }
            Ok(password)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)?;
            }
            let password = generate_password();
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?;
            writeln!(file, "{password}")?;
            Ok(password)
        }
        Err(err) => Err(err),
    }
}

/// A random password of [`PASSWORD_LENGTH`] characters from [`PASSWORD_ALPHABET`], each drawn
/// without the bias a plain modulo would give.
fn generate_password() -> String {
    let limit = (256 / PASSWORD_ALPHABET.len() * PASSWORD_ALPHABET.len()) as u8;
    let mut password = String::with_capacity(PASSWORD_LENGTH);
    while password.len() < PASSWORD_LENGTH {
        let byte: u8 = rand::random();
        if byte < limit {
            password.push(PASSWORD_ALPHABET[usize::from(byte) % PASSWORD_ALPHABET.len()] as char);
        }
    }
    password
}

/// Where the login password is kept: `$XDG_CONFIG_HOME/selkies/password`, else under
/// `~/.config`.
fn password_path(get: impl Fn(&str) -> Option<String>) -> PathBuf {
    let config = get("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(get("HOME").unwrap_or_default()).join(".config"));
    config.join("selkies/password")
}

/// The line that tells the user where their desktop is and how to sign in, from the settings
/// the session's server will resolve. It names the password's file rather than the password,
/// since the log is what a user pastes into a report.
fn banner(settings: &Settings, password_file: Option<&Path>) -> String {
    let scheme = if settings.enable_https().on {
        "https"
    } else {
        "http"
    };
    let address = format!("{scheme}://localhost:{}", settings.port());
    let reach = if settings.public().on || settings.addr() != "localhost" {
        format!("{address}, and on this machine's other addresses it listens on")
    } else {
        address
    };
    let login = if !settings.enable_basic_auth().on {
        "with no login".to_string()
    } else if let Some(file) = password_file {
        format!(
            "signing in as {} with the password in {}",
            settings.basic_auth_user(),
            file.display()
        )
    } else {
        format!(
            "signing in as {} with the password the environment sets",
            settings.basic_auth_user()
        )
    };
    format!(
        "Selkies: your desktop is at {reach}, {login}. `selkies session --help` and `selkies --help` \
         describe the other ways to run it."
    )
}

/// Run the standalone desktop: print where it is and re-execute this program as
/// `selkies session` with [`defaults`]. Returns only on failure, with the exit status.
pub fn main() -> i32 {
    let get = |name: &str| std::env::var(name).ok();
    let file = password_path(get);
    let generated = match password(&file) {
        Ok(password) => password,
        Err(err) => {
            eprintln!(
                "selkies: cannot keep a login password at {}: {err}",
                file.display()
            );
            return 1;
        }
    };
    let env = defaults(get, &generated);
    let ours = env
        .iter()
        .any(|(name, _)| name == "SELKIES_BASIC_AUTH_PASSWORD");
    let resolved = Settings::parse(&[], &|name| {
        env.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .or_else(|| get(name))
    });
    eprintln!("{}", banner(&resolved, ours.then_some(file.as_path())));
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("selkies: cannot find the selkies executable: {err}");
            return 1;
        }
    };
    let err = Command::new(exe).arg("session").envs(env).exec();
    eprintln!("selkies: cannot start the session: {err}");
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn only_a_bare_command_outside_a_session_is_the_standalone_desktop() {
        let none = env(&[]);
        assert!(wanted(&[], |k| none.get(k).cloned()));
        assert!(!wanted(&["--port=9000".to_string()], |k| none
            .get(k)
            .cloned()));
        let inside = env(&[(SESSION_ID_VAR, "1")]);
        assert!(!wanted(&[], |k| inside.get(k).cloned()));
    }

    #[test]
    fn the_defaults_are_wayland_the_shell_https_and_the_kept_password() {
        let none = env(&[]);
        let added: HashMap<String, String> = defaults(|k| none.get(k).cloned(), "secret")
            .into_iter()
            .collect();
        assert_eq!(added["SELKIES_WAYLAND"], "true");
        assert_eq!(added["SELKIES_DESKTOP"], "true");
        assert_eq!(added["SELKIES_ENABLE_HTTPS"], "true");
        assert_eq!(added["SELKIES_BASIC_AUTH_PASSWORD"], "secret");
    }

    #[test]
    fn a_variable_the_user_sets_wins_over_its_default() {
        let user = env(&[
            ("PIXELFLUX_WAYLAND", "false"),
            ("PASSWORD", "mine"),
            ("SELKIES_ENABLE_HTTPS", "false"),
        ]);
        let added: HashMap<String, String> = defaults(|k| user.get(k).cloned(), "secret")
            .into_iter()
            .collect();
        assert!(!added.contains_key("SELKIES_WAYLAND"));
        assert!(!added.contains_key("SELKIES_ENABLE_HTTPS"));
        assert!(!added.contains_key("SELKIES_BASIC_AUTH_PASSWORD"));
        assert_eq!(added["SELKIES_DESKTOP"], "true");
    }

    #[test]
    fn a_login_turned_off_takes_no_password() {
        let off = env(&[("SELKIES_ENABLE_BASIC_AUTH", "false")]);
        let added = defaults(|k| off.get(k).cloned(), "secret");
        assert!(
            !added
                .iter()
                .any(|(name, _)| name == "SELKIES_BASIC_AUTH_PASSWORD")
        );
    }

    #[test]
    fn the_password_is_generated_once_readable_by_its_owner_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("selkies/password");
        let first = password(&path).unwrap();
        assert_eq!(first.len(), PASSWORD_LENGTH);
        assert!(first.bytes().all(|b| PASSWORD_ALPHABET.contains(&b)));
        assert_eq!(password(&path).unwrap(), first);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn a_password_the_user_wrote_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("password");
        fs::write(&path, "chosen\n").unwrap();
        assert_eq!(password(&path).unwrap(), "chosen");
    }
}
