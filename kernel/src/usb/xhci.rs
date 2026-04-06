use alloc::vec;
use alloc::vec::Vec;
use core::convert::TryFrom;
use core::ffi::c_void;
use core::hint::spin_loop;
use core::mem::size_of;
use core::num::NonZeroUsize;
use core::ptr;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use spin::Mutex;
use xhci::Registers as XhciRegisters;
use xhci::accessor::Mapper;
use xhci::context::{EndpointType, Input32Byte, Input64Byte, InputHandler};
use xhci::registers::capability::StructuralParameters2;
use xhci::registers::doorbell::Register as DoorbellRegister;
use xhci::registers::operational::PortStatusAndControlRegister;
use xhci::ring::trb::command::{
    AddressDevice as AddressDeviceCommand, ConfigureEndpoint as ConfigureEndpointCommand,
    EnableSlot as EnableSlotCommand,
};
use xhci::ring::trb::event::{Allowed as EventTrb, CompletionCode};
use xhci::ring::trb::transfer::{
    DataStage, Direction as TrbDirection, Normal, SetupStage, StatusStage, TransferType,
};
use xhci::ring::trb::{BYTES as XHCI_TRB_BYTES, Link as LinkTrb};

use super::host::UsbHostControllerInfo;

const XHCI_TRB_CYCLE_BIT: u32 = 1 << 0;
const XHCI_TRB_TYPE_SHIFT: u32 = 10;
const XHCI_TRB_TYPE_MASK: u32 = 0x3f << XHCI_TRB_TYPE_SHIFT;

const XHCI_COMMAND_RING_TRBS: usize = 256;
const XHCI_EVENT_RING_TRBS: usize = 256;
const XHCI_TRANSFER_RING_TRBS: usize = 32;
const XHCI_CONTROL_BUFFER_BYTES: usize = 4096;
const XHCI_REGISTER_WAIT_SPINS: usize = 1_000_000;
const XHCI_EVENT_WAIT_SPINS: usize = 20_000_000;
const XHCI_TRANSFER_LOG_LIMIT: usize = 0;
const XHCI_POLL_SUBMIT_LOG_LIMIT: usize = 0;

const USB_DT_DEVICE: u8 = 0x01;
const USB_DT_CONFIG: u8 = 0x02;
const USB_DT_INTERFACE: u8 = 0x04;
const USB_DT_ENDPOINT: u8 = 0x05;
const USB_DT_HID: u8 = 0x21;
const USB_DT_REPORT: u8 = 0x22;

const USB_REQ_GET_DESCRIPTOR: u8 = 0x06;
const USB_REQ_SET_CONFIGURATION: u8 = 0x09;

const USB_DIR_OUT: u8 = 0x00;
const USB_DIR_IN: u8 = 0x80;
const USB_TYPE_STANDARD: u8 = 0x00 << 5;
const USB_RECIP_DEVICE: u8 = 0x00;
const USB_RECIP_INTERFACE: u8 = 0x01;
const USB_ENDPOINT_XFER_INT: u8 = 0x03;
const USB_CLASS_HID: u8 = 0x03;

const XHCI_COMP_SUCCESS: u8 = CompletionCode::Success as u8;
const XHCI_COMP_SHORT_PACKET: u8 = CompletionCode::ShortPacket as u8;
const XHCI_COMP_SLOT_NOT_ENABLED: u8 = CompletionCode::SlotNotEnabledError as u8;
const XHCI_COMP_ENDPOINT_NOT_ENABLED: u8 = CompletionCode::EndpointNotEnabledError as u8;
const XHCI_COMP_STALL: u8 = CompletionCode::StallError as u8;
const XHCI_COMP_CONTEXT_STATE: u8 = CompletionCode::ContextStateError as u8;

type XhciTrb = [u32; 4];

static XHCI_TRANSFER_EVENT_COUNT: AtomicU64 = AtomicU64::new(0);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct XhciEventRingSegmentTableEntry {
    ring_segment_base_address: u64,
    ring_segment_size: u16,
    _reserved0: u16,
    _reserved1: u32,
}

struct XhciDmaBuffer {
    cpu_ptr: *mut u8,
    dma_addr: u64,
    size: usize,
}

struct XhciRing {
    buffer: XhciDmaBuffer,
    trb_count: usize,
    enqueue_index: usize,
    cycle_state: bool,
}

struct XhciEnumeratedDevice {
    slot_id: u8,
    usb_device_ptr: usize,
    interface_ptr: usize,
    interface_number: u8,
    endpoint_address: u8,
    endpoint_max_packet_size: u16,
    ep_index: usize,
    output_context: XhciDmaBuffer,
    input_context: XhciDmaBuffer,
    ep0_ring: XhciRing,
    interrupt_ring: XhciRing,
    control_buffer: XhciDmaBuffer,
    poll_buffer: XhciDmaBuffer,
    pending_poll_trb_dma: u64,
}

struct XhciControllerState {
    record: XhciControllerRecord,
    registers: XhciRegisters<XhciMmioMapper>,
    command_ring: XhciRing,
    event_ring: XhciDmaBuffer,
    event_ring_table: XhciDmaBuffer,
    device_context_base_array: XhciDmaBuffer,
    _scratchpad_array: Option<XhciDmaBuffer>,
    _scratchpad_buffers: Vec<XhciDmaBuffer>,
    event_ring_dequeue_index: usize,
    event_ring_cycle_state: bool,
    last_portsc: Vec<u32>,
    devices: Vec<Option<XhciEnumeratedDevice>>,
    context_size: usize,
}

#[derive(Clone, Copy)]
struct XhciControlSetup {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
}

#[derive(Clone, Copy, Debug)]
// Retained as a rich controller inventory surface for future diagnostics and registries.
#[allow(dead_code)]
pub(crate) struct XhciControllerRecord {
    pub(crate) bdf: crate::arch::pci::PciDevice,
    pub(crate) mmio_base: usize,
    pub(crate) mmio_len: usize,
    pub(crate) cap_length: u8,
    pub(crate) hci_version: u16,
    pub(crate) max_slots: u8,
    pub(crate) max_interrupters: u16,
    pub(crate) max_ports: u8,
    pub(crate) doorbell_offset: u32,
    pub(crate) runtime_offset: u32,
    pub(crate) operational_base: usize,
    pub(crate) runtime_base: usize,
    pub(crate) doorbell_base: usize,
}

#[derive(Clone, Debug)]
struct ParsedHidInterface {
    configuration_value: u8,
    interface_number: u8,
    interface_class: u8,
    interface_sub_class: u8,
    interface_protocol: u8,
    hid_descriptor: Vec<u8>,
    report_descriptor_len: u16,
    endpoint_address: u8,
    endpoint_max_packet_size: u16,
    endpoint_interval: u8,
}

#[derive(Clone, Copy, Debug)]
enum XhciEvent {
    CommandCompletion {
        command_trb_dma: u64,
        completion_code: u8,
        slot_id: u8,
    },
    Transfer {
        transfer_trb_dma: u64,
        completion_code: u8,
        slot_id: u8,
        ep_id: u8,
        residual_len: u32,
    },
    PortStatusChange {
        port_id: usize,
    },
}

unsafe impl Send for XhciDmaBuffer {}
unsafe impl Send for XhciRing {}
unsafe impl Send for XhciEnumeratedDevice {}
unsafe impl Send for XhciControllerState {}

static XHCI_CONTROLLERS: Mutex<Vec<XhciControllerState>> = Mutex::new(Vec::new());
static XHCI_TRANSFER_LOGS: AtomicUsize = AtomicUsize::new(0);
static XHCI_POLL_SUBMIT_BEGIN_LOGS: AtomicUsize = AtomicUsize::new(0);
static XHCI_POLL_SUBMIT_QUEUED_LOGS: AtomicUsize = AtomicUsize::new(0);
static XHCI_POLL_SUBMIT_DONE_LOGS: AtomicUsize = AtomicUsize::new(0);
const XHCI_EVENTS_PER_SERVICE: usize = 32;

#[derive(Clone, Copy, Debug, Default)]
struct XhciMmioMapper;

impl Mapper for XhciMmioMapper {
    unsafe fn map(&mut self, phys_start: usize, _bytes: usize) -> NonZeroUsize {
        debug_assert_ne!(phys_start, 0);
        unsafe { NonZeroUsize::new_unchecked(phys_start) }
    }

    fn unmap(&mut self, _virt_start: usize, _bytes: usize) {}
}

pub(crate) fn initialize(controllers: &[UsbHostControllerInfo]) -> usize {
    let mut initialized = 0usize;
    for controller in controllers.iter().copied() {
        if controller.kind != crate::arch::pci::UsbHostControllerKind::Xhci {
            continue;
        }
        if let Some(state) = probe_controller(controller) {
            XHCI_CONTROLLERS.lock().push(state);
            initialized += 1;
        }
    }
    initialized
}

// Retained as an inspection surface for future USB diagnostics.
#[allow(dead_code)]
pub(crate) fn controllers() -> Vec<XhciControllerRecord> {
    XHCI_CONTROLLERS
        .lock()
        .iter()
        .map(|controller| controller.record)
        .collect()
}

pub(crate) fn service_pending() -> usize {
    let mut work = 0usize;
    let mut controllers = XHCI_CONTROLLERS.lock();
    for controller in controllers.iter_mut() {
        if work >= XHCI_EVENTS_PER_SERVICE {
            break;
        }
        work += service_controller(controller, XHCI_EVENTS_PER_SERVICE - work);
    }
    work
}

fn portsc_raw(portsc: PortStatusAndControlRegister) -> u32 {
    unsafe { core::mem::transmute(portsc) }
}

fn clear_port_change_bits(portsc: &mut PortStatusAndControlRegister) {
    portsc.clear_connect_status_change();
    portsc.clear_port_enabled_disabled_change();
    portsc.clear_warm_port_reset_change();
    portsc.clear_over_current_change();
    portsc.clear_port_reset_change();
    portsc.clear_port_link_state_change();
    portsc.clear_port_config_error_change();
}

fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
    for _ in 0..XHCI_REGISTER_WAIT_SPINS {
        if predicate() {
            return true;
        }
        spin_loop();
    }
    false
}

fn probe_controller(controller: UsbHostControllerInfo) -> Option<XhciControllerState> {
    let resource = controller.bar0?;
    let mmio_len = usize::try_from(resource.size).ok()?;
    if mmio_len < 0x100 {
        crate::debug::println!(
            "xhci: skip controller {:02x}:{:02x}.{} because bar0 is too small ({:#x})",
            controller.address.bus,
            controller.address.device,
            controller.address.function,
            resource.size,
        );
        return None;
    }

    let mmio = crate::driver::mmio::map(resource.start, mmio_len, false);
    if mmio.is_null() {
        crate::debug::println!(
            "xhci: failed to map controller {:02x}:{:02x}.{} bar0={:#x} len={:#x}",
            controller.address.bus,
            controller.address.device,
            controller.address.function,
            resource.start,
            resource.size,
        );
        return None;
    }

    let mmio_base = mmio as usize;
    let regs = unsafe { XhciRegisters::new(mmio_base, XhciMmioMapper) };
    let cap_length = regs.capability.caplength.read_volatile().get();
    let hci_version = regs.capability.hciversion.read_volatile().get();
    let hcs_params1 = regs.capability.hcsparams1.read_volatile();
    let hcs_params2 = regs.capability.hcsparams2.read_volatile();
    let hcc_params1 = regs.capability.hccparams1.read_volatile();
    let dboff = regs.capability.dboff.read_volatile().get();
    let rtsoff = regs.capability.rtsoff.read_volatile().get();
    let max_slots = hcs_params1.number_of_device_slots();
    let max_interrupters = hcs_params1.number_of_interrupts();
    let max_ports = hcs_params1.number_of_ports();
    let operational_base = mmio_base.checked_add(cap_length as usize)?;
    let runtime_base = mmio_base.checked_add(usize::try_from(rtsoff).ok()?)?;
    let doorbell_base = mmio_base.checked_add(usize::try_from(dboff).ok()?)?;
    let context_size = if hcc_params1.context_size() { 64 } else { 32 };

    let record = XhciControllerRecord {
        bdf: controller.address,
        mmio_base,
        mmio_len,
        cap_length,
        hci_version,
        max_slots,
        max_interrupters,
        max_ports,
        doorbell_offset: dboff,
        runtime_offset: rtsoff,
        operational_base,
        runtime_base,
        doorbell_base,
    };

    crate::driver::dma::set_mask_and_coherent(mmio as *mut c_void, u64::MAX);
    let mut state = allocate_controller_state(record, hcs_params2, context_size, regs)?;
    if !bring_up_controller(&mut state) {
        crate::debug::println!(
            "xhci: controller bring-up failed {:02x}:{:02x}.{}",
            controller.address.bus,
            controller.address.device,
            controller.address.function,
        );
        return None;
    }

    crate::debug::println!(
        "xhci controller ready: bdf={:02x}:{:02x}.{} mmio={:#x}/len={:#x} caplen={:#x} version={:#x} slots={} irqs={} ports={} ctx={} op={:#x} rt={:#x} db={:#x}",
        controller.address.bus,
        controller.address.device,
        controller.address.function,
        mmio_base,
        mmio_len,
        cap_length,
        hci_version,
        max_slots,
        max_interrupters,
        max_ports,
        context_size,
        operational_base,
        runtime_base,
        doorbell_base,
    );

    log_port_snapshot(&mut state, true);
    enumerate_boot_devices(&mut state);
    Some(state)
}

fn allocate_controller_state(
    record: XhciControllerRecord,
    hcs_params2: StructuralParameters2,
    context_size: usize,
    registers: XhciRegisters<XhciMmioMapper>,
) -> Option<XhciControllerState> {
    let device_ptr = record.mmio_base as *mut c_void;
    let command_ring_buffer =
        dma_buffer_alloc(device_ptr, XHCI_COMMAND_RING_TRBS * XHCI_TRB_BYTES)?;
    let event_ring = dma_buffer_alloc(device_ptr, XHCI_EVENT_RING_TRBS * XHCI_TRB_BYTES)?;
    let event_ring_table =
        dma_buffer_alloc(device_ptr, size_of::<XhciEventRingSegmentTableEntry>())?;
    let dcbaa_entries = usize::from(record.max_slots).saturating_add(1);
    let device_context_base_array =
        dma_buffer_alloc(device_ptr, dcbaa_entries.saturating_mul(size_of::<u64>()))?;

    let mut command_ring = XhciRing {
        buffer: command_ring_buffer,
        trb_count: XHCI_COMMAND_RING_TRBS,
        enqueue_index: 0,
        cycle_state: true,
    };
    prime_ring_link(&mut command_ring)?;

    let event_ring_table_entries = dma_erst_slice_mut(&event_ring_table, 1)?;
    event_ring_table_entries[0] = XhciEventRingSegmentTableEntry {
        ring_segment_base_address: event_ring.dma_addr,
        ring_segment_size: XHCI_EVENT_RING_TRBS as u16,
        _reserved0: 0,
        _reserved1: 0,
    };

    zero_dma_words(&device_context_base_array);
    let mut scratchpad_array = None;
    let mut scratchpad_buffers = Vec::new();
    let scratchpads = hcs_params2.max_scratchpad_buffers() as usize;
    if scratchpads != 0 {
        let array = dma_buffer_alloc(device_ptr, scratchpads * size_of::<u64>())?;
        let entries = dma_u64_slice_mut(&array, scratchpads)?;
        for slot in entries.iter_mut() {
            *slot = 0;
        }
        for (_index, entry) in entries.iter_mut().enumerate() {
            let buffer = dma_buffer_alloc(device_ptr, 4096)?;
            *entry = buffer.dma_addr;
            scratchpad_buffers.push(buffer);
            crate::debug::println!(
                "xhci scratchpad allocated: index={} dma={:#x}",
                _index,
                *entry
            );
        }
        let dcbaa = dma_u64_slice_mut(&device_context_base_array, dcbaa_entries)?;
        dcbaa[0] = array.dma_addr;
        scratchpad_array = Some(array);
    }

    Some(XhciControllerState {
        record,
        registers,
        command_ring,
        event_ring,
        event_ring_table,
        device_context_base_array,
        _scratchpad_array: scratchpad_array,
        _scratchpad_buffers: scratchpad_buffers,
        event_ring_dequeue_index: 0,
        event_ring_cycle_state: true,
        last_portsc: vec![0; usize::from(record.max_ports)],
        devices: (0..usize::from(record.max_ports)).map(|_| None).collect(),
        context_size,
    })
}

fn bring_up_controller(controller: &mut XhciControllerState) -> bool {
    if !xhci_stop_controller(controller) {
        return false;
    }
    if !xhci_reset_controller(controller) {
        return false;
    }
    xhci_power_ports(controller);
    if !xhci_program_runtime_state(controller) {
        return false;
    }
    xhci_run_controller(controller)
}

fn enumerate_boot_devices(controller: &mut XhciControllerState) {
    for port in 0..usize::from(controller.record.max_ports) {
        let portsc = controller
            .registers
            .port_register_set
            .read_volatile_at(port)
            .portsc;
        if !portsc.current_connect_status() {
            continue;
        }
        match enumerate_boot_port(controller, port) {
            Some(device) => {
                controller.devices[port] = Some(device);
            }
            None => {
                crate::debug::println!("xhci: enumeration skipped on port={}", port + 1);
            }
        }
    }
}

fn enumerate_boot_port(
    controller: &mut XhciControllerState,
    port: usize,
) -> Option<XhciEnumeratedDevice> {
    if !reset_port(controller, port) {
        crate::debug::println!("xhci: port reset failed: port={}", port + 1);
        return None;
    }

    let portsc = controller
        .registers
        .port_register_set
        .read_volatile_at(port)
        .portsc;
    let speed = portsc.port_speed();
    let slot_id = command_enable_slot(controller)?;
    let mut device = match allocate_enumerated_device(controller, port, slot_id, speed) {
        Some(device) => device,
        None => {
            crate::debug::println!(
                "xhci: device allocation failed: port={} slot={}",
                port + 1,
                slot_id
            );
            return None;
        }
    };

    let max_packet0 = default_control_max_packet(speed);
    zero_dma_words(&device.output_context);
    zero_dma_words(&device.input_context);
    {
        let dcbaa = dma_u64_slice_mut(
            &controller.device_context_base_array,
            usize::from(controller.record.max_slots).saturating_add(1),
        )?;
        dcbaa[slot_id as usize] = device.output_context.dma_addr;
    }

    build_address_device_context(
        controller,
        &device.input_context,
        port + 1,
        speed,
        max_packet0,
        device.ep0_ring.buffer.dma_addr,
    );
    if !command_address_device(controller, slot_id, device.input_context.dma_addr) {
        crate::debug::println!(
            "xhci: address-device failed: port={} slot={}",
            port + 1,
            slot_id
        );
        return None;
    }

    let mut device_desc = [0u8; 18];
    if control_in(
        controller,
        &mut device,
        XhciControlSetup {
            request_type: USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
            request: USB_REQ_GET_DESCRIPTOR,
            value: ((USB_DT_DEVICE as u16) << 8),
            index: 0,
        },
        &mut device_desc,
    )
    .is_none()
    {
        crate::debug::println!(
            "xhci: device descriptor fetch failed: port={} slot={}",
            port + 1,
            slot_id
        );
        return None;
    }

    let vendor_id = u16::from_le_bytes([device_desc[8], device_desc[9]]);
    let product_id = u16::from_le_bytes([device_desc[10], device_desc[11]]);
    let device_bcd = u16::from_le_bytes([device_desc[12], device_desc[13]]);
    let device_class = device_desc[4];
    let device_sub_class = device_desc[5];
    let device_protocol = device_desc[6];
    crate::debug::println!(
        "xhci device descriptor: port={} slot={} vendor={:04x} product={:04x} class={:#x}/{:#x}/{:#x} bcd={:04x}",
        port + 1,
        slot_id,
        vendor_id,
        product_id,
        device_class,
        device_sub_class,
        device_protocol,
        device_bcd,
    );

    let mut config_header = [0u8; 9];
    if control_in(
        controller,
        &mut device,
        XhciControlSetup {
            request_type: USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
            request: USB_REQ_GET_DESCRIPTOR,
            value: ((USB_DT_CONFIG as u16) << 8),
            index: 0,
        },
        &mut config_header,
    )
    .is_none()
    {
        crate::debug::println!(
            "xhci: config header fetch failed: port={} slot={}",
            port + 1,
            slot_id
        );
        return None;
    }

    let config_len = u16::from_le_bytes([config_header[2], config_header[3]]) as usize;
    if config_len < 9 || config_len > XHCI_CONTROL_BUFFER_BYTES {
        crate::debug::println!(
            "xhci: invalid config length={} port={} slot={}",
            config_len,
            port + 1,
            slot_id
        );
        return None;
    }

    let mut config_desc = vec![0u8; config_len];
    if control_in(
        controller,
        &mut device,
        XhciControlSetup {
            request_type: USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
            request: USB_REQ_GET_DESCRIPTOR,
            value: ((USB_DT_CONFIG as u16) << 8),
            index: 0,
        },
        &mut config_desc,
    )
    .is_none()
    {
        crate::debug::println!(
            "xhci: full config fetch failed: port={} slot={}",
            port + 1,
            slot_id
        );
        return None;
    }

    let parsed = match parse_hid_interface(&config_desc) {
        Some(parsed) => parsed,
        None => {
            crate::debug::println!(
                "xhci: no HID interrupt interface found: port={} slot={} vendor={:04x} product={:04x} device_class={:#x}/{:#x}/{:#x} config_len={}",
                port + 1,
                slot_id,
                vendor_id,
                product_id,
                device_class,
                device_sub_class,
                device_protocol,
                config_len,
            );
            return None;
        }
    };
    crate::debug::println!(
        "xhci hid interface: port={} slot={} cfg={} intf={} class={:#x}/{:#x}/{:#x} ep={:#x} mps={} interval={} report_len={}",
        port + 1,
        slot_id,
        parsed.configuration_value,
        parsed.interface_number,
        parsed.interface_class,
        parsed.interface_sub_class,
        parsed.interface_protocol,
        parsed.endpoint_address,
        parsed.endpoint_max_packet_size,
        parsed.endpoint_interval,
        parsed.report_descriptor_len,
    );
    if control_no_data(
        controller,
        &mut device,
        XhciControlSetup {
            request_type: USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
            request: USB_REQ_SET_CONFIGURATION,
            value: parsed.configuration_value as u16,
            index: 0,
        },
    )
    .is_none()
    {
        crate::debug::println!(
            "xhci: set-configuration failed: port={} slot={} cfg={}",
            port + 1,
            slot_id,
            parsed.configuration_value
        );
        return None;
    }

    let mut report_desc = vec![0u8; parsed.report_descriptor_len as usize];
    if control_in(
        controller,
        &mut device,
        XhciControlSetup {
            request_type: USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_INTERFACE,
            request: USB_REQ_GET_DESCRIPTOR,
            value: ((USB_DT_REPORT as u16) << 8),
            index: parsed.interface_number as u16,
        },
        &mut report_desc,
    )
    .is_none()
    {
        crate::debug::println!(
            "xhci: report descriptor fetch failed: port={} slot={} intf={}",
            port + 1,
            slot_id,
            parsed.interface_number
        );
        return None;
    }

    device.interface_number = parsed.interface_number;
    device.endpoint_address = parsed.endpoint_address;
    device.endpoint_max_packet_size = parsed.endpoint_max_packet_size;
    device.ep_index = endpoint_index_from_address(parsed.endpoint_address);

    zero_dma_words(&device.input_context);
    build_interrupt_endpoint_context(
        controller,
        &device.input_context,
        port + 1,
        speed,
        max_packet0,
        device.ep0_ring.buffer.dma_addr,
        device.ep_index,
        device.interrupt_ring.buffer.dma_addr,
        parsed.endpoint_max_packet_size,
        parsed.endpoint_interval,
    );
    if !command_configure_endpoint(controller, slot_id, device.input_context.dma_addr) {
        crate::debug::println!(
            "xhci: configure-endpoint failed: port={} slot={} ep_addr={:#x}",
            port + 1,
            slot_id,
            parsed.endpoint_address
        );
        return None;
    }

    let interface = super::register_owned_interface(super::core::UsbInterfaceRegistration {
        devnum: (port + 1) as u32,
        speed: port_speed_to_usb_speed(speed),
        vendor_id,
        product_id,
        device_bcd,
        device_class,
        device_sub_class,
        device_protocol,
        interface_class: parsed.interface_class,
        interface_sub_class: parsed.interface_sub_class,
        interface_protocol: parsed.interface_protocol,
        interface_number: parsed.interface_number,
        endpoint_address: parsed.endpoint_address,
        endpoint_attributes: USB_ENDPOINT_XFER_INT,
        endpoint_max_packet_size: parsed.endpoint_max_packet_size,
        endpoint_interval: parsed.endpoint_interval,
        interface_extra: Some(parsed.hid_descriptor.as_slice()),
        manufacturer: None,
        product: None,
        serial: None,
        synthetic_hid_kind: None,
    });
    if interface.is_null() {
        crate::debug::println!(
            "xhci: core registration failed: port={} slot={}",
            port + 1,
            slot_id
        );
        return None;
    }

    let usb_device = unsafe { (*interface).usb_dev };
    super::runtime::register_device(
        usb_device,
        interface,
        parsed.interface_number,
        &parsed.hid_descriptor,
        &report_desc,
    );

    device.usb_device_ptr = usb_device as usize;
    device.interface_ptr = interface as usize;

    crate::debug::println!(
        "xhci hid device ready: port={} slot={} usb_dev={:#x} intf={:#x} vendor={:04x} product={:04x} class={:#x}/{:#x}/{:#x} ep={:#x} mps={} interval={}",
        port + 1,
        slot_id,
        device.usb_device_ptr,
        device.interface_ptr,
        vendor_id,
        product_id,
        parsed.interface_class,
        parsed.interface_sub_class,
        parsed.interface_protocol,
        parsed.endpoint_address,
        parsed.endpoint_max_packet_size,
        parsed.endpoint_interval,
    );

    if !submit_interrupt_poll(&mut controller.registers, &mut device) {
        crate::debug::println!(
            "xhci: failed to submit polling transfer: port={} slot={}",
            port + 1,
            slot_id
        );
        return None;
    }

    Some(device)
}

fn service_controller(controller: &mut XhciControllerState, budget: usize) -> usize {
    if budget == 0 {
        return 0;
    }

    let mut work = 0usize;
    while work < budget {
        let Some(event) = poll_event(controller) else {
            break;
        };
        handle_async_event(controller, event);
        work += 1;
    }

    let status = controller.registers.operational.usbsts.read_volatile();
    if status.event_interrupt() || status.port_change_detect() {
        let clear_event_interrupt = status.event_interrupt();
        let clear_port_change = status.port_change_detect();
        controller
            .registers
            .operational
            .usbsts
            .update_volatile(|status| {
                if clear_event_interrupt {
                    status.clear_event_interrupt();
                } else {
                    status.set_0_event_interrupt();
                }
                if clear_port_change {
                    status.clear_port_change_detect();
                } else {
                    status.set_0_port_change_detect();
                }
            });
        work += 1;
    }

    let interrupt_pending = controller
        .registers
        .interrupter_register_set
        .interrupter(0)
        .iman
        .read_volatile()
        .interrupt_pending();
    if interrupt_pending {
        let mut interrupter0 = controller
            .registers
            .interrupter_register_set
            .interrupter_mut(0);
        interrupter0.iman.update_volatile(|iman| {
            iman.clear_interrupt_pending();
            iman.set_interrupt_enable();
        });
        work += 1;
    }

    let port_log_work = log_port_snapshot(controller, false);
    work.saturating_add(port_log_work)
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
fn handle_async_event(controller: &mut XhciControllerState, event: XhciEvent) {
    match event {
        XhciEvent::Transfer {
            transfer_trb_dma,
            completion_code,
            slot_id,
            ep_id,
            residual_len,
        } => {
            XHCI_TRANSFER_EVENT_COUNT.fetch_add(1, Ordering::Relaxed);
            if XHCI_TRANSFER_LOGS.fetch_add(1, Ordering::Relaxed) < XHCI_TRANSFER_LOG_LIMIT {
                crate::debug::println!(
                    "xhci transfer event: slot={} ep_id={} code={} residual={} trb={:#x}",
                    slot_id,
                    ep_id,
                    completion_code,
                    residual_len,
                    transfer_trb_dma
                );
            }
            let device_index = controller.devices.iter().position(|entry| {
                let Some(device) = entry.as_ref() else {
                    return false;
                };
                device.slot_id == slot_id
                    && device.pending_poll_trb_dma == transfer_trb_dma
                    && ep_id == endpoint_id_for_index(device.ep_index) as u8
            });
            let Some(device_index) = device_index else {
                return;
            };

            let mut should_resubmit = true;
            {
                let device = controller.devices[device_index]
                    .as_mut()
                    .expect("device disappeared");
                let request_len = device.endpoint_max_packet_size as usize;
                let report_len = request_len.saturating_sub(residual_len as usize);

                let completed_normally = completion_code == XHCI_COMP_SUCCESS
                    || completion_code == XHCI_COMP_SHORT_PACKET;

                if completed_normally && report_len != 0 && report_len <= device.poll_buffer.size {
                    let bytes = unsafe {
                        core::slice::from_raw_parts(device.poll_buffer.cpu_ptr, report_len)
                    };
                    super::runtime::enqueue_report(
                        device.usb_device_ptr
                            as *mut crate::driver::linux::compat::LinuxCompatUsbDevice,
                        bytes,
                    );
                }

                if !completed_normally {
                    crate::debug::println!(
                        "xhci transfer completion anomaly: slot={} ep_id={} code={} residual={} report_len={}",
                        slot_id,
                        ep_id,
                        completion_code,
                        residual_len,
                        report_len,
                    );
                    if matches!(
                        completion_code,
                        XHCI_COMP_SLOT_NOT_ENABLED
                            | XHCI_COMP_ENDPOINT_NOT_ENABLED
                            | XHCI_COMP_STALL
                            | XHCI_COMP_CONTEXT_STATE
                    ) {
                        should_resubmit = false;
                    }
                }
            }

            if should_resubmit {
                let registers = &mut controller.registers;
                let device = controller.devices[device_index]
                    .as_mut()
                    .expect("device disappeared");
                if !submit_interrupt_poll(registers, device) {
                    crate::debug::println!(
                        "xhci: failed to resubmit polling transfer: slot={} ep_id={}",
                        slot_id,
                        ep_id
                    );
                }
            }
            return;
        }
        XhciEvent::PortStatusChange { port_id } => {
            if (1..=usize::from(controller.record.max_ports)).contains(&port_id) {
                controller.last_portsc[port_id - 1] = 0;
            }
        }
        XhciEvent::CommandCompletion {
            command_trb_dma,
            completion_code,
            slot_id,
        } => {
            crate::debug::println!(
                "xhci async command completion: trb={:#x} slot={} code={}",
                command_trb_dma,
                slot_id,
                completion_code,
            );
        }
    }
}

pub(crate) fn debug_transfer_event_count() -> u64 {
    XHCI_TRANSFER_EVENT_COUNT.load(Ordering::Relaxed)
}

fn reset_port(controller: &mut XhciControllerState, port: usize) -> bool {
    let portsc = controller
        .registers
        .port_register_set
        .read_volatile_at(port)
        .portsc;
    if !portsc.current_connect_status() {
        crate::debug::println!("xhci: reset skipped on disconnected port={}", port + 1);
        return false;
    }

    controller
        .registers
        .port_register_set
        .update_volatile_at(port, |set| {
            set.portsc.set_port_power();
            set.portsc.set_port_reset();
            clear_port_change_bits(&mut set.portsc);
        });

    for _ in 0..XHCI_EVENT_WAIT_SPINS {
        let current = controller
            .registers
            .port_register_set
            .read_volatile_at(port)
            .portsc;
        if !current.port_reset()
            && current.current_connect_status()
            && current.port_enabled_disabled()
        {
            controller
                .registers
                .port_register_set
                .update_volatile_at(port, |set| {
                    clear_port_change_bits(&mut set.portsc);
                });
            controller.last_portsc[port] = portsc_raw(current);
            return true;
        }
        spin_loop();
    }
    false
}

fn allocate_enumerated_device(
    controller: &XhciControllerState,
    _port: usize,
    slot_id: u8,
    _speed: u8,
) -> Option<XhciEnumeratedDevice> {
    let device_ptr = controller.record.mmio_base as *mut c_void;
    let output_context = dma_buffer_alloc(device_ptr, controller.context_size * 4)?;
    let input_context = dma_buffer_alloc(device_ptr, controller.context_size * 5)?;
    let mut ep0_ring = XhciRing {
        buffer: dma_buffer_alloc(device_ptr, XHCI_TRANSFER_RING_TRBS * XHCI_TRB_BYTES)?,
        trb_count: XHCI_TRANSFER_RING_TRBS,
        enqueue_index: 0,
        cycle_state: true,
    };
    prime_ring_link(&mut ep0_ring)?;

    let mut interrupt_ring = XhciRing {
        buffer: dma_buffer_alloc(device_ptr, XHCI_TRANSFER_RING_TRBS * XHCI_TRB_BYTES)?,
        trb_count: XHCI_TRANSFER_RING_TRBS,
        enqueue_index: 0,
        cycle_state: true,
    };
    prime_ring_link(&mut interrupt_ring)?;

    Some(XhciEnumeratedDevice {
        slot_id,
        usb_device_ptr: 0,
        interface_ptr: 0,
        interface_number: 0,
        endpoint_address: 0,
        endpoint_max_packet_size: 0,
        ep_index: 0,
        output_context,
        input_context,
        ep0_ring,
        interrupt_ring,
        control_buffer: dma_buffer_alloc(device_ptr, XHCI_CONTROL_BUFFER_BYTES)?,
        poll_buffer: dma_buffer_alloc(device_ptr, XHCI_CONTROL_BUFFER_BYTES)?,
        pending_poll_trb_dma: 0,
    })
}

fn build_address_device_context(
    controller: &XhciControllerState,
    input_context: &XhciDmaBuffer,
    root_port: usize,
    speed: u8,
    max_packet0: u16,
    ep0_ring_dma: u64,
) {
    zero_dma_words(input_context);
    if controller.context_size == 64 {
        let mut input = Input64Byte::new_64byte();
        populate_address_device_context(&mut input, root_port, speed, max_packet0, ep0_ring_dma);
        write_input_context(input_context, &input);
    } else {
        let mut input = Input32Byte::new_32byte();
        populate_address_device_context(&mut input, root_port, speed, max_packet0, ep0_ring_dma);
        write_input_context(input_context, &input);
    }
}

fn build_interrupt_endpoint_context(
    controller: &XhciControllerState,
    input_context: &XhciDmaBuffer,
    root_port: usize,
    speed: u8,
    max_packet0: u16,
    ep0_ring_dma: u64,
    ep_index: usize,
    interrupt_ring_dma: u64,
    max_packet: u16,
    interval: u8,
) {
    zero_dma_words(input_context);
    if controller.context_size == 64 {
        let mut input = Input64Byte::new_64byte();
        populate_interrupt_endpoint_context(
            &mut input,
            root_port,
            speed,
            max_packet0,
            ep0_ring_dma,
            ep_index,
            interrupt_ring_dma,
            max_packet,
            interval,
        );
        write_input_context(input_context, &input);
    } else {
        let mut input = Input32Byte::new_32byte();
        populate_interrupt_endpoint_context(
            &mut input,
            root_port,
            speed,
            max_packet0,
            ep0_ring_dma,
            ep_index,
            interrupt_ring_dma,
            max_packet,
            interval,
        );
        write_input_context(input_context, &input);
    }
}

fn control_in(
    controller: &mut XhciControllerState,
    device: &mut XhciEnumeratedDevice,
    setup: XhciControlSetup,
    out: &mut [u8],
) -> Option<usize> {
    let copy_len = out.len().min(device.control_buffer.size);
    let actual = control_transfer(controller, device, setup, Some(copy_len), true)?;
    let copied = actual.min(copy_len);
    unsafe {
        core::ptr::copy_nonoverlapping(device.control_buffer.cpu_ptr, out.as_mut_ptr(), copied);
    }
    Some(copied)
}

fn control_no_data(
    controller: &mut XhciControllerState,
    device: &mut XhciEnumeratedDevice,
    setup: XhciControlSetup,
) -> Option<()> {
    control_transfer(controller, device, setup, None, false).map(|_| ())
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
fn control_transfer(
    controller: &mut XhciControllerState,
    device: &mut XhciEnumeratedDevice,
    setup: XhciControlSetup,
    data_len: Option<usize>,
    data_in: bool,
) -> Option<usize> {
    let ring = &mut device.ep0_ring;
    let mut setup_trb = SetupStage::new();
    setup_trb
        .set_request_type(setup.request_type)
        .set_request(setup.request)
        .set_value(setup.value)
        .set_index(setup.index)
        .set_length(data_len.unwrap_or(0) as u16)
        .set_transfer_type(match data_len {
            Some(_) if data_in => TransferType::In,
            Some(_) => TransferType::Out,
            None => TransferType::No,
        });
    enqueue_trb(ring, setup_trb.into_raw())?;

    if let Some(data_len) = data_len {
        let mut data_trb = DataStage::default();
        data_trb
            .set_data_buffer_pointer(device.control_buffer.dma_addr)
            .set_trb_transfer_length(data_len as u32)
            .set_direction(if data_in {
                TrbDirection::In
            } else {
                TrbDirection::Out
            })
            .set_chain_bit();
        enqueue_trb(ring, data_trb.into_raw())?;
    }

    let mut status_trb = StatusStage::default();
    if data_len.is_some() && data_in {
        status_trb.clear_direction();
    } else {
        status_trb.set_direction();
    }
    status_trb.set_interrupt_on_completion();
    let status_trb_dma = enqueue_trb(ring, status_trb.into_raw())?;
    ring_doorbell(controller, device.slot_id, 1);

    let event = wait_for_event(controller, |event| match event {
        XhciEvent::Transfer {
            transfer_trb_dma,
            slot_id,
            ..
        } if transfer_trb_dma == status_trb_dma && slot_id == device.slot_id => true,
        _ => false,
    });

    match event {
        None => {
            crate::debug::println!(
                "xhci control transfer timed out: slot={} req_type={:#x} req={:#x} value={:#x} index={:#x} len={} dir_in={} status_trb={:#x}",
                device.slot_id,
                setup.request_type,
                setup.request,
                setup.value,
                setup.index,
                data_len.unwrap_or(0),
                data_in,
                status_trb_dma,
            );
            None
        }
        Some(event) => match event {
            XhciEvent::Transfer {
                completion_code,
                residual_len,
                ..
            } if completion_code == XHCI_COMP_SUCCESS
                || completion_code == XHCI_COMP_SHORT_PACKET =>
            {
                let requested = data_len.unwrap_or(0);
                Some(requested.saturating_sub(residual_len as usize))
            }
            XhciEvent::Transfer {
                completion_code, ..
            } => {
                crate::debug::println!(
                    "xhci control transfer error: slot={} code={} req_type={:#x} req={:#x} value={:#x} index={:#x} len={} dir_in={}",
                    device.slot_id,
                    completion_code,
                    setup.request_type,
                    setup.request,
                    setup.value,
                    setup.index,
                    data_len.unwrap_or(0),
                    data_in,
                );
                None
            }
            other => {
                crate::debug::println!(
                    "xhci control transfer unexpected event: slot={} req={:#x} event={:?}",
                    device.slot_id,
                    setup.request,
                    other,
                );
                None
            }
        },
    }
}

fn submit_interrupt_poll(
    registers: &mut XhciRegisters<XhciMmioMapper>,
    device: &mut XhciEnumeratedDevice,
) -> bool {
    let request_len = device.endpoint_max_packet_size as usize;
    if XHCI_POLL_SUBMIT_BEGIN_LOGS.fetch_add(1, Ordering::Relaxed) < XHCI_POLL_SUBMIT_LOG_LIMIT {
        crate::debug::println!(
            "xhci poll submit begin: slot={} ep_id={} req_len={} ring_index={} cycle={}",
            device.slot_id,
            endpoint_id_for_index(device.ep_index),
            request_len,
            device.interrupt_ring.enqueue_index,
            device.interrupt_ring.cycle_state
        );
    }
    if request_len == 0 || request_len > device.poll_buffer.size {
        crate::debug::println!(
            "xhci poll submit invalid request: slot={} ep_id={} req_len={} poll_buffer_size={} ring_trbs={} ring_buffer_size={} enqueue_index={} cycle={} pending_trb={:#x}",
            device.slot_id,
            endpoint_id_for_index(device.ep_index),
            request_len,
            device.poll_buffer.size,
            device.interrupt_ring.trb_count,
            device.interrupt_ring.buffer.size,
            device.interrupt_ring.enqueue_index,
            device.interrupt_ring.cycle_state,
            device.pending_poll_trb_dma
        );
        return false;
    }
    let mut trb = Normal::default();
    trb.set_data_buffer_pointer(device.poll_buffer.dma_addr)
        .set_trb_transfer_length(request_len as u32)
        .set_interrupt_on_completion()
        .set_interrupt_on_short_packet();
    let mut trb_dma = enqueue_trb(&mut device.interrupt_ring, trb.into_raw());
    if trb_dma.is_none() {
        crate::debug::println!(
            "xhci poll submit enqueue failed: slot={} ep_id={} req_len={} poll_buffer_size={} ring_trbs={} ring_buffer_size={} enqueue_index={} cycle={} pending_trb={:#x}",
            device.slot_id,
            endpoint_id_for_index(device.ep_index),
            request_len,
            device.poll_buffer.size,
            device.interrupt_ring.trb_count,
            device.interrupt_ring.buffer.size,
            device.interrupt_ring.enqueue_index,
            device.interrupt_ring.cycle_state,
            device.pending_poll_trb_dma
        );
        if recover_transfer_ring(&mut device.interrupt_ring, XHCI_TRANSFER_RING_TRBS) {
            crate::debug::println!(
                "xhci poll submit ring recovered: slot={} ep_id={} ring_trbs={} ring_buffer_size={} enqueue_index={} cycle={}",
                device.slot_id,
                endpoint_id_for_index(device.ep_index),
                device.interrupt_ring.trb_count,
                device.interrupt_ring.buffer.size,
                device.interrupt_ring.enqueue_index,
                device.interrupt_ring.cycle_state
            );
            trb_dma = enqueue_trb(&mut device.interrupt_ring, trb.into_raw());
        }
    }
    let Some(trb_dma) = trb_dma else {
        return false;
    };
    device.pending_poll_trb_dma = trb_dma;
    if XHCI_POLL_SUBMIT_QUEUED_LOGS.fetch_add(1, Ordering::Relaxed) < XHCI_POLL_SUBMIT_LOG_LIMIT {
        crate::debug::println!(
            "xhci poll submit queued: slot={} ep_id={} trb={:#x} next_ring_index={} cycle={}",
            device.slot_id,
            endpoint_id_for_index(device.ep_index),
            trb_dma,
            device.interrupt_ring.enqueue_index,
            device.interrupt_ring.cycle_state
        );
    }
    ring_doorbell_at(
        registers,
        device.slot_id,
        endpoint_id_for_index(device.ep_index),
    );
    if XHCI_POLL_SUBMIT_DONE_LOGS.fetch_add(1, Ordering::Relaxed) < XHCI_POLL_SUBMIT_LOG_LIMIT {
        crate::debug::println!(
            "xhci poll submit done: slot={} ep_id={} trb={:#x}",
            device.slot_id,
            endpoint_id_for_index(device.ep_index),
            trb_dma
        );
    }
    true
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
fn command_enable_slot(controller: &mut XhciControllerState) -> Option<u8> {
    let trb_dma = enqueue_trb(
        &mut controller.command_ring,
        EnableSlotCommand::default().into_raw(),
    )?;
    ring_doorbell(controller, 0, 0);
    match wait_for_event(controller, |event| match event {
        XhciEvent::CommandCompletion {
            command_trb_dma, ..
        } if command_trb_dma == trb_dma => true,
        _ => false,
    }) {
        None => {
            crate::debug::println!("xhci enable-slot timed out: command_trb={:#x}", trb_dma);
            None
        }
        Some(event) => match event {
            XhciEvent::CommandCompletion {
                completion_code,
                slot_id,
                ..
            } if completion_code == XHCI_COMP_SUCCESS => Some(slot_id),
            XhciEvent::CommandCompletion {
                completion_code, ..
            } => {
                crate::debug::println!(
                    "xhci enable-slot failed: command_trb={:#x} code={}",
                    trb_dma,
                    completion_code,
                );
                None
            }
            other => {
                crate::debug::println!(
                    "xhci enable-slot unexpected event: command_trb={:#x} event={:?}",
                    trb_dma,
                    other,
                );
                None
            }
        },
    }
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
fn command_address_device(
    controller: &mut XhciControllerState,
    slot_id: u8,
    input_context_dma: u64,
) -> bool {
    let mut trb = AddressDeviceCommand::default();
    trb.set_input_context_pointer(input_context_dma)
        .set_slot_id(slot_id);
    let Some(trb_dma) = enqueue_trb(&mut controller.command_ring, trb.into_raw()) else {
        return false;
    };
    ring_doorbell(controller, 0, 0);
    match wait_for_event(controller, |event| match event {
        XhciEvent::CommandCompletion {
            command_trb_dma, ..
        } if command_trb_dma == trb_dma => true,
        _ => false,
    }) {
        Some(XhciEvent::CommandCompletion {
            completion_code: XHCI_COMP_SUCCESS,
            ..
        }) => true,
        Some(XhciEvent::CommandCompletion {
            completion_code, ..
        }) => {
            crate::debug::println!(
                "xhci address-device completion failed: slot={} input_ctx={:#x} command_trb={:#x} code={}",
                slot_id,
                input_context_dma,
                trb_dma,
                completion_code,
            );
            false
        }
        Some(other) => {
            crate::debug::println!(
                "xhci address-device unexpected event: slot={} input_ctx={:#x} command_trb={:#x} event={:?}",
                slot_id,
                input_context_dma,
                trb_dma,
                other,
            );
            false
        }
        None => {
            crate::debug::println!(
                "xhci address-device timed out: slot={} input_ctx={:#x} command_trb={:#x}",
                slot_id,
                input_context_dma,
                trb_dma,
            );
            false
        }
    }
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
fn command_configure_endpoint(
    controller: &mut XhciControllerState,
    slot_id: u8,
    input_context_dma: u64,
) -> bool {
    let mut trb = ConfigureEndpointCommand::default();
    trb.set_input_context_pointer(input_context_dma)
        .set_slot_id(slot_id);
    let Some(trb_dma) = enqueue_trb(&mut controller.command_ring, trb.into_raw()) else {
        return false;
    };
    ring_doorbell(controller, 0, 0);
    match wait_for_event(controller, |event| match event {
        XhciEvent::CommandCompletion {
            command_trb_dma, ..
        } if command_trb_dma == trb_dma => true,
        _ => false,
    }) {
        Some(XhciEvent::CommandCompletion {
            completion_code: XHCI_COMP_SUCCESS,
            ..
        }) => true,
        Some(XhciEvent::CommandCompletion {
            completion_code, ..
        }) => {
            crate::debug::println!(
                "xhci configure-endpoint completion failed: slot={} input_ctx={:#x} command_trb={:#x} code={}",
                slot_id,
                input_context_dma,
                trb_dma,
                completion_code,
            );
            false
        }
        Some(other) => {
            crate::debug::println!(
                "xhci configure-endpoint unexpected event: slot={} input_ctx={:#x} command_trb={:#x} event={:?}",
                slot_id,
                input_context_dma,
                trb_dma,
                other,
            );
            false
        }
        None => {
            crate::debug::println!(
                "xhci configure-endpoint timed out: slot={} input_ctx={:#x} command_trb={:#x}",
                slot_id,
                input_context_dma,
                trb_dma,
            );
            false
        }
    }
}

fn wait_for_event(
    controller: &mut XhciControllerState,
    matcher: impl Fn(XhciEvent) -> bool,
) -> Option<XhciEvent> {
    for _ in 0..XHCI_EVENT_WAIT_SPINS {
        if let Some(event) = poll_event(controller) {
            if matcher(event) {
                return Some(event);
            }
            handle_async_event(controller, event);
        }
        spin_loop();
    }
    None
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
fn poll_event(controller: &mut XhciControllerState) -> Option<XhciEvent> {
    let event_ring = dma_trb_slice_mut(&controller.event_ring, XHCI_EVENT_RING_TRBS)?;
    let trb = event_ring
        .get(controller.event_ring_dequeue_index)
        .copied()?;
    let cycle = (trb[3] & XHCI_TRB_CYCLE_BIT) != 0;
    if cycle != controller.event_ring_cycle_state {
        return None;
    }

    let event = match EventTrb::try_from(trb) {
        Ok(EventTrb::PortStatusChange(event)) => XhciEvent::PortStatusChange {
            port_id: event.port_id() as usize,
        },
        Ok(EventTrb::CommandCompletion(event)) => XhciEvent::CommandCompletion {
            command_trb_dma: event.command_trb_pointer(),
            completion_code: event
                .completion_code()
                .map(|code| code as u8)
                .unwrap_or_else(|code| code),
            slot_id: event.slot_id(),
        },
        Ok(EventTrb::TransferEvent(event)) => XhciEvent::Transfer {
            transfer_trb_dma: event.trb_pointer(),
            completion_code: event
                .completion_code()
                .map(|code| code as u8)
                .unwrap_or_else(|code| code),
            slot_id: event.slot_id(),
            ep_id: event.endpoint_id(),
            residual_len: event.trb_transfer_length(),
        },
        Ok(event) => {
            let raw = event.into_raw();
            if let Some(event) = parse_event_raw(raw) {
                advance_event_ring(controller);
                return Some(event);
            }
            let trb_type = (raw[3] & XHCI_TRB_TYPE_MASK) >> XHCI_TRB_TYPE_SHIFT;
            crate::debug::println!(
                "xhci event: type={} raw={:#010x}:{:#010x}:{:#010x}:{:#010x}",
                trb_type,
                raw[0],
                raw[1],
                raw[2],
                raw[3],
            );
            advance_event_ring(controller);
            return None;
        }
        Err(raw) => {
            if let Some(event) = parse_event_raw(raw) {
                advance_event_ring(controller);
                return Some(event);
            }
            let trb_type = (raw[3] & XHCI_TRB_TYPE_MASK) >> XHCI_TRB_TYPE_SHIFT;
            crate::debug::println!(
                "xhci event: type={} raw={:#010x}:{:#010x}:{:#010x}:{:#010x}",
                trb_type,
                raw[0],
                raw[1],
                raw[2],
                raw[3],
            );
            advance_event_ring(controller);
            return None;
        }
    };

    advance_event_ring(controller);
    Some(event)
}

fn parse_event_raw(raw: XhciTrb) -> Option<XhciEvent> {
    let trb_type = (raw[3] & XHCI_TRB_TYPE_MASK) >> XHCI_TRB_TYPE_SHIFT;
    match trb_type {
        32 => Some(XhciEvent::Transfer {
            transfer_trb_dma: (u64::from(raw[1]) << 32) | u64::from(raw[0]),
            completion_code: ((raw[2] >> 24) & 0xff) as u8,
            slot_id: ((raw[3] >> 24) & 0xff) as u8,
            ep_id: ((raw[3] >> 16) & 0x1f) as u8,
            residual_len: raw[2] & 0x00ff_ffff,
        }),
        33 => Some(XhciEvent::CommandCompletion {
            command_trb_dma: (u64::from(raw[1]) << 32) | u64::from(raw[0]),
            completion_code: ((raw[2] >> 24) & 0xff) as u8,
            slot_id: ((raw[3] >> 24) & 0xff) as u8,
        }),
        34 => Some(XhciEvent::PortStatusChange {
            port_id: ((raw[0] >> 24) & 0xff) as usize,
        }),
        _ => None,
    }
}

fn advance_event_ring(controller: &mut XhciControllerState) {
    controller.event_ring_dequeue_index += 1;
    if controller.event_ring_dequeue_index >= XHCI_EVENT_RING_TRBS {
        controller.event_ring_dequeue_index = 0;
        controller.event_ring_cycle_state = !controller.event_ring_cycle_state;
    }
    let dequeue_addr = controller.event_ring.dma_addr
        + (controller.event_ring_dequeue_index * XHCI_TRB_BYTES) as u64;
    let mut interrupter0 = controller
        .registers
        .interrupter_register_set
        .interrupter_mut(0);
    interrupter0.erdp.update_volatile(|erdp| {
        erdp.set_event_ring_dequeue_pointer(dequeue_addr);
        erdp.clear_event_handler_busy();
    });
}

fn enqueue_trb(ring: &mut XhciRing, mut trb: [u32; 4]) -> Option<u64> {
    let trbs = dma_trb_slice_mut(&ring.buffer, ring.trb_count)?;
    if ring.enqueue_index >= ring.trb_count.saturating_sub(1) {
        if let Some(link) = trbs.get_mut(ring.trb_count - 1) {
            link[3] = (link[3] & !XHCI_TRB_CYCLE_BIT)
                | if ring.cycle_state {
                    XHCI_TRB_CYCLE_BIT
                } else {
                    0
                };
        }
        ring.enqueue_index = 0;
        ring.cycle_state = !ring.cycle_state;
    }

    let index = ring.enqueue_index;
    let trb_dma = ring.buffer.dma_addr + (index * XHCI_TRB_BYTES) as u64;
    trb[3] = (trb[3] & !XHCI_TRB_CYCLE_BIT)
        | if ring.cycle_state {
            XHCI_TRB_CYCLE_BIT
        } else {
            0
        };
    trbs[index] = trb;
    ring.enqueue_index += 1;
    Some(trb_dma)
}

fn recover_transfer_ring(ring: &mut XhciRing, expected_trb_count: usize) -> bool {
    let bytes = expected_trb_count.saturating_mul(XHCI_TRB_BYTES);
    if bytes == 0 || bytes > ring.buffer.size {
        return false;
    }
    let Some(trbs) = dma_trb_slice_mut(&ring.buffer, expected_trb_count) else {
        return false;
    };
    for trb in trbs.iter_mut() {
        *trb = [0; 4];
    }
    ring.trb_count = expected_trb_count;
    ring.enqueue_index = 0;
    ring.cycle_state = true;
    prime_ring_link(ring).is_some()
}

fn prime_ring_link(ring: &mut XhciRing) -> Option<()> {
    let trbs = dma_trb_slice_mut(&ring.buffer, ring.trb_count)?;
    let link = trbs.last_mut()?;
    let mut trb = LinkTrb::default();
    trb.set_ring_segment_pointer(ring.buffer.dma_addr)
        .set_toggle_cycle()
        .set_cycle_bit();
    *link = trb.into_raw();
    Some(())
}

fn xhci_stop_controller(controller: &mut XhciControllerState) -> bool {
    controller
        .registers
        .operational
        .usbcmd
        .update_volatile(|command| {
            command.clear_run_stop();
        });
    wait_until(|| {
        controller
            .registers
            .operational
            .usbsts
            .read_volatile()
            .hc_halted()
    })
}

fn xhci_reset_controller(controller: &mut XhciControllerState) -> bool {
    controller
        .registers
        .operational
        .usbcmd
        .update_volatile(|command| {
            command.set_host_controller_reset();
        });
    if !wait_until(|| {
        !controller
            .registers
            .operational
            .usbcmd
            .read_volatile()
            .host_controller_reset()
    }) {
        return false;
    }
    wait_until(|| {
        !controller
            .registers
            .operational
            .usbsts
            .read_volatile()
            .controller_not_ready()
    })
}

fn xhci_program_runtime_state(controller: &mut XhciControllerState) -> bool {
    let dcbaa_dma = controller.device_context_base_array.dma_addr;
    let command_ring_dma = controller.command_ring.buffer.dma_addr;
    let event_ring_table_dma = controller.event_ring_table.dma_addr;
    let event_ring_dma = controller.event_ring.dma_addr;
    let max_slots = controller.record.max_slots;

    controller
        .registers
        .operational
        .dnctrl
        .update_volatile(|dnctrl| {
            for index in 0..16 {
                dnctrl.clear(index);
            }
        });
    controller
        .registers
        .operational
        .dcbaap
        .update_volatile(|dcbaap| {
            dcbaap.set(dcbaa_dma);
        });
    controller
        .registers
        .operational
        .crcr
        .update_volatile(|crcr| {
            crcr.set_command_ring_pointer(command_ring_dma);
            crcr.set_ring_cycle_state();
        });

    {
        let mut interrupter0 = controller
            .registers
            .interrupter_register_set
            .interrupter_mut(0);
        interrupter0.imod.write_volatile(Default::default());
        interrupter0.erstsz.update_volatile(|erstsz| {
            erstsz.set(1);
        });
        interrupter0.erstba.update_volatile(|erstba| {
            erstba.set(event_ring_table_dma);
        });
        interrupter0.erdp.update_volatile(|erdp| {
            erdp.set_event_ring_dequeue_pointer(event_ring_dma);
            erdp.set_0_event_handler_busy();
        });
        interrupter0.iman.update_volatile(|iman| {
            iman.set_0_interrupt_pending();
            iman.set_interrupt_enable();
        });
    }

    controller
        .registers
        .operational
        .config
        .update_volatile(|config| {
            config.set_max_device_slots_enabled(max_slots);
        });
    true
}

fn xhci_run_controller(controller: &mut XhciControllerState) -> bool {
    controller
        .registers
        .operational
        .usbcmd
        .update_volatile(|command| {
            command.set_run_stop();
            command.set_interrupter_enable();
        });
    wait_until(|| {
        !controller
            .registers
            .operational
            .usbsts
            .read_volatile()
            .hc_halted()
    })
}

fn xhci_power_ports(controller: &mut XhciControllerState) {
    for port in 0..usize::from(controller.record.max_ports) {
        let portsc = controller
            .registers
            .port_register_set
            .read_volatile_at(port)
            .portsc;
        if portsc.port_power() {
            continue;
        }
        controller
            .registers
            .port_register_set
            .update_volatile_at(port, |set| {
                set.portsc.set_port_power();
                clear_port_change_bits(&mut set.portsc);
            });
    }
}

fn ring_doorbell(controller: &mut XhciControllerState, slot_id: u8, target: usize) {
    let Some(doorbell) = build_doorbell_register(slot_id, target) else {
        return;
    };
    controller
        .registers
        .doorbell
        .write_volatile_at(slot_id as usize, doorbell);
}

fn ring_doorbell_at(registers: &mut XhciRegisters<XhciMmioMapper>, slot_id: u8, target: usize) {
    let Some(doorbell) = build_doorbell_register(slot_id, target) else {
        return;
    };
    registers
        .doorbell
        .write_volatile_at(slot_id as usize, doorbell);
}

fn build_doorbell_register(slot_id: u8, target: usize) -> Option<DoorbellRegister> {
    let target = match u8::try_from(target) {
        Ok(target) => target,
        Err(_) => {
            crate::debug::println!(
                "xhci: doorbell target overflow: slot={} target={}",
                slot_id,
                target
            );
            return None;
        }
    };

    let mut doorbell = DoorbellRegister::default();
    doorbell.set_doorbell_target(target);
    // The xhci crate's stream-id setter asserts in debug-style builds even for zero.
    // Leaving the default register value intact keeps stream-id at 0, which is what we want.
    Some(doorbell)
}

fn parse_hid_interface(config_desc: &[u8]) -> Option<ParsedHidInterface> {
    if config_desc.len() < 9 || config_desc[1] != USB_DT_CONFIG {
        return None;
    }

    let configuration_value = config_desc[5];
    let mut offset = config_desc[0] as usize;
    let mut current_interface: Option<(u8, u8, u8, u8)> = None;
    let mut current_hid: Option<Vec<u8>> = None;

    while offset + 2 <= config_desc.len() {
        let len = config_desc[offset] as usize;
        if len < 2 || offset + len > config_desc.len() {
            break;
        }
        let descriptor_type = config_desc[offset + 1];
        let bytes = &config_desc[offset..offset + len];

        match descriptor_type {
            USB_DT_INTERFACE if len >= 9 => {
                current_interface = Some((bytes[2], bytes[5], bytes[6], bytes[7]));
                current_hid = None;
            }
            USB_DT_HID if current_interface.is_some() => {
                current_hid = Some(bytes.to_vec());
            }
            USB_DT_ENDPOINT if len >= 7 => {
                let Some((
                    interface_number,
                    interface_class,
                    interface_sub_class,
                    interface_protocol,
                )) = current_interface
                else {
                    offset += len;
                    continue;
                };
                if interface_class != USB_CLASS_HID {
                    offset += len;
                    continue;
                }
                if (bytes[3] & 0x3) != USB_ENDPOINT_XFER_INT || (bytes[2] & USB_DIR_IN) == 0 {
                    offset += len;
                    continue;
                }
                let hid_descriptor = current_hid.clone()?;
                let report_descriptor_len = hid_report_descriptor_len(&hid_descriptor)?;
                return Some(ParsedHidInterface {
                    configuration_value,
                    interface_number,
                    interface_class,
                    interface_sub_class,
                    interface_protocol,
                    hid_descriptor,
                    report_descriptor_len,
                    endpoint_address: bytes[2],
                    endpoint_max_packet_size: u16::from_le_bytes([bytes[4], bytes[5]]) & 0x07ff,
                    endpoint_interval: bytes[6].max(1),
                });
            }
            _ => {}
        }

        offset += len;
    }

    None
}

fn hid_report_descriptor_len(hid_descriptor: &[u8]) -> Option<u16> {
    if hid_descriptor.len() < 9 || hid_descriptor[1] != USB_DT_HID {
        return None;
    }
    Some(u16::from_le_bytes([hid_descriptor[7], hid_descriptor[8]]))
}

fn endpoint_index_from_address(endpoint_address: u8) -> usize {
    let endpoint_number = usize::from(endpoint_address & 0x0f);
    if endpoint_number == 0 {
        return 0;
    }
    if (endpoint_address & USB_DIR_IN) != 0 {
        endpoint_number * 2
    } else {
        endpoint_number * 2 - 1
    }
}

fn endpoint_id_for_index(ep_index: usize) -> usize {
    ep_index + 1
}

fn endpoint_interval_value(speed: u8, interval: u8) -> u32 {
    match speed {
        1 | 2 => ceil_log2(u32::from(interval.max(1)) * 8).max(3),
        _ => u32::from(interval.saturating_sub(1)),
    }
}

fn ceil_log2(value: u32) -> u32 {
    if value <= 1 {
        return 0;
    }
    32 - (value - 1).leading_zeros() - 1
}

fn default_control_max_packet(speed: u8) -> u16 {
    match speed {
        1 | 2 => 8,
        3 => 64,
        _ => 512,
    }
}

fn port_speed_to_usb_speed(speed: u8) -> u32 {
    match speed {
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 5,
        _ => 0,
    }
}

fn zero_dma_words(buffer: &XhciDmaBuffer) {
    unsafe {
        core::ptr::write_bytes(buffer.cpu_ptr, 0, buffer.size);
    }
}

fn populate_address_device_context<T: InputHandler>(
    input: &mut T,
    root_port: usize,
    speed: u8,
    max_packet0: u16,
    ep0_ring_dma: u64,
) {
    let control = input.control_mut();
    control.set_add_context_flag(0);
    control.set_add_context_flag(1);

    let device = input.device_mut();
    let slot = device.slot_mut();
    slot.set_speed(speed);
    slot.set_context_entries(1);
    slot.set_root_hub_port_number(root_port as u8);

    let ep0 = device.endpoint_mut(1);
    ep0.set_error_count(3);
    ep0.set_endpoint_type(EndpointType::Control);
    ep0.set_max_packet_size(max_packet0);
    ep0.set_tr_dequeue_pointer(ep0_ring_dma);
    ep0.set_dequeue_cycle_state();
    ep0.set_average_trb_length(8);
}

#[allow(clippy::too_many_arguments)]
fn populate_interrupt_endpoint_context<T: InputHandler>(
    input: &mut T,
    root_port: usize,
    speed: u8,
    max_packet0: u16,
    ep0_ring_dma: u64,
    ep_index: usize,
    interrupt_ring_dma: u64,
    max_packet: u16,
    interval: u8,
) {
    populate_address_device_context(input, root_port, speed, max_packet0, ep0_ring_dma);

    let control = input.control_mut();
    control.clear_add_context_flag(1);
    control.set_add_context_flag(ep_index + 1);

    let device = input.device_mut();
    let slot = device.slot_mut();
    slot.set_context_entries((ep_index + 1) as u8);

    let ep = device.endpoint_mut(ep_index + 1);
    ep.set_interval(endpoint_interval_value(speed, interval) as u8);
    ep.set_error_count(3);
    ep.set_endpoint_type(EndpointType::InterruptIn);
    ep.set_max_packet_size(max_packet);
    ep.set_tr_dequeue_pointer(interrupt_ring_dma);
    ep.set_dequeue_cycle_state();
    ep.set_average_trb_length(max_packet);
    ep.set_max_endpoint_service_time_interval_payload_low(max_packet);
}

fn write_input_context<T>(buffer: &XhciDmaBuffer, input: &T) {
    let bytes = size_of::<T>();
    debug_assert!(bytes <= buffer.size);
    unsafe {
        ptr::copy_nonoverlapping((input as *const T).cast::<u8>(), buffer.cpu_ptr, bytes);
    }
}

fn dma_buffer_alloc(device_ptr: *mut c_void, size: usize) -> Option<XhciDmaBuffer> {
    let rounded = size.next_multiple_of(4096).max(4096);
    let mut dma_addr = 0u64;
    let cpu_ptr = crate::driver::dma::alloc_coherent(device_ptr, rounded, &mut dma_addr);
    if cpu_ptr.is_null() {
        return None;
    }
    Some(XhciDmaBuffer {
        cpu_ptr: cpu_ptr.cast(),
        dma_addr,
        size: rounded,
    })
}

fn dma_trb_slice_mut(buffer: &XhciDmaBuffer, count: usize) -> Option<&'static mut [XhciTrb]> {
    let bytes = count.checked_mul(XHCI_TRB_BYTES)?;
    if bytes > buffer.size {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts_mut(buffer.cpu_ptr.cast::<XhciTrb>(), count) })
}

fn dma_erst_slice_mut(
    buffer: &XhciDmaBuffer,
    count: usize,
) -> Option<&'static mut [XhciEventRingSegmentTableEntry]> {
    let bytes = count.checked_mul(size_of::<XhciEventRingSegmentTableEntry>())?;
    if bytes > buffer.size {
        return None;
    }
    Some(unsafe {
        core::slice::from_raw_parts_mut(
            buffer.cpu_ptr.cast::<XhciEventRingSegmentTableEntry>(),
            count,
        )
    })
}

fn dma_u64_slice_mut(buffer: &XhciDmaBuffer, count: usize) -> Option<&'static mut [u64]> {
    let bytes = count.checked_mul(size_of::<u64>())?;
    if bytes > buffer.size {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts_mut(buffer.cpu_ptr.cast::<u64>(), count) })
}

fn log_port_snapshot(controller: &mut XhciControllerState, initial: bool) -> usize {
    let mut work = 0usize;
    for port in 0..usize::from(controller.record.max_ports) {
        let portsc = controller
            .registers
            .port_register_set
            .read_volatile_at(port)
            .portsc;
        let portsc_raw = portsc_raw(portsc);
        let previous = controller.last_portsc[port];
        if initial || portsc_raw != previous {
            controller.last_portsc[port] = portsc_raw;
            work += 1;
        }
    }
    work
}

#[cfg(test)]
mod tests {
    use super::*;
    use xhci::ring::trb::command::Allowed as CommandTrb;

    #[test]
    fn builds_address_context_with_xhci_crate_types() {
        let mut input = Input32Byte::new_32byte();
        populate_address_device_context(&mut input, 5, 3, 64, 0x2000);

        assert!(input.control().add_context_flag(0));
        assert!(input.control().add_context_flag(1));
        assert_eq!(input.device().slot().root_hub_port_number(), 5);
        assert_eq!(input.device().slot().speed(), 3);
        assert_eq!(input.device().slot().context_entries(), 1);

        let ep0 = input.device().endpoint(1);
        assert_eq!(ep0.endpoint_type(), EndpointType::Control);
        assert_eq!(ep0.max_packet_size(), 64);
        assert_eq!(ep0.tr_dequeue_pointer() & !1, 0x2000);
        assert!(ep0.dequeue_cycle_state());
        assert_eq!(ep0.average_trb_length(), 8);
    }

    #[test]
    fn decodes_transfer_event_with_xhci_crate_types() {
        let raw = [
            0x89ab_cdef,
            0x0123_4567,
            ((CompletionCode::ShortPacket as u32) << 24) | 0x1234,
            (32u32 << XHCI_TRB_TYPE_SHIFT) | (3u32 << 16) | (2u32 << 24) | 1,
        ];

        let event = EventTrb::try_from(raw).expect("valid event trb");
        let EventTrb::TransferEvent(event) = event else {
            panic!("expected transfer event");
        };

        assert_eq!(event.trb_pointer(), 0x0123_4567_89ab_cdef);
        assert_eq!(event.endpoint_id(), 3);
        assert_eq!(event.slot_id(), 2);
        assert_eq!(event.trb_transfer_length(), 0x1234);
        assert_eq!(event.completion_code(), Ok(CompletionCode::ShortPacket));
    }

    #[test]
    fn falls_back_to_manual_transfer_event_decode() {
        let raw = [
            0x89ab_cdef,
            0x0123_4567,
            ((CompletionCode::ShortPacket as u32) << 24) | 0x1234,
            (32u32 << XHCI_TRB_TYPE_SHIFT) | (3u32 << 16) | (2u32 << 24),
        ];

        match parse_event_raw(raw) {
            Some(XhciEvent::Transfer {
                transfer_trb_dma,
                completion_code,
                slot_id,
                ep_id,
                residual_len,
            }) => {
                assert_eq!(transfer_trb_dma, 0x0123_4567_89ab_cdef);
                assert_eq!(completion_code, CompletionCode::ShortPacket as u8);
                assert_eq!(slot_id, 2);
                assert_eq!(ep_id, 3);
                assert_eq!(residual_len, 0x1234);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn encodes_command_trbs_with_xhci_crate_types() {
        let mut trb = AddressDeviceCommand::default();
        trb.set_input_context_pointer(0x4000).set_slot_id(7);

        let raw = trb.into_raw();
        let decoded = CommandTrb::try_from(raw).expect("valid command trb");
        let CommandTrb::AddressDevice(decoded) = decoded else {
            panic!("expected address-device command");
        };

        assert_eq!(decoded.input_context_pointer(), 0x4000);
        assert_eq!(decoded.slot_id(), 7);
    }
}
