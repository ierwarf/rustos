//! Pure mmap request classification.
//!
//! Classification is intentionally separate from address-space mutation so a
//! fixed mapping cannot remove an existing VMA before its flags and backing
//! descriptor have been accepted.
//!
//! - **Owner:** `syscalld` owns Linux mmap flag/backing classification; the
//!   kernel memory manager owns only the admitted mapping mechanism.
//! - **Boundary:** Protection, sharing, anonymous, and descriptor-kind fields
//!   are mutually validated before any address-space mutation.
//! - **Lifecycle:** Parse flags, classify the backing, produce a side-effect-free
//!   plan, then let the caller commit or discard that plan atomically.
//! - **Concurrency:** Planning is pure and lock-free; generation checks belong
//!   to the later commit boundary.
//! - **Failure:** Ambiguous flags, unsupported backing combinations, and denied
//!   shared mappings return a typed error without changing VMAs.
//! - **Forbidden:** No replace-before-validate, implicit device mapping,
//!   executable fallback, or silently widened compatibility.
//! - **Evidence:** `mm-broker`, `process-address-space`, and
//!   `linux-syscall-offload`.

pub const MAP_TYPE: u64 = 0x0f;
pub const MAP_SHARED: u64 = 0x01;
pub const MAP_PRIVATE: u64 = 0x02;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MmapFdKind {
    File,
    Memfd,
    DisplaySurface,
    Device,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MmapSharing {
    Private,
    Shared,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MmapPlan {
    Reserved,
    Anonymous,
    FilePrivate,
    MemfdShared,
    DeviceShared,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MmapPlanError {
    InvalidFlags,
    InvalidBacking,
    AccessDenied,
}

pub fn parse_mmap_sharing(flags: u64) -> Result<MmapSharing, MmapPlanError> {
    match flags & MAP_TYPE {
        MAP_PRIVATE => Ok(MmapSharing::Private),
        MAP_SHARED => Ok(MmapSharing::Shared),
        _ => Err(MmapPlanError::InvalidFlags),
    }
}

pub fn plan_mmap(
    anonymous: bool,
    sharing: MmapSharing,
    prot_none: bool,
    fd_kind: Option<MmapFdKind>,
) -> Result<MmapPlan, MmapPlanError> {
    match (anonymous, sharing, fd_kind) {
        (true, MmapSharing::Private, None) if prot_none => Ok(MmapPlan::Reserved),
        (true, MmapSharing::Private, None) => Ok(MmapPlan::Anonymous),
        (false, MmapSharing::Private, Some(MmapFdKind::File)) => Ok(MmapPlan::FilePrivate),
        (false, MmapSharing::Private, Some(_)) => Err(MmapPlanError::InvalidBacking),
        (false, MmapSharing::Shared, Some(MmapFdKind::Memfd)) => Ok(MmapPlan::MemfdShared),
        (false, MmapSharing::Shared, Some(MmapFdKind::DisplaySurface | MmapFdKind::Device)) => {
            Ok(MmapPlan::DeviceShared)
        }
        (false, MmapSharing::Shared, Some(_)) => Err(MmapPlanError::AccessDenied),
        _ => Err(MmapPlanError::InvalidFlags),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_backing_is_rejected_before_a_fixed_replace_plan_exists() {
        assert_eq!(
            plan_mmap(false, MmapSharing::Private, false, Some(MmapFdKind::Device)),
            Err(MmapPlanError::InvalidBacking)
        );
        assert_eq!(
            plan_mmap(false, MmapSharing::Shared, false, Some(MmapFdKind::File)),
            Err(MmapPlanError::AccessDenied)
        );
    }

    #[test]
    fn accepted_backings_produce_exact_broker_plans() {
        assert_eq!(
            plan_mmap(true, MmapSharing::Private, false, None),
            Ok(MmapPlan::Anonymous)
        );
        assert_eq!(
            plan_mmap(true, MmapSharing::Private, true, None),
            Ok(MmapPlan::Reserved)
        );
        assert_eq!(
            plan_mmap(false, MmapSharing::Private, false, Some(MmapFdKind::File)),
            Ok(MmapPlan::FilePrivate)
        );
        assert_eq!(
            plan_mmap(false, MmapSharing::Shared, false, Some(MmapFdKind::Memfd)),
            Ok(MmapPlan::MemfdShared)
        );
    }

    #[test]
    fn missing_or_ambiguous_mapping_type_is_rejected() {
        assert_eq!(parse_mmap_sharing(0), Err(MmapPlanError::InvalidFlags));
        assert_eq!(parse_mmap_sharing(0x03), Err(MmapPlanError::InvalidFlags));
    }

    #[test]
    fn anonymous_shared_mapping_is_not_silently_reclassified_private() {
        assert_eq!(
            plan_mmap(true, MmapSharing::Shared, false, None),
            Err(MmapPlanError::InvalidFlags)
        );
    }
}
