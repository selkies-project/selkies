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
//! backend, and serve it over HTTPS. So a bare `selkies` re-executes itself as `selkies session`
//! with [`defaults`] added to its environment: the backend, the shell, HTTPS on the port, which
//! the server gives a self-signed certificate it keeps under the state directory, and the login
//! the containers document, `ubuntu` with the placeholder password `mypasswd`. The session runs
//! as the user who started it, on the loopback addresses alone, so choosing a password of their
//! own is theirs, as it is for a container's operator. Each default is the environment's own
//! `SELKIES_` setting, set only where the environment leaves it unset, so a variable the user
//! exports still wins and the server's settings parser stays the one place they are read.
//!
//! Any argument is a request for the server alone (attached to the display and sound server the
//! environment names) or one of its commands; so is a bare `selkies` started inside a
//! session, which is how a session starts its server ([`super::session::SESSION_ID_VAR`]).

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use crate::settings::Settings;

use super::session::SESSION_ID_VAR;

/// Whether `args` (the command line after the program name) asks for the standalone desktop.
pub fn wanted(args: &[String], get: impl Fn(&str) -> Option<String>) -> bool {
    args.is_empty() && get(SESSION_ID_VAR).is_none()
}

/// The placeholder login a container documents, used where the environment names none.
pub const PLACEHOLDER_USER: &str = "ubuntu";

/// The placeholder password a container documents, used where the environment names none.
pub const PLACEHOLDER_PASSWORD: &str = "mypasswd";

/// The variables the standalone session adds to an environment `get` reads, each only where
/// that environment sets neither it nor a name the same setting is read under.
///
/// The certificate pair is named in the user's state directory, where the server writes a
/// self-signed one the first time and reuses it after, rather than left at the server's
/// default: that is the pair Debian's `ssl-cert` installs, whose key is root's and its group's
/// alone, so on a host that has it the server would find a certificate it cannot use and refuse
/// to start.
pub fn defaults(get: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
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
    let state = get("XDG_STATE_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(get("HOME").unwrap_or_default()).join(".local/state"));
    let tls = state.join("selkies");
    default(
        &["SELKIES_HTTPS_CERT"],
        &tls.join("selkies.pem").to_string_lossy(),
    );
    default(
        &["SELKIES_HTTPS_KEY"],
        &tls.join("selkies.key").to_string_lossy(),
    );
    default(
        &["SELKIES_BASIC_AUTH_USER", "CUSTOM_USER"],
        PLACEHOLDER_USER,
    );
    default(
        &["SELKIES_BASIC_AUTH_PASSWORD", "PASSWORD", "PASSWD"],
        PLACEHOLDER_PASSWORD,
    );
    env
}

/// The line that tells the user where their desktop is and how to sign in, from the settings
/// the session's server will resolve. It names the password only where it is the documented
/// placeholder, since the log is what a user pastes into a report.
fn banner(settings: &Settings) -> String {
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
    } else if settings.basic_auth_password() == PLACEHOLDER_PASSWORD {
        format!(
            "signing in as {} with the password {PLACEHOLDER_PASSWORD} (set SELKIES_BASIC_AUTH_PASSWORD \
             for your own)",
            settings.basic_auth_user()
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
    let env = defaults(get);
    let resolved = Settings::parse(&[], &|name| {
        env.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .or_else(|| get(name))
    });
    eprintln!("{}", banner(&resolved));
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
    fn the_defaults_are_wayland_the_shell_https_and_the_placeholder_login() {
        let none = env(&[]);
        let added: HashMap<String, String> =
            defaults(|k| none.get(k).cloned()).into_iter().collect();
        assert_eq!(added["SELKIES_WAYLAND"], "true");
        assert_eq!(added["SELKIES_DESKTOP"], "true");
        assert_eq!(added["SELKIES_ENABLE_HTTPS"], "true");
        assert_eq!(added["SELKIES_BASIC_AUTH_USER"], "ubuntu");
        assert_eq!(added["SELKIES_BASIC_AUTH_PASSWORD"], "mypasswd");
    }

    #[test]
    fn the_certificate_is_the_users_own_in_the_state_directory() {
        let home = env(&[("HOME", "/home/u")]);
        let added: HashMap<String, String> =
            defaults(|k| home.get(k).cloned()).into_iter().collect();
        assert_eq!(
            added["SELKIES_HTTPS_CERT"],
            "/home/u/.local/state/selkies/selkies.pem"
        );
        assert_eq!(
            added["SELKIES_HTTPS_KEY"],
            "/home/u/.local/state/selkies/selkies.key"
        );
        let state = env(&[("HOME", "/home/u"), ("XDG_STATE_HOME", "/s")]);
        let added: HashMap<String, String> =
            defaults(|k| state.get(k).cloned()).into_iter().collect();
        assert_eq!(added["SELKIES_HTTPS_CERT"], "/s/selkies/selkies.pem");
    }

    #[test]
    fn a_variable_the_user_sets_wins_over_its_default() {
        let user = env(&[
            ("PIXELFLUX_WAYLAND", "false"),
            ("PASSWORD", "mine"),
            ("CUSTOM_USER", "me"),
            ("SELKIES_ENABLE_HTTPS", "false"),
        ]);
        let added: HashMap<String, String> =
            defaults(|k| user.get(k).cloned()).into_iter().collect();
        assert!(!added.contains_key("SELKIES_WAYLAND"));
        assert!(!added.contains_key("SELKIES_ENABLE_HTTPS"));
        assert!(!added.contains_key("SELKIES_BASIC_AUTH_USER"));
        assert!(!added.contains_key("SELKIES_BASIC_AUTH_PASSWORD"));
        assert_eq!(added["SELKIES_DESKTOP"], "true");
    }
}
