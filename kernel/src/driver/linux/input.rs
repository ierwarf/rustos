use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use driver_abi::{
    BTN_EXTRA, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE, EV_KEY, EV_REL, EV_SYN,
    POINTER_BUTTON_LEFT, POINTER_BUTTON_MIDDLE, POINTER_BUTTON_RIGHT, POINTER_BUTTON_X1,
    POINTER_BUTTON_X2, PointerPacket, REL_HWHEEL, REL_WHEEL, REL_X, REL_Y, SYN_REPORT,
};
use spin::Mutex;
#[cfg(not(test))]
use x86_64::instructions::interrupts;

use super::compat::LinuxCompatInputDev;

const EV_ABS: u32 = 0x03;
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
    absbit: [u64; 2],
    absinfo: [CompatAbsInfo; ABS_SLOT_RANGE],
    mt_slots: u32,
    mt_flags: u32,
}

unsafe impl Send for CompatInputDevice {}

static INPUT_DEVICES: Mutex<Vec<CompatInputDevice>> = Mutex::new(Vec::new());
static INPUT_CONSUMERS: AtomicUsize = AtomicUsize::new(0);

pub(crate) unsafe extern "C" fn allocate_device() -> *mut LinuxCompatInputDev {
    with_input_devices(|devices| {
        devices.push(CompatInputDevice {
            dev: Box::<LinuxCompatInputDev>::default(),
            registered: false,
            opened: false,
            current_buttons: 0,
            pending_dx: 0,
            pending_dy: 0,
            pending_wheel_vertical: 0,
            pending_wheel_horizontal: 0,
            absbit: [0; 2],
            absinfo: [CompatAbsInfo::default(); ABS_SLOT_RANGE],
            mt_slots: 0,
            mt_flags: 0,
        });
        let Some(device) = devices.last_mut() else {
            return core::ptr::null_mut();
        };
        &mut *device.dev
    })
}

pub(crate) unsafe extern "C" fn free_device(dev: *mut LinuxCompatInputDev) {
    if dev.is_null() {
        return;
    }

    let removed = with_input_devices(|devices| {
        let index = devices
            .iter()
            .position(|device| core::ptr::eq(device.dev.as_ref(), unsafe { &*dev }))?;
        Some(devices.remove(index))
    });
    let Some(device) = removed else {
        return;
    };

    let close = if device.opened {
        device.dev.close.map(|close| (device.dev.as_ref() as *const _ as usize, close))
    } else {
        None
    };
    if let Some((dev_ptr, close)) = close {
        unsafe { close(dev_ptr as *mut LinuxCompatInputDev) };
    }
}

pub(crate) unsafe extern "C" fn register_device(dev: *mut LinuxCompatInputDev) -> i32 {
    crate::debug::println!("linux input_register_device: dev={:#x}", dev as usize);
    let (already_registered, open) = match with_input_devices(|devices| {
        let device = find_device_mut(devices, dev)?;
        if device.registered {
            return Some((true, None));
        }

        let should_open = INPUT_CONSUMERS.load(Ordering::Acquire) != 0;
        let open = if should_open {
            let dev_ptr = device.dev.as_ref() as *const _ as *mut LinuxCompatInputDev;
            device.dev.open.map(|open| (dev_ptr as usize, open))
        } else {
            None
        };
        Some((false, open))
    }) {
        Some(open) => open,
        None => return -19,
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

    let updated = with_input_devices(|devices| {
        let device = find_device_mut(devices, dev)?;
        device.registered = true;
        device.opened = INPUT_CONSUMERS.load(Ordering::Acquire) != 0;
        Some(())
    });
    if updated.is_none() {
        return -19;
    }

    crate::debug::println!(
        "linux input_register_device done: dev={:#x}",
        dev as usize
    );
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
    match with_input_devices(|devices| {
        let device = find_device_mut(devices, dev)?;
        Some(set_capability_bits(device, event_type, code))
    }) {
        Some(status) => status,
        None => -19,
    }
}

pub(crate) unsafe extern "C" fn event(
    dev: *mut LinuxCompatInputDev,
    event_type: u32,
    code: u32,
    value: i32,
) -> i32 {
    let packet = match with_input_devices(|devices| {
        let device = find_device_mut(devices, dev)?;
        match event_type {
            EV_REL => apply_relative_event(device, code, value),
            EV_KEY => apply_key_event(device, code, value),
            EV_ABS => apply_absolute_event(device, code, value),
            EV_SYN if code == SYN_REPORT => return Some(Some(take_packet(device))),
            _ => {}
        }
        Some(None)
    }) {
        Some(packet) => packet,
        None => return -19,
    };

    if let Some(packet) = packet {
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
    let _ = with_input_devices(|devices| {
        let device = find_device_mut(devices, dev)?;
        device.dev.dev.driver_data = drvdata as *mut core::ffi::c_void;
        Some(())
    });
}

pub(crate) unsafe extern "C" fn get_drvdata(dev: *mut LinuxCompatInputDev) -> usize {
    with_input_devices(|devices| {
        find_device(devices, dev).map(|device| device.dev.dev.driver_data as usize)
    })
    .unwrap_or(0)
}

pub(crate) unsafe extern "C" fn alloc_absinfo(dev: *mut LinuxCompatInputDev) -> i32 {
    if with_input_devices(|devices| find_device_mut(devices, dev).map(|_| ())).is_none() {
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
    let _ = with_input_devices(|devices| {
        let device = find_device_mut(devices, dev)?;
        let _ = set_bit(&mut device.dev.evbit, EV_ABS);
        let _ = set_bit(&mut device.absbit, axis);
        let Some(info) = device.absinfo.get_mut(axis as usize) else {
            return Some(());
        };
        *info = CompatAbsInfo {
            minimum,
            maximum,
            fuzz,
            flat,
            ..*info
        };
        Some(())
    });
}

pub(crate) unsafe extern "C" fn mt_init_slots(
    dev: *mut LinuxCompatInputDev,
    slots: u32,
    flags: u32,
) -> i32 {
    let updated = with_input_devices(|devices| {
        let device = find_device_mut(devices, dev)?;
        device.mt_slots = slots;
        device.mt_flags = flags;
        Some(())
    });
    if updated.is_none() {
        return -19;
    }
    0
}

pub(crate) unsafe extern "C" fn mt_assign_slots(
    dev: *mut LinuxCompatInputDev,
    slots: *mut i32,
    _positions: *const core::ffi::c_void,
    num_pos: i32,
    _dmax: i32,
) -> i32 {
    let span = match with_input_devices(|devices| {
        find_device(devices, dev).map(|device| device.mt_slots.max(1))
    }) {
        Some(span) => span,
        None => return -19,
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
                if let Some(device) = devices
                    .iter_mut()
                    .find(|device| ptr_eq(device.dev.as_ref(), failed_ptr as *mut LinuxCompatInputDev))
                {
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
        _ => None,
    }
}

fn with_input_devices<R>(f: impl FnOnce(&mut Vec<CompatInputDevice>) -> R) -> R {
    #[cfg(test)]
    {
        f(&mut INPUT_DEVICES.lock())
    }

    #[cfg(not(test))]
    {
        interrupts::without_interrupts(|| f(&mut INPUT_DEVICES.lock()))
    }
}

fn find_device<'a>(
    devices: &'a [CompatInputDevice],
    dev: *mut LinuxCompatInputDev,
) -> Option<&'a CompatInputDevice> {
    if dev.is_null() {
        return None;
    }
    devices
        .iter()
        .find(|device| core::ptr::eq(device.dev.as_ref(), unsafe { &*dev }))
}

fn find_device_mut<'a>(
    devices: &'a mut [CompatInputDevice],
    dev: *mut LinuxCompatInputDev,
) -> Option<&'a mut CompatInputDevice> {
    if dev.is_null() {
        return None;
    }
    devices
        .iter_mut()
        .find(|device| core::ptr::eq(device.dev.as_ref(), unsafe { &*dev }))
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

fn apply_key_event(device: &mut CompatInputDevice, code: u32, value: i32) {
    let pressed = value != 0;
    let mask = match code {
        BTN_LEFT => POINTER_BUTTON_LEFT,
        BTN_RIGHT => POINTER_BUTTON_RIGHT,
        BTN_MIDDLE => POINTER_BUTTON_MIDDLE,
        BTN_SIDE => POINTER_BUTTON_X1,
        BTN_EXTRA => POINTER_BUTTON_X2,
        _ => 0,
    };
    if mask == 0 {
        return;
    }
    if pressed {
        device.current_buttons |= mask;
    } else {
        device.current_buttons &= !mask;
    }
}

fn apply_absolute_event(device: &mut CompatInputDevice, code: u32, value: i32) {
    let Some(info) = device.absinfo.get_mut(code as usize) else {
        return;
    };
    info.value = value;
}

fn take_packet(device: &mut CompatInputDevice) -> PointerPacket {
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
