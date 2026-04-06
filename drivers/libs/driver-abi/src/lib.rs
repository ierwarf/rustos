#![no_std]

use core::ffi::{c_char, c_void};
use core::str;

pub const DRIVER_MODULE_ABI_VERSION: u32 = 4;
pub const DRIVER_NAME_MAX: usize = 32;
pub const DRIVER_PATH_MAX: usize = 128;
pub const RUSTOS_DRIVER_HEADER_SYMBOL: &str = "RUSTOS_DRIVER_HEADER";
pub const RUSTOS_DRIVER_ABI_VERSION_SYMBOL: &str = "rustos_driver_abi_version";
pub const RUSTOS_DRIVER_INIT_SYMBOL: &str = "rustos_driver_init";
pub const POINTER_BUTTON_LEFT: u8 = 1 << 0;
pub const POINTER_BUTTON_RIGHT: u8 = 1 << 1;
pub const POINTER_BUTTON_MIDDLE: u8 = 1 << 2;
pub const POINTER_BUTTON_X1: u8 = 1 << 3;
pub const POINTER_BUTTON_X2: u8 = 1 << 4;
pub const SERIO_ANY: u32 = u32::MAX;
pub const EV_SYN: u32 = 0x00;
pub const EV_KEY: u32 = 0x01;
pub const EV_REL: u32 = 0x02;
pub const SYN_REPORT: u32 = 0;
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_SIDE: u32 = 0x113;
pub const BTN_EXTRA: u32 = 0x114;
pub const REL_X: u32 = 0x00;
pub const REL_Y: u32 = 0x01;
pub const REL_HWHEEL: u32 = 0x06;
pub const REL_WHEEL: u32 = 0x08;

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverClass {
    Display = 1,
    Input = 2,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverBus {
    Platform = 1,
    Serio = 2,
    Usb = 3,
    Pci = 4,
    Virtio = 5,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayPixelFormat {
    Rgb = 0,
    Bgr = 1,
    Bitmask = 2,
    Unknown = 0xff,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverLogLevel {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverMmioCachePolicy {
    Uncached = 0,
    WriteCombine = 1,
}

pub const PCI_BAR_FLAG_IO_SPACE: u32 = 1 << 0;
pub const PCI_BAR_FLAG_PREFETCHABLE: u32 = 1 << 1;
pub const PCI_BAR_FLAG_64BIT: u32 = 1 << 2;

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputDeviceKind {
    Keyboard = 1,
    Pointer = 2,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerProtocol {
    GenericRelative = 1,
    HidBoot = 2,
    Ps2Standard = 3,
    Ps2IntelliMouse = 4,
    Ps2Explorer = 5,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SerioType {
    I8042 = 1,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SerioProto {
    Ps2Keyboard = 1,
    Ps2Mouse = 2,
    Ps2IntelliMouse = 3,
    Ps2Explorer = 4,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PointerPacket {
    pub buttons: u8,
    pub reserved0: u8,
    pub reserved1: u8,
    pub reserved2: u8,
    pub dx: i16,
    pub dy: i16,
    pub wheel_vertical: i16,
    pub wheel_horizontal: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SerioDeviceId {
    pub type_: u32,
    pub proto: u32,
    pub id: u32,
    pub extra: u32,
}

impl SerioDeviceId {
    pub const fn new(type_: u32, proto: u32, id: u32, extra: u32) -> Self {
        Self {
            type_,
            proto,
            id,
            extra,
        }
    }

    pub const fn i8042_mouse() -> Self {
        Self::new(
            SerioType::I8042 as u32,
            SerioProto::Ps2Mouse as u32,
            SERIO_ANY,
            SERIO_ANY,
        )
    }

    pub const fn i8042_keyboard() -> Self {
        Self::new(
            SerioType::I8042 as u32,
            SerioProto::Ps2Keyboard as u32,
            SERIO_ANY,
            SERIO_ANY,
        )
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SerioPortInfo {
    pub port_id: u32,
    pub type_: u32,
    pub proto: u32,
    pub id: u32,
    pub extra: u32,
}

impl SerioPortInfo {
    pub const fn new(port_id: u32, type_: u32, proto: u32, id: u32, extra: u32) -> Self {
        Self {
            port_id,
            type_,
            proto,
            id,
            extra,
        }
    }

    pub const fn i8042_mouse(port_id: u32) -> Self {
        Self::new(
            port_id,
            SerioType::I8042 as u32,
            SerioProto::Ps2Mouse as u32,
            0,
            0,
        )
    }

    pub const fn i8042_keyboard(port_id: u32) -> Self {
        Self::new(
            port_id,
            SerioType::I8042 as u32,
            SerioProto::Ps2Keyboard as u32,
            0,
            0,
        )
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinuxSerioDeviceId {
    pub type_: u8,
    pub extra: u8,
    pub id: u8,
    pub proto: u8,
}

impl LinuxSerioDeviceId {
    pub const fn new(type_: u8, extra: u8, id: u8, proto: u8) -> Self {
        Self {
            type_,
            extra,
            id,
            proto,
        }
    }

    pub const fn terminator() -> Self {
        Self::new(0, 0, 0, 0)
    }

    pub const fn i8042_mouse() -> Self {
        Self::new(
            SerioType::I8042 as u8,
            0,
            SERIO_ANY as u8,
            SerioProto::Ps2Mouse as u8,
        )
    }

    pub const fn i8042_keyboard() -> Self {
        Self::new(
            SerioType::I8042 as u8,
            0,
            SERIO_ANY as u8,
            SerioProto::Ps2Keyboard as u8,
        )
    }

    pub const fn is_terminator(self) -> bool {
        self.type_ == 0 && self.extra == 0 && self.id == 0 && self.proto == 0
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinuxSerio {
    pub port_id: u32,
    pub manual_bind: u8,
    pub reserved0: [u8; 3],
    pub id: LinuxSerioDeviceId,
    pub name: [u8; DRIVER_NAME_MAX],
    pub phys: [u8; DRIVER_NAME_MAX],
    pub drvdata: usize,
}

impl LinuxSerio {
    pub const fn from_port_info(info: SerioPortInfo) -> Self {
        let mut name = [0_u8; DRIVER_NAME_MAX];
        let mut phys = [0_u8; DRIVER_NAME_MAX];
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
            port_id: info.port_id,
            manual_bind: 0,
            reserved0: [0; 3],
            id: LinuxSerioDeviceId::new(
                info.type_ as u8,
                info.extra as u8,
                info.id as u8,
                info.proto as u8,
            ),
            name,
            phys,
            drvdata: 0,
        }
    }
}

pub type LinuxSerioWriteWakeupFn = unsafe extern "C" fn(serio: *mut LinuxSerio);
pub type LinuxSerioInterruptFn =
    unsafe extern "C" fn(serio: *mut LinuxSerio, byte: u8, flags: u32) -> i32;
pub type LinuxSerioConnectFn =
    unsafe extern "C" fn(serio: *mut LinuxSerio, drv: *mut LinuxSerioDriver) -> i32;
pub type LinuxSerioReconnectFn = unsafe extern "C" fn(serio: *mut LinuxSerio) -> i32;
pub type LinuxSerioDisconnectFn = unsafe extern "C" fn(serio: *mut LinuxSerio);
pub type LinuxSerioCleanupFn = unsafe extern "C" fn(serio: *mut LinuxSerio);

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LinuxSerioDriver {
    pub struct_size: u32,
    pub name: [u8; DRIVER_NAME_MAX],
    pub name_len: u32,
    pub description: [u8; DRIVER_PATH_MAX],
    pub description_len: u32,
    pub id_table: *const LinuxSerioDeviceId,
    pub manual_bind: u8,
    pub reserved0: [u8; 7],
    pub write_wakeup: Option<LinuxSerioWriteWakeupFn>,
    pub interrupt: Option<LinuxSerioInterruptFn>,
    pub connect: Option<LinuxSerioConnectFn>,
    pub reconnect: Option<LinuxSerioReconnectFn>,
    pub fast_reconnect: Option<LinuxSerioReconnectFn>,
    pub disconnect: Option<LinuxSerioDisconnectFn>,
    pub cleanup: Option<LinuxSerioCleanupFn>,
}

impl LinuxSerioDriver {
    pub const fn new(
        name: &'static str,
        description: &'static str,
        id_table: *const LinuxSerioDeviceId,
        connect: Option<LinuxSerioConnectFn>,
        disconnect: Option<LinuxSerioDisconnectFn>,
        interrupt: Option<LinuxSerioInterruptFn>,
    ) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            name: copy_ascii_name(name),
            name_len: name.len() as u32,
            description: copy_ascii_path(description),
            description_len: description.len() as u32,
            id_table,
            manual_bind: 0,
            reserved0: [0; 7],
            write_wakeup: None,
            interrupt,
            connect,
            reconnect: None,
            fast_reconnect: None,
            disconnect,
            cleanup: None,
        }
    }

    pub fn name_str(&self) -> Result<&str, str::Utf8Error> {
        str::from_utf8(&self.name[..(self.name_len as usize).min(DRIVER_NAME_MAX)])
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxPs2Disposition {
    Process = 0,
    Ignore = 1,
    Error = 2,
}

pub type LinuxPs2PreReceiveHandler =
    unsafe extern "C" fn(ps2dev: *mut LinuxPs2Dev, byte: u8, flags: u32) -> u32;
pub type LinuxPs2ReceiveHandler = unsafe extern "C" fn(ps2dev: *mut LinuxPs2Dev, byte: u8);

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LinuxPs2Dev {
    pub serio: *mut LinuxSerio,
    pub flags: u64,
    pub cmdbuf: [u8; 8],
    pub cmdcnt: u8,
    pub nak: u8,
    pub reserved0: [u8; 6],
    pub pre_receive_handler: Option<LinuxPs2PreReceiveHandler>,
    pub receive_handler: Option<LinuxPs2ReceiveHandler>,
    pub drvdata: *mut c_void,
}

impl Default for LinuxPs2Dev {
    fn default() -> Self {
        Self {
            serio: core::ptr::null_mut(),
            flags: 0,
            cmdbuf: [0; 8],
            cmdcnt: 0,
            nak: 0,
            reserved0: [0; 6],
            pre_receive_handler: None,
            receive_handler: None,
            drvdata: core::ptr::null_mut(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinuxInputId {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

pub type LinuxInputOpenFn = unsafe extern "C" fn(dev: *mut LinuxInputDev) -> i32;
pub type LinuxInputCloseFn = unsafe extern "C" fn(dev: *mut LinuxInputDev);

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LinuxInputDev {
    pub name: *const c_char,
    pub phys: *const c_char,
    pub uniq: *const c_char,
    pub id: LinuxInputId,
    pub open: Option<LinuxInputOpenFn>,
    pub close: Option<LinuxInputCloseFn>,
    pub drvdata: usize,
    pub evbit: [u64; 1],
    pub keybit: [u64; 5],
    pub relbit: [u64; 1],
}

impl Default for LinuxInputDev {
    fn default() -> Self {
        Self {
            name: core::ptr::null(),
            phys: core::ptr::null(),
            uniq: core::ptr::null(),
            id: LinuxInputId::default(),
            open: None,
            close: None,
            drvdata: 0,
            evbit: [0; 1],
            keybit: [0; 5],
            relbit: [0; 1],
        }
    }
}

pub type SerioConnectFn = unsafe extern "C" fn(port: *const SerioPortInfo) -> i32;
pub type SerioDisconnectFn = unsafe extern "C" fn(port: *const SerioPortInfo);
pub type SerioInterruptFn =
    unsafe extern "C" fn(port: *const SerioPortInfo, byte: u8, flags: u32) -> i32;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SerioDriverRegistration {
    pub abi_version: u32,
    pub struct_size: u32,
    pub name: [u8; DRIVER_NAME_MAX],
    pub name_len: u32,
    pub id: SerioDeviceId,
    pub connect: Option<SerioConnectFn>,
    pub disconnect: Option<SerioDisconnectFn>,
    pub interrupt: Option<SerioInterruptFn>,
}

impl SerioDriverRegistration {
    pub const fn new(
        name: &'static str,
        id: SerioDeviceId,
        connect: Option<SerioConnectFn>,
        disconnect: Option<SerioDisconnectFn>,
        interrupt: Option<SerioInterruptFn>,
    ) -> Self {
        Self {
            abi_version: DRIVER_MODULE_ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u32,
            name: copy_ascii_name(name),
            name_len: name.len() as u32,
            id,
            connect,
            disconnect,
            interrupt,
        }
    }

    pub fn name_str(&self) -> Result<&str, str::Utf8Error> {
        str::from_utf8(&self.name[..(self.name_len as usize).min(DRIVER_NAME_MAX)])
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DriverModuleHeader {
    pub abi_version: u32,
    pub header_size: u32,
    pub class: DriverClass,
    pub bus: DriverBus,
    pub flags: u32,
    pub module_path: [u8; DRIVER_PATH_MAX],
    pub module_path_len: u32,
    pub name: [u8; DRIVER_NAME_MAX],
    pub name_len: u32,
}

impl DriverModuleHeader {
    pub const fn new(
        class: DriverClass,
        bus: DriverBus,
        module_path: &'static str,
        name: &'static str,
    ) -> Self {
        Self {
            abi_version: DRIVER_MODULE_ABI_VERSION,
            header_size: core::mem::size_of::<Self>() as u32,
            class,
            bus,
            flags: 0,
            module_path: copy_ascii_path(module_path),
            module_path_len: module_path.len() as u32,
            name: copy_ascii_name(name),
            name_len: name.len() as u32,
        }
    }

    pub fn module_path_str(&self) -> Result<&str, str::Utf8Error> {
        str::from_utf8(&self.module_path[..(self.module_path_len as usize).min(DRIVER_PATH_MAX)])
    }

    pub fn name_str(&self) -> Result<&str, str::Utf8Error> {
        str::from_utf8(&self.name[..(self.name_len as usize).min(DRIVER_NAME_MAX)])
    }

    pub fn from_runtime(class: DriverClass, bus: DriverBus, module_path: &str, name: &str) -> Self {
        Self {
            abi_version: DRIVER_MODULE_ABI_VERSION,
            header_size: core::mem::size_of::<Self>() as u32,
            class,
            bus,
            flags: 0,
            module_path: copy_ascii_path_runtime(module_path),
            module_path_len: module_path.len().min(DRIVER_PATH_MAX) as u32,
            name: copy_ascii_name_runtime(name),
            name_len: name.len().min(DRIVER_NAME_MAX) as u32,
        }
    }
}

pub type RegisterSerioDriverFn =
    unsafe extern "C" fn(driver: *const SerioDriverRegistration) -> i32;
pub type ReportPointerPacketFn = unsafe extern "C" fn(packet: *const PointerPacket) -> i32;
pub type RegisterDisplayFramebufferFn =
    unsafe extern "C" fn(framebuffer: *const DisplayFramebufferRegistration) -> i32;
pub type DriverLogFn =
    unsafe extern "C" fn(level: u32, message_ptr: *const u8, message_len: u32) -> i32;
pub type PciFindDeviceFn = unsafe extern "C" fn(
    vendor_id: u16,
    device_id: u16,
    index: u32,
    out_info: *mut DriverPciDeviceInfo,
) -> i32;
pub type PciReadConfigU32Fn = unsafe extern "C" fn(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u32,
    out_value: *mut u32,
) -> i32;
pub type PciWriteConfigU32Fn = unsafe extern "C" fn(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u32,
    value: u32,
) -> i32;
pub type PciGetBarInfoFn = unsafe extern "C" fn(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    bar_index: u32,
    out_info: *mut DriverPciBarInfo,
) -> i32;
pub type MapMmioFn = unsafe extern "C" fn(
    phys_addr: u64,
    size: u64,
    cache_policy: u32,
    out_virt_addr: *mut u64,
) -> i32;
pub type ReadBootFileFn = unsafe extern "C" fn(
    path_ptr: *const u8,
    path_len: u32,
    dst: *mut u8,
    dst_len: u64,
    out_read_len: *mut u64,
) -> i32;
pub type QueryBootFramebufferFn =
    unsafe extern "C" fn(out_info: *mut DisplayFramebufferRegistration) -> i32;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DriverPciDeviceInfo {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub revision: u8,
    pub prog_if: u8,
    pub subclass: u8,
    pub class_code: u8,
    pub subsystem_vendor_id: u16,
    pub subsystem_device_id: u16,
    pub interrupt_line: u8,
    pub interrupt_pin: u8,
    pub config_size: u16,
    pub reserved0: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DriverPciBarInfo {
    pub base: u64,
    pub size: u64,
    pub flags: u32,
    pub reserved0: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DisplayFramebufferRegistration {
    pub addr: u64,
    pub size: u64,
    pub back_buffer_addr: u64,
    pub back_buffer_size: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: u32,
    pub bytes_per_pixel: u8,
    pub reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DriverKernelApiV1 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub register_serio_driver: Option<RegisterSerioDriverFn>,
    pub report_pointer_packet: Option<ReportPointerPacketFn>,
    pub register_display_framebuffer: Option<RegisterDisplayFramebufferFn>,
    pub log: Option<DriverLogFn>,
    pub pci_find_device: Option<PciFindDeviceFn>,
    pub pci_read_config_u32: Option<PciReadConfigU32Fn>,
    pub pci_write_config_u32: Option<PciWriteConfigU32Fn>,
    pub pci_get_bar_info: Option<PciGetBarInfoFn>,
    pub map_mmio: Option<MapMmioFn>,
    pub read_boot_file: Option<ReadBootFileFn>,
    pub query_boot_framebuffer: Option<QueryBootFramebufferFn>,
}

impl DriverKernelApiV1 {
    pub const fn new(
        register_serio_driver: Option<RegisterSerioDriverFn>,
        report_pointer_packet: Option<ReportPointerPacketFn>,
        register_display_framebuffer: Option<RegisterDisplayFramebufferFn>,
        log: Option<DriverLogFn>,
        pci_find_device: Option<PciFindDeviceFn>,
        pci_read_config_u32: Option<PciReadConfigU32Fn>,
        pci_write_config_u32: Option<PciWriteConfigU32Fn>,
        pci_get_bar_info: Option<PciGetBarInfoFn>,
        map_mmio: Option<MapMmioFn>,
        read_boot_file: Option<ReadBootFileFn>,
        query_boot_framebuffer: Option<QueryBootFramebufferFn>,
    ) -> Self {
        Self {
            abi_version: DRIVER_MODULE_ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u32,
            register_serio_driver,
            report_pointer_packet,
            register_display_framebuffer,
            log,
            pci_find_device,
            pci_read_config_u32,
            pci_write_config_u32,
            pci_get_bar_info,
            map_mmio,
            read_boot_file,
            query_boot_framebuffer,
        }
    }
}

pub type DriverInitFn = unsafe extern "C" fn(api: *const DriverKernelApiV1) -> i32;

const fn copy_ascii_name(value: &str) -> [u8; DRIVER_NAME_MAX] {
    let mut dest = [0_u8; DRIVER_NAME_MAX];
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() && index < DRIVER_NAME_MAX {
        dest[index] = bytes[index];
        index += 1;
    }
    dest
}

fn copy_ascii_name_runtime(value: &str) -> [u8; DRIVER_NAME_MAX] {
    let mut dest = [0_u8; DRIVER_NAME_MAX];
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() && index < DRIVER_NAME_MAX {
        dest[index] = bytes[index];
        index += 1;
    }
    dest
}

const fn copy_ascii_path(value: &str) -> [u8; DRIVER_PATH_MAX] {
    let mut dest = [0_u8; DRIVER_PATH_MAX];
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() && index < DRIVER_PATH_MAX {
        dest[index] = bytes[index];
        index += 1;
    }
    dest
}

fn copy_ascii_path_runtime(value: &str) -> [u8; DRIVER_PATH_MAX] {
    let mut dest = [0_u8; DRIVER_PATH_MAX];
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() && index < DRIVER_PATH_MAX {
        dest[index] = bytes[index];
        index += 1;
    }
    dest
}
