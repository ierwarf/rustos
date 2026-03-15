use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;

use crate::paging::{self, AddressSpaceError, ProcessAddressSpace, UserRegion};
use crate::user::handles::HandleTable;
use crate::user::linux::LinuxTaskState;

const PAGE_SIZE: u64 = 4096;
const DEFAULT_MAPPING_GAP: u64 = 16 * 1024 * 1024;
const ADMIN_REQUEST_PATH_CAPACITY: usize = 96;

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
    linux_state: Option<LinuxTaskState>,
    handles: HandleTable,
    security: ProcessSecurityContext,
    mapping_cursor: u64,
}

impl UserProcessState {
    pub fn new(
        address_space: ProcessAddressSpace,
        linux_state: Option<LinuxTaskState>,
        logical_admin: bool,
    ) -> Self {
        let default_cursor = if let Some(state) = linux_state {
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
            linux_state,
            handles: HandleTable::new(),
            security: ProcessSecurityContext::new(logical_admin),
            mapping_cursor: default_cursor,
        };
        state.sync_linux_mapping_cursor();
        state
    }

    pub fn address_space(&self) -> &ProcessAddressSpace {
        &self.address_space
    }

    pub fn address_space_mut(&mut self) -> &mut ProcessAddressSpace {
        &mut self.address_space
    }

    pub fn linux_state(&self) -> Option<&LinuxTaskState> {
        self.linux_state.as_ref()
    }

    pub fn address_space_and_linux_state_mut(
        &mut self,
    ) -> (&mut ProcessAddressSpace, &mut Option<LinuxTaskState>) {
        (&mut self.address_space, &mut self.linux_state)
    }

    pub fn handles(&self) -> &HandleTable {
        &self.handles
    }

    pub fn handles_mut(&mut self) -> &mut HandleTable {
        &mut self.handles
    }

    pub fn security(&self) -> ProcessSecurityContext {
        self.security
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

        if let Some(linux_state) = self.linux_state.as_ref() {
            if end > linux_state.brk_limit() || start < linux_state.brk_mapped_end {
                return Err(AddressSpaceError::OutOfFrames);
            }
        }

        let region =
            self.address_space
                .map_zeroed_user_pages_at(VirtAddr::new(start), page_count, flags)?;
        self.set_mapping_cursor(region.end().as_u64());
        Ok(region)
    }

    fn sync_linux_mapping_cursor(&mut self) {
        if let Some(linux_state) = self.linux_state.as_mut() {
            linux_state.mmap_next = self.mapping_cursor;
        }
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
