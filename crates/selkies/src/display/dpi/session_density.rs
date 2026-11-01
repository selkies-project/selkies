/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What the density ladder reads about the session, and what it remembers
//! (`test_session_density.py`): a desktop that keeps its settings on a session bus is reached
//! through the environment its own process runs with, read out of `/proc` rather than inherited,
//! and the density last applied is what a page joining an existing session is told the desktop
//! has. The readers run against real processes, stand-ins named as each daemon or session, and two
//! private X servers; what a process reads from its environment runs in a process of its own.

use super::*;
use crate::session_screens::testing::{
    Scratch, isolated, named_process, private_x_server, run_isolated, script,
};

/// `sleep` children on each display form, each with the marker a read looks for.
fn sleepers() -> Vec<(&'static str, std::process::Child)> {
    [":97", "localhost:98", ":98.0", "unix/:98"]
        .into_iter()
        .map(|display| {
            let child = std::process::Command::new("sleep")
                .arg("30")
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("SELKIES_PROBE", "density")
                .env("DISPLAY", display)
                .spawn()
                .expect("sleep runs");
            (display, child)
        })
        .collect()
}

#[test]
#[ignore = "run by processes_are_found_on_this_display_alone, in a process of its own"]
fn processes_are_found_in_their_process() {
    if !isolated() {
        return;
    }
    let mut children = sleepers();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let pid = |display: &str| {
        children
            .iter()
            .find(|(d, _)| *d == display)
            .map(|(_, c)| c.id() as i32)
            .expect("a sleeper")
    };
    let env = process_environ(pid(":98.0"));
    assert_eq!(
        env.get("SELKIES_PROBE").map(String::as_str),
        Some("density"),
        "a process's environment is read back from /proc: {env:?}"
    );
    let pids = pids_on_display("sleep");
    let mut found: Vec<&str> = children
        .iter()
        .filter(|(_, c)| pids.contains(&(c.id() as i32)))
        .map(|(d, _)| *d)
        .collect();
    found.sort_unstable();
    assert_eq!(
        found,
        [":98.0", "unix/:98"],
        "the processes running a binary are found on this display alone, however it is written"
    );
    assert!(
        pids_on_display("selkies-no-such-binary").is_empty(),
        "a binary nothing is running has no pids"
    );
    let gone = pid(":98.0");
    for (_, child) in &mut children {
        let _ = child.kill();
        let _ = child.wait();
    }
    assert!(
        process_environ(gone).is_empty(),
        "a process that is gone has no environment to read"
    );
}

#[test]
fn processes_are_found_on_this_display_alone() {
    run_isolated(
        "display::dpi::session_density::processes_are_found_in_their_process",
        &[("DISPLAY", "unix:98")],
    );
}

#[test]
fn startup_applies_only_what_differs() {
    for (current, configured, locked, apply, settle) in [
        (None, 96, false, false, 96),
        (Some(96), 96, false, false, 96),
        (Some(192), 96, false, true, 192),
        (Some(192), 96, true, true, 96),
        (None, 96, true, false, 96),
        (None, 192, true, true, 192),
        (Some(192), 192, true, true, 192),
        (Some(144), 192, true, true, 192),
    ] {
        assert_eq!(
            restore_plan(current, configured, locked),
            (settle, apply),
            "startup finds {current:?}, {configured} configured, locked {locked}"
        );
    }
}

fn in_home(name: &str) -> PathBuf {
    home().join(name)
}

#[test]
#[ignore = "run by a_restart_reads_the_density_the_home_persists, in a process of its own"]
fn a_restart_reads_in_its_process() {
    if !isolated() {
        return;
    }
    let declared = declared_desktop();
    if declared == "XFCE" {
        let channel = in_home(".config").join(XFCONF_XSETTINGS);
        std::fs::create_dir_all(channel.parent().expect("a parent")).expect("channel directory");
        let db = || Some("Xft.dpi: 120\n".to_string());
        assert_eq!(
            desktop_dpi_from(db()),
            Some(120),
            "an XFCE session that persisted no density reads the desktop as any other"
        );
        let property = |value: i32| {
            format!(
                "<channel name=\"xsettings\" version=\"1.0\">\n  <property name=\"Xft\" type=\"empty\">\n    <property name=\"DPI\" type=\"int\" value=\"{value}\"/>\n  </property>\n</channel>\n"
            )
        };
        std::fs::write(&channel, property(-1)).expect("channel");
        assert_eq!(
            desktop_dpi_from(db()),
            Some(120),
            "xfconf's unset density falls through"
        );
        std::fs::write(&channel, property(192)).expect("channel");
        assert_eq!(
            desktop_dpi_from(db()),
            Some(192),
            "the density xfconf persists is read over the database and the files"
        );
        return;
    }
    if declared == "Openbox" {
        let channel = in_home(".config").join(XFCONF_XSETTINGS);
        std::fs::create_dir_all(channel.parent().expect("a parent")).expect("channel directory");
        std::fs::write(&channel, "name=\"DPI\" type=\"int\" value=\"192\"").expect("channel");
        assert_eq!(
            desktop_dpi_from(Some("Xft.dpi: 120\n".to_string())),
            Some(120),
            "another session never reads xfconf's channel"
        );
        return;
    }
    assert_eq!(
        desktop_dpi_from_files(),
        None,
        "a fresh home names no density"
    );
    std::fs::write(in_home(".xsettingsd"), "Xft/Antialias 1\nXft/DPI 147456\n")
        .expect("xsettingsd");
    assert_eq!(
        desktop_dpi_from_files(),
        Some(144),
        "persisted xsettingsd resources name the density they serve"
    );
    std::fs::write(
        in_home(".Xresources"),
        "*background: black\nXft.dpi:   192\n",
    )
    .expect("Xresources");
    assert_eq!(
        desktop_dpi_from_files(),
        Some(192),
        "persisted Xresources are read over xsettingsd"
    );
}

#[test]
fn a_restart_reads_the_density_the_home_persists() {
    for desktop in ["", "XFCE", "Openbox"] {
        let home = Scratch::new("density-home");
        run_isolated(
            "display::dpi::session_density::a_restart_reads_in_its_process",
            &[
                ("HOME", &home.text()),
                (
                    "XDG_CONFIG_HOME",
                    &home.path(".config").display().to_string(),
                ),
                ("XDG_CURRENT_DESKTOP", desktop),
                ("DESKTOP_SESSION", ""),
            ],
        );
    }
}

/// The tool runs the stand-ins recorded since the last call, as `tool bus args`.
fn runs(written: &Path) -> Vec<String> {
    let lines = std::fs::read_to_string(written)
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default();
    let _ = std::fs::remove_file(written);
    lines
}

/// A stand-in named `binary`, a session of this home on display `on` with a bus named after it.
fn session(stand_ins: &Path, binary: &str, on: &str) -> std::process::Child {
    named_process(
        stand_ins,
        binary,
        &[
            ("HOME", &home().display().to_string()),
            ("DISPLAY", on),
            ("DBUS_SESSION_BUS_ADDRESS", &format!("unix:path={on}")),
        ],
    )
}

fn end(mut children: Vec<std::process::Child>) {
    for child in &mut children {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "run by the_ladder_reaches_only_this_display, in a process of its own"]
async fn the_ladder_reaches_in_its_process() {
    if !isolated() {
        return;
    }
    crate::settings::install(crate::settings::Settings::parse(&[], &|_| None));
    let display = std::env::var("DISPLAY").expect("a private server");
    let other = std::env::var("DENSITY_OTHER").expect("a second server");
    let stand_ins = home().join("stand-ins");
    std::fs::create_dir_all(&stand_ins).expect("stand-ins");
    assert_eq!(
        applied_dpi(),
        None,
        "no density is reported before one is applied"
    );

    std::fs::write(in_home(".Xresources"), "Xft.dpi:   192\n").expect("Xresources");
    assert_eq!(
        desktop_dpi().await,
        Some(192),
        "a resource database without Xft.dpi falls back to the files"
    );
    let merge = |text: &str| {
        use std::io::Write;
        let mut xrdb = std::process::Command::new("xrdb")
            .args(["-merge", "-"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("xrdb runs");
        xrdb.stdin
            .as_mut()
            .expect("piped")
            .write_all(text.as_bytes())
            .expect("xrdb reads");
        assert!(xrdb.wait().expect("xrdb ends").success());
    };
    merge("Xft.dpi: 120\n");
    assert_eq!(
        desktop_dpi().await,
        Some(120),
        "the server's resource database is read over the files"
    );

    // Stand-ins named xsettingsd, one serving this display and one another display from this same
    // home, which a SIGHUP ends.
    let mut here = named_process(&stand_ins, "xsettingsd", &[("DISPLAY", &display)]);
    let mut there = named_process(&stand_ins, "xsettingsd", &[("DISPLAY", &other)]);
    assert!(
        run_xrdb(144).await,
        "a density reaches the resource database"
    );
    assert_eq!(desktop_dpi().await, Some(144));
    let served = std::fs::read_to_string(in_home(".xsettingsd")).expect("xsettingsd");
    assert!(
        served.contains("Xft/DPI 147456"),
        "and the xsettingsd file: {served}"
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        here.try_wait().expect("readable").and_then(|s| s.signal()),
        Some(libc::SIGHUP),
        "the xsettingsd serving this display is told to reload"
    );
    assert_eq!(
        there.try_wait().expect("readable"),
        None,
        "and the other is not"
    );
    let _ = there.kill();
    let _ = there.wait();
    merge("Xft.dpi: 120\n");

    // Every LXQt application of this home watches its configuration from whichever display it is
    // on, so only the session on this display has its font retargeted, where that session reads it.
    let in_points = "[Qt]\nfont=\"Sans,11,-1,5,50,0,0,0,0,0\"\n";
    let home_conf = in_home(".config/lxqt/lxqt.conf");
    let session_config = in_home("session-config");
    let session_conf = session_config.join("lxqt/lxqt.conf");
    for conf in [&home_conf, &session_conf] {
        std::fs::create_dir_all(conf.parent().expect("a parent")).expect("configuration directory");
        std::fs::write(conf, in_points).expect("configuration");
    }
    let home_text = home().display().to_string();
    let mut sessions = vec![named_process(
        &stand_ins,
        "lxqt-session",
        &[("HOME", &home_text), ("DISPLAY", &other)],
    )];
    assert!(
        !run_lxqt_font(144).await
            && std::fs::read_to_string(&home_conf).expect("readable") == in_points,
        "a session of this home on another display keeps its font"
    );
    sessions.push(named_process(
        &stand_ins,
        "lxqt-session",
        &[
            ("HOME", &home_text),
            ("DISPLAY", &display),
            ("XDG_CONFIG_HOME", &session_config.display().to_string()),
        ],
    ));
    assert!(
        run_lxqt_font(144).await,
        "the session on this display has its font resolved"
    );
    let resolved = std::fs::read_to_string(&session_conf).expect("readable");
    assert!(
        resolved.contains("font=\"Sans,11,22,"),
        "where it reads it: {resolved}"
    );
    assert_eq!(
        std::fs::read_to_string(&home_conf).expect("readable"),
        in_points
    );
    end(sessions);

    // A desktop's settings stores are written through the bus of the session on this display,
    // never through the one this process inherited. The tools on PATH record the bus they got.
    let written = PathBuf::from(std::env::var("DENSITY_WRITTEN").expect("the record"));
    let mut desktops: Vec<std::process::Child> = ["xfce4-session", "mate-session", "gsd-xsettings"]
        .iter()
        .map(|binary| session(&stand_ins, binary, &other))
        .collect();
    let applied = (
        run_xfconf(144).await,
        run_mate_gsettings(144).await,
        set_cursor_size(48).await,
    );
    let recorded = runs(&written);
    assert!(
        applied == (false, false, true) && recorded.is_empty(),
        "sessions of this home on another display have no store written through any bus: {applied:?} {recorded:?}"
    );
    desktops.push(session(&stand_ins, "mate-session", &display));
    desktops.push(session(&stand_ins, "gsd-xsettings", &display));
    assert!(set_cursor_size(48).await);
    assert_eq!(
        runs(&written),
        [format!(
            "gsettings unix:path={display} set org.gnome.desktop.interface cursor-size 48"
        )],
        "a cursor size reaches the GNOME key through the bus of this display's session"
    );
    assert!(run_mate_gsettings(144).await);
    let recorded = runs(&written);
    assert!(
        recorded.len() == 2
            && recorded
                .iter()
                .all(|r| r.starts_with(&format!("gsettings unix:path={display} set org.mate."))),
        "a MATE density is written through the bus of this display's session: {recorded:?}"
    );
    desktops.push(session(&stand_ins, "xfce4-session", &display));
    assert!(run_xfconf(144).await);
    let recorded = runs(&written);
    assert!(
        recorded.len() == 2
            && recorded
                .iter()
                .all(|r| r.starts_with(&format!("xfconf-query unix:path={display} "))),
        "an XFCE density is written through the bus of this display's session: {recorded:?}"
    );
    assert!(set_cursor_size(48).await);
    assert_eq!(
        runs(&written),
        [format!(
            "xfconf-query unix:path={display} -c xsettings -p /Gtk/CursorThemeSize -s 48 --create -t int"
        )],
        "an XFCE cursor size is written through the bus of this display's session"
    );
    end(desktops);

    assert!(set_dpi(192).await);
    assert_eq!(
        applied_dpi(),
        Some(192),
        "the density last applied is what a joining page is told"
    );
}

#[test]
#[ignore = "needs Xvfb and xrdb"]
fn the_ladder_reaches_only_this_display() {
    let home = Scratch::new("density-home");
    let stub_bin = home.path("stub-bin");
    std::fs::create_dir_all(&stub_bin).expect("stub-bin");
    let written = home.path("written");
    for tool in ["xfconf-query", "gsettings"] {
        script(
            &stub_bin.join(tool),
            &format!(
                "echo \"{tool} $DBUS_SESSION_BUS_ADDRESS $*\" >> \"{}\"",
                written.display()
            ),
        );
    }
    let (_server, display) = private_x_server(320, 240);
    let (_other_server, other) = private_x_server(320, 240);
    let path = format!(
        "{}:{}",
        stub_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    run_isolated(
        "display::dpi::session_density::the_ladder_reaches_in_its_process",
        &[
            ("HOME", &home.text()),
            (
                "XDG_CONFIG_HOME",
                &home.path(".config").display().to_string(),
            ),
            ("XDG_CURRENT_DESKTOP", "Openbox"),
            ("DISPLAY", &display),
            ("PATH", &path),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=inherited"),
            ("DENSITY_OTHER", &other),
            ("DENSITY_WRITTEN", &written.display().to_string()),
        ],
    );
}
