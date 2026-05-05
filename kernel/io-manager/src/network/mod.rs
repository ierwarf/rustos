use core::sync::atomic::{AtomicBool, Ordering};

pub const STATIC_IPV4_ADDR: [u8; 4] = [10, 0, 2, 15];
pub const STATIC_IPV4_GATEWAY: [u8; 4] = [10, 0, 2, 2];
pub const STATIC_IPV4_PREFIX_LEN: u8 = 24;
pub const STATIC_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

static VIRTIO_NET_DRIVER_REGISTERED: AtomicBool = AtomicBool::new(false);
static NETDEV_REGISTERED: AtomicBool = AtomicBool::new(false);
static LINK_UP: AtomicBool = AtomicBool::new(false);

pub(crate) fn note_virtio_net_driver_registered() {
    VIRTIO_NET_DRIVER_REGISTERED.store(true, Ordering::Release);
    crate::debug::info!(driver, "virtio_net driver registered");
}

pub(crate) fn note_netdev_registered() {
    NETDEV_REGISTERED.store(true, Ordering::Release);
    LINK_UP.store(true, Ordering::Release);
    crate::debug::info!(
        driver,
        "netdev registered link up ipv4={}.{}.{}.{}/{} gateway={}.{}.{}.{} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
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
}

pub fn virtio_net_driver_registered() -> bool {
    VIRTIO_NET_DRIVER_REGISTERED.load(Ordering::Acquire)
}

pub fn netdev_registered() -> bool {
    NETDEV_REGISTERED.load(Ordering::Acquire)
}

pub fn link_up() -> bool {
    LINK_UP.load(Ordering::Acquire)
}
