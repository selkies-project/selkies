/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Per-desktop DPI and cursor-size application on X11.
//!
//! Only two desktops keep the density somewhere other than the X resource database, and
//! everything else -- named or not -- reads it from there: XFCE takes xfconf-query alone so
//! scaling is never applied twice, MATE takes gsettings plus xrdb for wider coverage, and
//! every other session gets `Xft.dpi` merged into the resource database and written to
//! `~/.Xresources` and `~/.xsettingsd`. The LXQt font repolish runs whichever branch was
//! taken: the session that owns the windows decides whether anything already drawn follows,
//! and it is the only one that can repolish them. On success the root pane's physical size
//! is stamped with the same density, because xdpyinfo/RandR consumers (Qt's fallback
//! included) read it and would otherwise render unscaled against the rest of the desktop.
//!
//! A density or cursor size reaches only the session on this display: the daemons told to
//! reload and the settings stores written through a session bus are found by the DISPLAY
//! their processes run with (`pids_on_display`, `session_env`), read out of
//! `/proc/<pid>/environ`, never through whatever bus Selkies inherited, since another
//! display's session may share the home and would apply the change live.
//!
//! On the Wayland backend a DPI is an output scale on the session compositor, never Xft
//! resources: XWayland runs in the compositor's logical space and is scaled with it, so
//! resources merged there would scale applications twice. `set_dpi` refuses there.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use log::{debug, error, info, warn};
use regex::Regex;

pub use super::x11::applied_dpi;
use super::x11::on_x11;
use super::xrandr::{run_tool, which};

/// Where xfconfd persists the channel `set_dpi` writes on XFCE.
const XFCONF_XSETTINGS: &str = "xfce4/xfconf/xfce-perchannel-xml/xsettings.xml";

/// The user's home directory as the environment names it.
fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Install `content` at `path` by writing a temporary file in the same directory and
/// renaming it over the target.
///
/// Nothing can observe a truncated or half-written file: a write that fails part-way (a full
/// filesystem, an I/O error, the process dying) leaves the existing target untouched. A
/// symlinked path is resolved first so a dotfile managed as a link into a dotfiles
/// repository stays a link, and an existing target's permission bits are carried onto the
/// replacement.
pub fn atomic_write_text(path: &Path, content: &str) -> std::io::Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".selkies-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        let mode = std::fs::metadata(&target)
            .map(|m| m.permissions().mode() & 0o7777)
            .unwrap_or(0o644);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&tmp, &target)
    };
    let result = write();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Persist `Xft.dpi` in the user's Xresources file, rewriting only that resource.
///
/// Every other line the user keeps there (colors, terminal settings, keyboard resources) is
/// preserved, so a DPI change never costs them their session configuration. A missing file
/// is created with just the DPI line.
pub fn write_xresources_dpi(path: &Path, dpi: i32) -> std::io::Result<()> {
    let xft = Regex::new(r"^\s*Xft\.dpi\s*:").expect("regex");
    let mut lines: Vec<String> = match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .filter(|l| !xft.is_match(l))
            .map(str::to_string)
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    lines.push(format!("Xft.dpi:   {dpi}"));
    atomic_write_text(path, &(lines.join("\n") + "\n"))
}

/// Resolve the LXQt session font's point size to pixels at `dpi`.
///
/// Returns `(points, pixels)` when the file was rewritten, `None` when there is no LXQt
/// configuration or no font in it. Only the font line changes; every other setting in the
/// file is left byte for byte.
///
/// Qt keeps a widget's font from the moment it is built, so a density delivered as Xft
/// resources alone reaches nothing already on screen. The LXQt platform theme watches this
/// file and answers a change with `QApplication::setFont`, which is the one call that
/// repolishes those widgets -- but Qt drops a font it considers equal, and a point size is
/// equal at every density. Resolving it to pixels is what makes the change land. The point
/// size stays in the field Qt ignores once a pixel size is set, so the next density has a
/// base to scale; a file Qt round-tripped carries pixels alone, resolved at the density
/// currently on the display (`current_dpi`).
pub fn rewrite_lxqt_font(path: &Path, dpi: i32, current_dpi: i32) -> Option<(f64, i32)> {
    let text = std::fs::read_to_string(path).ok()?;
    let font_line = Regex::new(r#"(?i)^(\s*font\s*=\s*)"?([^"\n]*)"?\s*$"#).expect("regex");
    let mut section = String::new();
    let mut out = String::new();
    let mut resolved: Option<(f64, i32)> = None;
    for line in text.split_inclusive('\n') {
        let stripped = line.trim();
        if stripped.starts_with('[') && stripped.ends_with(']') {
            section = stripped[1..stripped.len() - 1].to_lowercase();
        }
        let m = if section == "qt" && resolved.is_none() {
            font_line.captures(line.trim_end_matches('\n'))
        } else {
            None
        };
        let Some(m) = m else {
            out.push_str(line);
            continue;
        };
        let mut fields: Vec<String> = m[2].split(',').map(str::to_string).collect();
        if fields.len() < 3 {
            out.push_str(line);
            continue;
        }
        let (Ok(mut points), Ok(pixels)) = (
            fields[1].trim().parse::<f64>(),
            fields[2].trim().parse::<f64>(),
        ) else {
            out.push_str(line);
            continue;
        };
        if points <= 0.0 {
            points = pixels * 72.0 / current_dpi.max(1) as f64;
        }
        if points <= 0.0 {
            out.push_str(line);
            continue;
        }
        fields[1] = trim_float(points);
        let px = ((points * dpi as f64 / 72.0).round() as i32).max(1);
        fields[2] = px.to_string();
        out.push_str(&format!("{}\"{}\"\n", &m[1], fields.join(",")));
        resolved = Some((points, px));
    }
    let resolved = resolved?;
    atomic_write_text(path, &out).ok()?;
    Some(resolved)
}

/// A float as Python's `%g` writes it: no trailing zeros, no dangling point.
fn trim_float(v: f64) -> String {
    if v.fract() == 0.0 {
        return format!("{}", v as i64);
    }
    let s = format!("{v:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The environment process `pid` runs with, from `/proc`; empty when it cannot be read.
pub fn process_environ(pid: i32) -> BTreeMap<String, String> {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return BTreeMap::new();
    };
    raw.split(|b| *b == 0)
        .filter_map(|item| {
            let text = String::from_utf8_lossy(item);
            text.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect()
}

/// The X server a display name reaches, as its host and display number.
///
/// Names read as Xlib reads them, `[protocol/][host]:number[.screen]`: every local form
/// (`:20`, `:20.0`, `unix:20`, `unix/:20`) reaches host `""`, and a name over TCP keeps its
/// host, so it matches only a name of that host. `None` for anything that is not a display
/// name.
pub fn display_server(name: Option<&str>) -> Option<(String, String)> {
    let pat = Regex::new(r"^(?:(\w+)/)?(.*):(\d+)(?:\.\d+)?$").expect("regex");
    let m = pat.captures(name.unwrap_or(""))?;
    let protocol = m.get(1).map(|p| p.as_str());
    let host = &m[2];
    let host = if protocol == Some("unix") || host.is_empty() || host == "unix" {
        String::new()
    } else {
        host.to_lowercase()
    };
    Some((host, m[3].to_string()))
}

/// PIDs running `binary` on this process's X display, in PID order.
///
/// A process belongs to a display by the DISPLAY it was started with, which is the only way
/// xsettingsd or a session learns its display: one serving another display, a second
/// session of the same user or a test server, is never acted on for this one. Processes are
/// found by their `comm`, the name `pgrep -x` matches, read from `/proc`.
pub fn pids_on_display(binary: &str) -> Vec<i32> {
    let Some(display) = display_server(std::env::var("DISPLAY").ok().as_deref()) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let comm_name: String = binary.chars().take(15).collect();
    let mut pids: Vec<i32> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .is_ok_and(|c| c.trim_end() == comm_name)
                && display_server(process_environ(*pid).get("DISPLAY").map(String::as_str))
                    == Some(display.clone())
        })
        .collect();
    pids.sort_unstable();
    pids
}

/// Environment of the `binary` session process running on this display.
///
/// A desktop keeps its settings behind its own session bus (xfconfd, dconf), so a command
/// writing them runs with the environment read out of that process's `/proc/pid/environ`
/// rather than Selkies' own: the bus Selkies inherited may belong to another display's
/// session of the same home, whose settings daemons would apply the change there, live.
/// `None` when no such process with a `DBUS_SESSION_BUS_ADDRESS` runs on this display.
pub fn session_env(binary: &str) -> Option<BTreeMap<String, String>> {
    pids_on_display(binary)
        .into_iter()
        .map(process_environ)
        .find(|env| env.contains_key("DBUS_SESSION_BUS_ADDRESS"))
}

/// Hand the density to the Qt applications of the LXQt session on this display.
///
/// The X11 counterpart of the Wayland output scale: there the compositor tells clients their
/// scale and they redraw, here the platform theme repolishes them from its own
/// configuration. Applications on other toolkits, and any started later, take the same
/// density from the Xft resources instead. The file is rewritten only for a session on this
/// display, and it is the one the session reads: under its own `XDG_CONFIG_HOME`, else its
/// home.
async fn run_lxqt_font(dpi: i32) -> bool {
    let pids = tokio::task::spawn_blocking(|| pids_on_display("lxqt-session"))
        .await
        .unwrap_or_default();
    let Some(&pid) = pids.first() else {
        return false;
    };
    let env = process_environ(pid);
    let config = env
        .get("XDG_CONFIG_HOME")
        .filter(|c| !c.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env.get("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(home)
                .join(".config")
        });
    let path = config.join("lxqt").join("lxqt.conf");
    let current = applied_dpi().or_else(desktop_dpi_from_files).unwrap_or(96);
    let resolved = tokio::task::spawn_blocking(move || rewrite_lxqt_font(&path, dpi, current))
        .await
        .unwrap_or(None);
    let Some((points, pixels)) = resolved else {
        return false;
    };
    info!(
        target: "display",
        "LXQt session font resolved to {pixels}px for DPI {dpi} ({}pt), repolishing running applications.",
        trim_float(points)
    );
    true
}

/// Apply DPI via Xresources/xrdb and the xsettingsd config.
///
/// Writes `Xft.dpi` into `~/.Xresources` and merges it into the running resource database --
/// merged, never loaded wholesale, so the database keeps every resource the file does not
/// define -- then rewrites `~/.xsettingsd` with the matching `Xft/DPI` value (in 1024ths)
/// and SIGHUPs every xsettingsd serving this display: the one serving it is not necessarily
/// the oldest, and one serving another display from the same home would re-read this file
/// and hand that display this density. Returns whether the xrdb merge succeeded.
async fn run_xrdb(dpi: i32) -> bool {
    if !which("xrdb") {
        debug!(target: "display", "xrdb not found. Skipping Xresources DPI setting.");
        return false;
    }
    let xresources = home().join(".Xresources");
    let written = tokio::task::spawn_blocking({
        let path = xresources.clone();
        move || write_xresources_dpi(&path, dpi)
    })
    .await;
    if let Err(e) = written
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| e.to_string()))
    {
        error!(target: "display", "Error updating or loading DPI settings: {e}");
        return false;
    }
    debug!(target: "display", "Wrote 'Xft.dpi:   {dpi}' to {}.", xresources.display());
    let out = run_tool(
        "xrdb",
        &["-merge", xresources.to_str().unwrap_or("~/.Xresources")],
        None,
        None,
    )
    .await;
    if out.success {
        debug!(target: "display", "Successfully loaded {} using xrdb.", xresources.display());
    } else {
        warn!(target: "display", "Failed to load {} using xrdb. Error: {}", xresources.display(), out.stderr.trim());
    }
    let xsettingsd = home().join(".xsettingsd");
    let content = format!(
        "Xft/Antialias 1\nXft/Hinting 1\nXft/HintStyle \"hintfull\"\nXft/RGBA \"rgb\"\nXft/DPI {}\n",
        dpi * 1024
    );
    let written = tokio::task::spawn_blocking({
        let path = xsettingsd.clone();
        move || atomic_write_text(&path, &content)
    })
    .await;
    if let Err(e) = written
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| e.to_string()))
    {
        error!(target: "display", "Error updating or loading DPI settings: {e}");
        return false;
    }
    debug!(target: "display", "Wrote font and DPI settings to {}.", xsettingsd.display());
    let pids = tokio::task::spawn_blocking(|| pids_on_display("xsettingsd"))
        .await
        .unwrap_or_default();
    let mut signaled = Vec::new();
    for pid in &pids {
        match nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(*pid),
            nix::sys::signal::Signal::SIGHUP,
        ) {
            Ok(()) => signaled.push(pid.to_string()),
            Err(e) => {
                debug!(target: "display", "Failed to send SIGHUP to xsettingsd process {pid}: {e}")
            }
        }
    }
    if !signaled.is_empty() {
        debug!(target: "display", "Sent SIGHUP to xsettingsd to reload config ({}).", signaled.join(", "));
    } else if !pids.is_empty() {
        warn!(target: "display", "No xsettingsd process could be signaled to reload.");
    } else {
        debug!(target: "display", "No xsettingsd serves this display. Skipping reload.");
    }
    out.success
}

/// Apply DPI and a DPI-scaled cursor size via xfconf-query for XFCE. Commands run in the
/// environment of the XFCE session on this display (`session_env`), so they reach its own
/// bus; without one nothing is written. Returns whether both settings were applied.
async fn run_xfconf(dpi: i32) -> bool {
    if !which("xfconf-query") {
        debug!(target: "display", "xfconf-query not found. Skipping XFCE DPI setting via xfconf-query.");
        return false;
    }
    let Some(env) = tokio::task::spawn_blocking(|| session_env("xfce4-session"))
        .await
        .unwrap_or(None)
    else {
        warn!(target: "display", "No XFCE session with a session bus runs on this display; xfconf is left as it is.");
        return false;
    };
    let dpi_text = dpi.to_string();
    let out = run_tool(
        "xfconf-query",
        &[
            "-c",
            "xsettings",
            "-p",
            "/Xft/DPI",
            "-s",
            &dpi_text,
            "--create",
            "-t",
            "int",
        ],
        Some(&env),
        None,
    )
    .await;
    if !out.success {
        warn!(target: "display", "Failed to set XFCE DPI using xfconf-query. Error: {}", out.stderr.trim());
        return false;
    }
    debug!(target: "display", "Successfully set XFCE DPI to {dpi} using xfconf-query.");
    let cursor = ((dpi as f64 / 96.0 * 32.0).round() as i32).to_string();
    debug!(target: "display", "Attempting to set cursor size to: {cursor} (based on DPI {dpi})");
    let out = run_tool(
        "xfconf-query",
        &[
            "-c",
            "xsettings",
            "-p",
            "/Gtk/CursorThemeSize",
            "-s",
            &cursor,
            "--create",
            "-t",
            "int",
        ],
        Some(&env),
        None,
    )
    .await;
    if !out.success {
        warn!(target: "display", "Failed to set cursor size using xfconf-query. Error: {}", out.stderr.trim());
        return false;
    }
    debug!(target: "display", "Successfully set cursor size to {cursor}");
    true
}

/// Apply DPI via MATE gsettings (window-scaling-factor and font DPI).
///
/// `window-scaling-factor` is integer-only, so it carries whole scales and stays 1
/// otherwise, the fractional part riding on the font DPI. The commands run in the
/// environment of the MATE session on this display; without one nothing is written. Returns
/// whether at least one setting was applied.
async fn run_mate_gsettings(dpi: i32) -> bool {
    if !which("gsettings") {
        debug!(target: "display", "gsettings not found. Skipping MATE gsettings.");
        return false;
    }
    let Some(env) = tokio::task::spawn_blocking(|| session_env("mate-session"))
        .await
        .unwrap_or(None)
    else {
        debug!(target: "display", "No MATE session with a session bus runs on this display; gsettings left alone.");
        return false;
    };
    let scale = dpi as f64 / 96.0;
    let factor = if scale.fract() == 0.0 {
        (scale as i32).max(1)
    } else {
        1
    };
    let mut any = false;
    let out = run_tool(
        "gsettings",
        &[
            "set",
            "org.mate.interface",
            "window-scaling-factor",
            &factor.to_string(),
        ],
        Some(&env),
        None,
    )
    .await;
    if out.success {
        debug!(target: "display", "Successfully set MATE window-scaling-factor to {factor} (for DPI {dpi}) using gsettings.");
        any = true;
    } else if out.stderr.contains("No such schema") || out.stderr.contains("No such key") {
        debug!(target: "display", "gsettings: Schema/key 'org.mate.interface window-scaling-factor' not found. Error: {}", out.stderr.trim());
    } else {
        warn!(target: "display", "Failed to set MATE window-scaling-factor using gsettings. Error: {}", out.stderr.trim());
    }
    let out = run_tool(
        "gsettings",
        &["set", "org.mate.font-rendering", "dpi", &dpi.to_string()],
        Some(&env),
        None,
    )
    .await;
    if out.success {
        debug!(target: "display", "Successfully set MATE font-rendering DPI to {dpi} using gsettings.");
        any = true;
    } else if out.stderr.contains("No such schema") || out.stderr.contains("No such key") {
        debug!(target: "display", "gsettings: Schema/key 'org.mate.font-rendering dpi' not found. Error: {}", out.stderr.trim());
    } else {
        warn!(target: "display", "Failed to set MATE font-rendering DPI using gsettings. Error: {}", out.stderr.trim());
    }
    any
}

/// What the session calls itself, from the variables it sets for exactly this, or empty
/// when it says nothing.
pub fn declared_desktop() -> String {
    ["XDG_CURRENT_DESKTOP", "DESKTOP_SESSION"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// Whether the session runs the named desktop. A session that says what it is settles it;
/// an installed binary is only a hint, and the wrong one wherever more than one desktop is
/// installed, so it answers for a session that says nothing at all.
pub fn running_desktop(name: &str, session_binary: &str) -> bool {
    let declared = declared_desktop().to_lowercase();
    if !declared.is_empty() {
        return declared.contains(&name.to_lowercase());
    }
    which(session_binary)
}

/// The last positive match of `pattern`'s first group in `text`.
fn last_dpi(pattern: &Regex, text: &str) -> Option<i32> {
    pattern
        .captures_iter(text)
        .filter_map(|c| c[1].parse::<i32>().ok())
        .last()
        .filter(|v| *v > 0)
}

/// The density the home persists for the X11 desktop, read as `desktop_dpi` reads it but
/// without the server's resource database.
fn desktop_dpi_from_files() -> Option<i32> {
    desktop_dpi_from(None)
}

/// `desktop_dpi` over the persisted files and `resource_db`, the server's `RESOURCE_MANAGER`
/// where it was read.
fn desktop_dpi_from(resource_db: Option<String>) -> Option<i32> {
    let xft = Regex::new(r"(?m)^\s*Xft\.dpi\s*:\s*(\d+)").expect("regex");
    let xsettings = Regex::new(r"(?m)^\s*Xft/DPI\s+(\d+)").expect("regex");
    let xfconf = Regex::new(r#"name="DPI"[^>]*value="(-?\d+)""#).expect("regex");
    let persisted = |path: PathBuf| std::fs::read_to_string(path).unwrap_or_default();
    if running_desktop("xfce", "xfce4-session") {
        let config = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|c| !c.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config"));
        if let Some(dpi) = last_dpi(&xfconf, &persisted(config.join(XFCONF_XSETTINGS))) {
            return Some(dpi);
        }
    }
    if let Some(db) = resource_db
        && let Some(dpi) = last_dpi(&xft, &db)
    {
        return Some(dpi);
    }
    if let Some(dpi) = last_dpi(&xft, &persisted(home().join(".Xresources"))) {
        return Some(dpi);
    }
    last_dpi(&xsettings, &persisted(home().join(".xsettingsd"))).map(|v| v / 1024)
}

/// The density the X11 desktop has before this process applies one.
///
/// Read from where `set_dpi` puts it: an XFCE session's xfconf channel as xfconfd persists
/// it, else the server's resource database and then the resources this home persists, which
/// outlive a restart the session may not have merged yet. `None` when nothing names one.
pub async fn desktop_dpi() -> Option<i32> {
    let db = on_x11(|x| x.resource_manager_text()).await.ok().flatten();
    tokio::task::spawn_blocking(move || desktop_dpi_from(db))
        .await
        .unwrap_or(None)
}

/// Settle the X11 desktop's density at startup and return the one in effect.
///
/// A density persists in the home across restarts while this process starts knowing none,
/// so the desktop is read before anything is written: an operator-set value is applied
/// whenever the desktop differs from it, a client-driven desktop keeps the density its last
/// page gave it, and either way the one in effect is what a page's DPI is then compared
/// against. Unity on a fresh home writes nothing.
pub async fn restore_dpi(configured: i32, locked: bool) -> i32 {
    let current = desktop_dpi().await;
    let target = if locked {
        configured
    } else {
        current.unwrap_or(configured)
    };
    if target != 96 || !matches!(current, None | Some(96)) {
        set_dpi(target).await;
    }
    target
}

/// Set the X11 display DPI using DE-specific methods (see the module docblock). Returns
/// whether at least one method succeeded; false on the Wayland backend, where a DPI is an
/// output scale.
pub async fn set_dpi(dpi: i32) -> bool {
    if dpi <= 0 {
        error!(target: "display", "Invalid DPI value: {dpi}. Must be a positive integer.");
        return false;
    }
    if crate::settings::settings().wayland().on {
        debug!(target: "display", "Wayland backend: DPI realizes as a compositor output scale.");
        return false;
    }
    if applied_dpi() == Some(dpi) {
        debug!(target: "display", "DPI {dpi} already applied; skipping the re-ladder.");
        return true;
    }
    let declared = declared_desktop();
    let desktop = if declared.is_empty() {
        "unnamed".to_string()
    } else {
        declared
    };
    let mut any = false;
    if running_desktop("xfce", "xfce4-session") {
        debug!(target: "display", "XFCE session ({desktop}): applying xfconf-query for DPI {dpi}.");
        any |= run_xfconf(dpi).await;
    } else if running_desktop("mate", "mate-session") {
        debug!(target: "display", "MATE session ({desktop}): applying gsettings and xrdb for DPI {dpi}.");
        let mate = run_mate_gsettings(dpi).await;
        let xrdb = run_xrdb(dpi).await;
        any |= mate || xrdb;
    } else {
        debug!(target: "display", "{desktop} session: applying xrdb for DPI {dpi}.");
        any |= run_xrdb(dpi).await;
    }
    any |= run_lxqt_font(dpi).await;
    if !any {
        warn!(target: "display", "No DPI setting method succeeded for DPI {dpi} ({desktop} session).");
    } else if let Err(e) = on_x11(move |x| x.stamp_root_dpi(dpi)).await {
        warn!(target: "display", "Root mm-size retarget to {dpi} DPI failed: {e}");
    }
    any
}

/// Merge `Xcursor.size` into the root resource database. Xcursor-driven consumers (openbox
/// after the multi-monitor WM swap, plain X apps) take their cursor size from here, not from
/// XFCE/GNOME settings daemons.
async fn set_xcursor_resource(size: i32) -> bool {
    if !which("xrdb") {
        return false;
    }
    let out = run_tool(
        "xrdb",
        &["-merge", "-"],
        None,
        Some(format!("Xcursor.size: {size}\n").as_bytes()),
    )
    .await;
    if !out.success {
        debug!(target: "display", "xrdb Xcursor.size merge failed: {}", out.stderr.trim());
    }
    out.success
}

/// Set the X cursor size through every applicable settings channel.
///
/// Merges `Xcursor.size` via xrdb, then tries the XFCE and GNOME settings daemons serving
/// this display, each through its session's own environment (`session_env`: the session
/// manager for XFCE, the xsettings daemon that publishes the GNOME key); desktop-aware
/// toolkits follow their daemon while plain X apps follow the Xcursor resource, so daemon
/// success returns immediately and the xrdb merge alone still counts as success. Returns
/// whether any channel applied the size.
pub async fn set_cursor_size(size: i32) -> bool {
    if size <= 0 {
        error!(target: "display", "Invalid cursor size: {size}");
        return false;
    }
    let xrdb_ok = set_xcursor_resource(size).await;
    let size_text = size.to_string();
    if which("xfconf-query")
        && let Some(env) = tokio::task::spawn_blocking(|| session_env("xfce4-session"))
            .await
            .unwrap_or(None)
    {
        let out = run_tool(
            "xfconf-query",
            &[
                "-c",
                "xsettings",
                "-p",
                "/Gtk/CursorThemeSize",
                "-s",
                &size_text,
                "--create",
                "-t",
                "int",
            ],
            Some(&env),
            None,
        )
        .await;
        if out.success {
            return true;
        }
        warn!(target: "display", "Failed to set XFCE cursor size.");
    }
    if which("gsettings")
        && let Some(env) = tokio::task::spawn_blocking(|| session_env("gsd-xsettings"))
            .await
            .unwrap_or(None)
    {
        let out = run_tool(
            "gsettings",
            &[
                "set",
                "org.gnome.desktop.interface",
                "cursor-size",
                &size_text,
            ],
            Some(&env),
            None,
        )
        .await;
        if out.success {
            debug!(target: "display", "Set GNOME cursor-size to {size}");
            return true;
        }
        warn!(target: "display", "Failed to set GNOME cursor-size.");
    }
    if xrdb_ok {
        return true;
    }
    warn!(target: "display", "No supported tool found/worked to set cursor size.");
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("selkies-dpi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmpdir");
        dir.join(name)
    }

    #[test]
    fn display_names_resolve_to_servers() {
        assert_eq!(
            display_server(Some(":20")),
            Some((String::new(), "20".into()))
        );
        assert_eq!(
            display_server(Some(":20.0")),
            Some((String::new(), "20".into()))
        );
        assert_eq!(
            display_server(Some("unix:20")),
            Some((String::new(), "20".into()))
        );
        assert_eq!(
            display_server(Some("unix/:20")),
            Some((String::new(), "20".into()))
        );
        assert_eq!(
            display_server(Some("Host:1")),
            Some(("host".into(), "1".into()))
        );
        assert_eq!(display_server(Some("nonsense")), None);
        assert_eq!(display_server(None), None);
    }

    #[test]
    fn xresources_keeps_every_other_line() {
        let path = scratch("Xresources");
        std::fs::write(&path, "*background: black\nXft.dpi: 96\nXft.antialias: 1\n").expect("seed");
        write_xresources_dpi(&path, 144).expect("write");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(
            text,
            "*background: black\nXft.antialias: 1\nXft.dpi:   144\n"
        );
        let fresh = scratch("Xresources-fresh");
        let _ = std::fs::remove_file(&fresh);
        write_xresources_dpi(&fresh, 120).expect("write");
        assert_eq!(
            std::fs::read_to_string(&fresh).expect("read"),
            "Xft.dpi:   120\n"
        );
    }

    #[test]
    fn lxqt_font_resolves_points_to_pixels() {
        let path = scratch("lxqt.conf");
        std::fs::write(
            &path,
            "[General]\ntheme=x\n\n[Qt]\nfont=\"Sans,10,-1,5,50,0,0,0,0,0\"\nstyle=Fusion\n",
        )
        .expect("seed");
        assert_eq!(rewrite_lxqt_font(&path, 144, 96), Some((10.0, 20)));
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains("font=\"Sans,10,20,5,50,0,0,0,0,0\"\n"),
            "{text}"
        );
        assert!(text.contains("style=Fusion\n"));
        std::fs::write(&path, "[Qt]\nfont=\"Sans,-1,16,5,50,0,0,0,0,0\"\n").expect("seed");
        assert_eq!(
            rewrite_lxqt_font(&path, 192, 96),
            Some((12.0, 32)),
            "pixels alone are read back at the current density"
        );
        std::fs::write(&path, "[General]\nfont=\"Sans,10,-1\"\n").expect("seed");
        assert_eq!(
            rewrite_lxqt_font(&path, 144, 96),
            None,
            "a font outside [Qt] is not the session font"
        );
    }

    #[test]
    fn persisted_density_is_read_in_order() {
        let xft = Regex::new(r"(?m)^\s*Xft\.dpi\s*:\s*(\d+)").expect("regex");
        assert_eq!(last_dpi(&xft, "Xft.dpi: 96\nXft.dpi: 144\n"), Some(144));
        assert_eq!(last_dpi(&xft, "Xft.dpi: 0\n"), None);
        assert_eq!(trim_float(10.0), "10");
        assert_eq!(trim_float(10.5), "10.5");
    }
}
