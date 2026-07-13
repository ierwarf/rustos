//! RustOS network transport is DVM-only. Linux owns the virtio-net device;
//! RustOS consumes only the fixed, validated shared-memory frame ring.

pub const PACKET_MTU: usize = rustos_user_abi::syscall::NET_BROKER_PACKET_MTU;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketError {
    NoDevice,
    Invalid,
    Busy,
    TooLarge,
    WouldBlock,
}

pub fn available() -> bool {
    crate::io::dvm_network::available()
}

pub fn transmit_frame(frame: &[u8]) -> Result<usize, PacketError> {
    if frame.is_empty() {
        return Ok(0);
    }
    if frame.len() > PACKET_MTU {
        return Err(PacketError::TooLarge);
    }
    crate::io::dvm_network::transmit(frame)
}

pub fn receive_frame(out: &mut [u8]) -> Result<usize, PacketError> {
    if out.is_empty() {
        return Err(PacketError::Invalid);
    }
    crate::io::dvm_network::receive(out)
}
