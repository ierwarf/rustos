//! Read-only discovery for a RustOS x86_64 HVM guest.
//!
//! This module deliberately establishes no Xen transport. In particular, a
//! Xen domain ID is an observation, not an authorization decision; L0 must
//! bind and authorize every future DVM endpoint.

use core::arch::x86_64::__cpuid;
use x86_64::registers::model_specific::Msr;

const HYPERVISOR_CPUID_FIRST: u32 = 0x4000_0000;
const HYPERVISOR_CPUID_LIMIT_EXCLUSIVE: u32 = 0x4001_0000;
const HYPERVISOR_CPUID_STRIDE: u32 = 0x100;

const XEN_SIGNATURE_EBX: u32 = 0x566e_6558; // "XenV"
const XEN_SIGNATURE_ECX: u32 = 0x6558_4d4d; // "MMXe"
const XEN_SIGNATURE_EDX: u32 = 0x4d4d_566e; // "nVMM"

const XEN_VERSION_LEAF: u32 = 1;
const XEN_HYPERCALL_LEAF: u32 = 2;
const XEN_HVM_FEATURE_LEAF: u32 = 4;

const XEN_HVM_CPUID_VCPU_ID_PRESENT: u32 = 1 << 3;
const XEN_HVM_CPUID_DOMID_PRESENT: u32 = 1 << 4;

pub const HYPERCALL_PAGE_SIZE: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XenHypercallPageInfo {
    page_count: u32,
    msr_base: u32,
    feature_flags: u32,
}

impl XenHypercallPageInfo {
    pub const fn page_count(self) -> u32 {
        self.page_count
    }

    /// The Xen-specific MSR base advertised by CPUID.
    ///
    /// This is only discovery data. Creating a hypercall page additionally
    /// requires a dedicated executable page and a reviewed MSR write path.
    pub const fn msr_base(self) -> u32 {
        self.msr_base
    }

    pub const fn feature_flags(self) -> u32 {
        self.feature_flags
    }

    /// Installs one Xen-generated hypercall page at `guest_phys_addr`.
    ///
    /// The page must be private, writable guest RAM during the MSR write. The
    /// caller must change it to executable and non-writable before any future
    /// call can use it. This function creates no Xen endpoint and issues no
    /// hypercall itself.
    ///
    /// # Safety
    ///
    /// `guest_phys_addr` must identify a dedicated, page-aligned guest RAM
    /// page that remains owned by the caller for the lifetime of the domain.
    pub unsafe fn install(self, guest_phys_addr: u64) -> Result<XenHypercallPage, XenError> {
        validate_hypercall_page_address(guest_phys_addr)?;
        unsafe { Msr::new(self.msr_base).write(guest_phys_addr) };
        Ok(XenHypercallPage { guest_phys_addr })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XenHypercallPage {
    guest_phys_addr: u64,
}

impl XenHypercallPage {
    pub const fn guest_phys_addr(self) -> u64 {
        self.guest_phys_addr
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XenError {
    UnalignedHypercallPageAddress(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XenHvmInfo {
    cpuid_base: u32,
    version_major: Option<u16>,
    version_minor: Option<u16>,
    hypercall: Option<XenHypercallPageInfo>,
    hvm_feature_flags: Option<u32>,
    vcpu_id: Option<u32>,
    domain_id: Option<u16>,
}

impl XenHvmInfo {
    pub const fn cpuid_base(self) -> u32 {
        self.cpuid_base
    }

    pub const fn version(self) -> Option<(u16, u16)> {
        match (self.version_major, self.version_minor) {
            (Some(major), Some(minor)) => Some((major, minor)),
            _ => None,
        }
    }

    pub const fn hypercall(self) -> Option<XenHypercallPageInfo> {
        self.hypercall
    }

    pub const fn hvm_feature_flags(self) -> Option<u32> {
        self.hvm_feature_flags
    }

    pub const fn vcpu_id(self) -> Option<u32> {
        self.vcpu_id
    }

    /// Xen-reported domain identity for diagnostics only.
    ///
    /// Never treat this value as a capability or use it to select a DVM.
    pub const fn domain_id(self) -> Option<u16> {
        self.domain_id
    }
}

/// Returns Xen HVM discovery data without issuing a hypercall or changing any
/// guest-visible state.
pub fn probe_hvm() -> Option<XenHvmInfo> {
    let mut base = HYPERVISOR_CPUID_FIRST;
    while base < HYPERVISOR_CPUID_LIMIT_EXCLUSIVE {
        let signature = __cpuid(base);
        if signature.ebx == XEN_SIGNATURE_EBX
            && signature.ecx == XEN_SIGNATURE_ECX
            && signature.edx == XEN_SIGNATURE_EDX
        {
            return xen_info_from_cpuid(base, signature.eax, __cpuid);
        }
        base += HYPERVISOR_CPUID_STRIDE;
    }
    None
}

fn xen_info_from_cpuid(
    base: u32,
    max_leaf: u32,
    cpuid: impl Fn(u32) -> core::arch::x86_64::CpuidResult,
) -> Option<XenHvmInfo> {
    if max_leaf < base {
        return None;
    }

    let version = (max_leaf >= base + XEN_VERSION_LEAF).then(|| cpuid(base + XEN_VERSION_LEAF));
    let hypercall = (max_leaf >= base + XEN_HYPERCALL_LEAF)
        .then(|| cpuid(base + XEN_HYPERCALL_LEAF))
        .and_then(|leaf| {
            (leaf.eax != 0 && leaf.ebx != 0).then_some(XenHypercallPageInfo {
                page_count: leaf.eax,
                msr_base: leaf.ebx,
                feature_flags: leaf.ecx,
            })
        });
    let hvm = (max_leaf >= base + XEN_HVM_FEATURE_LEAF).then(|| cpuid(base + XEN_HVM_FEATURE_LEAF));

    let hvm_feature_flags = hvm.map(|leaf| leaf.eax);
    let vcpu_id =
        hvm.and_then(|leaf| (leaf.eax & XEN_HVM_CPUID_VCPU_ID_PRESENT != 0).then_some(leaf.ebx));
    let domain_id = hvm
        .and_then(|leaf| (leaf.eax & XEN_HVM_CPUID_DOMID_PRESENT != 0).then_some(leaf.ecx as u16));

    Some(XenHvmInfo {
        cpuid_base: base,
        version_major: version.map(|leaf| (leaf.eax >> 16) as u16),
        version_minor: version.map(|leaf| leaf.eax as u16),
        hypercall,
        hvm_feature_flags,
        vcpu_id,
        domain_id,
    })
}

fn validate_hypercall_page_address(guest_phys_addr: u64) -> Result<(), XenError> {
    (guest_phys_addr % HYPERCALL_PAGE_SIZE as u64 == 0)
        .then_some(())
        .ok_or(XenError::UnalignedHypercallPageAddress(guest_phys_addr))
}

#[cfg(test)]
mod tests {
    use core::arch::x86_64::CpuidResult;

    use super::{XEN_HVM_CPUID_DOMID_PRESENT, XEN_HVM_CPUID_VCPU_ID_PRESENT, xen_info_from_cpuid};

    const BASE: u32 = 0x4000_0100;

    fn leaf(eax: u32, ebx: u32, ecx: u32, edx: u32) -> CpuidResult {
        CpuidResult { eax, ebx, ecx, edx }
    }

    #[test]
    fn decodes_xen_hvm_cpuid_data_without_authorizing_the_domain() {
        let info = xen_info_from_cpuid(BASE, BASE + 4, |index| match index - BASE {
            1 => leaf((4 << 16) | 19, 0, 0, 0),
            2 => leaf(2, 0x400, 0x4, 0),
            4 => leaf(
                XEN_HVM_CPUID_VCPU_ID_PRESENT | XEN_HVM_CPUID_DOMID_PRESENT,
                7,
                42,
                0,
            ),
            _ => leaf(0, 0, 0, 0),
        })
        .unwrap();

        assert_eq!(info.cpuid_base(), BASE);
        assert_eq!(info.version(), Some((4, 19)));
        assert_eq!(info.hypercall().unwrap().page_count(), 2);
        assert_eq!(info.hypercall().unwrap().msr_base(), 0x400);
        assert_eq!(info.vcpu_id(), Some(7));
        assert_eq!(info.domain_id(), Some(42));
    }

    #[test]
    fn omits_optional_leaves_instead_of_inventing_a_transport() {
        let info = xen_info_from_cpuid(BASE, BASE, |_| leaf(0, 0, 0, 0)).unwrap();

        assert_eq!(info.version(), None);
        assert_eq!(info.hypercall(), None);
        assert_eq!(info.hvm_feature_flags(), None);
        assert_eq!(info.vcpu_id(), None);
        assert_eq!(info.domain_id(), None);
    }

    #[test]
    fn rejects_a_maximum_leaf_below_the_xen_base() {
        assert!(xen_info_from_cpuid(BASE, BASE - 1, |_| leaf(0, 0, 0, 0)).is_none());
    }

    #[test]
    fn rejects_an_unaligned_hypercall_page_before_any_msr_write() {
        assert!(super::validate_hypercall_page_address(0x1000).is_ok());
        assert_eq!(
            super::validate_hypercall_page_address(0x1001),
            Err(super::XenError::UnalignedHypercallPageAddress(0x1001)),
        );
    }
}
