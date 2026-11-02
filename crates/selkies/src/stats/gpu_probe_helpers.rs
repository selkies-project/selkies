/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The measurements behind the GPU probe's client facts, each run in a child process of this
//! binary because a driver may crash in the attempt: a vendor's EGL brought up on a render
//! node through the device platform (`--renderer`), on an X window (`--window`), and the
//! GPU's Vulkan driver asked for a swapchain on an X window (`--vulkan-window`).
//!
//! EGL, GL, X11, and Vulkan are loaded with `dlopen` at run time, never linked: the server
//! runs on hosts with none of them, and which vendor's EGL answers is exactly what each
//! helper is forced to by `__EGL_VENDOR_LIBRARY_FILENAMES`. The entry points are the few the
//! probes call, typed here as C function pointers.
//!
//! [`RealProbes`] is the [`ClientProbes`] implementation over those children, each with the
//! client-side environment of [`CLIENT_ENV`] stripped so what the caller may already have
//! decided about Mesa and glvnd is what is measured, and the throwaway X server is an Xvfb
//! told its display number through `-displayfd`.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_ulong, c_void};
use std::io::Read;
use std::os::unix::io::FromRawFd;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::ptr;
use std::time::{Duration, Instant};

use super::gpu_probe::{
    CLIENT_ENV, ClientProbes, EGL_VENDOR_DIRS, MESA_VENDOR, PROBE_TIMEOUT, ThrowawayDisplay,
    egl_vendors,
};
use super::resources::run_with_timeout;

const EGL_PLATFORM_DEVICE_EXT: c_uint = 0x313F;
const EGL_DRM_DEVICE_FILE_EXT: c_int = 0x3233;
const EGL_DRM_RENDER_NODE_FILE_EXT: c_int = 0x3377;
const EGL_OPENGL_API: c_uint = 0x30A2;
const EGL_RENDERABLE_TYPE: c_int = 0x3040;
const EGL_OPENGL_BIT: c_int = 0x0008;
const EGL_SURFACE_TYPE: c_int = 0x3033;
const EGL_PBUFFER_BIT: c_int = 0x0001;
const EGL_WINDOW_BIT: c_int = 0x0004;
const EGL_WIDTH: c_int = 0x3057;
const EGL_HEIGHT: c_int = 0x3056;
const EGL_NONE: c_int = 0x3038;
const GL_RENDERER: c_uint = 0x1F01;

/// A shared library opened for the life of the process.
struct Lib(*mut c_void);

impl Lib {
    fn open(name: &str) -> Option<Lib> {
        let name = CString::new(name).ok()?;
        // SAFETY: `name` is a valid C string; dlopen returns null on failure.
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        (!handle.is_null()).then_some(Lib(handle))
    }

    /// The symbol `name` as a function pointer of type `F`.
    ///
    /// # Safety
    /// `F` must be the C signature of the symbol.
    unsafe fn sym<F: Copy>(&self, name: &str) -> Option<F> {
        let name = CString::new(name).ok()?;
        // SAFETY: the handle is open and `name` a valid C string; a pointer-sized `F` holds
        // the address, which is the caller's contract.
        let address = unsafe { libc::dlsym(self.0, name.as_ptr()) };
        if address.is_null() || std::mem::size_of::<F>() != std::mem::size_of::<*mut c_void>() {
            return None;
        }
        Some(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&address) })
    }
}

type EglGetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type EglGetDisplay = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type EglInitialize = unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> c_uint;
type EglBindApi = unsafe extern "C" fn(c_uint) -> c_uint;
type EglChooseConfig =
    unsafe extern "C" fn(*mut c_void, *const c_int, *mut *mut c_void, c_int, *mut c_int) -> c_uint;
type EglCreateContext =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *const c_int) -> *mut c_void;
type EglCreatePbufferSurface =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_int) -> *mut c_void;
type EglCreateWindowSurface =
    unsafe extern "C" fn(*mut c_void, *mut c_void, c_ulong, *const c_int) -> *mut c_void;
type EglMakeCurrent =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> c_uint;
type EglSwapBuffers = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_uint;
type EglTerminate = unsafe extern "C" fn(*mut c_void) -> c_uint;
type GlGetString = unsafe extern "C" fn(c_uint) -> *const c_char;
type EglQueryDevicesExt = unsafe extern "C" fn(c_int, *mut *mut c_void, *mut c_int) -> c_uint;
type EglQueryDeviceStringExt = unsafe extern "C" fn(*mut c_void, c_int) -> *const c_char;
type EglGetPlatformDisplayExt =
    unsafe extern "C" fn(c_uint, *mut c_void, *const c_int) -> *mut c_void;

/// The few EGL and GL entry points the helpers call, through glvnd.
struct Egl {
    get_proc_address: EglGetProcAddress,
    get_display: EglGetDisplay,
    initialize: EglInitialize,
    bind_api: EglBindApi,
    choose_config: EglChooseConfig,
    create_context: EglCreateContext,
    create_pbuffer_surface: EglCreatePbufferSurface,
    create_window_surface: EglCreateWindowSurface,
    make_current: EglMakeCurrent,
    swap_buffers: EglSwapBuffers,
    terminate: EglTerminate,
    gl_get_string: GlGetString,
    _egl: Lib,
    _gl: Lib,
}

/// A C string the driver owns, copied out; "" for null.
unsafe fn c_string(pointer: *const c_char) -> String {
    if pointer.is_null() {
        return String::new();
    }
    // SAFETY: the caller passes a NUL-terminated string the driver keeps alive.
    unsafe { CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned()
}

impl Egl {
    fn load() -> Option<Self> {
        let egl = Lib::open("libEGL.so.1")?;
        let gl = ["libOpenGL.so.0", "libGL.so.1"]
            .iter()
            .find_map(|name| Lib::open(name))?;
        // SAFETY: each type matches the EGL/GL specification's signature for the symbol.
        unsafe {
            Some(Self {
                get_proc_address: egl.sym("eglGetProcAddress")?,
                get_display: egl.sym("eglGetDisplay")?,
                initialize: egl.sym("eglInitialize")?,
                bind_api: egl.sym("eglBindAPI")?,
                choose_config: egl.sym("eglChooseConfig")?,
                create_context: egl.sym("eglCreateContext")?,
                create_pbuffer_surface: egl.sym("eglCreatePbufferSurface")?,
                create_window_surface: egl.sym("eglCreateWindowSurface")?,
                make_current: egl.sym("eglMakeCurrent")?,
                swap_buffers: egl.sym("eglSwapBuffers")?,
                terminate: egl.sym("eglTerminate")?,
                gl_get_string: gl.sym("glGetString")?,
                _egl: egl,
                _gl: gl,
            })
        }
    }

    /// An extension entry point by name.
    ///
    /// # Safety
    /// `F` must be the C signature of the entry point.
    unsafe fn proc<F: Copy>(&self, name: &str) -> Option<F> {
        let name = CString::new(name).ok()?;
        // SAFETY: `name` is a valid C string; a null address is "not available".
        let address = unsafe { (self.get_proc_address)(name.as_ptr()) };
        if address.is_null() || std::mem::size_of::<F>() != std::mem::size_of::<*mut c_void>() {
            return None;
        }
        Some(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&address) })
    }

    /// An OpenGL context on `display` and a config that can back `surface_type`.
    fn context(
        &self,
        display: *mut c_void,
        surface_type: c_int,
    ) -> Option<(*mut c_void, *mut c_void)> {
        // SAFETY: `display` is a display the driver returned; the attribute lists are
        // EGL_NONE-terminated and outlive the calls.
        unsafe {
            if (self.initialize)(display, ptr::null_mut(), ptr::null_mut()) == 0
                || (self.bind_api)(EGL_OPENGL_API) == 0
            {
                return None;
            }
            let chosen = [
                EGL_RENDERABLE_TYPE,
                EGL_OPENGL_BIT,
                EGL_SURFACE_TYPE,
                surface_type,
                EGL_NONE,
            ];
            let mut config: *mut c_void = ptr::null_mut();
            let mut count: c_int = 0;
            if (self.choose_config)(display, chosen.as_ptr(), &mut config, 1, &mut count) == 0
                || count == 0
            {
                return None;
            }
            let none = [EGL_NONE];
            let context = (self.create_context)(display, config, ptr::null_mut(), none.as_ptr());
            (!context.is_null()).then_some((context, config))
        }
    }

    /// `GL_RENDERER` with `context` current on `surface` (null for a surfaceless context);
    /// "" where it cannot be made current, which is how vendors without surfaceless contexts
    /// ask for a pbuffer to answer on.
    fn renderer(&self, display: *mut c_void, context: *mut c_void, surface: *mut c_void) -> String {
        // SAFETY: the handles came from this display's driver; glGetString returns a string
        // the driver owns or null.
        unsafe {
            if (self.make_current)(display, surface, surface, context) == 0 {
                return String::new();
            }
            c_string((self.gl_get_string)(GL_RENDERER))
        }
    }
}

/// Helper: `GL_RENDERER` of the forced vendor's EGL on `node`'s device, printed, or nothing
/// and exit code 1.
pub(super) fn print_device_renderer(node: &str) -> i32 {
    let Some(api) = Egl::load() else {
        return 1;
    };
    // SAFETY: the types are the EGL_EXT_device_enumeration and EGL_EXT_platform_base
    // signatures.
    let (query_devices, device_string, platform_display) = unsafe {
        match (
            api.proc::<EglQueryDevicesExt>("eglQueryDevicesEXT"),
            api.proc::<EglQueryDeviceStringExt>("eglQueryDeviceStringEXT"),
            api.proc::<EglGetPlatformDisplayExt>("eglGetPlatformDisplayEXT"),
        ) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => return 1,
        }
    };
    let mut count: c_int = 0;
    // SAFETY: a null device array asks for the count alone.
    if unsafe { query_devices(0, ptr::null_mut(), &mut count) } == 0 || count <= 0 {
        return 1;
    }
    let mut devices: Vec<*mut c_void> = vec![ptr::null_mut(); count as usize];
    // SAFETY: the array holds `count` entries.
    unsafe { query_devices(count, devices.as_mut_ptr(), &mut count) };
    for device in devices.iter().take(count.max(0) as usize) {
        // SAFETY: `device` is one the driver enumerated.
        let files: Vec<String> = unsafe {
            [EGL_DRM_RENDER_NODE_FILE_EXT, EGL_DRM_DEVICE_FILE_EXT]
                .iter()
                .map(|attr| c_string(device_string(*device, *attr)))
                .collect()
        };
        // A node picks its own device; without one, every device is tried.
        if !node.is_empty() && !files.iter().any(|f| f == node) {
            continue;
        }
        // SAFETY: the platform display takes the enumerated device and no attributes.
        let display = unsafe { platform_display(EGL_PLATFORM_DEVICE_EXT, *device, ptr::null()) };
        if display.is_null() {
            continue;
        }
        let Some((context, config)) = api.context(display, EGL_PBUFFER_BIT) else {
            // SAFETY: `display` was initialized by `context` or refused; terminate is safe
            // either way.
            unsafe { (api.terminate)(display) };
            continue;
        };
        let mut seen = api.renderer(display, context, ptr::null_mut());
        if seen.is_empty() {
            let attribs = [EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE];
            // SAFETY: `config` backs pbuffers and the attribute list is terminated.
            let surface =
                unsafe { (api.create_pbuffer_surface)(display, config, attribs.as_ptr()) };
            if !surface.is_null() {
                seen = api.renderer(display, context, surface);
            }
        }
        // SAFETY: releasing the context and the display this helper brought up.
        unsafe {
            (api.make_current)(display, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
            (api.terminate)(display);
        }
        if !seen.is_empty() {
            println!("{seen}");
            return 0;
        }
    }
    1
}

type XOpenDisplay = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type XDefaultScreen = unsafe extern "C" fn(*mut c_void) -> c_int;
type XRootWindow = unsafe extern "C" fn(*mut c_void, c_int) -> c_ulong;
type XCreateSimpleWindow = unsafe extern "C" fn(
    *mut c_void,
    c_ulong,
    c_int,
    c_int,
    c_uint,
    c_uint,
    c_uint,
    c_ulong,
    c_ulong,
) -> c_ulong;
type XMapWindow = unsafe extern "C" fn(*mut c_void, c_ulong) -> c_int;
type XSync = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type XGetXcbConnection = unsafe extern "C" fn(*mut c_void) -> *mut c_void;

/// A mapped 64x64 window on `$DISPLAY`, for the window probes.
struct XWindow {
    display: *mut c_void,
    window: c_ulong,
    _x11: Lib,
}

impl XWindow {
    fn open() -> Option<Self> {
        let x11 = Lib::open("libX11.so.6")?;
        // SAFETY: the types are Xlib's signatures; the display pointer is checked for null
        // before the window calls use it.
        unsafe {
            let open: XOpenDisplay = x11.sym("XOpenDisplay")?;
            let default_screen: XDefaultScreen = x11.sym("XDefaultScreen")?;
            let root_window: XRootWindow = x11.sym("XRootWindow")?;
            let create: XCreateSimpleWindow = x11.sym("XCreateSimpleWindow")?;
            let map: XMapWindow = x11.sym("XMapWindow")?;
            let sync: XSync = x11.sym("XSync")?;
            let display = open(ptr::null());
            if display.is_null() {
                return None;
            }
            let root = root_window(display, default_screen(display));
            let window = create(display, root, 0, 0, 64, 64, 0, 0, 0);
            map(display, window);
            sync(display, 0);
            Some(Self {
                display,
                window,
                _x11: x11,
            })
        }
    }
}

/// Helper: `renderer<TAB>presented` for the forced vendor's EGL on `$DISPLAY`, or nothing and
/// exit code 1.
pub(super) fn print_window_test() -> i32 {
    let (Some(api), Some(x)) = (Egl::load(), XWindow::open()) else {
        return 1;
    };
    // SAFETY: the Xlib display is an EGL native display, and the window a native window.
    let display = unsafe { (api.get_display)(x.display) };
    if display.is_null() {
        return 1;
    }
    let Some((context, config)) = api.context(display, EGL_WINDOW_BIT) else {
        return 1;
    };
    // SAFETY: `config` backs windows; no attributes.
    let surface = unsafe { (api.create_window_surface)(display, config, x.window, ptr::null()) };
    let mut seen = if surface.is_null() {
        String::new()
    } else {
        api.renderer(display, context, surface)
    };
    // SAFETY: `surface` is current where `seen` is non-empty.
    let presented = !seen.is_empty()
        && unsafe {
            (api.swap_buffers)(display, surface) != 0 && (api.swap_buffers)(display, surface) != 0
        };
    if seen.is_empty() {
        // The renderer is worth knowing even where no window can be presented.
        seen = api.renderer(display, context, ptr::null_mut());
    }
    if seen.is_empty() {
        return 1;
    }
    println!("{seen}\t{}", if presented { "true" } else { "false" });
    0
}

const VK_STRUCTURE_TYPE_APPLICATION_INFO: i32 = 0;
const VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO: i32 = 1;
const VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO: i32 = 2;
const VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO: i32 = 3;
const VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR: i32 = 1000001000;
const VK_STRUCTURE_TYPE_XCB_SURFACE_CREATE_INFO_KHR: i32 = 1000005000;
const VK_SUCCESS: i32 = 0;
const VK_PHYSICAL_DEVICE_TYPE_CPU: u32 = 4;
const VK_QUEUE_GRAPHICS_BIT: u32 = 0x1;
const VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT: u32 = 0x10;
const VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR: u32 = 0x1;
const VK_PRESENT_MODE_FIFO_KHR: i32 = 2;
/// `VK_API_VERSION_1_1`.
const VK_API_VERSION: u32 = (1 << 22) | (1 << 12);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VkExtent2D {
    width: u32,
    height: u32,
}

#[repr(C)]
struct VkApplicationInfo {
    s_type: i32,
    p_next: *const c_void,
    application_name: *const c_char,
    application_version: u32,
    engine_name: *const c_char,
    engine_version: u32,
    api_version: u32,
}

#[repr(C)]
struct VkInstanceCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    application_info: *const VkApplicationInfo,
    enabled_layer_count: u32,
    enabled_layer_names: *const *const c_char,
    enabled_extension_count: u32,
    enabled_extension_names: *const *const c_char,
}

#[repr(C)]
struct VkXcbSurfaceCreateInfoKhr {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    connection: *mut c_void,
    window: u32,
}

#[repr(C)]
struct VkDeviceQueueCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    queue_family_index: u32,
    queue_count: u32,
    queue_priorities: *const f32,
}

#[repr(C)]
struct VkDeviceCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    queue_create_info_count: u32,
    queue_create_infos: *const VkDeviceQueueCreateInfo,
    enabled_layer_count: u32,
    enabled_layer_names: *const *const c_char,
    enabled_extension_count: u32,
    enabled_extension_names: *const *const c_char,
    enabled_features: *const c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VkQueueFamilyProperties {
    queue_flags: u32,
    queue_count: u32,
    timestamp_valid_bits: u32,
    min_image_transfer_granularity: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VkSurfaceCapabilitiesKhr {
    min_image_count: u32,
    max_image_count: u32,
    current_extent: VkExtent2D,
    min_image_extent: VkExtent2D,
    max_image_extent: VkExtent2D,
    max_image_array_layers: u32,
    supported_transforms: u32,
    current_transform: u32,
    supported_composite_alpha: u32,
    supported_usage_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VkSurfaceFormatKhr {
    format: u32,
    color_space: u32,
}

#[repr(C)]
struct VkSwapchainCreateInfoKhr {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    surface: u64,
    min_image_count: u32,
    image_format: u32,
    image_color_space: u32,
    image_extent: VkExtent2D,
    image_array_layers: u32,
    image_usage: u32,
    image_sharing_mode: i32,
    queue_family_index_count: u32,
    queue_family_indices: *const u32,
    pre_transform: u32,
    composite_alpha: u32,
    present_mode: i32,
    clipped: u32,
    old_swapchain: u64,
}

type VkGetInstanceProcAddr = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
type VkGetDeviceProcAddr = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
type VkCreateInstance =
    unsafe extern "C" fn(*const VkInstanceCreateInfo, *const c_void, *mut *mut c_void) -> i32;
type VkEnumeratePhysicalDevices =
    unsafe extern "C" fn(*mut c_void, *mut u32, *mut *mut c_void) -> i32;
type VkGetPhysicalDeviceProperties = unsafe extern "C" fn(*mut c_void, *mut u8);
type VkGetPhysicalDeviceQueueFamilyProperties =
    unsafe extern "C" fn(*mut c_void, *mut u32, *mut VkQueueFamilyProperties);
type VkCreateDevice = unsafe extern "C" fn(
    *mut c_void,
    *const VkDeviceCreateInfo,
    *const c_void,
    *mut *mut c_void,
) -> i32;
type VkCreateXcbSurfaceKhr = unsafe extern "C" fn(
    *mut c_void,
    *const VkXcbSurfaceCreateInfoKhr,
    *const c_void,
    *mut u64,
) -> i32;
type VkGetPhysicalDeviceSurfaceSupportKhr =
    unsafe extern "C" fn(*mut c_void, u32, u64, *mut u32) -> i32;
type VkGetPhysicalDeviceSurfaceCapabilitiesKhr =
    unsafe extern "C" fn(*mut c_void, u64, *mut VkSurfaceCapabilitiesKhr) -> i32;
type VkGetPhysicalDeviceSurfaceFormatsKhr =
    unsafe extern "C" fn(*mut c_void, u64, *mut u32, *mut VkSurfaceFormatKhr) -> i32;
type VkCreateSwapchainKhr = unsafe extern "C" fn(
    *mut c_void,
    *const VkSwapchainCreateInfoKhr,
    *const c_void,
    *mut u64,
) -> i32;

/// An entry point of `owner` (an instance or a device) by name through `getter`.
///
/// # Safety
/// `F` must be the Vulkan signature of the entry point.
unsafe fn vk_proc<F: Copy>(
    getter: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    owner: *mut c_void,
    name: &str,
) -> Option<F> {
    let name = CString::new(name).ok()?;
    // SAFETY: `owner` is a live handle and `name` a valid C string; null is "not available".
    let address = unsafe { getter(owner, name.as_ptr()) };
    if address.is_null() || std::mem::size_of::<F>() != std::mem::size_of::<*mut c_void>() {
        return None;
    }
    Some(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&address) })
}

/// Helper: whether the GPU's Vulkan driver gives a window on `$DISPLAY` a swapchain,
/// printed as `true` or `false`; nothing and exit code 1 where Vulkan cannot be asked.
pub(super) fn print_vulkan_window(node: &str) -> i32 {
    let (Some(vk), Some(x11_xcb)) = (Lib::open("libvulkan.so.1"), Lib::open("libX11-xcb.so.1"))
    else {
        return 1;
    };
    // SAFETY: the types are the Vulkan loader's exported signatures.
    let (
        get_instance_proc,
        get_device_proc,
        create_instance,
        enumerate,
        properties_of,
        queue_families,
        create_device,
        xcb_connection,
    ) = unsafe {
        match (
            vk.sym::<VkGetInstanceProcAddr>("vkGetInstanceProcAddr"),
            vk.sym::<VkGetDeviceProcAddr>("vkGetDeviceProcAddr"),
            vk.sym::<VkCreateInstance>("vkCreateInstance"),
            vk.sym::<VkEnumeratePhysicalDevices>("vkEnumeratePhysicalDevices"),
            vk.sym::<VkGetPhysicalDeviceProperties>("vkGetPhysicalDeviceProperties"),
            vk.sym::<VkGetPhysicalDeviceQueueFamilyProperties>(
                "vkGetPhysicalDeviceQueueFamilyProperties",
            ),
            vk.sym::<VkCreateDevice>("vkCreateDevice"),
            x11_xcb.sym::<XGetXcbConnection>("XGetXCBConnection"),
        ) {
            (Some(a), Some(b), Some(c), Some(d), Some(e), Some(f), Some(g), Some(h)) => {
                (a, b, c, d, e, f, g, h)
            }
            _ => return 1,
        }
    };
    let app_name = CString::new("selkies gpu-probe").expect("a C string");
    let engine_name = CString::new("selkies").expect("a C string");
    let app = VkApplicationInfo {
        s_type: VK_STRUCTURE_TYPE_APPLICATION_INFO,
        p_next: ptr::null(),
        application_name: app_name.as_ptr(),
        application_version: 1,
        engine_name: engine_name.as_ptr(),
        engine_version: 1,
        api_version: VK_API_VERSION,
    };
    let extension_names: Vec<CString> = ["VK_KHR_surface", "VK_KHR_xcb_surface"]
        .iter()
        .map(|e| CString::new(*e).expect("a C string"))
        .collect();
    let extensions: Vec<*const c_char> = extension_names.iter().map(|e| e.as_ptr()).collect();
    let info = VkInstanceCreateInfo {
        s_type: VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        p_next: ptr::null(),
        flags: 0,
        application_info: &app,
        enabled_layer_count: 0,
        enabled_layer_names: ptr::null(),
        enabled_extension_count: extensions.len() as u32,
        enabled_extension_names: extensions.as_ptr(),
    };
    let mut instance: *mut c_void = ptr::null_mut();
    // SAFETY: `info` and everything it points to outlive the call.
    if unsafe { create_instance(&info, ptr::null(), &mut instance) } != VK_SUCCESS {
        return 1;
    }
    let mut count: u32 = 0;
    // SAFETY: a null array asks for the count; the second call gets an array of that size.
    unsafe { enumerate(instance, &mut count, ptr::null_mut()) };
    let mut devices: Vec<*mut c_void> = vec![ptr::null_mut(); count.max(1) as usize];
    unsafe { enumerate(instance, &mut count, devices.as_mut_ptr()) };
    // The node's own GPU by its PCI ids, else the first device that is not a CPU.
    let wanted = super::gpu_probe::vk_device_select(node, Path::new("/sys/class/drm"));
    let mut chosen: *mut c_void = ptr::null_mut();
    for device in devices.iter().take(count as usize) {
        let mut properties = [0u8; 1024];
        // SAFETY: VkPhysicalDeviceProperties is 824 bytes; the buffer is larger.
        unsafe { properties_of(*device, properties.as_mut_ptr()) };
        let field = |at: usize| {
            u32::from_le_bytes([
                properties[at],
                properties[at + 1],
                properties[at + 2],
                properties[at + 3],
            ])
        };
        let (vendor, product, kind) = (field(8), field(12), field(16));
        if !wanted.is_empty() {
            if format!("{vendor:04x}:{product:04x}") == wanted {
                chosen = *device;
                break;
            }
        } else if kind != VK_PHYSICAL_DEVICE_TYPE_CPU {
            chosen = *device;
            break;
        }
    }
    if chosen.is_null() {
        return 1;
    }
    let Some(x) = XWindow::open() else {
        return 1;
    };
    // SAFETY: the WSI entry points' signatures.
    let (create_surface, surface_support, capabilities_of, formats_of) = unsafe {
        match (
            vk_proc::<VkCreateXcbSurfaceKhr>(get_instance_proc, instance, "vkCreateXcbSurfaceKHR"),
            vk_proc::<VkGetPhysicalDeviceSurfaceSupportKhr>(
                get_instance_proc,
                instance,
                "vkGetPhysicalDeviceSurfaceSupportKHR",
            ),
            vk_proc::<VkGetPhysicalDeviceSurfaceCapabilitiesKhr>(
                get_instance_proc,
                instance,
                "vkGetPhysicalDeviceSurfaceCapabilitiesKHR",
            ),
            vk_proc::<VkGetPhysicalDeviceSurfaceFormatsKhr>(
                get_instance_proc,
                instance,
                "vkGetPhysicalDeviceSurfaceFormatsKHR",
            ),
        ) {
            (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
            _ => return 1,
        }
    };
    let answer = |presents: bool| -> i32 {
        println!("{}", if presents { "true" } else { "false" });
        0
    };
    let surface_info = VkXcbSurfaceCreateInfoKhr {
        s_type: VK_STRUCTURE_TYPE_XCB_SURFACE_CREATE_INFO_KHR,
        p_next: ptr::null(),
        flags: 0,
        // SAFETY: the Xlib display is open.
        connection: unsafe { xcb_connection(x.display) },
        window: (x.window & 0xFFFF_FFFF) as u32,
    };
    let mut surface: u64 = 0;
    // SAFETY: the create info outlives the call.
    if unsafe { create_surface(instance, &surface_info, ptr::null(), &mut surface) } != VK_SUCCESS {
        return answer(false);
    }
    let mut families: u32 = 0;
    // SAFETY: count, then an array of that size.
    unsafe { queue_families(chosen, &mut families, ptr::null_mut()) };
    let mut properties = vec![VkQueueFamilyProperties::default(); families.max(1) as usize];
    unsafe { queue_families(chosen, &mut families, properties.as_mut_ptr()) };
    let mut family: Option<u32> = None;
    for index in 0..families {
        let mut supported: u32 = 0;
        // SAFETY: `surface` is the surface just created.
        unsafe { surface_support(chosen, index, surface, &mut supported) };
        if properties[index as usize].queue_flags & VK_QUEUE_GRAPHICS_BIT != 0 && supported != 0 {
            family = Some(index);
            break;
        }
    }
    let Some(family) = family else {
        return answer(false);
    };
    let priority = [1.0f32];
    let queue = VkDeviceQueueCreateInfo {
        s_type: VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        p_next: ptr::null(),
        flags: 0,
        queue_family_index: family,
        queue_count: 1,
        queue_priorities: priority.as_ptr(),
    };
    let swapchain_ext = CString::new("VK_KHR_swapchain").expect("a C string");
    let device_extensions = [swapchain_ext.as_ptr()];
    let device_info = VkDeviceCreateInfo {
        s_type: VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        p_next: ptr::null(),
        flags: 0,
        queue_create_info_count: 1,
        queue_create_infos: &queue,
        enabled_layer_count: 0,
        enabled_layer_names: ptr::null(),
        enabled_extension_count: 1,
        enabled_extension_names: device_extensions.as_ptr(),
        enabled_features: ptr::null(),
    };
    let mut device: *mut c_void = ptr::null_mut();
    // SAFETY: the create info outlives the call.
    if unsafe { create_device(chosen, &device_info, ptr::null(), &mut device) } != VK_SUCCESS {
        return answer(false);
    }
    let mut capabilities = VkSurfaceCapabilitiesKhr::default();
    let mut format_count: u32 = 0;
    // SAFETY: the surface and device are live; count, then an array of that size.
    unsafe {
        if capabilities_of(chosen, surface, &mut capabilities) != VK_SUCCESS
            || formats_of(chosen, surface, &mut format_count, ptr::null_mut()) != VK_SUCCESS
            || format_count == 0
        {
            return answer(false);
        }
    }
    let mut formats = vec![VkSurfaceFormatKhr::default(); format_count as usize];
    unsafe { formats_of(chosen, surface, &mut format_count, formats.as_mut_ptr()) };
    // SAFETY: the device-level signature of vkCreateSwapchainKHR.
    let Some(create_swapchain) = (unsafe {
        vk_proc::<VkCreateSwapchainKhr>(get_device_proc, device, "vkCreateSwapchainKHR")
    }) else {
        return answer(false);
    };
    let mut extent = capabilities.current_extent;
    if extent.width == 0xFFFF_FFFF {
        extent = VkExtent2D {
            width: 64,
            height: 64,
        };
    }
    let swapchain_info = VkSwapchainCreateInfoKhr {
        s_type: VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR,
        p_next: ptr::null(),
        flags: 0,
        surface,
        min_image_count: capabilities.min_image_count.max(2),
        image_format: formats[0].format,
        image_color_space: formats[0].color_space,
        image_extent: extent,
        image_array_layers: 1,
        image_usage: VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
        image_sharing_mode: 0,
        queue_family_index_count: 0,
        queue_family_indices: ptr::null(),
        pre_transform: capabilities.current_transform,
        composite_alpha: VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
        present_mode: VK_PRESENT_MODE_FIFO_KHR,
        clipped: 1,
        old_swapchain: 0,
    };
    let mut swapchain: u64 = 0;
    // SAFETY: the create info outlives the call.
    let created = unsafe { create_swapchain(device, &swapchain_info, ptr::null(), &mut swapchain) }
        == VK_SUCCESS;
    answer(created)
}

/// The environment a helper runs with: this process's, less [`CLIENT_ENV`], plus the forced
/// vendor and `extra`.
fn client_env(vendor_file: &str, extra: Option<&HashMap<String, String>>) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !CLIENT_ENV.contains(&k.as_str()))
        .collect();
    env.push((
        "__EGL_VENDOR_LIBRARY_FILENAMES".into(),
        vendor_file.to_string(),
    ));
    if let Some(extra) = extra {
        env.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    env
}

/// Run this binary's `gpu-probe` helper `args` in a process of its own with `env` as its
/// whole environment; its stdout, or "" where it failed or outlived `PROBE_TIMEOUT`.
fn helper(args: &[&str], env: &[(String, String)]) -> String {
    let Ok(exe) = std::env::current_exe() else {
        return String::new();
    };
    let mut command = Command::new(exe);
    command
        .arg("gpu-probe")
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)));
    match run_with_timeout(&mut command, PROBE_TIMEOUT) {
        Some((0, out)) => out.trim().to_string(),
        _ => String::new(),
    }
}

/// An Xvfb of the probe's own, ended when dropped.
struct XvfbDisplay {
    name: String,
    server: Option<Child>,
}

impl ThrowawayDisplay for XvfbDisplay {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for XvfbDisplay {
    fn drop(&mut self) {
        let Some(mut server) = self.server.take() else {
            return;
        };
        // SAFETY: the pid is this probe's own child, still owned by `server`.
        unsafe { libc::kill(server.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match server.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                _ => {
                    let _ = server.kill();
                    let _ = server.wait();
                    return;
                }
            }
        }
    }
}

/// An X server of this process's own for the window probes; a display named "" without one.
/// `node` is the render node to draw on through glamor and DRI3, the way the session's
/// framebuffer server does; "" for a software server.
fn throwaway_display(node: &str) -> Box<dyn ThrowawayDisplay> {
    let none = || -> Box<dyn ThrowawayDisplay> {
        Box::new(XvfbDisplay {
            name: String::new(),
            server: None,
        })
    };
    let xvfb = match super::resources::which_on_path("Xvfb") {
        Some(path) => path,
        None => return none(),
    };
    // The socket directory is the server's to make only as root.
    let _ = std::fs::create_dir_all("/tmp/.X11-unix");
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !CLIENT_ENV.contains(&k.as_str()))
        .collect();
    let mut command = Command::new(xvfb);
    command.args(["-screen", "0", "64x64x24", "-nolisten", "tcp"]);
    if !node.is_empty() {
        command.args(["-glamor", "-dri", node]);
    } else {
        // A software server probes GLX on Mesa's EGL: a vendor library's GBM has nothing to
        // offer it and crashes it, so the server is pinned to Mesa's ICD.
        let vendors = egl_vendors(&EGL_VENDOR_DIRS.map(std::path::PathBuf::from), &|k| {
            std::env::var(k).ok()
        });
        if let Some((_, mesa)) = vendors.iter().find(|(n, _)| n == MESA_VENDOR) {
            env.push(("__EGL_VENDOR_LIBRARY_FILENAMES".into(), mesa.clone()));
        }
    }
    let mut fds = [0 as c_int; 2];
    // SAFETY: `fds` holds two descriptors for pipe to fill.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return none();
    }
    let (reader_fd, writer_fd) = (fds[0], fds[1]);
    // SAFETY: the descriptors are this process's own, freshly created, and owned from here by
    // the File values; the writer is handed to the child by number, so it must stay open
    // across the spawn and is closed right after.
    let (mut reader, writer) = unsafe {
        (
            std::fs::File::from_raw_fd(reader_fd),
            std::fs::File::from_raw_fd(writer_fd),
        )
    };
    let server = command
        .arg("-displayfd")
        .arg(writer_fd.to_string())
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    drop(writer);
    let Ok(server) = server else {
        return none();
    };
    let number = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = reader.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + PROBE_TIMEOUT;
    while !number.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let text = if number.is_finished() {
        number.join().unwrap_or_default()
    } else {
        String::new()
    };
    let digits = text.trim();
    let name = if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        format!(":{digits}")
    } else {
        String::new()
    };
    Box::new(XvfbDisplay {
        name,
        server: Some(server),
    })
}

/// The client probes as the command runs them: children of this binary and an Xvfb.
pub struct RealProbes;

impl ClientProbes for RealProbes {
    fn renderer(&self, node: &str, icd: &str, extra: Option<&HashMap<String, String>>) -> String {
        helper(&["--renderer", node], &client_env(icd, extra))
    }

    fn window(
        &self,
        display: &str,
        icd: &str,
        extra: Option<&HashMap<String, String>>,
    ) -> (String, bool) {
        let mut env = client_env(icd, extra);
        env.push(("DISPLAY".into(), display.to_string()));
        let out = helper(&["--window"], &env);
        let (renderer, presented) = out.split_once('\t').unwrap_or((out.as_str(), ""));
        (renderer.to_string(), presented == "true")
    }

    fn display(&self, node: &str) -> Box<dyn ThrowawayDisplay> {
        throwaway_display(node)
    }

    fn vulkan(&self, display: &str, node: &str) -> Option<bool> {
        let mut env: Vec<(String, String)> = std::env::vars()
            .filter(|(k, _)| !CLIENT_ENV.contains(&k.as_str()))
            .collect();
        env.push(("DISPLAY".into(), display.to_string()));
        match helper(&["--vulkan-window", node], &env).as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }
}
