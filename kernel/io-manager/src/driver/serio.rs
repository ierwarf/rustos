// RING3-MIGRATION-REFERENCE START: Linux .ko serio compatibility bus
// substrate exception. Native non-.ko serio driver registration has been
// removed; ring0 keeps only compat port/driver callback dispatch.
use alloc::vec::Vec;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::sync::KernelSpinLock as Mutex;
use driver_abi::{SERIO_ANY, SerioDeviceId, SerioPortInfo};
use x86_64::instructions::interrupts;

use super::linux::compat::{
    LinuxCompatSerio, LinuxCompatSerioCloseFn, LinuxCompatSerioDeviceId, LinuxCompatSerioDriver,
    LinuxCompatSerioOpenFn, LinuxCompatSerioWriteFn, compat_cstr,
};

#[derive(Clone, Copy)]
pub(crate) struct RegisteredLinuxSerioDriver {
    pub(crate) driver_ptr: *mut LinuxCompatSerioDriver,
}

unsafe impl Send for RegisteredLinuxSerioDriver {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BoundSerioDriver {
    Linux(usize),
}

struct LinuxPortStorage(*mut LinuxCompatSerio);

impl LinuxPortStorage {
    fn as_ptr(&self) -> *mut LinuxCompatSerio {
        self.0
    }

    unsafe fn as_mut(&mut self) -> &mut LinuxCompatSerio {
        unsafe { &mut *self.as_ptr() }
    }
}

unsafe impl Send for LinuxPortStorage {}

struct RegisteredSerioPort {
    info: SerioPortInfo,
    linux_port: LinuxPortStorage,
    bound_driver: Option<BoundSerioDriver>,
    drvdata: usize,
    opened: bool,
}

static LINUX_SERIO_DRIVERS: Mutex<Vec<RegisteredLinuxSerioDriver>> = Mutex::new(Vec::new());
static SERIO_PORTS: Mutex<Vec<RegisteredSerioPort>> = Mutex::new(Vec::new());
static SERIO_RESCAN_PENDING: AtomicBool = AtomicBool::new(false);

fn irq_safe<T>(f: impl FnOnce() -> T) -> T {
    interrupts::without_interrupts(f)
}

fn request_bind_rescan() {
    SERIO_RESCAN_PENDING.store(true, Ordering::Release);
}

pub(crate) fn service_pending() -> usize {
    if SERIO_RESCAN_PENDING.swap(false, Ordering::AcqRel) {
        try_bind_all_ports();
        1
    } else {
        0
    }
}

pub(crate) unsafe extern "C" fn register_linux_driver(driver: *mut LinuxCompatSerioDriver) -> i32 {
    if driver.is_null() {
        return -22;
    }

    let driver_name_ptr = unsafe { (*driver).driver.name };
    let _description_ptr = unsafe { (*driver).description };
    let id_table = unsafe { (*driver).id_table };
    let _connect_ptr = unsafe {
        (*driver)
            .connect
            .map(|func| func as *const () as usize)
            .unwrap_or(0)
    };
    crate::debug::write_debugcon_only_line(
        alloc::format!(
            "serio register linux driver raw: driver_ptr={:#x} desc={:#x} name_ptr={:#x} id_table={:#x} connect={:#x}",
            driver as usize,
            _description_ptr as usize,
            driver_name_ptr as usize,
            id_table as usize,
            _connect_ptr
        )
        .as_bytes(),
    );
    let _driver_name = compat_cstr(driver_name_ptr).unwrap_or("invalid");

    {
        let mut drivers = LINUX_SERIO_DRIVERS.lock();
        if drivers.iter().any(|entry| entry.driver_ptr == driver) {
            return 0;
        }
        drivers.push(RegisteredLinuxSerioDriver { driver_ptr: driver });
    }

    crate::debug::write_debugcon_only_line(
        alloc::format!(
            "serio register linux driver: name={} driver_ptr={:#x} id_table={:#x}",
            _driver_name,
            driver as usize,
            id_table as usize
        )
        .as_bytes(),
    );
    if !id_table.is_null() {
        for index in 0..4 {
            let entry = unsafe { *id_table.add(index) };
            crate::debug::write_debugcon_only_line(
                alloc::format!(
                    "serio driver id[{}]: type={} proto={} id={} extra={}",
                    index,
                    entry.type_,
                    entry.proto,
                    entry.id,
                    entry.extra
                )
                .as_bytes(),
            );
            if entry.is_terminator() {
                break;
            }
        }
    }

    crate::debug::write_debugcon_only_line(
        alloc::format!(
            "serio register linux driver: schedule rescan name={}",
            _driver_name
        )
        .as_bytes(),
    );
    request_bind_rescan();
    0
}

pub(crate) fn find_linux_driver<T>(
    mut matcher: impl FnMut(usize, *mut LinuxCompatSerioDriver) -> Option<T>,
) -> Option<T> {
    irq_safe(|| {
        let drivers = LINUX_SERIO_DRIVERS.lock();
        for (index, entry) in drivers.iter().enumerate() {
            if let Some(result) = matcher(index, entry.driver_ptr) {
                return Some(result);
            }
        }
        None
    })
}

pub(crate) unsafe extern "C" fn unregister_linux_driver(driver: *mut LinuxCompatSerioDriver) {
    if driver.is_null() {
        return;
    }

    let removed_index = {
        let mut drivers = LINUX_SERIO_DRIVERS.lock();
        let Some(index) = drivers.iter().position(|entry| entry.driver_ptr == driver) else {
            return;
        };
        drivers.remove(index);
        index
    };

    let disconnects = {
        let mut ports = SERIO_PORTS.lock();
        let mut pending = Vec::new();
        for port in ports.iter_mut() {
            if port.bound_driver == Some(BoundSerioDriver::Linux(removed_index)) {
                port.bound_driver = None;
                unsafe {
                    super::linux::serio::clear_port_driver(port.linux_port.as_mut());
                }
                pending.push(port.linux_port.as_ptr());
            } else if let Some(BoundSerioDriver::Linux(index)) = port.bound_driver {
                if index > removed_index {
                    port.bound_driver = Some(BoundSerioDriver::Linux(index - 1));
                }
            }
        }
        pending
    };

    let disconnect = unsafe { (*driver).disconnect };
    if let Some(disconnect) = disconnect {
        for port in disconnects {
            unsafe { disconnect(port) };
        }
    }
}

pub(crate) fn ports_available() -> bool {
    irq_safe(|| !SERIO_PORTS.lock().is_empty())
}

pub(crate) unsafe fn register_linux_port(
    info: SerioPortInfo,
    linux_port: *mut LinuxCompatSerio,
) -> i32 {
    if linux_port.is_null() {
        return -22;
    }

    irq_safe(|| {
        let mut ports = SERIO_PORTS.lock();
        if let Some(existing) = ports
            .iter_mut()
            .find(|port| port.info.port_id == info.port_id)
        {
            existing.info = info;
            existing.linux_port = LinuxPortStorage(linux_port);
            existing.bound_driver = None;
            existing.drvdata = unsafe { (*linux_port).dev.driver_data as usize };
            existing.opened = false;
            sync_linux_port(existing);
        } else {
            ports.push(RegisteredSerioPort {
                info,
                linux_port: LinuxPortStorage(linux_port),
                bound_driver: None,
                drvdata: unsafe { (*linux_port).dev.driver_data as usize },
                opened: false,
            });
            let entry = ports.last_mut().expect("registered linux serio port");
            sync_linux_port(entry);
        }
    });

    request_bind_rescan();
    0
}

pub(crate) fn update_port_info(port_id: u32, proto: u32, id: u32, extra: u32) -> i32 {
    let status = irq_safe(|| {
        let mut ports = SERIO_PORTS.lock();
        let Some(port) = ports.iter_mut().find(|port| port.info.port_id == port_id) else {
            return -19;
        };
        port.info.proto = proto;
        port.info.id = id;
        port.info.extra = extra;
        sync_linux_port(port);
        0
    });
    if status == 0 {
        request_bind_rescan();
    }
    status
}

pub(crate) fn unregister_port(port_id: u32) -> i32 {
    enum DisconnectSnapshot {
        Linux {
            port: *mut LinuxCompatSerio,
            disconnect: Option<unsafe extern "C" fn(*mut LinuxCompatSerio)>,
        },
        None,
    }

    let snapshot = irq_safe(|| {
        let mut ports = SERIO_PORTS.lock();
        let linux_drivers = LINUX_SERIO_DRIVERS.lock();
        let Some(index) = ports.iter().position(|port| port.info.port_id == port_id) else {
            return None;
        };
        let port = ports.remove(index);
        Some(match port.bound_driver {
            Some(BoundSerioDriver::Linux(driver_index)) => {
                let disconnect = linux_drivers
                    .get(driver_index)
                    .and_then(|driver| unsafe { (*driver.driver_ptr).disconnect });
                DisconnectSnapshot::Linux {
                    port: port.linux_port.as_ptr(),
                    disconnect,
                }
            }
            None => DisconnectSnapshot::None,
        })
    });
    let Some(snapshot) = snapshot else {
        return -19;
    };

    match snapshot {
        DisconnectSnapshot::Linux {
            port,
            disconnect: Some(disconnect),
        } => unsafe {
            disconnect(port);
        },
        DisconnectSnapshot::Linux { .. } => {}
        DisconnectSnapshot::None => {}
    }

    0
}

pub(crate) fn open(port_id: u32) -> i32 {
    enum OpenSnapshot {
        Linux(*mut LinuxCompatSerio, LinuxCompatSerioOpenFn),
        None,
    }

    enum OpenPrepare {
        Missing,
        AlreadyOpen,
        Snapshot(OpenSnapshot),
    }

    let open_snapshot = irq_safe(|| {
        let mut ports = SERIO_PORTS.lock();
        let Some(port) = ports.iter_mut().find(|port| port.info.port_id == port_id) else {
            return OpenPrepare::Missing;
        };
        if port.opened {
            return OpenPrepare::AlreadyOpen;
        }
        port.opened = true;
        match linux_port_open_callback(port.linux_port.as_ptr()) {
            Some(open) => {
                OpenPrepare::Snapshot(OpenSnapshot::Linux(port.linux_port.as_ptr(), open))
            }
            None => OpenPrepare::Snapshot(OpenSnapshot::None),
        }
    });

    match open_snapshot {
        OpenPrepare::Missing => -19,
        OpenPrepare::AlreadyOpen => 0,
        OpenPrepare::Snapshot(OpenSnapshot::Linux(port, open)) => {
            let status = unsafe { open(port) };
            if status != 0 {
                irq_safe(|| {
                    let mut ports = SERIO_PORTS.lock();
                    if let Some(port) = ports.iter_mut().find(|port| port.info.port_id == port_id) {
                        port.opened = false;
                    }
                });
            }
            status
        }
        OpenPrepare::Snapshot(OpenSnapshot::None) => 0,
    }
}

pub(crate) fn close(port_id: u32) {
    enum CloseSnapshot {
        Linux(*mut LinuxCompatSerio, LinuxCompatSerioCloseFn),
        None,
    }

    enum ClosePrepare {
        Skip,
        Snapshot(CloseSnapshot),
    }

    let close_snapshot = irq_safe(|| {
        let mut ports = SERIO_PORTS.lock();
        let Some(port) = ports.iter_mut().find(|port| port.info.port_id == port_id) else {
            return ClosePrepare::Skip;
        };
        if !port.opened {
            return ClosePrepare::Skip;
        }
        port.opened = false;
        match linux_port_close_callback(port.linux_port.as_ptr()) {
            Some(close) => {
                ClosePrepare::Snapshot(CloseSnapshot::Linux(port.linux_port.as_ptr(), close))
            }
            None => ClosePrepare::Snapshot(CloseSnapshot::None),
        }
    });

    match close_snapshot {
        ClosePrepare::Skip => {}
        ClosePrepare::Snapshot(CloseSnapshot::Linux(port, close)) => unsafe { close(port) },
        ClosePrepare::Snapshot(CloseSnapshot::None) => {}
    }
}

pub(crate) fn driver_data(port_id: u32) -> usize {
    irq_safe(|| {
        let ports = SERIO_PORTS.lock();
        ports
            .iter()
            .find(|port| port.info.port_id == port_id)
            .map(|port| port.drvdata)
            .unwrap_or(0)
    })
}

pub(crate) fn set_driver_data(port_id: u32, drvdata: usize) -> i32 {
    irq_safe(|| {
        let mut ports = SERIO_PORTS.lock();
        let Some(port) = ports.iter_mut().find(|port| port.info.port_id == port_id) else {
            return -19;
        };
        port.drvdata = drvdata;
        unsafe {
            port.linux_port.as_mut().dev.driver_data = drvdata as *mut c_void;
        }
        0
    })
}

pub(crate) fn rescan(port_id: u32) -> i32 {
    let status = irq_safe(|| {
        let mut ports = SERIO_PORTS.lock();
        let Some(port) = ports.iter_mut().find(|port| port.info.port_id == port_id) else {
            return -19;
        };
        port.bound_driver = None;
        unsafe {
            super::linux::serio::clear_port_driver(port.linux_port.as_mut());
        }
        0
    });
    if status != 0 {
        return status;
    }

    request_bind_rescan();
    0
}

pub(crate) fn reconnect(port_id: u32) -> i32 {
    enum ReconnectSnapshot {
        Linux(
            *mut LinuxCompatSerio,
            Option<unsafe extern "C" fn(*mut LinuxCompatSerio) -> i32>,
        ),
        Rescan,
    }

    let snapshot = irq_safe(|| {
        let ports = SERIO_PORTS.lock();
        let linux_drivers = LINUX_SERIO_DRIVERS.lock();
        let Some(port) = ports.iter().find(|port| port.info.port_id == port_id) else {
            return None;
        };
        Some(match port.bound_driver {
            Some(BoundSerioDriver::Linux(index)) => {
                let Some(driver) = linux_drivers.get(index) else {
                    return None;
                };
                let reconnect = unsafe { (*driver.driver_ptr).reconnect };
                ReconnectSnapshot::Linux(port.linux_port.as_ptr(), reconnect)
            }
            None => ReconnectSnapshot::Rescan,
        })
    });
    let Some(snapshot) = snapshot else {
        return -19;
    };

    match snapshot {
        ReconnectSnapshot::Linux(port, Some(reconnect)) => unsafe { reconnect(port) },
        ReconnectSnapshot::Linux(_, None) => 0,
        ReconnectSnapshot::Rescan => rescan(port_id),
    }
}

pub(crate) fn receive_byte(port_id: u32, byte: u8, flags: u32) -> bool {
    enum InterruptSnapshot {
        Linux(
            *mut LinuxCompatSerio,
            unsafe extern "C" fn(*mut LinuxCompatSerio, u8, u32) -> i32,
        ),
    }

    let snapshot = irq_safe(|| {
        let ports = SERIO_PORTS.lock();
        let linux_drivers = LINUX_SERIO_DRIVERS.lock();
        let Some(port) = ports.iter().find(|port| port.info.port_id == port_id) else {
            return None;
        };
        if !port.opened {
            return None;
        }
        Some(match port.bound_driver {
            Some(BoundSerioDriver::Linux(index)) => {
                let Some(driver) = linux_drivers.get(index) else {
                    return None;
                };
                let Some(interrupt) = (unsafe { (*driver.driver_ptr).interrupt }) else {
                    return None;
                };
                InterruptSnapshot::Linux(port.linux_port.as_ptr(), interrupt)
            }
            None => return None,
        })
    });
    let Some(snapshot) = snapshot else {
        return false;
    };

    match snapshot {
        InterruptSnapshot::Linux(port, interrupt) => unsafe { interrupt(port, byte, flags) != 0 },
    }
}

pub(crate) fn interrupt(port_id: u32, byte: u8, flags: u32) -> i32 {
    if receive_byte(port_id, byte, flags) {
        1
    } else {
        0
    }
}

pub(crate) fn write(port_id: u32, byte: u8) -> i32 {
    let write_snapshot = irq_safe(|| {
        let ports = SERIO_PORTS.lock();
        let Some(port) = ports.iter().find(|port| port.info.port_id == port_id) else {
            return Err(-19);
        };
        let Some(write_byte) = linux_port_write_callback(port.linux_port.as_ptr()) else {
            return Err(-38);
        };
        Ok((port.linux_port.as_ptr(), write_byte))
    });
    let (port, write_byte) = match write_snapshot {
        Ok(snapshot) => snapshot,
        Err(status) => return status,
    };

    unsafe { write_byte(port, byte) }
}

pub(crate) fn drain(port_id: u32, _max_bytes: usize, _timeout_ms: u32) {
    let _exists = irq_safe(|| {
        let ports = SERIO_PORTS.lock();
        ports.iter().any(|port| port.info.port_id == port_id)
    });
}

pub(crate) unsafe extern "C" fn ps2_command(
    port_id: u32,
    command: u8,
    _data_ptr: *const u8,
    data_len: u32,
    _response_ptr: *mut u8,
    response_len: u32,
) -> i32 {
    crate::debug::println!(
        "serio ps2_command dispatch: port={} cmd={:#x} send={} recv={}",
        port_id,
        command,
        data_len,
        response_len
    );
    let status = irq_safe(|| {
        let ports = SERIO_PORTS.lock();
        if ports.iter().any(|port| port.info.port_id == port_id) {
            -38
        } else {
            -19
        }
    });
    crate::debug::println!(
        "serio ps2_command done: port={} cmd={:#x} status={}",
        port_id,
        command,
        status
    );
    status
}

pub(crate) fn port_id_for_linux_port(serio: *mut LinuxCompatSerio) -> Option<u32> {
    if serio.is_null() {
        return None;
    }
    irq_safe(|| {
        let ports = SERIO_PORTS.lock();
        ports
            .iter()
            .find(|port| port.linux_port.as_ptr() == serio)
            .map(|port| port.info.port_id)
    })
}

fn sync_linux_port(port: &mut RegisteredSerioPort) {
    let linux_port = unsafe { port.linux_port.as_mut() };
    linux_port.id = LinuxCompatSerioDeviceId::new(
        port.info.type_ as u8,
        port.info.extra as u8,
        port.info.id as u8,
        port.info.proto as u8,
    );
    linux_port.port_data = port.info.port_id as usize as *mut c_void;
    linux_port.dev.driver_data = port.drvdata as *mut c_void;
}

fn try_bind_all_ports() {
    let port_ids = irq_safe(|| {
        let ports = SERIO_PORTS.lock();
        let mut ids = Vec::with_capacity(ports.len());
        for port in ports.iter() {
            ids.push(port.info.port_id);
        }
        ids
    });

    crate::debug::println!("serio bind_all: ports={}", port_ids.len());
    for port_id in port_ids {
        crate::debug::println!("serio bind_all: try port={}", port_id);
        try_bind_port(port_id);
        crate::debug::println!("serio bind_all: done port={}", port_id);
    }
}

fn try_bind_port(port_id: u32) {
    let Some(linux_port_ptr) = irq_safe(|| {
        let ports = SERIO_PORTS.lock();
        match ports.iter().find(|port| port.info.port_id == port_id) {
            Some(port) if port.bound_driver.is_none() => Some(port.linux_port.as_ptr()),
            Some(_) => {
                crate::debug::println!("serio bind: port={} already bound", port_id);
                None
            }
            None => {
                crate::debug::println!("serio bind: port={} missing", port_id);
                None
            }
        }
    }) else {
        return;
    };

    crate::debug::println!("serio bind linux probe begin: port={}", port_id);
    if let Some((index, driver_ptr)) =
        super::linux::serio::first_matching_driver(port_id, linux_port_ptr)
    {
        crate::debug::println!(
            "serio bind snapshot resolved: port={} matched=true",
            port_id
        );
        bind_linux_port(port_id, index, driver_ptr, linux_port_ptr);
        return;
    }
    crate::debug::println!("serio bind linux probe end: port={} matched=false", port_id);

    crate::debug::println!(
        "serio bind snapshot resolved: port={} matched=false",
        port_id
    );
    irq_safe(|| {
        let ports = SERIO_PORTS.lock();
        if let Some(_port) = ports.iter().find(|port| port.info.port_id == port_id) {
            crate::debug::println!(
                "serio no match: port={} type={} proto={} id={} extra={}",
                _port.info.port_id,
                _port.info.type_,
                _port.info.proto,
                _port.info.id,
                _port.info.extra
            );
        }
    });
}

fn bind_linux_port(
    port_id: u32,
    index: usize,
    driver: *mut LinuxCompatSerioDriver,
    port: *mut LinuxCompatSerio,
) {
    crate::debug::println!("serio bind candidate ready(linux): port={}", port_id);
    let binding = BoundSerioDriver::Linux(index);
    if !apply_bound_driver_state(port_id, binding, Some(driver)) {
        return;
    }

    crate::debug::println!(
        "serio connect begin(linux): port={} type={} proto={} driver={}",
        port_id,
        unsafe { (*port).id.type_ },
        unsafe { (*port).id.proto },
        super::linux::serio::driver_name(driver)
    );
    let connect_status = unsafe { super::linux::serio::connect_driver(port, driver) };
    if connect_status != 0 {
        rollback_bound_driver_state(port_id, binding, connect_status);
        return;
    }

    irq_safe(|| {
        let ports = SERIO_PORTS.lock();
        let Some(_entry) = ports
            .iter()
            .find(|entry| entry.info.port_id == port_id && entry.bound_driver == Some(binding))
        else {
            return;
        };
        crate::debug::println!(
            "serio bound: port={} type={} proto={} driver={}",
            _entry.info.port_id,
            _entry.info.type_,
            _entry.info.proto,
            super::linux::serio::driver_name(driver)
        );
    });
}

fn apply_bound_driver_state(
    port_id: u32,
    binding: BoundSerioDriver,
    linux_driver: Option<*mut LinuxCompatSerioDriver>,
) -> bool {
    crate::debug::println!("serio bind acquiring port state: port={}", port_id);
    let state_applied = interrupts::without_interrupts(|| {
        let mut ports = SERIO_PORTS.lock();
        let Some(entry) = ports
            .iter_mut()
            .find(|entry| entry.info.port_id == port_id && entry.bound_driver.is_none())
        else {
            return false;
        };
        entry.bound_driver = Some(binding);
        if let Some(driver) = linux_driver {
            unsafe { super::linux::serio::apply_port_driver(entry.linux_port.as_mut(), driver) };
        }
        true
    });
    if state_applied {
        crate::debug::println!("serio bind state applied: port={}", port_id);
    }
    state_applied
}

fn rollback_bound_driver_state(port_id: u32, binding: BoundSerioDriver, _connect_status: i32) {
    interrupts::without_interrupts(|| {
        let mut ports = SERIO_PORTS.lock();
        if let Some(entry) = ports
            .iter_mut()
            .find(|entry| entry.info.port_id == port_id && entry.bound_driver == Some(binding))
        {
            crate::debug::println!(
                "serio connect failed: port={} type={} proto={} status={}",
                entry.info.port_id,
                entry.info.type_,
                entry.info.proto,
                _connect_status
            );
            entry.bound_driver = None;
            unsafe {
                super::linux::serio::clear_port_driver(entry.linux_port.as_mut());
            }
            sync_linux_port(entry);
        }
    });
}

fn linux_port_write_callback(port: *mut LinuxCompatSerio) -> Option<LinuxCompatSerioWriteFn> {
    if port.is_null() {
        return None;
    }
    let callback = unsafe { (*port).write };
    if callback.is_null() {
        None
    } else {
        Some(unsafe { core::mem::transmute(callback) })
    }
}

fn linux_port_open_callback(port: *mut LinuxCompatSerio) -> Option<LinuxCompatSerioOpenFn> {
    if port.is_null() {
        return None;
    }
    let callback = unsafe { (*port).open };
    if callback.is_null() {
        None
    } else {
        Some(unsafe { core::mem::transmute(callback) })
    }
}

fn linux_port_close_callback(port: *mut LinuxCompatSerio) -> Option<LinuxCompatSerioCloseFn> {
    if port.is_null() {
        return None;
    }
    let callback = unsafe { (*port).close };
    if callback.is_null() {
        None
    } else {
        Some(unsafe { core::mem::transmute(callback) })
    }
}

fn device_id_matches(id: SerioDeviceId, port: SerioPortInfo) -> bool {
    field_matches(id.type_, port.type_)
        && field_matches(id.proto, port.proto)
        && field_matches(id.id, port.id)
        && field_matches(id.extra, port.extra)
}

fn field_matches(expected: u32, actual: u32) -> bool {
    expected == SERIO_ANY || expected == actual
}
// RING3-MIGRATION-REFERENCE END: Linux .ko serio compatibility bus substrate exception.
