use alloc::boxed::Box;
use alloc::vec::Vec;
use core::arch::asm;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use driver_abi::{
    PointerPacket, BTN_EXTRA, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE, EV_KEY, EV_REL, EV_SYN,
    POINTER_BUTTON_LEFT, POINTER_BUTTON_MIDDLE, POINTER_BUTTON_RIGHT, POINTER_BUTTON_X1,
    POINTER_BUTTON_X2, REL_HWHEEL, REL_WHEEL, REL_X, REL_Y, SYN_REPORT,
};
use spin::{Mutex, MutexGuard};
#[cfg(not(test))]
use x86_64::instructions::interrupts;

use super::compat::LinuxCompatInputDev;
const EV_ABS: u32 = 0x03;
const ABS_X: u32 = 0x00;
const ABS_Y: u32 = 0x01;
const ABS_SLOT_RANGE: usize = 64;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CompatAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

struct CompatInputDevice {
    dev: Box<LinuxCompatInputDev>,
    registered: bool,
    opened: bool,
    current_buttons: u8,
    pending_dx: i32,
    pending_dy: i32,
    pending_wheel_vertical: i32,
    pending_wheel_horizontal: i32,
    last_abs_x: Option<i32>,
    last_abs_y: Option<i32>,
    pending_abs_x: Option<i32>,
    pending_abs_y: Option<i32>,
    absbit: [u64; 2],
    absinfo: [CompatAbsInfo; ABS_SLOT_RANGE],
    mt_slots: u32,
    mt_flags: u32,
}

unsafe impl Send for CompatInputDevice {}

static INPUT_DEVICES: Mutex<Vec<CompatInputDevice>> = Mutex::new(Vec::new());
static INPUT_CONSUMERS: AtomicUsize = AtomicUsize::new(0);
static INPUT_PACKET_DEBUG_REMAINING: AtomicUsize = AtomicUsize::new(0);
static INPUT_CALL_DEBUG_REMAINING: AtomicUsize = AtomicUsize::new(0);
static INPUT_LOCK_ACTIVE: AtomicUsize = AtomicUsize::new(0);
static INPUT_LOCK_LAST_SEQ: AtomicU64 = AtomicU64::new(0);
static INPUT_LOCK_NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

struct InputDevicesLock {
    guard: Option<MutexGuard<'static, Vec<CompatInputDevice>>>,
    restore_interrupts: bool,
    seq: u64,
}

impl InputDevicesLock {
    fn acquire() -> Self {
        let (guard, restore_interrupts) = lock_input_devices();

        let seq = INPUT_LOCK_NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
        INPUT_LOCK_LAST_SEQ.store(seq, Ordering::Release);
        INPUT_LOCK_ACTIVE.fetch_add(1, Ordering::AcqRel);

        Self {
            guard: Some(guard),
            restore_interrupts,
            seq,
        }
    }

    fn devices(&self) -> &[CompatInputDevice] {
        self.guard.as_deref().expect("input devices lock released")
    }

    fn devices_mut(&mut self) -> &mut Vec<CompatInputDevice> {
        self.guard
            .as_deref_mut()
            .expect("input devices lock released")
    }
}

impl Drop for InputDevicesLock {
    fn drop(&mut self) {
        let _ = self.guard.take();
        INPUT_LOCK_LAST_SEQ.store(self.seq, Ordering::Release);
        INPUT_LOCK_ACTIVE.fetch_sub(1, Ordering::AcqRel);

        #[cfg(not(test))]
        if self.restore_interrupts {
            interrupts::enable();
        }
    }
}

pub fn debug_lock_snapshot() -> (usize, u64) {
    (
        INPUT_LOCK_ACTIVE.load(Ordering::Acquire),
        INPUT_LOCK_LAST_SEQ.load(Ordering::Acquire),
    )
}

pub(crate) unsafe extern "C" fn allocate_device() -> *mut LinuxCompatInputDev {
    let mut devices = InputDevicesLock::acquire();
    let devices = devices.devices_mut();
    devices.push(CompatInputDevice {
        dev: Box::<LinuxCompatInputDev>::default(),
        registered: false,
        opened: false,
        current_buttons: 0,
        pending_dx: 0,
        pending_dy: 0,
        pending_wheel_vertical: 0,
        pending_wheel_horizontal: 0,
        last_abs_x: None,
        last_abs_y: None,
        pending_abs_x: None,
        pending_abs_y: None,
        absbit: [0; 2],
        absinfo: [CompatAbsInfo::default(); ABS_SLOT_RANGE],
        mt_slots: 0,
        mt_flags: 0,
    });
    let Some(device) = devices.last_mut() else {
        return core::ptr::null_mut();
    };
    &mut *device.dev
}

pub(crate) unsafe extern "C" fn free_device(dev: *mut LinuxCompatInputDev) {
    if dev.is_null() {
        return;
    }

    let removed = {
        let mut devices = InputDevicesLock::acquire();
        let devices = devices.devices_mut();
        let index = match devices
            .iter()
            .position(|device| core::ptr::eq(device.dev.as_ref(), unsafe { &*dev }))
        {
            Some(index) => index,
            None => return,
        };
        Some(devices.remove(index))
    };
    let Some(device) = removed else {
        return;
    };

    let close = if device.opened {
        device
            .dev
            .close
            .map(|close| (device.dev.as_ref() as *const _ as usize, close))
    } else {
        None
    };
    if let Some((dev_ptr, close)) = close {
        unsafe { close(dev_ptr as *mut LinuxCompatInputDev) };
    }
}

pub(crate) unsafe extern "C" fn register_device(dev: *mut LinuxCompatInputDev) -> i32 {
    crate::debug::println!("linux input_register_device: dev={:#x}", dev as usize);
    let (already_registered, open) = {
        let mut devices = InputDevicesLock::acquire();
        let devices = devices.devices_mut();
        let Some(device) = find_device_mut(devices, dev) else {
            return -19;
        };
        if device.registered {
            (true, None)
        } else {
            let should_open = INPUT_CONSUMERS.load(Ordering::Acquire) != 0;
            let open = if should_open {
                let dev_ptr = device.dev.as_ref() as *const _ as *mut LinuxCompatInputDev;
                device.dev.open.map(|open| (dev_ptr as usize, open))
            } else {
                None
            };
            (false, open)
        }
    };
    if already_registered {
        return 0;
    }

    if let Some((dev_ptr, open)) = open {
        let status = unsafe { open(dev_ptr as *mut LinuxCompatInputDev) };
        if status != 0 {
            return status;
        }
    }

    {
        let mut devices = InputDevicesLock::acquire();
        let devices = devices.devices_mut();
        let Some(device) = find_device_mut(devices, dev) else {
            return -19;
        };
        device.registered = true;
        device.opened = INPUT_CONSUMERS.load(Ordering::Acquire) != 0;
    }

    crate::debug::println!("linux input_register_device done: dev={:#x}", dev as usize);
    0
}

pub(crate) unsafe extern "C" fn unregister_device(dev: *mut LinuxCompatInputDev) {
    if dev.is_null() {
        return;
    }

    unsafe { free_device(dev) };
}

pub(crate) unsafe extern "C" fn set_capability(
    dev: *mut LinuxCompatInputDev,
    event_type: u32,
    code: u32,
) -> i32 {
    let mut _rsp = 0usize;
    unsafe {
        asm!("mov {}, rsp", out(reg) _rsp, options(nomem, nostack, preserves_flags));
    }
    let remaining = INPUT_CALL_DEBUG_REMAINING.load(Ordering::Relaxed);
    if remaining != 0
        && INPUT_CALL_DEBUG_REMAINING
            .compare_exchange(
                remaining,
                remaining - 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
    {
        crate::debug::println!(
            "linux input_set_capability enter: rsp_mod16={} dev={:#x} type={} code={}",
            _rsp & 0xf,
            dev as usize,
            event_type,
            code
        );
    }

    let mut devices = InputDevicesLock::acquire();
    let devices = devices.devices_mut();
    let Some(device) = find_device_mut(devices, dev) else {
        return -19;
    };
    set_capability_bits(device, event_type, code)
}

pub(crate) unsafe extern "C" fn event(
    dev: *mut LinuxCompatInputDev,
    event_type: u32,
    code: u32,
    value: i32,
) -> i32 {
    let packet = {
        let mut devices = InputDevicesLock::acquire();
        let devices = devices.devices_mut();
        let Some(device) = find_device_mut(devices, dev) else {
            return -19;
        };
        match event_type {
            EV_REL => {
                apply_relative_event(device, code, value);
                None
            }
            EV_KEY => {
                if apply_key_event(device, code, value).is_none() {
                    apply_keyboard_event(code, value);
                }
                None
            }
            EV_ABS => {
                apply_absolute_event(device, code, value);
                None
            }
            EV_SYN if code == SYN_REPORT => Some(take_packet(device)),
            _ => None,
        }
    };

    if let Some(packet) = packet {
        let remaining = INPUT_PACKET_DEBUG_REMAINING.load(Ordering::Relaxed);
        if remaining != 0
            && INPUT_PACKET_DEBUG_REMAINING
                .compare_exchange(
                    remaining,
                    remaining - 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
        {
            crate::debug::println!(
                "linux input packet: dx={} dy={} wheel_v={} wheel_h={} buttons={:#x}",
                packet.dx,
                packet.dy,
                packet.wheel_vertical,
                packet.wheel_horizontal,
                packet.buttons
            );
        }
        crate::driver::input::submit_pointer_packet(packet);
    }
    0
}

pub(crate) unsafe extern "C" fn report_key(dev: *mut LinuxCompatInputDev, code: u32, value: i32) {
    let _ = unsafe { event(dev, EV_KEY, code, value) };
}

pub(crate) unsafe extern "C" fn report_rel(dev: *mut LinuxCompatInputDev, code: u32, value: i32) {
    let _ = unsafe { event(dev, EV_REL, code, value) };
}

pub(crate) unsafe extern "C" fn sync(dev: *mut LinuxCompatInputDev) {
    let _ = unsafe { event(dev, EV_SYN, SYN_REPORT, 0) };
}

pub(crate) unsafe extern "C" fn set_drvdata(dev: *mut LinuxCompatInputDev, drvdata: usize) {
    let mut devices = InputDevicesLock::acquire();
    let devices = devices.devices_mut();
    if let Some(device) = find_device_mut(devices, dev) {
        device.dev.dev.driver_data = drvdata as *mut core::ffi::c_void;
    }
}

pub(crate) unsafe extern "C" fn get_drvdata(dev: *mut LinuxCompatInputDev) -> usize {
    let devices = InputDevicesLock::acquire();
    find_device(devices.devices(), dev)
        .map(|device| device.dev.dev.driver_data as usize)
        .unwrap_or(0)
}

pub(crate) unsafe extern "C" fn alloc_absinfo(dev: *mut LinuxCompatInputDev) -> i32 {
    let mut devices = InputDevicesLock::acquire();
    if find_device_mut(devices.devices_mut(), dev).is_none() {
        return -19;
    }
    0
}

pub(crate) unsafe extern "C" fn set_abs_params(
    dev: *mut LinuxCompatInputDev,
    axis: u32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
) {
    let mut devices = InputDevicesLock::acquire();
    let devices = devices.devices_mut();
    let Some(device) = find_device_mut(devices, dev) else {
        return;
    };
    let _ = set_bit(&mut device.dev.evbit, EV_ABS);
    let _ = set_bit(&mut device.absbit, axis);
    let Some(info) = device.absinfo.get_mut(axis as usize) else {
        return;
    };
    *info = CompatAbsInfo {
        minimum,
        maximum,
        fuzz,
        flat,
        ..*info
    };
}

pub(crate) unsafe extern "C" fn mt_init_slots(
    dev: *mut LinuxCompatInputDev,
    slots: u32,
    flags: u32,
) -> i32 {
    let mut devices = InputDevicesLock::acquire();
    let devices = devices.devices_mut();
    let Some(device) = find_device_mut(devices, dev) else {
        return -19;
    };
    device.mt_slots = slots;
    device.mt_flags = flags;
    0
}

pub(crate) unsafe extern "C" fn mt_assign_slots(
    dev: *mut LinuxCompatInputDev,
    slots: *mut i32,
    _positions: *const core::ffi::c_void,
    num_pos: i32,
    _dmax: i32,
) -> i32 {
    let span = {
        let devices = InputDevicesLock::acquire();
        let Some(device) = find_device(devices.devices(), dev) else {
            return -19;
        };
        device.mt_slots.max(1)
    };
    if slots.is_null() || num_pos <= 0 {
        return 0;
    }

    for index in 0..(num_pos as usize) {
        unsafe {
            *slots.add(index) = (index as u32 % span) as i32;
        }
    }
    num_pos
}

pub(crate) unsafe extern "C" fn mt_drop_unused(_dev: *mut LinuxCompatInputDev) {}

pub(crate) unsafe extern "C" fn mt_report_finger_count(
    _dev: *mut LinuxCompatInputDev,
    _count: i32,
) {
}

pub(crate) unsafe extern "C" fn mt_report_pointer_emulation(
    _dev: *mut LinuxCompatInputDev,
    _use_count: bool,
) {
}

pub(crate) unsafe extern "C" fn mt_report_slot_state(
    _dev: *mut LinuxCompatInputDev,
    _tool_type: u32,
    _active: bool,
) {
}

pub(crate) unsafe extern "C" fn mt_sync_frame(_dev: *mut LinuxCompatInputDev) {}

pub(crate) unsafe extern "C" fn ff_create(
    _dev: *mut LinuxCompatInputDev,
    _max_effects: u32,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn ff_event(
    _dev: *mut LinuxCompatInputDev,
    _type_: u32,
    _code: u32,
    _value: i32,
) {
}

pub(crate) unsafe extern "C" fn scancode_to_scalar(scancode: *const u8, len: usize) -> u32 {
    if scancode.is_null() || len == 0 {
        return 0;
    }

    let bytes = unsafe { core::slice::from_raw_parts(scancode, len.min(4)) };
    let mut value = 0u32;
    let mut index = 0usize;
    while index < bytes.len() {
        value |= (bytes[index] as u32) << (index * 8);
        index += 1;
    }
    value
}

pub(crate) fn consumer_acquire() {
    if INPUT_CONSUMERS.fetch_add(1, Ordering::AcqRel) != 0 {
        return;
    }

    let pending = with_input_devices(|devices| {
        let mut pending = Vec::new();
        for device in devices.iter_mut() {
            if !device.registered || device.opened {
                continue;
            }

            let dev_ptr = device.dev.as_ref() as *const _ as *mut LinuxCompatInputDev;
            if let Some(open) = device.dev.open {
                pending.push((dev_ptr as usize, open));
            }
            device.opened = true;
        }
        pending
    });

    let mut failed = Vec::new();
    for (dev_ptr, open) in pending {
        let status = unsafe { open(dev_ptr as *mut LinuxCompatInputDev) };
        if status != 0 {
            failed.push(dev_ptr);
        }
    }

    if !failed.is_empty() {
        with_input_devices(|devices| {
            for failed_ptr in failed {
                if let Some(device) = devices.iter_mut().find(|device| {
                    ptr_eq(device.dev.as_ref(), failed_ptr as *mut LinuxCompatInputDev)
                }) {
                    device.opened = false;
                }
            }
        });
    }
}

pub(crate) fn consumer_release() {
    let mut consumers = INPUT_CONSUMERS.load(Ordering::Acquire);
    loop {
        if consumers == 0 {
            return;
        }
        match INPUT_CONSUMERS.compare_exchange(
            consumers,
            consumers - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(actual) => consumers = actual,
        }
    }

    if consumers != 1 {
        return;
    }

    let pending = with_input_devices(|devices| {
        let mut pending = Vec::new();
        for device in devices.iter_mut() {
            if !device.opened {
                continue;
            }

            device.opened = false;
            if let Some(close) = device.dev.close {
                let dev_ptr = device.dev.as_ref() as *const _ as *mut LinuxCompatInputDev;
                pending.push((dev_ptr as usize, close));
            }
        }
        pending
    });

    for (dev_ptr, close) in pending {
        unsafe { close(dev_ptr as *mut LinuxCompatInputDev) };
    }
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "input_allocate_device" => Some(allocate_device as *const () as usize),
        "input_free_device" => Some(free_device as *const () as usize),
        "input_register_device" => Some(register_device as *const () as usize),
        "input_unregister_device" => Some(unregister_device as *const () as usize),
        "input_set_capability" => Some(set_capability as *const () as usize),
        "input_event" => Some(event as *const () as usize),
        "input_report_key" => Some(report_key as *const () as usize),
        "input_report_rel" => Some(report_rel as *const () as usize),
        "input_sync" => Some(sync as *const () as usize),
        "input_set_drvdata" => Some(set_drvdata as *const () as usize),
        "input_get_drvdata" => Some(get_drvdata as *const () as usize),
        "input_alloc_absinfo" => Some(alloc_absinfo as *const () as usize),
        "input_set_abs_params" => Some(set_abs_params as *const () as usize),
        "input_mt_init_slots" => Some(mt_init_slots as *const () as usize),
        "input_mt_assign_slots" => Some(mt_assign_slots as *const () as usize),
        "input_mt_drop_unused" => Some(mt_drop_unused as *const () as usize),
        "input_mt_report_finger_count" => Some(mt_report_finger_count as *const () as usize),
        "input_mt_report_pointer_emulation" => {
            Some(mt_report_pointer_emulation as *const () as usize)
        }
        "input_mt_report_slot_state" => Some(mt_report_slot_state as *const () as usize),
        "input_mt_sync_frame" => Some(mt_sync_frame as *const () as usize),
        "input_ff_create" => Some(ff_create as *const () as usize),
        "input_ff_event" => Some(ff_event as *const () as usize),
        "input_scancode_to_scalar" => Some(scancode_to_scalar as *const () as usize),
        _ => None,
    }
}

fn with_input_devices<R>(f: impl FnOnce(&mut Vec<CompatInputDevice>) -> R) -> R {
    let mut devices = InputDevicesLock::acquire();
    f(devices.devices_mut())
}

#[cfg(test)]
fn lock_input_devices() -> (MutexGuard<'static, Vec<CompatInputDevice>>, bool) {
    (INPUT_DEVICES.lock(), false)
}

#[cfg(not(test))]
fn lock_input_devices() -> (MutexGuard<'static, Vec<CompatInputDevice>>, bool) {
    if !interrupts::are_enabled() {
        return (INPUT_DEVICES.lock(), false);
    }

    loop {
        interrupts::disable();
        if let Some(guard) = INPUT_DEVICES.try_lock() {
            return (guard, true);
        }
        interrupts::enable();
        spin_loop();
    }
}

fn find_device<'a>(
    devices: &'a [CompatInputDevice],
    dev: *mut LinuxCompatInputDev,
) -> Option<&'a CompatInputDevice> {
    if dev.is_null() {
        return None;
    }
    for device in devices {
        if core::ptr::eq(device.dev.as_ref(), unsafe { &*dev }) {
            return Some(device);
        }
    }
    None
}

fn find_device_mut<'a>(
    devices: &'a mut [CompatInputDevice],
    dev: *mut LinuxCompatInputDev,
) -> Option<&'a mut CompatInputDevice> {
    if dev.is_null() {
        return None;
    }
    for device in devices {
        if core::ptr::eq(device.dev.as_ref(), unsafe { &*dev }) {
            return Some(device);
        }
    }
    None
}

fn ptr_eq(dev: &LinuxCompatInputDev, other: *mut LinuxCompatInputDev) -> bool {
    core::ptr::eq(dev, unsafe { &*other })
}

fn set_capability_bits(device: &mut CompatInputDevice, event_type: u32, code: u32) -> i32 {
    if let Err(err) = set_bit(&mut device.dev.evbit, event_type) {
        return err;
    }
    match event_type {
        EV_KEY => {
            if let Err(err) = set_bit(&mut device.dev.keybit, code) {
                return err;
            }
        }
        EV_REL => {
            if let Err(err) = set_bit(&mut device.dev.relbit, code) {
                return err;
            }
        }
        EV_ABS => {
            if let Err(err) = set_bit(&mut device.absbit, code) {
                return err;
            }
        }
        _ => {}
    }
    0
}

fn set_bit(bits: &mut [u64], bit: u32) -> Result<(), i32> {
    let index = (bit as usize) / 64;
    let shift = (bit as usize) % 64;
    let Some(word) = bits.get_mut(index) else {
        return Err(-22);
    };
    *word |= 1_u64 << shift;
    Ok(())
}

fn apply_relative_event(device: &mut CompatInputDevice, code: u32, value: i32) {
    match code {
        REL_X => device.pending_dx = device.pending_dx.saturating_add(value),
        REL_Y => device.pending_dy = device.pending_dy.saturating_add(value),
        REL_WHEEL => {
            device.pending_wheel_vertical = device.pending_wheel_vertical.saturating_add(value)
        }
        REL_HWHEEL => {
            device.pending_wheel_horizontal = device.pending_wheel_horizontal.saturating_add(value)
        }
        _ => {}
    }
}

fn apply_key_event(device: &mut CompatInputDevice, code: u32, value: i32) -> Option<()> {
    let pressed = value != 0;
    let mask = match code {
        BTN_LEFT => POINTER_BUTTON_LEFT,
        BTN_RIGHT => POINTER_BUTTON_RIGHT,
        BTN_MIDDLE => POINTER_BUTTON_MIDDLE,
        BTN_SIDE => POINTER_BUTTON_X1,
        BTN_EXTRA => POINTER_BUTTON_X2,
        _ => return None,
    };
    if pressed {
        device.current_buttons |= mask;
    } else {
        device.current_buttons &= !mask;
    }
    Some(())
}

fn apply_absolute_event(device: &mut CompatInputDevice, code: u32, value: i32) {
    let Some(info) = device.absinfo.get_mut(code as usize) else {
        return;
    };
    info.value = value;
    match code {
        ABS_X => device.pending_abs_x = Some(value),
        ABS_Y => device.pending_abs_y = Some(value),
        _ => {}
    }
}

fn take_packet(device: &mut CompatInputDevice) -> PointerPacket {
    if let Some(next_x) = device.pending_abs_x.take() {
        if let Some(previous_x) = device.last_abs_x.replace(next_x) {
            device.pending_dx = device.pending_dx.saturating_add(next_x - previous_x);
        }
    }
    if let Some(next_y) = device.pending_abs_y.take() {
        if let Some(previous_y) = device.last_abs_y.replace(next_y) {
            device.pending_dy = device.pending_dy.saturating_add(next_y - previous_y);
        }
    }

    let packet = PointerPacket {
        buttons: device.current_buttons,
        dx: saturating_i16(device.pending_dx),
        dy: saturating_i16(device.pending_dy),
        wheel_vertical: saturating_i16(device.pending_wheel_vertical),
        wheel_horizontal: saturating_i16(device.pending_wheel_horizontal),
        ..PointerPacket::default()
    };
    device.pending_dx = 0;
    device.pending_dy = 0;
    device.pending_wheel_vertical = 0;
    device.pending_wheel_horizontal = 0;
    packet
}

fn saturating_i16(value: i32) -> i16 {
    value.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

fn apply_keyboard_event(code: u32, value: i32) {
    let Some(key_code) = crate::input_core::linux_key_code_to_rustos(code) else {
        return;
    };
    crate::input::keyboard::inject_key_transition(key_code, value == 0);
}
