//! Minimal virtual mouse via `/dev/uinput`, used for touchpad-as-mouse
//! remap mode. Deliberately a SEPARATE device from VirtualDs4 -- a
//! gamepad reporting EV_REL mouse deltas is not how Linux input semantics
//! or games/the desktop interpret input; DS4Windows makes the same
//! separation (a "virtual mouse" distinct from the virtual XInput/DS4
//! pad) for the same reason.
//!
//! Reuses the same raw-ioctl approach as uinput_ds4.rs since the
//! high-level `uinput` crate has the same VID/PID limitation here too
//! (less critical for a mouse, but consistency keeps one code path to
//! maintain instead of two).

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::mem;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const SYN_REPORT: u16 = 0;

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;

const BUS_USB: u16 = 0x03;
const UINPUT_MAX_NAME_SIZE: usize = 80;

#[repr(C)]
#[derive(Clone, Copy)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [i8; UINPUT_MAX_NAME_SIZE],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputEvent {
    tv_sec: i64,
    tv_usec: i64,
    kind: u16,
    code: u16,
    value: i32,
}

const UI_SET_EVBIT: u64 = ioctl_w_int(b'U', 100);
const UI_SET_KEYBIT: u64 = ioctl_w_int(b'U', 101);
const UI_SET_RELBIT: u64 = ioctl_w_int(b'U', 102);
const UI_DEV_SETUP: u64 = ioctl_w::<UinputSetup>(b'U', 3);
const UI_DEV_CREATE: u64 = ioctl_none(b'U', 1);
const UI_DEV_DESTROY: u64 = ioctl_none(b'U', 2);

const fn ioctl_none(kind: u8, nr: u8) -> u64 {
    ((kind as u64) << 8) | (nr as u64)
}
const fn ioctl_w_int(kind: u8, nr: u8) -> u64 {
    (1u64 << 30) | (4u64 << 16) | ((kind as u64) << 8) | (nr as u64)
}
const fn ioctl_w<T>(kind: u8, nr: u8) -> u64 {
    (1u64 << 30) | ((mem::size_of::<T>() as u64) << 16) | ((kind as u64) << 8) | (nr as u64)
}

extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

pub struct VirtualMouse {
    file: File,
}

impl VirtualMouse {
    pub fn create() -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/uinput")?;
        let fd = file.as_raw_fd();

        unsafe {
            check(ioctl(fd, UI_SET_EVBIT, EV_KEY as i32))?;
            check(ioctl(fd, UI_SET_KEYBIT, BTN_LEFT as i32))?;
            check(ioctl(fd, UI_SET_KEYBIT, BTN_RIGHT as i32))?;

            check(ioctl(fd, UI_SET_EVBIT, EV_REL as i32))?;
            check(ioctl(fd, UI_SET_RELBIT, REL_X as i32))?;
            check(ioctl(fd, UI_SET_RELBIT, REL_Y as i32))?;

            let mut name = [0i8; UINPUT_MAX_NAME_SIZE];
            let cname = CString::new("ds4l Virtual Touchpad Mouse").unwrap();
            for (i, &b) in cname.as_bytes_with_nul().iter().take(UINPUT_MAX_NAME_SIZE).enumerate() {
                name[i] = b as i8;
            }

            let setup = UinputSetup {
                id: InputId {
                    bustype: BUS_USB,
                    // Arbitrary/generic VID/PID -- unlike the gamepad,
                    // there's no protocol requirement to spoof a specific
                    // real mouse; the desktop just needs a normal-looking
                    // relative pointer device.
                    vendor: 0x1234,
                    product: 0x0001,
                    version: 0x0100,
                },
                name,
                ff_effects_max: 0,
            };
            check(ioctl(fd, UI_DEV_SETUP, &setup))?;
            check(ioctl(fd, UI_DEV_CREATE))?;
        }

        std::thread::sleep(std::time::Duration::from_millis(200));

        Ok(VirtualMouse { file })
    }

    pub fn emit_rel(&mut self, code: u16, value: i32) -> io::Result<()> {
        self.write_event(EV_REL, code, value)
    }

    pub fn emit_key(&mut self, code: u16, pressed: bool) -> io::Result<()> {
        self.write_event(EV_KEY, code, pressed as i32)
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.write_event(EV_SYN, SYN_REPORT, 0)
    }

    fn write_event(&mut self, kind: u16, code: u16, value: i32) -> io::Result<()> {
        let ev = InputEvent {
            tv_sec: 0,
            tv_usec: 0,
            kind,
            code,
            value,
        };
        let ptr = &ev as *const InputEvent as *const u8;
        let bytes = unsafe { std::slice::from_raw_parts(ptr, mem::size_of::<InputEvent>()) };
        self.file.write_all(bytes)
    }
}

impl Drop for VirtualMouse {
    fn drop(&mut self) {
        unsafe {
            let _ = ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY);
        }
    }
}

fn check(ret: i32) -> io::Result<()> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
