/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Mutter host arranger: places a GNOME host's monitors where selkies laid its displays out.
//!
//! Mutter serves no `zwlr_output_manager_v1`; its interface is
//! `org.gnome.Mutter.DisplayConfig` on the session bus. A configuration names every
//! logical monitor with its connector, current mode, position, scale, transform and primary
//! flag, and Mutter refuses one whose monitors overlap or do not touch. Positions are
//! logical pixels, so a union-layout point is divided by its monitor's scale, and the layout
//! is shifted to start at 0,0. A monitor the request does not name keeps its place, and one
//! outside the layout stays out. A virtual monitor is left out of the layout until a
//! configuration places it, which is why a grown display on Mutter is offered only where this
//! arranger is available, and why one is placed even where it would land where it is wanted. Sizes are not set
//! here: a virtual monitor follows the format its stream is consumed at. Configurations are
//! applied temporarily (method 1), so nothing reaches `monitors.xml`.

use std::collections::HashMap;
use std::time::Duration;

use zbus::{Connection, OwnedVariant, Proxy, Variant};

/// Mutter's display configuration service on the session bus, and its object path.
const DISPLAY_CONFIG: &str = "org.gnome.Mutter.DisplayConfig";
const DISPLAY_CONFIG_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
/// Bound on each DisplayConfig call, so a hung compositor cannot hold the control thread.
const CALL_DEADLINE: Duration = Duration::from_secs(2);
/// `ApplyMonitorsConfig`'s method 1: applied until the next configuration, never saved to
/// `monitors.xml`.
const METHOD_TEMPORARY: u32 = 1;

/// A DisplayConfig property map (`a{sv}`).
type Props = HashMap<String, OwnedVariant>;
/// A monitor's identity: connector, vendor, product, and serial.
type MonitorSpec = (String, String, String, String);
/// One mode of a monitor: id, width, height, refresh rate, preferred scale, supported scales,
/// and properties (`is-current`, `is-preferred`).
type Mode = (String, i32, i32, f64, f64, Vec<f64>, Props);
/// `GetCurrentState`'s reply: the configuration serial, every monitor with its modes, the
/// logical monitors (x, y, scale, transform, primary, the monitors they show), and properties.
type CurrentState = (
    u32,
    Vec<(MonitorSpec, Vec<Mode>, Props)>,
    Vec<(i32, i32, f64, u32, bool, Vec<MonitorSpec>, Props)>,
    Props,
);
/// One logical monitor as `ApplyMonitorsConfig` takes it: x, y, scale, transform, primary, and
/// each monitor it shows as (connector, mode id, properties).
type LogicalConfig<'a> = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, HashMap<String, Variant<'a>>)>,
);

/// One logical monitor of a configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct MonitorState {
    pub connector: String,
    /// The id of the mode the monitor runs, which a configuration must name again.
    pub mode_id: String,
    /// Position in logical pixels.
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    /// Rotation and flip, as Mutter numbers them (0 is none).
    pub transform: u32,
    pub primary: bool,
    /// Whether the monitor is in Mutter's layout; one outside it (a virtual monitor not yet
    /// placed, or a monitor its owner turned off) has a position made up here.
    pub placed: bool,
}

/// `current` moved to the union-layout positions `want` names (`(connector, x, y)`, first
/// entry the primary), in logical pixels, shifted to start at 0,0. A monitor outside the
/// layout joins it only where `want` names it: a configuration enables every monitor it
/// lists, and one its owner turned off stays off.
pub fn layout_config(current: &[MonitorState], want: &[(String, i32, i32)]) -> Vec<MonitorState> {
    let mut out: Vec<MonitorState> = current
        .iter()
        .filter(|m| m.placed || want.iter().any(|(c, _, _)| *c == m.connector))
        .map(|m| {
            let mut m = m.clone();
            if let Some((_, x, y)) = want.iter().find(|(c, _, _)| *c == m.connector) {
                m.x = (f64::from(*x) / m.scale).round() as i32;
                m.y = (f64::from(*y) / m.scale).round() as i32;
            }
            m.primary = want.first().map(|(c, _, _)| c) == Some(&m.connector);
            m
        })
        .collect();
    if !out.iter().any(|m| m.primary)
        && let Some(first) = out.first_mut()
    {
        first.primary = true;
    }
    let min_x = out.iter().map(|m| m.x).min().unwrap_or(0);
    let min_y = out.iter().map(|m| m.y).min().unwrap_or(0);
    for m in &mut out {
        // Positions come from the client's layout: a span past `i32` saturates, and Mutter
        // refuses the configuration rather than the subtraction overflowing.
        m.x = m.x.saturating_sub(min_x);
        m.y = m.y.saturating_sub(min_y);
    }
    out
}

/// Whether `config` places any monitor differently from `current`, or brings one into the
/// layout: re-applying an identical layout still makes Mutter reconfigure, which pauses every
/// stream it carries, while a monitor outside it stays out until a configuration places it,
/// even where the position made up for it is already the one wanted.
pub fn needs_apply(current: &[MonitorState], config: &[MonitorState]) -> bool {
    config.iter().any(|c| {
        current
            .iter()
            .find(|m| m.connector == c.connector)
            .is_none_or(|m| !m.placed || (m.x, m.y, m.primary) != (c.x, c.y, c.primary))
    })
}

/// Whether a layout request is worth applying: one display has nothing to arrange, and
/// applying it would still re-place (and re-primary) a host's real monitors.
pub fn worth_arranging(want: &[(String, i32, i32)]) -> bool {
    want.len() >= 2
}

async fn connect() -> Result<Connection, String> {
    zbus::connection::Builder::session()
        .method_timeout(CALL_DEADLINE)
        .build()
        .await
        .map_err(|e| format!("session bus: {e}"))
}

/// Whether the session bus carries Mutter's DisplayConfig, the only way a Mutter host's
/// monitors are arranged.
pub fn available() -> bool {
    zbus::block_on(async {
        match connect().await {
            Ok(conn) => crate::wayland::portal::name_owned(&conn, DISPLAY_CONFIG).await,
            Err(_) => false,
        }
    })
}

fn current_mode(modes: &[Mode]) -> Option<&Mode> {
    let flag = |m: &&Mode, key: &str| {
        m.6.get(key)
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false)
    };
    modes
        .iter()
        .find(|m| flag(m, "is-current"))
        .or_else(|| modes.iter().find(|m| flag(m, "is-preferred")))
        .or_else(|| modes.first())
}

/// Every monitor Mutter knows, as placed now; a monitor outside the layout (a virtual one
/// not yet placed, or one turned off) goes right of the placed ones at scale 1, marked
/// unplaced.
fn monitors_now(state: &CurrentState) -> Vec<MonitorState> {
    let (_, monitors, logical, _) = state;
    let mut out = Vec::new();
    let mut right = 0;
    for (x, y, scale, transform, primary, specs, _) in logical {
        for spec in specs {
            let Some((_, modes, _)) = monitors.iter().find(|(s, _, _)| s.0 == spec.0) else {
                continue;
            };
            let Some(mode) = current_mode(modes) else {
                continue;
            };
            right = right.max(x + (f64::from(mode.1) / scale).round() as i32);
            out.push(MonitorState {
                connector: spec.0.clone(),
                mode_id: mode.0.clone(),
                x: *x,
                y: *y,
                scale: *scale,
                transform: *transform,
                primary: *primary,
                placed: true,
            });
        }
    }
    for (spec, modes, _) in monitors {
        if out.iter().any(|m| m.connector == spec.0) {
            continue;
        }
        let Some(mode) = current_mode(modes) else {
            continue;
        };
        out.push(MonitorState {
            connector: spec.0.clone(),
            mode_id: mode.0.clone(),
            x: right,
            y: 0,
            scale: 1.0,
            transform: 0,
            primary: false,
            placed: false,
        });
        right += mode.1;
    }
    out
}

/// Arrange the host's monitors at `want` (`(connector, x, y)` in union-layout pixels, the
/// primary first). `Ok(true)` once Mutter took a new configuration, `Ok(false)` where there
/// was nothing to arrange: one display, or a layout the monitors already have.
pub fn apply(want: &[(String, i32, i32)]) -> Result<bool, String> {
    if !worth_arranging(want) {
        return Ok(false);
    }
    zbus::block_on(async {
        let conn = connect().await?;
        let proxy = Proxy::new(&conn, DISPLAY_CONFIG, DISPLAY_CONFIG_PATH, DISPLAY_CONFIG)
            .await
            .map_err(|e| e.to_string())?;
        let state: CurrentState = proxy
            .call("GetCurrentState", &())
            .await
            .map_err(|e| format!("GetCurrentState: {e}"))?;
        let now = monitors_now(&state);
        let config = layout_config(&now, want);
        if !needs_apply(&now, &config) {
            return Ok(false);
        }
        let logical: Vec<LogicalConfig<'_>> = config
            .iter()
            .map(|m| {
                (
                    m.x,
                    m.y,
                    m.scale,
                    m.transform,
                    m.primary,
                    vec![(m.connector.clone(), m.mode_id.clone(), HashMap::new())],
                )
            })
            .collect();
        let props: HashMap<String, Variant<'_>> = HashMap::new();
        proxy
            .call::<_, _, ()>(
                "ApplyMonitorsConfig",
                &(state.0, METHOD_TEMPORARY, logical, props),
            )
            .await
            .map(|()| true)
            .map_err(|e| format!("ApplyMonitorsConfig: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(connector: &str, x: i32, y: i32, scale: f64) -> MonitorState {
        MonitorState {
            connector: connector.into(),
            mode_id: format!("{connector}-mode"),
            x,
            y,
            scale,
            transform: 0,
            primary: connector == "Meta-0",
            placed: true,
        }
    }

    fn want(list: &[(&str, i32, i32)]) -> Vec<(String, i32, i32)> {
        list.iter().map(|(c, x, y)| ((*c).into(), *x, *y)).collect()
    }

    fn places(cfg: &[MonitorState]) -> Vec<(String, i32, i32)> {
        cfg.iter()
            .map(|m| (m.connector.clone(), m.x, m.y))
            .collect()
    }

    #[test]
    fn config_stacks_above() {
        let cur = [mon("Meta-0", 0, 0, 1.0), mon("Meta-1", 2556, 0, 1.0)];
        let cfg = layout_config(&cur, &want(&[("Meta-0", 0, 1318), ("Meta-1", 0, 0)]));
        assert_eq!(places(&cfg), want(&[("Meta-0", 0, 1318), ("Meta-1", 0, 0)]));
    }

    /// A client layout spanning more than `i32` saturates rather than overflowing the shift to
    /// the origin.
    #[test]
    fn a_layout_wider_than_i32_saturates() {
        let cur = [mon("Meta-0", 0, 0, 1.0), mon("Meta-1", 2556, 0, 1.0)];
        let cfg = layout_config(
            &cur,
            &want(&[("Meta-0", i32::MIN, 0), ("Meta-1", i32::MAX, 0)]),
        );
        assert_eq!(
            places(&cfg),
            want(&[("Meta-0", 0, 0), ("Meta-1", i32::MAX, 0)])
        );
    }

    #[test]
    fn config_normalizes_origin() {
        let cur = [mon("Meta-0", 0, 0, 1.0), mon("Meta-1", 0, 0, 1.0)];
        let cfg = layout_config(&cur, &want(&[("Meta-0", 100, 50), ("Meta-1", 2656, 50)]));
        assert_eq!(places(&cfg), want(&[("Meta-0", 0, 0), ("Meta-1", 2556, 0)]));
    }

    #[test]
    fn config_divides_by_scale() {
        let cur = [mon("Meta-0", 0, 0, 2.0), mon("Meta-1", 1278, 0, 2.0)];
        let cfg = layout_config(&cur, &want(&[("Meta-0", 0, 1318), ("Meta-1", 0, 0)]));
        assert_eq!(places(&cfg), want(&[("Meta-0", 0, 659), ("Meta-1", 0, 0)]));
    }

    #[test]
    fn config_keeps_unlisted_monitor() {
        let cur = [mon("Meta-0", 0, 0, 1.0), mon("Meta-2", 2556, 0, 1.0)];
        let cfg = layout_config(&cur, &want(&[("Meta-0", 0, 0)]));
        assert_eq!(places(&cfg), want(&[("Meta-0", 0, 0), ("Meta-2", 2556, 0)]));
    }

    #[test]
    fn an_unchanged_layout_needs_no_apply() {
        let cur = [mon("Meta-0", 0, 1318, 1.0), mon("Meta-1", 0, 0, 1.0)];
        let cfg = layout_config(&cur, &want(&[("Meta-0", 0, 1318), ("Meta-1", 0, 0)]));
        assert!(!needs_apply(&cur, &cfg));
        let moved = layout_config(&cur, &want(&[("Meta-0", 0, 0), ("Meta-1", 2556, 0)]));
        assert!(needs_apply(&cur, &moved));
    }

    /// A grown monitor outside the layout is placed even where the position made up for it
    /// (right of the others) is the one wanted, and once placed there needs nothing more; a
    /// monitor outside the layout that the request does not name stays out.
    #[test]
    fn an_unplaced_monitor_is_placed_once() {
        let unplaced = |connector: &str, x: i32| MonitorState {
            placed: false,
            ..mon(connector, x, 0, 1.0)
        };
        let cur = [
            mon("Meta-0", 0, 0, 1.0),
            unplaced("Meta-1", 2556),
            unplaced("DP-1", 5112),
        ];
        let side = want(&[("Meta-0", 0, 0), ("Meta-1", 2556, 0)]);
        let cfg = layout_config(&cur, &side);
        assert_eq!(places(&cfg), side);
        assert!(needs_apply(&cur, &cfg));
        let now = [mon("Meta-0", 0, 0, 1.0), mon("Meta-1", 2556, 0, 1.0)];
        assert!(!needs_apply(&now, &layout_config(&now, &side)));
    }

    /// One display has nothing to arrange: a host's real monitors (and its primary) stay
    /// as the user set them.
    #[test]
    fn a_single_display_leaves_the_host_alone() {
        assert!(!worth_arranging(&want(&[("Meta-0", 0, 0)])));
        assert!(worth_arranging(&want(&[
            ("Meta-0", 0, 1318),
            ("Meta-1", 0, 0)
        ])));
    }

    #[test]
    fn config_primary_is_rank_zero() {
        let cur = [mon("Meta-0", 0, 0, 1.0), mon("Meta-1", 2556, 0, 1.0)];
        let cfg = layout_config(&cur, &want(&[("Meta-1", 0, 0), ("Meta-0", 0, 1318)]));
        let primary: Vec<_> = cfg
            .iter()
            .filter(|m| m.primary)
            .map(|m| m.connector.as_str())
            .collect();
        assert_eq!(primary, ["Meta-1"]);
    }
}
