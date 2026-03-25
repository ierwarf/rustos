use alloc::vec::Vec;
use core::convert::TryFrom;

use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;

use crate::memory::paging::{self, AddressSpaceError, ProcessAddressSpace, UserRegion};
use crate::user::handles::HandleTable;
use crate::user::linux::{
    LinuxMemoryMapState, LinuxProcessState, LinuxSigAction, MAX_SIGNAL_NUMBER,
};

const PAGE_SIZE: u64 = 4096;
const DEFAULT_MAPPING_GAP: u64 = 16 * 1024 * 1024;
const ADMIN_REQUEST_PATH_CAPACITY: usize = 96;
const PROCESS_EXEC_PATH_CAPACITY: usize = 192;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingAdminRequestKind {
    FileSystemAccess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingAdminRequest {
    kind: PendingAdminRequestKind,
    path: [u8; ADMIN_REQUEST_PATH_CAPACITY],
    path_len: usize,
}

impl PendingAdminRequest {
    fn for_path(kind: PendingAdminRequestKind, path: &str) -> Self {
        let mut stored = [0_u8; ADMIN_REQUEST_PATH_CAPACITY];
        let mut len = 0usize;
        for byte in path.bytes() {
            if len == stored.len() {
                break;
            }
            stored[len] = match byte {
                b' '..=b'~' => byte,
                _ => b'?',
            };
            len += 1;
        }

        Self {
            kind,
            path: stored,
            path_len: len,
        }
    }

    pub fn kind(self) -> PendingAdminRequestKind {
        self.kind
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessSecurityContext {
    logical_admin: bool,
    pending_admin_request: Option<PendingAdminRequest>,
}

impl ProcessSecurityContext {
    pub const fn new(logical_admin: bool) -> Self {
        Self {
            logical_admin,
            pending_admin_request: None,
        }
    }

    pub fn is_logical_admin(self) -> bool {
        self.logical_admin
    }

    pub fn pending_admin_request(self) -> Option<PendingAdminRequest> {
        self.pending_admin_request
    }

    fn queue_admin_request(&mut self, kind: PendingAdminRequestKind, path: &str) {
        self.pending_admin_request = Some(PendingAdminRequest::for_path(kind, path));
    }
}

pub struct UserProcessState {
    address_space: ProcessAddressSpace,
    linux_process_state: Option<LinuxProcessState>,
    linux_memory_map: Option<LinuxMemoryMapState>,
    linux_sigactions: [LinuxSigAction; MAX_SIGNAL_NUMBER + 1],
    handles: HandleTable,
    security: ProcessSecurityContext,
    mapping_cursor: u64,
    windows_allocations: Vec<WindowsAllocation>,
    exec_path: [u8; PROCESS_EXEC_PATH_CAPACITY],
    exec_path_len: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowsAllocationKind {
    Heap,
    Virtual,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowsAllocation {
    pub base: u64,
    pub len: u64,
    pub protect: u32,
    pub kind: WindowsAllocationKind,
}

impl WindowsAllocation {
    pub const fn new(base: u64, len: u64, protect: u32, kind: WindowsAllocationKind) -> Self {
        Self {
            base,
            len,
            protect,
            kind,
        }
    }

    pub fn contains_range(self, start: u64, len: u64) -> bool {
        let Some(end) = start.checked_add(len) else {
            return false;
        };
        let Some(allocation_end) = self.base.checked_add(self.len) else {
            return false;
        };
        start >= self.base && end <= allocation_end
    }
}

impl UserProcessState {
    pub fn new(
        address_space: ProcessAddressSpace,
        linux_process_state: Option<LinuxProcessState>,
        linux_memory_map: Option<LinuxMemoryMapState>,
        logical_admin: bool,
        exec_path: &str,
    ) -> Self {
        let default_cursor = if let Some(state) = linux_process_state {
            align_up(state.mmap_next)
        } else {
            let highest_region_end = address_space
                .regions()
                .iter()
                .map(|region| region.end().as_u64())
                .max()
                .unwrap_or(paging::USER_SPACE_BASE);
            align_up(highest_region_end.saturating_add(DEFAULT_MAPPING_GAP))
        };

        let mut state = Self {
            address_space,
            linux_process_state,
            linux_memory_map,
            linux_sigactions: [LinuxSigAction::default(); MAX_SIGNAL_NUMBER + 1],
            handles: HandleTable::new(),
            security: ProcessSecurityContext::new(logical_admin),
            mapping_cursor: default_cursor,
            windows_allocations: Vec::new(),
            exec_path: [0; PROCESS_EXEC_PATH_CAPACITY],
            exec_path_len: 0,
        };
        state.set_exec_path(exec_path);
        state.sync_linux_mapping_cursor();
        state
    }

    pub fn address_space(&self) -> &ProcessAddressSpace {
        &self.address_space
    }

    pub fn address_space_root(&self) -> u64 {
        self.address_space.root_phys().as_u64()
    }

    pub fn address_space_mut(&mut self) -> &mut ProcessAddressSpace {
        &mut self.address_space
    }

    pub fn linux_process_state(&self) -> Option<&LinuxProcessState> {
        self.linux_process_state.as_ref()
    }

    pub fn linux_process_state_mut(&mut self) -> Option<&mut LinuxProcessState> {
        self.linux_process_state.as_mut()
    }

    pub fn linux_memory_map(&self) -> Option<&LinuxMemoryMapState> {
        self.linux_memory_map.as_ref()
    }

    pub fn linux_memory_map_mut(&mut self) -> Option<&mut LinuxMemoryMapState> {
        self.linux_memory_map.as_mut()
    }

    pub fn address_space_and_linux_process_state_mut(
        &mut self,
    ) -> (&mut ProcessAddressSpace, &mut Option<LinuxProcessState>) {
        (&mut self.address_space, &mut self.linux_process_state)
    }

    pub fn handles(&self) -> &HandleTable {
        &self.handles
    }

    pub fn handles_mut(&mut self) -> &mut HandleTable {
        &mut self.handles
    }

    pub fn linux_signal_action(&self, signal: u64) -> Option<LinuxSigAction> {
        let index = usize::try_from(signal).ok()?;
        self.linux_sigactions.get(index).copied()
    }

    pub fn set_linux_signal_action(&mut self, signal: u64, action: LinuxSigAction) -> Option<()> {
        let index = usize::try_from(signal).ok()?;
        *self.linux_sigactions.get_mut(index)? = action;
        Some(())
    }

    pub fn security(&self) -> ProcessSecurityContext {
        self.security
    }

    pub fn exec_path(&self) -> &str {
        core::str::from_utf8(&self.exec_path[..self.exec_path_len]).unwrap_or("")
    }

    pub fn require_logical_admin_for_file_access(&mut self, path: &str) -> bool {
        if self.security.logical_admin {
            return true;
        }

        self.security
            .queue_admin_request(PendingAdminRequestKind::FileSystemAccess, path);
        false
    }

    pub fn set_mapping_cursor(&mut self, addr: u64) {
        self.mapping_cursor = align_up(addr);
        self.sync_linux_mapping_cursor();
    }

    pub fn map_zeroed_pages_from_mapping_cursor(
        &mut self,
        page_count: usize,
        flags: PageTableFlags,
    ) -> Result<UserRegion, AddressSpaceError> {
        if page_count == 0 {
            return Err(AddressSpaceError::ZeroSizedAllocation);
        }

        let start = align_up(self.mapping_cursor);
        let span = (page_count as u64)
            .checked_mul(PAGE_SIZE)
            .ok_or(AddressSpaceError::AddressOverflow)?;
        let end = start
            .checked_add(span)
            .ok_or(AddressSpaceError::AddressOverflow)?;

        if let Some(linux_process_state) = self.linux_process_state.as_ref() {
            if end > linux_process_state.brk_limit() || start < linux_process_state.brk_mapped_end {
                return Err(AddressSpaceError::OutOfFrames);
            }
        }

        let region =
            self.address_space
                .map_zeroed_user_pages_at(VirtAddr::new(start), page_count, flags)?;
        self.set_mapping_cursor(region.end().as_u64());
        Ok(region)
    }

    pub fn record_windows_allocation(&mut self, allocation: WindowsAllocation) {
        if let Some(existing) = self
            .windows_allocations
            .iter_mut()
            .find(|existing| existing.base == allocation.base)
        {
            *existing = allocation;
            return;
        }
        self.windows_allocations.push(allocation);
    }

    pub fn windows_allocation(&self, base: u64) -> Option<WindowsAllocation> {
        self.windows_allocations
            .iter()
            .copied()
            .find(|allocation| allocation.base == base)
    }

    pub fn windows_allocation_containing(&self, start: u64, len: u64) -> Option<WindowsAllocation> {
        self.windows_allocations
            .iter()
            .copied()
            .find(|allocation| allocation.contains_range(start, len))
    }

    pub fn update_windows_allocation_protect(&mut self, base: u64, protect: u32) -> Option<u32> {
        let allocation = self
            .windows_allocations
            .iter_mut()
            .find(|allocation| allocation.base == base)?;
        let previous = allocation.protect;
        allocation.protect = protect;
        Some(previous)
    }

    pub fn remove_windows_allocation(&mut self, base: u64) -> Option<WindowsAllocation> {
        let index = self
            .windows_allocations
            .iter()
            .position(|allocation| allocation.base == base)?;
        Some(self.windows_allocations.swap_remove(index))
    }

    fn sync_linux_mapping_cursor(&mut self) {
        if let Some(linux_process_state) = self.linux_process_state.as_mut() {
            linux_process_state.mmap_next = self.mapping_cursor;
        }
    }

    fn set_exec_path(&mut self, exec_path: &str) {
        self.exec_path.fill(0);
        self.exec_path_len = 0;
        for byte in exec_path.bytes() {
            if self.exec_path_len == self.exec_path.len() {
                break;
            }
            self.exec_path[self.exec_path_len] = match byte {
                b' '..=b'~' => byte,
                _ => b'?',
            };
            self.exec_path_len += 1;
        }
    }
}

pub struct SharedUserProcessState {
    process_id: u64,
    ref_count: usize,
    state: UserProcessState,
}

impl SharedUserProcessState {
    pub fn new(process_id: u64, state: UserProcessState) -> Self {
        Self {
            process_id,
            ref_count: 1,
            state,
        }
    }

    pub fn process_id(&self) -> u64 {
        self.process_id
    }

    pub fn state(&self) -> &UserProcessState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut UserProcessState {
        &mut self.state
    }

    pub fn retain(&mut self) {
        self.ref_count = self
            .ref_count
            .checked_add(1)
            .expect("shared process state refcount overflow");
    }

    pub fn release(&mut self) -> bool {
        assert!(
            self.ref_count != 0,
            "shared process state refcount underflow"
        );
        self.ref_count -= 1;
        self.ref_count == 0
    }
}

fn align_up(value: u64) -> u64 {
    value.saturating_add(PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

#[cfg(test)]
mod tests {
    use super::{PendingAdminRequestKind, ProcessSecurityContext};

    #[test]
    fn non_admin_context_queues_file_access_request() {
        let mut security = ProcessSecurityContext::new(false);
        assert!(!security.is_logical_admin());

        security.queue_admin_request(PendingAdminRequestKind::FileSystemAccess, "/etc/config");
        assert_eq!(
            security
                .pending_admin_request()
                .map(|request| request.kind()),
            Some(PendingAdminRequestKind::FileSystemAccess)
        );
    }
}
