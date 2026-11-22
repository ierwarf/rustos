// SPDX-License-Identifier: MIT
//! Checked, address-free description of a DVM app-buffer allocation.
//!
//! A record is not authority. It must be bound to an authenticated provider
//! generation and an independently retained RustOS allocation capability.

/// The GUI/atlas aperture is one 512 MiB virtio-pmem; the separate writable
/// app-buffer aperture follows it directly.
pub const DVM_PIXEL_APERTURE_BYTES: u64 = 512 * 1024 * 1024;
/// Both virtio-pmem windows live in QEMU's device-memory region, which starts at
/// the first 1 GiB boundary at or above the end of guest RAM (and never below
/// 4 GiB). The RustOS kernel and the host launcher derive the same base from the
/// guest's RAM top, so the guest RAM size is not a hidden constant.
pub const DVM_PMEM_MIN_BASE: u64 = 4 * 1024 * 1024 * 1024;
pub const DVM_PMEM_BASE_ALIGN: u64 = 1024 * 1024 * 1024;

/// Guest-physical base of the pixel aperture for a guest whose RAM ends at
/// `ram_top`. `None` when rounding overflows.
pub const fn dvm_pmem_base(ram_top: u64) -> Option<u64> {
    let Some(rounded) = ram_top.checked_add(DVM_PMEM_BASE_ALIGN - 1) else {
        return None;
    };
    let aligned = rounded & !(DVM_PMEM_BASE_ALIGN - 1);
    Some(if aligned < DVM_PMEM_MIN_BASE { DVM_PMEM_MIN_BASE } else { aligned })
}

/// Guest-physical base of the separate shared app-buffer aperture.
pub const fn dvm_app_buffer_phys_addr(pmem_base: u64) -> Option<u64> {
    pmem_base.checked_add(DVM_PIXEL_APERTURE_BYTES)
}
pub const DVM_APP_BUFFER_APERTURE_BYTES: u64 = 512 * 1024 * 1024;
pub const DVM_APP_BUFFER_PAGE_BYTES: u64 = 4096;
pub const DVM_APP_BUFFER_RECORD_BYTES: usize = 64;
pub const DVM_APP_BUFFER_MAGIC: [u8; 8] = *b"RSAPPB01";
pub const DVM_APP_BUFFER_VERSION: u32 = 1;
pub const DVM_APP_BUFFER_FORMAT_XRGB8888: u32 = 0x3432_5258;
pub const DVM_APP_BUFFER_FORMAT_ARGB8888: u32 = 0x3432_5241;
pub const DVM_APP_BUFFER_MAX_DIMENSION: u32 = 8192;

/// Dedicated mailbox in the GUI ivshmem control page, independent of atlas
/// records. The host publishes a monotonically increasing sequence last; the
/// DVM acknowledges that exact sequence last after writing its result.
pub const DVM_APP_BUFFER_REQUEST_SEQUENCE_OFFSET: usize = 2200;
pub const DVM_APP_BUFFER_REQUEST_KIND_OFFSET: usize = 2208;
pub const DVM_APP_BUFFER_REQUEST_RECORD_OFFSET: usize = 2216;
pub const DVM_APP_BUFFER_RESPONSE_STATUS_OFFSET: usize = 2280;
pub const DVM_APP_BUFFER_RESPONSE_SEQUENCE_OFFSET: usize = 2288;
pub const DVM_APP_BUFFER_REQUEST_EXPORT: u64 = 1;
pub const DVM_APP_BUFFER_REQUEST_REVOKE: u64 = 2;


/// Version 1 supports one plane and DRM_FORMAT_MOD_LINEAR only. `length`
/// includes the page-rounded allocation, while `stride * height` bounds the
/// image. Allocation identity is unique within the provider generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmAppBuffer {
    pub provider_generation: u64,
    pub allocation: u64,
    pub offset: u64,
    pub length: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
}

impl DvmAppBuffer {
    pub fn is_valid(self) -> bool {
        let Some(end) = self.offset.checked_add(self.length) else {
            return false;
        };
        let Some(row) = self.width.checked_mul(4) else {
            return false;
        };
        let Some(image_bytes) = u64::from(self.stride).checked_mul(u64::from(self.height)) else {
            return false;
        };
        self.provider_generation != 0
            && self.allocation != 0
            && self.offset.is_multiple_of(DVM_APP_BUFFER_PAGE_BYTES)
            && self.length != 0
            && self.length.is_multiple_of(DVM_APP_BUFFER_PAGE_BYTES)
            && end <= DVM_APP_BUFFER_APERTURE_BYTES
            && self.width != 0
            && self.width <= DVM_APP_BUFFER_MAX_DIMENSION
            && self.height != 0
            && self.height <= DVM_APP_BUFFER_MAX_DIMENSION
            && self.stride >= row
            && self.stride.is_multiple_of(4)
            && image_bytes <= self.length
            && matches!(
                self.format,
                DVM_APP_BUFFER_FORMAT_XRGB8888 | DVM_APP_BUFFER_FORMAT_ARGB8888
            )
    }

    pub fn encode(self) -> Option<[u8; DVM_APP_BUFFER_RECORD_BYTES]> {
        if !self.is_valid() {
            return None;
        }
        let mut bytes = [0; DVM_APP_BUFFER_RECORD_BYTES];
        bytes[0..8].copy_from_slice(&DVM_APP_BUFFER_MAGIC);
        bytes[8..12].copy_from_slice(&DVM_APP_BUFFER_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&(DVM_APP_BUFFER_RECORD_BYTES as u32).to_le_bytes());
        bytes[16..24].copy_from_slice(&self.provider_generation.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.allocation.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.offset.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.length.to_le_bytes());
        bytes[48..52].copy_from_slice(&self.width.to_le_bytes());
        bytes[52..56].copy_from_slice(&self.height.to_le_bytes());
        bytes[56..60].copy_from_slice(&self.stride.to_le_bytes());
        bytes[60..64].copy_from_slice(&self.format.to_le_bytes());
        Some(bytes)
    }

    pub fn decode(bytes: &[u8; DVM_APP_BUFFER_RECORD_BYTES]) -> Option<Self> {
        fn u32_at(b: &[u8], i: usize) -> Option<u32> {
            Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
        }
        fn u64_at(b: &[u8], i: usize) -> Option<u64> {
            Some(u64::from_le_bytes(b.get(i..i + 8)?.try_into().ok()?))
        }
        if bytes[0..8] != DVM_APP_BUFFER_MAGIC
            || u32_at(bytes, 8)? != DVM_APP_BUFFER_VERSION
            || u32_at(bytes, 12)? != DVM_APP_BUFFER_RECORD_BYTES as u32
        {
            return None;
        }
        let record = Self {
            provider_generation: u64_at(bytes, 16)?,
            allocation: u64_at(bytes, 24)?,
            offset: u64_at(bytes, 32)?,
            length: u64_at(bytes, 40)?,
            width: u32_at(bytes, 48)?,
            height: u32_at(bytes, 52)?,
            stride: u32_at(bytes, 56)?,
            format: u32_at(bytes, 60)?,
        };
        record.is_valid().then_some(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn valid() -> DvmAppBuffer {
        DvmAppBuffer {
            provider_generation: 1,
            allocation: 7,
            offset: 4096,
            length: 8192,
            width: 32,
            height: 32,
            stride: 128,
            format: DVM_APP_BUFFER_FORMAT_XRGB8888,
        }
    }
    #[test]
    fn valid_linear_record_round_trips() {
        let r = valid();
        assert_eq!(DvmAppBuffer::decode(&r.encode().unwrap()), Some(r));
    }
    #[test]
    fn invalid_identity_layout_format_and_bounds_are_rejected() {
        let r = valid();
        for bad in [
            DvmAppBuffer {
                provider_generation: 0,
                ..r
            },
            DvmAppBuffer { allocation: 0, ..r },
            DvmAppBuffer {
                offset: u64::MAX - 4095,
                length: 8192,
                ..r
            },
            DvmAppBuffer { offset: 1, ..r },
            DvmAppBuffer { length: 0, ..r },
            DvmAppBuffer {
                offset: DVM_APP_BUFFER_APERTURE_BYTES,
                ..r
            },
            DvmAppBuffer { width: 0, ..r },
            DvmAppBuffer { height: 8193, ..r },
            DvmAppBuffer { stride: 124, ..r },
            DvmAppBuffer {
                stride: 132,
                length: 4096,
                ..r
            },
            DvmAppBuffer { format: 0, ..r },
        ] {
            assert!(!bad.is_valid(), "{bad:?}");
            assert!(bad.encode().is_none());
        }
        let mut bytes = r.encode().unwrap();
        bytes[8] = 2;
        assert_eq!(DvmAppBuffer::decode(&bytes), None);
    }

    #[test]
    fn pmem_windows_start_at_the_first_gib_boundary_at_or_above_ram() {
        const GIB: u64 = 1 << 30;
        // RAM below 4 GiB keeps the historical 4 GiB base and the 0x1_2000_0000 app aperture.
        assert_eq!(dvm_pmem_base(2 * GIB), Some(4 * GIB));
        assert_eq!(dvm_pmem_base(0), Some(4 * GIB));
        assert_eq!(dvm_app_buffer_phys_addr(4 * GIB), Some(0x1_2000_0000));
        // RAM that ends above 4 GiB pushes the windows to the next GiB boundary.
        assert_eq!(dvm_pmem_base(4 * GIB), Some(4 * GIB));
        assert_eq!(dvm_pmem_base(8 * GIB), Some(8 * GIB));
        assert_eq!(dvm_pmem_base(8 * GIB - 4096), Some(8 * GIB));
        assert_eq!(dvm_pmem_base(8 * GIB + 4096), Some(9 * GIB));
        assert_eq!(dvm_pmem_base(u64::MAX), None);
        assert_eq!(dvm_app_buffer_phys_addr(u64::MAX), None);
    }
}
