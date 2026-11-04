// SPDX-License-Identifier: MIT
//! Moved verbatim from the former single-file crate root; see `lib.rs`.

#[allow(unused_imports)]
use crate::*;

/// A fixed two-ring Ethernet transport. RustOS is the producer of `tx` and
/// consumer of `rx`; the Linux DVM has the inverse ownership. No peer may
/// select an address, descriptor chain, or allocation outside this aperture.
pub const DVM_NET_MAGIC: [u8; 8] = *b"RSDVMNT1";
pub const DVM_NET_VERSION: u32 = 1;
pub const DVM_NET_HEADER_BYTES: u32 = 4096;
pub const DVM_NET_RECORD_BYTES: usize = 64;
pub const DVM_NET_SLOT_COUNT: u32 = 64;
pub const DVM_NET_SLOT_BYTES: u32 = 2048;
pub const DVM_NET_MTU: u32 = 1514;
/// Set only by the host when it has created a valid, fixed-layout aperture.
pub const DVM_NET_FLAG_READY: u32 = 1;
/// Set by the Linux relay only after it has opened both the aperture and its
/// DVM-owned NIC. RustOS must not regard the transport as usable before this.
pub const DVM_NET_FLAG_DVM_READY: u32 = 1 << 1;
pub const DVM_NET_KNOWN_FLAGS: u32 = DVM_NET_FLAG_READY | DVM_NET_FLAG_DVM_READY;
pub const DVM_NET_MIN_REGION_BYTES: u64 =
    DVM_NET_HEADER_BYTES as u64 + 2 * DVM_NET_SLOT_COUNT as u64 * DVM_NET_SLOT_BYTES as u64;
/// PCI BARs must be powers of two. This leaves the tail reserved while keeping
/// the two fixed rings bounded to `DVM_NET_MIN_REGION_BYTES`.
pub const DVM_NET_APERTURE_BYTES: u64 = 512 * 1024;

pub(crate) const ETHERNET_HEADER_BYTES: usize = 14;
pub(crate) const ETHERTYPE_IPV4: u16 = 0x0800;
pub(crate) const ETHERTYPE_ARP: u16 = 0x0806;
pub(crate) const IPV4_MIN_HEADER_BYTES: usize = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EthernetFrameError {
    Length,
    UnsupportedEtherType,
    InvalidArp,
    InvalidIpv4,
    InvalidIpv4Checksum,
    FragmentedIpv4,
}

/// Validate the complete Ethernet payload accepted by the enabled RustOS
/// network topology.  The netd stack is IPv4-only, so admitting an unknown
/// EtherType or fragmented datagram would create an unmodelled payload path.
/// Ethernet padding is allowed after the IPv4 total length.
pub fn validate_dvm_ethernet_frame(frame: &[u8]) -> Result<(), EthernetFrameError> {
    if !(ETHERNET_HEADER_BYTES..=DVM_NET_MTU as usize).contains(&frame.len()) {
        return Err(EthernetFrameError::Length);
    }
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    match ethertype {
        ETHERTYPE_ARP => validate_arp_payload(&frame[ETHERNET_HEADER_BYTES..]),
        ETHERTYPE_IPV4 => validate_ipv4_payload(&frame[ETHERNET_HEADER_BYTES..]),
        _ => Err(EthernetFrameError::UnsupportedEtherType),
    }
}

pub(crate) fn validate_arp_payload(payload: &[u8]) -> Result<(), EthernetFrameError> {
    if payload.len() < 28
        || payload[0..2] != 1_u16.to_be_bytes()
        || payload[2..4] != ETHERTYPE_IPV4.to_be_bytes()
        || payload[4] != 6
        || payload[5] != 4
        || !matches!(u16::from_be_bytes([payload[6], payload[7]]), 1 | 2)
    {
        return Err(EthernetFrameError::InvalidArp);
    }
    Ok(())
}

pub(crate) fn validate_ipv4_payload(payload: &[u8]) -> Result<(), EthernetFrameError> {
    if payload.len() < IPV4_MIN_HEADER_BYTES || payload[0] >> 4 != 4 {
        return Err(EthernetFrameError::InvalidIpv4);
    }
    let header_len = usize::from(payload[0] & 0x0f) * 4;
    if header_len < IPV4_MIN_HEADER_BYTES || header_len > payload.len() {
        return Err(EthernetFrameError::InvalidIpv4);
    }
    let total_len = usize::from(u16::from_be_bytes([payload[2], payload[3]]));
    if total_len < header_len || total_len > payload.len() {
        return Err(EthernetFrameError::InvalidIpv4);
    }
    let fragment = u16::from_be_bytes([payload[6], payload[7]]);
    if fragment & 0x3fff != 0 {
        return Err(EthernetFrameError::FragmentedIpv4);
    }
    let mut sum = 0_u32;
    for chunk in payload[..header_len].chunks_exact(2) {
        sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
        sum = (sum & 0xffff) + (sum >> 16);
    }
    if sum != 0xffff {
        return Err(EthernetFrameError::InvalidIpv4Checksum);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmNetHeader {
    pub region_bytes: u64,
    pub slot_count: u32,
    pub slot_bytes: u32,
    pub mtu: u32,
    pub flags: u32,
    pub generation: u64,
}

impl DvmNetHeader {
    pub const fn new(region_bytes: u64, generation: u64) -> Self {
        Self {
            region_bytes,
            slot_count: DVM_NET_SLOT_COUNT,
            slot_bytes: DVM_NET_SLOT_BYTES,
            mtu: DVM_NET_MTU,
            flags: DVM_NET_FLAG_READY,
            generation,
        }
    }

    pub const fn encoded_len() -> usize {
        DVM_NET_RECORD_BYTES
    }

    pub const fn ring_bytes(self) -> u64 {
        self.slot_count as u64 * self.slot_bytes as u64
    }

    pub const fn tx_ring_offset() -> u64 {
        DVM_NET_HEADER_BYTES as u64
    }

    pub fn rx_ring_offset(self) -> Option<u64> {
        Self::tx_ring_offset().checked_add(self.ring_bytes())
    }

    pub fn encode(self) -> [u8; DVM_NET_RECORD_BYTES] {
        let mut bytes = [0_u8; DVM_NET_RECORD_BYTES];
        bytes[0..8].copy_from_slice(&DVM_NET_MAGIC);
        bytes[8..12].copy_from_slice(&DVM_NET_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&DVM_NET_HEADER_BYTES.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.region_bytes.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.slot_count.to_le_bytes());
        bytes[28..32].copy_from_slice(&self.slot_bytes.to_le_bytes());
        bytes[32..36].copy_from_slice(&self.mtu.to_le_bytes());
        bytes[36..40].copy_from_slice(&self.flags.to_le_bytes());
        bytes[56..64].copy_from_slice(&self.generation.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_NET_RECORD_BYTES]) -> Option<Self> {
        if bytes[0..8] != DVM_NET_MAGIC
            || read_u32(bytes, 8)? != DVM_NET_VERSION
            || read_u32(bytes, 12)? != DVM_NET_HEADER_BYTES
        {
            return None;
        }
        let header = Self {
            region_bytes: read_u64(bytes, 16)?,
            slot_count: read_u32(bytes, 24)?,
            slot_bytes: read_u32(bytes, 28)?,
            mtu: read_u32(bytes, 32)?,
            flags: read_u32(bytes, 36)?,
            generation: read_u64(bytes, 56)?,
        };
        header.is_valid().then_some(header)
    }

    pub fn is_valid(self) -> bool {
        if self.flags & DVM_NET_FLAG_READY == 0
            || self.flags & !DVM_NET_KNOWN_FLAGS != 0
            || self.generation == 0
            || self.slot_count != DVM_NET_SLOT_COUNT
            || self.slot_bytes != DVM_NET_SLOT_BYTES
            || self.mtu != DVM_NET_MTU
            || self.slot_bytes <= self.mtu
        {
            return false;
        }
        self.region_bytes >= DVM_NET_MIN_REGION_BYTES
    }

    pub const fn dvm_ready(self) -> bool {
        self.flags & DVM_NET_FLAG_DVM_READY != 0
    }
}
