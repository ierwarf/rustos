//! Bounded Ring3 SMP qualification ABI.
//!
//! - **Owner:** `rustos-user-abi` owns this fixed wire vocabulary; kernel-compat
//!   owns live identity, endpoint, process-generation, and deadline admission.
//! - **Boundary:** the private `smpqual` workload crosses from sessiond policy
//!   into the kernel through one versioned bind request and milestone records.
//! - **Lifecycle:** a suspended child is bound, admitted, reports bounded
//!   phases, and then becomes terminal; callers cannot widen the window.
//! - **Concurrency:** worker CPU observations are immutable values checked
//!   against the CPU that enters the syscall; this module owns no shared state.
//! - **Failure:** malformed, stale, zero, oversized, or wrong-CPU records are
//!   rejected before they can represent qualification evidence.
//! - **Forbidden:** no ambient PID/path admission, reserved-tail reuse, worker
//!   cardinality widening, or compatibility fallback exists in this wire.
//! - **Evidence:** source-contract mutations and focused ABI/Kani tests.

/// Binds one private, suspended Ring3 SMP qualification workload to the
/// current `sessiond` endpoint generation before it is made runnable.
///
/// The argument is `RustosSmpQualificationBindArgs`. This is deliberately a
/// closed kernel admission ABI rather than an ambient pathname or PID check:
/// ring0 derives the caller and target process generations, address-space
/// generations, and service endpoint epoch itself.
pub const SYS_RUSTOS_SMP_QUALIFICATION_BIND: u64 = 0x5255_0049;

/// KVM-private Ring3 SMP qualification phases. These remain a closed
/// observability vocabulary: the kernel stamps the live PID/TID and accepts a
/// worker record only when its RDTSCP-observed logical CPU equals the CPU
/// executing the syscall.
pub const PRODUCT_MILESTONE_SMP_QUALIFICATION_READY: u64 = 7;
pub const PRODUCT_MILESTONE_SMP_QUALIFICATION_START: u64 = 8;
pub const PRODUCT_MILESTONE_SMP_QUALIFICATION_FINISH: u64 = 9;
pub const PRODUCT_MILESTONE_SMP_QUALIFICATION_COMPLETE: u64 = 10;
pub const SMP_QUALIFICATION_MAX_WORKERS: u32 = 8;
pub const SMP_QUALIFICATION_MAX_WORK_UNITS: u64 = 10_000_000;
/// The KVM qualification evidence window is deliberately short and fixed by
/// the private runtime contract. A caller cannot negotiate a longer window.
pub const SMP_QUALIFICATION_MAX_DEADLINE_MS: u32 = 5_000;
pub const SMP_QUALIFICATION_BIND_ABI_VERSION: u16 = 1;
pub const SMP_QUALIFICATION_WORK_BITS: u32 = 24;
pub const SMP_QUALIFICATION_WORK_MASK: u64 = (1_u64 << SMP_QUALIFICATION_WORK_BITS) - 1;
const _: () = assert!(SMP_QUALIFICATION_MAX_WORK_UNITS <= SMP_QUALIFICATION_WORK_MASK);

/// Versioned, fixed-layout admission request for one suspended `smpqual`
/// child. Every reserved field is part of the closed wire and must be zero;
/// that prevents an older kernel from silently accepting newer authority.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosSmpQualificationBindArgs {
    pub abi_version: u16,
    pub flags: u16,
    pub reserved0: u32,
    pub target_pid: u64,
    pub workers: u32,
    pub reserved1: u32,
    pub work_units: u64,
    pub deadline_ms: u32,
    pub reserved2: u32,
}

/// Pure wire validation for SMP qualification admission. Identity, service
/// ownership, suspended-child provenance, and the absolute monotonic deadline
/// are kernel-derived policy checks and intentionally live outside this ABI
/// predicate.
pub const fn smp_qualification_bind_shape_valid(args: &RustosSmpQualificationBindArgs) -> bool {
    super::smp_qualification_bind_shape_contract(args)
}

/// Packs one userspace RDTSCP observation and its fixed worker ordinal into
/// the product-milestone argument checked and then re-emitted by ring0.
pub const fn pack_smp_qualification_worker(observed_cpu: u32, worker_id: u32) -> u64 {
    (observed_cpu as u64) << 32 | worker_id as u64
}

pub const fn unpack_smp_qualification_worker(value: u64) -> (u32, u32) {
    ((value >> 32) as u32, value as u32)
}

/// Intrinsic shape accepted for one kernel-stamped Ring3 SMP phase. Process
/// and thread identity are stamped separately by ring0; this predicate binds
/// the userspace RDTSCP observation to the CPU that entered the syscall and
/// keeps both worker and work cardinality finite.
pub const fn smp_qualification_worker_shape_valid(
    packed_worker: u64,
    work_units: u64,
    current_cpu: u32,
) -> bool {
    super::smp_qualification_worker_shape_contract(packed_worker, work_units, current_cpu)
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn packed_worker_round_trips_every_cpu_and_worker_word() {
        let cpu: u32 = kani::any();
        let worker: u32 = kani::any();
        kani::cover!(
            cpu == 0 && worker == 0,
            "zero CPU and worker pair is reachable"
        );
        kani::cover!(
            cpu == u32::MAX && worker == u32::MAX,
            "maximum CPU and worker pair is reachable"
        );
        assert_eq!(
            unpack_smp_qualification_worker(pack_smp_qualification_worker(cpu, worker)),
            (cpu, worker)
        );
    }

    #[kani::proof]
    fn admitted_worker_has_every_exact_intrinsic_bound() {
        let packed: u64 = kani::any();
        let work_units: u64 = kani::any();
        let current_cpu: u32 = kani::any();
        let admitted = smp_qualification_worker_shape_valid(packed, work_units, current_cpu);
        kani::cover!(admitted, "one bounded worker shape is admitted");
        kani::cover!(!admitted, "one malformed worker shape is rejected");
        if admitted {
            let (observed_cpu, worker_id) = unpack_smp_qualification_worker(packed);
            assert_eq!(observed_cpu, current_cpu);
            assert!(super::super::smp_qualification_worker_bound_kani_contract(
                worker_id
            ));
            assert!((1..=SMP_QUALIFICATION_MAX_WORK_UNITS).contains(&work_units));
        }
    }

    #[kani::proof]
    fn smp_qualification_bind_shape_is_closed_and_bounded() {
        let args = RustosSmpQualificationBindArgs {
            abi_version: kani::any(),
            flags: kani::any(),
            reserved0: kani::any(),
            target_pid: kani::any(),
            workers: kani::any(),
            reserved1: kani::any(),
            work_units: kani::any(),
            deadline_ms: kani::any(),
            reserved2: kani::any(),
        };
        let admitted = smp_qualification_bind_shape_valid(&args);
        kani::cover!(admitted, "one exact qualification bind request is admitted");
        kani::cover!(
            !admitted,
            "one malformed qualification bind request is rejected"
        );
        if admitted {
            assert_eq!(args.abi_version, SMP_QUALIFICATION_BIND_ABI_VERSION);
            assert_eq!(args.flags, 0);
            assert_eq!(args.reserved0, 0);
            assert_ne!(args.target_pid, 0);
            assert!(matches!(args.workers, 1 | 2 | 4 | 8));
            assert_eq!(args.reserved1, 0);
            assert!((1..=SMP_QUALIFICATION_MAX_WORK_UNITS).contains(&args.work_units));
            assert!(args.work_units <= SMP_QUALIFICATION_WORK_MASK);
            assert!((1..=SMP_QUALIFICATION_MAX_DEADLINE_MS).contains(&args.deadline_ms));
            assert_eq!(args.reserved2, 0);
        }
    }
}
