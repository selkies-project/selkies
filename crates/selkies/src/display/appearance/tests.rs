/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The color scheme's ladder: which theme a scheme asks for, which buses a session's
//! applications reach, and the stores a real bus, dconf, and xsettingsd stand-in end up with.

use std::os::unix::net::UnixListener;

use super::*;
use crate::session_screens::testing::{Owned, Scratch, isolated, named_process, run_isolated};

#[test]
fn a_theme_moves_only_to_an_installed_counterpart() {
    let installed =
        |name: &str| ["Adwaita", "Adwaita-dark", "Breeze", "Breeze-Dark", "Yaru"].contains(&name);
    assert_eq!(
        theme_for(ColorScheme::Dark, "Adwaita", &installed).as_deref(),
        Some("Adwaita-dark")
    );
    assert_eq!(
        theme_for(ColorScheme::Dark, "Breeze", &installed).as_deref(),
        Some("Breeze-Dark"),
        "a theme's dark variant is found under either spelling"
    );
    assert_eq!(
        theme_for(ColorScheme::Light, "Adwaita-dark", &installed).as_deref(),
        Some("Adwaita")
    );
    assert_eq!(
        theme_for(ColorScheme::Light, "Breeze-Dark", &installed).as_deref(),
        Some("Breeze")
    );
    assert_eq!(
        theme_for(ColorScheme::Dark, "Adwaita-dark", &installed),
        None,
        "a dark theme already is what dark asks for"
    );
    assert_eq!(
        theme_for(ColorScheme::Light, "Adwaita", &installed),
        None,
        "and a light one what light asks for"
    );
    assert_eq!(
        theme_for(ColorScheme::Dark, "Yaru", &installed),
        None,
        "a theme without an installed counterpart is left as it is"
    );
    assert_eq!(
        theme_for(ColorScheme::Dark, "Missing", &installed).as_deref(),
        Some("Adwaita-dark"),
        "a theme that is not installed is GTK's default, whose dark variant is"
    );
    assert_eq!(
        theme_for(ColorScheme::Light, "Missing", &installed),
        None,
        "and that default already is light"
    );
}

#[test]
fn a_theme_is_installed_when_gtk3_can_load_it() {
    let scratch = Scratch::new("appearance-themes");
    let dirs = vec![scratch.path("themes")];
    std::fs::create_dir_all(scratch.path("themes/Adwaita-dark/gtk-3.0")).expect("theme");
    std::fs::create_dir_all(scratch.path("themes/Gtk2Only/gtk-2.0")).expect("theme");
    assert!(
        gtk3_theme_installed("Adwaita", &[]),
        "GTK 3 builds Adwaita in"
    );
    assert!(gtk3_theme_installed("Adwaita-dark", &dirs));
    assert!(
        !gtk3_theme_installed("Gtk2Only", &dirs),
        "a theme with no gtk-3.0 directory is not one GTK 3 loads"
    );
    assert!(!gtk3_theme_installed("Missing-dark", &dirs));
    assert!(
        !gtk3_theme_installed("../themes/Adwaita-dark", &dirs),
        "a name is never a path"
    );
}

#[test]
fn the_served_theme_is_the_xsettings_one_then_gtks_own() {
    let ini = "[Settings]\ngtk-icon-theme-name = breeze\ngtk-theme-name = Breeze\n".to_string();
    assert_eq!(
        served_gtk3_theme(
            "Xft/DPI 98304\nNet/ThemeName \"Adwaita-dark\"\n",
            std::slice::from_ref(&ini)
        ),
        "Adwaita-dark"
    );
    assert_eq!(served_gtk3_theme("Xft/DPI 98304\n", &[ini]), "Breeze");
    assert_eq!(served_gtk3_theme("", &[]), "Adwaita");
}

#[test]
fn gsettings_output_reads_as_its_value() {
    assert_eq!(gsettings_string("'prefer-dark'\n"), "prefer-dark");
    assert_eq!(gsettings_string("'Adwaita'"), "Adwaita");
    assert!(memory_backend(
        "GLib-GIO-Message: 12:00:00.000: Using the 'memory' GSettings backend.  Your settings will not be saved or shared with other applications.\n"
    ));
    assert!(!memory_backend(""));
}

#[test]
fn a_bus_is_one_bus_whatever_guid_its_address_carries() {
    assert!(same_bus(
        "unix:path=/run/user/1000/bus",
        "unix:path=/run/user/1000/bus,guid=fcb7d126ab00070553405e616af2a6b0"
    ));
    assert!(!same_bus(
        "unix:path=/run/user/1000/bus",
        "unix:path=/tmp/dbus-55kRcIFc9g,guid=a305bf63ee53e944b28b44eb6af2a6af"
    ));
}

#[test]
fn a_setting_value_names_a_scheme_or_none() {
    assert_eq!(ColorScheme::from_setting("dark"), Some(ColorScheme::Dark));
    assert_eq!(ColorScheme::from_setting("light"), Some(ColorScheme::Light));
    assert_eq!(ColorScheme::from_setting("off"), None);
    assert_eq!(ColorScheme::from_setting("Dark"), None);
}

#[test]
#[ignore = "run by the_buses_are_those_of_this_sessions_processes, in a process of its own"]
fn the_buses_are_found_in_their_process() {
    if !isolated() {
        return;
    }
    let scratch = PathBuf::from(std::env::var("APPEARANCE_SCRATCH").expect("scratch"));
    let runtime = std::env::var("XDG_RUNTIME_DIR").expect("runtime dir");
    let stand_ins = scratch.join("stand-ins");
    std::fs::create_dir_all(&stand_ins).expect("stand-ins");
    let listen = |name: &str| {
        let path = scratch.join(name);
        (
            UnixListener::bind(&path).expect("a socket"),
            format!("unix:path={}", path.display()),
        )
    };
    let (_desktop, desktop_bus) = listen("desktop-bus");
    let (_other, other_bus) = listen("other-bus");
    let (dead, dead_bus) = listen("dead-bus");
    drop(dead);
    let _runtime_bus =
        UnixListener::bind(Path::new(&runtime).join("bus")).expect("the runtime bus");
    let mut children = vec![
        named_process(
            &stand_ins,
            "selkies-desktop",
            &[
                ("DISPLAY", ":77"),
                ("DBUS_SESSION_BUS_ADDRESS", &desktop_bus),
            ],
        ),
        named_process(
            &stand_ins,
            "firefox",
            &[
                ("DISPLAY", ":77"),
                ("DBUS_SESSION_BUS_ADDRESS", &desktop_bus),
            ],
        ),
        named_process(
            &stand_ins,
            "xterm",
            &[("DISPLAY", ":77"), ("DBUS_SESSION_BUS_ADDRESS", &dead_bus)],
        ),
        named_process(
            &stand_ins,
            "portal",
            &[
                ("DISPLAY", ":77"),
                (
                    "DBUS_SESSION_BUS_ADDRESS",
                    &format!("unix:path={runtime}/bus,guid=0123456789abcdef0123456789abcdef"),
                ),
            ],
        ),
        named_process(
            &stand_ins,
            "elsewhere",
            &[("DISPLAY", ":78"), ("DBUS_SESSION_BUS_ADDRESS", &other_bus)],
        ),
    ];
    std::thread::sleep(std::time::Duration::from_millis(200));
    let session = AppSession {
        x11_display: Some(":77".into()),
        wayland_display: None,
        kind: "x11",
    };
    let found: Vec<String> = session_buses(&session)
        .into_iter()
        .map(|b| b.address)
        .collect();
    assert_eq!(
        found,
        [
            desktop_bus,
            format!("unix:path={runtime}/bus,guid=0123456789abcdef0123456789abcdef")
        ],
        "each live bus of this session's processes once, the runtime dir's own among them"
    );
    for child in &mut children {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[test]
fn the_buses_are_those_of_this_sessions_processes() {
    let scratch = Scratch::new("appearance-buses");
    std::fs::create_dir(scratch.path("run")).expect("runtime dir");
    run_isolated(
        "display::appearance::tests::the_buses_are_found_in_their_process",
        &[
            ("APPEARANCE_SCRATCH", &scratch.text()),
            (
                "XDG_RUNTIME_DIR",
                &scratch.path("run").display().to_string(),
            ),
        ],
    );
}

/// The GNOME interface keys this ladder writes, as a schema of their own: the suites need no
/// desktop's schemas installed.
const SCHEMA: &str = r#"<schemalist>
  <enum id="org.gnome.desktop.GDesktopColorScheme">
    <value nick="default" value="0"/>
    <value nick="prefer-dark" value="1"/>
    <value nick="prefer-light" value="2"/>
  </enum>
  <schema id="org.gnome.desktop.interface" path="/org/gnome/desktop/interface/">
    <key name="color-scheme" enum="org.gnome.desktop.GDesktopColorScheme"><default>'default'</default></key>
    <key name="gtk-theme" type="s"><default>'Adwaita'</default></key>
  </schema>
</schemalist>
"#;

/// The value of a GNOME interface key through the runtime dir's bus.
fn read_key(key: &str) -> String {
    let out = std::process::Command::new("gsettings")
        .args(["get", GNOME_INTERFACE, key])
        .output()
        .expect("gsettings runs");
    gsettings_string(&String::from_utf8_lossy(&out.stdout))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "run by a_scheme_reaches_dconf_and_xsettings, in a process of its own"]
async fn a_scheme_reaches_the_stores_in_its_process() {
    if !isolated() {
        return;
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR").expect("runtime dir");
    let display = std::env::var("DISPLAY").expect("a display name");
    let stand_ins = home().join("stand-ins");
    std::fs::create_dir_all(&stand_ins).expect("stand-ins");
    let bus = format!("unix:path={runtime}/bus");
    let _daemon = Owned::spawn(std::process::Command::new("dbus-daemon").args([
        "--session",
        "--nofork",
        "--nopidfile",
        &format!("--address={bus}"),
    ]));
    let started = std::time::Instant::now();
    while !dbus_address_live(&bus) {
        assert!(started.elapsed().as_secs() < 10, "the bus comes up");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::fs::write(home().join(".xsettingsd"), "Xft/DPI 147456\n").expect("xsettingsd");
    let mut manager = named_process(&stand_ins, "xsettingsd", &[("DISPLAY", &display)]);
    let session = AppSession {
        x11_display: Some(display.clone()),
        wayland_display: None,
        kind: "x11",
    };

    apply(ColorScheme::Dark, session.clone()).await;
    assert_eq!(
        read_key("color-scheme"),
        "prefer-dark",
        "dconf stores the portal's key"
    );
    assert_eq!(
        read_key("gtk-theme"),
        "Adwaita-dark",
        "and the GTK theme's installed dark variant"
    );
    let served = std::fs::read_to_string(home().join(".xsettingsd")).expect("xsettingsd");
    assert_eq!(
        (
            xsettings::entry(&served, "Net/ThemeName").as_deref(),
            xsettings::entry(&served, "Xft/DPI").as_deref()
        ),
        (Some("Adwaita-dark"), Some("147456")),
        "XSETTINGS serves the dark theme beside the density: {served}"
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        manager
            .try_wait()
            .expect("readable")
            .and_then(|s| s.signal()),
        Some(libc::SIGHUP),
        "the xsettingsd serving this display is told to reload"
    );

    let _ = manager.wait();
    manager = named_process(&stand_ins, "xsettingsd", &[("DISPLAY", &display)]);
    apply(ColorScheme::Light, session.clone()).await;
    assert_eq!(read_key("color-scheme"), "prefer-light");
    assert_eq!(
        read_key("gtk-theme"),
        "Adwaita",
        "light restores the theme it came from"
    );
    let served = std::fs::read_to_string(home().join(".xsettingsd")).expect("xsettingsd");
    assert_eq!(
        xsettings::entry(&served, "Net/ThemeName").as_deref(),
        Some("Adwaita"),
        "and XSETTINGS the light theme: {served}"
    );

    let set = std::process::Command::new("gsettings")
        .args(["set", GNOME_INTERFACE, "gtk-theme", "SelkiesTestTheme"])
        .env("DBUS_SESSION_BUS_ADDRESS", &bus)
        .status()
        .expect("gsettings runs");
    assert!(set.success(), "the user picks a theme of their own");
    apply(ColorScheme::Dark, session).await;
    assert_eq!(read_key("color-scheme"), "prefer-dark");
    assert_eq!(
        read_key("gtk-theme"),
        "SelkiesTestTheme",
        "a theme with no dark variant installed stays the user's"
    );
    let _ = manager.kill();
    let _ = manager.wait();
}

#[test]
#[ignore = "needs dbus-daemon, dconf, gsettings, and glib-compile-schemas"]
fn a_scheme_reaches_dconf_and_xsettings() {
    let home = Scratch::new("appearance-home");
    for dir in [
        "run",
        "schemas",
        "data/themes/Adwaita-dark/gtk-3.0",
        "data/themes/SelkiesTestTheme/gtk-3.0",
        "config/gtk-3.0",
    ] {
        std::fs::create_dir_all(home.path(dir)).expect("a directory");
    }
    // The user's own GTK settings, read before the host's /etc ones.
    std::fs::write(
        home.path("config/gtk-3.0/settings.ini"),
        "[Settings]\ngtk-theme-name = Adwaita\n",
    )
    .expect("settings.ini");
    std::fs::write(home.path("schemas/selkies-test.gschema.xml"), SCHEMA).expect("schema");
    assert!(
        std::process::Command::new("glib-compile-schemas")
            .arg(home.path("schemas"))
            .status()
            .expect("glib-compile-schemas runs")
            .success()
    );
    let text = |name: &str| home.path(name).display().to_string();
    run_isolated(
        "display::appearance::tests::a_scheme_reaches_the_stores_in_its_process",
        &[
            ("HOME", &home.text()),
            ("XDG_RUNTIME_DIR", &text("run")),
            ("XDG_CONFIG_HOME", &text("config")),
            ("XDG_DATA_HOME", &text("data")),
            (
                "XDG_DATA_DIRS",
                &format!("{}:/usr/local/share:/usr/share", text("data")),
            ),
            ("GSETTINGS_SCHEMA_DIR", &text("schemas")),
            ("GSETTINGS_BACKEND", "dconf"),
            ("DISPLAY", ":79"),
            ("DBUS_SESSION_BUS_ADDRESS", ""),
        ],
    );
}
