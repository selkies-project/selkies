/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A GPU semaphore an X server's renderer signals and the encoder's CUDA stream waits on, so the
//! encoder's work on a frame is queued behind the server's blit on the GPU itself instead of after
//! the CPU has learnt the blit landed ([`crate::x11`]'s DRI3 path, through the SELKIES-SEMAPHORE
//! extension of the images' Xvfb).
//!
//! Neither CUDA nor GL can make a semaphore another API imports, so Vulkan makes it, a binary one
//! exported as an OPAQUE_FD, on the device whose UUID is the CUDA device's. Two fds of it go out,
//! one for CUDA's import and one for the server's GL, and every Vulkan object is gone again once
//! they are: the imports hold the payload between them.

use std::ffi::{c_char, c_void};
use std::os::fd::{FromRawFd, OwnedFd};
use std::ptr;

use libloading::Library;

type VkResult = i32;
type VkInstance = *mut c_void;
type VkPhysicalDevice = *mut c_void;
type VkDevice = *mut c_void;
type VkSemaphore = u64;
type VoidFn = Option<unsafe extern "C" fn()>;

const VK_API_VERSION_1_1: u32 = (1 << 22) | (1 << 12);
const STRUCTURE_TYPE_APPLICATION_INFO: i32 = 0;
const STRUCTURE_TYPE_INSTANCE_CREATE_INFO: i32 = 1;
const STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO: i32 = 2;
const STRUCTURE_TYPE_DEVICE_CREATE_INFO: i32 = 3;
const STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO: i32 = 9;
const STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2: i32 = 1000059001;
const STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES: i32 = 1000071004;
const STRUCTURE_TYPE_EXPORT_SEMAPHORE_CREATE_INFO: i32 = 1000077000;
const STRUCTURE_TYPE_SEMAPHORE_GET_FD_INFO_KHR: i32 = 1000079001;
const EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD: u32 = 0x1;

#[repr(C)]
struct ApplicationInfo {
    s_type: i32,
    p_next: *const c_void,
    application_name: *const c_char,
    application_version: u32,
    engine_name: *const c_char,
    engine_version: u32,
    api_version: u32,
}

#[repr(C)]
struct InstanceCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    application_info: *const ApplicationInfo,
    enabled_layer_count: u32,
    enabled_layer_names: *const *const c_char,
    enabled_extension_count: u32,
    enabled_extension_names: *const *const c_char,
}

#[repr(C)]
struct DeviceQueueCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    queue_family_index: u32,
    queue_count: u32,
    queue_priorities: *const f32,
}

#[repr(C)]
struct DeviceCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    queue_create_info_count: u32,
    queue_create_infos: *const DeviceQueueCreateInfo,
    enabled_layer_count: u32,
    enabled_layer_names: *const *const c_char,
    enabled_extension_count: u32,
    enabled_extension_names: *const *const c_char,
    enabled_features: *const c_void,
}

#[repr(C)]
struct PhysicalDeviceIdProperties {
    s_type: i32,
    p_next: *mut c_void,
    device_uuid: [u8; 16],
    driver_uuid: [u8; 16],
    device_luid: [u8; 8],
    device_node_mask: u32,
    device_luid_valid: u32,
}

/// `VkPhysicalDeviceProperties2`, with the core properties (824 bytes, 8-aligned) left opaque:
/// only the ID properties chained after them are read.
#[repr(C)]
struct PhysicalDeviceProperties2 {
    s_type: i32,
    p_next: *mut c_void,
    properties: [u64; 103],
}

#[repr(C)]
struct ExportSemaphoreCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    handle_types: u32,
}

#[repr(C)]
struct SemaphoreCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
}

#[repr(C)]
struct SemaphoreGetFdInfo {
    s_type: i32,
    p_next: *const c_void,
    semaphore: VkSemaphore,
    handle_type: u32,
}

type GetInstanceProcAddr = unsafe extern "C" fn(VkInstance, *const c_char) -> VoidFn;
type CreateInstance =
    unsafe extern "C" fn(*const InstanceCreateInfo, *const c_void, *mut VkInstance) -> VkResult;
type DestroyInstance = unsafe extern "C" fn(VkInstance, *const c_void);
type EnumeratePhysicalDevices =
    unsafe extern "C" fn(VkInstance, *mut u32, *mut VkPhysicalDevice) -> VkResult;
type GetPhysicalDeviceProperties2 =
    unsafe extern "C" fn(VkPhysicalDevice, *mut PhysicalDeviceProperties2);
type CreateDevice = unsafe extern "C" fn(
    VkPhysicalDevice,
    *const DeviceCreateInfo,
    *const c_void,
    *mut VkDevice,
) -> VkResult;
type GetDeviceProcAddr = unsafe extern "C" fn(VkDevice, *const c_char) -> VoidFn;
type DestroyDevice = unsafe extern "C" fn(VkDevice, *const c_void);
type CreateSemaphore = unsafe extern "C" fn(
    VkDevice,
    *const SemaphoreCreateInfo,
    *const c_void,
    *mut VkSemaphore,
) -> VkResult;
type DestroySemaphore = unsafe extern "C" fn(VkDevice, VkSemaphore, *const c_void);
type GetSemaphoreFd =
    unsafe extern "C" fn(VkDevice, *const SemaphoreGetFdInfo, *mut i32) -> VkResult;

/// Two fds of one new binary semaphore exportable as OPAQUE_FD, made on the Vulkan device whose
/// UUID is `uuid` (the CUDA device's, as `cuDeviceGetUuid` reports it).
pub(crate) fn opaque_fd_pair(uuid: [u8; 16]) -> Result<(OwnedFd, OwnedFd), String> {
    unsafe {
        let lib = Library::new("libvulkan.so.1")
            .map_err(|e| format!("no Vulkan loader (libvulkan.so.1): {e}"))?;
        let get_instance_proc: GetInstanceProcAddr = *lib
            .get(b"vkGetInstanceProcAddr\0")
            .map_err(|e| format!("no vkGetInstanceProcAddr: {e}"))?;
        macro_rules! instance_fn {
            ($instance:expr, $ty:ty, $name:literal) => {
                match get_instance_proc($instance, $name.as_ptr()) {
                    Some(f) => std::mem::transmute::<unsafe extern "C" fn(), $ty>(f),
                    None => return Err(format!("no {}", $name.to_string_lossy())),
                }
            };
        }
        let create_instance = instance_fn!(ptr::null_mut(), CreateInstance, c"vkCreateInstance");
        let app = ApplicationInfo {
            s_type: STRUCTURE_TYPE_APPLICATION_INFO,
            p_next: ptr::null(),
            application_name: c"pixelflux".as_ptr(),
            application_version: 1,
            engine_name: ptr::null(),
            engine_version: 0,
            api_version: VK_API_VERSION_1_1,
        };
        let instance_info = InstanceCreateInfo {
            s_type: STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
            p_next: ptr::null(),
            flags: 0,
            application_info: &app,
            enabled_layer_count: 0,
            enabled_layer_names: ptr::null(),
            enabled_extension_count: 0,
            enabled_extension_names: ptr::null(),
        };
        let mut instance: VkInstance = ptr::null_mut();
        let r = create_instance(&instance_info, ptr::null(), &mut instance);
        if r != 0 {
            return Err(format!("vkCreateInstance: {r}"));
        }
        let destroy_instance = instance_fn!(instance, DestroyInstance, c"vkDestroyInstance");
        let result = (|| {
            let enumerate = instance_fn!(
                instance,
                EnumeratePhysicalDevices,
                c"vkEnumeratePhysicalDevices"
            );
            let properties = instance_fn!(
                instance,
                GetPhysicalDeviceProperties2,
                c"vkGetPhysicalDeviceProperties2"
            );
            let create_device = instance_fn!(instance, CreateDevice, c"vkCreateDevice");
            let get_device_proc = instance_fn!(instance, GetDeviceProcAddr, c"vkGetDeviceProcAddr");

            let mut count = 0u32;
            enumerate(instance, &mut count, ptr::null_mut());
            let mut devices: Vec<VkPhysicalDevice> = Vec::with_capacity(count as usize);
            enumerate(instance, &mut count, devices.as_mut_ptr());
            devices.set_len(count as usize);
            let physical = devices
                .into_iter()
                .find(|&d| {
                    let mut id = PhysicalDeviceIdProperties {
                        s_type: STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES,
                        p_next: ptr::null_mut(),
                        device_uuid: [0; 16],
                        driver_uuid: [0; 16],
                        device_luid: [0; 8],
                        device_node_mask: 0,
                        device_luid_valid: 0,
                    };
                    let mut props = PhysicalDeviceProperties2 {
                        s_type: STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
                        p_next: (&mut id as *mut PhysicalDeviceIdProperties).cast(),
                        properties: [0; 103],
                    };
                    properties(d, &mut props);
                    id.device_uuid == uuid
                })
                .ok_or("no Vulkan device has the CUDA device's UUID")?;

            let priority = 1.0f32;
            let queue = DeviceQueueCreateInfo {
                s_type: STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                p_next: ptr::null(),
                flags: 0,
                queue_family_index: 0,
                queue_count: 1,
                queue_priorities: &priority,
            };
            let extensions = [c"VK_KHR_external_semaphore_fd".as_ptr()];
            let device_info = DeviceCreateInfo {
                s_type: STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                p_next: ptr::null(),
                flags: 0,
                queue_create_info_count: 1,
                queue_create_infos: &queue,
                enabled_layer_count: 0,
                enabled_layer_names: ptr::null(),
                enabled_extension_count: 1,
                enabled_extension_names: extensions.as_ptr(),
                enabled_features: ptr::null(),
            };
            let mut device: VkDevice = ptr::null_mut();
            let r = create_device(physical, &device_info, ptr::null(), &mut device);
            if r != 0 {
                return Err(format!("vkCreateDevice: {r}"));
            }
            macro_rules! device_fn {
                ($ty:ty, $name:literal) => {
                    match get_device_proc(device, $name.as_ptr()) {
                        Some(f) => std::mem::transmute::<unsafe extern "C" fn(), $ty>(f),
                        None => return Err(format!("no {}", $name.to_string_lossy())),
                    }
                };
            }
            let destroy_device: DestroyDevice = device_fn!(DestroyDevice, c"vkDestroyDevice");
            let fds = (|| {
                let create_semaphore = device_fn!(CreateSemaphore, c"vkCreateSemaphore");
                let destroy_semaphore = device_fn!(DestroySemaphore, c"vkDestroySemaphore");
                let get_fd = device_fn!(GetSemaphoreFd, c"vkGetSemaphoreFdKHR");
                let export = ExportSemaphoreCreateInfo {
                    s_type: STRUCTURE_TYPE_EXPORT_SEMAPHORE_CREATE_INFO,
                    p_next: ptr::null(),
                    handle_types: EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD,
                };
                let info = SemaphoreCreateInfo {
                    s_type: STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
                    p_next: (&export as *const ExportSemaphoreCreateInfo).cast(),
                    flags: 0,
                };
                let mut semaphore: VkSemaphore = 0;
                let r = create_semaphore(device, &info, ptr::null(), &mut semaphore);
                if r != 0 {
                    return Err(format!("vkCreateSemaphore: {r}"));
                }
                let fd_info = SemaphoreGetFdInfo {
                    s_type: STRUCTURE_TYPE_SEMAPHORE_GET_FD_INFO_KHR,
                    p_next: ptr::null(),
                    semaphore,
                    handle_type: EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD,
                };
                let export_one = || -> Result<OwnedFd, String> {
                    let mut fd = -1;
                    let r = get_fd(device, &fd_info, &mut fd);
                    if r != 0 || fd < 0 {
                        return Err(format!("vkGetSemaphoreFdKHR: {r}"));
                    }
                    Ok(OwnedFd::from_raw_fd(fd))
                };
                let pair = export_one().and_then(|a| Ok((a, export_one()?)));
                destroy_semaphore(device, semaphore, ptr::null());
                pair
            })();
            destroy_device(device, ptr::null());
            fds
        })();
        destroy_instance(instance, ptr::null());
        result
    }
}
