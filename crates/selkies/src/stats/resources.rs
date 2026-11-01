/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Live readings for the dashboards' gauges: GPU utilization and memory, universal across
//! vendors, the session's CPU and memory ([`SystemUsage`]), and the one sampler both
//! transports run over them ([`ResourceMonitor`]).
//!
//! Sources, best first: NVIDIA reads NVML in-process (`nvml-wrapper`, the API behind nvtop;
//! exact PCI identity, no subprocess per poll) with an `nvidia-smi` fallback, and Tegra's
//! integrated GPU, which has neither, reports its load through devfreq. Every other card is
//! read through DRM: amdgpu counts utilization and VRAM device-wide in sysfs, and the rest
//! have the engine times in their clients' fdinfo summed, the kernel's vendor-neutral
//! interface that i915, xe, Mali, Adreno, and VideoCore all write. A card whose driver writes
//! neither, Apple's on Asahi, is listed with no utilization rather than a zero.
//!
//! A card's utilization is its busiest engine's, the video encoder and decoder included, on
//! every vendor: an encoder-bound session is a GPU-bound one, and the figure is the one an
//! operator reads to find the bottleneck. fdinfo already reports every engine; NVML and
//! nvidia-smi give NVENC and NVDEC apart from the graphics engine, and amdgpu's
//! `gpu_busy_percent` counts graphics alone, so their codec engines are folded in.
//!
//! NVML is probed once per process (`Latch`): a failed init is terminal and a successful
//! one is never repeated, since without that a host lacking the NVIDIA library would pay a
//! `dlopen` attempt and a log line on every poll for the life of the server.
//!
//! [`get_gpus`] keys the readings to the render node the pipeline captures and encodes on
//! (PCI match when the source knows its address, else a vendor-unique match), so the
//! monitored GPU is always the one doing the work. A [`GpuStat`]'s `load` is a 0..1
//! fraction, `None` where nothing on the host counts the card's utilization, and its memory
//! is in MiB, the units the stats collectors serialize.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use log::{debug, info};
use nvml_wrapper::Nvml;

use super::metrics::Metrics;

const MIB: f64 = 1024.0 * 1024.0;

/// The driver sysfs names a card's device is bound to, which is the platform driver's name on
/// an SoC and need not match the DRM driver's own (Broadcom's VideoCore IV binds as
/// `vc4-drm`). A card whose driver is not here is still read when its clients report engine
/// time; only the vendor match `dri_node` falls back to needs the keyword.
const DRIVER_VENDORS: &[(&str, &str)] = &[
    ("nvidia", "nvidia"),
    ("nvidia-drm", "nvidia"),
    ("nouveau", "nvidia"),
    ("amdgpu", "amd"),
    ("radeon", "amd"),
    ("i915", "intel"),
    ("xe", "intel"),
    ("asahi", "apple"),
    ("panfrost", "arm"),
    ("panthor", "arm"),
    ("lima", "arm"),
    ("msm", "qualcomm"),
    ("msm-kms", "qualcomm"),
    ("adreno", "qualcomm"),
    ("v3d", "broadcom"),
    ("vc4", "broadcom"),
    ("vc4-drm", "broadcom"),
    ("powervr", "imagination"),
    ("etnaviv", "vivante"),
];

/// NVIDIA's driver counts nothing through DRM, NVML and nvidia-smi being its sources, so a
/// host holding only its cards never pays for the fdinfo walk.
const DRM_UNCOUNTED: &[&str] = &["nvidia", "nvidia-drm"];

/// The sysfs roots the readers walk; a test's reader points at a fabricated tree.
const SYSFS_DRM_ROOT: &str = "/sys/class/drm";
const PROC_ROOT: &str = "/proc";

/// The vendor keyword of a kernel driver name.
fn vendor_of_driver(driver: &str) -> Option<&'static str> {
    DRIVER_VENDORS
        .iter()
        .find(|(name, _)| *name == driver)
        .map(|(_, vendor)| *vendor)
}

/// One GPU's live reading in the units the stats collectors serialize.
#[derive(Clone, Debug, PartialEq)]
pub struct GpuStat {
    /// Position of this GPU in the merged detection list.
    pub id: usize,
    /// Utilization as a 0..1 fraction, or `None` where nothing on the host counts it.
    pub load: Option<f64>,
    /// Total device memory in MiB, zero for a card on system memory.
    pub memory_total: f64,
    /// Used device memory in MiB.
    pub memory_used: f64,
    /// Normalized PCI address (lowercase, 4-hex-digit domain), or `None` when the source
    /// cannot name one.
    pub pci: Option<String>,
    /// Vendor keyword (`nvidia`, `amd`, `intel`, `apple`, ...), or `None`.
    pub vendor: Option<&'static str>,
}

/// Lowercase PCI address with the 4-hex-digit domain sysfs uses (nvidia-smi prints an
/// 8-digit domain).
fn normalize_pci(bus_id: &str) -> Option<String> {
    let lowered = bus_id.trim().to_lowercase();
    let parts: Vec<&str> = lowered.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let domain = &parts[0][parts[0].len().saturating_sub(4)..];
    Some(format!("{domain:0>4}:{}:{}", parts[1], parts[2]))
}

/// The sysfs directory of the device a `/dev/dri/card*` or `renderD*` node belongs to, the
/// one name both of a card's nodes resolve to; empty for no node.
fn device_of_node(dri_node: &str, root: &Path) -> String {
    let name = Path::new(dri_node.trim())
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.is_empty() {
        return String::new();
    }
    let link = root.join(&name).join("device");
    std::fs::canonicalize(&link)
        .unwrap_or(link)
        .to_string_lossy()
        .into_owned()
}

/// The PCI address backing a DRM node, via sysfs.
fn pci_of_node(dri_node: &str, root: &Path) -> Option<String> {
    let device = device_of_node(dri_node, root);
    if device.is_empty() {
        return None;
    }
    Path::new(&device)
        .file_name()
        .and_then(|n| normalize_pci(&n.to_string_lossy()))
}

/// The vendor keyword for a DRM node's kernel driver, via sysfs.
fn vendor_of_node(dri_node: &str, root: &Path) -> Option<&'static str> {
    let name = Path::new(dri_node.trim())
        .file_name()?
        .to_string_lossy()
        .into_owned();
    if name.is_empty() {
        return None;
    }
    let driver = std::fs::read_link(root.join(name).join("device").join("driver")).ok()?;
    vendor_of_driver(&driver.file_name()?.to_string_lossy())
}

/// What a once-per-process probe came to.
enum LatchState<T> {
    Untried,
    Ready(T),
    Failed,
}

/// A resource probed once per process: the first use runs `init`, and its outcome, success
/// or failure, is final. The NVML library cannot appear after startup, so a failure is never
/// retried or re-logged, and a success is never re-initialized.
pub(crate) struct Latch<T> {
    state: Mutex<LatchState<T>>,
}

impl<T> Latch<T> {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(LatchState::Untried),
        }
    }

    /// Run `read` over the resource, initializing it first if this is the first use; `None`
    /// where the init failed, now or before.
    pub(crate) fn with<R>(
        &self,
        init: impl FnOnce() -> Result<T, String>,
        read: impl FnOnce(&T) -> R,
    ) -> Option<R> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*state, LatchState::Untried) {
            *state = match init() {
                Ok(value) => LatchState::Ready(value),
                Err(_) => LatchState::Failed,
            };
        }
        match &*state {
            LatchState::Ready(value) => Some(read(value)),
            _ => None,
        }
    }

    /// `None` before the first use, `Some(true)` once initialized, `Some(false)` once failed.
    #[cfg(test)]
    pub(crate) fn status(&self) -> Option<bool> {
        match &*self.state.lock().unwrap_or_else(|e| e.into_inner()) {
            LatchState::Untried => None,
            LatchState::Ready(_) => Some(true),
            LatchState::Failed => Some(false),
        }
    }
}

static NVML: Latch<Nvml> = Latch::new();

/// NVIDIA via NVML: no subprocess, exact per-device PCI identity.
fn nvml_gpus() -> Vec<GpuStat> {
    NVML.with(
        || {
            Nvml::init().map_err(|e| {
                debug!(target: "stats", "NVML init failed; not retrying: {e}");
                e.to_string()
            })
        },
        |nvml| match read_nvml(nvml) {
            Ok(gpus) => gpus,
            Err(e) => {
                debug!(target: "stats", "NVML query failed: {e}");
                Vec::new()
            }
        },
    )
    .unwrap_or_default()
}

/// Every device NVML lists, its load the busiest of its graphics, encoder, and decoder
/// engines; an engine the device lacks reads zero.
fn read_nvml(nvml: &Nvml) -> Result<Vec<GpuStat>, nvml_wrapper::error::NvmlError> {
    let mut gpus = Vec::new();
    for idx in 0..nvml.device_count()? {
        let device = nvml.device_by_index(idx)?;
        let graphics = device.utilization_rates()?.gpu as f64;
        let encoder = device
            .encoder_utilization()
            .map(|u| u.utilization as f64)
            .unwrap_or(0.0);
        let decoder = device
            .decoder_utilization()
            .map(|u| u.utilization as f64)
            .unwrap_or(0.0);
        let memory = device.memory_info()?;
        let bus_id = device.pci_info()?.bus_id;
        gpus.push(GpuStat {
            id: idx as usize,
            load: Some(graphics.max(encoder).max(decoder) / 100.0),
            memory_total: memory.total as f64 / MIB,
            memory_used: memory.used as f64 / MIB,
            pci: normalize_pci(&bus_id),
            vendor: Some("nvidia"),
        });
    }
    Ok(gpus)
}

/// `utilization.*` are percentages, `memory.*` MiB (`nounits` strips the suffix), and
/// `pci.bus_id` keys the stats to the render node the pipeline encodes on. The codec engines
/// come last, and a driver that refuses them is asked again without.
const NVIDIA_SMI_BASE_QUERY: &str = "utilization.gpu,memory.total,memory.used,pci.bus_id";
const NVIDIA_SMI_QUERY: &str =
    "utilization.gpu,memory.total,memory.used,pci.bus_id,utilization.encoder,utilization.decoder";

/// The first executable `name` on `PATH`.
pub(crate) fn which_on_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|path| {
            std::fs::metadata(path)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

/// Run `command`, its exit code and stdout; `None` where it could not run or outlived
/// `timeout`, in which case it is killed.
pub(crate) fn run_with_timeout(
    command: &mut std::process::Command,
    timeout: Duration,
) -> Option<(i32, String)> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let out = reader.join().ok()?;
    Some((status.code().unwrap_or(-1), out))
}

/// NVIDIA via an nvidia-smi subprocess, when NVML is unavailable.
fn nvidia_gpus() -> Vec<GpuStat> {
    let Some(smi) = which_on_path("nvidia-smi") else {
        return Vec::new();
    };
    nvidia_gpus_with(|query| {
        let mut command = std::process::Command::new(&smi);
        command
            .arg(format!("--query-gpu={query}"))
            .arg("--format=csv,noheader,nounits");
        let out = run_with_timeout(&mut command, Duration::from_secs(5));
        if out.is_none() {
            debug!(target: "stats", "nvidia-smi query failed");
        }
        out
    })
}

/// The nvidia-smi reading through `run`, which answers a `--query-gpu` field list with the
/// tool's exit code and stdout, or `None` where it could not run: the full query first, the
/// base query where the driver refuses the codec fields.
pub(crate) fn nvidia_gpus_with(mut run: impl FnMut(&str) -> Option<(i32, String)>) -> Vec<GpuStat> {
    for query in [NVIDIA_SMI_QUERY, NVIDIA_SMI_BASE_QUERY] {
        let Some((code, stdout)) = run(query) else {
            return Vec::new();
        };
        if code == 0 {
            return parse_nvidia_smi(&stdout);
        }
    }
    Vec::new()
}

/// The cards in nvidia-smi's CSV: utilization, memory, bus id, and the codec engines where
/// asked, which a card lacking one reports as `[N/A]`.
fn parse_nvidia_smi(stdout: &str) -> Vec<GpuStat> {
    let mut gpus = Vec::new();
    for (idx, line) in stdout.trim().lines().enumerate() {
        let parts: Vec<&str> = line.split(',').map(str::trim).collect();
        if parts.len() < 3 {
            continue;
        }
        let (Ok(mut util), Ok(total), Ok(used)) = (
            parts[0].parse::<f64>(),
            parts[1].parse::<f64>(),
            parts[2].parse::<f64>(),
        ) else {
            continue;
        };
        for codec in parts.iter().skip(4).take(2) {
            if let Ok(value) = codec.parse::<f64>() {
                util = util.max(value);
            }
        }
        gpus.push(GpuStat {
            id: idx,
            load: Some(util / 100.0),
            memory_total: total,
            memory_used: used,
            pci: parts.get(3).and_then(|p| normalize_pci(p)),
            vendor: Some("nvidia"),
        });
    }
    gpus
}

/// The value of a sysfs counter file, or `None` when unreadable.
fn read_sysfs_number(path: &Path) -> Option<f64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Utilization for a card whose driver counts nothing device-wide is summed from its
/// clients' fdinfo. Finding those clients means walking every process's descriptors, which
/// costs far more than reading the few that DRM owns, so the walk runs on its own cadence
/// and the readings keep to the list it leaves.
const DRM_CLIENT_RESCAN_S: f64 = 5.0;
const DRM_CLIENT_WINDOW_S: f64 = 0.1;

/// The monotonic clock in seconds.
fn monotonic_secs() -> f64 {
    super::now_ns() as f64 / 1e9
}

/// Reads the DRM cards of a sysfs tree, carrying the engine totals between readings.
pub(crate) struct DrmReader {
    sysfs_root: PathBuf,
    proc_root: PathBuf,
    clock: Box<dyn Fn() -> f64 + Send>,
    sleep: Box<dyn Fn(f64) + Send>,
    /// fdinfo path to the device directory of every DRM descriptor found at the last walk.
    pub(crate) clients: HashMap<PathBuf, String>,
    pub(crate) clients_at: f64,
    /// Nanoseconds each `(device, engine)` had run at the last reading.
    pub(crate) busy: HashMap<(String, String), f64>,
    busy_at: f64,
}

impl DrmReader {
    pub(crate) fn new(
        sysfs_root: impl Into<PathBuf>,
        proc_root: impl Into<PathBuf>,
        clock: Box<dyn Fn() -> f64 + Send>,
        sleep: Box<dyn Fn(f64) + Send>,
    ) -> Self {
        Self {
            sysfs_root: sysfs_root.into(),
            proc_root: proc_root.into(),
            clock,
            sleep,
            clients: HashMap::new(),
            clients_at: 0.0,
            busy: HashMap::new(),
            busy_at: 0.0,
        }
    }

    /// The reader of this host's own trees.
    fn system() -> Self {
        Self::new(
            SYSFS_DRM_ROOT,
            PROC_ROOT,
            Box::new(monotonic_secs),
            Box::new(|seconds| std::thread::sleep(Duration::from_secs_f64(seconds))),
        )
    }

    /// `{fdinfo path: device directory}` for every DRM descriptor open on the host. Another
    /// user's descriptors are unreadable, so their work is missing from the total rather
    /// than guessed at.
    fn client_paths(&self) -> HashMap<PathBuf, String> {
        let mut clients = HashMap::new();
        let Ok(processes) = std::fs::read_dir(&self.proc_root) else {
            return clients;
        };
        for process in processes.flatten() {
            let pid = process.file_name();
            if !pid.to_string_lossy().chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let Ok(fds) = std::fs::read_dir(process.path().join("fd")) else {
                continue;
            };
            for fd in fds.flatten() {
                let Ok(target) = std::fs::read_link(fd.path()) else {
                    continue;
                };
                let target = target.to_string_lossy();
                if !target.starts_with("/dev/dri/") {
                    continue;
                }
                let device = device_of_node(&target, &self.sysfs_root);
                if !device.is_empty() {
                    clients.insert(process.path().join("fdinfo").join(fd.file_name()), device);
                }
            }
        }
        clients
    }

    /// Nanoseconds each device's engines have run, summed over its clients.
    ///
    /// A client is a `drm-client-id` on a device, not a descriptor: every duplicate of a
    /// descriptor, in the same process or one it was passed to, reports the same client's
    /// totals, so each id is counted once. A descriptor whose driver writes no id is a client
    /// of its own. Clients that have exited are dropped from `clients` as they are found, so
    /// the list never outgrows what is open.
    fn engine_busy(clients: &mut HashMap<PathBuf, String>) -> HashMap<(String, String), f64> {
        let mut busy: HashMap<(String, String), f64> = HashMap::new();
        let mut counted: HashSet<(String, String)> = HashSet::new();
        let paths: Vec<PathBuf> = clients.keys().cloned().collect();
        for path in paths {
            let Ok(text) = std::fs::read_to_string(&path) else {
                clients.remove(&path);
                continue;
            };
            let device = clients[&path].clone();
            let mut engines: HashMap<String, f64> = HashMap::new();
            let mut capacity: HashMap<String, f64> = HashMap::new();
            let mut client = path.to_string_lossy().into_owned();
            for line in text.lines() {
                let Some((key, value)) = line.split_once(':') else {
                    continue;
                };
                if key == "drm-client-id" {
                    client = value.trim().to_string();
                    continue;
                }
                let (name, into) = if let Some(name) = key.strip_prefix("drm-engine-capacity-") {
                    (name, &mut capacity)
                } else if let Some(name) = key.strip_prefix("drm-engine-") {
                    (name, &mut engines)
                } else {
                    continue;
                };
                if let Some(number) = value
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
                {
                    into.insert(name.to_string(), number);
                }
            }
            if !counted.insert((device.clone(), client)) {
                continue;
            }
            for (engine, ns) in engines {
                let share = ns / capacity.get(&engine).copied().unwrap_or(1.0).max(1.0);
                *busy.entry((device.clone(), engine)).or_insert(0.0) += share;
            }
        }
        busy
    }

    /// `{device directory: utilization}` for cards their clients report engine time for,
    /// taken as the busiest engine's share of the wall time since the last reading. The first
    /// reading has no predecessor, so it measures a short window of its own rather than
    /// report a zero it did not observe.
    pub(crate) fn client_load(&mut self) -> HashMap<String, f64> {
        let mut now = (self.clock)();
        if now - self.clients_at >= DRM_CLIENT_RESCAN_S {
            self.clients = self.client_paths();
            self.clients_at = now;
        }
        let mut busy = Self::engine_busy(&mut self.clients);
        if !busy.is_empty() && self.busy.is_empty() {
            self.busy = busy;
            self.busy_at = now;
            (self.sleep)(DRM_CLIENT_WINDOW_S);
            now = (self.clock)();
            busy = Self::engine_busy(&mut self.clients);
        }
        let elapsed = (now - self.busy_at) * 1e9;
        let mut load: HashMap<String, f64> = busy
            .keys()
            .map(|(device, _)| (device.clone(), 0.0))
            .collect();
        for (slot, ns) in &busy {
            let ran = ns - self.busy.get(slot).copied().unwrap_or(*ns);
            if elapsed > 0.0
                && let Some(entry) = load.get_mut(&slot.0)
            {
                *entry = entry.max((ran / elapsed).clamp(0.0, 1.0));
            }
        }
        self.busy = busy;
        self.busy_at = now;
        load
    }

    /// Every DRM card, read the best way its driver allows.
    ///
    /// amdgpu counts utilization and VRAM device-wide in sysfs, graphics only, so its
    /// clients' fdinfo is folded in for the codec engines; the rest are read from their
    /// clients' fdinfo alone, which is only looked for once a card is found that needs it.
    /// Mali writes its engine times only while profiling is on, so a card left at the
    /// driver's default reads as one nothing counts. Memory is reported only where the card
    /// has its own and the driver counts it, so one drawing on system memory is listed
    /// without rather than with a share invented for it. A card whose driver we know is
    /// listed even when nothing counts it, so `dri_node` can still match the one the pipeline
    /// captures on.
    pub(crate) fn gpus(&mut self) -> Vec<GpuStat> {
        let mut cards: Vec<(usize, PathBuf)> = std::fs::read_dir(&self.sysfs_root)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        let index = name.strip_prefix("card")?;
                        // Connector nodes (cardN-HDMI-A-1) are not devices.
                        if index.is_empty() || !index.chars().all(|c| c.is_ascii_digit()) {
                            return None;
                        }
                        Some((index.parse().ok()?, e.path()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        cards.sort_by(|a, b| a.1.cmp(&b.1));
        let mut load: Option<HashMap<String, f64>> = None;
        let mut gpus = Vec::new();
        for (index, card) in cards {
            let node = card.join("device");
            let Ok(driver) = std::fs::read_link(node.join("driver")) else {
                continue;
            };
            let driver = driver
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let device = std::fs::canonicalize(&node)
                .unwrap_or(node.clone())
                .to_string_lossy()
                .into_owned();
            let vendor = vendor_of_driver(&driver);
            let busy = read_sysfs_number(&node.join("gpu_busy_percent"));
            if load.is_none()
                && !DRM_UNCOUNTED.contains(&driver.as_str())
                && (busy.is_none() || vendor == Some("amd"))
            {
                load = Some(self.client_load());
            }
            let counted = load.as_ref().and_then(|l| l.get(&device).copied());
            let util = match busy {
                Some(busy) => Some((busy / 100.0).max(counted.unwrap_or(0.0))),
                None => counted,
            };
            if vendor.is_none() && util.is_none() {
                continue;
            }
            let total = read_sysfs_number(&node.join("mem_info_vram_total")).unwrap_or(0.0);
            let used = read_sysfs_number(&node.join("mem_info_vram_used")).unwrap_or(0.0);
            gpus.push(GpuStat {
                id: index,
                load: util,
                memory_total: total / MIB,
                memory_used: used / MIB,
                pci: Path::new(&device)
                    .file_name()
                    .and_then(|n| normalize_pci(&n.to_string_lossy())),
                vendor,
            });
        }
        gpus
    }
}

/// This host's DRM reader, carrying the engine totals between the monitor's ticks.
static DRM: LazyLock<Mutex<DrmReader>> = LazyLock::new(|| Mutex::new(DrmReader::system()));

/// Where a Tegra GPU's load is read: every `<dir>/*.<suffix>/load`, or one file.
///
/// Tegra's integrated GPU has no DRM node of its own (the tegra driver is the display
/// controller) and, before JetPack 6, no NVML either; devfreq (Xavier, Orin) and the legacy
/// `gpu.0` node (TX, Nano) report its load in tenths of a percent.
pub(crate) enum TegraLoad {
    Suffix { dir: PathBuf, suffix: String },
    File(PathBuf),
}

impl TegraLoad {
    /// The load files this source names, in name order.
    fn files(&self) -> Vec<PathBuf> {
        match self {
            TegraLoad::File(path) => vec![path.clone()],
            TegraLoad::Suffix { dir, suffix } => {
                let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
                    .map(|entries| {
                        entries
                            .flatten()
                            .filter(|e| {
                                e.file_name()
                                    .to_string_lossy()
                                    .ends_with(&format!(".{suffix}"))
                            })
                            .map(|e| e.path().join("load"))
                            .collect()
                    })
                    .unwrap_or_default();
                found.sort();
                found
            }
        }
    }
}

/// The names Tegra's GPU goes by under devfreq, and the legacy node.
fn tegra_load_sources() -> Vec<TegraLoad> {
    let mut sources: Vec<TegraLoad> = ["gpu", "gv11b", "gp10b", "ga10b", "gb10b"]
        .iter()
        .map(|name| TegraLoad::Suffix {
            dir: PathBuf::from("/sys/class/devfreq"),
            suffix: name.to_string(),
        })
        .collect();
    sources.push(TegraLoad::File(PathBuf::from("/sys/devices/gpu.0/load")));
    sources
}

/// Tegra's integrated GPU, whose memory is the system's rather than its own. The first
/// source that answers is the card; the rest are the same GPU under the names older kernels
/// give it.
pub(crate) fn tegra_gpus_from(sources: &[TegraLoad]) -> Vec<GpuStat> {
    for source in sources {
        let loads: Vec<f64> = source
            .files()
            .iter()
            .filter_map(|f| read_sysfs_number(f))
            .collect();
        if !loads.is_empty() {
            return loads
                .into_iter()
                .enumerate()
                .map(|(idx, load)| GpuStat {
                    id: idx,
                    load: Some((load / 1000.0).min(1.0)),
                    memory_total: 0.0,
                    memory_used: 0.0,
                    pci: None,
                    vendor: Some("nvidia"),
                })
                .collect();
        }
    }
    Vec::new()
}

fn tegra_gpus() -> Vec<GpuStat> {
    tegra_gpus_from(&tegra_load_sources())
}

/// The render node the gauge reads: `--encode-dri`'s where it names one, else the one
/// `gpu_id` points the encoder at, `renderD{128 + n}`, where it exists. Empty otherwise,
/// which leaves the gauge to `gpu_id` as a position among the GPUs found: a host given one
/// card of several is handed its node under the card's own number, which no list of one
/// reaches as a position.
pub fn gpu_node(gpu_id: Option<i64>, encode_dri: &str) -> String {
    gpu_node_where(gpu_id, encode_dri, |node| Path::new(node).exists())
}

/// [`gpu_node`] with `exists` saying which device paths are there.
pub(crate) fn gpu_node_where(
    gpu_id: Option<i64>,
    encode_dri: &str,
    exists: impl Fn(&str) -> bool,
) -> String {
    if !encode_dri.is_empty() {
        return encode_dri.to_string();
    }
    if let Some(id) = gpu_id
        && id >= 0
    {
        let node = format!("/dev/dri/renderD{}", 128 + id);
        if exists(&node) {
            return node;
        }
    }
    String::new()
}

/// One list from the sources, one source per vendor: the best that answered, in the order
/// they are tried, with sequential ids.
pub(crate) fn merge_gpus(
    nvidia: Vec<GpuStat>,
    tegra: Vec<GpuStat>,
    drm: Vec<GpuStat>,
) -> Vec<GpuStat> {
    let mut gpus = nvidia;
    for source in [tegra, drm] {
        let present: HashSet<Option<&str>> = gpus.iter().map(|g| g.vendor).collect();
        gpus.extend(source.into_iter().filter(|g| !present.contains(&g.vendor)));
    }
    for (i, gpu) in gpus.iter_mut().enumerate() {
        gpu.id = i;
    }
    gpus
}

/// The GPUs that are `dri_node`'s card (PCI match, else a unique vendor match), or all of
/// them where the node names none of them.
pub(crate) fn select_gpus(gpus: Vec<GpuStat>, dri_node: Option<&str>, root: &Path) -> Vec<GpuStat> {
    let Some(node) = dri_node.filter(|n| !n.is_empty()) else {
        return gpus;
    };
    if let Some(pci) = pci_of_node(node, root) {
        let matched: Vec<GpuStat> = gpus
            .iter()
            .filter(|g| g.pci.as_deref() == Some(pci.as_str()))
            .cloned()
            .collect();
        if !matched.is_empty() {
            return matched;
        }
    }
    if let Some(vendor) = vendor_of_node(node, root) {
        let matched: Vec<GpuStat> = gpus
            .iter()
            .filter(|g| g.vendor == Some(vendor))
            .cloned()
            .collect();
        if matched.len() == 1 {
            return matched;
        }
    }
    debug!(target: "stats", "No unique GPU stats source matches {node}");
    gpus
}

/// Every detected GPU, or only the one behind `dri_node`, the render node the pipeline
/// captures and encodes on, so stats always describe the same card the rest of the stack
/// uses; an unresolvable node falls back to the full list. Blocking: the first DRM reading
/// measures a short window of its own.
pub fn get_gpus(dri_node: Option<&str>) -> Vec<GpuStat> {
    let mut nvidia = nvml_gpus();
    if nvidia.is_empty() {
        nvidia = nvidia_gpus();
    }
    let drm = DRM.lock().unwrap_or_else(|e| e.into_inner()).gpus();
    let gpus = merge_gpus(nvidia, tegra_gpus(), drm);
    select_gpus(gpus, dri_node, Path::new(SYSFS_DRM_ROOT))
}

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const PROC_CGROUP: &str = "/proc/self/cgroup";

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Whether `text` is a run of ASCII digits, the way `str.isdigit` reads a counter.
fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
}

/// The value of `key` in a `key value` per line file.
fn stat_value(path: &Path, key: &str) -> Option<u64> {
    read_trimmed(path)?.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next(), parts.next()) {
            (Some(k), Some(v), None) if k == key && is_digits(v) => v.parse().ok(),
            _ => None,
        }
    })
}

/// The processors this process may run on.
fn allowed_cores() -> f64 {
    // SAFETY: a zeroed cpu_set_t is a valid empty set, and the call writes within its size.
    let count = unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) == 0 {
            libc::CPU_COUNT(&set)
        } else {
            0
        }
    };
    if count > 0 {
        return count as f64;
    }
    std::thread::available_parallelism()
        .map(|n| n.get() as f64)
        .unwrap_or(1.0)
}

/// Samples `(cpu_percent, mem_total_bytes, mem_used_bytes)` for the dashboards' gauges.
///
/// The node's figures show a container every processor and byte of the host, though its
/// cgroup allows it a share of them, and the host's figures count every other tenant too.
/// Where the cgroup the process runs in limits CPU or memory, the sampler reads that cgroup:
/// on the unified hierarchy `cpu.stat` and `memory.current` against `cpu.max` and
/// `memory.max`, on the legacy one the `cpuacct` and `memory` controllers against the CFS
/// quota and the memory limit. Memory in use leaves out the page cache the kernel can drop
/// (the inactive file pages), the way container tools report it, and CPU is the usage
/// counter's growth between two samples over the cores the cgroup may use: its quota, else
/// the processors it is allowed. A limit the cgroup does not set is the host's total. A
/// cgroup that limits nothing is not the environment, and neither is one that cannot be
/// read: the node is, through `sysinfo`'s system-wide figures.
pub struct SystemUsage {
    clock: Box<dyn Fn() -> f64 + Send>,
    unified: bool,
    cpu_dir: Option<PathBuf>,
    mem_dir: Option<PathBuf>,
    last: Option<(f64, f64)>,
    system: sysinfo::System,
}

impl SystemUsage {
    /// cgroup limit files, unified then legacy; the cgroup is the environment when one is set.
    const LIMITS: [&'static str; 4] = [
        "cpu.max",
        "memory.max",
        "cpu.cfs_quota_us",
        "memory.limit_in_bytes",
    ];

    /// The sampler of this process's own cgroup.
    pub fn new() -> Self {
        Self::with_roots(
            Path::new(CGROUP_ROOT),
            Path::new(PROC_CGROUP),
            Box::new(monotonic_secs),
        )
    }

    /// A sampler reading the cgroup tree at `root` as `proc_cgroup` names it, on `clock`.
    pub(crate) fn with_roots(
        root: &Path,
        proc_cgroup: &Path,
        clock: Box<dyn Fn() -> f64 + Send>,
    ) -> Self {
        let mut usage = Self {
            clock,
            unified: false,
            cpu_dir: None,
            mem_dir: None,
            last: None,
            system: sysinfo::System::new(),
        };
        for line in read_trimmed(proc_cgroup).unwrap_or_default().lines() {
            let parts: Vec<&str> = line.splitn(3, ':').collect();
            if parts.len() != 3 {
                continue;
            }
            let (controllers, path) = (parts[1], parts[2].trim_start_matches('/'));
            if controllers.is_empty() {
                let own = root.join(path);
                if own.join("cpu.stat").is_file() {
                    usage.unified = true;
                    usage.cpu_dir = Some(own.clone());
                    usage.mem_dir = Some(own);
                }
                continue;
            }
            let names: Vec<&str> = controllers.split(',').collect();
            // A container that keeps the host's cgroup names sees only its own cgroup at the
            // controller's mount, so that is the fallback.
            for mount in [root.join(controllers).join(path), root.join(controllers)] {
                if !mount.is_dir() {
                    continue;
                }
                if (names.contains(&"cpuacct") || names.contains(&"cpu")) && usage.cpu_dir.is_none()
                {
                    usage.cpu_dir = Some(mount.clone());
                }
                if names.contains(&"memory") && usage.mem_dir.is_none() {
                    usage.mem_dir = Some(mount);
                }
                break;
            }
        }
        if !usage.limited() {
            usage.cpu_dir = None;
            usage.mem_dir = None;
        }
        usage
    }

    /// The host's memory in bytes.
    fn host_total(&mut self) -> u64 {
        self.system.refresh_memory();
        self.system.total_memory()
    }

    /// Whether the cgroup caps CPU or memory below the host.
    fn limited(&mut self) -> bool {
        let host = self.host_total();
        for dir in [self.cpu_dir.clone(), self.mem_dir.clone()]
            .into_iter()
            .flatten()
        {
            for name in Self::LIMITS {
                let text = read_trimmed(&dir.join(name)).unwrap_or_default();
                let Some(first) = text.split_whitespace().next() else {
                    continue;
                };
                if !is_digits(first) {
                    continue;
                }
                if name.starts_with("cpu") || first.parse::<u64>().is_ok_and(|v| v < host) {
                    return true;
                }
            }
        }
        false
    }

    /// The cgroup's CPU time in seconds and the cores it may use.
    fn cpu(&self) -> Option<(f64, f64)> {
        let dir = self.cpu_dir.as_ref()?;
        if self.unified {
            let usec = stat_value(&dir.join("cpu.stat"), "usage_usec")?;
            let quota = read_trimmed(&dir.join("cpu.max")).unwrap_or_default();
            let parts: Vec<&str> = quota.split_whitespace().collect();
            let cores = match (parts.first(), parts.get(1)) {
                (Some(q), Some(p)) if is_digits(q) => {
                    q.parse::<f64>().unwrap_or(0.0) / p.parse::<f64>().unwrap_or(1.0)
                }
                _ => allowed_cores(),
            };
            return Some((usec as f64 / 1e6, cores));
        }
        let nsec = read_trimmed(&dir.join("cpuacct.usage")).filter(|s| is_digits(s))?;
        let quota = read_trimmed(&dir.join("cpu.cfs_quota_us"));
        let period = read_trimmed(&dir.join("cpu.cfs_period_us"));
        let cores = match (quota, period) {
            (Some(q), Some(p)) if is_digits(&q) && is_digits(&p) => {
                let (q, p) = (
                    q.parse::<f64>().unwrap_or(0.0),
                    p.parse::<f64>().unwrap_or(0.0),
                );
                if q > 0.0 && p > 0.0 {
                    q / p
                } else {
                    allowed_cores()
                }
            }
            _ => allowed_cores(),
        };
        Some((nsec.parse::<f64>().ok()? / 1e9, cores))
    }

    /// The cgroup's memory limit, the host's total without one, and its use.
    fn memory(&mut self) -> Option<(u64, u64)> {
        let dir = self.mem_dir.clone()?;
        let (current, limit, cache) = if self.unified {
            (
                read_trimmed(&dir.join("memory.current")),
                read_trimmed(&dir.join("memory.max")),
                stat_value(&dir.join("memory.stat"), "inactive_file"),
            )
        } else {
            (
                read_trimmed(&dir.join("memory.usage_in_bytes")),
                read_trimmed(&dir.join("memory.limit_in_bytes")),
                stat_value(&dir.join("memory.stat"), "total_inactive_file"),
            )
        };
        let current: u64 = current.filter(|c| is_digits(c))?.parse().ok()?;
        let host = self.host_total();
        let total = match limit
            .filter(|l| is_digits(l))
            .and_then(|l| l.parse::<u64>().ok())
        {
            Some(limit) if limit < host => limit,
            _ => host,
        };
        Some((total, current.saturating_sub(cache.unwrap_or(0))))
    }

    /// One sample: CPU percent to a decimal, memory total and used in bytes.
    pub fn sample(&mut self) -> (f64, u64, u64) {
        let cpu = self.cpu();
        let mem = self.memory();
        let now = (self.clock)();
        let percent = match cpu {
            None => {
                self.system.refresh_cpu_usage();
                self.system.global_cpu_usage() as f64
            }
            Some((used, cores)) => {
                let mut percent = 0.0;
                if let Some((last_used, last_at)) = self.last
                    && now > last_at
                {
                    percent = ((used - last_used) / (now - last_at) / cores.max(1e-9) * 100.0)
                        .clamp(0.0, 100.0);
                }
                self.last = Some((used, now));
                percent
            }
        };
        let (total, used) = match mem {
            Some(mem) => mem,
            None => {
                self.system.refresh_memory();
                (self.system.total_memory(), self.system.used_memory())
            }
        };
        ((percent * 10.0).round() / 10.0, total, used)
    }
}

impl Default for SystemUsage {
    fn default() -> Self {
        Self::new()
    }
}

/// How soon after a page opens its stats its first figures follow, in seconds: long enough
/// for the CPU and the encode to be differenced over something.
pub const RUSH_S: f64 = 0.5;

/// The session's CPU and memory as last sampled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SystemSample {
    pub cpu_percent: f64,
    pub mem_total: u64,
    pub mem_used: u64,
}

/// The GPU's last reading: utilization in percent, memory in bytes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GpuSample {
    pub gpu_percent: f64,
    pub memory_total: u64,
    pub memory_used: u64,
}

/// Runs every period with the wall-clock time, for whatever a transport does on the same
/// cadence (sending `stream_stats`).
pub type TickFn = dyn Fn(f64) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync;
/// Says whether a page has its stats open, so a period is sampled only for someone.
pub type WatchedFn = dyn Fn() -> bool + Send + Sync;

struct MonitorState {
    system: Option<SystemSample>,
    gpu: Option<GpuSample>,
    probe_gpu: bool,
    gpu_seen: bool,
    last_sample_at: Option<Instant>,
    rush_at: Option<Instant>,
    task: Option<tokio::task::JoinHandle<()>>,
    stopping: bool,
}

struct Hooks {
    watched: Option<Arc<WatchedFn>>,
    on_tick: Option<Arc<TickFn>>,
}

/// Samples the session's CPU and memory, and the GPU its pipeline encodes on, once a period
/// off the runtime's workers, and keeps the latest sample in the shapes the pages take.
///
/// `system` is always the last [`SystemUsage`] sample; `gpu` is the card's last reading,
/// `None` while it cannot be read, and never polled again once the first probe finds no GPU,
/// since a vendor tool may be spawned per query. The tick hook runs every period for whatever
/// a transport does on the same cadence; `metrics`, when given, takes the GPU utilization. A
/// period is sampled only for someone: the watched hook, when set, says whether a page has
/// its stats open, and without one and without `metrics` the period passes unsampled and
/// `system` and `gpu` read `None` rather than go stale. A page that opens its stats is not
/// kept waiting for the period to come round ([`ResourceMonitor::rush`]). The GPU is
/// `dri_node`'s card when that narrows the list to one, else the `gpu_id`th of the
/// unfiltered list.
pub struct ResourceMonitor {
    period: Duration,
    gpu_id: i64,
    dri_node: String,
    metrics: Option<Arc<Metrics>>,
    usage: Arc<Mutex<SystemUsage>>,
    state: Mutex<MonitorState>,
    hooks: Mutex<Hooks>,
    sampling: tokio::sync::Mutex<()>,
    wake: tokio::sync::Notify,
}

impl ResourceMonitor {
    /// A monitor sampling every `period_s` seconds (at least one) for the GPU `gpu_id`, or
    /// `dri_node`'s where that names one.
    pub fn new(
        period_s: f64,
        gpu_id: i64,
        dri_node: &str,
        metrics: Option<Arc<Metrics>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            period: Duration::from_secs_f64(period_s.max(1.0)),
            gpu_id,
            dri_node: dri_node.to_string(),
            metrics,
            usage: Arc::new(Mutex::new(SystemUsage::new())),
            state: Mutex::new(MonitorState {
                system: None,
                gpu: None,
                probe_gpu: true,
                gpu_seen: false,
                last_sample_at: None,
                rush_at: None,
                task: None,
                stopping: false,
            }),
            hooks: Mutex::new(Hooks {
                watched: None,
                on_tick: None,
            }),
            sampling: tokio::sync::Mutex::new(()),
            wake: tokio::sync::Notify::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, MonitorState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn period(&self) -> Duration {
        self.period
    }

    /// The last CPU and memory sample, `None` while nobody watches.
    pub fn system(&self) -> Option<SystemSample> {
        self.lock().system
    }

    /// The GPU's last reading, `None` while nobody watches or nothing reads it.
    pub fn gpu(&self) -> Option<GpuSample> {
        self.lock().gpu
    }

    /// When the CPU counter was last read, a tick's sample or a rush's baseline.
    pub fn last_sample_at(&self) -> Option<Instant> {
        self.lock().last_sample_at
    }

    /// Say whether a page has its stats open; without this hook every period is sampled.
    pub fn set_watched(&self, watched: impl Fn() -> bool + Send + Sync + 'static) {
        self.hooks.lock().unwrap_or_else(|e| e.into_inner()).watched = Some(Arc::new(watched));
    }

    /// Run `on_tick` every period with the wall-clock time.
    pub fn set_on_tick(
        &self,
        on_tick: impl Fn(f64) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync + 'static,
    ) {
        self.hooks.lock().unwrap_or_else(|e| e.into_inner()).on_tick = Some(Arc::new(on_tick));
    }

    /// One GPU reading from `gpus`, the detection for `dri_node`, or `None` where there is
    /// nothing to read.
    ///
    /// A card is listed for what it is even when it exposes no counters, so the pipeline can
    /// match it by vendor or PCI address, but a card nothing counts and that reports no
    /// memory is not a reading: published every tick it would leave a page showing a
    /// utilization that can never move and a memory total of nothing. `None` instead stops
    /// the probe and leaves those off the page. A card that is merely idle has a
    /// utilization, so it keeps reporting.
    pub(crate) fn gpu_sample_of(
        gpus: &[GpuStat],
        dri_node: &str,
        gpu_id: i64,
    ) -> Option<GpuSample> {
        let idx = if !dri_node.is_empty() && gpus.len() == 1 {
            0
        } else {
            gpu_id
        };
        let gpu = usize::try_from(idx).ok().and_then(|i| gpus.get(i))?;
        if gpu.load.is_none() && gpu.memory_total <= 0.0 {
            return None;
        }
        Some(GpuSample {
            gpu_percent: gpu.load.unwrap_or(0.0) * 100.0,
            memory_total: (gpu.memory_total * MIB).round() as u64,
            memory_used: (gpu.memory_used * MIB).round() as u64,
        })
    }

    /// One blocking sample of both, for a worker thread.
    fn sample_blocking(
        usage: &Mutex<SystemUsage>,
        probe_gpu: bool,
        dri_node: &str,
        gpu_id: i64,
    ) -> (SystemSample, Option<GpuSample>) {
        let (cpu_percent, mem_total, mem_used) =
            usage.lock().unwrap_or_else(|e| e.into_inner()).sample();
        let system = SystemSample {
            cpu_percent,
            mem_total,
            mem_used,
        };
        let gpu = probe_gpu.then(|| {
            let node = (!dri_node.is_empty()).then_some(dri_node);
            Self::gpu_sample_of(&get_gpus(node), dri_node, gpu_id)
        });
        (system, gpu.flatten())
    }

    /// Sample off the runtime and keep the result; `None` where the worker failed.
    async fn sample(&self) -> Option<(SystemSample, Option<GpuSample>)> {
        let usage = self.usage.clone();
        let probe_gpu = self.lock().probe_gpu;
        let (dri_node, gpu_id) = (self.dri_node.clone(), self.gpu_id);
        let (system, gpu) = tokio::task::spawn_blocking(move || {
            Self::sample_blocking(&usage, probe_gpu, &dri_node, gpu_id)
        })
        .await
        .ok()?;
        let mut state = self.lock();
        state.last_sample_at = Some(Instant::now());
        if probe_gpu {
            if gpu.is_some() {
                state.gpu_seen = true;
            } else if !state.gpu_seen {
                state.probe_gpu = false;
                info!(target: "stats", "No GPU with ID {} reports utilization or memory; GPU stats disabled.", self.gpu_id);
            }
        }
        Some((system, gpu))
    }

    /// The sampling loop: a sample where someone watches, the tick, then the wait for the
    /// period or a rush, whichever a page asked for.
    async fn run(self: Arc<Self>) {
        loop {
            {
                let mut state = self.lock();
                if state.stopping {
                    return;
                }
                state.rush_at = None;
            }
            let (watched, on_tick) = {
                let hooks = self.hooks.lock().unwrap_or_else(|e| e.into_inner());
                (hooks.watched.clone(), hooks.on_tick.clone())
            };
            let wanted = self.metrics.is_some() || watched.is_none_or(|w| w());
            let mut gpu = None;
            if wanted {
                let _sampling = self.sampling.lock().await;
                if let Some((system, read)) = self.sample().await {
                    let mut state = self.lock();
                    state.system = Some(system);
                    state.gpu = read;
                    gpu = read;
                }
            } else {
                let mut state = self.lock();
                state.system = None;
                state.gpu = None;
            }
            if let (Some(metrics), Some(gpu)) = (&self.metrics, gpu) {
                metrics.set_gpu_utilization(gpu.gpu_percent);
            }
            if let Some(tick) = on_tick {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                tick(now).await;
            }
            let period_end = Instant::now() + self.period;
            loop {
                let target = {
                    let state = self.lock();
                    if state.stopping {
                        return;
                    }
                    state.rush_at.unwrap_or(period_end)
                };
                tokio::select! {
                    _ = tokio::time::sleep_until(target.into()) => break,
                    _ = self.wake.notified() => continue,
                }
            }
        }
    }

    /// Start sampling on the running runtime; a second start is a no-op.
    pub fn start(self: &Arc<Self>) {
        let mut state = self.lock();
        if state.task.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        state.stopping = false;
        state.task = Some(tokio::spawn(self.clone().run()));
    }

    /// Bring the next period forward for a page that just opened its stats.
    ///
    /// The CPU is differenced from now, so the first figures describe the moment rather than
    /// the spell nobody watched, and they follow `RUSH_S` later instead of up to a period. A
    /// period that would end sooner waits for them, since a sample right behind the baseline
    /// is differenced over next to nothing; a sample already under way is the baseline
    /// itself, and a later rush restarts the wait.
    pub async fn rush(&self) {
        {
            let mut state = self.lock();
            if state.task.as_ref().is_none_or(|t| t.is_finished()) {
                return;
            }
            state.rush_at = Some(Instant::now() + Duration::from_secs_f64(RUSH_S));
        }
        self.wake.notify_one();
        if let Ok(_sampling) = self.sampling.try_lock() {
            let usage = self.usage.clone();
            let _ = tokio::task::spawn_blocking(move || {
                usage.lock().unwrap_or_else(|e| e.into_inner()).sample()
            })
            .await;
            self.lock().last_sample_at = Some(Instant::now());
        }
    }

    /// End the loop at once and wait for it.
    pub async fn stop(&self) {
        let task = {
            let mut state = self.lock();
            state.stopping = true;
            state.task.take()
        };
        self.wake.notify_one();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests {
    //! The checks of `tests/unit/test_system_usage.py`, `test_drm_gpu_stats.py`,
    //! `test_gpu_stats_gate.py`, `test_nvml_failfast.py`, and the monitor half of
    //! `test_stream_stats.py`, driven against fabricated `/sys` and `/proc` trees with a
    //! clock the test advances itself; no GPU needed.

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const GIB: u64 = 1024 * 1024 * 1024;
    const MS: f64 = 1_000_000.0;

    /// A directory of this test's own, removed with it.
    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "selkies-stats-{tag}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Monotonic time the test advances itself, so a window is exactly as long as the check
    /// says it is.
    #[derive(Clone)]
    struct Clock(Arc<Mutex<f64>>);

    impl Clock {
        fn new(start: f64) -> Self {
            Self(Arc::new(Mutex::new(start)))
        }

        fn advance(&self, seconds: f64) {
            *self.0.lock().unwrap() += seconds;
        }

        fn reader(&self) -> Box<dyn Fn() -> f64 + Send> {
            let clock = self.clone();
            Box::new(move || *clock.0.lock().unwrap())
        }

        fn sleeper(&self) -> Box<dyn Fn(f64) + Send> {
            let clock = self.clone();
            Box::new(move |seconds| clock.advance(seconds))
        }
    }

    fn host_total() -> u64 {
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        system.total_memory()
    }

    fn unified(
        tree: &Tree,
        usage_usec: u64,
        cpu_max: &str,
        current: u64,
        limit: &str,
        inactive_file: u64,
    ) {
        tree.write("proc/cgroup", "0::/\n");
        tree.write(
            "cpu.stat",
            &format!("usage_usec {usage_usec}\nuser_usec 1\n"),
        );
        tree.write("cpu.max", &format!("{cpu_max}\n"));
        tree.write("memory.current", &format!("{current}\n"));
        tree.write("memory.max", &format!("{limit}\n"));
        tree.write(
            "memory.stat",
            &format!("anon 1\ninactive_file {inactive_file}\n"),
        );
    }

    fn usage_of(tree: &Tree, proc_cgroup: &str, clock: &Clock) -> SystemUsage {
        SystemUsage::with_roots(&tree.0, &tree.path(proc_cgroup), clock.reader())
    }

    #[test]
    fn unified_cgroup_limits_are_the_environment() {
        let tree = Tree::new("unified");
        let clock = Clock::new(100.0);
        unified(
            &tree,
            1_000_000,
            "200000 100000",
            3_000_000_000,
            "4000000000",
            1_000_000_000,
        );
        let mut usage = usage_of(&tree, "proc/cgroup", &clock);
        let (cpu, total, used) = usage.sample();
        assert_eq!(
            (cpu, total, used),
            (0.0, 4_000_000_000, 2_000_000_000),
            "the first sample reports no CPU, the memory limit, and the use without dropped cache"
        );
        unified(
            &tree,
            2_000_000,
            "200000 100000",
            3_000_000_000,
            "4000000000",
            1_000_000_000,
        );
        clock.advance(1.0);
        assert_eq!(
            usage.sample().0,
            50.0,
            "a second of CPU over a second on a two-core quota is half"
        );
        unified(
            &tree,
            12_000_000,
            "200000 100000",
            3_000_000_000,
            "4000000000",
            1_000_000_000,
        );
        clock.advance(1.0);
        assert_eq!(
            usage.sample().0,
            100.0,
            "usage past the quota's cores caps at a hundred"
        );
    }

    #[test]
    fn a_cgroup_without_limits_is_not_the_environment() {
        let tree = Tree::new("unlimited");
        let clock = Clock::new(100.0);
        let cores = allowed_cores();
        unified(&tree, 0, "max 100000", 500_000_000, "max", 0);
        let mut usage = usage_of(&tree, "proc/cgroup", &clock);
        let (cpu, total, used) = usage.sample();
        assert_eq!(total, host_total(), "the node's figures stand");
        assert_ne!(used, 500_000_000);
        assert!((0.0..=100.0).contains(&cpu));
        unified(&tree, 0, "max 100000", 500_000_000, "2000000000", 0);
        let mut usage = usage_of(&tree, "proc/cgroup", &clock);
        let (_, total, used) = usage.sample();
        unified(
            &tree,
            (cores * 1_000_000.0) as u64,
            "max 100000",
            500_000_000,
            "2000000000",
            0,
        );
        clock.advance(1.0);
        let (cpu, _, _) = usage.sample();
        assert_eq!(
            (total, used, cpu),
            (2_000_000_000, 500_000_000, 100.0),
            "with a memory limit alone the cgroup counts, its CPU over the allowed processors"
        );
    }

    #[test]
    fn legacy_cgroup_quota_and_limit() {
        let tree = Tree::new("legacy");
        let clock = Clock::new(100.0);
        tree.write(
            "proc/cgroup",
            "5:cpu,cpuacct:/docker/abc\n4:memory:/docker/abc\n1:name=systemd:/docker/abc\n",
        );
        tree.write("cpu,cpuacct/docker/abc/cpuacct.usage", "1000000000\n");
        tree.write("cpu,cpuacct/docker/abc/cpu.cfs_quota_us", "400000\n");
        tree.write("cpu,cpuacct/docker/abc/cpu.cfs_period_us", "100000\n");
        tree.write("memory/docker/abc/memory.usage_in_bytes", "6000000000\n");
        tree.write("memory/docker/abc/memory.limit_in_bytes", "8000000000\n");
        tree.write(
            "memory/docker/abc/memory.stat",
            "cache 1\ntotal_inactive_file 2000000000\n",
        );
        let mut usage = usage_of(&tree, "proc/cgroup", &clock);
        let (_, total, used) = usage.sample();
        tree.write("cpu,cpuacct/docker/abc/cpuacct.usage", "3000000000\n");
        clock.advance(1.0);
        let (cpu, _, _) = usage.sample();
        assert_eq!(
            (total, used, cpu),
            (8_000_000_000, 4_000_000_000, 50.0),
            "the CFS quota, the memory limit, and the use without dropped cache"
        );
    }

    #[test]
    fn legacy_cgroup_without_a_namespace() {
        let tree = Tree::new("legacy-flat");
        let clock = Clock::new(100.0);
        tree.write(
            "proc/cgroup",
            "5:cpu,cpuacct:/docker/abc\n4:memory:/docker/abc\n",
        );
        tree.write("cpu,cpuacct/cpuacct.usage", "0\n");
        tree.write("cpu,cpuacct/cpu.cfs_quota_us", "-1\n");
        tree.write("cpu,cpuacct/cpu.cfs_period_us", "100000\n");
        tree.write("memory/memory.usage_in_bytes", "100\n");
        tree.write("memory/memory.limit_in_bytes", "9223372036854771712\n");
        tree.write("memory/memory.stat", "");
        let mut usage = usage_of(&tree, "proc/cgroup", &clock);
        let (_, total, used) = usage.sample();
        assert_eq!(
            total,
            host_total(),
            "without limits the node's figures stand"
        );
        assert_ne!(used, 100);
        tree.write("cpu,cpuacct/cpu.cfs_quota_us", "100000\n");
        let mut usage = usage_of(&tree, "proc/cgroup", &clock);
        let (_, total, used) = usage.sample();
        assert_eq!(
            (total, used),
            (host_total(), 100),
            "with a CPU quota alone the controller mounts are the cgroup, the memory total the host's"
        );
    }

    #[test]
    fn no_cgroup_means_the_node_figures() {
        let tree = Tree::new("none");
        let clock = Clock::new(100.0);
        let mut usage = usage_of(&tree, "missing", &clock);
        let (cpu, total, used) = usage.sample();
        assert_eq!(total, host_total());
        assert!((0.0..=100.0).contains(&cpu));
        assert!(used > 0 && used <= total, "{used} of {total}");
    }

    /// A DRM card in the fabricated tree, with its render node beside it. Returns the device
    /// directory both nodes resolve to, which is the key the fdinfo readings are aggregated
    /// under.
    fn card(tree: &Tree, name: &str, driver: &str, pci: &str, files: &[(&str, &str)]) -> String {
        let device = tree.path(&format!("sys/devices/pci0000:00/{pci}"));
        std::fs::create_dir_all(device.join("driver_dir").join(driver)).unwrap();
        let link = device.join("driver");
        if std::fs::symlink_metadata(&link).is_err() {
            std::os::unix::fs::symlink(Path::new("driver_dir").join(driver), &link).unwrap();
        }
        for (key, value) in files {
            std::fs::write(device.join(key), value).unwrap();
        }
        let index: usize = name[4..].parse().unwrap();
        for node in [name.to_string(), format!("renderD{}", 128 + index)] {
            let node_dir = tree.path(&format!("sys/class/drm/{node}"));
            std::fs::create_dir_all(&node_dir).unwrap();
            let link = node_dir.join("device");
            if std::fs::symlink_metadata(&link).is_err() {
                std::os::unix::fs::symlink(&device, &link).unwrap();
            }
        }
        std::fs::canonicalize(&device)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    /// One process holding that DRM node open, its engines at those totals in nanoseconds. A
    /// capacity is given as `engine__capacity`, and a descriptor duplicated from another's as
    /// that one's `client_id`.
    fn client(
        tree: &Tree,
        pid: u32,
        fd: u32,
        node: &str,
        client_id: Option<u32>,
        engines: &[(&str, f64)],
    ) {
        let fds = tree.path(&format!("proc/{pid}/fd"));
        let info = tree.path(&format!("proc/{pid}/fdinfo"));
        std::fs::create_dir_all(&fds).unwrap();
        std::fs::create_dir_all(&info).unwrap();
        let link = fds.join(fd.to_string());
        if std::fs::symlink_metadata(&link).is_err() {
            std::os::unix::fs::symlink(format!("/dev/dri/{node}"), &link).unwrap();
        }
        let mut lines = vec![
            "pos:\t0".to_string(),
            "drm-driver:\tfabricated".to_string(),
            format!("drm-client-id:\t{}", client_id.unwrap_or(pid)),
        ];
        for (name, value) in engines {
            if let Some(engine) = name.strip_suffix("__capacity") {
                lines.push(format!("drm-engine-capacity-{engine}:\t{}", *value as u64));
            } else {
                lines.push(format!("drm-engine-{name}:\t{} ns", *value as u64));
            }
        }
        std::fs::write(info.join(fd.to_string()), lines.join("\n") + "\n").unwrap();
    }

    fn reader(tree: &Tree, clock: &Clock) -> DrmReader {
        DrmReader::new(
            tree.path("sys/class/drm"),
            tree.path("proc"),
            clock.reader(),
            clock.sleeper(),
        )
    }

    fn by_pci(gpus: &[GpuStat]) -> HashMap<String, GpuStat> {
        gpus.iter()
            .filter_map(|g| g.pci.clone().map(|p| (p, g.clone())))
            .collect()
    }

    #[test]
    fn drm_cards_are_read_off_their_clients_and_their_drivers() {
        let tree = Tree::new("drm");
        let clock = Clock::new(1000.0);
        let mut drm = reader(&tree, &clock);

        // A card read only through its clients: Mali, through the render node.
        let mali = card(&tree, "card0", "panthor", "0000:03:00.0", &[]);
        let mut engines: Vec<(&str, f64)> = vec![
            ("panthor", 0.0),
            ("render", 0.0),
            ("video", 0.0),
            ("video__capacity", 2.0),
        ];
        client(&tree, 4001, 3, "renderD128", None, &engines);
        let first = drm.client_load();
        assert_eq!(
            first.keys().collect::<Vec<_>>(),
            vec![&mali],
            "a card its clients report is keyed by the device both its nodes share"
        );
        assert_eq!(
            first.get(&mali),
            Some(&0.0),
            "an idle one reads zero, which is a measurement, not an absence"
        );

        clock.advance(0.5);
        engines[0].1 = 250.0 * MS;
        client(&tree, 4001, 3, "renderD128", None, &engines);
        assert_eq!(
            drm.client_load().get(&mali),
            Some(&0.5),
            "a quarter second of engine time over half a second is half the card"
        );

        clock.advance(0.5);
        engines[1].1 = 400.0 * MS;
        client(&tree, 4001, 3, "renderD128", None, &engines);
        assert_eq!(
            drm.client_load().get(&mali),
            Some(&0.8),
            "the busiest engine is the reading, not the engines added up"
        );

        clock.advance(0.5);
        engines[2].1 = 500.0 * MS;
        client(&tree, 4001, 3, "renderD128", None, &engines);
        assert_eq!(
            drm.client_load().get(&mali),
            Some(&0.5),
            "two engines under one key share the window between them"
        );

        // A client's descriptor duplicated, in its own process and into another, reports the
        // same totals again: one client, counted once.
        client(&tree, 4001, 4, "renderD128", None, &engines);
        client(&tree, 4005, 9, "card0", Some(4001), &engines);
        drm.clients_at = 0.0;
        drm.client_load();
        clock.advance(0.5);
        engines[0].1 = 350.0 * MS;
        for (pid, fd, node) in [
            (4001, 3, "renderD128"),
            (4001, 4, "renderD128"),
            (4005, 9, "card0"),
        ] {
            client(&tree, pid, fd, node, Some(4001), &engines);
        }
        assert_eq!(
            drm.client_load().get(&mali),
            Some(&0.2),
            "a client held through several descriptors is counted once"
        );
        std::fs::remove_file(tree.path("proc/4001/fd/4")).unwrap();
        std::fs::remove_file(tree.path("proc/4001/fdinfo/4")).unwrap();
        std::fs::remove_dir_all(tree.path("proc/4005")).unwrap();
        drm.clients_at = 0.0;
        drm.client_load();

        // Two clients on one card add up, and no client can push it past full.
        client(&tree, 4002, 7, "card0", None, &[("panthor", 0.0)]);
        drm.clients_at = 0.0;
        drm.client_load();
        clock.advance(0.5);
        engines[0].1 = 550.0 * MS;
        client(&tree, 4001, 3, "renderD128", None, &engines);
        client(&tree, 4002, 7, "card0", None, &[("panthor", 400.0 * MS)]);
        assert_eq!(
            drm.client_load().get(&mali),
            Some(&1.0),
            "the clients of one card are summed, and the sum stops at full"
        );

        // A client that exits leaves nothing behind.
        std::fs::remove_dir_all(tree.path("proc/4002")).unwrap();
        clock.advance(0.5);
        drm.client_load();
        assert_eq!(
            drm.clients.len(),
            1,
            "a client that exits is dropped from the list it was found in"
        );
        let devices: HashSet<&String> = drm.busy.keys().map(|(d, _)| d).collect();
        assert!(
            devices.len() == 1 && devices.contains(&mali) && drm.busy.len() == 3,
            "and from the totals carried between readings: {:?}",
            drm.busy
        );

        // What the collector makes of those cards.
        card(
            &tree,
            "card1",
            "amdgpu",
            "0000:04:00.0",
            &[
                ("gpu_busy_percent", "37"),
                ("mem_info_vram_total", &(8 * GIB).to_string()),
                ("mem_info_vram_used", &(2 * GIB).to_string()),
            ],
        );
        card(&tree, "card2", "nouveau", "0000:05:00.0", &[]);
        card(&tree, "card3", "ast", "0000:12:00.0", &[]);
        client(&tree, 4003, 5, "renderD129", None, &[("gfx", 0.0)]);
        drm.clients_at = 0.0;
        clock.advance(0.5);
        let gpus = by_pci(&drm.gpus());
        let amd = &gpus["0000:04:00.0"];
        assert!(
            amd.load == Some(0.37) && amd.vendor == Some("amd"),
            "a card counted device-wide is read there, not from its clients: {amd:?}"
        );
        assert!(
            amd.memory_total == 8192.0 && amd.memory_used == 2048.0,
            "with its memory in MiB"
        );
        let arm = &gpus["0000:03:00.0"];
        assert!(
            arm.memory_total == 0.0 && arm.vendor == Some("arm"),
            "a card on shared memory is listed without any of its own"
        );
        let nv = &gpus["0000:05:00.0"];
        assert!(
            nv.load.is_none() && nv.vendor == Some("nvidia"),
            "a card nothing counts is listed for matching, with no utilization: {nv:?}"
        );
        assert!(
            !gpus.contains_key("0000:12:00.0"),
            "a device that is not a GPU and has no engines is not a card"
        );

        // amdgpu's own figure counts graphics alone: an encode its clients report running
        // longer than that is what the card is busy with.
        client(
            &tree,
            4004,
            6,
            "renderD129",
            None,
            &[("gfx", 0.0), ("enc", 0.0)],
        );
        drm.clients_at = 0.0;
        drm.gpus();
        clock.advance(0.5);
        client(
            &tree,
            4004,
            6,
            "renderD129",
            None,
            &[("gfx", 50.0 * MS), ("enc", 400.0 * MS)],
        );
        let gpus = by_pci(&drm.gpus());
        assert_eq!(
            gpus["0000:04:00.0"].load,
            Some(0.8),
            "an amdgpu card busier encoding than drawing reads its encoder's share"
        );
        client(
            &tree,
            4004,
            6,
            "renderD129",
            None,
            &[("gfx", 50.0 * MS), ("enc", 400.0 * MS)],
        );
        clock.advance(0.5);
        let gpus = by_pci(&drm.gpus());
        assert_eq!(
            gpus["0000:04:00.0"].load,
            Some(0.37),
            "and its graphics figure once the encoder idles"
        );
        std::fs::remove_dir_all(tree.path("proc/4004")).unwrap();

        // The long tail: every SoC GPU is read the same way, off the engine time its clients
        // report, and a driver this table has never heard of is read too.
        let soc: [(&str, Option<&str>); 11] = [
            ("panfrost", Some("arm")),
            ("panthor", Some("arm")),
            ("lima", Some("arm")),
            ("msm", Some("qualcomm")),
            ("adreno", Some("qualcomm")),
            ("v3d", Some("broadcom")),
            ("vc4-drm", Some("broadcom")),
            ("etnaviv", Some("vivante")),
            ("powervr", Some("imagination")),
            ("asahi", Some("apple")),
            ("a-driver-from-next-year", None),
        ];
        for (offset, (driver, _)) in soc.iter().enumerate() {
            let index = 4 + offset;
            card(
                &tree,
                &format!("card{index}"),
                driver,
                &format!("ffe4{index:04x}.gpu"),
                &[],
            );
            client(
                &tree,
                5000 + index as u32,
                3,
                &format!("renderD{}", 128 + index),
                None,
                &[("gpu", 0.0)],
            );
        }
        drm.clients_at = 0.0;
        drm.gpus();
        clock.advance(0.5);
        for offset in 0..soc.len() {
            client(
                &tree,
                5004 + offset as u32,
                3,
                &format!("renderD{}", 132 + offset),
                None,
                &[("gpu", 250.0 * MS)],
            );
        }
        let listed: HashMap<usize, GpuStat> = drm.gpus().into_iter().map(|g| (g.id, g)).collect();
        for (offset, (driver, vendor)) in soc.iter().enumerate() {
            let gpu = listed
                .get(&(4 + offset))
                .unwrap_or_else(|| panic!("{driver} not read"));
            assert_eq!(
                gpu.load,
                Some(0.5),
                "every SoC GPU is read off its clients, a quarter second in half a second: {driver}"
            );
            assert_eq!(
                gpu.vendor, *vendor,
                "each is named by its vendor, the unknown driver by none: {driver}"
            );
            assert!(
                gpu.pci.is_none(),
                "none of them claims a PCI address it does not have: {driver}"
            );
        }

        // Tegra, whose load devfreq reports in tenths of a percent.
        tree.write("sys/class/devfreq/17000000.ga10b/load", "642\n");
        tree.write("sys/devices/gpu.0/load", "1000\n");
        let sources = vec![
            TegraLoad::Suffix {
                dir: tree.path("sys/class/devfreq"),
                suffix: "ga10b".into(),
            },
            TegraLoad::File(tree.path("sys/devices/gpu.0/load")),
        ];
        let found = tegra_gpus_from(&sources);
        assert!(
            found.len() == 1 && found[0].load == Some(0.642) && found[0].vendor == Some("nvidia"),
            "Tegra's tenths of a percent are a fraction of one: {found:?}"
        );
        assert_eq!(
            tegra_gpus_from(&sources[1..])[0].load,
            Some(1.0),
            "the same GPU under an older name is not a second card"
        );

        // A Jetson's DRM node belongs to its display controller, so the GPU's own reading has
        // to reach the list ahead of anything that node contributes.
        let merged = merge_gpus(Vec::new(), tegra_gpus_from(&sources), drm.gpus());
        let nvidia: Vec<&GpuStat> = merged
            .iter()
            .filter(|g| g.vendor == Some("nvidia"))
            .collect();
        assert!(
            nvidia.len() == 1 && nvidia[0].load == Some(0.642),
            "on a board whose GPU only devfreq counts, that is the one reading of it: {merged:?}"
        );
        assert!(
            merged.iter().enumerate().all(|(i, g)| g.id == i),
            "ids are sequential"
        );

        // The node's card is the one selected, by PCI address, else by a unique vendor.
        let root = tree.path("sys/class/drm");
        let all = drm.gpus();
        let picked = select_gpus(all.clone(), Some("/dev/dri/renderD129"), &root);
        assert!(
            picked.len() == 1 && picked[0].pci.as_deref() == Some("0000:04:00.0"),
            "{picked:?}"
        );
        let picked = select_gpus(all.clone(), Some("/dev/dri/renderD130"), &root);
        assert!(
            picked.len() == 1 && picked[0].vendor == Some("nvidia"),
            "{picked:?}"
        );
        assert_eq!(
            select_gpus(all.clone(), Some("/dev/dri/renderD200"), &root).len(),
            all.len(),
            "an unresolvable node falls back to the full list"
        );
    }

    #[test]
    fn the_gauge_follows_the_gpu_ids_node() {
        let exists = |path: &str| path == "/dev/dri/renderD132";
        assert_eq!(
            gpu_node_where(Some(4), "", exists),
            "/dev/dri/renderD132",
            "the GPU ID's render node is the gauge's"
        );
        assert_eq!(
            gpu_node_where(Some(4), "/dev/dri/renderD129", exists),
            "/dev/dri/renderD129",
            "the encode node's ahead of it"
        );
        assert_eq!(
            gpu_node_where(Some(9), "", exists),
            "",
            "none where the node is absent"
        );
        assert_eq!(gpu_node_where(None, "", exists), "", "or no ID was given");
        assert_eq!(gpu_node_where(Some(-1), "", exists), "");
    }

    #[test]
    fn nvidia_smi_reads_the_codec_engines_and_retries_without_them() {
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let found = {
            let asked = asked.clone();
            nvidia_gpus_with(|query| {
                asked.lock().unwrap().push(query.to_string());
                Some((0, "10, 8192, 425, 00000000:85:00.0, 70, 5\n30, 4096, 100, 00000000:86:00.0, [N/A], [N/A]\n".into()))
            })
        };
        assert_eq!(
            found.iter().map(|g| g.load).collect::<Vec<_>>(),
            vec![Some(0.7), Some(0.3)],
            "the busiest engine is the load, a codec engine the card lacks reading [N/A]"
        );
        assert_eq!(found[0].pci.as_deref(), Some("0000:85:00.0"));
        assert!(asked.lock().unwrap()[0].ends_with("utilization.decoder"));
        asked.lock().unwrap().clear();
        let found = {
            let asked = asked.clone();
            let answers = Mutex::new(
                vec![
                    (6, String::new()),
                    (0, "20, 8192, 425, 00000000:85:00.0\n".to_string()),
                ]
                .into_iter(),
            );
            nvidia_gpus_with(|query| {
                asked.lock().unwrap().push(query.to_string());
                answers.lock().unwrap().next()
            })
        };
        assert_eq!(
            found.iter().map(|g| g.load).collect::<Vec<_>>(),
            vec![Some(0.2)]
        );
        assert!(
            asked.lock().unwrap()[1].ends_with("pci.bus_id"),
            "a driver that refuses the codec fields is asked again without them"
        );
        assert!(
            nvidia_gpus_with(|_| None).is_empty(),
            "a tool that cannot run is no reading"
        );
    }

    #[test]
    fn a_card_measuring_nothing_is_not_a_reading() {
        let stat = |load: Option<f64>, total: f64, used: f64, vendor: &'static str| GpuStat {
            id: 0,
            load,
            memory_total: total,
            memory_used: used,
            pci: Some("0000:01:00.0".into()),
            vendor: Some(vendor),
        };
        let sample = |gpus: Vec<GpuStat>| ResourceMonitor::gpu_sample_of(&gpus, "", 0);
        assert!(
            sample(vec![stat(None, 0.0, 0.0, "intel")]).is_none(),
            "a card exposing no counters is not a reading"
        );
        assert!(sample(vec![]).is_none(), "nor is an empty detection");
        let measured = sample(vec![stat(Some(0.0), 0.0, 0.0, "arm")]);
        assert_eq!(
            measured.map(|s| s.gpu_percent),
            Some(0.0),
            "a card measured idle is a reading, so the probe stays on"
        );
        let idle = sample(vec![stat(Some(0.0), 8192.0, 512.0, "amd")]).unwrap();
        assert!(
            idle.gpu_percent == 0.0 && idle.memory_total == 8192 * 1024 * 1024,
            "an idle card with memory still reports"
        );
        let busy = sample(vec![stat(Some(0.42), 0.0, 0.0, "apple")]).unwrap();
        assert_eq!(
            (busy.gpu_percent * 100.0).round() / 100.0,
            42.0,
            "so does a card that reports utilization but no memory"
        );
        let two = vec![
            stat(None, 0.0, 0.0, "intel"),
            stat(Some(0.5), 0.0, 0.0, "amd"),
        ];
        assert!(
            ResourceMonitor::gpu_sample_of(&two, "", 1).is_some(),
            "the gpu_id picks among an unfiltered list"
        );
        assert!(
            ResourceMonitor::gpu_sample_of(&two[1..], "/dev/dri/renderD128", 1).is_some(),
            "a node that narrowed the list to one is that one"
        );
    }

    #[test]
    fn nvml_is_probed_once_per_process() {
        let latch: Latch<u32> = Latch::new();
        let attempts = AtomicUsize::new(0);
        let results: Vec<Option<u32>> = (0..5)
            .map(|_| {
                latch.with(
                    || {
                        attempts.fetch_add(1, Ordering::Relaxed);
                        Err("libnvidia-ml.so.1: cannot open shared object file".into())
                    },
                    |v| *v,
                )
            })
            .collect();
        assert!(
            results.iter().all(Option::is_none),
            "a failed init returns no GPUs"
        );
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            1,
            "a failed init is attempted exactly once"
        );
        assert_eq!(
            latch.status(),
            Some(false),
            "the failure is recorded as terminal"
        );

        let latch: Latch<u32> = Latch::new();
        let attempts = AtomicUsize::new(0);
        for _ in 0..3 {
            latch.with(
                || {
                    attempts.fetch_add(1, Ordering::Relaxed);
                    Ok(7)
                },
                |v| *v,
            );
        }
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            1,
            "a successful init is not repeated"
        );
        assert_eq!(latch.status(), Some(true), "success is recorded");
        assert_eq!(latch.with(|| Err(String::new()), |v| *v), Some(7));

        // The overall call survives whatever NVML comes to on this host by falling through
        // to the other sources, and the probe is not repeated.
        let _ = get_gpus(None);
        let status = NVML.status();
        assert!(status.is_some(), "get_gpus tried NVML once");
        let _ = get_gpus(None);
        assert_eq!(NVML.status(), status, "and did not try it again");
    }

    /// What the monitor holds after about two periods, and when it ticked.
    async fn sampled(watched: bool) -> (Arc<ResourceMonitor>, Vec<Option<SystemSample>>) {
        let monitor = ResourceMonitor::new(1.0, 0, "", None);
        let ticks = Arc::new(Mutex::new(Vec::new()));
        monitor.set_watched(move || watched);
        {
            let ticks = ticks.clone();
            let m = Arc::downgrade(&monitor);
            monitor.set_on_tick(move |_| {
                let ticks = ticks.clone();
                let m = m.clone();
                Box::pin(async move {
                    ticks
                        .lock()
                        .unwrap()
                        .push(m.upgrade().and_then(|m| m.system()));
                })
            });
        }
        monitor.start();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        monitor.stop().await;
        let ticks = ticks.lock().unwrap().clone();
        (monitor, ticks)
    }

    #[tokio::test]
    async fn an_unwatched_monitor_ticks_but_samples_nothing() {
        let (monitor, ticks) = sampled(false).await;
        assert!(
            ticks.len() >= 2,
            "unwatched, the monitor still ticks: {}",
            ticks.len()
        );
        assert!(
            ticks.iter().all(Option::is_none) && monitor.system().is_none(),
            "but samples nothing"
        );
    }

    #[tokio::test]
    async fn a_watched_monitor_samples_the_host() {
        let (monitor, _) = sampled(true).await;
        let host = super::super::host_stats(&monitor);
        let system = monitor.system().expect("a sample");
        assert!((0.0..=100.0).contains(&system.cpu_percent), "{host:?}");
        assert!(
            system.mem_used > 0 && system.mem_used <= system.mem_total,
            "{host:?}"
        );
        assert!(host.contains_key("cpu_percent") && host.contains_key("mem_total"));
    }

    #[tokio::test]
    async fn a_rush_as_a_period_ends_still_gets_a_window() {
        let monitor = ResourceMonitor::new(1.0, 0, "", None);
        monitor.set_watched(|| true);
        let ticks = Arc::new(Mutex::new(Vec::<Instant>::new()));
        {
            let ticks = ticks.clone();
            monitor.set_on_tick(move |_| {
                let ticks = ticks.clone();
                Box::pin(async move { ticks.lock().unwrap().push(Instant::now()) })
            });
        }
        monitor.start();
        let first = loop {
            if let Some(first) = ticks.lock().unwrap().first().copied() {
                break first;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let until = first + monitor.period() - Duration::from_millis(50);
        tokio::time::sleep(until.saturating_duration_since(Instant::now())).await;
        monitor.rush().await;
        let baseline = monitor.last_sample_at().expect("the rush took a baseline");
        let next = loop {
            if let Some(at) = monitor.last_sample_at().filter(|at| *at > baseline) {
                break at;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        monitor.stop().await;
        let window = next.duration_since(baseline).as_secs_f64();
        assert!(
            window >= RUSH_S / 2.0,
            "a page that opens its stats as a period ends still has its first CPU figure differenced over the rush's window: {window:.3} s"
        );
    }
}
