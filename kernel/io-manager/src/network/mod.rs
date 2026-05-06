use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use spin::Mutex;

mod virtio_net;

pub const STATIC_IPV4_ADDR: [u8; 4] = [10, 0, 2, 15];
pub const STATIC_IPV4_GATEWAY: [u8; 4] = [10, 0, 2, 2];
pub const STATIC_IPV4_PREFIX_LEN: u8 = 24;
pub const STATIC_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

static VIRTIO_NET_DRIVER_REGISTERED: AtomicBool = AtomicBool::new(false);
static NEXT_INET_SOCKET: AtomicU64 = AtomicU64::new(1);
static NEXT_NETDEV_ID: AtomicU64 = AtomicU64::new(1);
static CURRENT_NETDEV_TRANSPORT: AtomicU8 = AtomicU8::new(LINUX_NETDEV_TRANSPORT_UNKNOWN);
static INET_SOCKETS: Mutex<Vec<InetSocketRecord>> = Mutex::new(Vec::new());
static LINUX_NETDEVS: Mutex<Vec<LinuxNetdevRecord>> = Mutex::new(Vec::new());

const LINUX_NETDEV_TRANSPORT_UNKNOWN: u8 = 0;
const LINUX_NETDEV_TRANSPORT_PCI: u8 = 1;
const LINUX_NETDEV_TRANSPORT_VIRTIO: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxNetdevTransport {
    Unknown,
    Pci,
    Virtio,
}

impl LinuxNetdevTransport {
    fn to_raw(self) -> u8 {
        match self {
            Self::Unknown => LINUX_NETDEV_TRANSPORT_UNKNOWN,
            Self::Pci => LINUX_NETDEV_TRANSPORT_PCI,
            Self::Virtio => LINUX_NETDEV_TRANSPORT_VIRTIO,
        }
    }

    fn from_raw(raw: u8) -> Self {
        match raw {
            LINUX_NETDEV_TRANSPORT_PCI => Self::Pci,
            LINUX_NETDEV_TRANSPORT_VIRTIO => Self::Virtio,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinuxNetdevState {
    Allocated,
    Registered,
    Unregistered,
}

struct LinuxNetdevRecord {
    id: u64,
    dev_addr: usize,
    priv_bytes: usize,
    tx_queues: u32,
    rx_queues: u32,
    transport: LinuxNetdevTransport,
    state: LinuxNetdevState,
    carrier: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InetSocketError {
    BadFileDescriptor,
    InvalidArgument,
    NetworkUnreachable,
    NotConnected,
    OperationNotSupported,
    TryAgain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InetSocketState {
    Open,
    Connected,
}

struct InetSocketRecord {
    token: u64,
    type_: u64,
    protocol: u64,
    remote_addr: [u8; 4],
    remote_port: u16,
    state: InetSocketState,
    rx: VecDeque<u8>,
}

#[allow(dead_code)]
pub(crate) fn note_virtio_net_driver_registered() {
    VIRTIO_NET_DRIVER_REGISTERED.store(true, Ordering::Release);
    crate::debug::record_milestone(crate::debug::LogCategory::Driver, "virtio-net-driver", 0, 0);
    if let Err(error) = virtio_net::ensure_initialized() {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Driver,
            "virtio-net-init-deferred",
            error as u64,
            0,
        );
    }
}

pub fn virtio_net_driver_registered() -> bool {
    VIRTIO_NET_DRIVER_REGISTERED.load(Ordering::Acquire)
}

pub fn netdev_registered() -> bool {
    LINUX_NETDEVS
        .lock()
        .iter()
        .any(|netdev| netdev.state == LinuxNetdevState::Registered)
}

pub fn link_up() -> bool {
    let _ = virtio_net::ensure_initialized();
    LINUX_NETDEVS
        .lock()
        .iter()
        .any(|netdev| netdev.state == LinuxNetdevState::Registered && netdev.carrier)
}

pub(crate) fn allocate_linux_netdev(
    dev_addr: usize,
    priv_bytes: usize,
    tx_queues: u32,
    rx_queues: u32,
) {
    if dev_addr == 0 {
        return;
    }
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "netdev-allocate",
        dev_addr as u64,
        ((tx_queues as u64) << 32) | rx_queues as u64,
    );

    let mut netdevs = LINUX_NETDEVS.lock();
    if let Some(netdev) = netdevs
        .iter_mut()
        .find(|netdev| netdev.dev_addr == dev_addr)
    {
        netdev.priv_bytes = priv_bytes;
        netdev.tx_queues = tx_queues;
        netdev.rx_queues = rx_queues;
        netdev.state = LinuxNetdevState::Allocated;
        netdev.carrier = false;
        return;
    }

    netdevs.push(LinuxNetdevRecord {
        id: NEXT_NETDEV_ID.fetch_add(1, Ordering::Relaxed),
        dev_addr,
        priv_bytes,
        tx_queues,
        rx_queues,
        transport: LinuxNetdevTransport::Unknown,
        state: LinuxNetdevState::Allocated,
        carrier: false,
    });
}

pub(crate) fn register_linux_netdev(dev_addr: usize, transport: LinuxNetdevTransport) -> i32 {
    if dev_addr == 0 {
        return -22;
    }

    let mut netdevs = LINUX_NETDEVS.lock();
    let netdev = match netdevs
        .iter_mut()
        .find(|netdev| netdev.dev_addr == dev_addr)
    {
        Some(netdev) => netdev,
        None => {
            netdevs.push(LinuxNetdevRecord {
                id: NEXT_NETDEV_ID.fetch_add(1, Ordering::Relaxed),
                dev_addr,
                priv_bytes: 0,
                tx_queues: 0,
                rx_queues: 0,
                transport,
                state: LinuxNetdevState::Registered,
                carrier: false,
            });
            netdevs
                .last_mut()
                .expect("pushed linux netdev record must exist")
        }
    };

    netdev.transport = transport;
    netdev.state = LinuxNetdevState::Registered;
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "netdev-register",
        netdev.id,
        transport.to_raw() as u64,
    );
    crate::debug::info!(
        driver,
        "linux compat: netdev registered id={} dev={:#x} transport={:?} txqs={} rxqs={} ipv4={}.{}.{}.{}/{} gateway={}.{}.{}.{} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        netdev.id,
        dev_addr,
        netdev.transport,
        netdev.tx_queues,
        netdev.rx_queues,
        STATIC_IPV4_ADDR[0],
        STATIC_IPV4_ADDR[1],
        STATIC_IPV4_ADDR[2],
        STATIC_IPV4_ADDR[3],
        STATIC_IPV4_PREFIX_LEN,
        STATIC_IPV4_GATEWAY[0],
        STATIC_IPV4_GATEWAY[1],
        STATIC_IPV4_GATEWAY[2],
        STATIC_IPV4_GATEWAY[3],
        STATIC_MAC[0],
        STATIC_MAC[1],
        STATIC_MAC[2],
        STATIC_MAC[3],
        STATIC_MAC[4],
        STATIC_MAC[5],
    );
    0
}

pub(crate) fn current_linux_netdev_transport() -> LinuxNetdevTransport {
    LinuxNetdevTransport::from_raw(CURRENT_NETDEV_TRANSPORT.load(Ordering::Acquire))
}

pub(crate) fn set_current_linux_netdev_transport(transport: LinuxNetdevTransport) {
    CURRENT_NETDEV_TRANSPORT.store(transport.to_raw(), Ordering::Release);
}

pub(crate) fn unregister_linux_netdev(dev_addr: usize) {
    let mut netdevs = LINUX_NETDEVS.lock();
    if let Some(netdev) = netdevs
        .iter_mut()
        .find(|netdev| netdev.dev_addr == dev_addr)
    {
        netdev.state = LinuxNetdevState::Unregistered;
        netdev.carrier = false;
        crate::debug::record_milestone(
            crate::debug::LogCategory::Driver,
            "netdev-unregister",
            netdev.id,
            dev_addr as u64,
        );
        crate::debug::info!(
            driver,
            "linux compat: netdev unregistered id={} dev={:#x}",
            netdev.id,
            dev_addr
        );
    }
}

pub(crate) fn free_linux_netdev(dev_addr: usize) {
    let mut netdevs = LINUX_NETDEVS.lock();
    if let Some(index) = netdevs
        .iter()
        .position(|netdev| netdev.dev_addr == dev_addr)
    {
        let netdev = netdevs.remove(index);
        crate::debug::record_milestone(
            crate::debug::LogCategory::Driver,
            "netdev-free",
            netdev.id,
            dev_addr as u64,
        );
        crate::debug::info!(
            driver,
            "linux compat: netdev freed id={} dev={:#x}",
            netdev.id,
            dev_addr
        );
    }
}

pub(crate) fn set_linux_netdev_carrier(dev_addr: usize, up: bool) {
    let mut netdevs = LINUX_NETDEVS.lock();
    let Some(netdev) = netdevs
        .iter_mut()
        .find(|netdev| netdev.dev_addr == dev_addr)
    else {
        crate::debug::warn!(
            driver,
            "linux compat: netdev carrier change for unknown dev={:#x} up={}",
            dev_addr,
            up
        );
        return;
    };
    netdev.carrier = up;
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "netdev-carrier",
        netdev.id,
        if up { 1 } else { 0 },
    );
    crate::debug::info!(
        driver,
        "linux compat: netdev carrier id={} dev={:#x} up={}",
        netdev.id,
        dev_addr,
        up
    );
}

pub fn create_inet_socket(type_: u64, protocol: u64) -> u64 {
    let token = NEXT_INET_SOCKET.fetch_add(1, Ordering::Relaxed);
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "af-inet-create",
        token,
        type_,
    );
    INET_SOCKETS.lock().push(InetSocketRecord {
        token,
        type_,
        protocol,
        remote_addr: [0; 4],
        remote_port: 0,
        state: InetSocketState::Open,
        rx: VecDeque::new(),
    });
    token
}

pub fn close_inet_socket(token: u64) {
    crate::debug::record_milestone(crate::debug::LogCategory::Driver, "af-inet-close", token, 0);
    virtio_net::close_tcp(token);
    let mut sockets = INET_SOCKETS.lock();
    if let Some(index) = sockets.iter().position(|socket| socket.token == token) {
        sockets.remove(index);
    }
}

pub fn connect_inet_socket(token: u64, addr: [u8; 4], port: u16) -> Result<(), InetSocketError> {
    if port == 0 {
        return Err(InetSocketError::InvalidArgument);
    }
    let mut sockets = INET_SOCKETS.lock();
    let socket = sockets
        .iter_mut()
        .find(|socket| socket.token == token)
        .ok_or(InetSocketError::BadFileDescriptor)?;
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "af-inet-connect-begin",
        token,
        ((addr[0] as u64) << 24)
            | ((addr[1] as u64) << 16)
            | ((addr[2] as u64) << 8)
            | addr[3] as u64,
    );
    virtio_net::connect_tcp(token, addr, port)?;
    socket.remote_addr = addr;
    socket.remote_port = port;
    socket.state = InetSocketState::Connected;
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "af-inet-connect-done",
        token,
        port as u64,
    );
    crate::debug::info!(
        driver,
        "AF_INET TCP connect succeeded remote={}.{}.{}.{}:{} type={} protocol={}",
        addr[0],
        addr[1],
        addr[2],
        addr[3],
        port,
        socket.type_,
        socket.protocol
    );
    Ok(())
}

pub fn send_inet_socket(token: u64, bytes: &[u8]) -> Result<usize, InetSocketError> {
    let sockets = INET_SOCKETS.lock();
    let socket = sockets
        .iter()
        .find(|socket| socket.token == token)
        .ok_or(InetSocketError::BadFileDescriptor)?;
    if socket.state != InetSocketState::Connected {
        return Err(InetSocketError::NotConnected);
    }
    virtio_net::send_tcp(token, bytes)
}

pub fn recv_inet_socket(
    token: u64,
    out: &mut [u8],
    nonblocking: bool,
) -> Result<usize, InetSocketError> {
    let sockets = INET_SOCKETS.lock();
    let socket = sockets
        .iter()
        .find(|socket| socket.token == token)
        .ok_or(InetSocketError::BadFileDescriptor)?;
    if socket.state != InetSocketState::Connected {
        return Err(InetSocketError::NotConnected);
    }
    drop(sockets);
    match virtio_net::recv_tcp(token, out, nonblocking) {
        Ok(read) => return Ok(read),
        Err(InetSocketError::TryAgain) => {}
        Err(error) => return Err(error),
    }
    let mut sockets = INET_SOCKETS.lock();
    let socket = sockets
        .iter_mut()
        .find(|socket| socket.token == token)
        .ok_or(InetSocketError::BadFileDescriptor)?;
    if socket.rx.is_empty() {
        return if nonblocking {
            Err(InetSocketError::TryAgain)
        } else {
            Ok(0)
        };
    }
    let count = out.len().min(socket.rx.len());
    for dest in out.iter_mut().take(count) {
        *dest = socket.rx.pop_front().unwrap_or(0);
    }
    Ok(count)
}

pub fn inet_readable_bytes(token: u64) -> Result<usize, InetSocketError> {
    if let Ok(len) = virtio_net::readable_tcp_bytes(token) {
        return Ok(len);
    }
    let sockets = INET_SOCKETS.lock();
    let socket = sockets
        .iter()
        .find(|socket| socket.token == token)
        .ok_or(InetSocketError::BadFileDescriptor)?;
    Ok(socket.rx.len())
}

pub fn inet_socket_writable(token: u64) -> bool {
    if virtio_net::tcp_may_send(token) {
        return true;
    }
    INET_SOCKETS
        .lock()
        .iter()
        .any(|socket| socket.token == token && socket.state == InetSocketState::Connected)
}
