use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use spin::Mutex;
use x86_64::instructions::interrupts;

use super::linux::compat::{
    LinuxCompatPciDev, LinuxCompatPciDeviceId, LinuxCompatPciDriver, LinuxCompatResource,
};

const DEVICE_RESOURCE_COUNT: usize = 17;
const PCI_ANY_ID: u32 = u32::MAX;

const IORESOURCE_IO: usize = 0x0000_0100;
const IORESOURCE_MEM: usize = 0x0000_0200;
const IORESOURCE_PREFETCH: usize = 0x0000_2000;
const IORESOURCE_MEM_64: usize = 0x0010_0000;
const IORESOURCE_UNSET: usize = 0x2000_0000;

static PCI_BUS_TYPE: [u8; 64] = [0; 64];
static PCI_DEVICES: Mutex<Vec<RegisteredPciDevice>> = Mutex::new(Vec::new());
static LINUX_PCI_DRIVERS: Mutex<Vec<RegisteredLinuxPciDriver>> = Mutex::new(Vec::new());
static PCI_ENUMERATED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
struct RegisteredLinuxPciDriver {
    driver_ptr: *mut LinuxCompatPciDriver,
}

unsafe impl Send for RegisteredLinuxPciDriver {}

struct RegisteredPciDevice {
    address: crate::arch::pci::PciDevice,
    compat_dev: Box<LinuxCompatPciDev>,
    bound_driver: Option<usize>,
}

unsafe impl Send for RegisteredPciDevice {}

pub(crate) fn bus_type_ptr() -> *const c_void {
    &PCI_BUS_TYPE as *const [u8; 64] as *const c_void
}

pub(crate) fn register_linux_driver(driver: *mut LinuxCompatPciDriver) -> i32 {
    if driver.is_null() {
        return -22;
    }

    ensure_enumerated();
    prepare_driver_struct(driver);

    let driver_index = irq_safe(|| {
        let mut drivers = LINUX_PCI_DRIVERS.lock();
        if let Some(index) = drivers.iter().position(|entry| entry.driver_ptr == driver) {
            return index;
        }
        drivers.push(RegisteredLinuxPciDriver { driver_ptr: driver });
        drivers.len() - 1
    });

    bind_driver_to_devices(driver_index, driver);
    0
}

pub(crate) fn unregister_linux_driver(driver: *mut LinuxCompatPciDriver) {
    if driver.is_null() {
        return;
    }

    let Some((removed_index, remove)) = irq_safe(|| {
        let mut drivers = LINUX_PCI_DRIVERS.lock();
        let index = drivers
            .iter()
            .position(|entry| entry.driver_ptr == driver)?;
        let remove = unsafe { (*driver).remove };
        drivers.remove(index);
        Some((index, remove))
    }) else {
        return;
    };

    let removals = irq_safe(|| {
        let devices = PCI_DEVICES.lock();
        devices
            .iter()
            .filter(|device| device.bound_driver == Some(removed_index))
            .map(|device| device.compat_dev.as_ref() as *const _ as usize)
            .collect::<Vec<_>>()
    });

    if let Some(remove) = remove {
        for dev_ptr in removals.iter().copied() {
            unsafe { remove(dev_ptr as *mut LinuxCompatPciDev) };
            let compat_dev = dev_ptr as *mut LinuxCompatPciDev;
            let dev_struct = unsafe { &mut (*compat_dev).dev as *mut _ as *mut c_void };
            crate::driver::devres::release_device(dev_struct);
        }
    } else {
        for dev_ptr in removals.iter().copied() {
            let compat_dev = dev_ptr as *mut LinuxCompatPciDev;
            let dev_struct = unsafe { &mut (*compat_dev).dev as *mut _ as *mut c_void };
            crate::driver::devres::release_device(dev_struct);
        }
    }

    irq_safe(|| {
        let mut devices = PCI_DEVICES.lock();
        for device in devices.iter_mut() {
            match device.bound_driver {
                Some(index) if index == removed_index => {
                    clear_bound_driver(device.compat_dev.as_mut());
                    device.bound_driver = None;
                }
                Some(index) if index > removed_index => {
                    device.bound_driver = Some(index - 1);
                }
                _ => {}
            }
        }
    });
}

pub(crate) fn enable_device(dev: *mut LinuxCompatPciDev) -> i32 {
    with_device_mut(dev, |device| {
        let mut set_bits = 0u16;
        for resource in device.compat_dev.resource.iter().take(6) {
            if (resource.flags & IORESOURCE_IO) != 0 {
                set_bits |= 1 << 0;
            }
            if (resource.flags & IORESOURCE_MEM) != 0 {
                set_bits |= 1 << 1;
            }
        }
        device.address.update_command_bits(set_bits, 0);
        0
    })
    .unwrap_or(-19)
}

pub(crate) fn disable_device(dev: *mut LinuxCompatPciDev) {
    let _ = with_device(dev, |device| {
        device
            .address
            .update_command_bits(0, (1 << 0) | (1 << 1) | (1 << 2));
    });
}

pub(crate) fn set_master(dev: *mut LinuxCompatPciDev) {
    let _ = with_device(dev, |device| {
        device.address.update_command_bits(1 << 2, 0);
    });
}

pub(crate) fn clear_master(dev: *mut LinuxCompatPciDev) {
    let _ = with_device(dev, |device| {
        device.address.update_command_bits(0, 1 << 2);
    });
}

pub(crate) fn resource_start(dev: *mut LinuxCompatPciDev, bar: u32) -> u64 {
    resource_field(dev, bar, |resource| resource.start).unwrap_or(0)
}

pub(crate) fn resource_end(dev: *mut LinuxCompatPciDev, bar: u32) -> u64 {
    resource_field(dev, bar, |resource| resource.end).unwrap_or(0)
}

pub(crate) fn resource_len(dev: *mut LinuxCompatPciDev, bar: u32) -> u64 {
    resource_field(dev, bar, |resource| {
        if resource.end < resource.start {
            0
        } else {
            resource.end - resource.start + 1
        }
    })
    .unwrap_or(0)
}

pub(crate) fn resource_flags(dev: *mut LinuxCompatPciDev, bar: u32) -> usize {
    resource_field(dev, bar, |resource| resource.flags).unwrap_or(0)
}

pub(crate) fn read_config_byte(dev: *mut LinuxCompatPciDev, offset: i32, value: *mut u8) -> i32 {
    read_config(dev, offset, value, |address, where_| {
        read_u8(address, where_)
    })
}

pub(crate) fn read_config_word(dev: *mut LinuxCompatPciDev, offset: i32, value: *mut u16) -> i32 {
    read_config(dev, offset, value, |address, where_| {
        read_u16(address, where_)
    })
}

pub(crate) fn read_config_dword(dev: *mut LinuxCompatPciDev, offset: i32, value: *mut u32) -> i32 {
    read_config(dev, offset, value, |address, where_| {
        read_u32(address, where_)
    })
}

pub(crate) fn write_config_byte(dev: *mut LinuxCompatPciDev, offset: i32, value: u8) -> i32 {
    with_device(dev, |device| write_u8(device.address, offset, value)).unwrap_or(-19)
}

pub(crate) fn write_config_word(dev: *mut LinuxCompatPciDev, offset: i32, value: u16) -> i32 {
    with_device(dev, |device| write_u16(device.address, offset, value)).unwrap_or(-19)
}

pub(crate) fn write_config_dword(dev: *mut LinuxCompatPciDev, offset: i32, value: u32) -> i32 {
    with_device(dev, |device| write_u32(device.address, offset, value)).unwrap_or(-19)
}

pub(crate) fn set_drvdata(dev: *mut LinuxCompatPciDev, drvdata: usize) {
    let _ = with_device_mut(dev, |device| {
        device.compat_dev.dev.driver_data = drvdata as *mut c_void;
    });
}

pub(crate) fn get_drvdata(dev: *mut LinuxCompatPciDev) -> usize {
    with_device(dev, |device| device.compat_dev.dev.driver_data as usize).unwrap_or(0)
}

fn ensure_enumerated() {
    if PCI_ENUMERATED.load(Ordering::Acquire) {
        return;
    }

    irq_safe(|| {
        if PCI_ENUMERATED.load(Ordering::Acquire) {
            return;
        }

        let mut devices = PCI_DEVICES.lock();
        crate::arch::pci::visit_devices(|address| {
            devices.push(snapshot_device(address));
            false
        });
        PCI_ENUMERATED.store(true, Ordering::Release);
    });
}

fn snapshot_device(address: crate::arch::pci::PciDevice) -> RegisteredPciDevice {
    let mut compat_dev = Box::<LinuxCompatPciDev>::default();
    compat_dev.bus = ptr::null_mut();
    compat_dev.subordinate = ptr::null_mut();
    compat_dev.sysdata = ptr::null_mut();
    compat_dev.procent = ptr::null_mut();
    compat_dev.slot = ptr::null_mut();
    compat_dev.devfn = address.devfn() as u32;
    compat_dev.vendor = address.vendor_id();
    compat_dev.device = address.device_id();
    compat_dev.subsystem_vendor = address.subsystem_vendor_id();
    compat_dev.subsystem_device = address.subsystem_device_id();
    compat_dev.class = address.class();
    compat_dev.revision = address.revision();
    compat_dev.hdr_type = address.header_type();
    compat_dev.rom_base_reg = rom_base_register_offset(address.header_type());
    compat_dev.pin = address.interrupt_pin();
    compat_dev.pcie_flags_reg = 0;
    compat_dev.dma_alias_mask = 0;
    compat_dev.driver = ptr::null_mut();
    compat_dev.dma_mask = u64::MAX;
    compat_dev.current_state = 0;
    compat_dev.dev.bus = bus_type_ptr();
    compat_dev.cfg_size = address.config_size();
    compat_dev.irq = match address.interrupt_line() {
        0xff => 0,
        line => line as u32,
    };
    populate_resources(&mut compat_dev, address);

    RegisteredPciDevice {
        address,
        compat_dev,
        bound_driver: None,
    }
}

fn populate_resources(compat_dev: &mut LinuxCompatPciDev, address: crate::arch::pci::PciDevice) {
    let mut bar = 0usize;
    while bar < address.standard_bar_count() && bar < DEVICE_RESOURCE_COUNT {
        let Some(resource) = address.resource(bar) else {
            bar += 1;
            continue;
        };

        compat_dev.resource[bar] = LinuxCompatResource {
            start: resource.start,
            end: resource
                .start
                .saturating_add(resource.size.saturating_sub(1)),
            name: ptr::null(),
            flags: pci_resource_flags(resource),
            desc: 0,
            parent: ptr::null_mut(),
            sibling: ptr::null_mut(),
            child: ptr::null_mut(),
        };

        bar += if resource.is_64bit { 2 } else { 1 };
    }
}

fn bind_driver_to_devices(driver_index: usize, driver: *mut LinuxCompatPciDriver) {
    let candidates = irq_safe(|| {
        let mut devices = PCI_DEVICES.lock();
        let id_table = unsafe { (*driver).id_table };
        let mut candidates = Vec::new();

        for device in devices.iter_mut() {
            if device.bound_driver.is_some() {
                continue;
            }
            let Some(id_ptr) = first_matching_id(id_table, device.compat_dev.as_ref()) else {
                continue;
            };

            apply_bound_driver(device.compat_dev.as_mut(), driver);
            device.bound_driver = Some(driver_index);
            candidates.push((device.compat_dev.as_mut() as *mut LinuxCompatPciDev, id_ptr));
        }

        candidates
    });

    let probe = unsafe { (*driver).probe };
    for (dev_ptr, id_ptr) in candidates {
        crate::network::set_current_linux_netdev_transport(
            crate::network::LinuxNetdevTransport::Pci,
        );
        let status = if let Some(probe) = probe {
            unsafe { probe(dev_ptr, id_ptr) }
        } else {
            0
        };
        crate::network::set_current_linux_netdev_transport(
            crate::network::LinuxNetdevTransport::Unknown,
        );

        if status == 0 {
            continue;
        }

        irq_safe(|| {
            let mut devices = PCI_DEVICES.lock();
            if let Some(device) = find_device_mut(&mut devices, dev_ptr) {
                clear_bound_driver(device.compat_dev.as_mut());
                device.bound_driver = None;
            }
        });
        let dev_struct = unsafe { &mut (*dev_ptr).dev as *mut _ as *mut c_void };
        crate::driver::devres::release_device(dev_struct);
    }
}

fn prepare_driver_struct(driver: *mut LinuxCompatPciDriver) {
    unsafe {
        if (*driver).driver.name.is_null() {
            (*driver).driver.name = (*driver).name;
        }
        (*driver).driver.bus = bus_type_ptr();
    }
}

fn apply_bound_driver(dev: &mut LinuxCompatPciDev, driver: *mut LinuxCompatPciDriver) {
    dev.driver = driver;
    dev.dev.driver = unsafe { &mut (*driver).driver };
}

fn clear_bound_driver(dev: &mut LinuxCompatPciDev) {
    dev.driver = ptr::null_mut();
    dev.dev.driver = ptr::null_mut();
}

fn resource_field<T>(
    dev: *mut LinuxCompatPciDev,
    bar: u32,
    f: impl FnOnce(&LinuxCompatResource) -> T,
) -> Option<T> {
    with_device(dev, |device| {
        let resource = device.compat_dev.resource.get(bar as usize)?;
        Some(f(resource))
    })
    .flatten()
}

fn with_device<T>(
    dev: *mut LinuxCompatPciDev,
    f: impl FnOnce(&RegisteredPciDevice) -> T,
) -> Option<T> {
    if dev.is_null() {
        return None;
    }

    irq_safe(|| {
        let devices = PCI_DEVICES.lock();
        let device = devices.iter().find(|device| {
            ptr::eq(
                device.compat_dev.as_ref() as *const LinuxCompatPciDev,
                dev as *const LinuxCompatPciDev,
            )
        })?;
        Some(f(device))
    })
}

fn with_device_mut<T>(
    dev: *mut LinuxCompatPciDev,
    f: impl FnOnce(&mut RegisteredPciDevice) -> T,
) -> Option<T> {
    if dev.is_null() {
        return None;
    }

    irq_safe(|| {
        let mut devices = PCI_DEVICES.lock();
        let device = find_device_mut(&mut devices, dev)?;
        Some(f(device))
    })
}

fn find_device_mut<'a>(
    devices: &'a mut [RegisteredPciDevice],
    dev: *mut LinuxCompatPciDev,
) -> Option<&'a mut RegisteredPciDevice> {
    devices.iter_mut().find(|device| {
        ptr::eq(
            device.compat_dev.as_ref() as *const LinuxCompatPciDev,
            dev as *const LinuxCompatPciDev,
        )
    })
}

fn first_matching_id(
    id_table: *const LinuxCompatPciDeviceId,
    dev: &LinuxCompatPciDev,
) -> Option<*const LinuxCompatPciDeviceId> {
    if id_table.is_null() {
        return None;
    }

    let mut index = 0usize;
    while index < 256 {
        let id = unsafe { *id_table.add(index) };
        if id.is_terminator() {
            return None;
        }
        if pci_id_matches(id, dev) {
            return Some(unsafe { id_table.add(index) });
        }
        index += 1;
    }

    None
}

fn pci_id_matches(id: LinuxCompatPciDeviceId, dev: &LinuxCompatPciDev) -> bool {
    pci_any_matches(id.vendor, dev.vendor as u32)
        && pci_any_matches(id.device, dev.device as u32)
        && pci_any_matches(id.subvendor, dev.subsystem_vendor as u32)
        && pci_any_matches(id.subdevice, dev.subsystem_device as u32)
        && class_matches(id, dev.class)
}

fn pci_any_matches(expected: u32, actual: u32) -> bool {
    expected == PCI_ANY_ID || expected == actual
}

fn class_matches(id: LinuxCompatPciDeviceId, class: u32) -> bool {
    if id.class_mask == 0 {
        return true;
    }
    ((class ^ id.class) & id.class_mask) == 0
}

fn pci_resource_flags(resource: crate::arch::pci::PciResource) -> usize {
    let mut flags = if resource.is_io {
        IORESOURCE_IO
    } else {
        IORESOURCE_MEM
    };

    if resource.prefetchable {
        flags |= IORESOURCE_PREFETCH;
    }
    if resource.is_64bit {
        flags |= IORESOURCE_MEM_64;
    }
    if resource.start == 0 {
        flags |= IORESOURCE_UNSET;
    }
    flags
}

fn rom_base_register_offset(header_type: u8) -> u8 {
    match header_type {
        0x01 => 0x38,
        _ => 0x30,
    }
}

fn read_config<T: Copy>(
    dev: *mut LinuxCompatPciDev,
    offset: i32,
    value: *mut T,
    read: impl FnOnce(crate::arch::pci::PciDevice, usize) -> Option<T>,
) -> i32 {
    if value.is_null() || offset < 0 {
        return -22;
    }

    let Some(result) = with_device(dev, |device| read(device.address, offset as usize)) else {
        return -19;
    };
    let Some(result) = result else {
        return -22;
    };

    unsafe {
        *value = result;
    }
    0
}

fn read_u8(address: crate::arch::pci::PciDevice, offset: usize) -> Option<u8> {
    if offset > u8::MAX as usize {
        let aligned = offset & !0x3;
        let shift = ((offset & 0x3) * 8) as u32;
        let value = read_u32(address, aligned)?;
        return Some(((value >> shift) & 0xff) as u8);
    }
    Some(address.read_u8(offset as u8))
}

fn read_u16(address: crate::arch::pci::PciDevice, offset: usize) -> Option<u16> {
    if offset > u8::MAX as usize {
        let aligned = offset & !0x3;
        let shift = ((offset & 0x2) * 8) as u32;
        let value = read_u32(address, aligned)?;
        return Some(((value >> shift) & 0xffff) as u16);
    }
    Some(address.read_u16(offset as u8))
}

fn read_u32(address: crate::arch::pci::PciDevice, offset: usize) -> Option<u32> {
    if offset > u8::MAX as usize {
        let addr = crate::arch::acpi::pci_config_address(
            address.segment,
            address.bus,
            address.device,
            address.function,
            offset,
        )?;
        return Some(unsafe { ptr::read_volatile(addr as *const u32) });
    }
    Some(address.read_u32(offset as u8))
}

fn write_u8(address: crate::arch::pci::PciDevice, offset: i32, value: u8) -> i32 {
    if offset < 0 {
        return -22;
    }

    let offset = offset as usize;
    if offset > u8::MAX as usize {
        let aligned = offset & !0x3;
        let shift = ((offset & 0x3) * 8) as u32;
        let Some(current) = read_u32(address, aligned) else {
            return -22;
        };
        let next = (current & !(0xff_u32 << shift)) | ((value as u32) << shift);
        return write_u32(address, aligned as i32, next);
    }

    address.write_u8(offset as u8, value);
    0
}

fn write_u16(address: crate::arch::pci::PciDevice, offset: i32, value: u16) -> i32 {
    if offset < 0 {
        return -22;
    }

    let offset = offset as usize;
    if offset > u8::MAX as usize {
        let aligned = offset & !0x3;
        let shift = ((offset & 0x2) * 8) as u32;
        let Some(current) = read_u32(address, aligned) else {
            return -22;
        };
        let next = (current & !(0xffff_u32 << shift)) | ((value as u32) << shift);
        return write_u32(address, aligned as i32, next);
    }

    address.write_u16(offset as u8, value);
    0
}

fn write_u32(address: crate::arch::pci::PciDevice, offset: i32, value: u32) -> i32 {
    if offset < 0 {
        return -22;
    }

    let offset = offset as usize;
    if offset > u8::MAX as usize {
        let Some(addr) = crate::arch::acpi::pci_config_address(
            address.segment,
            address.bus,
            address.device,
            address.function,
            offset,
        ) else {
            return -22;
        };
        unsafe {
            ptr::write_volatile(addr as *mut u32, value);
        }
        return 0;
    }

    address.write_u32(offset as u8, value);
    0
}

fn irq_safe<T>(f: impl FnOnce() -> T) -> T {
    interrupts::without_interrupts(f)
}
