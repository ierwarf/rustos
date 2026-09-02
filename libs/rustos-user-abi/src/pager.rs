//! Canonical, ABI-neutral pager wire shared by Linux and Windows frontends.
//!
//! Policy lives in pagerd. Ring0 may create and consume one-shot tokens, map
//! frames, and flush TLBs, but must not infer a policy action from an integer
//! status or a textual label.

pub mod region_edit;

pub use region_edit::{
    PAGER_MAX_FRAME_GRANTS, PAGER_MAX_REGION_GROWTH_PER_PROTECT, PAGER_MAX_REGION_GROWTH_PER_UNMAP,
    PAGER_MIN_FULLY_TRACKED_PROCESSES, PAGER_PRESSURE_FAULT_FRAME_RESERVE_EMPTY,
    PAGER_PRESSURE_FAULT_SLOTS_FULL, PAGER_PRESSURE_GRANT_TABLE_FULL, PAGER_PRESSURE_KNOWN_MAX,
    PAGER_PRESSURE_REGION_SPLIT_NO_SLOT, PAGER_PRESSURE_REGION_TABLE_FULL,
    PAGER_PRESSURE_RELEASE_QUEUE_FULL, PAGER_PRESSURE_SEQUENCE_EXHAUSTED,
    PAGER_PRESSURE_UNSPECIFIED, PAGER_PRESSURE_VMA_SLOTS_FULL, PAGER_WIRED_FAULT_FRAMES,
    PagerRangeEdit, PagerRegionEdit, apply_region_edit, pager_pressure_name,
};

pub const PAGER_FAULT_ABI_VERSION: u16 = 2;
pub const PAGER_PAGE_BYTES: u64 = 4096;

/// Fixed number of ring0 fault slots a token may name.
pub const PAGER_MAX_FAULT_SLOTS: usize = 128;

/// Pager VMA slots ring0 keeps for one process.
///
/// Published here because a pager sizes its own region table against it: one
/// process must not be able to wedge the pager for every other process.
pub const PAGER_MAX_VMAS_PER_PROCESS: usize = 64;

/// Regions a pager tracks across all processes.
///
/// Ring0 releases a VMA slot when its range is unmapped and tells the pager, so
/// this bounds the live working set rather than every mapping a boot creates.
/// It is deliberately a multiple of `PAGER_MAX_VMAS_PER_PROCESS` so one process
/// filling its own slots still leaves room for others. Exhaustion must be an
/// explicit, observable admission refusal - never a silent downgrade to eager
/// mapping.
pub const PAGER_MAX_TRACKED_REGIONS: usize = 4 * PAGER_MAX_VMAS_PER_PROCESS;
/// Width of the slot component of a fault token.
pub const PAGER_FAULT_TOKEN_SLOT_BITS: u32 = 8;
/// Mask selecting the slot component of a fault token.
pub const PAGER_FAULT_TOKEN_SLOT_MASK: u64 = (1 << PAGER_FAULT_TOKEN_SLOT_BITS) - 1;
/// Highest generation a fault token may carry before its slot fails closed.
pub const PAGER_MAX_FAULT_TOKEN_GENERATION: u64 = u64::MAX >> PAGER_FAULT_TOKEN_SLOT_BITS;

/// Exact range whose pager-tracked protection ring0 has narrowed.
///
/// `mprotect` on part of a region splits it on ring0's side. Without this
/// notification pagerd keeps one region at the original protection and answers
/// faults in the narrowed span with the *old* rights, so the two replicas
/// disagree about what the process may do with a page.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PagerProtectRangeWire {
    pub version: u16,
    pub reserved0: u16,
    pub prot: u32,
    pub process_handle: u64,
    pub process_generation: u64,
    pub start: u64,
    pub end: u64,
    pub reserved1: [u64; 2],
}

impl PagerProtectRangeWire {
    pub const fn is_canonical(self) -> bool {
        self.version == PAGER_FAULT_ABI_VERSION
            && self.reserved0 == 0
            && self.reserved1[0] == 0
            && self.reserved1[1] == 0
            && self.prot != 0
            && self.prot & !VM_PROT_KNOWN == 0
            && !(self.prot & VM_PROT_WRITE != 0 && self.prot & VM_PROT_EXECUTE != 0)
            && self.process_handle != 0
            && self.process_generation != 0
            && self.start != 0
            && self.start < self.end
            && self.start & (PAGER_PAGE_BYTES - 1) == 0
            && self.end & (PAGER_PAGE_BYTES - 1) == 0
    }

    pub const fn edit(self) -> region_edit::PagerRangeEdit {
        region_edit::PagerRangeEdit::protect(self.start, self.end, self.prot)
    }
}

/// Exact range whose pager tracking ring0 is releasing.
///
/// The process identity is kernel-stamped, so a pager releases only what ring0
/// says has died; it never infers liveness from a PID alone.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PagerReleaseRangeWire {
    pub version: u16,
    pub reserved0: [u16; 3],
    pub process_handle: u64,
    pub process_generation: u64,
    pub start: u64,
    pub end: u64,
    pub reserved1: [u64; 2],
}

impl PagerReleaseRangeWire {
    pub const fn edit(self) -> region_edit::PagerRangeEdit {
        region_edit::PagerRangeEdit::unmap(self.start, self.end)
    }

    pub const fn is_canonical(self) -> bool {
        self.version == PAGER_FAULT_ABI_VERSION
            && self.reserved0[0] == 0
            && self.reserved0[1] == 0
            && self.reserved0[2] == 0
            && self.reserved1[0] == 0
            && self.reserved1[1] == 0
            && self.process_handle != 0
            && self.process_generation != 0
            && self.start != 0
            && self.start < self.end
            && self.start & (PAGER_PAGE_BYTES - 1) == 0
            && self.end & (PAGER_PAGE_BYTES - 1) == 0
    }
}

/// One-based slot index carried by a fault token, or `None` when malformed.
///
/// Ring0 mints `token = (generation << PAGER_FAULT_TOKEN_SLOT_BITS) | slot`
/// with a strictly increasing per-slot generation. Publishing that shape here
/// lets a pager reject a replayed or stale token by comparing generations per
/// slot, in fixed memory, instead of retaining every token it has ever seen -
/// a list that can only overflow and then fail every later fault.
pub const fn pager_fault_token_slot(token: u64) -> Option<usize> {
    let slot = token & PAGER_FAULT_TOKEN_SLOT_MASK;
    if slot == 0 || slot > PAGER_MAX_FAULT_SLOTS as u64 {
        return None;
    }
    Some(slot as usize)
}

/// Strictly increasing per-slot generation carried by a fault token.
pub const fn pager_fault_token_generation(token: u64) -> Option<u64> {
    let generation = token >> PAGER_FAULT_TOKEN_SLOT_BITS;
    if generation == 0 || generation > PAGER_MAX_FAULT_TOKEN_GENERATION {
        return None;
    }
    Some(generation)
}

pub const VM_OBJECT_ANONYMOUS: u16 = 1;
pub const VM_OBJECT_FILE_PRIVATE: u16 = 2;
pub const VM_OBJECT_FILE_SHARED: u16 = 3;
pub const VM_OBJECT_MEMFD: u16 = 4;
pub const VM_OBJECT_IMAGE_SECTION: u16 = 5;
pub const VM_OBJECT_DEVICE_PINNED: u16 = 6;

pub const VM_ACCESS_READ: u16 = 1 << 0;
pub const VM_ACCESS_WRITE: u16 = 1 << 1;
pub const VM_ACCESS_EXECUTE: u16 = 1 << 2;
pub const VM_ACCESS_KNOWN: u16 = VM_ACCESS_READ | VM_ACCESS_WRITE | VM_ACCESS_EXECUTE;

pub const VM_FAULT_PRESENT: u16 = 1 << 0;
pub const VM_FAULT_PROTECTION: u16 = 1 << 1;
pub const VM_FAULT_COW: u16 = 1 << 2;
pub const VM_FAULT_KNOWN: u16 = VM_FAULT_PRESENT | VM_FAULT_PROTECTION | VM_FAULT_COW;

pub const VM_PROT_READ: u32 = 1 << 0;
pub const VM_PROT_WRITE: u32 = 1 << 1;
pub const VM_PROT_EXECUTE: u32 = 1 << 2;
pub const VM_PROT_KNOWN: u32 = VM_PROT_READ | VM_PROT_WRITE | VM_PROT_EXECUTE;

pub const PAGER_ACTION_MAP_ZEROED: u16 = 1;
pub const PAGER_ACTION_MAP_SHARED: u16 = 2;
pub const PAGER_ACTION_MAP_COW: u16 = 3;
pub const PAGER_ACTION_RETRY_AFTER: u16 = 4;
pub const PAGER_ACTION_DENY: u16 = 5;
pub const PAGER_ACTION_TERMINATE: u16 = 6;
pub const VM_SHARING_PRIVATE: u16 = 1;
pub const VM_SHARING_SHARED: u16 = 2;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PagerEndpointCapabilityWire {
    pub slot: u64,
    pub generation: u64,
    pub rights: u64,
}

impl PagerEndpointCapabilityWire {
    pub const fn has_authority(self) -> bool {
        self.slot != 0 && self.generation != 0 && self.rights != 0
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PagerObjectIdentityWire {
    pub object_type: u16,
    pub reserved0: u16,
    pub rights: u32,
    pub backing_service: u64,
    pub slot: u64,
    pub generation: u64,
    pub pager_epoch: u64,
    pub backing_generation: u64,
}

impl PagerObjectIdentityWire {
    pub const fn is_canonical(self) -> bool {
        self.object_type >= VM_OBJECT_ANONYMOUS
            && self.object_type <= VM_OBJECT_DEVICE_PINNED
            && self.reserved0 == 0
            && self.rights & !VM_PROT_KNOWN == 0
            && !(self.rights & VM_PROT_WRITE != 0 && self.rights & VM_PROT_EXECUTE != 0)
    }

    pub const fn has_authority(self) -> bool {
        self.is_canonical()
            && (self.object_type == VM_OBJECT_ANONYMOUS || self.backing_service != 0)
            && self.slot != 0
            && self.generation != 0
            && self.pager_epoch != 0
            && self.backing_generation != 0
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PagerVmRegionWire {
    pub start: u64,
    pub end: u64,
    pub object: PagerObjectIdentityWire,
    pub object_offset: u64,
    pub prot: u32,
    pub sharing: u16,
    pub reserved0: u16,
    pub vma_generation: u64,
    pub process_handle: u64,
    pub process_generation: u64,
    pub mm_generation: u64,
    pub fault_endpoint: PagerEndpointCapabilityWire,
    pub reserved1: [u64; 2],
}

impl PagerVmRegionWire {
    pub const fn is_canonical(self) -> bool {
        self.start != 0
            && self.start < self.end
            && self.start & (PAGER_PAGE_BYTES - 1) == 0
            && self.end & (PAGER_PAGE_BYTES - 1) == 0
            && self.object_offset & (PAGER_PAGE_BYTES - 1) == 0
            && self.prot != 0
            && self.prot & !VM_PROT_KNOWN == 0
            && !(self.prot & VM_PROT_WRITE != 0 && self.prot & VM_PROT_EXECUTE != 0)
            && (self.sharing == VM_SHARING_PRIVATE || self.sharing == VM_SHARING_SHARED)
            && self.reserved0 == 0
            && self.reserved1[0] == 0
            && self.reserved1[1] == 0
            && self.vma_generation != 0
            && self.process_handle != 0
            && self.process_generation != 0
            && self.mm_generation != 0
            && self.object.has_authority()
            && self.fault_endpoint.has_authority()
    }

    pub const fn contains(self, address: u64) -> bool {
        address >= self.start && address < self.end
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PagerFaultRequestWire {
    pub version: u16,
    pub access: u16,
    pub fault_flags: u16,
    pub reserved0: u16,
    pub fault_token: u64,
    pub process_handle: u64,
    pub process_generation: u64,
    pub task_id: u64,
    pub task_generation: u64,
    pub mm_generation: u64,
    pub vma_generation: u64,
    pub virtual_address: u64,
    pub object_offset: u64,
    pub deadline_ns: u64,
    pub scheduling_domain: u64,
    pub charge_token: u64,
    pub object: PagerObjectIdentityWire,
    pub reserved1: [u64; 2],
}

impl PagerFaultRequestWire {
    pub const fn is_canonical(self) -> bool {
        self.version == PAGER_FAULT_ABI_VERSION
            && self.reserved0 == 0
            && self.reserved1[0] == 0
            && self.reserved1[1] == 0
            && self.access != 0
            && self.access & !VM_ACCESS_KNOWN == 0
            && self.access.count_ones() == 1
            && self.fault_flags & !VM_FAULT_KNOWN == 0
            && self.virtual_address & (PAGER_PAGE_BYTES - 1) == 0
            && self.object_offset & (PAGER_PAGE_BYTES - 1) == 0
            && self.fault_token != 0
            && self.process_handle != 0
            && self.process_generation != 0
            && self.task_id != 0
            && self.task_generation != 0
            && self.mm_generation != 0
            && self.vma_generation != 0
            && self.deadline_ns != 0
            && self.scheduling_domain != 0
            && self.charge_token != 0
            && self.object.has_authority()
    }
}

/// A kernel-dispatched anonymous fault together with its one-shot, pre-zeroed
/// frame capability.
///
/// The kernel creates this only after the fault slot has entered
/// `DispatchedToPager`.  `pagerd` may return the capability solely in a
/// `PAGER_ACTION_MAP_ZEROED` reply for this exact request; it is not a
/// physical address, reusable allocation right, or cache-frame authority.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PagerFaultDispatchWire {
    pub request: PagerFaultRequestWire,
    pub zeroed_frame_capability: u64,
    pub granted_frame_rights: u32,
    pub reserved0: u32,
    pub reserved1: [u64; 2],
}

impl PagerFaultDispatchWire {
    pub const fn is_canonical(self) -> bool {
        self.request.is_canonical()
            && self.request.object.object_type == VM_OBJECT_ANONYMOUS
            && self.zeroed_frame_capability != 0
            && self.granted_frame_rights != 0
            && self.granted_frame_rights & !VM_PROT_KNOWN == 0
            && !(self.granted_frame_rights & VM_PROT_WRITE != 0
                && self.granted_frame_rights & VM_PROT_EXECUTE != 0)
            && self.granted_frame_rights & !self.request.object.rights == 0
            && self.reserved0 == 0
            && self.reserved1[0] == 0
            && self.reserved1[1] == 0
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PagerFaultReplyWire {
    pub version: u16,
    pub action: u16,
    pub frame_rights: u32,
    pub fault_token: u64,
    pub process_generation: u64,
    pub task_generation: u64,
    pub mm_generation: u64,
    pub vma_generation: u64,
    pub pager_epoch: u64,
    pub frame_capability: u64,
    pub cache_generation: u64,
    pub source_generation: u64,
    pub replay_generation: u64,
    pub disposition: u64,
    pub reserved0: [u64; 2],
}

impl PagerFaultReplyWire {
    pub const fn is_canonical_for(self, request: PagerFaultRequestWire) -> bool {
        if self.version != PAGER_FAULT_ABI_VERSION
            || self.reserved0[0] != 0
            || self.reserved0[1] != 0
            || self.fault_token != request.fault_token
            || self.process_generation != request.process_generation
            || self.task_generation != request.task_generation
            || self.mm_generation != request.mm_generation
            || self.vma_generation != request.vma_generation
            || self.pager_epoch != request.object.pager_epoch
            || self.frame_rights & !VM_PROT_KNOWN != 0
            || self.frame_rights & VM_PROT_WRITE != 0 && self.frame_rights & VM_PROT_EXECUTE != 0
        {
            return false;
        }
        match self.action {
            PAGER_ACTION_MAP_ZEROED => {
                request.object.object_type == VM_OBJECT_ANONYMOUS
                    && self.frame_capability != 0
                    && self.cache_generation == 0
                    && self.source_generation == 0
                    && self.replay_generation == 0
                    && self.disposition == 0
            }
            PAGER_ACTION_MAP_SHARED => {
                self.frame_capability != 0
                    && self.cache_generation != 0
                    && self.source_generation == 0
                    && self.replay_generation == 0
                    && self.disposition == 0
            }
            PAGER_ACTION_MAP_COW => {
                self.frame_capability != 0
                    && self.cache_generation == 0
                    && self.source_generation != 0
                    && self.replay_generation == 0
                    && self.disposition == 0
            }
            PAGER_ACTION_RETRY_AFTER => {
                self.frame_capability == 0
                    && self.frame_rights == 0
                    && self.replay_generation != 0
                    && self.disposition == 0
            }
            PAGER_ACTION_DENY | PAGER_ACTION_TERMINATE => {
                self.frame_capability == 0
                    && self.frame_rights == 0
                    && self.cache_generation == 0
                    && self.source_generation == 0
                    && self.replay_generation == 0
                    && self.disposition != 0
            }
            _ => false,
        }
    }

    /// Validates the stricter Phase-7 anonymous first-touch reply.
    ///
    /// A generic reply may later name a cache or COW frame, but the foundation
    /// path must consume the exact one-shot pre-zeroed grant supplied in the
    /// dispatch envelope and may only attenuate its rights.
    pub const fn is_canonical_zeroed_for(self, dispatch: PagerFaultDispatchWire) -> bool {
        dispatch.is_canonical()
            && self.action == PAGER_ACTION_MAP_ZEROED
            && self.frame_capability == dispatch.zeroed_frame_capability
            && self.frame_rights & !dispatch.granted_frame_rights == 0
            && self.is_canonical_for(dispatch.request)
    }
}

const _: () = assert!(core::mem::size_of::<PagerFaultRequestWire>() <= 256);
const _: () = assert!(core::mem::size_of::<PagerFaultDispatchWire>() <= 256);
const _: () = assert!(core::mem::size_of::<PagerFaultReplyWire>() <= 256);

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> PagerFaultRequestWire {
        PagerFaultRequestWire {
            version: PAGER_FAULT_ABI_VERSION,
            access: VM_ACCESS_WRITE,
            fault_flags: 0,
            fault_token: 7,
            process_handle: 11,
            process_generation: 13,
            task_id: 17,
            task_generation: 19,
            mm_generation: 23,
            vma_generation: 29,
            virtual_address: 0x4000,
            object_offset: 0x8000,
            deadline_ns: 31,
            scheduling_domain: 37,
            charge_token: 41,
            object: PagerObjectIdentityWire {
                object_type: VM_OBJECT_ANONYMOUS,
                rights: VM_PROT_READ | VM_PROT_WRITE,
                slot: 43,
                generation: 47,
                pager_epoch: 53,
                backing_generation: 59,
                ..PagerObjectIdentityWire::default()
            },
            ..PagerFaultRequestWire::default()
        }
    }

    #[test]
    fn fault_wire_rejects_reserved_unknown_unaligned_and_wx_authority() {
        let valid = request();
        assert!(valid.is_canonical());
        let file_without_owner = PagerFaultRequestWire {
            object: PagerObjectIdentityWire {
                object_type: VM_OBJECT_FILE_PRIVATE,
                backing_service: 0,
                ..valid.object
            },
            ..valid
        };
        assert!(!file_without_owner.is_canonical());
        assert!(
            PagerFaultRequestWire {
                object: PagerObjectIdentityWire {
                    backing_service: 2,
                    ..file_without_owner.object
                },
                ..file_without_owner
            }
            .is_canonical()
        );
        for invalid in [
            PagerFaultRequestWire {
                reserved0: 1,
                ..valid
            },
            PagerFaultRequestWire { access: 8, ..valid },
            PagerFaultRequestWire {
                virtual_address: 0x4001,
                ..valid
            },
            PagerFaultRequestWire {
                object: PagerObjectIdentityWire {
                    rights: VM_PROT_WRITE | VM_PROT_EXECUTE,
                    ..valid.object
                },
                ..valid
            },
        ] {
            assert!(!invalid.is_canonical());
        }
    }

    #[test]
    fn fault_reply_is_explicit_generation_exact_and_wx_closed() {
        let request = request();
        let dispatch = PagerFaultDispatchWire {
            request,
            zeroed_frame_capability: 61,
            granted_frame_rights: VM_PROT_READ | VM_PROT_WRITE,
            ..PagerFaultDispatchWire::default()
        };
        assert!(dispatch.is_canonical());
        let reply = PagerFaultReplyWire {
            version: PAGER_FAULT_ABI_VERSION,
            action: PAGER_ACTION_MAP_ZEROED,
            frame_rights: VM_PROT_READ | VM_PROT_WRITE,
            fault_token: request.fault_token,
            process_generation: request.process_generation,
            task_generation: request.task_generation,
            mm_generation: request.mm_generation,
            vma_generation: request.vma_generation,
            pager_epoch: request.object.pager_epoch,
            frame_capability: 61,
            ..PagerFaultReplyWire::default()
        };
        assert!(reply.is_canonical_for(request));
        assert!(reply.is_canonical_zeroed_for(dispatch));
        assert!(
            !PagerFaultReplyWire {
                frame_capability: 71,
                ..reply
            }
            .is_canonical_zeroed_for(dispatch)
        );
        assert!(
            !PagerFaultReplyWire {
                frame_rights: VM_PROT_READ,
                ..reply
            }
            .is_canonical_zeroed_for(PagerFaultDispatchWire {
                granted_frame_rights: VM_PROT_WRITE,
                ..dispatch
            })
        );
        assert!(
            !PagerFaultReplyWire {
                mm_generation: 67,
                ..reply
            }
            .is_canonical_for(request)
        );
        assert!(
            !PagerFaultReplyWire {
                frame_rights: VM_PROT_WRITE | VM_PROT_EXECUTE,
                ..reply
            }
            .is_canonical_for(request)
        );
    }
}
