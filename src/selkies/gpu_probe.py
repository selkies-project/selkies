# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""What GPU a session has here, for whatever brings one up.

A Wayland compositor needs a working GBM/EGL stack on a DRM render node, and
device paths cannot answer whether that stack works -- a node can exist with
nothing usable behind it, and a driver can be installed with no node at all --
so the answer comes from pixelflux running the compositor's own renderer
bring-up. That bring-up also resolves `render_dri` and the `auto_gpu` token
exactly as the capture will, so the GPU it reports is the one the session uses.

Prints what it found as `KEY=VALUE` lines on stdout, for a caller to export and
act on, and one line saying it in words on stderr:

    SELKIES_GPU_RENDER_NODE  the node the session renders on, empty for none
    SELKIES_GPU_DRIVER       the driver behind it, empty when no GPU backs it
    SELKIES_GPU_PRESENT      whether a GPU is here at all
    SELKIES_GPU_ACCELERATED  whether the compositor's renderer came up on it
    SELKIES_GPU_GL_VENDOR    the glvnd vendor library whose EGL brings that GPU up
    SELKIES_GPU_EGL_X11      the first glvnd vendor whose EGL initialises on an X display
    SELKIES_GPU_MESA_DRIVER  how Mesa reaches the GPU: `native`, `zink`, or "" for software
    SELKIES_GPU_VULKAN_PRESENTS  whether the GPU's Vulkan driver gave an X window a swapchain, when Zink is Mesa's path

The client-side facts come from bringing each vendor's EGL up the way an
application would: on the render node through the device platform, and on a
throwaway X server for the window paths -- one drawing on the node through
glamor and DRI3 where the session's will -- so a driver that renders but
cannot present is reported as such rather than guessed from device files.

What to do about that -- which display backend to start, which GL stack the
session's applications are given -- is the deployment's own policy, and belongs
where the deployment is assembled rather than here.

Exits non-zero without printing anything when no report can be had: a driver
that refuses to answer at all.
"""

import contextlib
import ctypes
import json
import os
import re
import select
import shutil
import subprocess
import sys
from typing import Callable, Dict, Iterator, List, Optional, Tuple

# The proprietary NVIDIA stack, which no Mesa driver covers; what a session does
# about that is the deployment's policy, not this module's.
NVIDIA_DRIVER: str = "nvidia"


def render_node_driver(node: str, sysfs: str = "/sys/class/drm") -> str:
    """The kernel driver behind a DRM render node.

    Args:
        node: A `/dev/dri/renderD*` path.
        sysfs: Where the DRM class lives; a test points this elsewhere.

    Returns:
        The driver name (`nvidia`, `i915`, `xe`, `amdgpu`, ...), or "" when the
        node has no identity here — a system without `/sys` among others.
    """
    if not node:
        return ""
    link = os.path.join(sysfs, os.path.basename(node), "device", "driver")
    if not os.path.islink(link):
        return ""
    driver = os.path.basename(os.path.realpath(link))
    # The DRM node of the proprietary stack answers `nvidia` or `nvidia-drm`
    # depending on the kernel; nouveau is a different driver and stays itself.
    return NVIDIA_DRIVER if driver.startswith(NVIDIA_DRIVER) else driver


def session_gpu(node: str, sysfs: str = "/sys/class/drm",
                nvidia_device: str = "/dev/nvidiactl") -> str:
    """The driver of the GPU the session renders on.

    A render node names its own driver. With no node the GPU can still be
    reachable — the NVIDIA driver and its Vulkan ICD work without a DRM node —
    so the driver device answers for that case alone.

    Args:
        node: The render node the session renders on, or "" for none.
        sysfs: Where the DRM class lives; a test points this elsewhere.
        nvidia_device: The NVIDIA control device; a test points this elsewhere.

    Returns:
        The driver name, or "" when no GPU backs the session.
    """
    driver = render_node_driver(node, sysfs)
    if driver:
        return driver
    return NVIDIA_DRIVER if os.path.exists(nvidia_device) else ""


def facts(report: Dict[str, object]) -> Dict[str, str]:
    """The GPU a `pixelflux.probe_wayland_gpu` report describes.

    Args:
        report: That report (keys: `accelerated`, `gpu`, `renderer`, `node`,
            `error`).

    Returns:
        The variables a caller exports, all present and all strings.
    """
    node = str(report.get("node") or "")
    # The report echoes the node it was asked for, which a failed bring-up may
    # say is not there at all; that is no node to render on.
    if node and not os.path.exists(node):
        node = ""
    return {
        "SELKIES_GPU_RENDER_NODE": node,
        "SELKIES_GPU_DRIVER": session_gpu(node),
        "SELKIES_GPU_PRESENT": "true" if report.get("gpu") else "false",
        "SELKIES_GPU_ACCELERATED": "true" if report.get("accelerated") else "false",
    }


def summary(report: Dict[str, object], found: Dict[str, str]) -> str:
    """One line describing `found`, for a log.

    Args:
        report: The report it came from, for the renderer name and the error.
        found: The variables `facts` derived.

    Returns:
        The line to print.
    """
    node = found["SELKIES_GPU_RENDER_NODE"]
    where = f" on {node}" if node else ""
    if found["SELKIES_GPU_ACCELERATED"] == "true":
        return f"GPU: {report.get('renderer') or found['SELKIES_GPU_DRIVER']}{where}"
    why = str(report.get("error") or "no hardware acceleration")
    if found["SELKIES_GPU_PRESENT"] == "true":
        return f"GPU: {found['SELKIES_GPU_DRIVER'] or 'unknown'}{where}, unusable: {why}"
    return "GPU: none here"


# glvnd's EGL vendor ICD directories, the same on every distribution that
# packages glvnd; __EGL_VENDOR_LIBRARY_DIRS is glvnd's own override of them.
EGL_VENDOR_DIRS: Tuple[str, ...] = ("/etc/glvnd/egl_vendor.d", "/usr/share/glvnd/egl_vendor.d")
MESA_VENDOR: str = "mesa"
# The client-side environment the probes must not inherit: what the caller
# may already have decided about Mesa and glvnd is exactly what is measured.
CLIENT_ENV: Tuple[str, ...] = ("MESA_LOADER_DRIVER_OVERRIDE", "GALLIUM_DRIVER", "LIBGL_KOPPER_DRI2",
                               "LIBGL_ALWAYS_SOFTWARE", "MESA_VK_DEVICE_SELECT",
                               "__GLX_VENDOR_LIBRARY_NAME", "__EGL_VENDOR_LIBRARY_FILENAMES")
ZINK_ENV: Dict[str, str] = {"MESA_LOADER_DRIVER_OVERRIDE": "zink", "GALLIUM_DRIVER": "zink",
                            "LIBGL_KOPPER_DRI2": "1"}
PROBE_TIMEOUT: float = 15.0

EGL_PLATFORM_DEVICE_EXT = 0x313F
EGL_DRM_DEVICE_FILE_EXT = 0x3233
EGL_DRM_RENDER_NODE_FILE_EXT = 0x3377
EGL_OPENGL_API = 0x30A2
EGL_RENDERABLE_TYPE = 0x3040
EGL_OPENGL_BIT = 0x0008
EGL_SURFACE_TYPE = 0x3033
EGL_PBUFFER_BIT = 0x0001
EGL_WINDOW_BIT = 0x0004
EGL_WIDTH = 0x3057
EGL_HEIGHT = 0x3056
EGL_NONE = 0x3038
GL_RENDERER = 0x1F01


def egl_vendors(dirs: Tuple[str, ...] = EGL_VENDOR_DIRS,
                environ: Optional[Dict[str, str]] = None) -> List[Tuple[str, str]]:
    """glvnd's EGL vendors, in the order glvnd tries them.

    Args:
        dirs: The ICD directories to read when the environment names none.
        environ: The environment to read `__EGL_VENDOR_LIBRARY_DIRS` from.

    Returns:
        `(name, icd_path)` pairs, `name` being what `__GLX_VENDOR_LIBRARY_NAME`
        and `__EGL_VENDOR_LIBRARY_FILENAMES` know the vendor as; one per name.
    """
    environ = os.environ if environ is None else environ
    listed = environ.get("__EGL_VENDOR_LIBRARY_DIRS", "")
    found: List[Tuple[str, str]] = []
    for directory in listed.split(":") if listed else dirs:
        try:
            names = sorted(os.listdir(directory))
        except OSError:
            continue
        for name in names:
            if not name.endswith(".json"):
                continue
            path = os.path.join(directory, name)
            try:
                with open(path, encoding="utf-8") as icd:
                    library = str(json.load(icd)["ICD"]["library_path"])
            except (OSError, ValueError, KeyError, TypeError):
                continue
            match = re.match(r"libEGL_(\w+)\.so", os.path.basename(library))
            if match and match.group(1) not in [vendor for vendor, _ in found]:
                found.append((match.group(1), path))
    return found


def software_renderer(renderer: str) -> bool:
    """Whether a GL_RENDERER string names a software rasteriser."""
    lowered = renderer.lower()
    return lowered.startswith(("llvmpipe", "softpipe", "swr")) or "software" in lowered


def vk_device_select(node: str, sysfs: str = "/sys/class/drm") -> str:
    """The `MESA_VK_DEVICE_SELECT` value naming a render node's GPU, or ""."""
    if not node:
        return ""
    ids = []
    for name in ("vendor", "device"):
        try:
            with open(os.path.join(sysfs, os.path.basename(node), "device", name), encoding="ascii") as f:
                ids.append(f.read().strip().lower().removeprefix("0x"))
        except OSError:
            return ""
    return ":".join(ids)


def _client_env(vendor_file: str, extra: Optional[Dict[str, str]] = None) -> Dict[str, str]:
    env = {k: v for k, v in os.environ.items() if k not in CLIENT_ENV}
    env["__EGL_VENDOR_LIBRARY_FILENAMES"] = vendor_file
    env.update(extra or {})
    return env


def _helper(args: List[str], env: Dict[str, str]) -> str:
    """Run this module's helper `args` in a process of its own; its stdout, or ""."""
    try:
        done = subprocess.run([sys.executable, "-m", __spec__.name if __spec__ else "selkies.gpu_probe"] + args,
                              env=env, capture_output=True, text=True, timeout=PROBE_TIMEOUT)
    except (OSError, subprocess.SubprocessError):
        return ""
    return done.stdout.strip() if done.returncode == 0 else ""


def renderer_of(node: str, vendor_file: str, extra: Optional[Dict[str, str]] = None) -> str:
    """GL_RENDERER of a vendor's EGL brought up on `node` through the device platform, or ""."""
    return _helper(["--renderer", node], _client_env(vendor_file, extra))


def window_test(display: str, vendor_file: str, extra: Optional[Dict[str, str]] = None) -> Tuple[str, bool]:
    """A vendor's EGL on an X display: the renderer it gives a window, and whether it presented."""
    env = _client_env(vendor_file, extra)
    env["DISPLAY"] = display
    renderer, _, presented = _helper(["--window"], env).partition("\t")
    return renderer, presented == "true"


@contextlib.contextmanager
def throwaway_display(node: str = "") -> Iterator[str]:
    """An X server of this process's own for the window probes; "" without one.

    Args:
        node: The render node to draw on through glamor and DRI3, the way the
            session's framebuffer server does; "" for a software server.
    """
    xvfb = shutil.which("Xvfb")
    if not xvfb:
        yield ""
        return
    # The socket directory is the server's to make only as root.
    try:
        os.makedirs("/tmp/.X11-unix", mode=0o1777, exist_ok=True)
    except OSError:
        pass
    env = {k: v for k, v in os.environ.items() if k not in CLIENT_ENV}
    command = [xvfb, "-screen", "0", "64x64x24", "-nolisten", "tcp"]
    if node:
        command += ["-glamor", "-dri", node]
    else:
        # A software server probes GLX on Mesa's EGL: a vendor library's GBM has
        # nothing to offer it and crashes it, so the server is pinned to Mesa's ICD.
        mesa = dict(egl_vendors()).get(MESA_VENDOR)
        if mesa:
            env["__EGL_VENDOR_LIBRARY_FILENAMES"] = mesa
    reader, writer = os.pipe()
    try:
        server = subprocess.Popen(command + ["-displayfd", str(writer)],
                                  pass_fds=(writer,), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
    except OSError:
        os.close(reader)
        os.close(writer)
        yield ""
        return
    os.close(writer)
    number = b""
    try:
        while b"\n" not in number and select.select([reader], [], [], PROBE_TIMEOUT)[0]:
            chunk = os.read(reader, 16)
            if not chunk:
                break
            number += chunk
    finally:
        os.close(reader)
    try:
        yield f":{number.decode().strip()}" if number.strip().isdigit() else ""
    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()


def vulkan_presents(display: str, node: str) -> Optional[bool]:
    """Whether the GPU's Vulkan driver gives a window on `display` a swapchain; None when unanswerable."""
    env = {k: v for k, v in os.environ.items() if k not in CLIENT_ENV}
    env["DISPLAY"] = display
    answer = _helper(["--vulkan-window", node], env)
    return {"true": True, "false": False}.get(answer)


def client_facts(found: Dict[str, str],
                 renderer: Callable[..., str] = renderer_of,
                 window: Callable[..., Tuple[str, bool]] = window_test,
                 display: Callable[[str], "contextlib.AbstractContextManager[str]"] = throwaway_display,
                 vulkan: Callable[[str, str], Optional[bool]] = vulkan_presents,
                 vendors: Optional[List[Tuple[str, str]]] = None) -> Dict[str, str]:
    """How the session's applications reach the GPU `found` describes.

    Args:
        found: The variables `facts` derived.
        renderer, window, display, vulkan, vendors: The probes and the vendor
            list; a test injects its own.

    Returns:
        The client-side variables, all present and all strings.
    """
    out = {"SELKIES_GPU_GL_VENDOR": "", "SELKIES_GPU_EGL_X11": "",
           "SELKIES_GPU_MESA_DRIVER": "", "SELKIES_GPU_VULKAN_PRESENTS": ""}
    node = found.get("SELKIES_GPU_RENDER_NODE", "")
    if not found.get("SELKIES_GPU_DRIVER"):
        return out
    vendors = egl_vendors() if vendors is None else vendors
    for name, icd in vendors:
        seen = renderer(node, icd)
        if seen and not software_renderer(seen):
            out["SELKIES_GPU_GL_VENDOR"] = name
            break
    mesa = dict(vendors).get(MESA_VENDOR, "")
    if out["SELKIES_GPU_GL_VENDOR"] == MESA_VENDOR:
        out["SELKIES_GPU_MESA_DRIVER"] = "native"
    # The window paths are measured on a server like the session's: glamor on
    # the node where the compositor's renderer came up on it, software otherwise.
    with display(node if found.get("SELKIES_GPU_ACCELERATED") == "true" else "") as x_display:
        if not x_display:
            return out
        for name, icd in vendors:
            seen, _ = window(x_display, icd)
            if seen:
                out["SELKIES_GPU_EGL_X11"] = name
                break
        if mesa and out["SELKIES_GPU_MESA_DRIVER"] != "native":
            select_gpu = vk_device_select(node)
            extra = dict(ZINK_ENV, **({"MESA_VK_DEVICE_SELECT": select_gpu} if select_gpu else {}))
            seen, _ = window(x_display, mesa, extra)
            if seen.lower().startswith("zink"):
                out["SELKIES_GPU_MESA_DRIVER"] = "zink"
                # Zink's own swap reports success whether or not the driver
                # gave it a swapchain, so the driver is asked directly.
                presented = vulkan(x_display, node)
                out["SELKIES_GPU_VULKAN_PRESENTS"] = "" if presented is None else ("true" if presented else "false")
    return out


class _EGL:
    """The few EGL and GL entry points the helpers call, through glvnd."""

    def __init__(self) -> None:
        self.egl = ctypes.CDLL("libEGL.so.1")
        c = ctypes
        self.egl.eglGetProcAddress.restype = c.c_void_p
        self.egl.eglGetProcAddress.argtypes = [c.c_char_p]
        self.egl.eglGetDisplay.restype = c.c_void_p
        self.egl.eglGetDisplay.argtypes = [c.c_void_p]
        self.egl.eglInitialize.restype = c.c_uint
        self.egl.eglInitialize.argtypes = [c.c_void_p, c.POINTER(c.c_int), c.POINTER(c.c_int)]
        self.egl.eglBindAPI.restype = c.c_uint
        self.egl.eglBindAPI.argtypes = [c.c_uint]
        self.egl.eglChooseConfig.restype = c.c_uint
        self.egl.eglChooseConfig.argtypes = [c.c_void_p, c.POINTER(c.c_int), c.POINTER(c.c_void_p), c.c_int, c.POINTER(c.c_int)]
        self.egl.eglCreateContext.restype = c.c_void_p
        self.egl.eglCreateContext.argtypes = [c.c_void_p, c.c_void_p, c.c_void_p, c.POINTER(c.c_int)]
        self.egl.eglCreatePbufferSurface.restype = c.c_void_p
        self.egl.eglCreatePbufferSurface.argtypes = [c.c_void_p, c.c_void_p, c.POINTER(c.c_int)]
        self.egl.eglCreateWindowSurface.restype = c.c_void_p
        self.egl.eglCreateWindowSurface.argtypes = [c.c_void_p, c.c_void_p, c.c_ulong, c.POINTER(c.c_int)]
        self.egl.eglMakeCurrent.restype = c.c_uint
        self.egl.eglMakeCurrent.argtypes = [c.c_void_p, c.c_void_p, c.c_void_p, c.c_void_p]
        self.egl.eglSwapBuffers.restype = c.c_uint
        self.egl.eglSwapBuffers.argtypes = [c.c_void_p, c.c_void_p]
        self.egl.eglTerminate.argtypes = [c.c_void_p]
        for name in ("libOpenGL.so.0", "libGL.so.1"):
            try:
                self.gl = ctypes.CDLL(name)
                break
            except OSError:
                continue
        else:
            raise OSError("no GL dispatch library")
        self.gl.glGetString.restype = c.c_char_p
        self.gl.glGetString.argtypes = [c.c_uint]

    def proc(self, name: str, restype, argtypes):
        address = self.egl.eglGetProcAddress(name.encode())
        if not address:
            raise OSError(f"{name} is not available")
        return ctypes.CFUNCTYPE(restype, *argtypes)(address)

    def attribs(self, *values: int):
        return (ctypes.c_int * (len(values) + 1))(*values, EGL_NONE)

    def context(self, display, surface_type: int) -> Tuple[Optional[ctypes.c_void_p], Optional[ctypes.c_void_p]]:
        """An OpenGL context on `display` and a config that can back `surface_type`."""
        if not self.egl.eglInitialize(display, None, None) or not self.egl.eglBindAPI(EGL_OPENGL_API):
            return None, None
        config = ctypes.c_void_p()
        count = ctypes.c_int(0)
        chosen = self.attribs(EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_SURFACE_TYPE, surface_type)
        if not self.egl.eglChooseConfig(display, chosen, ctypes.byref(config), 1, ctypes.byref(count)) or not count.value:
            return None, None
        context = self.egl.eglCreateContext(display, config, None, self.attribs())
        return (ctypes.c_void_p(context) if context else None), config

    def renderer(self, display, context, surface) -> str:
        if not self.egl.eglMakeCurrent(display, surface, surface, context):
            # Vendors without surfaceless contexts get a pbuffer to answer on.
            return ""
        seen = self.gl.glGetString(GL_RENDERER)
        return seen.decode(errors="replace") if seen else ""


def _print_device_renderer(node: str) -> int:
    """Helper: GL_RENDERER of the forced vendor's EGL on `node`'s device, or nothing."""
    try:
        api = _EGL()
        query_devices = api.proc("eglQueryDevicesEXT", ctypes.c_uint,
                                 [ctypes.c_int, ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_int)])
        device_string = api.proc("eglQueryDeviceStringEXT", ctypes.c_char_p, [ctypes.c_void_p, ctypes.c_int])
        platform_display = api.proc("eglGetPlatformDisplayEXT", ctypes.c_void_p,
                                    [ctypes.c_uint, ctypes.c_void_p, ctypes.POINTER(ctypes.c_int)])
    except OSError:
        return 1
    count = ctypes.c_int(0)
    if not query_devices(0, None, ctypes.byref(count)) or count.value <= 0:
        return 1
    devices = (ctypes.c_void_p * count.value)()
    query_devices(count.value, devices, ctypes.byref(count))
    for device in list(devices)[:count.value]:
        files = {device_string(device, attr) for attr in (EGL_DRM_RENDER_NODE_FILE_EXT, EGL_DRM_DEVICE_FILE_EXT)}
        # A node picks its own device; without one, every device is tried.
        if node and node.encode() not in files:
            continue
        display = platform_display(EGL_PLATFORM_DEVICE_EXT, device, None)
        if not display:
            continue
        context, config = api.context(display, EGL_PBUFFER_BIT)
        if context is None:
            api.egl.eglTerminate(display)
            continue
        seen = api.renderer(display, context, None)
        if not seen:
            surface = api.egl.eglCreatePbufferSurface(display, config, api.attribs(EGL_WIDTH, 1, EGL_HEIGHT, 1))
            seen = api.renderer(display, context, surface) if surface else ""
        api.egl.eglMakeCurrent(display, None, None, None)
        api.egl.eglTerminate(display)
        if seen:
            print(seen)
            return 0
    return 1


def _print_window_test() -> int:
    """Helper: `renderer<TAB>presented` for the forced vendor's EGL on $DISPLAY, or nothing."""
    c = ctypes
    try:
        api = _EGL()
        x11 = c.CDLL("libX11.so.6")
    except OSError:
        return 1
    x11.XOpenDisplay.restype = c.c_void_p
    x11.XOpenDisplay.argtypes = [c.c_char_p]
    x11.XDefaultScreen.argtypes = [c.c_void_p]
    x11.XRootWindow.restype = c.c_ulong
    x11.XRootWindow.argtypes = [c.c_void_p, c.c_int]
    x11.XCreateSimpleWindow.restype = c.c_ulong
    x11.XCreateSimpleWindow.argtypes = [c.c_void_p, c.c_ulong, c.c_int, c.c_int, c.c_uint, c.c_uint, c.c_uint, c.c_ulong, c.c_ulong]
    x11.XMapWindow.argtypes = [c.c_void_p, c.c_ulong]
    x11.XSync.argtypes = [c.c_void_p, c.c_int]
    xdisplay = x11.XOpenDisplay(None)
    if not xdisplay:
        return 1
    window = x11.XCreateSimpleWindow(xdisplay, x11.XRootWindow(xdisplay, x11.XDefaultScreen(xdisplay)), 0, 0, 64, 64, 0, 0, 0)
    x11.XMapWindow(xdisplay, window)
    x11.XSync(xdisplay, 0)
    display = api.egl.eglGetDisplay(xdisplay)
    if not display:
        return 1
    context, config = api.context(display, EGL_WINDOW_BIT)
    if context is None:
        return 1
    surface = api.egl.eglCreateWindowSurface(display, config, window, None)
    seen = api.renderer(display, context, surface) if surface else ""
    presented = bool(seen and api.egl.eglSwapBuffers(display, surface) and api.egl.eglSwapBuffers(display, surface))
    if not seen:
        # The renderer is worth knowing even where no window can be presented.
        seen = api.renderer(display, context, None)
    if not seen:
        return 1
    print(f"{seen}\t{'true' if presented else 'false'}")
    return 0


VK_STRUCTURE_TYPE_APPLICATION_INFO = 0
VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO = 1
VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO = 2
VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO = 3
VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR = 1000001000
VK_STRUCTURE_TYPE_XCB_SURFACE_CREATE_INFO_KHR = 1000005000
VK_SUCCESS = 0
VK_PHYSICAL_DEVICE_TYPE_CPU = 4
VK_QUEUE_GRAPHICS_BIT = 0x1
VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT = 0x10
VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR = 0x1
VK_PRESENT_MODE_FIFO_KHR = 2


class _VkExtent2D(ctypes.Structure):
    _fields_ = [("width", ctypes.c_uint32), ("height", ctypes.c_uint32)]


class _VkApplicationInfo(ctypes.Structure):
    _fields_ = [("sType", ctypes.c_int), ("pNext", ctypes.c_void_p), ("pApplicationName", ctypes.c_char_p),
                ("applicationVersion", ctypes.c_uint32), ("pEngineName", ctypes.c_char_p),
                ("engineVersion", ctypes.c_uint32), ("apiVersion", ctypes.c_uint32)]


class _VkInstanceCreateInfo(ctypes.Structure):
    _fields_ = [("sType", ctypes.c_int), ("pNext", ctypes.c_void_p), ("flags", ctypes.c_uint32),
                ("pApplicationInfo", ctypes.POINTER(_VkApplicationInfo)),
                ("enabledLayerCount", ctypes.c_uint32), ("ppEnabledLayerNames", ctypes.POINTER(ctypes.c_char_p)),
                ("enabledExtensionCount", ctypes.c_uint32), ("ppEnabledExtensionNames", ctypes.POINTER(ctypes.c_char_p))]


class _VkXcbSurfaceCreateInfoKHR(ctypes.Structure):
    _fields_ = [("sType", ctypes.c_int), ("pNext", ctypes.c_void_p), ("flags", ctypes.c_uint32),
                ("connection", ctypes.c_void_p), ("window", ctypes.c_uint32)]


class _VkDeviceQueueCreateInfo(ctypes.Structure):
    _fields_ = [("sType", ctypes.c_int), ("pNext", ctypes.c_void_p), ("flags", ctypes.c_uint32),
                ("queueFamilyIndex", ctypes.c_uint32), ("queueCount", ctypes.c_uint32),
                ("pQueuePriorities", ctypes.POINTER(ctypes.c_float))]


class _VkDeviceCreateInfo(ctypes.Structure):
    _fields_ = [("sType", ctypes.c_int), ("pNext", ctypes.c_void_p), ("flags", ctypes.c_uint32),
                ("queueCreateInfoCount", ctypes.c_uint32), ("pQueueCreateInfos", ctypes.POINTER(_VkDeviceQueueCreateInfo)),
                ("enabledLayerCount", ctypes.c_uint32), ("ppEnabledLayerNames", ctypes.POINTER(ctypes.c_char_p)),
                ("enabledExtensionCount", ctypes.c_uint32), ("ppEnabledExtensionNames", ctypes.POINTER(ctypes.c_char_p)),
                ("pEnabledFeatures", ctypes.c_void_p)]


class _VkQueueFamilyProperties(ctypes.Structure):
    _fields_ = [("queueFlags", ctypes.c_uint32), ("queueCount", ctypes.c_uint32),
                ("timestampValidBits", ctypes.c_uint32), ("minImageTransferGranularity", ctypes.c_uint32 * 3)]


class _VkSurfaceCapabilitiesKHR(ctypes.Structure):
    _fields_ = [("minImageCount", ctypes.c_uint32), ("maxImageCount", ctypes.c_uint32),
                ("currentExtent", _VkExtent2D), ("minImageExtent", _VkExtent2D), ("maxImageExtent", _VkExtent2D),
                ("maxImageArrayLayers", ctypes.c_uint32), ("supportedTransforms", ctypes.c_uint32),
                ("currentTransform", ctypes.c_uint32), ("supportedCompositeAlpha", ctypes.c_uint32),
                ("supportedUsageFlags", ctypes.c_uint32)]


class _VkSurfaceFormatKHR(ctypes.Structure):
    _fields_ = [("format", ctypes.c_uint32), ("colorSpace", ctypes.c_uint32)]


class _VkSwapchainCreateInfoKHR(ctypes.Structure):
    _fields_ = [("sType", ctypes.c_int), ("pNext", ctypes.c_void_p), ("flags", ctypes.c_uint32),
                ("surface", ctypes.c_uint64), ("minImageCount", ctypes.c_uint32), ("imageFormat", ctypes.c_uint32),
                ("imageColorSpace", ctypes.c_uint32), ("imageExtent", _VkExtent2D), ("imageArrayLayers", ctypes.c_uint32),
                ("imageUsage", ctypes.c_uint32), ("imageSharingMode", ctypes.c_int),
                ("queueFamilyIndexCount", ctypes.c_uint32), ("pQueueFamilyIndices", ctypes.POINTER(ctypes.c_uint32)),
                ("preTransform", ctypes.c_uint32), ("compositeAlpha", ctypes.c_uint32), ("presentMode", ctypes.c_int),
                ("clipped", ctypes.c_uint32), ("oldSwapchain", ctypes.c_uint64)]


def _names(*values: str):
    return (ctypes.c_char_p * len(values))(*[v.encode() for v in values])


def _print_vulkan_window(node: str) -> int:
    """Helper: whether the GPU's Vulkan driver gives a window on $DISPLAY a swapchain: true/false."""
    c = ctypes
    try:
        vk = c.CDLL("libvulkan.so.1")
        x11 = c.CDLL("libX11.so.6")
        x11_xcb = c.CDLL("libX11-xcb.so.1")
    except OSError:
        return 1
    vk.vkGetInstanceProcAddr.restype = c.c_void_p
    vk.vkGetInstanceProcAddr.argtypes = [c.c_void_p, c.c_char_p]
    vk.vkGetDeviceProcAddr.restype = c.c_void_p
    vk.vkGetDeviceProcAddr.argtypes = [c.c_void_p, c.c_char_p]
    vk.vkCreateInstance.argtypes = [c.POINTER(_VkInstanceCreateInfo), c.c_void_p, c.POINTER(c.c_void_p)]
    vk.vkEnumeratePhysicalDevices.argtypes = [c.c_void_p, c.POINTER(c.c_uint32), c.POINTER(c.c_void_p)]
    vk.vkGetPhysicalDeviceProperties.argtypes = [c.c_void_p, c.c_void_p]
    vk.vkGetPhysicalDeviceQueueFamilyProperties.argtypes = [c.c_void_p, c.POINTER(c.c_uint32), c.POINTER(_VkQueueFamilyProperties)]
    vk.vkCreateDevice.argtypes = [c.c_void_p, c.POINTER(_VkDeviceCreateInfo), c.c_void_p, c.POINTER(c.c_void_p)]
    x11.XOpenDisplay.restype = c.c_void_p
    x11.XOpenDisplay.argtypes = [c.c_char_p]
    x11.XDefaultScreen.argtypes = [c.c_void_p]
    x11.XRootWindow.restype = c.c_ulong
    x11.XRootWindow.argtypes = [c.c_void_p, c.c_int]
    x11.XCreateSimpleWindow.restype = c.c_ulong
    x11.XCreateSimpleWindow.argtypes = [c.c_void_p, c.c_ulong, c.c_int, c.c_int, c.c_uint, c.c_uint, c.c_uint, c.c_ulong, c.c_ulong]
    x11.XMapWindow.argtypes = [c.c_void_p, c.c_ulong]
    x11.XSync.argtypes = [c.c_void_p, c.c_int]
    x11_xcb.XGetXCBConnection.restype = c.c_void_p
    x11_xcb.XGetXCBConnection.argtypes = [c.c_void_p]

    def proc(owner, name: str, getter, restype, argtypes):
        address = getter(owner, name.encode())
        if not address:
            raise OSError(f"{name} is not available")
        return c.CFUNCTYPE(restype, *argtypes)(address)

    app = _VkApplicationInfo(VK_STRUCTURE_TYPE_APPLICATION_INFO, None, b"selkies-gpu-probe", 1, b"selkies-gpu-probe", 1, (1 << 22) | (1 << 12))
    extensions = _names("VK_KHR_surface", "VK_KHR_xcb_surface")
    info = _VkInstanceCreateInfo(VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, None, 0, c.pointer(app), 0, None, 2, extensions)
    instance = c.c_void_p()
    if vk.vkCreateInstance(c.byref(info), None, c.byref(instance)) != VK_SUCCESS:
        return 1
    count = c.c_uint32(0)
    vk.vkEnumeratePhysicalDevices(instance, c.byref(count), None)
    devices = (c.c_void_p * max(count.value, 1))()
    vk.vkEnumeratePhysicalDevices(instance, c.byref(count), devices)
    # The node's own GPU by its PCI ids, else the first device that is not a CPU.
    wanted = vk_device_select(node)
    chosen = None
    for device in list(devices)[:count.value]:
        properties = c.create_string_buffer(1024)
        vk.vkGetPhysicalDeviceProperties(device, properties)
        vendor, product, kind = (int.from_bytes(properties.raw[at:at + 4], "little") for at in (8, 12, 16))
        if wanted:
            if f"{vendor:04x}:{product:04x}" == wanted:
                chosen = device
                break
        elif kind != VK_PHYSICAL_DEVICE_TYPE_CPU:
            chosen = device
            break
    if chosen is None:
        return 1
    xdisplay = x11.XOpenDisplay(None)
    if not xdisplay:
        return 1
    window = x11.XCreateSimpleWindow(xdisplay, x11.XRootWindow(xdisplay, x11.XDefaultScreen(xdisplay)), 0, 0, 64, 64, 0, 0, 0)
    x11.XMapWindow(xdisplay, window)
    x11.XSync(xdisplay, 0)
    try:
        create_surface = proc(instance, "vkCreateXcbSurfaceKHR", vk.vkGetInstanceProcAddr, c.c_int,
                              [c.c_void_p, c.POINTER(_VkXcbSurfaceCreateInfoKHR), c.c_void_p, c.POINTER(c.c_uint64)])
        surface_support = proc(instance, "vkGetPhysicalDeviceSurfaceSupportKHR", vk.vkGetInstanceProcAddr, c.c_int,
                               [c.c_void_p, c.c_uint32, c.c_uint64, c.POINTER(c.c_uint32)])
        capabilities_of = proc(instance, "vkGetPhysicalDeviceSurfaceCapabilitiesKHR", vk.vkGetInstanceProcAddr, c.c_int,
                               [c.c_void_p, c.c_uint64, c.POINTER(_VkSurfaceCapabilitiesKHR)])
        formats_of = proc(instance, "vkGetPhysicalDeviceSurfaceFormatsKHR", vk.vkGetInstanceProcAddr, c.c_int,
                          [c.c_void_p, c.c_uint64, c.POINTER(c.c_uint32), c.POINTER(_VkSurfaceFormatKHR)])
    except OSError:
        return 1
    surface_info = _VkXcbSurfaceCreateInfoKHR(VK_STRUCTURE_TYPE_XCB_SURFACE_CREATE_INFO_KHR, None, 0,
                                             x11_xcb.XGetXCBConnection(xdisplay), window & 0xFFFFFFFF)
    surface = c.c_uint64(0)
    if create_surface(instance, c.byref(surface_info), None, c.byref(surface)) != VK_SUCCESS:
        print("false")
        return 0
    families = c.c_uint32(0)
    vk.vkGetPhysicalDeviceQueueFamilyProperties(chosen, c.byref(families), None)
    properties = (_VkQueueFamilyProperties * max(families.value, 1))()
    vk.vkGetPhysicalDeviceQueueFamilyProperties(chosen, c.byref(families), properties)
    family = None
    for index in range(families.value):
        supported = c.c_uint32(0)
        surface_support(chosen, index, surface, c.byref(supported))
        if properties[index].queueFlags & VK_QUEUE_GRAPHICS_BIT and supported.value:
            family = index
            break
    if family is None:
        print("false")
        return 0
    priority = (c.c_float * 1)(1.0)
    queue = _VkDeviceQueueCreateInfo(VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, None, 0, family, 1, priority)
    device_extensions = _names("VK_KHR_swapchain")
    device_info = _VkDeviceCreateInfo(VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, None, 0, 1, c.pointer(queue), 0, None, 1, device_extensions, None)
    device = c.c_void_p()
    if vk.vkCreateDevice(chosen, c.byref(device_info), None, c.byref(device)) != VK_SUCCESS:
        print("false")
        return 0
    capabilities = _VkSurfaceCapabilitiesKHR()
    count = c.c_uint32(0)
    if capabilities_of(chosen, surface, c.byref(capabilities)) != VK_SUCCESS \
            or formats_of(chosen, surface, c.byref(count), None) != VK_SUCCESS or not count.value:
        print("false")
        return 0
    formats = (_VkSurfaceFormatKHR * count.value)()
    formats_of(chosen, surface, c.byref(count), formats)
    try:
        create_swapchain = proc(device, "vkCreateSwapchainKHR", vk.vkGetDeviceProcAddr, c.c_int,
                                [c.c_void_p, c.POINTER(_VkSwapchainCreateInfoKHR), c.c_void_p, c.POINTER(c.c_uint64)])
    except OSError:
        print("false")
        return 0
    extent = capabilities.currentExtent
    if extent.width == 0xFFFFFFFF:
        extent = _VkExtent2D(64, 64)
    swapchain_info = _VkSwapchainCreateInfoKHR(
        VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR, None, 0, surface.value, max(2, capabilities.minImageCount),
        formats[0].format, formats[0].colorSpace, extent, 1, VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT, 0, 0, None,
        capabilities.currentTransform, VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR, VK_PRESENT_MODE_FIFO_KHR, 1, 0)
    swapchain = c.c_uint64(0)
    print("true" if create_swapchain(device, c.byref(swapchain_info), None, c.byref(swapchain)) == VK_SUCCESS else "false")
    return 0


def main() -> int:
    """Probe the GPU via pixelflux and print what it found.

    Returns:
        Process exit code: 0 with the facts on stdout, non-zero (and nothing
        printed) when no report could be obtained.
    """
    if sys.argv[1:2] == ["--renderer"]:
        return _print_device_renderer(sys.argv[2] if len(sys.argv) > 2 else "")
    if sys.argv[1:2] == ["--window"]:
        return _print_window_test()
    if sys.argv[1:2] == ["--vulkan-window"]:
        return _print_vulkan_window(sys.argv[2] if len(sys.argv) > 2 else "")
    # Imported here so the module stays usable where pixelflux is absent, and so
    # the settings parser never sees this tool's own invocation.
    from .settings import settings
    try:
        import pixelflux
        report = pixelflux.probe_wayland_gpu(
            str(getattr(settings, "render_dri", "") or ""),
            str(getattr(settings, "auto_gpu", "") or ""),
        )
    except Exception as e:
        print(f"GPU: support cannot be determined ({e})", file=sys.stderr)
        return 1
    found = facts(report)
    print(summary(report, found), file=sys.stderr)
    found.update(client_facts(found))
    for name, value in found.items():
        print(f"{name}={value}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
