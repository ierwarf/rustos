//! Exact process/MM-generation binding of a physical kernel-mm execution pin.
use super::*;
pub(in crate::multitask) struct MmExecutionPin {
    handle: ProcessHandle,
    generation: u32,
    pin: kernel_mm::api::address_space::ExecutionPin,
}

/// Process/MM-generation-qualified transient activation, not CPU authority.
#[derive(Debug)]
pub(in crate::multitask) struct MmActivationPin {
    handle: ProcessHandle,
    generation: u32,
    pin: kernel_mm::api::address_space::ActivationPin,
}
impl MmActivationPin {
    pub(in crate::multitask) fn matches(&self, other: &MmExecutionPin) -> bool {
        self.handle == other.handle
            && self.generation == other.generation
            && self.pin.root_phys() == other.root()
            && self.pin.is_open()
    }

    pub(in crate::multitask) fn same(&self, other: &Self) -> bool {
        self.handle == other.handle
            && self.generation == other.generation
            && self.root() == other.root()
    }
    pub(in crate::multitask) fn root(&self) -> u64 {
        self.pin.root_phys()
    }
}
impl MmExecutionPin {
    pub(super) fn new(
        handle: ProcessHandle,
        generation: u32,
        pin: kernel_mm::api::address_space::ExecutionPin,
    ) -> Self {
        assert_ne!(generation, 0);
        Self {
            handle,
            generation,
            pin,
        }
    }
    pub(super) fn pin_for_publication(&self) -> kernel_mm::api::address_space::ExecutionPin {
        self.pin.try_clone().expect("closed MM cannot be published")
    }

    pub(in crate::multitask) fn matches(&self, handle: ProcessHandle, root: u64) -> bool {
        self.handle == handle && self.pin.root_phys() == root && self.pin.is_open()
    }
    pub(in crate::multitask) fn root(&self) -> u64 {
        self.pin.root_phys()
    }
    pub(in crate::multitask) fn is_open(&self) -> bool {
        self.pin.is_open()
    }
    pub(in crate::multitask) fn try_clone(&self) -> Option<Self> {
        Some(Self::new(
            self.handle,
            self.generation,
            self.pin.try_clone()?,
        ))
    }

    pub(in crate::multitask) fn try_activate(&self) -> Option<MmActivationPin> {
        Some(MmActivationPin {
            handle: self.handle,
            generation: self.generation,
            pin: self.pin.try_activate()?,
        })
    }
    pub(in crate::multitask) fn matches_exec(
        &self,
        reservation: ExecReservation,
        root: u64,
    ) -> bool {
        self.generation == reservation.next_mm_generation && self.matches(reservation.handle, root)
    }
}
pub(in crate::multitask) fn pin_process_mm(
    handle: ProcessHandle,
    process_id: u64,
) -> Option<MmExecutionPin> {
    let mut table = PROCESS_TABLE.lock();
    let object = table.lookup_object_mut(handle)?;
    if object.process_id != process_id || object.exec_state_staged {
        return None;
    }
    Some(MmExecutionPin::new(
        handle,
        object.mm_generation,
        object.mm_execution.try_clone()?,
    ))
}
