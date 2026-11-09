/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The `xrandr` subprocess fallbacks behind the native RandR calls, taken only when the native
//! call fails: the resolution listing parsed from `xrandr`'s output, a resize through
//! `--newmode`/`--addmode`/`--output` with the timings the native path makes its modes from
//! (`modes::mode_timings`), and the one-shot `xrandr` commands the logical-monitor path
//! degrades to.
//!
//! Every tool runs bounded (`run_tool`): on expiry the process is killed and its failure
//! reported, so a wedged X server never hangs a reconfiguration.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use log::{debug, error, info, warn};
use regex::Regex;
use tokio::process::Command;

use super::geometry::fit_res;
use super::modes::{REFRESH_SLACK, mode_timings, target_refresh};
use super::resize::ResolutionInfo;

/// How long a display tool may run before it is killed.
const TOOL_TIMEOUT: Duration = Duration::from_secs(5);

/// The 8K ceiling a client may drive the server to.
pub const MAX_WIDTH: i32 = 7680;
pub const MAX_HEIGHT: i32 = 4320;

/// What a tool run came back with.
#[derive(Debug)]
pub struct ToolOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run `program` with `args`, bounded to `TOOL_TIMEOUT`; `env` replaces the process
/// environment where given. On expiry the process is killed and reaped and the failure
/// carries a timeout message, so callers observe a failed run instead of hanging.
pub async fn run_tool(
    program: &str,
    args: &[&str],
    env: Option<&BTreeMap<String, String>>,
    stdin: Option<&[u8]>,
) -> ToolOutput {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    if let Some(env) = env {
        cmd.env_clear().envs(env);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return ToolOutput {
                success: false,
                stdout: String::new(),
                stderr: e.to_string(),
            };
        }
    };
    if let Some(bytes) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        use tokio::io::AsyncWriteExt;
        let _ = pipe.write_all(bytes).await;
        drop(pipe);
    }
    match tokio::time::timeout(TOOL_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => ToolOutput {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        },
        Ok(Err(e)) => ToolOutput {
            success: false,
            stdout: String::new(),
            stderr: e.to_string(),
        },
        Err(_) => ToolOutput {
            success: false,
            stdout: String::new(),
            stderr: format!("timed out after {}s", TOOL_TIMEOUT.as_secs()),
        },
    }
}

/// Whether `program` is on `PATH`.
pub fn which(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join(program);
            std::fs::metadata(&candidate).is_ok_and(|m| {
                m.is_file()
                    && std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0
            })
        })
    })
}

/// Run one `xrandr` command, returning success; failures are logged, not raised, so layout
/// application degrades per step.
pub async fn run_xrandr(args: &[&str], what: &str) -> bool {
    let out = run_tool("xrandr", args, None, None).await;
    if !out.success {
        warn!(target: "display", "xrandr {what} failed: {}", out.stderr.trim());
    }
    out.success
}

/// What `parse_xrandr_listing` reads of an `xrandr` listing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct XrandrListing {
    /// The root's size as `WxH`, which heads the listing whether or not an output follows.
    pub current: Option<String>,
    /// The first connected output.
    pub output: Option<String>,
    /// The `WxH` mode names of that output, sorted and unique.
    pub resolutions: Vec<String>,
    /// The size that output shows, from its `WxH+X+Y` geometry; `None` where it shows none.
    pub shown: Option<String>,
}

/// Parse an `xrandr` listing for the root's size, the first connected output, its modes, and
/// the size it shows.
pub fn parse_xrandr_listing(text: &str) -> XrandrListing {
    let screen_pat = Regex::new(r"(\S+) connected").expect("regex");
    let shown_pat = Regex::new(r"connected (?:primary )?(\d+x\d+)[+-]\d+[+-]\d+").expect("regex");
    let current_pat = Regex::new(r".*current (\d+\s*x\s*\d+).*").expect("regex");
    let res_pat = Regex::new(r"^(\d+x\d+)\s+\d+\.\d+.*").expect("regex");
    let mut listing = XrandrListing::default();
    let mut in_output = false;
    for line in text.lines() {
        if let Some(m) = current_pat.captures(line) {
            listing.current = Some(m[1].replace(' ', ""));
        }
        if let Some(m) = screen_pat.captures(line) {
            let name = m[1].to_string();
            if listing.output.is_none() {
                listing.output = Some(name.clone());
                listing.shown = shown_pat.captures(line).map(|s| s[1].to_string());
            }
            in_output = listing.output.as_deref() == Some(name.as_str());
        }
        if in_output && let Some(m) = res_pat.captures(line.trim()) {
            listing.resolutions.push(m[1].to_string());
        }
    }
    listing.resolutions.sort();
    listing.resolutions.dedup();
    listing
}

/// xrandr-subprocess fallback for `get_new_res`.
pub async fn get_new_res_xrandr(res_str: &str) -> ResolutionInfo {
    get_new_res_from(res_str, "xrandr").await
}

/// `get_new_res_xrandr` through the listing `program` prints.
async fn get_new_res_from(res_str: &str, program: &str) -> ResolutionInfo {
    let mut info = ResolutionInfo {
        current: res_str.to_string(),
        fitted: res_str.to_string(),
        modes: Vec::new(),
        max: res_str.to_string(),
        output: None,
        shown: None,
    };
    let out = run_tool(program, &[], None, None).await;
    if !out.success && out.stdout.is_empty() {
        error!(target: "display", "xrandr command failed: {}", out.stderr.trim());
        return info;
    }
    let listing = parse_xrandr_listing(&out.stdout);
    if let Some(current) = listing.current {
        info.current = current;
    }
    info.modes = listing.resolutions;
    let Some(output) = listing.output else {
        debug!(target: "display", "No connected RandR output on this X server; the root is all there is.");
        return info;
    };
    info.output = Some(output);
    info.shown = listing.shown;
    info.max = format!("{MAX_WIDTH}x{MAX_HEIGHT}");
    match parse_wh(res_str) {
        Some((w, h)) => {
            let (fw, fh) = fit_res(w, h, MAX_WIDTH, MAX_HEIGHT);
            info.fitted = format!("{fw}x{fh}");
        }
        None => error!(target: "display", "Invalid resolution format for fitting: {res_str}"),
    }
    info
}

/// `WxH` as two positive numbers.
pub fn parse_wh(text: &str) -> Option<(i32, i32)> {
    let (w, h) = text.trim().split_once('x')?;
    let (w, h): (i32, i32) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

/// The first connected output and the refresh of each mode it lists, by name, as the
/// `xrandr` listing gives them (a driver lists several modes under one name).
pub fn parse_xrandr_rates(text: &str) -> (Option<String>, BTreeMap<String, Vec<f64>>) {
    let output_pat = Regex::new(r"^(\S+) (?:dis)?connected").expect("regex");
    let rate_pat = Regex::new(r"^\d+\.\d+").expect("regex");
    let number_pat = Regex::new(r"\d+\.\d+").expect("regex");
    let mut screen_name: Option<String> = None;
    let mut rates: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for line in text.lines() {
        if let Some(m) = output_pat.captures(line) {
            if screen_name.is_some() {
                break;
            }
            if line.contains(" connected") {
                screen_name = Some(m[1].to_string());
            }
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if screen_name.is_some()
            && line.chars().next().is_some_and(char::is_whitespace)
            && fields.len() > 1
            && fields[1..].iter().all(|f| rate_pat.is_match(f))
        {
            let listed: Vec<f64> = fields[1..]
                .iter()
                .flat_map(|f| {
                    number_pat
                        .find_iter(f)
                        .filter_map(|m| m.as_str().parse().ok())
                        .collect::<Vec<f64>>()
                })
                .collect();
            rates
                .entry(fields[0].to_string())
                .or_default()
                .extend(listed);
        }
    }
    (screen_name, rates)
}

/// `parse_xrandr_rates` over a live `xrandr` run.
pub async fn xrandr_output_rates() -> (Option<String>, BTreeMap<String, Vec<f64>>) {
    let out = run_tool("xrandr", &[], None, None).await;
    if !out.success && out.stdout.is_empty() {
        error!(target: "display", "xrandr command failed: {}", out.stderr.trim());
        return (None, BTreeMap::new());
    }
    parse_xrandr_rates(&out.stdout)
}

/// Resize the display using xrandr subprocesses.
///
/// The mode follows the native path's rule (`modes::mode_at`): one of the requested
/// geometry at the refresh `target_refresh` makes of `refresh`, chosen by name and rate
/// since a driver lists several modes under one name. A missing one is made from
/// `modes::mode_timings` at that refresh and named for the geometry those timings carry (CVT
/// snaps the width up to its 8-pixel cell, so it can be wider than requested), with the rate
/// appended where that name is taken; a mode so named is found again by that name, so the
/// next resize to the size and rate reuses it rather than making another. The
/// mode is set together with `--fb` sized from that realized geometry: without it a larger
/// root left over from a prior extended layout keeps the screen oversized, so the new mode
/// lands top-left and whole-root capture shows black bars, and a framebuffer narrower than
/// the active mode is rejected outright.
///
/// Returns the realized size, or `None` on failure.
pub async fn resize_display_xrandr(res_str: &str, refresh: Option<f64>) -> Option<(i32, i32)> {
    let (screen_name, rates) = xrandr_output_rates().await;
    let Some(screen_name) = screen_name else {
        error!(target: "display", "Cannot resize display via xrandr, no screen identified.");
        return None;
    };
    let Some((w_req, h_req)) = parse_wh(res_str) else {
        error!(target: "display", "Invalid resolution format: {res_str}");
        return None;
    };
    let target = target_refresh(refresh);
    let fitting = |geometry: &str| -> Option<(String, f64)> {
        rates.iter().find_map(|(name, listed)| {
            (name == geometry || name.starts_with(&format!("{geometry}_")))
                .then(|| {
                    listed
                        .iter()
                        .find(|r| (*r - target).abs() <= target * REFRESH_SLACK)
                        .map(|r| (name.clone(), *r))
                })
                .flatten()
        })
    };
    let mut mode_to_set = res_str.to_string();
    let (mut realized_w, mut realized_h) = (w_req, h_req);
    let mut found = fitting(res_str);
    let mut rate: Option<f64> = None;
    if found.is_none() {
        debug!(target: "display", "No {res_str} mode at {target:.2} Hz in the xrandr list. Attempting to add for screen '{screen_name}'.");
        let timings = mode_timings(w_req as u32, h_req as u32, target);
        let modeline = timings.modeline();
        (realized_w, realized_h) = (timings.width as i32, timings.height as i32);
        let geometry = format!("{realized_w}x{realized_h}");
        mode_to_set = geometry.clone();
        found = fitting(&geometry);
        if found.is_none() {
            let candidates = [
                geometry.clone(),
                format!("{geometry}_{target:.0}"),
                format!("{geometry}_{target:.2}"),
            ];
            mode_to_set = candidates
                .iter()
                .find(|n| !rates.contains_key(*n))
                .cloned()
                .unwrap_or_else(|| format!("{geometry}_{target:.3}"));
            let mut new_args: Vec<&str> = vec!["--newmode", &mode_to_set];
            new_args.extend(modeline.split_whitespace());
            let out = run_tool("xrandr", &new_args, None, None).await;
            if !out.success {
                error!(target: "display", "Failed to create new xrandr mode with 'xrandr {}': {}", new_args.join(" "), out.stderr.trim());
                return None;
            }
            debug!(target: "display", "Successfully ran: xrandr {}", new_args.join(" "));
            let out = run_tool(
                "xrandr",
                &["--addmode", &screen_name, &mode_to_set],
                None,
                None,
            )
            .await;
            if !out.success {
                error!(target: "display", "Failed to add mode '{mode_to_set}' to screen '{screen_name}': {}", out.stderr.trim());
                run_tool(
                    "xrandr",
                    &["--delmode", &screen_name, &mode_to_set],
                    None,
                    None,
                )
                .await;
                run_tool("xrandr", &["--rmmode", &mode_to_set], None, None).await;
                return None;
            }
            debug!(target: "display", "Successfully ran: xrandr --addmode {screen_name} {mode_to_set}");
        }
    }
    if let Some((name, r)) = found {
        mode_to_set = name;
        rate = Some(r);
    }
    debug!(target: "display", "Applying xrandr mode '{mode_to_set}' for screen '{screen_name}'.");
    let rate_text = rate.map(|r| format!("{r:.2}"));
    let mut rate_args: Vec<&str> = Vec::new();
    if let Some(r) = &rate_text {
        rate_args.extend(["--rate", r.as_str()]);
    }
    let fb = format!("{realized_w}x{realized_h}");
    let mut args: Vec<&str> = vec!["--output", &screen_name, "--mode", &mode_to_set];
    args.extend(rate_args.iter().copied());
    args.extend(["--fb", fb.as_str()]);
    let out = run_tool("xrandr", &args, None, None).await;
    if !out.success {
        // A pre-existing mode can be CVT-snapped wider than its name claims, so a
        // framebuffer sized from the name is rejected; retry at the snapped width.
        let snapped_w = (w_req + 7) / 8 * 8;
        let mut stderr = out.stderr;
        let mut retried = false;
        if mode_to_set == res_str && snapped_w != realized_w {
            let fb = format!("{snapped_w}x{h_req}");
            let mut retry: Vec<&str> = vec!["--output", &screen_name, "--mode", &mode_to_set];
            retry.extend(rate_args.iter().copied());
            retry.extend(["--fb", fb.as_str()]);
            let again = run_tool("xrandr", &retry, None, None).await;
            if again.success {
                (realized_w, realized_h) = (snapped_w, h_req);
                retried = true;
            } else {
                stderr = again.stderr;
            }
        }
        if !retried {
            error!(target: "display", "Failed to set mode '{mode_to_set}' on screen '{screen_name}': {}", stderr.trim());
            return None;
        }
    }
    info!(
        target: "display",
        "Successfully applied xrandr mode '{mode_to_set}' ({realized_w}x{realized_h} at {:.2} Hz).",
        rate.unwrap_or(target)
    );
    Some((realized_w, realized_h))
}

/// Names of all RandR logical monitors as `xrandr --listmonitors` lists them.
pub async fn list_monitors_xrandr() -> Vec<String> {
    let out = run_tool("xrandr", &["--listmonitors"], None, None).await;
    if !out.success {
        warn!(target: "display", "xrandr --listmonitors failed: {}", out.stderr.trim());
        return Vec::new();
    }
    parse_monitor_listing(&out.stdout)
}

/// The monitor names of an `xrandr --listmonitors` listing.
pub fn parse_monitor_listing(text: &str) -> Vec<String> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            (parts.len() >= 2).then(|| parts[1].trim_start_matches(['*', '+']).to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUTLESS: &str =
        "Screen 0: minimum 8 x 8, current 1920 x 1080, maximum 32767 x 32767\n";
    const WITH_OUTPUT: &str = concat!(
        "Screen 0: minimum 1 x 1, current 1280 x 720, maximum 8192 x 4096\n",
        "screen connected primary 1280x720+0+0 0mm x 0mm\n",
        "   8192x4096      0.00\n",
        "   1280x720       0.00*\n",
    );

    /// A driver lists several refreshes under one mode name, marks the current one with `*`
    /// and the preferred with `+`, and nothing stops a mode being named like a rate; a line
    /// such as the sixth must neither count nor stall the parse.
    fn multi_rate() -> String {
        [
            "Screen 0: minimum 8 x 8, current 1920 x 1080, maximum 32767 x 32767",
            "DP-0 connected primary 1920x1080+0+0 520mm x 290mm",
            "   1920x1080     60.00*+ 119.98    59.94    50.00",
            "   1024x768      75.03    60.00",
            "   1024x768i     43.48",
            &format!(" ! 0.{} x", "000.".repeat(40)),
            "DP-1 disconnected",
            "   800x600       56.25",
            "",
        ]
        .join("\n")
    }

    #[test]
    fn outputless_server_reports_root_and_no_output() {
        let listing = parse_xrandr_listing(OUTPUTLESS);
        assert_eq!(listing.current.as_deref(), Some("1920x1080"));
        assert_eq!(listing.output, None);
        assert!(listing.resolutions.is_empty());
        assert_eq!(listing.shown, None);
    }

    #[test]
    fn server_with_output_reports_modes() {
        let listing = parse_xrandr_listing(WITH_OUTPUT);
        assert_eq!(listing.current.as_deref(), Some("1280x720"));
        assert_eq!(listing.output.as_deref(), Some("screen"));
        assert_eq!(listing.resolutions, ["1280x720", "8192x4096"]);
        assert_eq!(listing.shown.as_deref(), Some("1280x720"));
        let (fw, fh) = fit_res(2560, 1440, MAX_WIDTH, MAX_HEIGHT);
        assert_eq!(format!("{fw}x{fh}"), "2560x1440");
    }

    #[test]
    fn every_rate_of_the_connected_output_is_kept() {
        let started = std::time::Instant::now();
        let (name, rates) = parse_xrandr_rates(&multi_rate());
        assert_eq!(name.as_deref(), Some("DP-0"));
        let want: BTreeMap<String, Vec<f64>> = [
            ("1920x1080".to_string(), vec![60.0, 119.98, 59.94, 50.0]),
            ("1024x768".to_string(), vec![75.03, 60.0]),
            ("1024x768i".to_string(), vec![43.48]),
        ]
        .into_iter()
        .collect();
        assert_eq!(rates, want);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a line that only looks like rates is skipped in linear time"
        );
    }

    #[test]
    fn monitor_listing_names() {
        let text = "Monitors: 2\n 0: +*screen 1248/330x694/184+0+0  screen\n 1: +selkies-display2 100/10x100/10+0+0  none\n";
        assert_eq!(parse_monitor_listing(text), ["screen", "selkies-display2"]);
    }

    /// A stand-in `xrandr` printing `listing`, as an executable in a private directory.
    fn stand_in(listing: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "selkies-xrandr-{}-{}",
            std::process::id(),
            listing.len()
        ));
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let script = dir.join("xrandr");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ncat <<'LISTING'\n{listing}LISTING\n"),
        )
        .expect("script");
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        script
    }

    #[tokio::test]
    async fn listings_through_a_stand_in_xrandr() {
        let script = stand_in(OUTPUTLESS);
        let info = get_new_res_from("1x1", script.to_str().expect("path")).await;
        assert_eq!(info.current, "1920x1080");
        assert_eq!(info.output, None);
        assert!(info.modes.is_empty());
        let _ = std::fs::remove_dir_all(script.parent().expect("dir"));

        let script = stand_in(WITH_OUTPUT);
        let info = get_new_res_from("2560x1440", script.to_str().expect("path")).await;
        assert_eq!(
            (info.current.as_str(), info.output.as_deref()),
            ("1280x720", Some("screen"))
        );
        assert_eq!(info.modes, ["1280x720", "8192x4096"]);
        assert_eq!(info.shown.as_deref(), Some("1280x720"));
        assert_eq!(info.fitted, "2560x1440");
        let _ = std::fs::remove_dir_all(script.parent().expect("dir"));
    }
}
