/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The GPU a session's processes get, settled by the launcher before anything of the session
//! starts.
//!
//! Three steps, in this order, because each needs the one before it:
//!
//! 1. **The driver's files.** A container runtime can pass in the NVIDIA driver's libraries
//!    without the loader files that point the vendor-neutral loaders at them: the glvnd EGL
//!    vendor file, the Vulkan ICD file, the OpenCL ICD, and the GBM backend under the
//!    directory libgbm loads backends from. Without them EGL, Vulkan, and GBM never reach the
//!    GPU, the probe below included. The files are written into the session's runtime directory
//!    and the loaders are pointed at it through their environment
//!    ([`driver_files`]), never into `/etc` or `/usr`, so nothing of this needs root and a host
//!    whose files are in place is left as it is: a file is written only where the system has
//!    none and the library it names is installed.
//! 2. **The probe.** `selkies gpu-probe` ([`crate::stats::gpu_probe`]) brings the compositor's
//!    renderer and each vendor's EGL up the way the session will, in child processes of its own
//!    since a driver may crash or wedge in the attempt, and reports what came up ([`probe`]).
//! 3. **The policy** ([`plan`]): GLX goes to the vendor library that renders on the node the X
//!    server's glamor draws on, Mesa reaches a GPU it has no driver of its own for through Zink
//!    where the Vulkan driver can present into a window, and a Wayland session whose compositor
//!    cannot reach a GPU that is here runs on X11 instead, where the session still gets it, unless
//!    `SELKIES_WAYLAND_X11_FALLBACK=false` keeps Wayland in software. `DISABLE_ZINK=true` keeps
//!    Zink out. The facts are exported as the probe names them, for what reads them later.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::session::Env;
use crate::stats::gpu_probe::render_node_driver;

/// Where the kernel lists the DRM nodes with their devices.
const SYSFS_DRM: &str = "/sys/class/drm";
/// What the NVIDIA kernel driver publishes once it is loaded.
const NVIDIA_PROC: &str = "/proc/driver/nvidia/version";
/// How long the probe may take before it is taken for wedged.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Whether a value of a variable reads as true the way the server's settings read one: "true"
/// or "1" before any `|` suffix, in any case.
pub fn is_true(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        let v = v.split('|').next().unwrap_or("").trim();
        v == "1" || v.eq_ignore_ascii_case("true")
    })
}

/// Whether the NVIDIA driver drives a GPU here: its kernel module is loaded, or a render node
/// answers to it.
pub fn nvidia_present(sysfs: &Path, proc_version: &Path) -> bool {
    if proc_version.exists() {
        return true;
    }
    let Ok(entries) = fs::read_dir(sysfs) else {
        return false;
    };
    entries.flatten().any(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        name.starts_with("renderD")
            && render_node_driver(&format!("/dev/dri/{name}"), sysfs) == "nvidia"
    })
}

/// The directories the dynamic loader searches by default for this machine's libraries, the
/// multiarch one first.
fn library_dirs() -> Vec<PathBuf> {
    let triplet = format!("{}-linux-gnu", std::env::consts::ARCH);
    [
        format!("/usr/lib/{triplet}"),
        format!("/lib/{triplet}"),
        "/usr/lib64".to_string(),
        "/lib64".to_string(),
        "/usr/lib".to_string(),
        "/lib".to_string(),
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

/// The installed library `name` (a soname such as `libEGL_nvidia.so.0`): the loader's cache as
/// `ldconfig -p` lists it, else the default directories, since a runtime that bind-mounts a
/// driver's libraries need not refresh the cache.
pub fn find_library(name: &str) -> Option<PathBuf> {
    for ldconfig in ["/sbin/ldconfig", "/usr/sbin/ldconfig", "ldconfig"] {
        let Ok(out) = Command::new(ldconfig)
            .arg("-p")
            .stderr(Stdio::null())
            .output()
        else {
            continue;
        };
        let listed = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| line.trim_start().starts_with(&format!("{name} ")))
            .filter_map(|line| {
                line.rsplit_once("=> ")
                    .map(|(_, p)| PathBuf::from(p.trim()))
            })
            .find(|p| p.exists());
        if listed.is_some() {
            return listed;
        }
        break;
    }
    library_dirs()
        .into_iter()
        .map(|d| d.join(name))
        .find(|p| p.exists())
}

/// Whether any `*.json` or `*.icd` file in `dirs` names `needle` in its file name.
fn has_file_naming(dirs: &[PathBuf], needle: &str) -> bool {
    dirs.iter().any(|dir| {
        fs::read_dir(dir).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().contains(needle))
        })
    })
}

/// The Vulkan API version the installed loader carries, read from the file name its soname
/// resolves to (`libvulkan.so.1.4.313`), which an ICD file declares as its own; 1.1.0, the
/// lowest that lets the loader take the driver's library, where it cannot be read.
fn vulkan_api_version() -> String {
    find_library("libvulkan.so.1")
        .and_then(|p| fs::canonicalize(p).ok())
        .and_then(|p| {
            let name = p.file_name()?.to_string_lossy().into_owned();
            let version = name.strip_prefix("libvulkan.so.")?.to_string();
            (version.split('.').count() == 3
                && version.split('.').all(|n| n.parse::<u32>().is_ok()))
            .then_some(version)
        })
        .unwrap_or_else(|| "1.1.0".to_string())
}

/// What libgbm searches for backends where `GBM_BACKENDS_PATH` is unset: its own directory's
/// `gbm`, for every library directory that has one.
fn gbm_backend_dirs() -> Vec<PathBuf> {
    library_dirs()
        .into_iter()
        .map(|d| d.join("gbm"))
        .filter(|d| d.is_dir())
        .collect()
}

/// A colon-separated list with `first` ahead of the existing `rest`, each once.
fn prepend_path(first: &Path, rest: impl IntoIterator<Item = String>) -> String {
    let mut out = vec![first.to_string_lossy().into_owned()];
    for entry in rest {
        if !entry.is_empty() && !out.contains(&entry) {
            out.push(entry);
        }
    }
    out.join(":")
}

/// The loader files the NVIDIA driver's libraries need and the system lacks, written under
/// `<runtime_dir>/gpu`, as the variables that point each loader at them. Empty where the NVIDIA
/// driver drives no GPU here, or where everything is already in place. `get` reads the
/// launcher's environment, whose own settings of these variables are kept behind ours.
pub fn driver_files(runtime_dir: &Path, get: impl Fn(&str) -> Option<String>) -> Env {
    let mut env = Env::new();
    if !nvidia_present(Path::new(SYSFS_DRM), Path::new(NVIDIA_PROC)) {
        return env;
    }
    let root = runtime_dir.join("gpu");
    let write = |path: &Path, text: &str| -> bool {
        path.parent().is_some_and(|p| fs::create_dir_all(p).is_ok())
            && fs::write(path, text).is_ok()
    };

    let egl_dirs = [
        PathBuf::from("/etc/glvnd/egl_vendor.d"),
        PathBuf::from("/usr/share/glvnd/egl_vendor.d"),
    ];
    if !has_file_naming(&egl_dirs, "nvidia") && find_library("libEGL_nvidia.so.0").is_some() {
        let dir = root.join("egl_vendor.d");
        let file = "{\n    \"file_format_version\" : \"1.0.0\",\n    \"ICD\": {\n        \"library_path\": \"libEGL_nvidia.so.0\"\n    }\n}\n";
        if write(&dir.join("10_nvidia.json"), file) {
            let rest = get("__EGL_VENDOR_LIBRARY_DIRS")
                .map(|v| v.split(':').map(str::to_string).collect::<Vec<_>>())
                .unwrap_or_else(|| {
                    egl_dirs
                        .iter()
                        .map(|d| d.to_string_lossy().into_owned())
                        .collect()
                });
            env.insert("__EGL_VENDOR_LIBRARY_DIRS".into(), prepend_path(&dir, rest));
        }
    }

    let vulkan_dirs = [
        PathBuf::from("/etc/vulkan/icd.d"),
        PathBuf::from("/usr/share/vulkan/icd.d"),
    ];
    if !has_file_naming(&vulkan_dirs, "nvidia") && find_library("libGLX_nvidia.so.0").is_some() {
        let path = root.join("vulkan/nvidia_icd.json");
        let file = format!(
            "{{\n    \"file_format_version\" : \"1.0.0\",\n    \"ICD\": {{\n        \"library_path\": \"libGLX_nvidia.so.0\",\n        \"api_version\" : \"{}\"\n    }}\n}}\n",
            vulkan_api_version()
        );
        if write(&path, &file) {
            let rest = get("VK_ADD_DRIVER_FILES")
                .map(|v| v.split(':').map(str::to_string).collect::<Vec<_>>())
                .unwrap_or_default();
            env.insert("VK_ADD_DRIVER_FILES".into(), prepend_path(&path, rest));
        }
    }

    let opencl = PathBuf::from("/etc/OpenCL/vendors");
    if !has_file_naming(std::slice::from_ref(&opencl), "nvidia")
        && find_library("libnvidia-opencl.so.1").is_some()
    {
        let dir = root.join("OpenCL/vendors");
        if write(&dir.join("nvidia.icd"), "libnvidia-opencl.so.1\n") {
            // OCL_ICD_VENDORS replaces the system's directory, so its files come along.
            for entry in fs::read_dir(&opencl).into_iter().flatten().flatten() {
                let _ = fs::copy(entry.path(), dir.join(entry.file_name()));
            }
            env.insert("OCL_ICD_VENDORS".into(), dir.to_string_lossy().into_owned());
        }
    }

    let backends = gbm_backend_dirs();
    let backend = "nvidia-drm_gbm.so";
    let loaded = backends.iter().any(|d| d.join(backend).exists());
    let found = library_dirs()
        .into_iter()
        .flat_map(|d| [d.join("gbm").join(backend), d.join(backend)])
        .find(|p| p.exists());
    if let (false, Some(found)) = (loaded, found) {
        let dir = root.join("gbm");
        let link = dir.join(backend);
        if fs::create_dir_all(&dir).is_ok()
            && (link.exists() || std::os::unix::fs::symlink(&found, &link).is_ok())
        {
            // GBM_BACKENDS_PATH replaces libgbm's own directory, where Mesa keeps its backend.
            let rest = get("GBM_BACKENDS_PATH")
                .map(|v| v.split(':').map(str::to_string).collect::<Vec<_>>())
                .unwrap_or_else(|| {
                    backends
                        .iter()
                        .map(|d| d.to_string_lossy().into_owned())
                        .collect()
                });
            env.insert("GBM_BACKENDS_PATH".into(), prepend_path(&dir, rest));
        }
    }
    env
}

/// What the probe answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    /// The facts it reported, by the names it prints them under.
    Report(BTreeMap<String, String>),
    /// It wedged or died on this driver stack, which the session would then do on every
    /// restart: a GPU is here and nothing of it can be reached.
    Wedged,
    /// No report could be had, which settles nothing.
    Unavailable,
}

/// The `KEY=VALUE` lines of the probe's output, those with a clean upper-case name only, so a
/// tool answering something else cannot set what it likes.
pub fn parse_facts(out: &str) -> BTreeMap<String, String> {
    out.lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(k, _)| {
            !k.is_empty()
                && k.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        })
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Run `selkies gpu-probe` (`exe`) in `env`, killed once it outlives `timeout`.
pub fn probe(exe: &str, env: &Env, timeout: Duration) -> Probe {
    let Ok(mut child) = Command::new(exe)
        .arg("gpu-probe")
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
    else {
        return Probe::Unavailable;
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let Some(status) = status else {
        return Probe::Wedged;
    };
    if status.code().is_none() {
        return Probe::Wedged;
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    if !status.success() {
        return Probe::Unavailable;
    }
    Probe::Report(parse_facts(&out))
}

/// What the policy settled.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Variables to set in the session's environment.
    pub set: Env,
    /// Variables to take out of it.
    pub unset: Vec<String>,
    /// Whether the session runs on Wayland.
    pub wayland: bool,
    /// The render node the X server's glamor draws on, empty for the software framebuffer.
    pub x_render_node: String,
    /// What was decided, in words, one line each.
    pub lines: Vec<String>,
}

/// The session's GPU policy from what the probe answered (`probe`), whether Wayland was asked
/// for, and the launcher's environment (`get`). `first_node` is the render node an X server
/// draws on when the probe named none.
pub fn plan(
    probe: &Probe,
    wayland: bool,
    first_node: &str,
    nvidia_device: bool,
    get: impl Fn(&str) -> Option<String>,
) -> Plan {
    let mut facts = match probe {
        Probe::Report(facts) => facts.clone(),
        _ => BTreeMap::new(),
    };
    let mut out = Plan {
        wayland,
        ..Plan::default()
    };
    if *probe == Probe::Wedged {
        out.lines
            .push("GPU: the renderer bring-up did not survive this driver stack".into());
        facts.insert("SELKIES_GPU_PRESENT".into(), "true".into());
        facts.insert("SELKIES_GPU_ACCELERATED".into(), "false".into());
    }
    let fact =
        |facts: &BTreeMap<String, String>, k: &str| facts.get(k).cloned().unwrap_or_default();
    if fact(&facts, "SELKIES_GPU_DRIVER").is_empty() && nvidia_device {
        facts.insert("SELKIES_GPU_DRIVER".into(), "nvidia".into());
    }
    let node = fact(&facts, "SELKIES_GPU_RENDER_NODE");
    let accelerated = fact(&facts, "SELKIES_GPU_ACCELERATED");
    let dri3_server = !node.is_empty() && accelerated != "false";

    let gl_vendor = fact(&facts, "SELKIES_GPU_GL_VENDOR");
    let mut gl = if dri3_server && !gl_vendor.is_empty() {
        out.set
            .insert("__GLX_VENDOR_LIBRARY_NAME".into(), gl_vendor.clone());
        format!("GLX on the {gl_vendor} library through the X server's DRI3")
    } else {
        "GLX through Mesa".to_string()
    };
    let egl_x11 = fact(&facts, "SELKIES_GPU_EGL_X11");
    gl.push_str(&format!(
        ", EGL on the {} library",
        if egl_x11.is_empty() { "Mesa" } else { &egl_x11 }
    ));
    match fact(&facts, "SELKIES_GPU_MESA_DRIVER").as_str() {
        "native" => gl.push_str(", Mesa on its own driver"),
        "zink" => {
            if is_true(get("DISABLE_ZINK").as_deref()) {
                gl.push_str(", Mesa in software (DISABLE_ZINK is set)");
            } else if fact(&facts, "SELKIES_GPU_VULKAN_PRESENTS") == "false" {
                gl.push_str(", Mesa in software: the Vulkan driver cannot present into a window");
            } else {
                out.set
                    .insert("MESA_LOADER_DRIVER_OVERRIDE".into(), "zink".into());
                out.set.insert("GALLIUM_DRIVER".into(), "zink".into());
                // Zink presents through DRI3 where the X server has it; the software
                // framebuffer server has the DRI2 path only.
                if !dri3_server {
                    out.set.insert("LIBGL_KOPPER_DRI2".into(), "1".into());
                }
                gl.push_str(", Mesa through Zink on the Vulkan driver");
            }
        }
        _ => gl.push_str(", Mesa in software"),
    }
    if !fact(&facts, "SELKIES_GPU_DRIVER").is_empty() {
        out.lines.push(format!("GL: {gl}"));
    }

    if wayland && fact(&facts, "SELKIES_GPU_PRESENT") == "true" && accelerated == "false" {
        let fallback = get("SELKIES_WAYLAND_X11_FALLBACK");
        if fallback.is_none() || is_true(fallback.as_deref()) {
            out.lines.push(
                "GPU: the compositor cannot reach it; starting X11 so the session keeps it".into(),
            );
            out.wayland = false;
        } else {
            out.lines
                .push("GPU: the compositor cannot reach it; compositing in software".into());
            for var in [
                "GALLIUM_DRIVER",
                "MESA_LOADER_DRIVER_OVERRIDE",
                "LIBGL_KOPPER_DRI2",
                "__GLX_VENDOR_LIBRARY_NAME",
            ] {
                out.set.remove(var);
                out.unset.push(var.to_string());
            }
            if get("SELKIES_WAYLAND_RENDERER").is_none_or(|v| v.is_empty()) {
                out.set
                    .insert("SELKIES_WAYLAND_RENDERER".into(), "pixman".into());
            }
        }
    }

    if accelerated != "false" {
        out.x_render_node = if node.is_empty() {
            first_node.to_string()
        } else {
            node
        };
    }
    for (key, value) in facts {
        if key.starts_with("SELKIES_GPU_") {
            out.set.insert(key, value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(pairs: &[(&str, &str)]) -> Probe {
        Probe::Report(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn only_clean_assignments_are_read_from_the_probe() {
        let facts = parse_facts(
            "SELKIES_GPU_DRIVER=nvidia\nnoise line\nlower=x\nPATH;rm=y\nSELKIES_GPU_PRESENT=true\n",
        );
        assert_eq!(facts.len(), 2);
        assert_eq!(facts["SELKIES_GPU_DRIVER"], "nvidia");
    }

    #[test]
    fn glx_goes_to_the_vendor_that_renders_on_the_x_servers_node() {
        let plan = plan(
            &report(&[
                ("SELKIES_GPU_RENDER_NODE", "/dev/dri/renderD128"),
                ("SELKIES_GPU_DRIVER", "nvidia"),
                ("SELKIES_GPU_PRESENT", "true"),
                ("SELKIES_GPU_ACCELERATED", "true"),
                ("SELKIES_GPU_GL_VENDOR", "nvidia"),
                ("SELKIES_GPU_EGL_X11", "nvidia"),
            ]),
            true,
            "",
            false,
            no_env,
        );
        assert_eq!(plan.set["__GLX_VENDOR_LIBRARY_NAME"], "nvidia");
        assert!(plan.wayland);
        assert_eq!(plan.x_render_node, "/dev/dri/renderD128");
        assert_eq!(plan.set["SELKIES_GPU_DRIVER"], "nvidia");
        assert!(!plan.set.contains_key("GALLIUM_DRIVER"));
    }

    #[test]
    fn mesa_reaches_a_gpu_it_has_no_driver_for_through_zink_unless_told_not_to() {
        let facts = report(&[
            ("SELKIES_GPU_RENDER_NODE", "/dev/dri/renderD128"),
            ("SELKIES_GPU_DRIVER", "nvidia"),
            ("SELKIES_GPU_PRESENT", "true"),
            ("SELKIES_GPU_ACCELERATED", "true"),
            ("SELKIES_GPU_MESA_DRIVER", "zink"),
        ]);
        let zink = plan(&facts, false, "", false, no_env);
        assert_eq!(zink.set["GALLIUM_DRIVER"], "zink");
        assert!(
            !zink.set.contains_key("LIBGL_KOPPER_DRI2"),
            "a DRI3 server takes Zink's own presentation"
        );
        let off = plan(&facts, false, "", false, |k| {
            (k == "DISABLE_ZINK").then(|| "true".to_string())
        });
        assert!(!off.set.contains_key("GALLIUM_DRIVER"));
    }

    #[test]
    fn a_compositor_that_cannot_reach_the_gpu_hands_the_session_to_x11_unless_kept() {
        let facts = report(&[
            ("SELKIES_GPU_RENDER_NODE", "/dev/dri/renderD128"),
            ("SELKIES_GPU_DRIVER", "nvidia"),
            ("SELKIES_GPU_PRESENT", "true"),
            ("SELKIES_GPU_ACCELERATED", "false"),
            ("SELKIES_GPU_GL_VENDOR", "nvidia"),
        ]);
        let fallback = plan(&facts, true, "", false, no_env);
        assert!(!fallback.wayland, "X11 keeps the GPU");
        assert_eq!(
            fallback.x_render_node, "",
            "a node whose renderer did not come up draws nothing"
        );
        let kept = plan(&facts, true, "", false, |k| {
            (k == "SELKIES_WAYLAND_X11_FALLBACK").then(|| "false".to_string())
        });
        assert!(kept.wayland);
        assert_eq!(kept.set["SELKIES_WAYLAND_RENDERER"], "pixman");
        assert!(
            kept.unset
                .contains(&"__GLX_VENDOR_LIBRARY_NAME".to_string())
        );
    }

    #[test]
    fn a_wedged_probe_reads_as_a_gpu_nothing_can_reach() {
        let wedged = plan(&Probe::Wedged, true, "/dev/dri/renderD128", false, no_env);
        assert!(!wedged.wayland);
        assert_eq!(wedged.set["SELKIES_GPU_ACCELERATED"], "false");
        assert_eq!(wedged.x_render_node, "");
        let none = plan(
            &Probe::Unavailable,
            true,
            "/dev/dri/renderD128",
            false,
            no_env,
        );
        assert!(
            none.wayland,
            "a report that could not be had settles nothing"
        );
        assert_eq!(
            none.x_render_node, "/dev/dri/renderD128",
            "the X server still tries the GPU here"
        );
    }

    #[test]
    fn a_list_takes_its_first_entry_once() {
        assert_eq!(
            prepend_path(
                Path::new("/rt/gpu"),
                [
                    "/a".to_string(),
                    "/rt/gpu".to_string(),
                    String::new(),
                    "/b".to_string()
                ]
            ),
            "/rt/gpu:/a:/b"
        );
    }

    #[test]
    fn a_value_reads_true_as_the_settings_read_it() {
        assert!(is_true(Some("TRUE|locked")));
        assert!(is_true(Some(" 1 ")));
        assert!(!is_true(Some("")));
        assert!(!is_true(None));
        assert!(!is_true(Some("yes")));
    }
}
