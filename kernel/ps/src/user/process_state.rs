//! Per-process ABI, VMA backing, and descriptor-adjacent lifetime state.
//!
//! - **Owner:** `kernel-ps` owns kernel process substrate; service-owned policy
//!   enters only through validated plans.
//! - **Boundary:** Linux/Windows ABI state and remote backing tokens are valid
//!   only under the exact process generation.
//! - **Lifecycle:** Fork clones retained backing, unmap releases exact spans,
//!   and exec replaces state transactionally before old-state retirement.
//! - **Concurrency:** Mutation requires the process-state owner lock and never
//!   performs synchronous service IPC while partially changed.
//! - **Failure:** Partial map/exec/fork failure restores the prior state and
//!   releases staged holds exactly once.
//! - **Forbidden:** No descriptor close as backing release, stale process
//!   snapshot, or policy implementation in this substrate.
//! - **Evidence:** `memory-map` and `process-address-space-lifecycle`.
use alloc::string::String;
use alloc::vec::Vec;
use core::convert::TryFrom;
use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;

use crate::memory::paging::{self, AddressSpaceError, ProcessAddressSpace, UserRegion};
use crate::user::handles::{HandleTable, KernelHandle};
use crate::user::linux::{
    LinuxMemoryMapState, LinuxProcessState, LinuxRuntimeProfile, LinuxSigAction, MAX_SIGNAL_NUMBER,
    SIG_IGN,
};
use crate::user::memfd::MemfdMappingHold;
use kernel_ipc_runtime::api::KernelSharedRegionMappingHold;

const PAGE_SIZE: u64 = 4096;
static NEXT_FUTEX_NAMESPACE_ID: AtomicU64 = AtomicU64::new(1);
const DEFAULT_MAPPING_GAP: u64 = 16 * 1024 * 1024;
const ADMIN_REQUEST_PATH_CAPACITY: usize = 96;
const PROCESS_EXEC_PATH_CAPACITY: usize = 192;
pub const WINDOWS_TLS_SLOT_COUNT: usize = 64;
pub const DEFAULT_DESKTOP_UID: u32 = 1000;
pub const DEFAULT_DESKTOP_GID: u32 = 1000;

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

    // TEST-HARNESS: Host lifecycle tests inspect the staged request kind;
    // production consumes the complete request atomically.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn kind(self) -> PendingAdminRequestKind {
        self.kind
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessSecurityContext {
    logical_admin: bool,
    uid: u32,
    gid: u32,
    euid: u32,
    egid: u32,
    pending_admin_request: Option<PendingAdminRequest>,
}

impl ProcessSecurityContext {
    pub const fn new(logical_admin: bool) -> Self {
        let uid = if logical_admin {
            0
        } else {
            DEFAULT_DESKTOP_UID
        };
        let gid = if logical_admin {
            0
        } else {
            DEFAULT_DESKTOP_GID
        };
        Self {
            logical_admin,
            uid,
            gid,
            euid: uid,
            egid: gid,
            pending_admin_request: None,
        }
    }

    pub const fn new_with_credentials(
        logical_admin: bool,
        uid: u32,
        gid: u32,
        euid: u32,
        egid: u32,
    ) -> Self {
        Self {
            logical_admin,
            uid,
            gid,
            euid,
            egid,
            pending_admin_request: None,
        }
    }

    pub fn is_logical_admin(self) -> bool {
        self.logical_admin
    }

    pub fn uid(self) -> u32 {
        self.uid
    }

    pub fn gid(self) -> u32 {
        self.gid
    }

    pub fn euid(self) -> u32 {
        self.euid
    }

    pub fn egid(self) -> u32 {
        self.egid
    }

    // TEST-HARNESS: Recovery tests observe pending authority without consuming
    // it; production transitions through the mutation methods.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn pending_admin_request(self) -> Option<PendingAdminRequest> {
        self.pending_admin_request
    }

    fn queue_admin_request(&mut self, kind: PendingAdminRequestKind, path: &str) {
        self.pending_admin_request = Some(PendingAdminRequest::for_path(kind, path));
    }

    fn set_uid(&mut self, uid: u32) {
        self.uid = uid;
        self.euid = uid;
        self.pending_admin_request = None;
        self.logical_admin = self.uid == 0 && self.gid == 0 && self.euid == 0 && self.egid == 0;
    }

    fn set_gid(&mut self, gid: u32) {
        self.gid = gid;
        self.egid = gid;
        self.pending_admin_request = None;
        self.logical_admin = self.uid == 0 && self.gid == 0 && self.euid == 0 && self.egid == 0;
    }
}

pub struct UserProcessState {
    address_space: ProcessAddressSpace,
    futex_namespace_id: u64,
    linux_process_state: Option<LinuxProcessState>,
    linux_memory_map: Option<LinuxMemoryMapState>,
    linux_runtime_profile: Option<LinuxRuntimeProfile>,
    linux_sigactions: [LinuxSigAction; MAX_SIGNAL_NUMBER + 1],
    windows_runtime: Option<WindowsProcessRuntimeState>,
    handles: HandleTable,
    security: ProcessSecurityContext,
    mapping_cursor: u64,
    shared_memfd_mappings: Vec<SharedMemfdMapping>,
    shared_region_mappings: Vec<SharedRegionMapping>,
    cwd: String,
    exec_path: [u8; PROCESS_EXEC_PATH_CAPACITY],
    exec_path_len: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowsLoadedModule {
    pub base_address: u64,
    pub image_size: u64,
    pub entry_point: u64,
    pub full_path: String,
    pub base_name: String,
}

impl WindowsLoadedModule {
    pub fn new(
        base_address: u64,
        image_size: u64,
        entry_point: u64,
        full_path: &str,
        base_name: &str,
    ) -> Self {
        Self {
            base_address,
            image_size,
            entry_point,
            full_path: String::from(full_path),
            base_name: String::from(base_name),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowsProcessRuntimeState {
    pub image_base: u64,
    pub image_size: u64,
    pub allocation_base_hint: u64,
    pub public_runtime_address: u64,
    pub peb_address: u64,
    pub teb_address: u64,
    pub process_parameters_address: u64,
    pub loader_data_address: u64,
    pub loader_module_array_address: u64,
    pub loader_module_count: u32,
    pub loader_reserved: u32,
    pub main_module_entry_address: u64,
    pub command_line_w_ptr: u64,
    pub command_line_a_ptr: u64,
    pub environment_w_ptr: u64,
    pub environment_a_ptr: u64,
    pub module_path_w_ptr: u64,
    pub module_path_a_ptr: u64,
    pub module_directory_w_ptr: u64,
    pub module_directory_a_ptr: u64,
    pub main_module_base_name_w_ptr: u64,
    pub main_module_base_name_a_ptr: u64,
    pub argc: i32,
    pub argc_ptr: u64,
    pub argv_ptr_ptr: u64,
    pub environ_ptr_ptr: u64,
    pub argv_ptr: u64,
    pub environ_ptr: u64,
    pub initial_narrow_environment_ptr: u64,
    pub initenv_ptr: u64,
    pub errno_ptr: u64,
    pub last_error_ptr: u64,
    pub commode_ptr: u64,
    pub fmode_ptr: u64,
    pub iob_array_ptr: u64,
    pub stdin_file_ptr: u64,
    pub stdout_file_ptr: u64,
    pub stderr_file_ptr: u64,
    pub localeconv_ptr: u64,
    pub strerror_einval_ptr: u64,
    pub strerror_enomem_ptr: u64,
    pub strerror_eio_ptr: u64,
    pub strerror_erange_ptr: u64,
    pub strerror_unknown_ptr: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowsThreadRuntimeState {
    pub thread_id: u64,
    pub teb_address: u64,
    pub tls_values: [u64; WINDOWS_TLS_SLOT_COUNT],
}

#[derive(Clone, Debug)]
pub struct SharedMemfdMapping {
    start: u64,
    len: u64,
    backing_offset: u64,
    hold: MemfdMappingHold,
}

#[derive(Clone, Debug)]
struct SharedRegionMapping {
    start: u64,
    len: u64,
    backing_offset: u64,
    hold: KernelSharedRegionMappingHold,
}

/// Stable, generational identity of one shared futex word. Physical frames
/// are deliberately absent: remap, migration, and allocator reuse must not
/// change or alias the rendezvous identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SharedFutexBackingKey {
    Memfd { object_id: u64, byte_offset: u64 },
    SharedRegion { region_id: u64, byte_offset: u64 },
}

impl SharedMemfdMapping {
    fn end(&self) -> u64 {
        self.start.saturating_add(self.len)
    }
}

impl SharedRegionMapping {
    fn end(&self) -> u64 {
        self.start.saturating_add(self.len)
    }
}

fn overlap_segment(
    mapping_start: u64,
    mapping_end: u64,
    requested_start: u64,
    requested_end: u64,
) -> Option<(u64, u64)> {
    let overlap_start = mapping_start.max(requested_start);
    let overlap_end = mapping_end.min(requested_end);
    if overlap_start >= overlap_end {
        return None;
    }
    Some((overlap_start, overlap_end - overlap_start))
}

impl WindowsThreadRuntimeState {
    pub const fn new(thread_id: u64, teb_address: u64) -> Self {
        Self {
            thread_id,
            teb_address,
            tls_values: [0; WINDOWS_TLS_SLOT_COUNT],
        }
    }
}

impl UserProcessState {
    pub fn new(
        address_space: ProcessAddressSpace,
        linux_process_state: Option<LinuxProcessState>,
        linux_memory_map: Option<LinuxMemoryMapState>,
        linux_runtime_profile: Option<LinuxRuntimeProfile>,
        windows_runtime: Option<WindowsProcessRuntimeState>,
        logical_admin: bool,
        exec_path: &str,
    ) -> Self {
        let default_cursor = if let Some(state) = linux_process_state {
            align_up(state.mmap_next)
        } else if let Some(runtime) = windows_runtime {
            align_up(runtime.allocation_base_hint)
        } else {
            let highest_region_end = address_space
                .highest_user_mapping_end()
                .expect("bootstrap address-space topology is invalid")
                .unwrap_or(paging::USER_SPACE_BASE);
            align_up(highest_region_end.saturating_add(DEFAULT_MAPPING_GAP))
        };

        let mut state = Self {
            address_space,
            futex_namespace_id: allocate_futex_namespace_id(),
            linux_process_state,
            linux_memory_map,
            linux_runtime_profile,
            linux_sigactions: [LinuxSigAction::default(); MAX_SIGNAL_NUMBER + 1],
            windows_runtime,
            handles: HandleTable::new(),
            security: ProcessSecurityContext::new(logical_admin),
            mapping_cursor: default_cursor,
            shared_memfd_mappings: Vec::new(),
            shared_region_mappings: Vec::new(),
            cwd: String::from("/"),
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

    /// Never-reused address-space generation used by private futex keys.
    pub const fn futex_namespace_id(&self) -> u64 {
        self.futex_namespace_id
    }

    pub fn address_space_mut(&mut self) -> &mut ProcessAddressSpace {
        &mut self.address_space
    }

    pub fn linux_process_state(&self) -> Option<&LinuxProcessState> {
        self.linux_process_state.as_ref()
    }

    pub fn linux_memory_map(&self) -> Option<&LinuxMemoryMapState> {
        self.linux_memory_map.as_ref()
    }

    pub fn linux_memory_map_mut(&mut self) -> Option<&mut LinuxMemoryMapState> {
        self.linux_memory_map.as_mut()
    }

    pub fn linux_runtime_profile(&self) -> Option<&LinuxRuntimeProfile> {
        self.linux_runtime_profile.as_ref()
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

    pub fn record_shared_memfd_mapping(
        &mut self,
        start: u64,
        len: u64,
        backing_offset: u64,
        hold: MemfdMappingHold,
    ) {
        self.shared_memfd_mappings.push(SharedMemfdMapping {
            start,
            len,
            backing_offset,
            hold,
        });
    }

    pub fn shared_memfd_overlap_segments(&self, start: u64, end: u64) -> Vec<(u64, usize)> {
        let mut overlaps = Vec::new();
        for mapping in &self.shared_memfd_mappings {
            let overlap_start = start.max(mapping.start);
            let overlap_end = end.min(mapping.end());
            if overlap_start >= overlap_end {
                continue;
            }
            overlaps.push((overlap_start, (overlap_end - overlap_start) as usize));
        }
        overlaps
    }

    pub fn release_shared_memfd_mappings_in_range(&mut self, start: u64, end: u64) {
        let mut updated = Vec::with_capacity(self.shared_memfd_mappings.len() + 1);
        for mapping in self.shared_memfd_mappings.drain(..) {
            let mapping_end = mapping.end();
            if end <= mapping.start || start >= mapping_end {
                updated.push(mapping);
                continue;
            }

            if start > mapping.start {
                updated.push(SharedMemfdMapping {
                    start: mapping.start,
                    len: start - mapping.start,
                    backing_offset: mapping.backing_offset,
                    hold: mapping.hold.clone(),
                });
            }
            if end < mapping_end {
                updated.push(SharedMemfdMapping {
                    start: end,
                    len: mapping_end - end,
                    backing_offset: mapping
                        .backing_offset
                        .checked_add(end - mapping.start)
                        .expect("shared memfd split offset overflow"),
                    hold: mapping.hold.clone(),
                });
            }
        }
        self.shared_memfd_mappings = updated;
    }

    pub fn record_shared_region_mapping(
        &mut self,
        start: u64,
        len: u64,
        hold: KernelSharedRegionMappingHold,
    ) {
        self.shared_region_mappings.push(SharedRegionMapping {
            start,
            len,
            backing_offset: 0,
            hold,
        });
    }

    pub fn shared_region_overlap_segments(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        self.shared_region_mappings
            .iter()
            .filter_map(|mapping| overlap_segment(mapping.start, mapping.end(), start, end))
            .collect()
    }

    pub fn release_shared_region_mappings_in_range(&mut self, start: u64, end: u64) {
        let mut updated = Vec::with_capacity(self.shared_region_mappings.len() + 1);
        for mapping in self.shared_region_mappings.drain(..) {
            let mapping_end = mapping.end();
            if end <= mapping.start || start >= mapping_end {
                updated.push(mapping);
                continue;
            }

            if start > mapping.start {
                updated.push(SharedRegionMapping {
                    start: mapping.start,
                    len: start - mapping.start,
                    backing_offset: mapping.backing_offset,
                    hold: mapping.hold.clone(),
                });
            }
            if end < mapping_end {
                updated.push(SharedRegionMapping {
                    start: end,
                    len: mapping_end - end,
                    backing_offset: mapping
                        .backing_offset
                        .checked_add(end - mapping.start)
                        .expect("shared region split offset overflow"),
                    hold: mapping.hold.clone(),
                });
            }
        }
        self.shared_region_mappings = updated;
    }

    /// Resolve a process-shared futex through retained VMA backing metadata.
    /// The process-state lock pins these holds for the complete resolution;
    /// object IDs/generations remain non-ABA after the returned value escapes.
    pub fn shared_futex_backing_key(
        &self,
        address: u64,
    ) -> Result<SharedFutexBackingKey, AddressSpaceError> {
        self.address_space.validate_shared_futex_word(address)?;
        let word_end = address
            .checked_add(core::mem::size_of::<u32>() as u64)
            .ok_or(AddressSpaceError::AddressOverflow)?;
        let mut resolved = None;

        for mapping in &self.shared_memfd_mappings {
            if mapping.start <= address && word_end <= mapping.end() {
                let key = SharedFutexBackingKey::Memfd {
                    object_id: mapping.hold.object_id(),
                    byte_offset: mapping
                        .backing_offset
                        .checked_add(address - mapping.start)
                        .ok_or(AddressSpaceError::AddressOverflow)?,
                };
                assert!(
                    resolved.replace(key).is_none(),
                    "shared futex invariant: overlapping stable backing mappings"
                );
            }
        }
        for mapping in &self.shared_region_mappings {
            if mapping.start <= address && word_end <= mapping.end() {
                let key = SharedFutexBackingKey::SharedRegion {
                    region_id: mapping.hold.identity(),
                    byte_offset: mapping
                        .backing_offset
                        .checked_add(address - mapping.start)
                        .ok_or(AddressSpaceError::AddressOverflow)?,
                };
                assert!(
                    resolved.replace(key).is_none(),
                    "shared futex invariant: overlapping stable backing mappings"
                );
            }
        }
        resolved.ok_or(AddressSpaceError::NotMapped)
    }

    pub fn linux_signal_action(&self, signal: u64) -> Option<LinuxSigAction> {
        let index = usize::try_from(signal).ok()?;
        self.linux_sigactions.get(index).copied()
    }

    pub fn windows_runtime(&self) -> Option<WindowsProcessRuntimeState> {
        self.windows_runtime
    }

    pub fn windows_runtime_mut(&mut self) -> Option<&mut WindowsProcessRuntimeState> {
        self.windows_runtime.as_mut()
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

    pub fn cwd(&self) -> &str {
        self.cwd.as_str()
    }

    pub fn set_cwd(&mut self, cwd: &str) {
        self.cwd.clear();
        self.cwd.push_str(cwd);
    }

    pub fn require_logical_admin_for_file_access(&mut self, path: &str) -> bool {
        if self.security.logical_admin {
            return true;
        }

        self.security
            .queue_admin_request(PendingAdminRequestKind::FileSystemAccess, path);
        false
    }

    pub fn set_uid(&mut self, uid: u32) {
        self.security.set_uid(uid);
    }

    pub fn set_gid(&mut self, gid: u32) {
        self.security.set_gid(gid);
    }

    pub fn fork_clone(
        &self,
        address_space: ProcessAddressSpace,
        exec_path_override: Option<&str>,
    ) -> Self {
        let mut state = Self {
            address_space,
            futex_namespace_id: allocate_futex_namespace_id(),
            linux_process_state: self.linux_process_state,
            linux_memory_map: self.linux_memory_map.clone(),
            linux_runtime_profile: self.linux_runtime_profile.clone(),
            linux_sigactions: self.linux_sigactions,
            windows_runtime: self.windows_runtime,
            handles: self.handles.clone(),
            security: self.security,
            mapping_cursor: self.mapping_cursor,
            shared_memfd_mappings: self.shared_memfd_mappings.clone(),
            shared_region_mappings: self.shared_region_mappings.clone(),
            cwd: self.cwd.clone(),
            exec_path: [0; PROCESS_EXEC_PATH_CAPACITY],
            exec_path_len: 0,
        };
        state.set_exec_path(exec_path_override.unwrap_or(self.exec_path()));
        state
    }

    pub fn inherit_fork_process_metadata_from(&mut self, parent: &Self) {
        self.linux_sigactions = parent.linux_sigactions;
        self.handles = parent.handles.clone();
        self.security = parent.security;
        self.mapping_cursor = parent.mapping_cursor;
        self.shared_memfd_mappings = parent.shared_memfd_mappings.clone();
        self.cwd = parent.cwd.clone();
        self.set_exec_path(parent.exec_path());
    }

    pub fn set_mapping_cursor(&mut self, addr: u64) {
        self.mapping_cursor = align_up(addr);
        self.sync_linux_mapping_cursor();
    }

    pub fn replace_for_exec(
        &mut self,
        address_space: ProcessAddressSpace,
        linux_process_state: LinuxProcessState,
        linux_memory_map: LinuxMemoryMapState,
        linux_runtime_profile: LinuxRuntimeProfile,
        exec_path: &str,
    ) -> (Vec<KernelHandle>, Self) {
        let preserved_ignored = self
            .linux_sigactions
            .map(|action| action.handler == SIG_IGN);
        let security = ProcessSecurityContext::new_with_credentials(
            self.security.logical_admin,
            self.security.uid,
            self.security.gid,
            self.security.euid,
            self.security.egid,
        );
        let cwd = self.cwd.clone();
        let mut handles = core::mem::take(&mut self.handles);
        let closed = handles.close_cloexec();

        let mut fresh = Self::new(
            address_space,
            Some(linux_process_state),
            Some(linux_memory_map),
            Some(linux_runtime_profile),
            None,
            security.logical_admin,
            exec_path,
        );
        fresh.cwd = cwd;
        fresh.handles = handles;
        fresh.security = security;
        for (signal, ignored) in preserved_ignored
            .iter()
            .enumerate()
            .take(MAX_SIGNAL_NUMBER + 1)
            .skip(1)
        {
            if *ignored {
                fresh.linux_sigactions[signal] = self.linux_sigactions[signal];
            }
        }

        // Return the old bundle so the exec coordinator can retain its MM and
        // other generation-owned state until the scheduler has activated the
        // new root/context. Dropping it before that publication can reclaim an
        // address space that is still current on the executing CPU.
        let old = core::mem::replace(self, fresh);
        (closed, old)
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

        if let Some(linux_process_state) = self.linux_process_state.as_ref()
            && (end > linux_process_state.brk_limit() || start < linux_process_state.brk_mapped_end)
        {
            return Err(AddressSpaceError::OutOfFrames);
        }

        let region =
            self.address_space
                .map_zeroed_user_pages_at(VirtAddr::new(start), page_count, flags)?;
        self.set_mapping_cursor(region.end().as_u64());
        Ok(region)
    }

    pub fn map_existing_pages_from_mapping_cursor(
        &mut self,
        frames: &[u64],
        flags: PageTableFlags,
    ) -> Result<UserRegion, AddressSpaceError> {
        if frames.is_empty() {
            return Err(AddressSpaceError::ZeroSizedAllocation);
        }

        let start = align_up(self.mapping_cursor);
        let span = (frames.len() as u64)
            .checked_mul(PAGE_SIZE)
            .ok_or(AddressSpaceError::AddressOverflow)?;
        let end = start
            .checked_add(span)
            .ok_or(AddressSpaceError::AddressOverflow)?;

        if let Some(linux_process_state) = self.linux_process_state.as_ref()
            && (end > linux_process_state.brk_limit() || start < linux_process_state.brk_mapped_end)
        {
            return Err(AddressSpaceError::OutOfFrames);
        }

        let region =
            self.address_space
                .map_existing_user_pages_at(VirtAddr::new(start), frames, flags)?;
        self.set_mapping_cursor(region.end().as_u64());
        Ok(region)
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

fn align_up(value: u64) -> u64 {
    value.saturating_add(PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

fn allocate_futex_namespace_id() -> u64 {
    // ORDERING: the counter only allocates unique, never-reused identity; it
    // does not publish process or page-table contents.
    NEXT_FUTEX_NAMESPACE_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1).filter(|next| *next != 0)
        })
        .unwrap_or_else(|_| panic!("futex address-space identity exhausted"))
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_DESKTOP_GID, DEFAULT_DESKTOP_UID, PendingAdminRequestKind, ProcessSecurityContext,
        overlap_segment,
    };

    #[test]
    fn overlap_segment_rejects_disjoint_ranges_without_unsigned_underflow() {
        assert_eq!(overlap_segment(0x1000, 0x2000, 0x3000, 0x4000), None);
        assert_eq!(overlap_segment(0x3000, 0x4000, 0x1000, 0x2000), None);
        assert_eq!(
            overlap_segment(0x1000, 0x3000, 0x2000, 0x4000),
            Some((0x2000, 0x1000))
        );
    }

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

    #[test]
    fn process_security_context_assigns_default_posix_credentials() {
        let user = ProcessSecurityContext::new(false);
        assert_eq!(user.uid(), DEFAULT_DESKTOP_UID);
        assert_eq!(user.gid(), DEFAULT_DESKTOP_GID);
        assert_eq!(user.euid(), DEFAULT_DESKTOP_UID);
        assert_eq!(user.egid(), DEFAULT_DESKTOP_GID);

        let admin = ProcessSecurityContext::new(true);
        assert_eq!(admin.uid(), 0);
        assert_eq!(admin.gid(), 0);
        assert_eq!(admin.euid(), 0);
        assert_eq!(admin.egid(), 0);
    }
}
