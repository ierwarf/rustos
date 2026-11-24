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
    DeviceBuffer,
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
    /// Writable `MAP_SHARED` of a file: only a filesystem whose bytes live in
    /// shared frames (TmpFs) can honour it; vfsd refuses every other file and
    /// the broker reports that as `EACCES`, never as a private copy.
    FileShared,
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
    plan_mmap_with_write(anonymous, sharing, prot_none, true, fd_kind)
}

pub fn plan_mmap_with_write(
    anonymous: bool,
    sharing: MmapSharing,
    prot_none: bool,
    _writable: bool,
    fd_kind: Option<MmapFdKind>,
) -> Result<MmapPlan, MmapPlanError> {
    match (anonymous, sharing, fd_kind) {
        (true, MmapSharing::Private, None) if prot_none => Ok(MmapPlan::Reserved),
        (true, MmapSharing::Private, None) => Ok(MmapPlan::Anonymous),
        (false, MmapSharing::Private, Some(MmapFdKind::File)) => Ok(MmapPlan::FilePrivate),
        (false, MmapSharing::Shared, Some(MmapFdKind::File)) => Ok(MmapPlan::FileShared),
        (false, MmapSharing::Private, Some(_)) => Err(MmapPlanError::InvalidBacking),
        (false, MmapSharing::Shared, Some(MmapFdKind::Memfd)) => Ok(MmapPlan::MemfdShared),
        (false, MmapSharing::Shared, Some(MmapFdKind::DisplaySurface | MmapFdKind::Device | MmapFdKind::DeviceBuffer)) => {
            Ok(MmapPlan::DeviceShared)
        }
        (false, MmapSharing::Shared, Some(_)) => Err(MmapPlanError::AccessDenied),
        _ => Err(MmapPlanError::InvalidFlags),
    }
}

/// Whether a planned private file mapping is worth asking the kernel to bind
/// to a shared library object. This is only a hint that saves a vfsd round trip
/// for mappings that can never be served: eligibility itself is vfsd's
/// attestation plus the kernel's sealed-object registry, and a refusal leaves
/// the eager copy path in force. Reserved (PROT_NONE), unreadable and
/// misaligned mappings are never candidates.
pub fn library_bind_candidate(plan: MmapPlan, prot: u64, offset: u64) -> bool {
    const PROT_READ: u64 = 0x1;
    plan == MmapPlan::FilePrivate && prot & PROT_READ != 0 && offset.is_multiple_of(4096)
}

/// Private file mappings at least this long ask the kernel to cache the file's
/// pages for every later mapper (a demand object) when no pager-warmed object
/// covers the file. Smaller mappings only join an object that already exists:
/// below this size the eager copy costs less than the bookkeeping it would
/// share. The kernel enforces the mechanical caps; this is the policy.
pub const LIBRARY_DEMAND_MIN_MAP_BYTES: u64 = 1 << 20;

/// Whether a bind request should carry the demand-object flag.
pub fn library_demand_candidate(plan: MmapPlan, prot: u64, offset: u64, map_len: u64) -> bool {
    library_bind_candidate(plan, prot, offset) && map_len >= LIBRARY_DEMAND_MIN_MAP_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_large_bind_candidates_request_a_demand_object() {
        let big = LIBRARY_DEMAND_MIN_MAP_BYTES;
        assert!(library_demand_candidate(MmapPlan::FilePrivate, 0x1, 0, big));
        // Chrome's image mappings: RO whole-image and RW fixed follow-ups.
        assert!(library_demand_candidate(MmapPlan::FilePrivate, 0x1 | 0x2, 0x1d5c000, 0x1d5c000));
        assert!(!library_demand_candidate(MmapPlan::FilePrivate, 0x1, 0, big - 4096));
        assert!(!library_demand_candidate(MmapPlan::FilePrivate, 0, 0, big));
        assert!(!library_demand_candidate(MmapPlan::FilePrivate, 0x1, 0x2001, big));
        assert!(!library_demand_candidate(MmapPlan::FileShared, 0x1, 0, big));
        assert!(!library_demand_candidate(MmapPlan::Anonymous, 0x1, 0, big));
    }

    #[test]
    fn only_readable_aligned_private_file_mappings_are_bind_candidates() {
        assert!(library_bind_candidate(MmapPlan::FilePrivate, 0x1, 0x2000));
        assert!(library_bind_candidate(MmapPlan::FilePrivate, 0x1 | 0x4, 0));
        assert!(!library_bind_candidate(MmapPlan::FilePrivate, 0, 0));
        assert!(!library_bind_candidate(MmapPlan::FilePrivate, 0x2, 0));
        assert!(!library_bind_candidate(MmapPlan::FilePrivate, 0x1, 0x2001));
        for plan in [
            MmapPlan::Anonymous,
            MmapPlan::Reserved,
            MmapPlan::FileShared,
            MmapPlan::MemfdShared,
            MmapPlan::DeviceShared,
        ] {
            assert!(!library_bind_candidate(plan, 0x1, 0));
        }
    }

    #[test]
    fn device_buffer_maps_shared_only() {
        assert_eq!(
            plan_mmap(false, MmapSharing::Shared, false, Some(MmapFdKind::DeviceBuffer)),
            Ok(MmapPlan::DeviceShared)
        );
        assert_eq!(
            plan_mmap(false, MmapSharing::Private, false, Some(MmapFdKind::DeviceBuffer)),
            Err(MmapPlanError::InvalidBacking)
        );
    }

    #[test]
    fn invalid_backing_is_rejected_before_a_fixed_replace_plan_exists() {
        assert_eq!(
            plan_mmap(false, MmapSharing::Private, false, Some(MmapFdKind::Device)),
            Err(MmapPlanError::InvalidBacking)
        );
        assert_eq!(
            plan_mmap(false, MmapSharing::Shared, false, Some(MmapFdKind::Other)),
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
            plan_mmap_with_write(false, MmapSharing::Shared, false, false, Some(MmapFdKind::File)),
            Ok(MmapPlan::FileShared)
        );
        assert_eq!(
            plan_mmap_with_write(false, MmapSharing::Shared, false, true, Some(MmapFdKind::File)),
            Ok(MmapPlan::FileShared)
        );
        // Read-only / PROT_NONE shared file mappings also use the shared-backing
        // path so writes to mutable files (e.g. SQLite on TmpFs) stay coherent,
        // with read-only fallback to private snapshot only for immutable volumes.
        assert_eq!(
            plan_mmap_with_write(false, MmapSharing::Shared, true, false, Some(MmapFdKind::File)),
            Ok(MmapPlan::FileShared)
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
