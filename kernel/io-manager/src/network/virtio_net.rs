use alloc::vec;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{Ordering, compiler_fence};

use heapless::Deque as HeaplessDeque;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, Ipv4Address};
use tock_registers::register_bitfields;

use crate::sync::KernelWaitLock;

use super::{
    InetSocketError, LinuxNetdevTransport, STATIC_IPV4_ADDR, STATIC_IPV4_GATEWAY,
    STATIC_IPV4_PREFIX_LEN, STATIC_MAC,
};

const PCI_VENDOR_VIRTIO: u16 = 0x1af4;
const PCI_DEVICE_VIRTIO_NET_TRANSITIONAL: u16 = 0x1000;
const PCI_DEVICE_VIRTIO_NET_MODERN: u16 = 0x1041;
const PCI_CAP_STATUS: u8 = 0x06;
const PCI_CAP_POINTER: u8 = 0x34;
const PCI_STATUS_CAP_LIST: u16 = 1 << 4;
const PCI_CAP_ID_VENDOR: u8 = 0x09;

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;

register_bitfields![u8,
    VirtioDeviceStatus [
        ACKNOWLEDGE OFFSET(0) NUMBITS(1) [],
        DRIVER OFFSET(1) NUMBITS(1) [],
        DRIVER_OK OFFSET(2) NUMBITS(1) [],
        FEATURES_OK OFFSET(3) NUMBITS(1) [],
        FAILED OFFSET(7) NUMBITS(1) []
    ]
];

const VIRTIO_STATUS_ACKNOWLEDGE: u8 =
    VirtioDeviceStatus::ACKNOWLEDGE.mask << VirtioDeviceStatus::ACKNOWLEDGE.shift;
const VIRTIO_STATUS_DRIVER: u8 =
    VirtioDeviceStatus::DRIVER.mask << VirtioDeviceStatus::DRIVER.shift;
const VIRTIO_STATUS_DRIVER_OK: u8 =
    VirtioDeviceStatus::DRIVER_OK.mask << VirtioDeviceStatus::DRIVER_OK.shift;
const VIRTIO_STATUS_FEATURES_OK: u8 =
    VirtioDeviceStatus::FEATURES_OK.mask << VirtioDeviceStatus::FEATURES_OK.shift;
const VIRTIO_STATUS_FAILED: u8 =
    VirtioDeviceStatus::FAILED.mask << VirtioDeviceStatus::FAILED.shift;
const VIRTIO_F_VERSION_1: u32 = 32;

const VIRTIO_NET_F_MAC: u32 = 5;

const COMMON_DEVICE_FEATURE_SELECT: usize = 0x00;
const COMMON_DEVICE_FEATURE: usize = 0x04;
const COMMON_DRIVER_FEATURE_SELECT: usize = 0x08;
const COMMON_DRIVER_FEATURE: usize = 0x0c;
const COMMON_NUM_QUEUES: usize = 0x12;
const COMMON_DEVICE_STATUS: usize = 0x14;
const COMMON_QUEUE_SELECT: usize = 0x16;
const COMMON_QUEUE_SIZE: usize = 0x18;
const COMMON_QUEUE_ENABLE: usize = 0x1c;
const COMMON_QUEUE_NOTIFY_OFF: usize = 0x1e;
const COMMON_QUEUE_DESC: usize = 0x20;
const COMMON_QUEUE_DRIVER: usize = 0x28;
const COMMON_QUEUE_DEVICE: usize = 0x30;

const RX_QUEUE_INDEX: u16 = 0;
const TX_QUEUE_INDEX: u16 = 1;
const QUEUE_DEPTH: usize = 16;
const QUEUE_SIZE: u16 = QUEUE_DEPTH as u16;
const QUEUE_MEM_SIZE: usize = virtio_drivers::PAGE_SIZE;
const RX_BUFFER_SIZE: usize = 2048;
const TX_BUFFER_SIZE: usize = 2048;
const VIRTIO_NET_HDR_SIZE: usize = 10;
const ETHERNET_MTU: usize = 1514;
const TCP_RX_BUFFER_SIZE: usize = 8192;
const TCP_TX_BUFFER_SIZE: usize = 8192;
const TCP_CONNECT_TIMEOUT_MS: u64 = 5_000;
const TCP_IO_POLL_ATTEMPTS: usize = 20_000;

static STACK: KernelWaitLock<Option<NetworkStack>> = KernelWaitLock::new(None);

#[derive(Clone, Copy)]
struct VirtioPciCaps {
    common: MmioRegion,
    notify: MmioRegion,
    notify_multiplier: u32,
}

#[derive(Clone, Copy)]
struct MmioRegion {
    bar: usize,
    offset: u64,
    length: usize,
}

struct NetworkStack {
    device: VirtioNetDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    tcp: Vec<TcpSocketRecord>,
}

unsafe impl Send for NetworkStack {}

struct TcpSocketRecord {
    token: u64,
    handle: SocketHandle,
}

struct VirtioNetDevice {
    _common: *mut u8,
    notify: *mut u8,
    notify_multiplier: u32,
    rx_queue: VirtQueue,
    tx_queue: VirtQueue,
    rx_buffers: Vec<DmaBuffer>,
    tx_buffers: Vec<DmaBuffer>,
    rx_pending: HeaplessDeque<Vec<u8>, QUEUE_DEPTH>,
    tx_in_use: [bool; QUEUE_DEPTH],
    tx_slot: usize,
    rx_frames: u64,
    tx_frames: u64,
    tx_completions: u64,
}

unsafe impl Send for VirtioNetDevice {}

struct VirtQueue {
    _index: u16,
    queue_size: u16,
    notify_off: u16,
    mem_cpu: *mut u8,
    _mem_dma: u64,
    desc_offset: usize,
    avail_offset: usize,
    used_offset: usize,
    avail_idx: u16,
    used_idx: u16,
}

unsafe impl Send for VirtQueue {}

struct DmaBuffer {
    cpu: *mut u8,
    dma: u64,
    size: usize,
}

unsafe impl Send for DmaBuffer {}

pub(super) fn ensure_initialized() -> Result<(), InetSocketError> {
    if STACK.lock().is_some() {
        return Ok(());
    }

    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "virtio-net-init-begin",
        0,
        0,
    );
    let mut stack = STACK.lock();
    if stack.is_some() {
        return Ok(());
    }
    let Some(mut device) = VirtioNetDevice::init() else {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Driver,
            "virtio-net-init-unavailable",
            0,
            0,
        );
        return Err(InetSocketError::NetworkUnreachable);
    };

    let mut config = Config::new(EthernetAddress(STATIC_MAC).into());
    config.random_seed = 0x5255_5354_4f53_4e45;
    let now = smol_now();
    let mut iface = Interface::new(config, &mut device, now);
    iface.update_ip_addrs(|ip_addrs| {
        let _ = ip_addrs.push(IpCidr::new(
            IpAddress::v4(
                STATIC_IPV4_ADDR[0],
                STATIC_IPV4_ADDR[1],
                STATIC_IPV4_ADDR[2],
                STATIC_IPV4_ADDR[3],
            ),
            STATIC_IPV4_PREFIX_LEN,
        ));
    });
    let _ = iface.routes_mut().add_default_ipv4_route(Ipv4Address::new(
        STATIC_IPV4_GATEWAY[0],
        STATIC_IPV4_GATEWAY[1],
        STATIC_IPV4_GATEWAY[2],
        STATIC_IPV4_GATEWAY[3],
    ));

    crate::network::set_current_linux_netdev_transport(LinuxNetdevTransport::Virtio);
    crate::network::allocate_linux_netdev(&device as *const _ as usize, 0, 1, 1);
    let _ = crate::network::register_linux_netdev(
        &device as *const _ as usize,
        LinuxNetdevTransport::Virtio,
    );
    crate::network::set_linux_netdev_carrier(&device as *const _ as usize, true);
    crate::network::set_current_linux_netdev_transport(LinuxNetdevTransport::Unknown);

    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "virtio-net-init-done",
        STATIC_IPV4_ADDR[3] as u64,
        STATIC_IPV4_GATEWAY[3] as u64,
    );

    *stack = Some(NetworkStack {
        device,
        iface,
        sockets: SocketSet::new(vec![]),
        tcp: Vec::new(),
    });
    Ok(())
}

pub(super) fn close_tcp(token: u64) {
    let mut stack = STACK.lock();
    let Some(stack) = stack.as_mut() else {
        return;
    };
    if let Some(index) = stack.tcp.iter().position(|record| record.token == token) {
        let record = stack.tcp.remove(index);
        stack.sockets.remove(record.handle);
    }
}

pub(super) fn connect_tcp(token: u64, addr: [u8; 4], port: u16) -> Result<(), InetSocketError> {
    ensure_initialized()?;
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "virtio-net-tcp-connect-begin",
        token,
        port as u64,
    );
    {
        let mut stack = STACK.lock();
        let stack = stack.as_mut().ok_or(InetSocketError::NetworkUnreachable)?;
        let handle = ensure_tcp_socket(stack, token);
        let remote = IpAddress::v4(addr[0], addr[1], addr[2], addr[3]);
        let local_port = 49152 + ((token as u16) % 16384);
        let cx = stack.iface.context();
        let socket = stack.sockets.get_mut::<tcp::Socket>(handle);
        if !socket.is_active() {
            socket
                .connect(cx, (remote, port), local_port)
                .map_err(|_| InetSocketError::NetworkUnreachable)?;
        }
    }

    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    let timeout_ticks = TCP_CONNECT_TIMEOUT_MS
        .saturating_mul(ticks_per_second)
        .div_ceil(1_000);
    let deadline = crate::arch::rtc::ticks().saturating_add(timeout_ticks.max(1));
    loop {
        {
            let mut stack = STACK.lock();
            let stack = stack.as_mut().ok_or(InetSocketError::NetworkUnreachable)?;
            let handle = find_tcp_socket(stack, token)?;
            poll_stack(stack);
            let socket = stack.sockets.get_mut::<tcp::Socket>(handle);
            if socket.may_send() {
                crate::debug::record_milestone(
                    crate::debug::LogCategory::Driver,
                    "virtio-net-tcp-connect-done",
                    token,
                    port as u64,
                );
                return Ok(());
            }
            if !socket.is_active() {
                crate::debug::record_milestone(
                    crate::debug::LogCategory::Driver,
                    "virtio-net-tcp-connect-closed",
                    token,
                    port as u64,
                );
                return Err(InetSocketError::NetworkUnreachable);
            }
        }
        if crate::arch::rtc::ticks() >= deadline {
            break;
        }
        crate::arch::rtc::sleep(1);
    }
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "virtio-net-tcp-connect-timeout",
        token,
        port as u64,
    );
    Err(InetSocketError::TryAgain)
}

pub(super) fn send_tcp(token: u64, bytes: &[u8]) -> Result<usize, InetSocketError> {
    ensure_initialized()?;
    let mut stack = STACK.lock();
    let stack = stack.as_mut().ok_or(InetSocketError::NetworkUnreachable)?;
    let handle = find_tcp_socket(stack, token)?;
    poll_stack(stack);
    let written = {
        let socket = stack.sockets.get_mut::<tcp::Socket>(handle);
        if !socket.may_send() {
            return Err(InetSocketError::TryAgain);
        }
        socket
            .send_slice(bytes)
            .map_err(|_| InetSocketError::OperationNotSupported)?
    };
    poll_stack(stack);
    Ok(written)
}

pub(super) fn recv_tcp(
    token: u64,
    out: &mut [u8],
    nonblocking: bool,
) -> Result<usize, InetSocketError> {
    ensure_initialized()?;
    let attempts = if nonblocking { 1 } else { TCP_IO_POLL_ATTEMPTS };
    for _ in 0..attempts {
        {
            let mut stack = STACK.lock();
            let stack = stack.as_mut().ok_or(InetSocketError::NetworkUnreachable)?;
            let handle = find_tcp_socket(stack, token)?;
            poll_stack(stack);
            let socket = stack.sockets.get_mut::<tcp::Socket>(handle);
            if socket.can_recv() {
                return socket
                    .recv_slice(out)
                    .map_err(|_| InetSocketError::OperationNotSupported);
            }
            if !socket.may_recv() {
                return Ok(0);
            }
        }
        if !nonblocking {
            crate::multitask::yield_now();
        }
    }
    Err(InetSocketError::TryAgain)
}

pub(super) fn readable_tcp_bytes(token: u64) -> Result<usize, InetSocketError> {
    ensure_initialized()?;
    let mut stack = STACK.lock();
    let stack = stack.as_mut().ok_or(InetSocketError::NetworkUnreachable)?;
    let handle = find_tcp_socket(stack, token)?;
    poll_stack(stack);
    Ok(stack.sockets.get::<tcp::Socket>(handle).recv_queue())
}

pub(super) fn tcp_may_send(token: u64) -> bool {
    let mut stack = STACK.lock();
    let Some(stack) = stack.as_mut() else {
        return false;
    };
    let Ok(handle) = find_tcp_socket(stack, token) else {
        return false;
    };
    poll_stack(stack);
    stack.sockets.get::<tcp::Socket>(handle).may_send()
}

fn ensure_tcp_socket(stack: &mut NetworkStack, token: u64) -> SocketHandle {
    if let Ok(handle) = find_tcp_socket(stack, token) {
        return handle;
    }
    let rx_buffer = tcp::SocketBuffer::new(vec![0; TCP_RX_BUFFER_SIZE]);
    let tx_buffer = tcp::SocketBuffer::new(vec![0; TCP_TX_BUFFER_SIZE]);
    let socket = tcp::Socket::new(rx_buffer, tx_buffer);
    let handle = stack.sockets.add(socket);
    stack.tcp.push(TcpSocketRecord { token, handle });
    handle
}

fn find_tcp_socket(stack: &NetworkStack, token: u64) -> Result<SocketHandle, InetSocketError> {
    stack
        .tcp
        .iter()
        .find(|record| record.token == token)
        .map(|record| record.handle)
        .ok_or(InetSocketError::BadFileDescriptor)
}

fn poll_stack(stack: &mut NetworkStack) {
    stack.device.poll_rx();
    let now = smol_now();
    let _ = stack.iface.poll(now, &mut stack.device, &mut stack.sockets);
}

impl VirtioNetDevice {
    fn init() -> Option<Self> {
        let mut found = None;
        crate::arch::pci::visit_devices(|pci| {
            if pci.vendor_id() == PCI_VENDOR_VIRTIO
                && matches!(
                    pci.device_id(),
                    PCI_DEVICE_VIRTIO_NET_TRANSITIONAL | PCI_DEVICE_VIRTIO_NET_MODERN
                )
            {
                found = Some(pci);
                return true;
            }
            false
        });
        let pci = found?;
        let caps = parse_virtio_pci_caps(pci)?;
        let common = map_region(pci, caps.common)?;
        let notify = map_region(pci, caps.notify)?;
        pci.enable_memory_bus_master();

        unsafe {
            write_common_u8(common, COMMON_DEVICE_STATUS, 0);
            write_common_u8(
                common,
                COMMON_DEVICE_STATUS,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER,
            );
            negotiate_features(common)?;
        }

        let rx_queue = setup_queue(common, RX_QUEUE_INDEX)?;
        let tx_queue = setup_queue(common, TX_QUEUE_INDEX)?;
        let mut device = Self {
            _common: common,
            notify,
            notify_multiplier: caps.notify_multiplier,
            rx_queue,
            tx_queue,
            rx_buffers: Vec::new(),
            tx_buffers: Vec::new(),
            rx_pending: HeaplessDeque::new(),
            tx_in_use: [false; QUEUE_DEPTH],
            tx_slot: 0,
            rx_frames: 0,
            tx_frames: 0,
            tx_completions: 0,
        };
        device.allocate_buffers()?;

        unsafe {
            let status = read_common_u8(common, COMMON_DEVICE_STATUS) | VIRTIO_STATUS_DRIVER_OK;
            write_common_u8(common, COMMON_DEVICE_STATUS, status);
        }
        device.populate_rx();
        Some(device)
    }

    fn allocate_buffers(&mut self) -> Option<()> {
        for _ in 0..self.rx_queue.queue_size {
            self.rx_buffers.push(alloc_dma(RX_BUFFER_SIZE)?);
        }
        for _ in 0..self.tx_queue.queue_size {
            self.tx_buffers.push(alloc_dma(TX_BUFFER_SIZE)?);
        }
        Some(())
    }

    fn populate_rx(&mut self) {
        for index in 0..self.rx_buffers.len() {
            let buffer = &self.rx_buffers[index];
            unsafe {
                ptr::write_bytes(buffer.cpu, 0, buffer.size);
                self.rx_queue
                    .write_desc(index, buffer.dma, buffer.size as u32, DESC_F_WRITE, 0);
                self.rx_queue.push_avail(index as u16);
            }
        }
        self.notify_queue(RX_QUEUE_INDEX, self.rx_queue.notify_off);
    }

    fn poll_rx(&mut self) {
        let mut requeued = false;
        while let Some((id, len)) = self.rx_queue.pop_used() {
            let index = id as usize;
            if let Some(buffer) = self.rx_buffers.get(index) {
                let frame_len = (len as usize).saturating_sub(VIRTIO_NET_HDR_SIZE);
                if frame_len != 0 && frame_len <= buffer.size.saturating_sub(VIRTIO_NET_HDR_SIZE) {
                    let frame = unsafe {
                        core::slice::from_raw_parts(buffer.cpu.add(VIRTIO_NET_HDR_SIZE), frame_len)
                    };
                    self.rx_frames = self.rx_frames.wrapping_add(1);
                    if self.rx_frames <= 8 {
                        crate::debug::record_milestone(
                            crate::debug::LogCategory::Driver,
                            "virtio-net-rx-frame",
                            self.rx_frames,
                            ethernet_frame_marker(frame),
                        );
                    }
                    if self.rx_pending.len() < QUEUE_DEPTH {
                        let _ = self.rx_pending.push_back(frame.to_vec());
                    }
                }
                unsafe {
                    ptr::write_bytes(buffer.cpu, 0, buffer.size);
                    self.rx_queue.write_desc(
                        index,
                        buffer.dma,
                        buffer.size as u32,
                        DESC_F_WRITE,
                        0,
                    );
                    self.rx_queue.push_avail(index as u16);
                }
                requeued = true;
            }
        }
        if requeued {
            self.notify_queue(RX_QUEUE_INDEX, self.rx_queue.notify_off);
        }
    }

    fn reclaim_tx_completions(&mut self) {
        while let Some((id, _)) = self.tx_queue.pop_used() {
            let slot = id as usize;
            if slot < self.tx_in_use.len() {
                self.tx_in_use[slot] = false;
            }
            self.tx_completions = self.tx_completions.wrapping_add(1);
            if self.tx_completions <= 8 {
                crate::debug::record_milestone(
                    crate::debug::LogCategory::Driver,
                    "virtio-net-tx-complete",
                    self.tx_completions,
                    slot as u64,
                );
            }
        }
    }

    fn has_free_tx_slot(&self) -> bool {
        self.tx_buffers
            .iter()
            .enumerate()
            .any(|(slot, _)| !self.tx_in_use[slot])
    }

    fn next_tx_slot(&mut self) -> Option<usize> {
        self.reclaim_tx_completions();
        if self.tx_buffers.is_empty() {
            return None;
        }
        for offset in 0..self.tx_buffers.len() {
            let slot = (self.tx_slot + offset) % self.tx_buffers.len();
            if !self.tx_in_use[slot] {
                self.tx_slot = slot.wrapping_add(1);
                return Some(slot);
            }
        }
        None
    }

    fn transmit_frame_from<R>(&mut self, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
        if len
            .checked_add(VIRTIO_NET_HDR_SIZE)
            .is_none_or(|total| total > TX_BUFFER_SIZE)
        {
            let mut frame = vec![0u8; len];
            return f(&mut frame);
        }
        let Some(slot) = self.next_tx_slot() else {
            let mut frame = vec![0u8; len];
            return f(&mut frame);
        };

        let buffer = &self.tx_buffers[slot];
        let buffer_cpu = buffer.cpu;
        let buffer_dma = buffer.dma;
        unsafe {
            ptr::write_bytes(buffer_cpu, 0, VIRTIO_NET_HDR_SIZE);
        }

        let (result, marker) = {
            let frame = unsafe {
                core::slice::from_raw_parts_mut(buffer_cpu.add(VIRTIO_NET_HDR_SIZE), len)
            };
            let result = f(frame);
            (result, ethernet_frame_marker(frame))
        };

        unsafe {
            self.tx_queue
                .write_desc(slot, buffer_dma, (len + VIRTIO_NET_HDR_SIZE) as u32, 0, 0);
            self.tx_queue.push_avail(slot as u16);
        }
        self.tx_in_use[slot] = true;
        self.tx_frames = self.tx_frames.wrapping_add(1);
        if self.tx_frames <= 8 {
            crate::debug::record_milestone(
                crate::debug::LogCategory::Driver,
                "virtio-net-tx-frame",
                self.tx_frames,
                marker,
            );
        }
        self.notify_queue(TX_QUEUE_INDEX, self.tx_queue.notify_off);
        result
    }

    fn notify_queue(&self, queue_index: u16, notify_off: u16) {
        unsafe {
            ptr::write_volatile(
                self.notify
                    .add((notify_off as usize) * self.notify_multiplier as usize)
                    as *mut u16,
                queue_index,
            );
        }
    }
}

impl Device for VirtioNetDevice {
    type RxToken<'a> = VirtioRxToken;
    type TxToken<'a> = VirtioTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.poll_rx();
        self.rx_pending
            .pop_front()
            .map(|frame| (VirtioRxToken { frame }, VirtioTxToken { device: self }))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        self.reclaim_tx_completions();
        if self.has_free_tx_slot() {
            Some(VirtioTxToken { device: self })
        } else {
            None
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = ETHERNET_MTU;
        caps.checksum = ChecksumCapabilities::default();
        caps
    }
}

struct VirtioRxToken {
    frame: Vec<u8>,
}

impl RxToken for VirtioRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.frame)
    }
}

struct VirtioTxToken<'a> {
    device: &'a mut VirtioNetDevice,
}

impl TxToken for VirtioTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        self.device.transmit_frame_from(len, f)
    }
}

const DESC_F_WRITE: u16 = 2;

impl VirtQueue {
    unsafe fn write_desc(&self, index: usize, addr: u64, len: u32, flags: u16, next: u16) {
        unsafe {
            let desc = self.mem_cpu.add(self.desc_offset + index * 16);
            ptr::write_volatile(desc as *mut u64, addr);
            ptr::write_volatile(desc.add(8) as *mut u32, len);
            ptr::write_volatile(desc.add(12) as *mut u16, flags);
            ptr::write_volatile(desc.add(14) as *mut u16, next);
        }
    }

    unsafe fn push_avail(&mut self, head: u16) {
        unsafe {
            let avail = self.mem_cpu.add(self.avail_offset);
            ptr::write_volatile(
                avail.add(4 + ((self.avail_idx as usize % self.queue_size as usize) * 2))
                    as *mut u16,
                head,
            );
            self.avail_idx = self.avail_idx.wrapping_add(1);
            compiler_fence(Ordering::SeqCst);
            ptr::write_volatile(avail.add(2) as *mut u16, self.avail_idx);
        }
    }

    fn pop_used(&mut self) -> Option<(u16, u32)> {
        let used = unsafe { self.mem_cpu.add(self.used_offset) };
        let next_used = unsafe { ptr::read_volatile(used.add(2) as *const u16) };
        if next_used == self.used_idx {
            return None;
        }
        let ring_index = self.used_idx as usize % self.queue_size as usize;
        let elem = unsafe { used.add(4 + ring_index * 8) };
        let id = unsafe { ptr::read_volatile(elem as *const u32) } as u16;
        let len = unsafe { ptr::read_volatile(elem.add(4) as *const u32) };
        self.used_idx = self.used_idx.wrapping_add(1);
        Some((id, len))
    }
}

fn setup_queue(common: *mut u8, queue_index: u16) -> Option<VirtQueue> {
    unsafe {
        let num_queues = read_common_u16(common, COMMON_NUM_QUEUES);
        if queue_index >= num_queues {
            return None;
        }
        write_common_u16(common, COMMON_QUEUE_SELECT, queue_index);
        let max_size = read_common_u16(common, COMMON_QUEUE_SIZE);
        if max_size < 2 {
            return None;
        }
        let queue_size = QUEUE_SIZE.min(max_size);
        let notify_off = read_common_u16(common, COMMON_QUEUE_NOTIFY_OFF);
        let queue_mem = alloc_dma(QUEUE_MEM_SIZE)?;
        ptr::write_bytes(queue_mem.cpu, 0, QUEUE_MEM_SIZE);

        let desc_offset = 0;
        let avail_offset = align_up(queue_size as usize * 16, 2);
        let used_offset = align_up(avail_offset + 4 + queue_size as usize * 2, 4);

        write_common_u16(common, COMMON_QUEUE_SIZE, queue_size);
        write_common_u64(
            common,
            COMMON_QUEUE_DESC,
            queue_mem.dma + desc_offset as u64,
        );
        write_common_u64(
            common,
            COMMON_QUEUE_DRIVER,
            queue_mem.dma + avail_offset as u64,
        );
        write_common_u64(
            common,
            COMMON_QUEUE_DEVICE,
            queue_mem.dma + used_offset as u64,
        );
        write_common_u16(common, COMMON_QUEUE_ENABLE, 1);

        Some(VirtQueue {
            _index: queue_index,
            queue_size,
            notify_off,
            mem_cpu: queue_mem.cpu,
            _mem_dma: queue_mem.dma,
            desc_offset,
            avail_offset,
            used_offset,
            avail_idx: 0,
            used_idx: 0,
        })
    }
}

unsafe fn negotiate_features(common: *mut u8) -> Option<()> {
    unsafe {
        write_common_u32(common, COMMON_DEVICE_FEATURE_SELECT, 1);
        let high = read_common_u32(common, COMMON_DEVICE_FEATURE);
        if (high & (1 << (VIRTIO_F_VERSION_1 - 32))) == 0 {
            return None;
        }
        write_common_u32(common, COMMON_DEVICE_FEATURE_SELECT, 0);
        let low = read_common_u32(common, COMMON_DEVICE_FEATURE);
        let net_features = low & (1 << VIRTIO_NET_F_MAC);

        write_common_u32(common, COMMON_DRIVER_FEATURE_SELECT, 0);
        write_common_u32(common, COMMON_DRIVER_FEATURE, net_features);
        write_common_u32(common, COMMON_DRIVER_FEATURE_SELECT, 1);
        write_common_u32(
            common,
            COMMON_DRIVER_FEATURE,
            1 << (VIRTIO_F_VERSION_1 - 32),
        );

        let status = read_common_u8(common, COMMON_DEVICE_STATUS) | VIRTIO_STATUS_FEATURES_OK;
        write_common_u8(common, COMMON_DEVICE_STATUS, status);
        if (read_common_u8(common, COMMON_DEVICE_STATUS) & VIRTIO_STATUS_FEATURES_OK) == 0 {
            write_common_u8(common, COMMON_DEVICE_STATUS, VIRTIO_STATUS_FAILED);
            return None;
        }
        Some(())
    }
}

fn parse_virtio_pci_caps(pci: crate::arch::pci::PciDevice) -> Option<VirtioPciCaps> {
    if (pci.read_u16(PCI_CAP_STATUS) & PCI_STATUS_CAP_LIST) == 0 {
        return None;
    }

    let mut common = None;
    let mut notify = None;
    let mut notify_multiplier = 0;
    let mut cap = pci.read_u8(PCI_CAP_POINTER) & !0x3;
    let mut guard = 0;

    while cap != 0 && guard < 32 {
        guard += 1;
        if pci.read_u8(cap) != PCI_CAP_ID_VENDOR {
            cap = pci.read_u8(cap + 1) & !0x3;
            continue;
        }

        let cfg_type = pci.read_u8(cap + 3);
        let bar = pci.read_u8(cap + 4) as usize;
        let offset = pci.read_u32(cap + 8) as u64;
        let length = pci.read_u32(cap + 12) as usize;
        let region = MmioRegion {
            bar,
            offset,
            length,
        };

        match cfg_type {
            VIRTIO_PCI_CAP_COMMON_CFG => common = Some(region),
            VIRTIO_PCI_CAP_NOTIFY_CFG => {
                notify = Some(region);
                notify_multiplier = pci.read_u32(cap + 16);
            }
            _ => {}
        }

        cap = pci.read_u8(cap + 1) & !0x3;
    }

    Some(VirtioPciCaps {
        common: common?,
        notify: notify?,
        notify_multiplier,
    })
}

fn map_region(pci: crate::arch::pci::PciDevice, region: MmioRegion) -> Option<*mut u8> {
    let resource = pci.resource(region.bar)?;
    if resource.is_io || region.length == 0 {
        return None;
    }
    let ptr = crate::driver::mmio::map(
        resource.start.checked_add(region.offset)?,
        region.length.max(4),
        false,
    );
    (!ptr.is_null()).then_some(ptr.cast())
}

fn alloc_dma(size: usize) -> Option<DmaBuffer> {
    let mut dma = 0u64;
    let ptr = crate::driver::dma::alloc_coherent(core::ptr::null_mut(), size, &mut dma);
    (!ptr.is_null()).then_some(DmaBuffer {
        cpu: ptr.cast(),
        dma,
        size,
    })
}

unsafe fn read_common_u8(base: *mut u8, offset: usize) -> u8 {
    unsafe { ptr::read_volatile(base.add(offset) as *const u8) }
}

unsafe fn write_common_u8(base: *mut u8, offset: usize, value: u8) {
    unsafe { ptr::write_volatile(base.add(offset) as *mut u8, value) }
}

unsafe fn read_common_u16(base: *mut u8, offset: usize) -> u16 {
    unsafe { ptr::read_volatile(base.add(offset) as *const u16) }
}

unsafe fn write_common_u16(base: *mut u8, offset: usize, value: u16) {
    unsafe { ptr::write_volatile(base.add(offset) as *mut u16, value) }
}

unsafe fn read_common_u32(base: *mut u8, offset: usize) -> u32 {
    unsafe { ptr::read_volatile(base.add(offset) as *const u32) }
}

unsafe fn write_common_u32(base: *mut u8, offset: usize, value: u32) {
    unsafe { ptr::write_volatile(base.add(offset) as *mut u32, value) }
}

unsafe fn write_common_u64(base: *mut u8, offset: usize, value: u64) {
    unsafe { ptr::write_volatile(base.add(offset) as *mut u64, value) }
}

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

fn ethernet_frame_marker(frame: &[u8]) -> u64 {
    if frame.len() < 14 {
        return frame.len() as u64;
    }
    let ether_type = u16::from_be_bytes([frame[12], frame[13]]) as u64;
    let len = frame.len().min(u16::MAX as usize) as u64;
    (ether_type << 32) | len
}

fn smol_now() -> Instant {
    Instant::from_millis(crate::driver::linux::runtime::current_jiffies() as i64)
}
