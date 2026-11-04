/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What GPU a session has here, for whatever brings one up: the `selkies gpu-probe` command.
//!
//! A Wayland compositor needs a working GBM/EGL stack on a DRM render node, and device paths
//! cannot answer whether that stack works: a node can exist with nothing usable behind it,
//! and a driver can be installed with no node at all. So the answer comes from pixelflux
//! running the compositor's own renderer bring-up ([`pixelflux::probe_wayland_gpu`]). That
//! bring-up also resolves `render_dri` and the `auto_gpu` token exactly as the capture will,
//! so the GPU it reports is the one the session uses.
//!
//! The command prints what it found as `KEY=VALUE` lines on stdout, for a caller to export
//! and act on, and one line saying it in words on stderr:
//!
//! - `SELKIES_GPU_RENDER_NODE`: the node the session renders on, empty for none
//! - `SELKIES_GPU_DRIVER`: the driver behind it, empty when no GPU backs it
//! - `SELKIES_GPU_PRESENT`: whether a GPU is here at all
//! - `SELKIES_GPU_ACCELERATED`: whether the compositor's renderer came up on it
//! - `SELKIES_GPU_GL_VENDOR`: the glvnd vendor library whose EGL brings that GPU up
//! - `SELKIES_GPU_EGL_X11`: the first glvnd vendor whose EGL initializes on an X display
//! - `SELKIES_GPU_MESA_DRIVER`: how Mesa reaches the GPU: `native`, `zink`, or "" for software
//! - `SELKIES_GPU_VULKAN_PRESENTS`: whether the GPU's Vulkan driver gave an X window a
//!   swapchain, when Zink is Mesa's path
//!
//! The client-side facts come from bringing each vendor's EGL up the way an application
//! would: on the render node through the device platform, and on a throwaway X server for
//! the window paths, one drawing on the node through glamor and DRI3 where the session's
//! will, so a driver that renders but cannot present is reported as such rather than guessed
//! from device files. Each of those bring-ups runs in a child process of this same binary
//! (`selkies gpu-probe --renderer <node>`, `--window`, `--vulkan-window <node>`), since a
//! driver may crash in the attempt; the helpers live in `gpu_probe_helpers`.
//!
//! What to do about that, which display backend to start, which GL stack the applications
//! get, is the deployment's policy, not this module's. The command exits non-zero without
//! printing anything when no report can be had.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

/// The compositor bring-up probe this reads; Linux's, where the Wayland compositor
/// renders through GBM and GL.
#[cfg(target_os = "linux")]
use pixelflux::WaylandGpuProbe;
use regex::Regex;

#[cfg(target_os = "linux")]
use super::gpu_probe_helpers::{
    RealProbes, print_device_renderer, print_vulkan_window, print_window_test,
};

/// The proprietary NVIDIA stack, which no Mesa driver covers.
pub const NVIDIA_DRIVER: &str = "nvidia";
/// glvnd's EGL vendor ICD directories, the same on every distribution that packages glvnd;
/// `__EGL_VENDOR_LIBRARY_DIRS` is glvnd's own override of them.
pub const EGL_VENDOR_DIRS: [&str; 2] = ["/etc/glvnd/egl_vendor.d", "/usr/share/glvnd/egl_vendor.d"];
pub const MESA_VENDOR: &str = "mesa";
/// The client-side environment the probes must not inherit: what the caller may already
/// have decided about Mesa and glvnd is exactly what is measured.
pub const CLIENT_ENV: [&str; 7] = [
    "MESA_LOADER_DRIVER_OVERRIDE",
    "GALLIUM_DRIVER",
    "LIBGL_KOPPER_DRI2",
    "LIBGL_ALWAYS_SOFTWARE",
    "MESA_VK_DEVICE_SELECT",
    "__GLX_VENDOR_LIBRARY_NAME",
    "__EGL_VENDOR_LIBRARY_FILENAMES",
];
/// The environment that makes Mesa reach the GPU through Zink.
pub const ZINK_ENV: [(&str, &str); 3] = [
    ("MESA_LOADER_DRIVER_OVERRIDE", "zink"),
    ("GALLIUM_DRIVER", "zink"),
    ("LIBGL_KOPPER_DRI2", "1"),
];
/// How long one helper or the throwaway X server may take.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const SYSFS_DRM: &str = "/sys/class/drm";
/// The proprietary stack's device node, which the sysfs walk below asks after.
#[cfg(target_os = "linux")]
const NVIDIA_DEVICE: &str = "/dev/nvidiactl";

/// The variables the probe exports, in the order they are printed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts(Vec<(&'static str, String)>);

impl Facts {
    pub fn get(&self, key: &str) -> &str {
        self.0
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }

    /// Set `key`, keeping its position where it is already listed.
    pub fn set(&mut self, key: &'static str, value: impl Into<String>) {
        let value = value.into();
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = value,
            None => self.0.push((key, value)),
        }
    }

    pub fn keys(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.0.iter().map(|(k, _)| *k)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &str)> + '_ {
        self.0.iter().map(|(k, v)| (*k, v.as_str()))
    }

    pub fn extend(&mut self, other: Facts) {
        for (key, value) in other.0 {
            self.set(key, value);
        }
    }
}

/// The kernel driver behind a DRM render node: `nvidia`, `i915`, `xe`, `amdgpu`, ..., or
/// "" when the node has no identity under `sysfs` (a system without `/sys` among others).
pub fn render_node_driver(node: &str, sysfs: &Path) -> String {
    if node.is_empty() {
        return String::new();
    }
    let Some(name) = Path::new(node).file_name() else {
        return String::new();
    };
    let link = sysfs.join(name).join("device").join("driver");
    if !std::fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()) {
        return String::new();
    }
    let driver = std::fs::canonicalize(&link)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    // The DRM node of the proprietary stack answers `nvidia` or `nvidia-drm` depending on
    // the kernel; nouveau is a different driver and stays itself.
    if driver.starts_with(NVIDIA_DRIVER) {
        NVIDIA_DRIVER.to_string()
    } else {
        driver
    }
}

/// The driver of the GPU the session renders on, or "" when no GPU backs it.
///
/// A render node names its own driver. With no node the GPU can still be reachable (the
/// NVIDIA driver and its Vulkan ICD work without a DRM node), so the driver device answers
/// for that case alone.
pub fn session_gpu(node: &str, sysfs: &Path, nvidia_device: &Path) -> String {
    let driver = render_node_driver(node, sysfs);
    if !driver.is_empty() {
        return driver;
    }
    if nvidia_device.exists() {
        NVIDIA_DRIVER.to_string()
    } else {
        String::new()
    }
}

/// The GPU a pixelflux probe report describes, as the variables a caller exports.
/// Linux alone: the report it reads is of the compositor's GBM/GL bring-up.
#[cfg(target_os = "linux")]
pub fn facts(report: &WaylandGpuProbe) -> Facts {
    // The report echoes the node it was asked for, which a failed bring-up may say is not
    // there at all; that is no node to render on.
    let node = if !report.node.is_empty() && !Path::new(&report.node).exists() {
        String::new()
    } else {
        report.node.clone()
    };
    let mut found = Facts::default();
    found.set(
        "SELKIES_GPU_DRIVER",
        session_gpu(&node, Path::new(SYSFS_DRM), Path::new(NVIDIA_DEVICE)),
    );
    found.0.insert(0, ("SELKIES_GPU_RENDER_NODE", node));
    found.set(
        "SELKIES_GPU_PRESENT",
        if report.gpu { "true" } else { "false" },
    );
    found.set(
        "SELKIES_GPU_ACCELERATED",
        if report.accelerated { "true" } else { "false" },
    );
    found
}

/// One line describing `found`, for a log, with the report's renderer name and error.
/// Linux alone, with the report `facts` reads.
#[cfg(target_os = "linux")]
pub fn summary(report: &WaylandGpuProbe, found: &Facts) -> String {
    let node = found.get("SELKIES_GPU_RENDER_NODE");
    let where_ = if node.is_empty() {
        String::new()
    } else {
        format!(" on {node}")
    };
    if found.get("SELKIES_GPU_ACCELERATED") == "true" {
        let name = if report.renderer.is_empty() {
            found.get("SELKIES_GPU_DRIVER")
        } else {
            &report.renderer
        };
        return format!("GPU: {name}{where_}");
    }
    let why = if report.error.is_empty() {
        "no hardware acceleration"
    } else {
        &report.error
    };
    if found.get("SELKIES_GPU_PRESENT") == "true" {
        let driver = found.get("SELKIES_GPU_DRIVER");
        let driver = if driver.is_empty() { "unknown" } else { driver };
        return format!("GPU: {driver}{where_}, unusable: {why}");
    }
    "GPU: none here".to_string()
}

static EGL_LIBRARY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^libEGL_(\w+)\.so").expect("a valid pattern"));

/// glvnd's EGL vendors, in the order glvnd tries them: `(name, icd_path)` pairs, `name`
/// being what `__GLX_VENDOR_LIBRARY_NAME` and `__EGL_VENDOR_LIBRARY_FILENAMES` know the
/// vendor as; one per name. `dirs` are the ICD directories read when `environ` names none in
/// `__EGL_VENDOR_LIBRARY_DIRS`.
pub fn egl_vendors(
    dirs: &[PathBuf],
    environ: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let listed = environ("__EGL_VENDOR_LIBRARY_DIRS").unwrap_or_default();
    let directories: Vec<PathBuf> = if listed.is_empty() {
        dirs.to_vec()
    } else {
        listed.split(':').map(PathBuf::from).collect()
    };
    let mut found: Vec<(String, String)> = Vec::new();
    for directory in directories {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names.into_iter().filter(|n| n.ends_with(".json")) {
            let path = directory.join(&name);
            let library = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .and_then(|icd| {
                    icd.get("ICD")?
                        .get("library_path")?
                        .as_str()
                        .map(str::to_string)
                });
            let Some(library) = library else {
                continue;
            };
            let base = Path::new(&library)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let Some(vendor) = EGL_LIBRARY.captures(&base).and_then(|c| c.get(1))
                && !found.iter().any(|(v, _)| v == vendor.as_str())
            {
                found.push((
                    vendor.as_str().to_string(),
                    path.to_string_lossy().into_owned(),
                ));
            }
        }
    }
    found
}

/// Whether a `GL_RENDERER` string names a software rasterizer.
pub fn software_renderer(renderer: &str) -> bool {
    let lowered = renderer.to_lowercase();
    ["llvmpipe", "softpipe", "swr"]
        .iter()
        .any(|p| lowered.starts_with(p))
        || lowered.contains("software")
}

/// The `MESA_VK_DEVICE_SELECT` value naming a render node's GPU (`vendor:device`), or "".
pub fn vk_device_select(node: &str, sysfs: &Path) -> String {
    if node.is_empty() {
        return String::new();
    }
    let Some(name) = Path::new(node).file_name() else {
        return String::new();
    };
    let mut ids = Vec::new();
    for file in ["vendor", "device"] {
        let Ok(text) = std::fs::read_to_string(sysfs.join(name).join("device").join(file)) else {
            return String::new();
        };
        ids.push(
            text.trim()
                .to_lowercase()
                .trim_start_matches("0x")
                .to_string(),
        );
    }
    ids.join(":")
}

/// An X server of the probe's own for the window paths, ended when dropped.
pub trait ThrowawayDisplay {
    /// The display name (`:N`), or "" where no server could be run.
    fn name(&self) -> &str;
}

/// The measurements behind the client facts: a test injects its own.
pub trait ClientProbes {
    /// `GL_RENDERER` of a vendor's EGL brought up on `node` through the device platform, or
    /// "".
    fn renderer(&self, node: &str, icd: &str, extra: Option<&HashMap<String, String>>) -> String;
    /// A vendor's EGL on an X display: the renderer it gives a window, and whether it
    /// presented.
    fn window(
        &self,
        display: &str,
        icd: &str,
        extra: Option<&HashMap<String, String>>,
    ) -> (String, bool);
    /// A throwaway X server drawing on `node` through glamor and DRI3, the way the session's
    /// framebuffer server does; "" for a software server.
    fn display(&self, node: &str) -> Box<dyn ThrowawayDisplay>;
    /// Whether the GPU's Vulkan driver gives a window on `display` a swapchain; `None` when
    /// unanswerable.
    fn vulkan(&self, display: &str, node: &str) -> Option<bool>;
}

/// How the session's applications reach the GPU `found` describes: the client-side
/// variables, all present and all strings. `vendors` is the glvnd list, read from the host
/// where `None`.
pub fn client_facts(
    found: &Facts,
    probes: &dyn ClientProbes,
    vendors: Option<&[(String, String)]>,
) -> Facts {
    let mut out = Facts::default();
    for key in [
        "SELKIES_GPU_GL_VENDOR",
        "SELKIES_GPU_EGL_X11",
        "SELKIES_GPU_MESA_DRIVER",
        "SELKIES_GPU_VULKAN_PRESENTS",
    ] {
        out.set(key, "");
    }
    let node = found.get("SELKIES_GPU_RENDER_NODE");
    if found.get("SELKIES_GPU_DRIVER").is_empty() {
        return out;
    }
    let host_vendors;
    let vendors = match vendors {
        Some(list) => list,
        None => {
            host_vendors = egl_vendors(&EGL_VENDOR_DIRS.map(PathBuf::from), &|k| {
                std::env::var(k).ok()
            });
            &host_vendors
        }
    };
    for (name, icd) in vendors {
        let seen = probes.renderer(node, icd, None);
        if !seen.is_empty() && !software_renderer(&seen) {
            out.set("SELKIES_GPU_GL_VENDOR", name.clone());
            break;
        }
    }
    let mesa = vendors
        .iter()
        .find(|(n, _)| n == MESA_VENDOR)
        .map(|(_, icd)| icd.clone())
        .unwrap_or_default();
    if out.get("SELKIES_GPU_GL_VENDOR") == MESA_VENDOR {
        out.set("SELKIES_GPU_MESA_DRIVER", "native");
    }
    // The window paths are measured on a server like the session's: glamor on the node where
    // the compositor's renderer came up on it, software otherwise.
    let display = probes.display(if found.get("SELKIES_GPU_ACCELERATED") == "true" {
        node
    } else {
        ""
    });
    let x_display = display.name().to_string();
    if x_display.is_empty() {
        return out;
    }
    for (name, icd) in vendors {
        let (seen, _) = probes.window(&x_display, icd, None);
        if !seen.is_empty() {
            out.set("SELKIES_GPU_EGL_X11", name.clone());
            break;
        }
    }
    if !mesa.is_empty() && out.get("SELKIES_GPU_MESA_DRIVER") != "native" {
        let mut extra: HashMap<String, String> = ZINK_ENV
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let select_gpu = vk_device_select(node, Path::new(SYSFS_DRM));
        if !select_gpu.is_empty() {
            extra.insert("MESA_VK_DEVICE_SELECT".into(), select_gpu);
        }
        let (seen, _) = probes.window(&x_display, &mesa, Some(&extra));
        if seen.to_lowercase().starts_with("zink") {
            out.set("SELKIES_GPU_MESA_DRIVER", "zink");
            // Zink's own swap reports success whether or not the driver gave it a swapchain,
            // so the driver is asked directly.
            let presented = probes.vulkan(&x_display, node);
            out.set(
                "SELKIES_GPU_VULKAN_PRESENTS",
                match presented {
                    None => "",
                    Some(true) => "true",
                    Some(false) => "false",
                },
            );
        }
    }
    drop(display);
    out
}

/// The `selkies gpu-probe` command: with `--renderer <node>`, `--window`, or
/// `--vulkan-window <node>` one of the helpers a probe runs in a child process; otherwise
/// the probe itself, reading `render_dri` and `auto_gpu` from the installed settings and
/// printing the facts. Returns the process exit code.
#[cfg(target_os = "linux")]
pub fn main(args: &[String]) -> i32 {
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or("");
    match arg(0) {
        "--renderer" => return print_device_renderer(arg(1)),
        "--window" => return print_window_test(),
        "--vulkan-window" => return print_vulkan_window(arg(1)),
        _ => {}
    }
    let (render_dri, auto_gpu) = {
        let settings = crate::settings::settings();
        (
            settings.render_dri().to_string(),
            settings.auto_gpu().to_string(),
        )
    };
    let report = pixelflux::probe_wayland_gpu(render_dri, auto_gpu);
    let mut found = facts(&report);
    eprintln!("{}", summary(&report, &found));
    found.extend(client_facts(&found, &RealProbes, None));
    for (key, value) in found.iter() {
        println!("{key}={value}");
    }
    0
}

/// The `selkies gpu-probe` command where no compositor bring-up exists to probe: the
/// documented exit for a session that cannot have a report.
#[cfg(not(target_os = "linux"))]
pub fn main(_args: &[String]) -> i32 {
    eprintln!("No GPU probe exists on this OS: no compositor bring-up to report.");
    1
}

#[cfg(test)]
mod tests {
    //! The checks of `tests/unit/test_gpu_probe_facts.py`: the reading of the helpers'
    //! answers, which is where a wrong fact would come from: a software renderer mistaken for
    //! a vendor's, a vendor listed twice, a Zink that initializes but cannot present reported
    //! as a working path.

    use super::*;
    use std::sync::{Arc, Mutex};

    const NODE: &str = "/dev/dri/renderD128";

    fn found() -> Facts {
        let mut f = Facts::default();
        f.set("SELKIES_GPU_RENDER_NODE", NODE);
        f.set("SELKIES_GPU_DRIVER", "nvidia");
        f.set("SELKIES_GPU_PRESENT", "true");
        f.set("SELKIES_GPU_ACCELERATED", "true");
        f
    }

    fn vendors() -> Vec<(String, String)> {
        vec![
            ("nvidia".into(), "/icd/10_nvidia.json".into()),
            ("mesa".into(), "/icd/50_mesa.json".into()),
        ]
    }

    struct Display(String);

    impl ThrowawayDisplay for Display {
        fn name(&self) -> &str {
            &self.0
        }
    }

    /// What a window probe answers for an ICD, asked with or without Zink: the renderer and
    /// whether it presented.
    type WindowAnswer = ((&'static str, bool), (&'static str, bool));

    /// Stand-in helpers answering from `device` (icd to renderer) and `windows` (icd, and
    /// whether Zink was asked, to renderer and presented).
    struct Fake {
        device: HashMap<&'static str, &'static str>,
        windows: HashMap<(&'static str, bool), (&'static str, bool)>,
        vulkan: Option<bool>,
        has_display: bool,
        displays: Arc<Mutex<Vec<String>>>,
        calls: Mutex<Vec<(&'static str, String, bool)>>,
    }

    impl Fake {
        fn new(
            device: &[(&'static str, &'static str)],
            windows: &[WindowAnswer],
            vulkan: Option<bool>,
        ) -> Self {
            Self {
                device: device.iter().copied().collect(),
                windows: windows.iter().copied().collect(),
                vulkan,
                has_display: true,
                displays: Arc::new(Mutex::new(Vec::new())),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn window_calls(&self) -> Vec<(String, bool)> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.0 == "window")
                .map(|c| (c.1.clone(), c.2))
                .collect()
        }
    }

    impl ClientProbes for Fake {
        fn renderer(
            &self,
            _node: &str,
            icd: &str,
            _extra: Option<&HashMap<String, String>>,
        ) -> String {
            self.calls
                .lock()
                .unwrap()
                .push(("renderer", icd.to_string(), false));
            self.device.get(icd).unwrap_or(&"").to_string()
        }

        fn window(
            &self,
            _display: &str,
            icd: &str,
            extra: Option<&HashMap<String, String>>,
        ) -> (String, bool) {
            let zink =
                extra.is_some_and(|e| e.get("GALLIUM_DRIVER").map(String::as_str) == Some("zink"));
            self.calls
                .lock()
                .unwrap()
                .push(("window", icd.to_string(), zink));
            let (seen, presented) = self
                .windows
                .get(&(icd, zink))
                .copied()
                .unwrap_or(("", false));
            (seen.to_string(), presented)
        }

        fn display(&self, node: &str) -> Box<dyn ThrowawayDisplay> {
            self.displays.lock().unwrap().push(node.to_string());
            Box::new(Display(if self.has_display {
                ":99".into()
            } else {
                String::new()
            }))
        }

        fn vulkan(&self, _display: &str, _node: &str) -> Option<bool> {
            self.vulkan
        }
    }

    #[test]
    fn vendors_are_read_the_way_glvnd_reads_them() {
        let tmp = std::env::temp_dir().join(format!("selkies-gpu-probe-{}", std::process::id()));
        let (etc, usr) = (tmp.join("etc"), tmp.join("usr"));
        let icd = |lib: &str| {
            format!(r#"{{"file_format_version": "1.0.0", "ICD": {{"library_path": "{lib}"}}}}"#)
        };
        for (dir, files) in [
            (&etc, vec![("10_nvidia.json", icd("libEGL_nvidia.so.0"))]),
            (
                &usr,
                vec![
                    ("10_nvidia.json", icd("libEGL_nvidia.so.0")),
                    ("50_mesa.json", icd("libEGL_mesa.so.0")),
                    ("60_broken.json", "{not json".to_string()),
                    ("70_odd.json", icd("libGLES_x.so")),
                    ("README", "x".to_string()),
                ],
            ),
        ] {
            std::fs::create_dir_all(dir).unwrap();
            for (name, text) in files {
                std::fs::write(dir.join(name), text).unwrap();
            }
        }
        let none = |_: &str| None;
        let listed = egl_vendors(&[etc.clone(), usr.clone()], &none);
        assert_eq!(
            listed.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["nvidia", "mesa"],
            "vendors are read from the ICD directories in glvnd's order, one per name"
        );
        assert!(
            listed[0].1.starts_with(etc.to_str().unwrap()),
            "the first directory naming a vendor is the one kept"
        );
        assert!(
            !listed.iter().any(|(n, _)| n == "odd" || n == "broken"),
            "unreadable and non-EGL ICDs are skipped"
        );
        let usr_text = usr.to_string_lossy().into_owned();
        let overridden = egl_vendors(&[etc.clone(), usr.clone()], &|k| {
            (k == "__EGL_VENDOR_LIBRARY_DIRS").then(|| usr_text.clone())
        });
        assert!(
            overridden[0].1.starts_with(&usr_text),
            "__EGL_VENDOR_LIBRARY_DIRS overrides the directories, as it does for glvnd"
        );
        assert!(
            egl_vendors(&[tmp.join("none")], &none).is_empty(),
            "no directory at all is an empty list"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn software_rasterizers_are_told_from_a_gpus_renderer() {
        assert!(
            software_renderer("llvmpipe (LLVM 21.1.8, 256 bits)") && software_renderer("softpipe")
        );
        assert!(!software_renderer("Tesla P100-SXM2-16GB/PCIe/SSE2"));
        assert!(!software_renderer(
            "zink Vulkan 1.4(Tesla P100 (NVIDIA_PROPRIETARY))"
        ));
    }

    #[test]
    fn a_nodes_gpu_is_named_for_mesas_device_selection() {
        let fake = std::env::temp_dir().join(format!("selkies-vk-select-{}", std::process::id()));
        let device = fake.join("renderD128").join("device");
        std::fs::create_dir_all(&device).unwrap();
        std::fs::write(device.join("vendor"), "0x10de\n").unwrap();
        std::fs::write(device.join("device"), "0x15f8\n").unwrap();
        assert_eq!(vk_device_select(NODE, &fake), "10de:15f8");
        assert_eq!(
            vk_device_select("/dev/dri/renderD129", &fake),
            "",
            "a node with no identity names none"
        );
        assert_eq!(vk_device_select("", &fake), "");
        assert_eq!(
            render_node_driver(NODE, &fake),
            "",
            "a node whose device names no driver has none"
        );
        let _ = std::fs::remove_dir_all(&fake);
    }

    #[test]
    fn the_facts_from_stand_in_answers() {
        let keys = [
            "SELKIES_GPU_GL_VENDOR",
            "SELKIES_GPU_EGL_X11",
            "SELKIES_GPU_MESA_DRIVER",
            "SELKIES_GPU_VULKAN_PRESENTS",
        ];
        let fake = Fake::new(
            &[
                ("/icd/10_nvidia.json", "Tesla P100-SXM2-16GB/PCIe/SSE2"),
                ("/icd/50_mesa.json", "llvmpipe (LLVM 21.1.8, 256 bits)"),
            ],
            &[
                (
                    ("/icd/50_mesa.json", false),
                    ("llvmpipe (LLVM 21.1.8, 256 bits)", true),
                ),
                (
                    ("/icd/50_mesa.json", true),
                    ("zink Vulkan 1.4(Tesla P100 (NVIDIA_PROPRIETARY))", true),
                ),
            ],
            Some(true),
        );
        let got = client_facts(&found(), &fake, Some(&vendors()));
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            keys.to_vec(),
            "every client fact is reported"
        );
        assert_eq!(
            fake.displays.lock().unwrap().last().map(String::as_str),
            Some(NODE),
            "the window probes run on a server drawing on the node the renderer came up on"
        );
        assert_eq!(
            got.get("SELKIES_GPU_GL_VENDOR"),
            "nvidia",
            "the vendor whose EGL renders on the node is the GL vendor"
        );
        assert_eq!(
            got.get("SELKIES_GPU_EGL_X11"),
            "mesa",
            "a vendor that cannot initialize on X leaves EGL there to the next one"
        );
        assert!(
            got.get("SELKIES_GPU_MESA_DRIVER") == "zink"
                && got.get("SELKIES_GPU_VULKAN_PRESENTS") == "true",
            "Mesa answering with Zink and the driver giving a window a swapchain is the Zink path: {got:?}"
        );
        assert!(
            fake.window_calls().iter().any(|(_, zink)| *zink),
            "the Zink window is asked for on the node's own GPU"
        );

        let fake = Fake::new(
            &[
                ("/icd/10_nvidia.json", "Tesla P100-SXM2-16GB/PCIe/SSE2"),
                ("/icd/50_mesa.json", ""),
            ],
            &[
                (
                    ("/icd/10_nvidia.json", false),
                    ("Tesla P100-SXM2-16GB/PCIe/SSE2", true),
                ),
                (("/icd/50_mesa.json", false), ("llvmpipe", true)),
                (
                    ("/icd/50_mesa.json", true),
                    ("zink Vulkan 1.4(Tesla P100 (NVIDIA_PROPRIETARY))", true),
                ),
            ],
            Some(false),
        );
        let got = client_facts(&found(), &fake, Some(&vendors()));
        assert!(
            got.get("SELKIES_GPU_MESA_DRIVER") == "zink"
                && got.get("SELKIES_GPU_VULKAN_PRESENTS") == "false",
            "a Zink that initializes while the driver refuses a swapchain is reported as exactly that: {got:?}"
        );
        assert_eq!(
            got.get("SELKIES_GPU_EGL_X11"),
            "nvidia",
            "a vendor whose EGL initializes on X is EGL's vendor there"
        );

        let fake = Fake::new(
            &[
                ("/icd/10_nvidia.json", ""),
                ("/icd/50_mesa.json", "AMD Radeon RX 7900 XTX (radeonsi)"),
            ],
            &[(
                ("/icd/50_mesa.json", false),
                ("AMD Radeon RX 7900 XTX (radeonsi)", true),
            )],
            None,
        );
        let mut amd = found();
        amd.set("SELKIES_GPU_DRIVER", "amdgpu");
        let got = client_facts(&amd, &fake, Some(&vendors()));
        assert!(
            got.get("SELKIES_GPU_GL_VENDOR") == "mesa"
                && got.get("SELKIES_GPU_MESA_DRIVER") == "native"
                && got.get("SELKIES_GPU_VULKAN_PRESENTS").is_empty(),
            "a GPU Mesa drives itself is Mesa's vendor on its native driver, and Zink is never asked: {got:?}"
        );
        assert!(!fake.window_calls().iter().any(|(_, zink)| *zink));

        let fake = Fake::new(
            &[
                ("/icd/10_nvidia.json", ""),
                ("/icd/50_mesa.json", "llvmpipe (LLVM 21.1.8, 256 bits)"),
            ],
            &[
                (
                    ("/icd/50_mesa.json", false),
                    ("llvmpipe (LLVM 21.1.8, 256 bits)", true),
                ),
                (
                    ("/icd/50_mesa.json", true),
                    ("llvmpipe (LLVM 21.1.8, 256 bits)", true),
                ),
            ],
            Some(true),
        );
        let got = client_facts(&found(), &fake, Some(&vendors()));
        assert!(
            got.get("SELKIES_GPU_GL_VENDOR").is_empty()
                && got.get("SELKIES_GPU_MESA_DRIVER").is_empty(),
            "no vendor rendering on the node and no Zink is software all round: {got:?}"
        );

        let mut fake = Fake::new(
            &[("/icd/10_nvidia.json", "Tesla P100-SXM2-16GB/PCIe/SSE2")],
            &[],
            Some(true),
        );
        fake.has_display = false;
        let got = client_facts(&found(), &fake, Some(&vendors()));
        assert!(
            got.get("SELKIES_GPU_GL_VENDOR") == "nvidia"
                && got.get("SELKIES_GPU_EGL_X11").is_empty()
                && got.get("SELKIES_GPU_MESA_DRIVER").is_empty()
                && fake.window_calls().is_empty(),
            "without an X server to run, the window facts stay unanswered and the node's vendor still answers: {got:?}"
        );

        let mut none = found();
        none.set("SELKIES_GPU_DRIVER", "");
        none.set("SELKIES_GPU_RENDER_NODE", "");
        let got = client_facts(&none, &fake, Some(&vendors()));
        assert!(
            got.iter().all(|(_, v)| v.is_empty())
                && got.keys().collect::<Vec<_>>() == keys.to_vec(),
            "no GPU asks nothing and answers empty: {got:?}"
        );

        let fake = Fake::new(
            &[("/icd/10_nvidia.json", "Tesla P100-SXM2-16GB/PCIe/SSE2")],
            &[(
                ("/icd/50_mesa.json", true),
                ("zink Vulkan 1.4(Tesla P100 (NVIDIA_PROPRIETARY))", true),
            )],
            None,
        );
        let mut software = found();
        software.set("SELKIES_GPU_ACCELERATED", "false");
        let unanswered = client_facts(&software, &fake, Some(&vendors()));
        assert!(
            unanswered.get("SELKIES_GPU_MESA_DRIVER") == "zink"
                && unanswered.get("SELKIES_GPU_VULKAN_PRESENTS").is_empty()
                && fake.displays.lock().unwrap().last().map(String::as_str) == Some(""),
            "a driver that could not be asked leaves the presentation fact unanswered, on a software server: {unanswered:?}"
        );
    }

    #[test]
    fn the_summary_says_what_was_found() {
        let report = WaylandGpuProbe {
            node: String::new(),
            accelerated: false,
            gpu: false,
            renderer: String::new(),
            error: "No render node".into(),
        };
        let f = facts(&report);
        assert_eq!(f.get("SELKIES_GPU_PRESENT"), "false");
        assert_eq!(
            f.keys().collect::<Vec<_>>(),
            vec![
                "SELKIES_GPU_RENDER_NODE",
                "SELKIES_GPU_DRIVER",
                "SELKIES_GPU_PRESENT",
                "SELKIES_GPU_ACCELERATED"
            ]
        );
        assert_eq!(summary(&report, &f), "GPU: none here");
        let mut present = f.clone();
        present.set("SELKIES_GPU_PRESENT", "true");
        assert_eq!(
            summary(&report, &present),
            "GPU: unknown, unusable: No render node"
        );
        present.set("SELKIES_GPU_ACCELERATED", "true");
        present.set("SELKIES_GPU_DRIVER", "i915");
        present.set("SELKIES_GPU_RENDER_NODE", NODE);
        assert_eq!(summary(&report, &present), format!("GPU: i915 on {NODE}"));
    }
}
