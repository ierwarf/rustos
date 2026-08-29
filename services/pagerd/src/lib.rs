#![no_std]

use rustos_user_abi::pager::{
    PagerFaultReplyWire, PagerFaultRequestWire, PagerVmRegionWire, PAGER_ACTION_DENY,
    PAGER_ACTION_MAP_ZEROED, PAGER_FAULT_ABI_VERSION, VM_ACCESS_EXECUTE, VM_ACCESS_READ,
    VM_ACCESS_WRITE, VM_FAULT_COW, VM_FAULT_PRESENT, VM_FAULT_PROTECTION, VM_OBJECT_ANONYMOUS,
    VM_PROT_EXECUTE, VM_PROT_READ, VM_PROT_WRITE,
};

pub const PAGER_MAX_REGIONS: usize = 64;
pub const PAGER_MAX_CONSUMED_TOKENS: usize = 64;
pub const PAGER_DISPOSITION_ILLEGAL_ACCESS: u64 = 1;
pub const PAGER_DISPOSITION_NOT_DEMAND: u64 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PagerFaultError {
    Malformed,
    Stale,
    NotManaged,
    Pressure,
    EpochExhausted,
}

pub struct PagerState {
    epoch: u64,
    regions: [Option<PagerVmRegionWire>; PAGER_MAX_REGIONS],
    consumed_tokens: [u64; PAGER_MAX_CONSUMED_TOKENS],
    consumed_len: usize,
}

impl PagerState {
    pub const fn new(epoch: u64) -> Self {
        Self {
            epoch,
            regions: [None; PAGER_MAX_REGIONS],
            consumed_tokens: [0; PAGER_MAX_CONSUMED_TOKENS],
            consumed_len: 0,
        }
    }

    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn admit_region(&mut self, region: PagerVmRegionWire) -> Result<(), PagerFaultError> {
        if !region.is_canonical() || region.object.pager_epoch != self.epoch {
            return Err(PagerFaultError::Malformed);
        }
        if self.regions.iter().flatten().any(|current| {
            current.process_handle == region.process_handle
                && current.start < region.end
                && region.start < current.end
        }) {
            return Err(PagerFaultError::Stale);
        }
        let slot = self
            .regions
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(PagerFaultError::Pressure)?;
        *slot = Some(region);
        Ok(())
    }

    pub fn invalidate_process(&mut self, process_handle: u64) -> usize {
        let mut removed = 0;
        for slot in &mut self.regions {
            if slot.is_some_and(|region| region.process_handle == process_handle) {
                *slot = None;
                removed += 1;
            }
        }
        removed
    }

    pub fn restart(&mut self) -> Result<u64, PagerFaultError> {
        self.epoch = self
            .epoch
            .checked_add(1)
            .filter(|epoch| *epoch != 0)
            .ok_or(PagerFaultError::EpochExhausted)?;
        self.regions.fill(None);
        self.consumed_tokens.fill(0);
        self.consumed_len = 0;
        Ok(self.epoch)
    }

    pub fn resolve_anonymous_first_touch(
        &mut self,
        request: PagerFaultRequestWire,
        zeroed_frame_capability: u64,
    ) -> Result<PagerFaultReplyWire, PagerFaultError> {
        if !request.is_canonical() {
            return Err(PagerFaultError::Malformed);
        }
        if request.object.pager_epoch != self.epoch
            || self.consumed_tokens[..self.consumed_len].contains(&request.fault_token)
        {
            return Err(PagerFaultError::Stale);
        }
        let region = self
            .regions
            .iter()
            .flatten()
            .find(|region| fault_matches_region(request, **region))
            .copied()
            .ok_or(PagerFaultError::NotManaged)?;

        let mut reply = reply_envelope(request);
        if request.fault_flags & (VM_FAULT_PRESENT | VM_FAULT_PROTECTION | VM_FAULT_COW) != 0 {
            reply.action = PAGER_ACTION_DENY;
            reply.disposition = PAGER_DISPOSITION_NOT_DEMAND;
            self.consume(request.fault_token)?;
            return Ok(reply);
        }
        if !access_is_allowed(request.access, region.prot) {
            reply.action = PAGER_ACTION_DENY;
            reply.disposition = PAGER_DISPOSITION_ILLEGAL_ACCESS;
            self.consume(request.fault_token)?;
            return Ok(reply);
        }
        if region.object.object_type != VM_OBJECT_ANONYMOUS || zeroed_frame_capability == 0 {
            return Err(PagerFaultError::NotManaged);
        }
        reply.action = PAGER_ACTION_MAP_ZEROED;
        reply.frame_rights = region.prot;
        reply.frame_capability = zeroed_frame_capability;
        self.consume(request.fault_token)?;
        debug_assert!(reply.is_canonical_for(request));
        Ok(reply)
    }

    fn consume(&mut self, token: u64) -> Result<(), PagerFaultError> {
        let slot = self
            .consumed_tokens
            .get_mut(self.consumed_len)
            .ok_or(PagerFaultError::Pressure)?;
        *slot = token;
        self.consumed_len += 1;
        Ok(())
    }
}

fn fault_matches_region(request: PagerFaultRequestWire, region: PagerVmRegionWire) -> bool {
    region.contains(request.virtual_address)
        && region.process_handle == request.process_handle
        && region.process_generation == request.process_generation
        && region.mm_generation == request.mm_generation
        && region.vma_generation == request.vma_generation
        && region.object == request.object
}

const fn access_is_allowed(access: u16, prot: u32) -> bool {
    match access {
        VM_ACCESS_READ => prot & VM_PROT_READ != 0,
        VM_ACCESS_WRITE => prot & VM_PROT_WRITE != 0,
        VM_ACCESS_EXECUTE => prot & VM_PROT_EXECUTE != 0,
        _ => false,
    }
}

fn reply_envelope(request: PagerFaultRequestWire) -> PagerFaultReplyWire {
    PagerFaultReplyWire {
        version: PAGER_FAULT_ABI_VERSION,
        fault_token: request.fault_token,
        process_generation: request.process_generation,
        task_generation: request.task_generation,
        mm_generation: request.mm_generation,
        vma_generation: request.vma_generation,
        pager_epoch: request.object.pager_epoch,
        ..PagerFaultReplyWire::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::pager::{
        PagerEndpointCapabilityWire, PagerObjectIdentityWire, VM_OBJECT_ANONYMOUS,
        VM_SHARING_PRIVATE,
    };

    fn region(epoch: u64) -> PagerVmRegionWire {
        PagerVmRegionWire {
            start: 0x4000,
            end: 0x8000,
            object: PagerObjectIdentityWire {
                object_type: VM_OBJECT_ANONYMOUS,
                rights: VM_PROT_READ | VM_PROT_WRITE,
                slot: 3,
                generation: 5,
                pager_epoch: epoch,
                backing_generation: 7,
                ..PagerObjectIdentityWire::default()
            },
            prot: VM_PROT_READ | VM_PROT_WRITE,
            sharing: VM_SHARING_PRIVATE,
            vma_generation: 11,
            process_handle: 13,
            process_generation: 17,
            mm_generation: 19,
            fault_endpoint: PagerEndpointCapabilityWire {
                slot: 23,
                generation: epoch,
                rights: 1,
            },
            ..PagerVmRegionWire::default()
        }
    }

    fn request(region: PagerVmRegionWire, token: u64) -> PagerFaultRequestWire {
        PagerFaultRequestWire {
            version: PAGER_FAULT_ABI_VERSION,
            access: VM_ACCESS_WRITE,
            fault_token: token,
            process_handle: region.process_handle,
            process_generation: region.process_generation,
            task_id: 29,
            task_generation: 31,
            mm_generation: region.mm_generation,
            vma_generation: region.vma_generation,
            virtual_address: region.start,
            deadline_ns: 37,
            scheduling_domain: 41,
            charge_token: 43,
            object: region.object,
            ..PagerFaultRequestWire::default()
        }
    }

    #[test]
    fn anonymous_first_touch_is_zero_fill_and_one_shot() {
        let mut pager = PagerState::new(2);
        let region = region(2);
        pager.admit_region(region).unwrap();
        let request = request(region, 47);
        let reply = pager.resolve_anonymous_first_touch(request, 53).unwrap();
        assert_eq!(reply.action, PAGER_ACTION_MAP_ZEROED);
        assert!(reply.is_canonical_for(request));
        assert_eq!(
            pager.resolve_anonymous_first_touch(request, 59),
            Err(PagerFaultError::Stale)
        );
    }

    #[test]
    fn exec_unmap_and_exit_invalidation_reject_stale_faults() {
        let mut pager = PagerState::new(3);
        let region = region(3);
        pager.admit_region(region).unwrap();
        assert_eq!(pager.invalidate_process(region.process_handle), 1);
        assert_eq!(
            pager.resolve_anonymous_first_touch(request(region, 61), 67),
            Err(PagerFaultError::NotManaged)
        );
    }

    #[test]
    fn restart_advances_epoch_and_old_reply_authority_dies() {
        let mut pager = PagerState::new(5);
        let old = region(5);
        pager.admit_region(old).unwrap();
        let old_request = request(old, 71);
        assert_eq!(pager.restart(), Ok(6));
        assert_eq!(
            pager.resolve_anonymous_first_touch(old_request, 73),
            Err(PagerFaultError::Stale)
        );
        let new = region(6);
        pager.admit_region(new).unwrap();
        assert_eq!(
            pager
                .resolve_anonymous_first_touch(request(new, 79), 83)
                .unwrap()
                .action,
            PAGER_ACTION_MAP_ZEROED
        );
    }

    #[test]
    fn permission_and_protection_faults_have_explicit_abi_dispositions() {
        let mut pager = PagerState::new(7);
        let region = region(7);
        pager.admit_region(region).unwrap();
        let protection = PagerFaultRequestWire {
            fault_flags: VM_FAULT_PROTECTION,
            ..request(region, 89)
        };
        let reply = pager.resolve_anonymous_first_touch(protection, 97).unwrap();
        assert_eq!(reply.action, PAGER_ACTION_DENY);
        assert_eq!(reply.disposition, PAGER_DISPOSITION_NOT_DEMAND);
        assert!(reply.is_canonical_for(protection));
    }
}
