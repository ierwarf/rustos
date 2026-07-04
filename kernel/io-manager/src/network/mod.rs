use core::ffi::c_void;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::sync::KernelWaitLock;

pub const PACKET_MTU: usize = rustos_user_abi::syscall::NET_BROKER_PACKET_MTU;

const PCI_VENDOR_VIRTIO: u16 = 0x1af4;
const PCI_DEVICE_VIRTIO_NET_TRANSITIONAL: u16 = 0x1000;
const PCI_DEVICE_VIRTIO_MODERN_BASE: u16 = 0x1040;
const PCI_DEVICE_VIRTIO_MODERN_END: u16 = 0x107f;
const VIRTIO_DEVICE_ID_NET: u32 = 1;

const PCI_STATUS_OFFSET: u8 = 0x06;
const PCI_CAPABILITY_LIST: u16 = 1 << 4;
const PCI_CAPABILITY_POINTER_OFFSET: u8 = 0x34;
const PCI_CAP_ID_VENDOR: u8 = 0x09;

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;

const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;
const VIRTIO_STATUS_FEATURES_OK: u8 = 8;
const VIRTIO_STATUS_FAILED: u8 = 0x80;
const VIRTIO_F_VERSION_1_WORD: u32 = 1;

const COMMON_DEVICE_FEATURE_SELECT: usize = 0;
const COMMON_DEVICE_FEATURE: usize = 4;
const COMMON_DRIVER_FEATURE_SELECT: usize = 8;
const COMMON_DRIVER_FEATURE: usize = 12;
const COMMON_DEVICE_STATUS: usize = 20;
const COMMON_QUEUE_SELECT: usize = 22;
const COMMON_QUEUE_SIZE: usize = 24;
const COMMON_QUEUE_ENABLE: usize = 28;
const COMMON_QUEUE_NOTIFY_OFF: usize = 30;
const COMMON_QUEUE_DESC: usize = 32;
const COMMON_QUEUE_DRIVER: usize = 40;
const COMMON_QUEUE_DEVICE: usize = 48;

const QUEUE_SIZE: u16 = 8;
const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;
const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;
const VIRTIO_NET_HDR_LEN: usize = 10;
const RX_DMA_LEN: usize = VIRTIO_NET_HDR_LEN + PACKET_MTU;
const TX_DMA_LEN: usize = VIRTIO_NET_HDR_LEN + PACKET_MTU;
const TX_POLL_BUDGET: usize = 100_000;

static CURRENT_TRANSPORT: AtomicUsize = AtomicUsize::new(0);
static STATE: KernelWaitLock<Option<VirtioNetState>> = KernelWaitLock::new(None);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LinuxNetdevTransport {
    Unknown,
    Pci,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketError {
    NoDevice,
    Invalid,
    Busy,
    TooLarge,
    WouldBlock,
}

pub(crate) fn note_virtio_net_driver_registered() {}

pub(crate) fn current_linux_netdev_transport() -> LinuxNetdevTransport {
    match CURRENT_TRANSPORT.load(Ordering::Acquire) {
        1 => LinuxNetdevTransport::Pci,
        _ => LinuxNetdevTransport::Unknown,
    }
}

pub(crate) fn set_current_linux_netdev_transport(transport: LinuxNetdevTransport) {
    let value = match transport {
        LinuxNetdevTransport::Unknown => 0,
        LinuxNetdevTransport::Pci => 1,
    };
    CURRENT_TRANSPORT.store(value, Ordering::Release);
}

pub(crate) fn register_linux_netdev(_dev: usize, _transport: LinuxNetdevTransport) -> i32 {
    0
}

pub(crate) fn unregister_linux_netdev(_dev: usize) {}

pub(crate) fn allocate_linux_netdev(_dev: usize, _sizeof_priv: usize, _txqs: u32, _rxqs: u32) {}

pub(crate) fn free_linux_netdev(_dev: usize) {}

pub(crate) fn set_linux_netdev_carrier(_dev: usize, _carrier: bool) {}

pub fn available() -> bool {
    ensure_state().is_ok()
}

pub fn transmit_frame(frame: &[u8]) -> Result<usize, PacketError> {
    if frame.is_empty() {
        return Ok(0);
    }
    if frame.len() > PACKET_MTU {
        return Err(PacketError::TooLarge);
    }
    ensure_state()?;
    let mut guard = STATE.lock();
    let state = guard.as_mut().ok_or(PacketError::NoDevice)?;
    state.transmit_frame(frame)?;
    Ok(frame.len())
}

pub fn receive_frame(out: &mut [u8]) -> Result<usize, PacketError> {
    if out.is_empty() {
        return Err(PacketError::Invalid);
    }
    ensure_state()?;
    let mut guard = STATE.lock();
    let state = guard.as_mut().ok_or(PacketError::NoDevice)?;
    state.receive_frame(out)
}

fn ensure_state() -> Result<(), PacketError> {
    if STATE.lock().is_some() {
        return Ok(());
    }
    let mut guard = STATE.lock();
    if guard.is_none() {
        *guard = Some(initialize_virtio_net()?);
    }
    Ok(())
}

struct VirtioNetState {
    _pci: crate::arch::pci::PciDevice,
    common: MmioRegion,
    notify: MmioRegion,
    notify_multiplier: u32,
    rx_notify_off: u16,
    tx_notify_off: u16,
    rx: VirtQueue,
    tx: VirtQueue,
    rx_buffer: DmaBlock,
    tx_buffer: DmaBlock,
    rx_posted: bool,
}

unsafe impl Send for VirtioNetState {}

struct MmioRegion {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for MmioRegion {}

struct DmaBlock {
    cpu: *mut u8,
    dma: u64,
    len: usize,
}

unsafe impl Send for DmaBlock {}

impl Drop for DmaBlock {
    fn drop(&mut self) {
        if self.cpu.is_null() {
            return;
        }
        crate::driver::dma::free_coherent(
            core::ptr::null_mut(),
            self.cpu.cast::<c_void>(),
            self.dma,
        );
    }
}

struct VirtQueue {
    desc: DmaBlock,
    avail: DmaBlock,
    used: DmaBlock,
    next_avail: u16,
    last_used: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqUsedElem {
    id: u32,
    len: u32,
}

#[derive(Clone, Copy)]
struct VirtioPciCap {
    bar: u8,
    offset: u32,
    length: u32,
}

#[derive(Clone, Copy)]
struct VirtioNetCaps {
    common: VirtioPciCap,
    notify: VirtioPciCap,
    notify_multiplier: u32,
}

fn initialize_virtio_net() -> Result<VirtioNetState, PacketError> {
    let pci = find_virtio_net_device().ok_or(PacketError::NoDevice)?;
    pci.enable_memory_bus_master();
    crate::driver::dma::set_mask_and_coherent(core::ptr::null_mut(), u64::MAX);
    let caps = discover_caps(pci)?;
    let common = map_cap(pci, caps.common).ok_or(PacketError::NoDevice)?;
    let notify = map_cap(pci, caps.notify).ok_or(PacketError::NoDevice)?;
    let notify_multiplier = caps.notify_multiplier;

    reset_device(&common);
    write_common_u8(&common, COMMON_DEVICE_STATUS, VIRTIO_STATUS_ACKNOWLEDGE);
    write_common_u8(
        &common,
        COMMON_DEVICE_STATUS,
        VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER,
    );
    write_common_u32(&common, COMMON_DEVICE_FEATURE_SELECT, 0);
    let _ = read_common_u32(&common, COMMON_DEVICE_FEATURE);
    write_common_u32(&common, COMMON_DRIVER_FEATURE_SELECT, 0);
    write_common_u32(&common, COMMON_DRIVER_FEATURE, 0);
    write_common_u32(&common, COMMON_DEVICE_FEATURE_SELECT, 1);
    let device_features_hi = read_common_u32(&common, COMMON_DEVICE_FEATURE);
    if device_features_hi & VIRTIO_F_VERSION_1_WORD == 0 {
        write_common_u8(&common, COMMON_DEVICE_STATUS, VIRTIO_STATUS_FAILED);
        return Err(PacketError::NoDevice);
    }
    write_common_u32(&common, COMMON_DRIVER_FEATURE_SELECT, 1);
    write_common_u32(&common, COMMON_DRIVER_FEATURE, VIRTIO_F_VERSION_1_WORD);
    write_common_u8(
        &common,
        COMMON_DEVICE_STATUS,
        VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK,
    );
    if read_common_u8(&common, COMMON_DEVICE_STATUS) & VIRTIO_STATUS_FEATURES_OK == 0 {
        write_common_u8(&common, COMMON_DEVICE_STATUS, VIRTIO_STATUS_FAILED);
        return Err(PacketError::NoDevice);
    }

    let rx = setup_queue(&common, RX_QUEUE)?;
    let rx_notify_off = read_common_u16(&common, COMMON_QUEUE_NOTIFY_OFF);
    let tx = setup_queue(&common, TX_QUEUE)?;
    let tx_notify_off = read_common_u16(&common, COMMON_QUEUE_NOTIFY_OFF);
    let rx_buffer = alloc_dma(RX_DMA_LEN)?;
    let tx_buffer = alloc_dma(TX_DMA_LEN)?;
    write_common_u8(
        &common,
        COMMON_DEVICE_STATUS,
        VIRTIO_STATUS_ACKNOWLEDGE
            | VIRTIO_STATUS_DRIVER
            | VIRTIO_STATUS_FEATURES_OK
            | VIRTIO_STATUS_DRIVER_OK,
    );

    let mut state = VirtioNetState {
        _pci: pci,
        common,
        notify,
        notify_multiplier,
        rx_notify_off,
        tx_notify_off,
        rx,
        tx,
        rx_buffer,
        tx_buffer,
        rx_posted: false,
    };
    state.post_rx_buffer();
    crate::debug::info!(driver, "virtio-net: native packet substrate ready");
    Ok(state)
}

impl VirtioNetState {
    fn transmit_frame(&mut self, frame: &[u8]) -> Result<(), PacketError> {
        if self.tx.last_used != self.tx.next_avail {
            self.poll_tx_used(TX_POLL_BUDGET)?;
        }
        unsafe {
            ptr::write_bytes(self.tx_buffer.cpu, 0, VIRTIO_NET_HDR_LEN);
            ptr::copy_nonoverlapping(
                frame.as_ptr(),
                self.tx_buffer.cpu.add(VIRTIO_NET_HDR_LEN),
                frame.len(),
            );
            let desc = self.tx.desc.cpu.cast::<VirtqDesc>();
            ptr::write(
                desc,
                VirtqDesc {
                    addr: self.tx_buffer.dma,
                    len: (VIRTIO_NET_HDR_LEN + frame.len()) as u32,
                    flags: 0,
                    next: 0,
                },
            );
            push_avail(&mut self.tx, 0);
        }
        self.notify_queue(self.tx_notify_off, TX_QUEUE);
        self.poll_tx_used(TX_POLL_BUDGET)
    }

    fn receive_frame(&mut self, out: &mut [u8]) -> Result<usize, PacketError> {
        self.post_rx_buffer();
        let used_idx = unsafe { ptr::read_volatile(self.rx.used.cpu.add(2).cast::<u16>()) };
        if used_idx == self.rx.last_used {
            return Err(PacketError::WouldBlock);
        }
        let slot = (self.rx.last_used as usize) % QUEUE_SIZE as usize;
        let elem = unsafe {
            ptr::read_volatile(
                self.rx
                    .used
                    .cpu
                    .add(4 + slot * size_of::<VirtqUsedElem>())
                    .cast::<VirtqUsedElem>(),
            )
        };
        self.rx.last_used = self.rx.last_used.wrapping_add(1);
        self.rx_posted = false;
        if elem.len as usize <= VIRTIO_NET_HDR_LEN {
            self.post_rx_buffer();
            return Err(PacketError::WouldBlock);
        }
        let frame_len = (elem.len as usize - VIRTIO_NET_HDR_LEN).min(PACKET_MTU);
        if out.len() < frame_len {
            self.post_rx_buffer();
            return Err(PacketError::TooLarge);
        }
        unsafe {
            ptr::copy_nonoverlapping(
                self.rx_buffer.cpu.add(VIRTIO_NET_HDR_LEN),
                out.as_mut_ptr(),
                frame_len,
            );
        }
        self.post_rx_buffer();
        Ok(frame_len)
    }

    fn post_rx_buffer(&mut self) {
        if self.rx_posted {
            return;
        }
        unsafe {
            ptr::write_bytes(self.rx_buffer.cpu, 0, self.rx_buffer.len);
            let desc = self.rx.desc.cpu.cast::<VirtqDesc>();
            ptr::write(
                desc,
                VirtqDesc {
                    addr: self.rx_buffer.dma,
                    len: self.rx_buffer.len as u32,
                    flags: VIRTQ_DESC_F_WRITE,
                    next: 0,
                },
            );
            push_avail(&mut self.rx, 0);
        }
        self.rx_posted = true;
        self.notify_queue(self.rx_notify_off, RX_QUEUE);
    }

    fn notify_queue(&self, notify_off: u16, queue: u16) {
        let offset = (notify_off as usize).saturating_mul(self.notify_multiplier as usize);
        if offset + size_of::<u16>() > self.notify.len {
            return;
        }
        unsafe {
            ptr::write_volatile(self.notify.ptr.add(offset).cast::<u16>(), queue);
        }
    }

    fn poll_tx_used(&mut self, budget: usize) -> Result<(), PacketError> {
        for iteration in 0..budget {
            let used_idx = unsafe { ptr::read_volatile(self.tx.used.cpu.add(2).cast::<u16>()) };
            if used_idx != self.tx.last_used {
                self.tx.last_used = self.tx.last_used.wrapping_add(1);
                return Ok(());
            }
            if iteration & 0xff == 0xff {
                crate::multitask::cond_resched();
            }
            core::hint::spin_loop();
        }
        Err(PacketError::Busy)
    }
}

unsafe fn push_avail(queue: &mut VirtQueue, desc_id: u16) {
    let avail = queue.avail.cpu;
    let ring = unsafe { avail.add(4).cast::<u16>() };
    unsafe {
        ptr::write_volatile(
            ring.add((queue.next_avail as usize) % QUEUE_SIZE as usize),
            desc_id,
        );
    }
    queue.next_avail = queue.next_avail.wrapping_add(1);
    unsafe {
        ptr::write_volatile(avail.add(2).cast::<u16>(), queue.next_avail);
    }
}

fn setup_queue(common: &MmioRegion, index: u16) -> Result<VirtQueue, PacketError> {
    write_common_u16(common, COMMON_QUEUE_SELECT, index);
    let device_queue_size = read_common_u16(common, COMMON_QUEUE_SIZE);
    if device_queue_size < 2 {
        return Err(PacketError::NoDevice);
    }
    let queue_size = device_queue_size.min(QUEUE_SIZE);
    let desc_len = queue_size as usize * size_of::<VirtqDesc>();
    let avail_len = 6 + queue_size as usize * size_of::<u16>();
    let used_len = 6 + queue_size as usize * size_of::<VirtqUsedElem>();
    let desc = alloc_dma(desc_len)?;
    let avail = alloc_dma(avail_len)?;
    let used = alloc_dma(used_len)?;
    unsafe {
        ptr::write_bytes(desc.cpu, 0, desc.len);
        ptr::write_bytes(avail.cpu, 0, avail.len);
        ptr::write_bytes(used.cpu, 0, used.len);
    }
    write_common_u64(common, COMMON_QUEUE_DESC, desc.dma);
    write_common_u64(common, COMMON_QUEUE_DRIVER, avail.dma);
    write_common_u64(common, COMMON_QUEUE_DEVICE, used.dma);
    write_common_u16(common, COMMON_QUEUE_SIZE, queue_size);
    write_common_u16(common, COMMON_QUEUE_ENABLE, 1);
    Ok(VirtQueue {
        desc,
        avail,
        used,
        next_avail: 0,
        last_used: 0,
    })
}

fn find_virtio_net_device() -> Option<crate::arch::pci::PciDevice> {
    let mut found = None;
    crate::arch::pci::visit_devices(|pci| {
        if pci.vendor_id() != PCI_VENDOR_VIRTIO {
            return false;
        }
        if virtio_device_type(pci.device_id()) != Some(VIRTIO_DEVICE_ID_NET) {
            return false;
        }
        found = Some(pci);
        true
    });
    found
}

fn virtio_device_type(device_id: u16) -> Option<u32> {
    match device_id {
        PCI_DEVICE_VIRTIO_NET_TRANSITIONAL => Some(VIRTIO_DEVICE_ID_NET),
        PCI_DEVICE_VIRTIO_MODERN_BASE..=PCI_DEVICE_VIRTIO_MODERN_END => {
            Some(u32::from(device_id - PCI_DEVICE_VIRTIO_MODERN_BASE))
        }
        _ => None,
    }
}

fn discover_caps(pci: crate::arch::pci::PciDevice) -> Result<VirtioNetCaps, PacketError> {
    if pci.read_u16(PCI_STATUS_OFFSET) & PCI_CAPABILITY_LIST == 0 {
        return Err(PacketError::NoDevice);
    }
    let mut common = None;
    let mut notify = None;
    let mut notify_multiplier = 0;
    let mut cap = pci.read_u8(PCI_CAPABILITY_POINTER_OFFSET) & !0x3;
    let mut guard = 0;
    while cap >= 0x40 && guard < 48 {
        guard += 1;
        let cap_id = pci.read_u8(cap);
        let next = pci.read_u8(cap.wrapping_add(1)) & !0x3;
        if cap_id == PCI_CAP_ID_VENDOR {
            let cfg_type = pci.read_u8(cap.wrapping_add(3));
            let bar = pci.read_u8(cap.wrapping_add(4));
            let offset = pci.read_u32(cap.wrapping_add(8));
            let length = pci.read_u32(cap.wrapping_add(12));
            let virtio_cap = VirtioPciCap {
                bar,
                offset,
                length,
            };
            match cfg_type {
                VIRTIO_PCI_CAP_COMMON_CFG => common = Some(virtio_cap),
                VIRTIO_PCI_CAP_NOTIFY_CFG => {
                    notify = Some(virtio_cap);
                    notify_multiplier = pci.read_u32(cap.wrapping_add(16));
                }
                _ => {}
            }
        }
        if next == 0 || next == cap {
            break;
        }
        cap = next;
    }
    Ok(VirtioNetCaps {
        common: common.ok_or(PacketError::NoDevice)?,
        notify: notify.ok_or(PacketError::NoDevice)?,
        notify_multiplier,
    })
}

fn map_cap(pci: crate::arch::pci::PciDevice, cap: VirtioPciCap) -> Option<MmioRegion> {
    let resource = pci.resource(cap.bar as usize)?;
    if resource.is_io || cap.length == 0 {
        return None;
    }
    let start = resource.start.checked_add(cap.offset as u64)?;
    let len = cap.length as usize;
    let ptr = crate::driver::mmio::map(start, len, false).cast::<u8>();
    if ptr.is_null() {
        return None;
    }
    Some(MmioRegion { ptr, len })
}

fn alloc_dma(len: usize) -> Result<DmaBlock, PacketError> {
    let len = len.max(8);
    let mut dma = 0_u64;
    let cpu = crate::driver::dma::alloc_coherent(core::ptr::null_mut(), len, &mut dma);
    if cpu.is_null() || dma == crate::driver::dma::DMA_MAPPING_ERROR {
        return Err(PacketError::NoDevice);
    }
    Ok(DmaBlock {
        cpu: cpu.cast::<u8>(),
        dma,
        len,
    })
}

fn reset_device(common: &MmioRegion) {
    write_common_u8(common, COMMON_DEVICE_STATUS, 0);
    for _ in 0..10_000 {
        if read_common_u8(common, COMMON_DEVICE_STATUS) == 0 {
            return;
        }
        core::hint::spin_loop();
    }
}

fn read_common_u8(common: &MmioRegion, offset: usize) -> u8 {
    if offset >= common.len {
        return 0;
    }
    unsafe { ptr::read_volatile(common.ptr.add(offset)) }
}

fn read_common_u16(common: &MmioRegion, offset: usize) -> u16 {
    if offset + size_of::<u16>() > common.len {
        return 0;
    }
    unsafe { ptr::read_volatile(common.ptr.add(offset).cast::<u16>()) }
}

fn read_common_u32(common: &MmioRegion, offset: usize) -> u32 {
    if offset + size_of::<u32>() > common.len {
        return 0;
    }
    unsafe { ptr::read_volatile(common.ptr.add(offset).cast::<u32>()) }
}

fn write_common_u8(common: &MmioRegion, offset: usize, value: u8) {
    if offset >= common.len {
        return;
    }
    unsafe { ptr::write_volatile(common.ptr.add(offset), value) }
}

fn write_common_u16(common: &MmioRegion, offset: usize, value: u16) {
    if offset + size_of::<u16>() > common.len {
        return;
    }
    unsafe { ptr::write_volatile(common.ptr.add(offset).cast::<u16>(), value) }
}

fn write_common_u32(common: &MmioRegion, offset: usize, value: u32) {
    if offset + size_of::<u32>() > common.len {
        return;
    }
    unsafe { ptr::write_volatile(common.ptr.add(offset).cast::<u32>(), value) }
}

fn write_common_u64(common: &MmioRegion, offset: usize, value: u64) {
    if offset + size_of::<u64>() > common.len {
        return;
    }
    unsafe { ptr::write_volatile(common.ptr.add(offset).cast::<u64>(), value) }
}
