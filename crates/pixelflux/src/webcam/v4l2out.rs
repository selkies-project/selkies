//! Kernel-device sink: writes frames into a v4l2loopback output device.
//!
//! Where the host (or a privileged container) carries the v4l2loopback module, a real `/dev/videoN`
//! is the zero-configuration path for consumers — no preload, no socket — exactly as `/dev/uinput`
//! is for the gamepads. The device is opened for output, set to the ring's raw format, and fed one
//! `write()` per published frame. It is a best-effort mirror of the ring: an error disables the
//! sink and is reported once, never failing the camera.

use std::ffi::CStr;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use super::ring::{RingFormat, V4L2_PIX_FMT_MJPEG};

const V4L2_BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
const V4L2_CAP_VIDEO_OUTPUT: u32 = 0x0000_0002;
const V4L2_CAP_READWRITE: u32 = 0x0100_0000;
const V4L2_CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const V4L2_FIELD_NONE: u32 = 1;
const V4L2_COLORSPACE_SMPTE170M: u32 = 1;
const V4L2_COLORSPACE_SRGB: u32 = 8;

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const fn ioc(dir: u32, typ: u8, nr: u8, size: usize) -> libc::c_ulong {
    ((dir << 30) | ((size as u32) << 16) | ((typ as u32) << 8) | nr as u32) as libc::c_ulong
}

#[repr(C)]
struct V4l2Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    private: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

/// `struct v4l2_format`: the type word, padding to the 8-byte aligned union, then the union's
/// 200 bytes, of which the pixel format occupies the first 48.
#[repr(C)]
struct V4l2Format {
    type_: u32,
    _pad: u32,
    pix: V4l2PixFormat,
    _rest: [u8; 200 - mem::size_of::<V4l2PixFormat>()],
}

const VIDIOC_QUERYCAP: libc::c_ulong = ioc(IOC_READ, b'V', 0, mem::size_of::<V4l2Capability>());
const VIDIOC_S_FMT: libc::c_ulong =
    ioc(IOC_READ | IOC_WRITE, b'V', 5, mem::size_of::<V4l2Format>());

fn query_cap(fd: libc::c_int) -> io::Result<V4l2Capability> {
    let mut cap: V4l2Capability = unsafe { mem::zeroed() };
    if unsafe { libc::ioctl(fd, VIDIOC_QUERYCAP as _, &mut cap as *mut V4l2Capability) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cap)
}

fn cstr_field(b: &[u8]) -> String {
    let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn is_loopback_output(cap: &V4l2Capability) -> bool {
    let caps = if cap.capabilities & V4L2_CAP_DEVICE_CAPS != 0 {
        cap.device_caps
    } else {
        cap.capabilities
    };
    caps & V4L2_CAP_VIDEO_OUTPUT != 0 && caps & V4L2_CAP_READWRITE != 0
}

pub struct V4l2Output {
    fd: OwnedFd,
    path: String,
    frame_bytes: usize,
    failed: bool,
}

impl V4l2Output {
    /// Open `path` for output and set the ring's format on it.
    pub fn open(path: &str, fmt: &RingFormat) -> Result<Self, String> {
        let c_path =
            std::ffi::CString::new(path).map_err(|_| "device path contains NUL".to_string())?;
        let raw = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(format!("open({}): {}", path, io::Error::last_os_error()));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let cap =
            query_cap(fd.as_raw_fd()).map_err(|e| format!("VIDIOC_QUERYCAP({}): {}", path, e))?;
        if !is_loopback_output(&cap) {
            return Err(format!(
                "{} ({}) is not a writable video output device",
                path,
                cstr_field(&cap.driver)
            ));
        }
        let mut f: V4l2Format = unsafe { mem::zeroed() };
        f.type_ = V4L2_BUF_TYPE_VIDEO_OUTPUT;
        f.pix = V4l2PixFormat {
            width: fmt.width,
            height: fmt.height,
            pixelformat: fmt.fourcc,
            field: V4L2_FIELD_NONE,
            bytesperline: fmt.bytesperline,
            sizeimage: fmt.sizeimage,
            colorspace: if fmt.fourcc == V4L2_PIX_FMT_MJPEG {
                V4L2_COLORSPACE_SRGB
            } else {
                V4L2_COLORSPACE_SMPTE170M
            },
            private: 0,
            flags: 0,
            ycbcr_enc: 0,
            quantization: 0,
            xfer_func: 0,
        };
        if unsafe { libc::ioctl(fd.as_raw_fd(), VIDIOC_S_FMT as _, &mut f as *mut V4l2Format) } != 0
        {
            return Err(format!(
                "VIDIOC_S_FMT({}): {}",
                path,
                io::Error::last_os_error()
            ));
        }
        if f.pix.pixelformat != fmt.fourcc || f.pix.width != fmt.width || f.pix.height != fmt.height
        {
            return Err(format!(
                "{} refused {}x{} {}",
                path,
                fmt.width,
                fmt.height,
                fourcc_str(fmt.fourcc)
            ));
        }
        let frame_bytes = if f.pix.sizeimage != 0 {
            f.pix.sizeimage as usize
        } else {
            fmt.sizeimage as usize
        };
        Ok(V4l2Output {
            fd,
            path: path.to_string(),
            frame_bytes,
            failed: false,
        })
    }

    /// First v4l2loopback output device among `/dev/video0..63`, if any.
    pub fn find_loopback_device() -> Option<String> {
        for n in 0..64 {
            let path = format!("/dev/video{}", n);
            let c_path = std::ffi::CString::new(path.clone()).ok()?;
            let raw = unsafe {
                libc::open(
                    c_path.as_ptr(),
                    libc::O_RDWR | libc::O_CLOEXEC | libc::O_NONBLOCK,
                )
            };
            if raw < 0 {
                continue;
            }
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            if let Ok(cap) = query_cap(fd.as_raw_fd())
                && cstr_field(&cap.driver).starts_with("v4l2 loopback")
                && is_loopback_output(&cap)
            {
                return Some(path);
            }
        }
        None
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn is_failed(&self) -> bool {
        self.failed
    }

    /// Write one device frame; a short or failed write disables the sink.
    pub fn write_frame(&mut self, frame: &[u8]) {
        if self.failed {
            return;
        }
        let len = frame.len().min(self.frame_bytes.max(frame.len()));
        let n = unsafe { libc::write(self.fd.as_raw_fd(), frame.as_ptr() as *const _, len) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if matches!(err.raw_os_error(), Some(libc::EAGAIN) | Some(libc::EINTR)) {
                return;
            }
            eprintln!(
                "[webcam] {}: write failed ({}); kernel device sink disabled",
                self.path, err
            );
            self.failed = true;
        } else if (n as usize) < len {
            eprintln!(
                "[webcam] {}: short write ({} of {}); kernel device sink disabled",
                self.path, n, len
            );
            self.failed = true;
        }
    }
}

pub fn fourcc_str(f: u32) -> String {
    let b = f.to_le_bytes();
    CStr::from_bytes_until_nul(&[b[0], b[1], b[2], b[3], 0])
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_kernel_headers() {
        assert_eq!(mem::size_of::<V4l2Capability>(), 104);
        assert_eq!(mem::size_of::<V4l2Format>(), 208);
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_S_FMT, 0xC0D0_5605);
    }

    #[test]
    fn missing_device_is_an_error_not_a_panic() {
        let fmt = RingFormat::raw(super::super::ring::V4L2_PIX_FMT_YUV420, 64, 48, 30, 1).unwrap();
        assert!(V4l2Output::open("/dev/video-does-not-exist", &fmt).is_err());
        assert!(V4l2Output::open("/dev/null", &fmt).is_err());
    }
}
