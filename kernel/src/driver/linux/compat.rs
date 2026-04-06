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
pub(crate) struct LinuxCompatSemaphore {
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
    pub(crate) tail: [u8; 608],
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
            tail: [0; 608],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxCompatHidEmbeddedDevice {
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

impl Default for LinuxCompatHidEmbeddedDevice {
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
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxCompatResource {
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) name: *const c_char,
    pub(crate) flags: usize,
    pub(crate) desc: usize,
    pub(crate) parent: *mut LinuxCompatResource,
    pub(crate) sibling: *mut LinuxCompatResource,
    pub(crate) child: *mut LinuxCompatResource,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct LinuxCompatPciDeviceId {
    pub(crate) vendor: u32,
    pub(crate) device: u32,
    pub(crate) subvendor: u32,
    pub(crate) subdevice: u32,
    pub(crate) class: u32,
    pub(crate) class_mask: u32,
    pub(crate) driver_data: usize,
    pub(crate) override_only: u32,
}

impl LinuxCompatPciDeviceId {
    pub(crate) const fn is_terminator(self) -> bool {
        self.vendor == 0
            && self.device == 0
            && self.subvendor == 0
            && self.subdevice == 0
            && self.class == 0
            && self.class_mask == 0
            && self.driver_data == 0
            && self.override_only == 0
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxCompatPciDriver {
    pub(crate) name: *const c_char,
    pub(crate) id_table: *const LinuxCompatPciDeviceId,
    pub(crate) probe: Option<LinuxCompatPciProbeFn>,
    pub(crate) remove: Option<LinuxCompatPciRemoveFn>,
    pub(crate) suspend: *const c_void,
    pub(crate) resume: *const c_void,
    pub(crate) shutdown: *const c_void,
    pub(crate) sriov_configure: *const c_void,
    pub(crate) sriov_set_msix_vec_count: *const c_void,
    pub(crate) sriov_get_vf_total_msix: *const c_void,
    pub(crate) err_handler: *const c_void,
    pub(crate) groups: *const *const c_void,
    pub(crate) dev_groups: *const *const c_void,
    pub(crate) driver: LinuxCompatDeviceDriver,
    pub(crate) _pad0: [u8; 24],
    pub(crate) driver_managed_dma: bool,
    pub(crate) _pad1: [u8; 7],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxCompatPciDev {
    pub(crate) bus_list: [u8; 16],
    pub(crate) bus: *mut c_void,
    pub(crate) subordinate: *mut c_void,
    pub(crate) sysdata: *mut c_void,
    pub(crate) procent: *mut c_void,
    pub(crate) slot: *mut c_void,
    pub(crate) devfn: u32,
    pub(crate) vendor: u16,
    pub(crate) device: u16,
    pub(crate) subsystem_vendor: u16,
    pub(crate) subsystem_device: u16,
    pub(crate) class: u32,
    pub(crate) revision: u8,
    pub(crate) hdr_type: u8,
    pub(crate) _pad0: [u8; 40],
    pub(crate) rom_base_reg: u8,
    pub(crate) pin: u8,
    pub(crate) pcie_flags_reg: u16,
    pub(crate) dma_alias_mask: u64,
    pub(crate) driver: *mut LinuxCompatPciDriver,
    pub(crate) dma_mask: u64,
    pub(crate) dma_parms: [u8; 16],
    pub(crate) current_state: u32,
    pub(crate) _pad1: [u8; 36],
    pub(crate) dev: LinuxCompatDevice,
    pub(crate) _pad2: [u8; 0],
    pub(crate) cfg_size: i32,
    pub(crate) irq: u32,
    pub(crate) resource: [LinuxCompatResource; 17],
    pub(crate) tail: [u8; 632],
}

impl Default for LinuxCompatPciDev {
    fn default() -> Self {
        Self {
            bus_list: [0; 16],
            bus: core::ptr::null_mut(),
            subordinate: core::ptr::null_mut(),
            sysdata: core::ptr::null_mut(),
            procent: core::ptr::null_mut(),
            slot: core::ptr::null_mut(),
            devfn: 0,
            vendor: 0,
            device: 0,
            subsystem_vendor: 0,
            subsystem_device: 0,
            class: 0,
            revision: 0,
            hdr_type: 0,
            _pad0: [0; 40],
            rom_base_reg: 0,
            pin: 0,
            pcie_flags_reg: 0,
            dma_alias_mask: 0,
            driver: core::ptr::null_mut(),
            dma_mask: 0,
            dma_parms: [0; 16],
            current_state: 0,
            _pad1: [0; 36],
            dev: LinuxCompatDevice::default(),
            _pad2: [0; 0],
            cfg_size: 0,
            irq: 0,
            resource: [LinuxCompatResource::default(); 17],
            tail: [0; 632],
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
pub(crate) type LinuxCompatPciProbeFn =
    unsafe extern "C" fn(dev: *mut LinuxCompatPciDev, id: *const LinuxCompatPciDeviceId) -> i32;
pub(crate) type LinuxCompatPciRemoveFn = unsafe extern "C" fn(dev: *mut LinuxCompatPciDev);

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
                tail: [0; 608],
            },
            node: LinuxCompatListHead {
                next: core::ptr::null_mut(),
                prev: core::ptr::null_mut(),
            },
            ps2_cmd_mutex: core::ptr::null_mut(),
        }
    }

    // Resolver/export surface used by Linux-compat modules without static Rust call sites.
    #[allow(dead_code)]
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

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct LinuxCompatUsbDeviceId {
    pub(crate) match_flags: u16,
    pub(crate) id_vendor: u16,
    pub(crate) id_product: u16,
    pub(crate) bcd_device_lo: u16,
    pub(crate) bcd_device_hi: u16,
    pub(crate) b_device_class: u8,
    pub(crate) b_device_sub_class: u8,
    pub(crate) b_device_protocol: u8,
    pub(crate) b_interface_class: u8,
    pub(crate) b_interface_sub_class: u8,
    pub(crate) b_interface_protocol: u8,
    pub(crate) b_interface_number: u8,
    pub(crate) driver_info: usize,
}

impl LinuxCompatUsbDeviceId {
    pub(crate) const fn is_terminator(self) -> bool {
        self.match_flags == 0
            && self.id_vendor == 0
            && self.id_product == 0
            && self.bcd_device_lo == 0
            && self.bcd_device_hi == 0
            && self.b_device_class == 0
            && self.b_device_sub_class == 0
            && self.b_device_protocol == 0
            && self.b_interface_class == 0
            && self.b_interface_sub_class == 0
            && self.b_interface_protocol == 0
            && self.b_interface_number == 0
            && self.driver_info == 0
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxCompatUsbDynids {
    pub(crate) list: LinuxCompatListHead,
}

pub(crate) type LinuxCompatUsbProbeFn = unsafe extern "C" fn(
    intf: *mut LinuxCompatUsbInterface,
    id: *const LinuxCompatUsbDeviceId,
) -> i32;
pub(crate) type LinuxCompatUsbDisconnectFn =
    unsafe extern "C" fn(intf: *mut LinuxCompatUsbInterface);
pub(crate) type LinuxCompatUsbIoctlFn =
    unsafe extern "C" fn(intf: *mut LinuxCompatUsbInterface, code: u32, buf: *mut c_void) -> i32;
pub(crate) type LinuxCompatUsbSuspendFn =
    unsafe extern "C" fn(intf: *mut LinuxCompatUsbInterface, message: u32) -> i32;
pub(crate) type LinuxCompatUsbResumeFn =
    unsafe extern "C" fn(intf: *mut LinuxCompatUsbInterface) -> i32;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxCompatUsbDriver {
    pub(crate) name: *const c_char,
    pub(crate) probe: Option<LinuxCompatUsbProbeFn>,
    pub(crate) disconnect: Option<LinuxCompatUsbDisconnectFn>,
    pub(crate) unlocked_ioctl: Option<LinuxCompatUsbIoctlFn>,
    pub(crate) suspend: Option<LinuxCompatUsbSuspendFn>,
    pub(crate) resume: Option<LinuxCompatUsbResumeFn>,
    pub(crate) reset_resume: Option<LinuxCompatUsbResumeFn>,
    pub(crate) pre_reset: Option<LinuxCompatUsbResumeFn>,
    pub(crate) post_reset: Option<LinuxCompatUsbResumeFn>,
    pub(crate) shutdown: Option<LinuxCompatUsbDisconnectFn>,
    pub(crate) id_table: *const LinuxCompatUsbDeviceId,
    pub(crate) dev_groups: *const *const c_void,
    pub(crate) dynids: LinuxCompatUsbDynids,
    pub(crate) driver: LinuxCompatDeviceDriver,
    pub(crate) no_dynamic_id: u8,
    pub(crate) supports_autosuspend: u8,
    pub(crate) disable_hub_initiated_lpm: u8,
    pub(crate) soft_unbind: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxCompatUsbHostEndpoint {
    pub(crate) desc: [u8; 9],
    pub(crate) ss_ep_comp: [u8; 6],
    pub(crate) ssp_isoc_ep_comp: [u8; 8],
    pub(crate) eusb2_isoc_ep_comp: [u8; 8],
    pub(crate) _pad0: u8,
    pub(crate) urb_list: LinuxCompatListHead,
    pub(crate) hcpriv: *mut c_void,
    pub(crate) ep_dev: *mut c_void,
    pub(crate) extra: *mut u8,
    pub(crate) extralen: i32,
    pub(crate) enabled: i32,
    pub(crate) streams: i32,
    pub(crate) _pad1: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxCompatUsbHostInterface {
    pub(crate) desc: [u8; 9],
    pub(crate) _pad0: [u8; 3],
    pub(crate) extralen: i32,
    pub(crate) extra: *mut u8,
    pub(crate) endpoint: *mut LinuxCompatUsbHostEndpoint,
    pub(crate) string: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxCompatUsbBus {
    pub(crate) _pad0: [u8; 24],
    pub(crate) bus_name: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxCompatUsbInterface {
    pub(crate) altsetting: *mut LinuxCompatUsbHostInterface,
    pub(crate) cur_altsetting: *mut LinuxCompatUsbHostInterface,
    pub(crate) num_altsetting: u32,
    pub(crate) intf_assoc: *mut c_void,
    pub(crate) minor: i32,
    pub(crate) condition: u32,
    pub(crate) flags_bits: u32,
    pub(crate) wireless_status: u32,
    pub(crate) wireless_status_work: [u8; 32],
    pub(crate) dev: LinuxCompatDevice,
    pub(crate) usb_dev: *mut LinuxCompatUsbDevice,
    pub(crate) reset_ws: [u8; 64],
}

impl Default for LinuxCompatUsbInterface {
    fn default() -> Self {
        Self {
            altsetting: core::ptr::null_mut(),
            cur_altsetting: core::ptr::null_mut(),
            num_altsetting: 0,
            intf_assoc: core::ptr::null_mut(),
            minor: -1,
            condition: 0,
            flags_bits: 1 << 7,
            wireless_status: 0,
            wireless_status_work: [0; 32],
            dev: LinuxCompatDevice::default(),
            usb_dev: core::ptr::null_mut(),
            reset_ws: [0; 64],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxCompatUsbDevice {
    pub(crate) devnum: i32,
    pub(crate) devpath: [u8; 16],
    pub(crate) route: u32,
    pub(crate) state: u32,
    pub(crate) speed: u32,
    pub(crate) rx_lanes: u32,
    pub(crate) tx_lanes: u32,
    pub(crate) ssp_rate: u32,
    pub(crate) tt: *mut c_void,
    pub(crate) ttport: i32,
    pub(crate) toggle: [u32; 2],
    pub(crate) _pad0: u32,
    pub(crate) parent: *mut LinuxCompatUsbDevice,
    pub(crate) bus: *mut c_void,
    pub(crate) ep0: LinuxCompatUsbHostEndpoint,
    pub(crate) dev: LinuxCompatDevice,
    pub(crate) _pad_after_dev: [u8; 16],
    pub(crate) descriptor: [u8; 24],
    pub(crate) bos: *mut c_void,
    pub(crate) config: *mut c_void,
    pub(crate) actconfig: *mut c_void,
    pub(crate) ep_in: [*mut LinuxCompatUsbHostEndpoint; 16],
    pub(crate) ep_out: [*mut LinuxCompatUsbHostEndpoint; 16],
    pub(crate) rawdescriptors: *mut *mut c_char,
    pub(crate) bus_ma: u16,
    pub(crate) portnum: u8,
    pub(crate) level: u8,
    pub(crate) devaddr: u8,
    pub(crate) _pad1: [u8; 3],
    pub(crate) _pad_strings: [u8; 8],
    pub(crate) product: *const c_char,
    pub(crate) manufacturer: *const c_char,
    pub(crate) serial: *const c_char,
    pub(crate) flags_bits: u32,
    pub(crate) string_langid: i32,
    pub(crate) _pad2: [u8; 4],
    pub(crate) _tail: [u8; 64],
}

impl Default for LinuxCompatUsbDevice {
    fn default() -> Self {
        Self {
            devnum: 0,
            devpath: [0; 16],
            route: 0,
            state: 0,
            speed: 0,
            rx_lanes: 0,
            tx_lanes: 0,
            ssp_rate: 0,
            tt: core::ptr::null_mut(),
            ttport: 0,
            toggle: [0; 2],
            _pad0: 0,
            parent: core::ptr::null_mut(),
            bus: core::ptr::null_mut(),
            ep0: LinuxCompatUsbHostEndpoint::default(),
            dev: LinuxCompatDevice::default(),
            _pad_after_dev: [0; 16],
            descriptor: [0; 24],
            bos: core::ptr::null_mut(),
            config: core::ptr::null_mut(),
            actconfig: core::ptr::null_mut(),
            ep_in: [core::ptr::null_mut(); 16],
            ep_out: [core::ptr::null_mut(); 16],
            rawdescriptors: core::ptr::null_mut(),
            bus_ma: 0,
            portnum: 0,
            level: 0,
            devaddr: 0,
            _pad1: [0; 3],
            _pad_strings: [0; 8],
            product: core::ptr::null(),
            manufacturer: core::ptr::null(),
            serial: core::ptr::null(),
            flags_bits: 0,
            string_langid: 0,
            _pad2: [0; 4],
            _tail: [0; 64],
        }
    }
}

pub(crate) type LinuxCompatUsbCompleteFn = unsafe extern "C" fn(urb: *mut LinuxCompatUrb);

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxCompatUrb {
    pub(crate) kref: [u8; 4],
    pub(crate) unlinked: i32,
    pub(crate) hcpriv: *mut c_void,
    pub(crate) use_count: i32,
    pub(crate) reject: i32,
    pub(crate) urb_list: LinuxCompatListHead,
    pub(crate) anchor_list: LinuxCompatListHead,
    pub(crate) anchor: *mut c_void,
    pub(crate) dev: *mut LinuxCompatUsbDevice,
    pub(crate) ep: *mut c_void,
    pub(crate) pipe: u32,
    pub(crate) stream_id: u32,
    pub(crate) status: i32,
    pub(crate) transfer_flags: u32,
    pub(crate) transfer_buffer: *mut c_void,
    pub(crate) transfer_dma: u64,
    pub(crate) sg: *mut c_void,
    pub(crate) sgt: *mut c_void,
    pub(crate) num_mapped_sgs: i32,
    pub(crate) num_sgs: i32,
    pub(crate) transfer_buffer_length: u32,
    pub(crate) actual_length: u32,
    pub(crate) setup_packet: *mut u8,
    pub(crate) setup_dma: u64,
    pub(crate) start_frame: i32,
    pub(crate) number_of_packets: i32,
    pub(crate) interval: i32,
    pub(crate) error_count: i32,
    pub(crate) context: *mut c_void,
    pub(crate) complete: Option<LinuxCompatUsbCompleteFn>,
}

impl Default for LinuxCompatUrb {
    fn default() -> Self {
        Self {
            kref: [0; 4],
            unlinked: 0,
            hcpriv: core::ptr::null_mut(),
            use_count: 0,
            reject: 0,
            urb_list: LinuxCompatListHead::default(),
            anchor_list: LinuxCompatListHead::default(),
            anchor: core::ptr::null_mut(),
            dev: core::ptr::null_mut(),
            ep: core::ptr::null_mut(),
            pipe: 0,
            stream_id: 0,
            status: 0,
            transfer_flags: 0,
            transfer_buffer: core::ptr::null_mut(),
            transfer_dma: 0,
            sg: core::ptr::null_mut(),
            sgt: core::ptr::null_mut(),
            num_mapped_sgs: 0,
            num_sgs: 0,
            transfer_buffer_length: 0,
            actual_length: 0,
            setup_packet: core::ptr::null_mut(),
            setup_dma: 0,
            start_frame: 0,
            number_of_packets: 0,
            interval: 0,
            error_count: 0,
            context: core::ptr::null_mut(),
            complete: None,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxCompatUsbClassDriver {
    pub(crate) name: *mut c_char,
    pub(crate) devnode: *const c_void,
    pub(crate) fops: *const c_void,
    pub(crate) minor_base: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct LinuxCompatHidDeviceId {
    pub(crate) bus: u16,
    pub(crate) group: u16,
    pub(crate) vendor: u32,
    pub(crate) product: u32,
    pub(crate) driver_data: usize,
}

impl LinuxCompatHidDeviceId {
    pub(crate) const fn is_terminator(self) -> bool {
        self.bus == 0
            && self.group == 0
            && self.vendor == 0
            && self.product == 0
            && self.driver_data == 0
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxCompatHidUsage {
    pub(crate) hid: u32,
    pub(crate) collection_index: u32,
    pub(crate) usage_index: u32,
    pub(crate) resolution_multiplier: i8,
    pub(crate) wheel_factor: i8,
    pub(crate) code: u16,
    pub(crate) type_: u8,
    pub(crate) _pad0: u8,
    pub(crate) hat_min: i16,
    pub(crate) hat_max: i16,
    pub(crate) hat_dir: i16,
    pub(crate) wheel_accumulated: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxCompatHidReportEnum {
    pub(crate) numbered: u32,
    pub(crate) _pad0: [u8; 4],
    pub(crate) report_list: LinuxCompatListHead,
    pub(crate) report_id_hash: [*mut LinuxCompatHidReport; 256],
}

impl Default for LinuxCompatHidReportEnum {
    fn default() -> Self {
        Self {
            numbered: 0,
            _pad0: [0; 4],
            report_list: LinuxCompatListHead::default(),
            report_id_hash: [core::ptr::null_mut(); 256],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxCompatHidReport {
    pub(crate) list: LinuxCompatListHead,
    pub(crate) hidinput_list: LinuxCompatListHead,
    pub(crate) field_entry_list: LinuxCompatListHead,
    pub(crate) id: u32,
    pub(crate) type_: u32,
    pub(crate) application: u32,
    pub(crate) _pad0: u32,
    pub(crate) field: [*mut LinuxCompatHidField; 256],
    pub(crate) field_entries: *mut LinuxCompatHidFieldEntry,
    pub(crate) maxfield: u32,
    pub(crate) size: u32,
    pub(crate) device: *mut LinuxCompatHidDevice,
    pub(crate) tool_active: bool,
    pub(crate) _pad1: [u8; 3],
    pub(crate) tool: u32,
}

impl Default for LinuxCompatHidReport {
    fn default() -> Self {
        Self {
            list: LinuxCompatListHead::default(),
            hidinput_list: LinuxCompatListHead::default(),
            field_entry_list: LinuxCompatListHead::default(),
            id: 0,
            type_: 0,
            application: 0,
            _pad0: 0,
            field: [core::ptr::null_mut(); 256],
            field_entries: core::ptr::null_mut(),
            maxfield: 0,
            size: 0,
            device: core::ptr::null_mut(),
            tool_active: false,
            _pad1: [0; 3],
            tool: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxCompatHidInput {
    pub(crate) list: LinuxCompatListHead,
    pub(crate) report: *mut LinuxCompatHidReport,
    pub(crate) input: *mut LinuxCompatInputDev,
    pub(crate) name: *const c_char,
    pub(crate) reports: LinuxCompatListHead,
    pub(crate) application: u32,
    pub(crate) registered: bool,
    pub(crate) _pad0: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxCompatHidFieldEntry {
    pub(crate) list: LinuxCompatListHead,
    pub(crate) field: *mut LinuxCompatHidField,
    pub(crate) index: u32,
    pub(crate) priority: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxCompatHidField {
    pub(crate) physical: u32,
    pub(crate) logical: u32,
    pub(crate) application: u32,
    pub(crate) usage: *mut LinuxCompatHidUsage,
    pub(crate) maxusage: u32,
    pub(crate) flags: u32,
    pub(crate) report_offset: u32,
    pub(crate) report_size: u32,
    pub(crate) report_count: u32,
    pub(crate) report_type: u32,
    pub(crate) value: *mut i32,
    pub(crate) new_value: *mut i32,
    pub(crate) usages_priorities: *mut i32,
    pub(crate) logical_minimum: i32,
    pub(crate) logical_maximum: i32,
    pub(crate) physical_minimum: i32,
    pub(crate) physical_maximum: i32,
    pub(crate) unit_exponent: i32,
    pub(crate) unit: u32,
    pub(crate) ignored: bool,
    pub(crate) _pad0: [u8; 7],
    pub(crate) report: *mut LinuxCompatHidReport,
    pub(crate) index: u32,
    pub(crate) _pad1: [u8; 4],
    pub(crate) hidinput: *mut LinuxCompatHidInput,
    pub(crate) dpad: u16,
    pub(crate) _pad2: [u8; 2],
    pub(crate) slot_idx: u32,
}

pub(crate) type LinuxCompatHidMatchFn =
    unsafe extern "C" fn(dev: *mut LinuxCompatHidDevice, ignore_special_driver: bool) -> bool;
pub(crate) type LinuxCompatHidProbeFn =
    unsafe extern "C" fn(dev: *mut LinuxCompatHidDevice, id: *const LinuxCompatHidDeviceId) -> i32;
pub(crate) type LinuxCompatHidRemoveFn = unsafe extern "C" fn(dev: *mut LinuxCompatHidDevice);
pub(crate) type LinuxCompatHidSuspendFn =
    unsafe extern "C" fn(dev: *mut LinuxCompatHidDevice, message: u32) -> i32;
pub(crate) type LinuxCompatHidResumeFn =
    unsafe extern "C" fn(dev: *mut LinuxCompatHidDevice) -> i32;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxCompatHidDriver {
    pub(crate) name: *mut c_char,
    pub(crate) id_table: *const LinuxCompatHidDeviceId,
    pub(crate) dyn_list: LinuxCompatListHead,
    pub(crate) dyn_lock: u32,
    pub(crate) _pad0: [u8; 4],
    pub(crate) match_: Option<LinuxCompatHidMatchFn>,
    pub(crate) probe: Option<LinuxCompatHidProbeFn>,
    pub(crate) remove: Option<LinuxCompatHidRemoveFn>,
    pub(crate) report_table: *const c_void,
    pub(crate) raw_event: *const c_void,
    pub(crate) usage_table: *const c_void,
    pub(crate) event: *const c_void,
    pub(crate) report: *const c_void,
    pub(crate) report_fixup: *const c_void,
    pub(crate) input_mapping: *const c_void,
    pub(crate) input_mapped: *const c_void,
    pub(crate) input_configured: *const c_void,
    pub(crate) feature_mapping: *const c_void,
    pub(crate) suspend: Option<LinuxCompatHidSuspendFn>,
    pub(crate) resume: Option<LinuxCompatHidResumeFn>,
    pub(crate) reset_resume: Option<LinuxCompatHidResumeFn>,
    pub(crate) on_hid_hw_open: Option<LinuxCompatHidRemoveFn>,
    pub(crate) on_hid_hw_close: Option<LinuxCompatHidRemoveFn>,
    pub(crate) driver: LinuxCompatDeviceDriver,
}

pub(crate) type LinuxCompatHidLlStartFn =
    unsafe extern "C" fn(hdev: *mut LinuxCompatHidDevice) -> i32;
pub(crate) type LinuxCompatHidLlStopFn = unsafe extern "C" fn(hdev: *mut LinuxCompatHidDevice);
pub(crate) type LinuxCompatHidLlRequestFn = unsafe extern "C" fn(
    hdev: *mut LinuxCompatHidDevice,
    report: *mut LinuxCompatHidReport,
    reqtype: i32,
);
pub(crate) type LinuxCompatHidLlOutputReportFn =
    unsafe extern "C" fn(hdev: *mut LinuxCompatHidDevice, buf: *mut u8, len: usize) -> i32;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxCompatHidLlDriver {
    pub(crate) start: Option<LinuxCompatHidLlStartFn>,
    pub(crate) stop: Option<LinuxCompatHidLlStopFn>,
    pub(crate) open: Option<LinuxCompatHidLlStartFn>,
    pub(crate) close: Option<LinuxCompatHidLlStopFn>,
    pub(crate) power: *const c_void,
    pub(crate) parse: Option<LinuxCompatHidLlStartFn>,
    pub(crate) request: Option<LinuxCompatHidLlRequestFn>,
    pub(crate) wait: Option<LinuxCompatHidLlStartFn>,
    pub(crate) raw_request: *const c_void,
    pub(crate) output_report: Option<LinuxCompatHidLlOutputReportFn>,
    pub(crate) idle: *const c_void,
    pub(crate) may_wakeup: *const c_void,
    pub(crate) max_buffer_size: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxCompatHidDevice {
    pub(crate) dev_rdesc: *const u8,
    pub(crate) bpf_rdesc: *const u8,
    pub(crate) rdesc: *const u8,
    pub(crate) dev_rsize: u32,
    pub(crate) bpf_rsize: u32,
    pub(crate) rsize: u32,
    pub(crate) collection_size: u32,
    pub(crate) collection: *mut c_void,
    pub(crate) maxcollection: u32,
    pub(crate) maxapplication: u32,
    pub(crate) bus: u16,
    pub(crate) group: u16,
    pub(crate) vendor: u32,
    pub(crate) product: u32,
    pub(crate) version: u32,
    pub(crate) type_: u32,
    pub(crate) country: u32,
    pub(crate) report_enum: [LinuxCompatHidReportEnum; 3],
    pub(crate) led_work: [u8; 32],
    pub(crate) driver_input_lock: LinuxCompatSemaphore,
    pub(crate) dev: LinuxCompatHidEmbeddedDevice,
    pub(crate) driver: *mut LinuxCompatHidDriver,
    pub(crate) devres_group_id: *mut c_void,
    pub(crate) ll_driver: *const LinuxCompatHidLlDriver,
    pub(crate) ll_open_lock: LinuxCompatMutex,
    pub(crate) ll_open_count: u32,
    pub(crate) _pad0: [u8; 4],
    pub(crate) battery: *mut c_void,
    pub(crate) battery_capacity: i32,
    pub(crate) battery_min: i32,
    pub(crate) battery_max: i32,
    pub(crate) battery_report_type: i32,
    pub(crate) battery_report_id: i32,
    pub(crate) battery_charge_status: i32,
    pub(crate) battery_status: u32,
    pub(crate) battery_avoid_query: bool,
    pub(crate) _pad1: [u8; 3],
    pub(crate) battery_ratelimit_time: i64,
    pub(crate) status: usize,
    pub(crate) claimed: u32,
    pub(crate) quirks: u32,
    pub(crate) initial_quirks: u32,
    pub(crate) io_started: bool,
    pub(crate) _pad2: [u8; 3],
    pub(crate) inputs: LinuxCompatListHead,
    pub(crate) hiddev: *mut c_void,
    pub(crate) hidraw: *mut c_void,
    pub(crate) name: [u8; 128],
    pub(crate) phys: [u8; 64],
    pub(crate) uniq: [u8; 64],
    pub(crate) driver_data: *mut c_void,
    pub(crate) ff_init: *const c_void,
    pub(crate) hiddev_connect: *const c_void,
    pub(crate) hiddev_disconnect: *const c_void,
    pub(crate) hiddev_hid_event: *const c_void,
    pub(crate) hiddev_report_event: *const c_void,
    pub(crate) tail: [u8; 184],
}

impl Default for LinuxCompatHidDevice {
    fn default() -> Self {
        Self {
            dev_rdesc: core::ptr::null(),
            bpf_rdesc: core::ptr::null(),
            rdesc: core::ptr::null(),
            dev_rsize: 0,
            bpf_rsize: 0,
            rsize: 0,
            collection_size: 0,
            collection: core::ptr::null_mut(),
            maxcollection: 0,
            maxapplication: 0,
            bus: 0,
            group: 0,
            vendor: 0,
            product: 0,
            version: 0,
            type_: 0,
            country: 0,
            report_enum: [LinuxCompatHidReportEnum::default(); 3],
            led_work: [0; 32],
            driver_input_lock: LinuxCompatSemaphore::default(),
            dev: LinuxCompatHidEmbeddedDevice::default(),
            driver: core::ptr::null_mut(),
            devres_group_id: core::ptr::null_mut(),
            ll_driver: core::ptr::null(),
            ll_open_lock: LinuxCompatMutex::default(),
            ll_open_count: 0,
            _pad0: [0; 4],
            battery: core::ptr::null_mut(),
            battery_capacity: 0,
            battery_min: 0,
            battery_max: 0,
            battery_report_type: 0,
            battery_report_id: 0,
            battery_charge_status: 0,
            battery_status: 0,
            battery_avoid_query: false,
            _pad1: [0; 3],
            battery_ratelimit_time: 0,
            status: 0,
            claimed: 0,
            quirks: 0,
            initial_quirks: 0,
            io_started: false,
            _pad2: [0; 3],
            inputs: LinuxCompatListHead::default(),
            hiddev: core::ptr::null_mut(),
            hidraw: core::ptr::null_mut(),
            name: [0; 128],
            phys: [0; 64],
            uniq: [0; 64],
            driver_data: core::ptr::null_mut(),
            ff_init: core::ptr::null(),
            hiddev_connect: core::ptr::null(),
            hiddev_disconnect: core::ptr::null(),
            hiddev_hid_event: core::ptr::null(),
            hiddev_report_event: core::ptr::null(),
            tail: [0; 184],
        }
    }
}

unsafe impl Send for LinuxCompatSerio {}
unsafe impl Send for LinuxCompatPs2Dev {}
unsafe impl Send for LinuxCompatPciDev {}
unsafe impl Send for LinuxCompatUsbInterface {}
unsafe impl Send for LinuxCompatUsbDevice {}
unsafe impl Send for LinuxCompatUrb {}
unsafe impl Send for LinuxCompatHidDevice {}
unsafe impl Send for LinuxCompatUsbDriver {}
unsafe impl Send for LinuxCompatHidDriver {}
unsafe impl Sync for LinuxCompatUsbBus {}

const _: [(); 64] = [(); core::mem::size_of::<LinuxCompatResource>()];
const _: [(); 40] = [(); core::mem::size_of::<LinuxCompatPciDeviceId>()];
const _: [(); 144] = [(); core::mem::size_of::<LinuxCompatDeviceDriver>()];
const _: [(); 768] = [(); core::mem::size_of::<LinuxCompatDevice>()];
const _: [(); 784] = [(); core::mem::size_of::<LinuxCompatHidEmbeddedDevice>()];
const _: [(); 280] = [(); core::mem::size_of::<LinuxCompatPciDriver>()];
const _: [(); 2696] = [(); core::mem::size_of::<LinuxCompatPciDev>()];
const _: [(); 1400] = [(); core::mem::size_of::<LinuxCompatInputDev>()];
const _: [(); 224] = [(); core::mem::size_of::<LinuxCompatSerioDriver>()];
const _: [(); 1136] = [(); core::mem::size_of::<LinuxCompatSerio>()];
const _: [(); 104] = [(); core::mem::size_of::<LinuxCompatPs2Dev>()];
const _: [(); 88] = [(); core::mem::size_of::<LinuxCompatUsbHostEndpoint>()];
const _: [(); 40] = [(); core::mem::size_of::<LinuxCompatUsbHostInterface>()];
const _: [(); 0x40] = [(); core::mem::offset_of!(LinuxCompatUrb, dev)];
const _: [(); 0x48] = [(); core::mem::offset_of!(LinuxCompatUrb, ep)];
const _: [(); 0x50] = [(); core::mem::offset_of!(LinuxCompatUrb, pipe)];
const _: [(); 0x60] = [(); core::mem::offset_of!(LinuxCompatUrb, transfer_buffer)];
const _: [(); 0x68] = [(); core::mem::offset_of!(LinuxCompatUrb, transfer_dma)];
const _: [(); 0x70] = [(); core::mem::offset_of!(LinuxCompatUrb, sg)];
const _: [(); 0x78] = [(); core::mem::offset_of!(LinuxCompatUrb, sgt)];
const _: [(); 0x80] = [(); core::mem::offset_of!(LinuxCompatUrb, num_mapped_sgs)];
const _: [(); 0x84] = [(); core::mem::offset_of!(LinuxCompatUrb, num_sgs)];
const _: [(); 0x88] = [(); core::mem::offset_of!(LinuxCompatUrb, transfer_buffer_length)];
const _: [(); 0x8c] = [(); core::mem::offset_of!(LinuxCompatUrb, actual_length)];
const _: [(); 0x90] = [(); core::mem::offset_of!(LinuxCompatUrb, setup_packet)];
const _: [(); 0xb0] = [(); core::mem::offset_of!(LinuxCompatUrb, context)];
const _: [(); 0xb8] = [(); core::mem::offset_of!(LinuxCompatUrb, complete)];
const _: [(); 0xc0] = [(); core::mem::size_of::<LinuxCompatUrb>()];
const _: [(); 0x18] = [(); core::mem::offset_of!(LinuxCompatUsbBus, bus_name)];
const _: [(); 0x50] = [(); core::mem::offset_of!(LinuxCompatUsbInterface, dev)];
const _: [(); 0x90] = [(); core::mem::offset_of!(LinuxCompatUsbInterface, dev)
    + core::mem::offset_of!(LinuxCompatDevice, parent)];
const _: [(); 0xb0] = [(); core::mem::offset_of!(LinuxCompatUsbDevice, dev)];
const _: [(); 0x3c0] = [(); core::mem::offset_of!(LinuxCompatUsbDevice, descriptor)];
const _: [(); 0x508] = [(); core::mem::offset_of!(LinuxCompatUsbDevice, product)];
const _: [(); 0x510] = [(); core::mem::offset_of!(LinuxCompatUsbDevice, manufacturer)];
const _: [(); 0x1c] = [(); core::mem::size_of::<LinuxCompatHidUsage>()];
const _: [(); 0x40] = [(); core::mem::size_of::<LinuxCompatHidInput>()];
const _: [(); 0x20] = [(); core::mem::size_of::<LinuxCompatHidFieldEntry>()];
const _: [(); 0x88] = [(); core::mem::size_of::<LinuxCompatHidField>()];
const _: [(); 0x860] = [(); core::mem::size_of::<LinuxCompatHidReport>()];
const _: [(); 0x818] = [(); core::mem::size_of::<LinuxCompatHidReportEnum>()];
const _: [(); 0x18b8] = [(); core::mem::offset_of!(LinuxCompatHidDevice, driver_input_lock)];
const _: [(); 0x18d8] = [(); core::mem::offset_of!(LinuxCompatHidDevice, dev)];
const _: [(); 0x1be8] = [(); core::mem::offset_of!(LinuxCompatHidDevice, driver)];
const _: [(); 0x1bf0] = [(); core::mem::offset_of!(LinuxCompatHidDevice, devres_group_id)];
const _: [(); 0x1bf8] = [(); core::mem::offset_of!(LinuxCompatHidDevice, ll_driver)];
const _: [(); 0x1c00] = [(); core::mem::offset_of!(LinuxCompatHidDevice, ll_open_lock)];
const _: [(); 0x1c20] = [(); core::mem::offset_of!(LinuxCompatHidDevice, ll_open_count)];
const _: [(); 0x1c58] = [(); core::mem::offset_of!(LinuxCompatHidDevice, status)];
const _: [(); 0x1c60] = [(); core::mem::offset_of!(LinuxCompatHidDevice, claimed)];
const _: [(); 0x1c64] = [(); core::mem::offset_of!(LinuxCompatHidDevice, quirks)];
const _: [(); 0x1c68] = [(); core::mem::offset_of!(LinuxCompatHidDevice, initial_quirks)];
const _: [(); 0x1c6c] = [(); core::mem::offset_of!(LinuxCompatHidDevice, io_started)];
const _: [(); 0x1c70] = [(); core::mem::offset_of!(LinuxCompatHidDevice, inputs)];
const _: [(); 0x1c80] = [(); core::mem::offset_of!(LinuxCompatHidDevice, hiddev)];
const _: [(); 0x1c88] = [(); core::mem::offset_of!(LinuxCompatHidDevice, hidraw)];
const _: [(); 0x1c90] = [(); core::mem::offset_of!(LinuxCompatHidDevice, name)];
const _: [(); 0x1d10] = [(); core::mem::offset_of!(LinuxCompatHidDevice, phys)];
const _: [(); 0x1d50] = [(); core::mem::offset_of!(LinuxCompatHidDevice, uniq)];
const _: [(); 0x1d90] = [(); core::mem::offset_of!(LinuxCompatHidDevice, driver_data)];
const _: [(); 0x1d98] = [(); core::mem::offset_of!(LinuxCompatHidDevice, ff_init)];
const _: [(); 0x1da0] = [(); core::mem::offset_of!(LinuxCompatHidDevice, hiddev_connect)];
const _: [(); 0x1da8] = [(); core::mem::offset_of!(LinuxCompatHidDevice, hiddev_disconnect)];
const _: [(); 0x1db0] = [(); core::mem::offset_of!(LinuxCompatHidDevice, hiddev_hid_event)];
const _: [(); 0x1db8] = [(); core::mem::offset_of!(LinuxCompatHidDevice, hiddev_report_event)];
const _: [(); 7800] = [(); core::mem::size_of::<LinuxCompatHidDevice>()];

pub(crate) fn compat_cstr(ptr: *const c_char) -> Option<&'static str> {
    if ptr.is_null() {
        return None;
    }

    // Some imported Linux modules hand us low-half pointers into their own image.
    // On opt-level=0, core's CStr/UTF-8 helpers have been observed to hit aligned SIMD
    // stores on this path, so keep the conversion byte-wise and ASCII-only here.
    let bytes = ptr as *const u8;
    let mut len = 0usize;
    while len < 256 {
        let byte = unsafe { bytes.add(len).read_volatile() };
        if byte == 0 {
            let slice = unsafe { core::slice::from_raw_parts(bytes, len) };
            if !slice.is_ascii() {
                return None;
            }
            return Some(unsafe { core::str::from_utf8_unchecked(slice) });
        }
        len += 1;
    }

    None
}

pub(crate) fn serio_any_matches(expected: u8, actual: u8) -> bool {
    expected == SERIO_ANY as u8 || expected == actual
}
