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
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::convert::TryFrom;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;

use crate::memory::paging::{self, AddressSpaceError, ProcessAddressSpace};
use crate::user::handles::{HandleTable, KernelHandle};
use crate::user::linux::{
    LinuxMemoryMapState, LinuxProcessState, LinuxRuntimeProfile, LinuxSigAction, MAX_SIGNAL_NUMBER,
    SIG_IGN,
};
use crate::user::memfd::MemfdMappingHold;
use crate::user::memlock::MemlockLedger;
use kernel_ipc_runtime::api::KernelSharedRegionMappingHold;

const PAGE_SIZE: u64 = 4096;
static NEXT_FUTEX_NAMESPACE_ID: AtomicU64 = AtomicU64::new(1);
const DEFAULT_MAPPING_GAP: u64 = 16 * 1024 * 1024;
const PROCESS_EXEC_PATH_CAPACITY: usize = 192;
pub const WINDOWS_TLS_SLOT_COUNT: usize = 64;
pub const DEFAULT_DESKTOP_UID: u32 = rustos_user_abi::desktop_session::DESKTOP_SESSION_UID;
pub const DEFAULT_DESKTOP_GID: u32 = rustos_user_abi::desktop_session::DESKTOP_SESSION_GID;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinuxCapabilities {
    pub effective: u64,
    pub permitted: u64,
    pub inheritable: u64,
}

impl LinuxCapabilities {
    pub const EMPTY: Self = Self {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    };

    /// Validate a Linux `capset` transition against the process's current sets:
    /// permitted may only drop bits, effective must be a subset of permitted,
    /// and inheritable cannot gain bits outside the existing inheritable | permitted.
    pub const fn restrict_to(self, next: Self) -> Option<Self> {
        if (next.permitted & !self.permitted) != 0
            || (next.effective & !next.permitted) != 0
            || (next.inheritable & !(self.inheritable | self.permitted)) != 0
        {
            return None;
        }
        Some(next)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessSecurityContext {
    logical_admin: bool,
    uid: u32,
    gid: u32,
    euid: u32,
    egid: u32,
    /// `PR_SET_NO_NEW_PRIVS`: one-way per-process flag. Inherited by fork and
    /// preserved across exec (`for_exec`); RustOS has no setuid/file-capability
    /// exec path, so recording it is the whole truthful behavior today.
    no_new_privs: bool,
    /// `PR_SET_DUMPABLE` (0/1). Defaults to dumpable, inherited by fork and
    /// reset to dumpable by exec (`for_exec`), as Linux does for a process whose
    /// credentials did not change. RustOS produces no core dumps or ptrace
    /// attach checks that consume it yet.
    dumpable: bool,
    /// `/proc/<pid>/oom_score_adj` in `-1000..=1000`. Inherited by fork and
    /// preserved across exec, as Linux does. RustOS ranks nothing by it: the
    /// job-group memory limit has no victim selection, so the value is
    /// recorded and read back but has no policy effect.
    oom_score_adj: i16,
    capabilities: LinuxCapabilities,
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
            no_new_privs: false,
            dumpable: true,
            oom_score_adj: 0,
            capabilities: LinuxCapabilities::EMPTY,
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
            no_new_privs: false,
            dumpable: true,
            oom_score_adj: 0,
            capabilities: LinuxCapabilities::EMPTY,
        }
    }

    /// Security context of the same process after a successful exec:
    /// credentials and `no_new_privs` carry over, everything image-specific
    /// (dumpable) is reset.
    pub const fn for_exec(self) -> Self {
        let mut next = Self::new_with_credentials(
            self.logical_admin,
            self.uid,
            self.gid,
            self.euid,
            self.egid,
        );
        next.no_new_privs = self.no_new_privs;
        next.oom_score_adj = self.oom_score_adj;
        next.capabilities = self.capabilities;
        next
    }

    pub const fn capabilities(self) -> LinuxCapabilities {
        self.capabilities
    }

    pub const fn oom_score_adj(self) -> i16 {
        self.oom_score_adj
    }

    pub const fn no_new_privs(self) -> bool {
        self.no_new_privs
    }

    pub const fn dumpable(self) -> bool {
        self.dumpable
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

    fn set_uid(&mut self, uid: u32) {
        self.uid = uid;
        self.euid = uid;
        self.logical_admin = self.uid == 0 && self.gid == 0 && self.euid == 0 && self.egid == 0;
    }

    fn set_gid(&mut self, gid: u32) {
        self.gid = gid;
        self.egid = gid;
        self.logical_admin = self.uid == 0 && self.gid == 0 && self.euid == 0 && self.egid == 0;
    }
}

pub struct UserMmState {
    address_space: ProcessAddressSpace,
    futex_namespace_id: u64,
    linux_process_state: Option<LinuxProcessState>,
    linux_memory_map: Option<LinuxMemoryMapState>,
    linux_runtime_profile: Option<LinuxRuntimeProfile>,
    mapping_cursor: u64,
    shared_memfd_mappings: BTreeMap<u64, SharedMemfdMapping>,
    shared_region_mappings: BTreeMap<u64, SharedRegionMapping>,
    /// App DMA-BUF mappings. Not inherited by fork (the child has no PTEs for
    /// them and holds no custody); the child must import its own capability.
    device_buffer_mappings: BTreeMap<u64, DeviceBufferMapping>,
    /// Fixed physical windows (scanout/GPU surfaces) mapped into this process.
    /// They have no shared region to hold, and the surface handle's own record
    /// dies with the fd, so this map is the only thing that keeps the PTEs
    /// visible to `unmap` as external backing after the fd is closed.
    /// Keyed by start virtual address, value is byte length.
    external_physical_mappings: BTreeMap<u64, u64>,
    /// `mlock` ranges and the `mlockall(MCL_FUTURE)` request. Not inherited by
    /// fork (Linux clears `VM_LOCKED` in the child) and replaced by exec.
    memlock: MemlockLedger,
    /// Bytes of the two mapping lists above, billed to the Job as
    /// `Class::Vma` once the process is published.
    mapping_kmem: kernel_mm::api::kmem_charge::SharedMeter,
}

pub struct UserProcessLocal {
    linux_sigactions: [LinuxSigAction; MAX_SIGNAL_NUMBER + 1],
    windows_runtime: Option<WindowsProcessRuntimeState>,
    win32_objects: Win32ProcessObjects,
    handles: HandleTable,
    fork_handle_pins: usize,
    deferred_fork_handle_drops: Vec<KernelHandle>,
    security: ProcessSecurityContext,
    exec_path: [u8; PROCESS_EXEC_PATH_CAPACITY],
    exec_path_len: usize,
    /// Monotonic nanoseconds when the process was created. A fork child starts
    /// at its own creation; `exec` keeps the value (same process). Reported as
    /// `starttime` by procfs.
    start_ns: u64,
}

impl UserProcessLocal {
    pub fn handles(&self) -> &HandleTable {
        &self.handles
    }

    pub fn handles_mut(&mut self) -> &mut HandleTable {
        &mut self.handles
    }

    pub fn security(&self) -> ProcessSecurityContext {
        self.security
    }

    pub fn exec_path(&self) -> &str {
        core::str::from_utf8(&self.exec_path[..self.exec_path_len]).unwrap_or("")
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

pub struct UserProcessState {
    mm: Box<UserMmState>,
    local: Box<UserProcessLocal>,
}

/// Old process state captured across `exec` until post-switch retirement.
pub enum ExecOldState {
    /// Standard exec replaced an owned address space that must be retired.
    Owned(UserProcessState),
    /// `vfork` exec detached a child from the parent's shared `Mm`; the parent's
    /// address space remains live, so only child-local state is dropped.
    DetachedVfork(Box<UserProcessLocal>),
}

impl ExecOldState {
    pub fn retire_execution_state(self) {
        match self {
            Self::Owned(state) => state.retire_execution_state(),
            Self::DetachedVfork(local) => drop(local),
        }
    }
}

/// Shared execution-state container keyed by owner MM and any live `vfork`
/// children borrowing that MM under the same `ProcessStateLock`.
pub(crate) struct ProcessStateBundle {
    owner_pid: u64,
    mm: Option<Box<UserMmState>>,
    owner_local: Option<Box<UserProcessLocal>>,
    vfork_locals: BTreeMap<u64, Box<UserProcessLocal>>,
}

struct BundleRestoreGuard<'a> {
    bundle: &'a mut ProcessStateBundle,
    process_id: u64,
    state: ManuallyDrop<UserProcessState>,
}

impl Drop for BundleRestoreGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: `state` is moved out exactly once on drop and restored to its
        // owning `mm` / `local` slots in `bundle`.
        let UserProcessState { mm, local } = unsafe { ManuallyDrop::take(&mut self.state) };
        self.bundle.restore_for_pid(self.process_id, mm, local);
    }
}

impl ProcessStateBundle {
    pub(crate) fn new(owner_pid: u64, state: UserProcessState) -> Self {
        Self {
            owner_pid,
            mm: Some(state.mm),
            owner_local: Some(state.local),
            vfork_locals: BTreeMap::new(),
        }
    }

    pub(crate) fn insert_vfork_local(&mut self, child_pid: u64, local: Box<UserProcessLocal>) {
        self.vfork_locals.insert(child_pid, local);
    }

    pub(crate) fn remove_vfork_local(&mut self, child_pid: u64) -> Option<Box<UserProcessLocal>> {
        self.vfork_locals.remove(&child_pid)
    }

    fn take_local_for_pid(&mut self, process_id: u64) -> Option<Box<UserProcessLocal>> {
        if process_id == self.owner_pid {
            self.owner_local.take()
        } else {
            self.remove_vfork_local(process_id)
        }
    }

    fn restore_for_pid(
        &mut self,
        process_id: u64,
        mm: Box<UserMmState>,
        local: Box<UserProcessLocal>,
    ) {
        self.mm = Some(mm);
        if process_id == self.owner_pid {
            self.owner_local = Some(local);
        } else {
            self.vfork_locals.insert(process_id, local);
        }
    }

    pub(crate) fn with_state_for_pid<R>(
        &mut self,
        process_id: u64,
        f: impl FnOnce(&mut UserProcessState) -> R,
    ) -> Option<R> {
        let mm = self.mm.take()?;
        let Some(local) = self.take_local_for_pid(process_id) else {
            self.mm = Some(mm);
            return None;
        };
        let mut guard = BundleRestoreGuard {
            bundle: self,
            process_id,
            state: ManuallyDrop::new(UserProcessState { mm, local }),
        };
        Some(f(&mut guard.state))
    }

    /// Reads the shared MM's mapping charge meter without taking `owner_local`.
    pub(crate) fn reserve_exec_mapping_kmem(
        &self,
        address_space: &ProcessAddressSpace,
    ) -> Option<kernel_mm::api::kmem_charge::SharedMeter> {
        self.mm
            .as_ref()?
            .mapping_kmem
            .try_peer(address_space.immutable_mapping_metadata_bytes() as u64)
    }
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
    pub bootstrap_entry_point: u64,
    pub kuser_shared_data_address: u64,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Win32SectionObject {
    pub handle: u64,
    pub fd: Option<u64>,
    pub size: u64,
    pub protect: u64,
    pub is_image: bool,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Win32EventObject {
    pub handle: u64,
    pub signaled: bool,
    pub manual_reset: bool,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Win32MutantObject {
    pub handle: u64,
    pub owner_tid: Option<u64>,
    pub recursion_count: u32,
    pub abandoned: bool,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Win32SemaphoreObject {
    pub handle: u64,
    pub count: i32,
    pub max_count: i32,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Win32ThreadHandleObject {
    pub handle: u64,
    pub tid: u64,
    pub teb_address: u64,
    pub start_address: u64,
    pub suspended: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Win32ProcessHandleObject {
    pub handle: u64,
    pub pid: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Win32KeyHandleObject {
    pub handle: u64,
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Win32ObjDirHandleObject {
    pub handle: u64,
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Win32SymlinkHandleObject {
    pub handle: u64,
    pub path: String,
    pub target: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Win32DirEntrySnapshot {
    pub name: String,
    pub d_type: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Win32DirEnumCursor {
    pub fd: u64,
    pub dir_path: String,
    pub pattern: String,
    pub entries: Vec<Win32DirEntrySnapshot>,
    pub cursor: usize,
    pub exhausted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Win32AfdSocketObject {
    pub handle: u64,
    pub fd: Option<u64>,
    pub family: i32,
    pub sock_type: i32,
    pub protocol: i32,
    pub flags: u32,
    pub nonblocking: bool,
    pub event_handle: u64,
    pub event_mask: u32,
    pub connect_error: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Win32NsiHandleObject {
    pub handle: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Win32MappedView {
    pub base: u64,
    pub size: u64,
    pub allocation_base: u64,
    pub state: u32,
    pub protect: u32,
    pub mem_type: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Win32ProcessObjects {
    pub next_section_seq: u64,
    pub next_event_seq: u64,
    pub next_mutant_seq: u64,
    pub next_semaphore_seq: u64,
    pub next_thread_seq: u64,
    pub next_process_seq: u64,
    pub next_key_seq: u64,
    pub next_obj_dir_seq: u64,
    pub next_symlink_seq: u64,
    pub next_afd_seq: u64,
    pub next_nsi_seq: u64,
    pub sections: Vec<Win32SectionObject>,
    pub events: Vec<Win32EventObject>,
    pub mutants: Vec<Win32MutantObject>,
    pub semaphores: Vec<Win32SemaphoreObject>,
    pub threads: Vec<Win32ThreadHandleObject>,
    pub processes: Vec<Win32ProcessHandleObject>,
    pub keys: Vec<Win32KeyHandleObject>,
    pub obj_dirs: Vec<Win32ObjDirHandleObject>,
    pub symlinks: Vec<Win32SymlinkHandleObject>,
    pub dir_cursors: Vec<Win32DirEnumCursor>,
    pub dir_paths: Vec<(u64, String)>,
    pub afd_sockets: Vec<Win32AfdSocketObject>,
    pub nsi_handles: Vec<Win32NsiHandleObject>,
    pub vmas: Vec<Win32MappedView>,
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
    page_flags: PageTableFlags,
    hold: MemfdMappingHold,
}

/// One admitted app DMA-BUF mapping. The `Arc` lets a partial munmap keep the
/// remaining pieces' custody without a second admission; the capability's
/// mapping count drops only when the last piece is gone (after PTE removal and
/// TLB shootdown, or after retirement reclaim for teardown/exec).
#[derive(Clone, Debug)]
struct DeviceBufferMapping {
    start: u64,
    len: u64,
    hold: alloc::sync::Arc<crate::user::handles::BufferMappingHold>,
}

impl DeviceBufferMapping {
    fn end(&self) -> u64 {
        self.start.saturating_add(self.len)
    }
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

/// Visits all disjoint `[k, end_of(k, v))` entries in `map` that overlap
/// `[start, end)` in ascending start-VA order in `O(log M + k)`.
fn overlapping_entries<'a, V>(
    map: &'a BTreeMap<u64, V>,
    start: u64,
    end: u64,
    end_of: impl Fn(u64, &V) -> u64,
) -> impl Iterator<Item = (u64, &'a V)> {
    let pred = if start < end {
        map.range(..start)
            .next_back()
            .filter(|&(&k, v)| end_of(k, v) > start)
            .map(|(&k, v)| (k, v))
    } else {
        None
    };
    let tail = if start < end {
        Some(map.range(start..end).map(|(&k, v)| (k, v)))
    } else {
        None
    };
    pred.into_iter().chain(tail.into_iter().flatten())
}

/// Returns whether any disjoint `[k, end_of(k, v))` entry in `map` overlaps
/// `[start, end)` in `O(log M)` with zero heap allocations.
fn has_overlapping_entry<V>(
    map: &BTreeMap<u64, V>,
    start: u64,
    end: u64,
    end_of: impl Fn(u64, &V) -> u64,
) -> bool {
    if start >= end {
        return false;
    }
    if let Some((&k, v)) = map.range(..start).next_back()
        && end_of(k, v) > start
    {
        return true;
    }
    map.range(start..end).next().is_some()
}

/// Collects only the start keys of entries in `map` overlapping `[start, end)`.
/// Allocates nothing when there is no overlap (`O(log M)`).
fn overlapping_keys<V>(
    map: &BTreeMap<u64, V>,
    start: u64,
    end: u64,
    end_of: impl Fn(u64, &V) -> u64,
) -> Vec<u64> {
    if !has_overlapping_entry(map, start, end, &end_of) {
        return Vec::new();
    }
    overlapping_entries(map, start, end, end_of)
        .map(|(k, _)| k)
        .collect()
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

        let mut mm = Box::new(UserMmState {
            address_space,
            futex_namespace_id: allocate_futex_namespace_id(),
            linux_process_state,
            linux_memory_map,
            linux_runtime_profile,
            mapping_cursor: default_cursor,
            shared_memfd_mappings: BTreeMap::new(),
            shared_region_mappings: BTreeMap::new(),
            device_buffer_mappings: BTreeMap::new(),
            external_physical_mappings: BTreeMap::new(),
            memlock: MemlockLedger::new(),
            mapping_kmem: kernel_mm::api::kmem_charge::SharedMeter::new(
                kernel_mm::api::kmem_charge::Class::Vma,
            ),
        });
        mm.address_space.set_mapping_meter(mm.mapping_kmem.clone());

        let mut local = Box::new(UserProcessLocal {
            linux_sigactions: [LinuxSigAction::default(); MAX_SIGNAL_NUMBER + 1],
            windows_runtime,
            win32_objects: Win32ProcessObjects::default(),
            handles: HandleTable::new(),
            fork_handle_pins: 0,
            deferred_fork_handle_drops: Vec::new(),
            security: ProcessSecurityContext::new(logical_admin),
            exec_path: [0; PROCESS_EXEC_PATH_CAPACITY],
            exec_path_len: 0,
            start_ns: crate::arch::clock::monotonic_nanos(),
        });
        local.set_exec_path(exec_path);

        let mut state = Self { mm, local };
        state.sync_linux_mapping_cursor();
        state
    }

    pub fn address_space(&self) -> &ProcessAddressSpace {
        &self.mm.address_space
    }

    pub fn address_space_root(&self) -> u64 {
        self.mm.address_space.root_phys().as_u64()
    }

    /// Never-reused address-space generation used by private futex keys.
    pub const fn futex_namespace_id(&self) -> u64 {
        self.mm.futex_namespace_id
    }

    pub fn address_space_mut(&mut self) -> &mut ProcessAddressSpace {
        &mut self.mm.address_space
    }

    pub fn linux_process_state(&self) -> Option<&LinuxProcessState> {
        self.mm.linux_process_state.as_ref()
    }

    /// Immutable stack ownership is checked under the same state lock as a
    /// mapping/VMA publication, even when a service planned against an old MM.
    pub fn mapping_below_stack_guard(&self, start: u64, end: u64) -> bool {
        self.mm
            .linux_process_state
            .as_ref()
            .is_none_or(|state| state.mapping_below_stack_guard(start, end))
    }

    pub fn linux_memory_map(&self) -> Option<&LinuxMemoryMapState> {
        self.mm.linux_memory_map.as_ref()
    }

    pub fn linux_memory_map_mut(&mut self) -> Option<&mut LinuxMemoryMapState> {
        self.mm.linux_memory_map.as_mut()
    }

    pub fn linux_runtime_profile(&self) -> Option<&LinuxRuntimeProfile> {
        self.mm.linux_runtime_profile.as_ref()
    }

    pub fn address_space_and_linux_process_state_mut(
        &mut self,
    ) -> (&mut ProcessAddressSpace, &mut Option<LinuxProcessState>) {
        (&mut self.mm.address_space, &mut self.mm.linux_process_state)
    }

    pub fn handles(&self) -> &HandleTable {
        &self.local.handles
    }

    pub fn handles_mut(&mut self) -> &mut HandleTable {
        &mut self.local.handles
    }

    pub fn record_shared_memfd_mapping(
        &mut self,
        start: u64,
        len: u64,
        backing_offset: u64,
        page_flags: PageTableFlags,
        hold: MemfdMappingHold,
    ) {
        if len == 0 {
            return;
        }
        let end = start.saturating_add(len);
        self.release_shared_memfd_mappings_in_range(start, end);
        self.mm.shared_memfd_mappings.insert(
            start,
            SharedMemfdMapping {
                start,
                len,
                backing_offset,
                page_flags,
                hold,
            },
        );
        self.sync_mapping_kmem();
    }

    pub fn shared_memfd_object_ids_in_range(&self, start: u64, end: u64) -> Vec<u64> {
        let mut ids = Vec::new();
        for (_, mapping) in overlapping_entries(
            &self.mm.shared_memfd_mappings,
            start,
            end,
            |_, m| m.end(),
        ) {
            let id = mapping.hold.object_id();
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids
    }

    pub fn shared_memfd_holds(&self) -> Vec<MemfdMappingHold> {
        let mut holds: Vec<MemfdMappingHold> = Vec::new();
        for mapping in self.mm.shared_memfd_mappings.values() {
            let id = mapping.hold.object_id();
            if !holds.iter().any(|h| h.object_id() == id) {
                holds.push(mapping.hold.clone());
            }
        }
        holds
    }

    pub fn reconcile_shared_memfd_mappings(
        &mut self,
        object_id: u64,
        frames: &[u64],
    ) -> Result<(), crate::memory::paging::AddressSpaceError> {
        let mut ranges = Vec::new();
        for mapping in self.mm.shared_memfd_mappings.values() {
            if mapping.hold.object_id() == object_id {
                ranges.push((
                    mapping.start,
                    mapping.len,
                    mapping.backing_offset,
                    mapping.page_flags,
                ));
            }
        }
        for (start, len, backing_offset, page_flags) in ranges {
            let page_count = (len as usize).div_ceil(4096);
            let start_page = (backing_offset as usize) / 4096;
            let available = if start_page < frames.len() {
                let end_page = frames.len().min(start_page.saturating_add(page_count));
                &frames[start_page..end_page]
            } else {
                &[]
            };
            self.mm.address_space.reconcile_external_user_pages(
                VirtAddr::new(start),
                available,
                page_count,
                page_flags,
            )?;
        }
        Ok(())
    }

    pub fn shared_memfd_overlap_segments(&self, start: u64, end: u64) -> Vec<(u64, usize)> {
        let mut overlaps = Vec::new();
        for (_, mapping) in overlapping_entries(
            &self.mm.shared_memfd_mappings,
            start,
            end,
            |_, m| m.end(),
        ) {
            if let Some((overlap_start, overlap_len)) =
                overlap_segment(mapping.start, mapping.end(), start, end)
            {
                overlaps.push((overlap_start, overlap_len as usize));
            }
        }
        overlaps
    }

    /// Whether any part of `[start, end)` is a shared memfd mapping that was
    /// admitted read-only. Such a range may not gain write permission: the
    /// writable admission is where `F_SEAL_WRITE` and the writable-mapping
    /// count are enforced, so a later `mprotect(PROT_WRITE)` would bypass both.
    pub fn shared_memfd_range_forbids_write_upgrade(&self, start: u64, end: u64) -> bool {
        overlapping_entries(&self.mm.shared_memfd_mappings, start, end, |_, m| m.end())
            .any(|(_, mapping)| !mapping.hold.is_writable())
    }

    pub fn release_shared_memfd_mappings_in_range(&mut self, start: u64, end: u64) {
        let keys = overlapping_keys(&self.mm.shared_memfd_mappings, start, end, |_, m| m.end());
        if keys.is_empty() {
            return;
        }
        for key in keys {
            let Some(mapping) = self.mm.shared_memfd_mappings.remove(&key) else {
                continue;
            };
            let mapping_end = mapping.end();
            if start > mapping.start {
                self.mm.shared_memfd_mappings.insert(
                    mapping.start,
                    SharedMemfdMapping {
                        start: mapping.start,
                        len: start - mapping.start,
                        backing_offset: mapping.backing_offset,
                        page_flags: mapping.page_flags,
                        hold: mapping.hold.clone(),
                    },
                );
            }
            if end < mapping_end {
                self.mm.shared_memfd_mappings.insert(
                    end,
                    SharedMemfdMapping {
                        start: end,
                        len: mapping_end - end,
                        backing_offset: mapping
                            .backing_offset
                            .checked_add(end - mapping.start)
                            .expect("shared memfd split offset overflow"),
                        page_flags: mapping.page_flags,
                        hold: mapping.hold.clone(),
                    },
                );
            }
        }
        self.sync_mapping_kmem();
    }

    pub fn update_shared_memfd_page_flags_in_range(
        &mut self,
        start: u64,
        end: u64,
        page_flags: PageTableFlags,
    ) {
        let keys = overlapping_keys(&self.mm.shared_memfd_mappings, start, end, |_, m| m.end());
        if keys.is_empty() {
            return;
        }
        for key in keys {
            let Some(mapping) = self.mm.shared_memfd_mappings.remove(&key) else {
                continue;
            };
            let mapping_end = mapping.end();
            if start > mapping.start {
                self.mm.shared_memfd_mappings.insert(
                    mapping.start,
                    SharedMemfdMapping {
                        start: mapping.start,
                        len: start - mapping.start,
                        backing_offset: mapping.backing_offset,
                        page_flags: mapping.page_flags,
                        hold: mapping.hold.clone(),
                    },
                );
            }
            let overlap_start = start.max(mapping.start);
            let overlap_end = end.min(mapping_end);
            self.mm.shared_memfd_mappings.insert(
                overlap_start,
                SharedMemfdMapping {
                    start: overlap_start,
                    len: overlap_end - overlap_start,
                    backing_offset: mapping
                        .backing_offset
                        .checked_add(overlap_start - mapping.start)
                        .expect("shared memfd protect offset overflow"),
                    page_flags,
                    hold: mapping.hold.clone(),
                },
            );
            if end < mapping_end {
                self.mm.shared_memfd_mappings.insert(
                    end,
                    SharedMemfdMapping {
                        start: end,
                        len: mapping_end - end,
                        backing_offset: mapping
                            .backing_offset
                            .checked_add(end - mapping.start)
                            .expect("shared memfd protect offset overflow"),
                        page_flags: mapping.page_flags,
                        hold: mapping.hold.clone(),
                    },
                );
            }
        }
        self.sync_mapping_kmem();
    }

    pub fn record_shared_region_mapping(
        &mut self,
        start: u64,
        len: u64,
        hold: KernelSharedRegionMappingHold,
    ) {
        if len == 0 {
            return;
        }
        let end = start.saturating_add(len);
        self.release_shared_region_mappings_in_range(start, end);
        self.mm.shared_region_mappings.insert(
            start,
            SharedRegionMapping {
                start,
                len,
                backing_offset: 0,
                hold,
            },
        );
        self.sync_mapping_kmem();
    }

    /// Records custody for a mapping whose PTEs were just installed. The hold
    /// is released only by `release_device_buffer_mappings_in_range` after the
    /// unmap's TLB shootdown, or by retirement of this whole state.
    pub fn record_device_buffer_mapping(
        &mut self,
        start: u64,
        len: u64,
        hold: crate::user::handles::BufferMappingHold,
    ) {
        if len == 0 {
            return;
        }
        let end = start.saturating_add(len);
        self.release_device_buffer_mappings_in_range(start, end);
        self.mm.device_buffer_mappings.insert(
            start,
            DeviceBufferMapping {
                start,
                len,
                hold: alloc::sync::Arc::new(hold),
            },
        );
        self.sync_mapping_kmem();
    }

    pub fn device_buffer_overlap_segments(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        overlapping_entries(&self.mm.device_buffer_mappings, start, end, |_, m| m.end())
            .filter_map(|(_, mapping)| {
                overlap_segment(mapping.start, mapping.end(), start, end)
                    .map(|(seg_start, len)| (seg_start, seg_start.saturating_add(len)))
            })
            .collect()
    }

    pub fn release_device_buffer_mappings_in_range(&mut self, start: u64, end: u64) {
        let keys = overlapping_keys(&self.mm.device_buffer_mappings, start, end, |_, m| m.end());
        if keys.is_empty() {
            return;
        }
        for key in keys {
            let Some(mapping) = self.mm.device_buffer_mappings.remove(&key) else {
                continue;
            };
            let mapping_end = mapping.end();
            if start > mapping.start {
                self.mm.device_buffer_mappings.insert(
                    mapping.start,
                    DeviceBufferMapping {
                        start: mapping.start,
                        len: start - mapping.start,
                        hold: mapping.hold.clone(),
                    },
                );
            }
            if end < mapping_end {
                self.mm.device_buffer_mappings.insert(
                    end,
                    DeviceBufferMapping {
                        start: end,
                        len: mapping_end - end,
                        hold: mapping.hold.clone(),
                    },
                );
            }
        }
        self.sync_mapping_kmem();
    }

    pub fn record_external_physical_mapping(&mut self, start: u64, len: u64) {
        if len == 0 {
            return;
        }
        let end = start.saturating_add(len);
        self.release_external_physical_mappings_in_range(start, end);
        self.mm.external_physical_mappings.insert(start, len);
        self.sync_mapping_kmem();
    }

    pub fn external_physical_overlap_segments(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        overlapping_entries(
            &self.mm.external_physical_mappings,
            start,
            end,
            |k, &len| k.saturating_add(len),
        )
        .filter_map(|(map_start, &map_len)| {
            overlap_segment(map_start, map_start.saturating_add(map_len), start, end)
        })
        .collect()
    }

    pub fn release_external_physical_mappings_in_range(&mut self, start: u64, end: u64) {
        let keys = overlapping_keys(
            &self.mm.external_physical_mappings,
            start,
            end,
            |k, &len| k.saturating_add(len),
        );
        if keys.is_empty() {
            return;
        }
        for key in keys {
            let Some(map_len) = self.mm.external_physical_mappings.remove(&key) else {
                continue;
            };
            let map_start = key;
            let map_end = map_start.saturating_add(map_len);
            if start > map_start {
                self.mm
                    .external_physical_mappings
                    .insert(map_start, start - map_start);
            }
            if end < map_end {
                self.mm
                    .external_physical_mappings
                    .insert(end, map_end - end);
            }
        }
        self.sync_mapping_kmem();
    }

    pub fn shared_region_overlap_segments(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        overlapping_entries(&self.mm.shared_region_mappings, start, end, |_, m| m.end())
            .filter_map(|(_, mapping)| overlap_segment(mapping.start, mapping.end(), start, end))
            .collect()
    }

    pub fn release_shared_region_mappings_in_range(&mut self, start: u64, end: u64) {
        let keys = overlapping_keys(&self.mm.shared_region_mappings, start, end, |_, m| m.end());
        if keys.is_empty() {
            return;
        }
        for key in keys {
            let Some(mapping) = self.mm.shared_region_mappings.remove(&key) else {
                continue;
            };
            let mapping_end = mapping.end();
            if start > mapping.start {
                self.mm.shared_region_mappings.insert(
                    mapping.start,
                    SharedRegionMapping {
                        start: mapping.start,
                        len: start - mapping.start,
                        backing_offset: mapping.backing_offset,
                        hold: mapping.hold.clone(),
                    },
                );
            }
            if end < mapping_end {
                self.mm.shared_region_mappings.insert(
                    end,
                    SharedRegionMapping {
                        start: end,
                        len: mapping_end - end,
                        backing_offset: mapping
                            .backing_offset
                            .checked_add(end - mapping.start)
                            .expect("shared region split offset overflow"),
                        hold: mapping.hold.clone(),
                    },
                );
            }
        }
        self.sync_mapping_kmem();
    }

    fn mapping_kmem_total(&self) -> u64 {
        (self.mm.shared_memfd_mappings.len() * core::mem::size_of::<SharedMemfdMapping>()
            + self.mm.shared_region_mappings.len() * core::mem::size_of::<SharedRegionMapping>()
            + self.mm.device_buffer_mappings.len() * core::mem::size_of::<DeviceBufferMapping>()
            + self.mm.external_physical_mappings.len() * core::mem::size_of::<(u64, u64)>()
            + self.mm.address_space.immutable_mapping_metadata_bytes())
            as u64
    }

    /// Re-aims the Job charge at the lists' exact byte total. Callers run it
    /// after the list changed, so it cannot refuse: the new record already
    /// exists (admission was `admit_mapping_records`) and a split by unmap
    /// must not fail. A shrink refunds.
    pub fn sync_mapping_kmem(&mut self) {
        let total = self.mapping_kmem_total();
        self.mm.mapping_kmem.set_total_forced(total);
    }

    /// Pre-admit immutable private-image metadata splits, independently of
    /// shared mappings. The MM preflights its complete edit before PTE changes.
    pub fn admit_immutable_mapping_edit(&mut self, start: u64, end: u64) -> bool {
        let extra = self.mm.address_space.immutable_edit_headroom_bytes(start, end) as u64;
        self.mm.mapping_kmem.set_total(self.mapping_kmem_total().saturating_add(extra))
    }

    /// Independently reserve the replacement while the old root retains its meter.
    pub fn reserve_exec_mapping_kmem(&self, address_space: &ProcessAddressSpace)
        -> Option<kernel_mm::api::kmem_charge::SharedMeter>
    {
        self.mm.mapping_kmem.try_peer(address_space.immutable_mapping_metadata_bytes() as u64)
    }

    /// Bills the mapping lists to `group` (the Job's charge group); refused,
    /// charging nothing, when the budget cannot hold the records already
    /// present. A state binds once.
    pub fn bind_mapping_kmem(&mut self, group: usize) -> bool {
        let total = self.mapping_kmem_total();
        self.mm.mapping_kmem.bind(group, total)
    }

    /// Whether `extra` more shared-mapping records fit the Job's `Vma`
    /// budget. Called before the mapping is installed so a refusal leaves
    /// nothing to unwind.
    pub fn admit_mapping_records(&self, extra: usize) -> bool {
        let per_record = core::mem::size_of::<SharedMemfdMapping>()
            .max(core::mem::size_of::<SharedRegionMapping>());
        self.mm
            .mapping_kmem
            .would_admit(self.mapping_kmem_total() + (extra * per_record) as u64)
    }

    pub fn memlock(&self) -> &MemlockLedger {
        &self.mm.memlock
    }

    pub fn memlock_mut(&mut self) -> &mut MemlockLedger {
        &mut self.mm.memlock
    }

    /// Whether any part of `[start, end)` is mapped through a record this
    /// process owns outside the page-table/frame ledger (shared memfd or
    /// region, DMA-BUF, fixed physical window, display surface). Such backing
    /// is released by its own owner and has no leaf claim a relocation could
    /// re-key, so it must not be moved.
    pub fn has_external_backing_in(&self, start: u64, end: u64) -> bool {
        has_overlapping_entry(&self.mm.shared_memfd_mappings, start, end, |_, m| m.end())
            || has_overlapping_entry(&self.mm.shared_region_mappings, start, end, |_, m| m.end())
            || has_overlapping_entry(&self.mm.device_buffer_mappings, start, end, |_, m| m.end())
            || has_overlapping_entry(
                &self.mm.external_physical_mappings,
                start,
                end,
                |k, &len| k.saturating_add(len),
            )
            || !self.handles().surface_overlap_segments(start, end).is_empty()
    }

    /// Whether `[start, end)` touches the ELF image, heap or stack areas whose
    /// labels `/proc/<pid>/maps` renders from `linux_memory_map`.
    pub fn overlaps_labelled_area(&self, start: u64, end: u64) -> bool {
        self.mm.linux_memory_map.as_ref().is_some_and(|map| {
            map.areas()
                .iter()
                .any(|area| start < area.end && area.start < end)
        })
    }

    #[cfg(test)]
    pub fn mapping_kmem_charged(&self) -> u64 {
        self.mm.mapping_kmem.charged()
    }

    /// Resolve a process-shared futex through retained VMA backing metadata.
    /// The process-state lock pins these holds for the complete resolution;
    /// object IDs/generations remain non-ABA after the returned value escapes.
    pub fn shared_futex_backing_key(
        &self,
        address: u64,
    ) -> Result<SharedFutexBackingKey, AddressSpaceError> {
        self.mm.address_space.validate_shared_futex_word(address)?;
        let word_end = address
            .checked_add(core::mem::size_of::<u32>() as u64)
            .ok_or(AddressSpaceError::AddressOverflow)?;
        let mut resolved = None;

        if let Some((_, mapping)) = self.mm.shared_memfd_mappings.range(..=address).next_back()
            && mapping.start <= address
            && word_end <= mapping.end()
        {
            let key = SharedFutexBackingKey::Memfd {
                object_id: mapping.hold.object_id(),
                byte_offset: mapping
                    .backing_offset
                    .checked_add(address - mapping.start)
                    .ok_or(AddressSpaceError::AddressOverflow)?,
            };
            if resolved.replace(key).is_some() {
                return Err(AddressSpaceError::AlreadyMapped);
            }
        }
        if let Some((_, mapping)) = self.mm.shared_region_mappings.range(..=address).next_back()
            && mapping.start <= address
            && word_end <= mapping.end()
        {
            let key = SharedFutexBackingKey::SharedRegion {
                region_id: mapping.hold.identity(),
                byte_offset: mapping
                    .backing_offset
                    .checked_add(address - mapping.start)
                    .ok_or(AddressSpaceError::AddressOverflow)?,
            };
            // Overlapping stable backings would make the rendezvous key
            // ambiguous. Refuse the wait/wake instead of panicking the
            // kernel on a process-visible mapping state.
            if resolved.replace(key).is_some() {
                return Err(AddressSpaceError::AlreadyMapped);
            }
        }
        resolved.ok_or(AddressSpaceError::NotMapped)
    }

    pub fn linux_signal_action(&self, signal: u64) -> Option<LinuxSigAction> {
        let index = usize::try_from(signal).ok()?;
        self.local.linux_sigactions.get(index).copied()
    }

    pub fn windows_runtime(&self) -> Option<WindowsProcessRuntimeState> {
        self.local.windows_runtime
    }

    pub fn win32_objects(&self) -> &Win32ProcessObjects {
        &self.local.win32_objects
    }

    pub fn win32_objects_mut(&mut self) -> &mut Win32ProcessObjects {
        &mut self.local.win32_objects
    }

    #[cfg(test)]
    pub fn set_linux_signal_action(&mut self, signal: u64, action: LinuxSigAction) -> Option<()> {
        let index = usize::try_from(signal).ok()?;
        *self.local.linux_sigactions.get_mut(index)? = action;
        Some(())
    }

    pub fn security(&self) -> ProcessSecurityContext {
        self.local.security
    }

    /// Monotonic creation time of the process in nanoseconds.
    pub fn start_ns(&self) -> u64 {
        self.local.start_ns
    }

    pub fn exec_path(&self) -> &str {
        self.local.exec_path()
    }

    pub fn set_uid(&mut self, uid: u32) {
        self.local.security.set_uid(uid);
    }

    /// One-way: there is deliberately no way to clear the flag.
    pub fn set_no_new_privs(&mut self) {
        self.local.security.no_new_privs = true;
    }

    pub fn set_dumpable(&mut self, dumpable: bool) {
        self.local.security.dumpable = dumpable;
    }

    /// Restrict per-process Linux capabilities without allowing escalation.
    pub fn set_capabilities(&mut self, next: LinuxCapabilities) -> bool {
        let Some(updated) = self.local.security.capabilities.restrict_to(next) else {
            return false;
        };
        self.local.security.capabilities = updated;
        true
    }

    /// Records `/proc/<pid>/oom_score_adj`; `None` when outside `-1000..=1000`.
    pub fn set_oom_score_adj(&mut self, value: i64) -> Option<()> {
        self.local.security.oom_score_adj = i16::try_from(value).ok().filter(|v| (-1000..=1000).contains(v))?;
        Some(())
    }

    pub fn set_gid(&mut self, gid: u32) {
        self.local.security.set_gid(gid);
    }

    /// Pin open-description reference drops while a concurrent `fork` acquires
    /// inherited references for the snapshotted child handle table.
    pub fn begin_fork_handle_pin(&mut self) {
        self.local.fork_handle_pins = self
            .local
            .fork_handle_pins
            .checked_add(1)
            .expect("fork handle pin count overflow");
    }

    pub fn end_fork_handle_pin(&mut self) -> Vec<KernelHandle> {
        self.local.fork_handle_pins = self.local.fork_handle_pins.saturating_sub(1);
        if self.local.fork_handle_pins == 0 {
            core::mem::take(&mut self.local.deferred_fork_handle_drops)
        } else {
            Vec::new()
        }
    }

    pub fn defer_handle_drop_if_pinned(&mut self, handle: KernelHandle) -> (KernelHandle, bool) {
        if self.local.fork_handle_pins > 0 {
            self.local.deferred_fork_handle_drops.push(handle.clone());
            (handle, false)
        } else {
            (handle, true)
        }
    }

    pub fn close_handle_respecting_fork_pin(&mut self, fd: u64) -> Option<(KernelHandle, bool)> {
        let handle = self.local.handles.close(fd)?;
        Some(self.defer_handle_drop_if_pinned(handle))
    }

    pub fn take_deferred_fork_handle_drops(&mut self) -> Vec<KernelHandle> {
        core::mem::take(&mut self.local.deferred_fork_handle_drops)
    }

    pub fn fork_clone(
        &self,
        address_space: ProcessAddressSpace,
        exec_path_override: Option<&str>,
    ) -> Self {
        let mut mm = Box::new(UserMmState {
            address_space,
            futex_namespace_id: allocate_futex_namespace_id(),
            linux_process_state: self.mm.linux_process_state,
            linux_memory_map: self.mm.linux_memory_map.clone(),
            linux_runtime_profile: self.mm.linux_runtime_profile.clone(),
            mapping_cursor: self.mm.mapping_cursor,
            shared_memfd_mappings: self.mm.shared_memfd_mappings.clone(),
            shared_region_mappings: self.mm.shared_region_mappings.clone(),
            device_buffer_mappings: BTreeMap::new(),
            external_physical_mappings: self.mm.external_physical_mappings.clone(),
            memlock: MemlockLedger::new(),
            // A fork child is billed when it is published, not when cloned.
            mapping_kmem: kernel_mm::api::kmem_charge::SharedMeter::new(
                kernel_mm::api::kmem_charge::Class::Vma,
            ),
        });
        mm.address_space.set_mapping_meter(mm.mapping_kmem.clone());
        let local = self.fork_clone_vfork_local(false, exec_path_override);
        Self { mm, local }
    }

    /// Clones only process-local metadata (handles, credentials, signal
    /// dispositions) for a `vfork` child that borrows the parent's `Mm`.
    pub fn fork_clone_vfork_local(
        &self,
        clear_sighand: bool,
        exec_path_override: Option<&str>,
    ) -> Box<UserProcessLocal> {
        let mut linux_sigactions = self.local.linux_sigactions;
        if clear_sighand {
            for action in &mut linux_sigactions {
                if action.handler != SIG_IGN {
                    *action = LinuxSigAction::default();
                }
            }
        }
        let mut local = Box::new(UserProcessLocal {
            linux_sigactions,
            windows_runtime: self.local.windows_runtime,
            win32_objects: self.local.win32_objects.clone(),
            handles: self.local.handles.clone(),
            fork_handle_pins: 0,
            deferred_fork_handle_drops: Vec::new(),
            security: self.local.security,
            exec_path: [0; PROCESS_EXEC_PATH_CAPACITY],
            exec_path_len: 0,
            start_ns: crate::arch::clock::monotonic_nanos(),
        });
        local.set_exec_path(exec_path_override.unwrap_or(self.exec_path()));
        local
    }

    pub fn set_mapping_cursor(&mut self, addr: u64) {
        self.mm.mapping_cursor = align_up(addr);
        self.sync_linux_mapping_cursor();
    }

    pub fn retire_execution_state(self) {
        // Destructure exhaustively: adding an owned field must decide its cold
        // retirement custody, not silently drop shared backing during execution.
        let UserMmState {
            address_space,
            futex_namespace_id,
            linux_process_state,
            linux_memory_map,
            linux_runtime_profile,
            mapping_cursor,
            shared_memfd_mappings,
            shared_region_mappings,
            device_buffer_mappings,
            external_physical_mappings,
            memlock,
            mapping_kmem,
        } = *self.mm;
        let UserProcessLocal {
            linux_sigactions,
            windows_runtime,
            win32_objects,
            handles,
            fork_handle_pins,
            deferred_fork_handle_drops,
            security,
            exec_path,
            exec_path_len,
            start_ns,
        } = *self.local;
        address_space.into_retirement_owner().retire((
            futex_namespace_id,
            linux_process_state,
            linux_memory_map,
            linux_runtime_profile,
            linux_sigactions,
            windows_runtime,
            win32_objects,
            handles,
            fork_handle_pins,
            deferred_fork_handle_drops,
            security,
            mapping_cursor,
            shared_memfd_mappings,
            shared_region_mappings,
            device_buffer_mappings,
            external_physical_mappings,
            memlock,
            mapping_kmem,
            exec_path,
            exec_path_len,
            start_ns,
        ));
    }

    pub fn replace_for_exec(
        &mut self,
        address_space: ProcessAddressSpace,
        linux_process_state: LinuxProcessState,
        linux_memory_map: LinuxMemoryMapState,
        linux_runtime_profile: LinuxRuntimeProfile,
        exec_path: &str,
        replacement_mapping_kmem: kernel_mm::api::kmem_charge::SharedMeter,
    ) -> (Vec<KernelHandle>, Self) {
        let (closed, fresh) = Self::replace_local_for_exec(
            &mut self.local,
            address_space,
            linux_process_state,
            linux_memory_map,
            linux_runtime_profile,
            exec_path,
            replacement_mapping_kmem,
        );

        // Return the old bundle so the exec coordinator can retain its MM and
        // other generation-owned state until the scheduler has activated the
        // new root/context. Dropping it before that publication can reclaim an
        // address space that is still current on the executing CPU.
        let old = core::mem::replace(self, fresh);
        (closed, old)
    }

    pub(crate) fn replace_local_for_exec(
        old_local: &mut UserProcessLocal,
        address_space: ProcessAddressSpace,
        linux_process_state: LinuxProcessState,
        linux_memory_map: LinuxMemoryMapState,
        linux_runtime_profile: LinuxRuntimeProfile,
        exec_path: &str,
        replacement_mapping_kmem: kernel_mm::api::kmem_charge::SharedMeter,
    ) -> (Vec<KernelHandle>, Self) {
        let preserved_ignored = old_local
            .linux_sigactions
            .map(|action| action.handler == SIG_IGN);
        let security = old_local.security.for_exec();
        let mut handles = core::mem::take(&mut old_local.handles);
        let mut closed = handles.close_cloexec();
        closed.append(&mut old_local.deferred_fork_handle_drops);

        let mut fresh = Self::new(
            address_space,
            Some(linux_process_state),
            Some(linux_memory_map),
            Some(linux_runtime_profile),
            None,
            security.logical_admin,
            exec_path,
        );
        fresh.local.handles = handles;
        fresh.local.start_ns = old_local.start_ns;
        // The old meter remains in old_state until its root is reclaimed.
        fresh.mm.mapping_kmem = replacement_mapping_kmem;
        fresh
            .mm
            .address_space
            .set_mapping_meter(fresh.mm.mapping_kmem.clone());
        fresh.local.security = security;
        for (signal, ignored) in preserved_ignored
            .iter()
            .enumerate()
            .take(MAX_SIGNAL_NUMBER + 1)
            .skip(1)
        {
            if *ignored {
                fresh.local.linux_sigactions[signal] = old_local.linux_sigactions[signal];
            }
        }
        (closed, fresh)
    }

    fn sync_linux_mapping_cursor(&mut self) {
        if let Some(linux_process_state) = self.mm.linux_process_state.as_mut() {
            linux_process_state.mmap_next = self.mm.mapping_cursor;
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
        DEFAULT_DESKTOP_GID, DEFAULT_DESKTOP_UID, ProcessSecurityContext,
        overlap_segment,
    };

    #[test]
    fn device_buffer_mapping_custody_survives_split_and_releases_last() {
        use crate::user::handles::{app_buffer_pool, BufferOwner, BufferRights};
        use crate::user::process_state::UserProcessState;
        use alloc::vec;
        use kernel_mm::api::ProcessAddressSpace;
        let owner = BufferOwner { service_id: 4, generation: 0x7d_0001 };
        let cap = app_buffer_pool().allocate(owner, 2 * 4096, BufferRights::ALL).unwrap();
        let id = cap.identity();
        let mut state = UserProcessState::new(
            ProcessAddressSpace::empty_for_tests(),
            Some(crate::user::linux::LinuxProcessState::default()),
            None, None, None, false, "/dmabuf",
        );
        let base = 0x4000_0000_u64;
        state.record_device_buffer_mapping(base, 2 * 4096, cap.admit_mapping(0, 2 * 4096, true).unwrap());
        assert_eq!(state.device_buffer_overlap_segments(base, base + 4096), vec![(base, base + 4096)]);
        {
            let mut pool = app_buffer_pool();
            pool.revoke(id).unwrap();
            pool.provider_retired(id).unwrap();
        }
        drop(cap);
        assert_eq!(app_buffer_pool().next_reclaimable(), None, "mapped range must stay quarantined");
        // Partial munmap keeps custody for the remaining page.
        state.release_device_buffer_mappings_in_range(base, base + 4096);
        assert_eq!(state.device_buffer_overlap_segments(base, base + 2 * 4096), vec![(base + 4096, base + 2 * 4096)]);
        assert_eq!(app_buffer_pool().next_reclaimable(), None);
        // Unrelated range changes nothing.
        state.release_device_buffer_mappings_in_range(base + 8 * 4096, base + 9 * 4096);
        assert_eq!(app_buffer_pool().next_reclaimable(), None);
        state.release_device_buffer_mappings_in_range(base + 4096, base + 2 * 4096);
        assert_eq!(app_buffer_pool().next_reclaimable(), Some(id));
        app_buffer_pool().reclaim_after_scrub(id).unwrap();
    }

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

    #[test]
    fn no_new_privs_is_one_way_and_survives_exec_dumpable_resets() {
        use crate::user::process_state::UserProcessState;
        use kernel_mm::api::ProcessAddressSpace;
        let mut state = UserProcessState::new(
            ProcessAddressSpace::empty_for_tests(),
            Some(crate::user::linux::LinuxProcessState::default()),
            None, None, None, false, "/p",
        );
        assert!(!state.security().no_new_privs());
        assert!(state.security().dumpable());
        state.set_dumpable(false);
        state.set_no_new_privs();
        state.set_no_new_privs();
        assert!(state.security().no_new_privs());
        assert!(!state.security().dumpable());
        // Fork copies the context by value.
        let forked = state.security();
        assert!(forked.no_new_privs() && !forked.dumpable());
        // Exec keeps credentials and no_new_privs, resets dumpable.
        let exec = state.security().for_exec();
        assert!(exec.no_new_privs());
        assert!(exec.dumpable());
        assert_eq!(exec.uid(), state.security().uid());
        assert_eq!(exec.egid(), state.security().egid());
    }

    #[test]
    fn oom_score_adj_is_range_checked_inherited_and_exec_preserved() {
        use crate::user::process_state::UserProcessState;
        use kernel_mm::api::ProcessAddressSpace;
        let mut state = UserProcessState::new(
            ProcessAddressSpace::empty_for_tests(),
            Some(crate::user::linux::LinuxProcessState::default()),
            None, None, None, false, "/p",
        );
        assert_eq!(state.security().oom_score_adj(), 0);
        assert_eq!(state.set_oom_score_adj(1001), None);
        assert_eq!(state.set_oom_score_adj(-1001), None);
        assert_eq!(state.security().oom_score_adj(), 0);
        assert_eq!(state.set_oom_score_adj(-1000), Some(()));
        assert_eq!(state.security().oom_score_adj(), -1000);
        assert_eq!(state.security().for_exec().oom_score_adj(), -1000);
        assert_eq!(state.set_oom_score_adj(1000), Some(()));
        assert_eq!(state.security().oom_score_adj(), 1000);
    }

    #[test]
    fn shared_memfd_object_ids_in_range_deduplicates_and_filters_by_overlap() {
        use crate::user::memfd::MemfdHandle;
        use crate::user::process_state::UserProcessState;
        use alloc::string::String;
        use kernel_mm::api::{PageTableFlags, ProcessAddressSpace};

        let mut state = UserProcessState::new(
            ProcessAddressSpace::empty_for_tests(),
            Some(crate::user::linux::LinuxProcessState::default()),
            None, None, None, false, "/p",
        );
        let m1 = MemfdHandle::new(String::from("m1"), true, None);
        m1.truncate(4096).unwrap();
        let (_f1, _p1, h1) = m1.acquire_mapping_with_eof_guard(0, 4096, true).unwrap();
        let id1 = h1.object_id();
        let flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;

        state.record_shared_memfd_mapping(0x10000, 4096, 0, flags, h1.clone());
        state.record_shared_memfd_mapping(0x12000, 4096, 0, flags, h1);
        assert_eq!(state.shared_memfd_object_ids_in_range(0x10000, 0x13000), alloc::vec![id1]);
        assert!(state.shared_memfd_object_ids_in_range(0x11000, 0x12000).is_empty());
    }

    #[test]
    fn mprotect_updates_shared_memfd_page_flags_across_split_ranges() {
        use crate::user::memfd::MemfdHandle;
        use crate::user::process_state::UserProcessState;
        use alloc::string::String;
        use kernel_mm::api::{PageTableFlags, ProcessAddressSpace};

        let mut state = UserProcessState::new(
            ProcessAddressSpace::empty_for_tests(),
            Some(crate::user::linux::LinuxProcessState::default()),
            None, None, None, false, "/p",
        );
        let memfd = MemfdHandle::new(String::from("mprotect-split"), true, None);
        memfd.truncate(3 * 4096).unwrap();
        let (_f, _p, hold) = memfd
            .acquire_mapping_with_eof_guard(0, 3 * 4096, true)
            .unwrap();
        let rw = PageTableFlags::PRESENT
            | PageTableFlags::USER_ACCESSIBLE
            | PageTableFlags::WRITABLE
            | PageTableFlags::NO_EXECUTE;
        let ro = PageTableFlags::PRESENT
            | PageTableFlags::USER_ACCESSIBLE
            | PageTableFlags::NO_EXECUTE;

        state.record_shared_memfd_mapping(0x20000, 3 * 4096, 0, rw, hold);
        state.update_shared_memfd_page_flags_in_range(0x21000, 0x22000, ro);

        assert_eq!(state.mm.shared_memfd_mappings.len(), 3);
        let entries: alloc::vec::Vec<_> = state.mm.shared_memfd_mappings.values().collect();
        assert_eq!(
            (
                entries[0].start,
                entries[0].len,
                entries[0].backing_offset,
                entries[0].page_flags,
            ),
            (0x20000, 4096, 0, rw)
        );
        assert_eq!(
            (
                entries[1].start,
                entries[1].len,
                entries[1].backing_offset,
                entries[1].page_flags,
            ),
            (0x21000, 4096, 4096, ro)
        );
        assert_eq!(
            (
                entries[2].start,
                entries[2].len,
                entries[2].backing_offset,
                entries[2].page_flags,
            ),
            (0x22000, 4096, 8192, rw)
        );
        let holds = state.shared_memfd_holds();
        assert_eq!(holds.len(), 1);
        assert_eq!(holds[0].object_id(), entries[0].hold.object_id());
    }

    #[test]
    fn fork_handle_pin_defers_closed_handle_drops_until_last_fork_unpins() {
        use crate::user::epoll::EpollHandle;
        use crate::user::process_state::{KernelHandle, UserProcessState};
        use kernel_mm::api::ProcessAddressSpace;
        let mut state = UserProcessState::new(
            ProcessAddressSpace::empty_for_tests(),
            None,
            None,
            None,
            None,
            false,
            "/bin/test",
        );
        let epoll = EpollHandle::from_token(77);
        let fd = state
            .handles_mut()
            .install(KernelHandle::Epoll(epoll))
            .unwrap();
        state.begin_fork_handle_pin();
        state.begin_fork_handle_pin();

        let (_, release_now) = state.close_handle_respecting_fork_pin(fd).unwrap();
        assert!(!release_now);
        assert!(state.end_fork_handle_pin().is_empty());

        let deferred = state.end_fork_handle_pin();
        assert_eq!(deferred.len(), 1);
        assert!(matches!(deferred[0], KernelHandle::Epoll(ref e) if e.token_id() == 77));
    }

    #[test]
    fn btree_mappings_support_ordered_overlap_split_unmap_and_vfork_locals() {
        use crate::user::memfd::MemfdHandle;
        use crate::user::process_state::{ProcessStateBundle, UserProcessState};
        use alloc::string::String;
        use alloc::vec;
        use kernel_mm::api::{PageTableFlags, ProcessAddressSpace};

        let mut state = UserProcessState::new(
            ProcessAddressSpace::empty_for_tests(),
            Some(crate::user::linux::LinuxProcessState::default()),
            None,
            None,
            None,
            false,
            "/bin/btree",
        );

        // External physical mappings: insert out-of-order, query overlaps, middle-split, and release.
        state.record_external_physical_mapping(0x50000, 0x4000);
        state.record_external_physical_mapping(0x10000, 0x3000);
        state.record_external_physical_mapping(0x30000, 0x2000);
        assert!(!state.has_external_backing_in(0x13000, 0x30000));
        assert!(state.has_external_backing_in(0x12000, 0x13000));
        assert_eq!(
            state.external_physical_overlap_segments(0x11000, 0x51000),
            vec![(0x11000, 0x2000), (0x30000, 0x2000), (0x50000, 0x1000)]
        );
        // Punch a hole in the middle of [0x50000, 0x54000) -> splits into [0x50000, 0x51000) and [0x53000, 0x54000).
        state.release_external_physical_mappings_in_range(0x51000, 0x53000);
        assert_eq!(
            state.external_physical_overlap_segments(0x50000, 0x54000),
            vec![(0x50000, 0x1000), (0x53000, 0x1000)]
        );
        // Unmapping an unmapped gap changes nothing.
        state.release_external_physical_mappings_in_range(0x51000, 0x53000);
        assert_eq!(state.mm.external_physical_mappings.len(), 4);
        // Release spanning across multiple entries trims left & right and removes middle.
        state.release_external_physical_mappings_in_range(0x12000, 0x50800);
        assert_eq!(
            state.external_physical_overlap_segments(0x0, 0x60000),
            vec![(0x10000, 0x2000), (0x50800, 0x800), (0x53000, 0x1000)]
        );

        // Shared memfd mappings: read-only write-upgrade guard + partial release.
        let memfd = MemfdHandle::new(String::from("ro-split"), true, None);
        memfd.truncate(4 * 4096).unwrap();
        let (_f, _p, ro_hold) = memfd.acquire_mapping_with_eof_guard(0, 4 * 4096, false).unwrap();
        let flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        state.record_shared_memfd_mapping(0x80000, 4 * 4096, 0, flags, ro_hold);
        assert!(state.shared_memfd_range_forbids_write_upgrade(0x81000, 0x82000));
        assert!(!state.shared_memfd_range_forbids_write_upgrade(0x84000, 0x85000));
        state.release_shared_memfd_mappings_in_range(0x81000, 0x83000);
        assert!(!state.shared_memfd_range_forbids_write_upgrade(0x81000, 0x83000));
        assert!(state.shared_memfd_range_forbids_write_upgrade(0x80000, 0x81000));
        assert!(state.shared_memfd_range_forbids_write_upgrade(0x83000, 0x84000));

        // ProcessStateBundle vfork_locals BTreeMap lookup/replace/remove.
        let v1 = state.fork_clone_vfork_local(false, Some("/child1"));
        let v2 = state.fork_clone_vfork_local(false, Some("/child2"));
        let v1_replaced = state.fork_clone_vfork_local(false, Some("/child1b"));
        let mut bundle = ProcessStateBundle::new(100, state);
        bundle.insert_vfork_local(101, v1);
        bundle.insert_vfork_local(102, v2);
        bundle.insert_vfork_local(101, v1_replaced);
        assert_eq!(
            bundle.with_state_for_pid(101, |s| String::from(s.exec_path())),
            Some(String::from("/child1b"))
        );
        assert_eq!(
            bundle.remove_vfork_local(102).map(|l| String::from(l.exec_path())),
            Some(String::from("/child2"))
        );
        assert!(bundle.remove_vfork_local(102).is_none());
    }
}
