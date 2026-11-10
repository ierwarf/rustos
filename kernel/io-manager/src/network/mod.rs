//! RustOS network transport is DVM-only. Linux owns the virtio-net device;
//! RustOS consumes only the fixed, validated shared-memory frame ring.

pub const PACKET_MTU: usize = rustos_user_abi::syscall::NET_BROKER_PACKET_MTU;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u64)]
pub enum PacketTransportStatus {
    Unavailable = rustos_user_abi::syscall::NET_BROKER_PACKET_STATUS_UNAVAILABLE,
    AwaitingAuthenticatedControl =
        rustos_user_abi::syscall::NET_BROKER_PACKET_STATUS_AWAITING_AUTHENTICATED_CONTROL,
    Active = rustos_user_abi::syscall::NET_BROKER_PACKET_STATUS_ACTIVE,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketError {
    NoDevice,
    Invalid,
    Busy,
    TooLarge,
    WouldBlock,
    Revoked,
}

pub fn available() -> bool {
    transport_status(|| true) == Ok(PacketTransportStatus::Active)
}

pub fn transport_status(authorize: impl FnOnce() -> bool) -> Result<PacketTransportStatus, PacketError> {
    crate::io::dvm_network::transport_status(authorize)
}

pub fn transmit_frame(frame: &[u8], authorize: impl FnOnce() -> bool) -> Result<usize, PacketError> {
    if frame.is_empty() {
        return Ok(0);
    }
    if frame.len() > PACKET_MTU {
        return Err(PacketError::TooLarge);
    }
    crate::io::dvm_network::transmit(frame, authorize)
}

pub fn receive_frame(out: &mut [u8], authorize: impl FnOnce() -> bool) -> Result<usize, PacketError> {
    if out.is_empty() {
        return Err(PacketError::Invalid);
    }
    crate::io::dvm_network::receive(out, authorize)
}
