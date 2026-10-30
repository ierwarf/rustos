//! Versioned qualitative job ceilings, separate from Job ABI v4 limits.
//!
//! A set bit permits an operation at this ceiling; it does not grant authority
//! to a process. Broker bindings must also require their exact subject grant.
//! All 256 positions are ceiling positions, not service-role bits. Operation
//! bindings assign their meanings; an out-of-range index never permits access.
//!
//! - Owner: the ABI defines the wire; userspace chooses qualitative ceilings.
//! - Boundary: JobPolicy v1 is separate from the unchanged Job limits ABI v4.
//! - Lifecycle: masks carry no descriptor or positive subject authority.
//! - Concurrency: the wire gives no atomic whole-subtree snapshot guarantee.
//! - Failure: operation indexes outside the bitmap refuse rather than wrap.
//! - Forbidden: never interpret a ceiling bit as a service-role grant.
//! - Evidence: native all-word algebra/wire tests and job-policy-lifecycle.

pub const JOB_POLICY_ABI_VERSION: u16 = 1;
pub const JOB_POLICY_OP_GET: u16 = 1;
pub const JOB_POLICY_OP_SET: u16 = 2;
pub const JOB_POLICY_WORDS: usize = 4;
pub const JOB_POLICY_OPERATIONS: usize = JOB_POLICY_WORDS * u64::BITS as usize;

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosJobPolicy {
    pub words: [u64; JOB_POLICY_WORDS],
}

impl RustosJobPolicy {
    pub const EMPTY: Self = Self { words: [0; JOB_POLICY_WORDS] };
    pub const ALL: Self = Self { words: [u64::MAX; JOB_POLICY_WORDS] };

    pub const fn allows(self, operation: u16) -> bool {
        let operation = operation as usize;
        if operation >= JOB_POLICY_OPERATIONS {
            return false;
        }
        self.words[operation / 64] & (1_u64 << (operation % 64)) != 0
    }

    pub const fn with_operation(mut self, operation: u16, allowed: bool) -> Option<Self> {
        let operation = operation as usize;
        if operation >= JOB_POLICY_OPERATIONS {
            return None;
        }
        let word = &mut self.words[operation / 64];
        let bit = 1_u64 << (operation % 64);
        if allowed {
            *word |= bit;
        } else {
            *word &= !bit;
        }
        Some(self)
    }

    pub const fn intersection(self, parent: Self) -> Self {
        Self { words: [
            self.words[0] & parent.words[0],
            self.words[1] & parent.words[1],
            self.words[2] & parent.words[2],
            self.words[3] & parent.words[3],
        ] }
    }

    pub const fn is_subset_of(self, parent: Self) -> bool {
        ((self.words[0] & !parent.words[0])
            | (self.words[1] & !parent.words[1])
            | (self.words[2] & !parent.words[2])
            | (self.words[3] & !parent.words[3])) == 0
    }

    pub const fn is_empty(self) -> bool {
        (self.words[0] | self.words[1] | self.words[2] | self.words[3]) == 0
    }
}

/// Request/reply for `SYS_RUSTOS_JOB_POLICY`, ABI v1 (96 bytes).
///
/// Both operations require a local typed Job descriptor: GET requires INSPECT,
/// SET requires SET_POLICY. `job`, `effective`, flags and reserved must be zero
/// on input; GET also requires empty `own`. Replies return exact job identity,
/// own ceiling and the live ancestor intersection. SET changes only the own
/// ceiling; it neither grants broker authority nor widens an ancestor ceiling.
/// Copyout failure does not undo a committed SET, as for SET_LIMITS.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosJobPolicyArgs {
    pub abi_version: u16,
    pub op: u16,
    pub flags: u32,
    pub job_handle: u64,
    pub job: u64,
    pub own: RustosJobPolicy,
    pub effective: RustosJobPolicy,
    pub reserved: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_policy_wire_is_fixed_width_and_separate_from_v4_job_limits() {
        assert_eq!(super::super::SYS_RUSTOS_JOB_POLICY, 0x5255_005a);
        assert_eq!(core::mem::size_of::<RustosJobPolicy>(), 32);
        assert_eq!(core::mem::size_of::<RustosJobPolicyArgs>(), 96);
        assert_eq!(core::mem::align_of::<RustosJobPolicyArgs>(), 8);
        assert_eq!(core::mem::offset_of!(RustosJobPolicyArgs, job_handle), 8);
        assert_eq!(core::mem::offset_of!(RustosJobPolicyArgs, job), 16);
        assert_eq!(core::mem::offset_of!(RustosJobPolicyArgs, own), 24);
        assert_eq!(core::mem::offset_of!(RustosJobPolicyArgs, effective), 56);
        assert_eq!(core::mem::offset_of!(RustosJobPolicyArgs, reserved), 88);
        assert_eq!(super::super::JOB_ABI_VERSION, 4);
    }

    #[test]
    fn job_policy_checks_every_word_and_refuses_out_of_range_operations() {
        let mut policy = RustosJobPolicy::EMPTY;
        for operation in [0, 63, 64, 127, 128, 191, 192, 255] {
            policy = policy.with_operation(operation, true).unwrap();
            assert!(policy.allows(operation));
        }
        for operation in [1, 62, 65, 126, 129, 190, 193, 254, 256, u16::MAX] {
            assert!(!policy.allows(operation));
        }
        assert!(policy.with_operation(256, true).is_none());
        assert!(policy.with_operation(u16::MAX, false).is_none());
        assert!(!RustosJobPolicy::ALL.allows(256));
        for operation in [0, 63, 64, 127, 128, 191, 192, 255] {
            policy = policy.with_operation(operation, false).unwrap();
        }
        assert!(policy.is_empty());
    }

    #[test]
    fn job_policy_intersection_is_monotone_and_cannot_restore_ancestor_denials() {
        let child = RustosJobPolicy { words: [0xff, 0xff00, 0xff0000, 0xff000000] };
        let parent = RustosJobPolicy { words: [0x0f, 0x0f00, 0x0f0000, 0x0f000000] };
        let grandparent = RustosJobPolicy { words: [0x33, 0x3300, 0x330000, 0x33000000] };
        let effective = child.intersection(parent).intersection(grandparent);
        assert_eq!(effective.words, [3, 0x300, 0x30000, 0x3000000]);
        assert!(effective.is_subset_of(child));
        assert!(effective.is_subset_of(parent));
        assert!(effective.is_subset_of(grandparent));
        assert_eq!(effective.intersection(RustosJobPolicy::ALL), effective);
        assert!(effective.intersection(RustosJobPolicy::EMPTY).is_empty());
        assert!(!child.is_subset_of(parent));
        assert_eq!(child.intersection(parent), parent.intersection(child));
        assert_eq!(child.intersection(parent).intersection(grandparent),
                   child.intersection(parent.intersection(grandparent)));
    }
}
