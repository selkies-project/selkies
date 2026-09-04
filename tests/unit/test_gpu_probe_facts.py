#!/usr/bin/env python3
"""The client-path facts the GPU probe measures, on stand-in vendors.

The probe brings each glvnd vendor's EGL up the way an application would and
reports which one drives the session's GPU, whether Mesa reaches it natively
or through Zink, and whether Zink could present into a window. The measuring
itself runs in helper processes against real drivers; what is pinned here is
the reading of their answers, which is where a wrong fact would come from: a
software renderer mistaken for a vendor's, a vendor listed twice, a Zink that
initialises but cannot present reported as a working path.
"""
import contextlib
import json
import os
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, TESTS)
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))
import helpers as H  # noqa: E402

from selkies import gpu_probe as GP  # noqa: E402

res = H.Results("gpu-probe-facts")
NODE = "/dev/dri/renderD128"
FOUND = {"SELKIES_GPU_RENDER_NODE": NODE, "SELKIES_GPU_DRIVER": "nvidia",
         "SELKIES_GPU_PRESENT": "true", "SELKIES_GPU_ACCELERATED": "true"}
VENDORS = [("nvidia", "/icd/10_nvidia.json"), ("mesa", "/icd/50_mesa.json")]


DISPLAYS = []


@contextlib.contextmanager
def display(node: str = ""):
    DISPLAYS.append(node)
    yield ":99"


def vulkan(answer):
    return lambda x_display, node: answer


def probes(device: dict, windows: dict):
    """Stand-in helpers answering from `device` (icd -> renderer) and `windows` (icd -> (renderer, presented))."""
    calls = []

    def renderer(node, icd, extra=None):
        calls.append(("renderer", icd, extra))
        return device.get(icd, "")

    def window(x_display, icd, extra=None):
        calls.append(("window", icd, extra))
        key = (icd, "zink") if extra and extra.get("GALLIUM_DRIVER") == "zink" else icd
        return windows.get(key, ("", False))
    return renderer, window, calls


# --- the vendor list, read the way glvnd reads it ---------------------------
with tempfile.TemporaryDirectory() as tmp:
    etc, usr = os.path.join(tmp, "etc"), os.path.join(tmp, "usr")
    for directory, files in ((etc, {"10_nvidia.json": "libEGL_nvidia.so.0"}),
                             (usr, {"10_nvidia.json": "libEGL_nvidia.so.0", "50_mesa.json": "libEGL_mesa.so.0",
                                    "60_broken.json": None, "70_odd.json": "libGLES_x.so", "README": "x"})):
        os.makedirs(directory)
        for name, library in files.items():
            with open(os.path.join(directory, name), "w") as fh:
                fh.write(json.dumps({"file_format_version": "1.0.0", "ICD": {"library_path": library}})
                         if library else "{not json")
    listed = GP.egl_vendors((etc, usr), environ={})
    res.check("vendors are read from the ICD directories in glvnd's order, one per name",
              [name for name, _ in listed] == ["nvidia", "mesa"], listed)
    res.check("the first directory naming a vendor is the one kept",
              dict(listed)["nvidia"].startswith(etc), listed)
    res.check("unreadable and non-EGL ICDs are skipped", "odd" not in dict(listed) and "broken" not in dict(listed))
    res.check("__EGL_VENDOR_LIBRARY_DIRS overrides the directories, as it does for glvnd",
              GP.egl_vendors((etc, usr), environ={"__EGL_VENDOR_LIBRARY_DIRS": usr})[0][1].startswith(usr))
    res.check("no directory at all is an empty list", GP.egl_vendors((os.path.join(tmp, "none"),), environ={}) == [])

# --- what a renderer string says ---------------------------------------------
res.check("software rasterisers are told from a GPU's renderer",
          GP.software_renderer("llvmpipe (LLVM 21.1.8, 256 bits)") and GP.software_renderer("softpipe")
          and not GP.software_renderer("Tesla P100-SXM2-16GB/PCIe/SSE2")
          and not GP.software_renderer("zink Vulkan 1.4(Tesla P100 (NVIDIA_PROPRIETARY))"))

# --- the GPU a render node names for Mesa's device selection -----------------
with tempfile.TemporaryDirectory() as fake:
    device = os.path.join(fake, "renderD128", "device")
    os.makedirs(device)
    for name, value in (("vendor", "0x10de\n"), ("device", "0x15f8\n")):
        with open(os.path.join(device, name), "w") as fh:
            fh.write(value)
    res.check("a node's GPU is named as vendor:device for Mesa's device selection",
              GP.vk_device_select(NODE, fake) == "10de:15f8", GP.vk_device_select(NODE, fake))
    res.check("a node with no identity names none",
              GP.vk_device_select("/dev/dri/renderD129", fake) == "" and GP.vk_device_select("", fake) == "")

# --- the facts, from stand-in answers ----------------------------------------
KEYS = {"SELKIES_GPU_GL_VENDOR", "SELKIES_GPU_EGL_X11", "SELKIES_GPU_MESA_DRIVER", "SELKIES_GPU_VULKAN_PRESENTS"}

renderer, window, calls = probes(
    {"/icd/10_nvidia.json": "Tesla P100-SXM2-16GB/PCIe/SSE2", "/icd/50_mesa.json": "llvmpipe (LLVM 21.1.8, 256 bits)"},
    {"/icd/50_mesa.json": ("llvmpipe (LLVM 21.1.8, 256 bits)", True),
     ("/icd/50_mesa.json", "zink"): ("zink Vulkan 1.4(Tesla P100 (NVIDIA_PROPRIETARY))", True)})
got = GP.client_facts(FOUND, renderer, window, display, vulkan(True), VENDORS)
res.check("every client fact is reported", set(got) == KEYS, sorted(got))
res.check("the window probes run on a server drawing on the node the renderer came up on",
          DISPLAYS[-1] == NODE, DISPLAYS)
res.check("the vendor whose EGL renders on the node is the GL vendor",
          got["SELKIES_GPU_GL_VENDOR"] == "nvidia", got)
res.check("a vendor that cannot initialise on X leaves EGL there to the next one",
          got["SELKIES_GPU_EGL_X11"] == "mesa", got)
res.check("Mesa answering with Zink and the driver giving a window a swapchain is the Zink path",
          got["SELKIES_GPU_MESA_DRIVER"] == "zink" and got["SELKIES_GPU_VULKAN_PRESENTS"] == "true", got)
res.check("the Zink window is asked for on the node's own GPU",
          any(kind == "window" and extra and extra.get("GALLIUM_DRIVER") == "zink" for kind, _, extra in calls), calls)

renderer, window, _ = probes(
    {"/icd/10_nvidia.json": "Tesla P100-SXM2-16GB/PCIe/SSE2", "/icd/50_mesa.json": ""},
    {"/icd/10_nvidia.json": ("Tesla P100-SXM2-16GB/PCIe/SSE2", True),
     "/icd/50_mesa.json": ("llvmpipe", True),
     ("/icd/50_mesa.json", "zink"): ("zink Vulkan 1.4(Tesla P100 (NVIDIA_PROPRIETARY))", True)})
got = GP.client_facts(FOUND, renderer, window, display, vulkan(False), VENDORS)
res.check("a Zink that initialises while the driver refuses a swapchain is reported as exactly that, whatever Zink's swap said",
          got["SELKIES_GPU_MESA_DRIVER"] == "zink" and got["SELKIES_GPU_VULKAN_PRESENTS"] == "false", got)
res.check("a vendor whose EGL initialises on X is EGL's vendor there", got["SELKIES_GPU_EGL_X11"] == "nvidia", got)

renderer, window, _ = probes(
    {"/icd/10_nvidia.json": "", "/icd/50_mesa.json": "AMD Radeon RX 7900 XTX (radeonsi)"},
    {"/icd/50_mesa.json": ("AMD Radeon RX 7900 XTX (radeonsi)", True)})
got = GP.client_facts(dict(FOUND, SELKIES_GPU_DRIVER="amdgpu"), renderer, window, display, vulkan(None), VENDORS)
res.check("a GPU Mesa drives itself is Mesa's vendor on its native driver, and Zink is never asked",
          got["SELKIES_GPU_GL_VENDOR"] == "mesa" and got["SELKIES_GPU_MESA_DRIVER"] == "native"
          and got["SELKIES_GPU_VULKAN_PRESENTS"] == "", got)

renderer, window, _ = probes(
    {"/icd/10_nvidia.json": "", "/icd/50_mesa.json": "llvmpipe (LLVM 21.1.8, 256 bits)"},
    {"/icd/50_mesa.json": ("llvmpipe (LLVM 21.1.8, 256 bits)", True),
     ("/icd/50_mesa.json", "zink"): ("llvmpipe (LLVM 21.1.8, 256 bits)", True)})
got = GP.client_facts(FOUND, renderer, window, display, vulkan(True), VENDORS)
res.check("no vendor rendering on the node and no Zink is software all round",
          got["SELKIES_GPU_GL_VENDOR"] == "" and got["SELKIES_GPU_MESA_DRIVER"] == "", got)


@contextlib.contextmanager
def no_display(node: str = ""):
    yield ""


renderer, window, calls = probes({"/icd/10_nvidia.json": "Tesla P100-SXM2-16GB/PCIe/SSE2"}, {})
got = GP.client_facts(FOUND, renderer, window, no_display, vulkan(True), VENDORS)
res.check("without an X server to run, the window facts stay unanswered and the node's vendor still answers",
          got["SELKIES_GPU_GL_VENDOR"] == "nvidia" and got["SELKIES_GPU_EGL_X11"] == ""
          and got["SELKIES_GPU_MESA_DRIVER"] == "" and not any(kind == "window" for kind, _, _ in calls), got)

got = GP.client_facts(dict(FOUND, SELKIES_GPU_DRIVER="", SELKIES_GPU_RENDER_NODE=""), renderer, window, display, vulkan(True), VENDORS)
renderer, window, _ = probes({"/icd/10_nvidia.json": "Tesla P100-SXM2-16GB/PCIe/SSE2"},
                             {("/icd/50_mesa.json", "zink"): ("zink Vulkan 1.4(Tesla P100 (NVIDIA_PROPRIETARY))", True)})
unanswered = GP.client_facts(dict(FOUND, SELKIES_GPU_ACCELERATED="false"), renderer, window, display, vulkan(None), VENDORS)
res.check("a driver that could not be asked leaves the presentation fact unanswered, on a software server",
          unanswered["SELKIES_GPU_MESA_DRIVER"] == "zink" and unanswered["SELKIES_GPU_VULKAN_PRESENTS"] == ""
          and DISPLAYS[-1] == "", unanswered)
res.check("no GPU asks nothing and answers empty", all(v == "" for v in got.values()) and set(got) == KEYS, got)

sys.exit(0 if res.summary() else 1)
