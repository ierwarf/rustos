use core::ffi::{c_char, c_void};

use driver_abi::{SERIO_ANY, SerioPortInfo};

pub(crate) type LinuxCompatSerioWriteWakeupFn = unsafe extern "C" fn(serio: *mut LinuxCompatSerio);
pub(crate) type LinuxCompatSerioWriteFn =
    unsafe extern "C" fn(serio: *mut LinuxCompatSerio, byte: u8) -> i32;
pub(crate) type LinuxCompatSerioOpenFn = unsafe extern "C" fn(serio: *mut LinuxCompatSerio) -> i32;
pub(crate) type LinuxCompatSerioCloseFn = unsafe extern "C" fn(serio: *mut LinuxCompatSerio);
pub(crate) type LinuxCompatSerioInterruptFn =
    unsafe extern "C" fn(serio: *mut LinuxCompatSerio, byte: u8, flags: u32) -> i32;
pub(crate) type LinuxCompatSerioConnectFn =
    unsafe extern "C" fn(serio: *mut LinuxCompatSerio, drv: *mut LinuxCompatSerioDriver) -> i32;
pub(crate) type LinuxCompatSerioReconnectFn =
    unsafe extern "C" fn(serio: *mut LinuxCompatSerio) -> i32;
pub(crate) type LinuxCompatSerioDisconnectFn = unsafe extern "C" fn(serio: *mut LinuxCompatSerio);
pub(crate) type LinuxCompatSerioCleanupFn = unsafe extern "C" fn(serio: *mut LinuxCompatSerio);

pub(crate) type LinuxCompatPs2PreReceiveHandler =
    unsafe extern "C" fn(ps2dev: *mut LinuxCompatPs2Dev, byte: u8, flags: u32) -> u32;
pub(crate) type LinuxCompatPs2ReceiveHandler =
    unsafe extern "C" fn(ps2dev: *mut LinuxCompatPs2Dev, byte: u8);

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct LinuxCompatSerioDeviceId {
    pub(crate) type_: u8,
    pub(crate) extra: u8,
    pub(crate) id: u8,
    pub(crate) proto: u8,
}

impl LinuxCompatSerioDeviceId {
    pub(crate) const fn new(type_: u8, extra: u8, id: u8, proto: u8) -> Self {
        Self {
            type_,
            extra,
            id,
            proto,
        }
    }

    pub(crate) const fn is_terminator(self) -> bool {
        self.type_ == 0 && self.extra == 0 && self.id == 0 && self.proto == 0
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxCompatListHead {
    pub(crate) next: *mut LinuxCompatListHead,
    pub(crate) prev: *mut LinuxCompatListHead,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxCompatMutex {
    pub(crate) bytes: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxCompatDeviceDriver {
    pub(crate) name: *const c_char,
    pub(crate) bus: *const c_void,
    pub(crate) owner: *mut c_void,
    pub(crate) mod_name: *const c_char,
    pub(crate) suppress_bind_attrs: bool,
    pub(crate) _pad0: [u8; 3],
    pub(crate) probe_type: u32,
    pub(crate) of_match_table: *const c_void,
    pub(crate) acpi_match_table: *const c_void,
    pub(crate) probe: *const c_void,
    pub(crate) sync_state: *const c_void,
    pub(crate) remove: *const c_void,
    pub(crate) shutdown: *const c_void,
    pub(crate) suspend: *const c_void,
    pub(crate) resume: *const c_void,
    pub(crate) groups: *const *const c_void,
    pub(crate) dev_groups: *const *const c_void,
    pub(crate) pm: *const c_void,
    pub(crate) coredump: *const c_void,
    pub(crate) p: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxCompatDevice {
    pub(crate) kobj: [u8; 64],
    pub(crate) parent: *mut LinuxCompatDevice,
    pub(crate) p: *mut c_void,
    pub(crate) init_name: *const c_char,
    pub(crate) type_: *const c_void,
    pub(crate) bus: *const c_void,
    pub(crate) driver: *mut LinuxCompatDeviceDriver,
    pub(crate) platform_data: *mut c_void,
    pub(crate) driver_data: *mut c_void,
    pub(crate) mutex: LinuxCompatMutex,
    pub(crate) tail: [u8; 624],
}

impl Default for LinuxCompatDevice {
    fn default() -> Self {
        Self {
            kobj: [0; 64],
            parent: core::ptr::null_mut(),
            p: core::ptr::null_mut(),
            init_name: core::ptr::null(),
            type_: core::ptr::null(),
            bus: core::ptr::null(),
            driver: core::ptr::null_mut(),
            platform_data: core::ptr::null_mut(),
            driver_data: core::ptr::null_mut(),
            mutex: LinuxCompatMutex::default(),
            tail: [0; 624],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct LinuxCompatInputId {
    pub(crate) bustype: u16,
    pub(crate) vendor: u16,
    pub(crate) product: u16,
    pub(crate) version: u16,
}

pub(crate) type LinuxCompatInputOpenFn = unsafe extern "C" fn(dev: *mut LinuxCompatInputDev) -> i32;
pub(crate) type LinuxCompatInputCloseFn = unsafe extern "C" fn(dev: *mut LinuxCompatInputDev);

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxCompatInputDev {
    pub(crate) name: *const c_char,
    pub(crate) phys: *const c_char,
    pub(crate) uniq: *const c_char,
    pub(crate) id: LinuxCompatInputId,
    pub(crate) propbit: [u64; 1],
    pub(crate) evbit: [u64; 1],
    pub(crate) keybit: [u64; 12],
    pub(crate) relbit: [u64; 1],
    pub(crate) absbit: [u64; 1],
    pub(crate) _pad0: [u8; 296],
    pub(crate) open: Option<LinuxCompatInputOpenFn>,
    pub(crate) close: Option<LinuxCompatInputCloseFn>,
    pub(crate) _pad1: [u8; 24],
    pub(crate) event_lock: [u8; 8],
    pub(crate) mutex: LinuxCompatMutex,
    pub(crate) _pad2: [u8; 8],
    pub(crate) dev: LinuxCompatDevice,
    pub(crate) _pad3: [u8; 88],
}

impl Default for LinuxCompatInputDev {
    fn default() -> Self {
        Self {
            name: core::ptr::null(),
            phys: core::ptr::null(),
            uniq: core::ptr::null(),
            id: LinuxCompatInputId::default(),
            propbit: [0; 1],
            evbit: [0; 1],
            keybit: [0; 12],
            relbit: [0; 1],
            absbit: [0; 1],
            _pad0: [0; 296],
            open: None,
            close: None,
            _pad1: [0; 24],
            event_lock: [0; 8],
            mutex: LinuxCompatMutex::default(),
            _pad2: [0; 8],
            dev: LinuxCompatDevice::default(),
            _pad3: [0; 88],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxCompatSerioDriver {
    pub(crate) description: *const c_char,
    pub(crate) id_table: *const LinuxCompatSerioDeviceId,
    pub(crate) manual_bind: bool,
    pub(crate) _pad0: [u8; 7],
    pub(crate) write_wakeup: Option<LinuxCompatSerioWriteWakeupFn>,
    pub(crate) interrupt: Option<LinuxCompatSerioInterruptFn>,
    pub(crate) connect: Option<LinuxCompatSerioConnectFn>,
    pub(crate) reconnect: Option<LinuxCompatSerioReconnectFn>,
    pub(crate) fast_reconnect: Option<LinuxCompatSerioReconnectFn>,
    pub(crate) disconnect: Option<LinuxCompatSerioDisconnectFn>,
    pub(crate) cleanup: Option<LinuxCompatSerioCleanupFn>,
    pub(crate) driver: LinuxCompatDeviceDriver,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxCompatSerio {
    pub(crate) port_data: *mut c_void,
    pub(crate) name: [u8; 32],
    pub(crate) phys: [u8; 32],
    pub(crate) firmware_id: [u8; 128],
    pub(crate) manual_bind: bool,
    pub(crate) id: LinuxCompatSerioDeviceId,
    pub(crate) lock: u32,
    pub(crate) _pad0: [u8; 4],
    pub(crate) write: *const c_void,
    pub(crate) open: *const c_void,
    pub(crate) close: *const c_void,
    pub(crate) start: *const c_void,
    pub(crate) stop: *const c_void,
    pub(crate) parent: *mut LinuxCompatSerio,
    pub(crate) child_node: LinuxCompatListHead,
    pub(crate) children: LinuxCompatListHead,
    pub(crate) depth: u32,
    pub(crate) _pad1: [u8; 4],
    pub(crate) drv: *mut LinuxCompatSerioDriver,
    pub(crate) drv_mutex: LinuxCompatMutex,
    pub(crate) dev: LinuxCompatDevice,
    pub(crate) node: LinuxCompatListHead,
    pub(crate) ps2_cmd_mutex: *mut LinuxCompatMutex,
}

impl LinuxCompatSerio {
    pub(crate) const fn from_port_info(info: SerioPortInfo) -> Self {
        let mut name = [0_u8; 32];
        let mut phys = [0_u8; 32];
        name[0] = b'i';
        name[1] = b'8';
        name[2] = b'0';
        name[3] = b'4';
        name[4] = b'2';
        name[5] = b'-';
        name[6] = b's';
        name[7] = b'e';
        name[8] = b'r';
        name[9] = b'i';
        name[10] = b'o';
        phys[0] = b's';
        phys[1] = b'e';
        phys[2] = b'r';
        phys[3] = b'i';
        phys[4] = b'o';
        phys[5] = b'/';
        phys[6] = b'0';

        Self {
            port_data: core::ptr::null_mut(),
            name,
            phys,
            firmware_id: [0; 128],
            manual_bind: false,
            id: LinuxCompatSerioDeviceId::new(
                info.type_ as u8,
                info.extra as u8,
                info.id as u8,
                info.proto as u8,
            ),
            lock: 0,
            _pad0: [0; 4],
            write: core::ptr::null(),
            open: core::ptr::null(),
            close: core::ptr::null(),
            start: core::ptr::null(),
            stop: core::ptr::null(),
            parent: core::ptr::null_mut(),
            child_node: LinuxCompatListHead {
                next: core::ptr::null_mut(),
                prev: core::ptr::null_mut(),
            },
            children: LinuxCompatListHead {
                next: core::ptr::null_mut(),
                prev: core::ptr::null_mut(),
            },
            depth: 0,
            _pad1: [0; 4],
            drv: core::ptr::null_mut(),
            drv_mutex: LinuxCompatMutex { bytes: [0; 32] },
            dev: LinuxCompatDevice {
                kobj: [0; 64],
                parent: core::ptr::null_mut(),
                p: core::ptr::null_mut(),
                init_name: core::ptr::null(),
                type_: core::ptr::null(),
                bus: core::ptr::null(),
                driver: core::ptr::null_mut(),
                platform_data: core::ptr::null_mut(),
                driver_data: core::ptr::null_mut(),
                mutex: LinuxCompatMutex { bytes: [0; 32] },
                tail: [0; 624],
            },
            node: LinuxCompatListHead {
                next: core::ptr::null_mut(),
                prev: core::ptr::null_mut(),
            },
            ps2_cmd_mutex: core::ptr::null_mut(),
        }
    }

    pub(crate) fn driver_name_ptr(&self) -> *const c_char {
        if self.drv.is_null() {
            core::ptr::null()
        } else {
            unsafe { (*self.drv).driver.name }
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxCompatWaitQueueHead {
    pub(crate) lock: u32,
    pub(crate) _pad0: [u8; 4],
    pub(crate) head: LinuxCompatListHead,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxCompatPs2Dev {
    pub(crate) serio: *mut LinuxCompatSerio,
    pub(crate) cmd_mutex: LinuxCompatMutex,
    pub(crate) wait: LinuxCompatWaitQueueHead,
    pub(crate) flags: u64,
    pub(crate) cmdbuf: [u8; 8],
    pub(crate) cmdcnt: u8,
    pub(crate) nak: u8,
    pub(crate) _pad0: [u8; 6],
    pub(crate) pre_receive_handler: Option<LinuxCompatPs2PreReceiveHandler>,
    pub(crate) receive_handler: Option<LinuxCompatPs2ReceiveHandler>,
}

unsafe impl Send for LinuxCompatSerio {}
unsafe impl Send for LinuxCompatPs2Dev {}

pub(crate) fn compat_cstr(ptr: *const c_char) -> Option<&'static str> {
    if ptr.is_null() {
        return None;
    }
    let cstr = unsafe { core::ffi::CStr::from_ptr(ptr) };
    cstr.to_str().ok()
}

pub(crate) fn serio_any_matches(expected: u8, actual: u8) -> bool {
    expected == SERIO_ANY as u8 || expected == actual
}
