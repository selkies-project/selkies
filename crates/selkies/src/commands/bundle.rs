/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The release bundle's payload, found beside the executable: what a session runs on a host
//! that has none of it.
//!
//! The bundle (`infra/bundle`) is one directory tree, unpacked anywhere:
//!
//! - `bin/`: `selkies` and `selkies-desktop`
//! - `libexec/selkies/`: the X servers (XLibre's Xvfb, Xwayland), `xkbcomp`, `pulseaudio`, and
//!   `dbus-daemon`
//! - `lib/selkies/`: the libraries those link that the host need not have, each found through
//!   its own `$ORIGIN` run path; `pulse/modules/` and `preload/`, the interposers the session's
//!   applications preload
//! - `share/selkies/`: the xkb data and the buses' configuration
//!
//! The launcher takes each program from the bundle where the bundle has it and from the host
//! otherwise ([`Bundle::program`]), passing the bundled one the paths its compiled-in defaults
//! do not reach. It names each by its full path rather than putting `libexec` on the session's
//! `PATH`, so the desktop's applications keep the host's programs; only `bin/` joins the
//! `PATH`, for the shell the server starts by name, and Xwayland, which the compositor starts
//! by name, is named to Selkies alone ([`Bundle::selkies_env`]). The server's keymaps read the
//! host's xkb data where it has some and the bundle's where it has none, and the interposers
//! default to the bundle's where the environment names none.

use std::path::{Path, PathBuf};

use super::session::{APPLICATION_PRELOADS, Env};

/// The host's xkb data, which the server's xkbcommon reads when it is there.
const HOST_XKB: &str = "/usr/share/X11/xkb";

/// The payload of a release bundle, rooted where the executable's `bin/` sits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundle {
    root: PathBuf,
}

impl Bundle {
    /// The bundle the running executable belongs to, if it runs from one.
    pub fn locate() -> Option<Bundle> {
        let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
        Self::at(exe.parent()?.parent()?)
    }

    /// The bundle rooted at `root`, if `root` holds one.
    pub fn at(root: &Path) -> Option<Bundle> {
        root.join("libexec/selkies").is_dir().then(|| Bundle {
            root: root.to_path_buf(),
        })
    }

    /// The directory of the bundle's servers and tools.
    pub fn libexec(&self) -> PathBuf {
        self.root.join("libexec/selkies")
    }

    /// The bundle's `name` where it carries one.
    pub fn program(&self, name: &str) -> Option<PathBuf> {
        let path = self.libexec().join(name);
        path.is_file().then_some(path)
    }

    /// The xkb data the bundle carries.
    pub fn xkb(&self) -> PathBuf {
        self.root.join("share/selkies/xkb")
    }

    /// Where the bundled PulseAudio's modules are.
    pub fn pulse_modules(&self) -> PathBuf {
        self.root.join("lib/selkies/pulse/modules")
    }

    /// The configuration of the bundled `dbus-daemon` for a bus of `kind`, `session` or
    /// `system`.
    pub fn dbus_config(&self, kind: &str) -> PathBuf {
        self.root.join(format!("share/selkies/dbus-1/{kind}.conf"))
    }

    /// The interposer a variable of [`APPLICATION_PRELOADS`] names, as the bundle carries it.
    fn preload(&self, var: &str) -> Option<PathBuf> {
        let name = match var {
            "SELKIES_INTERPOSER" => "selkies_input_interposer.so",
            "FAKE_UDEV_LIB" => "libudev.so.1.0.0-fake",
            "SELKIES_WEBCAM_INTERPOSER" => "selkies_v4l2_interposer.so",
            _ => return None,
        };
        let path = self.root.join("lib/selkies/preload").join(name);
        path.is_file().then_some(path)
    }

    /// What the session's environment takes from the bundle: `bin/` ahead of the `PATH`, the
    /// bundle's xkb data where the host has none (`host_xkb`), and its interposers where the
    /// environment names none.
    pub fn session_env(&self, env: &mut Env, host_xkb: &Path) {
        let bin = self.root.join("bin").to_string_lossy().into_owned();
        let path = env.get("PATH").cloned().unwrap_or_default();
        if !path.split(':').any(|p| p == bin) {
            let joined = if path.is_empty() {
                bin
            } else {
                format!("{bin}:{path}")
            };
            env.insert("PATH".to_string(), joined);
        }
        if !host_xkb.join("rules/evdev").is_file() && !env.contains_key("XKB_CONFIG_ROOT") {
            env.insert(
                "XKB_CONFIG_ROOT".to_string(),
                self.xkb().to_string_lossy().into_owned(),
            );
        }
        for var in APPLICATION_PRELOADS {
            if env.get(var).is_none_or(|v| v.is_empty())
                && let Some(path) = self.preload(var)
            {
                env.insert(var.to_string(), path.to_string_lossy().into_owned());
            }
        }
    }

    /// What Selkies alone takes from the bundle on top of the session's environment: the
    /// Xwayland its compositor starts.
    pub fn selkies_env(&self, env: &mut Env) {
        if let Some(xwayland) = self.program("Xwayland") {
            env.insert(
                pixelflux::XWAYLAND_VAR.to_string(),
                xwayland.to_string_lossy().into_owned(),
            );
        }
    }
}

/// The host's xkb data directory, which [`Bundle::session_env`] is given.
pub fn host_xkb() -> &'static Path {
    Path::new(HOST_XKB)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> PathBuf {
        let root = crate::http::testing::tempfile_dir("selkies-bundle-");
        for dir in [
            "bin",
            "libexec/selkies",
            "lib/selkies/preload",
            "share/selkies/xkb/rules",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for file in [
            "libexec/selkies/Xwayland",
            "libexec/selkies/pulseaudio",
            "lib/selkies/preload/selkies_input_interposer.so",
            "share/selkies/xkb/rules/evdev",
        ] {
            std::fs::write(root.join(file), "").unwrap();
        }
        root
    }

    #[test]
    fn a_tree_without_libexec_is_no_bundle() {
        let root = crate::http::testing::tempfile_dir("selkies-not-bundle-");
        assert_eq!(Bundle::at(&root), None);
        let bundle = Bundle::at(&tree()).expect("a bundle");
        assert!(bundle.program("pulseaudio").is_some());
        assert_eq!(
            bundle.program("dbus-daemon"),
            None,
            "the host's is taken instead"
        );
    }

    #[test]
    fn the_session_takes_bin_the_bundles_xkb_where_the_host_has_none_and_unnamed_interposers() {
        let root = tree();
        let bundle = Bundle::at(&root).unwrap();
        let mut env = Env::from([
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("FAKE_UDEV_LIB".to_string(), "/opt/lib/fake".to_string()),
        ]);
        bundle.session_env(&mut env, Path::new("/nonexistent/xkb"));
        assert_eq!(env["PATH"], format!("{}/bin:/usr/bin", root.display()));
        assert_eq!(env["XKB_CONFIG_ROOT"], bundle.xkb().to_string_lossy());
        assert_eq!(
            env["SELKIES_INTERPOSER"],
            root.join("lib/selkies/preload/selkies_input_interposer.so")
                .to_string_lossy()
        );
        assert_eq!(
            env["FAKE_UDEV_LIB"], "/opt/lib/fake",
            "a named interposer is kept"
        );
        assert!(
            !env.contains_key("SELKIES_WEBCAM_INTERPOSER"),
            "one the bundle lacks is left unset"
        );
        let mut again = env.clone();
        bundle.session_env(&mut again, Path::new("/nonexistent/xkb"));
        assert_eq!(again["PATH"], env["PATH"], "bin joins the PATH once");

        let mut host = Env::new();
        bundle.session_env(&mut host, &root.join("share/selkies/xkb"));
        assert!(
            !host.contains_key("XKB_CONFIG_ROOT"),
            "the host's own xkb data is read where it has some"
        );
        let mut selkies = Env::new();
        bundle.selkies_env(&mut selkies);
        assert_eq!(
            selkies[pixelflux::XWAYLAND_VAR],
            root.join("libexec/selkies/Xwayland").to_string_lossy()
        );
    }
}
