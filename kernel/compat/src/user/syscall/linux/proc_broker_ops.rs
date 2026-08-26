//! Transactional loader/procd broker operations for Linux and Windows tasks.
//!
//! - **Owner:** Compat owns privileged process substrate brokerage;
//!   `loaderd`/`procd` own parsing and lifecycle policy.
//! - **Boundary:** User requests, service replies, executable mappings,
//!   initial registers, handles, and claimed process identities are untrusted.
//! - **Lifecycle:** Prepare a suspended target, bind the exact requester and
//!   loader epoch, stage mappings, commit once, activate, or retire/rollback.
//! - **Concurrency:** Broker sessions are generation-bound and never expose a
//!   partially initialized runnable task.
//! - **Failure:** Requester/service exit, restart, mapping failure, timeout,
//!   cancellation, and duplicate commit retire staged authority exactly once.
//! - **Forbidden:** No runnable-before-commit child, raw image parsing in
//!   ring0, pathname authority, or stale prepare-session reuse.
//! - **Evidence:** `deferred-process-activation`,
//!   `atomic-process-activation-batch`, `loader-request-authority`,
//!   `post-init-service-authority`, and `remote-file-map`.
use super::*;

mod activation_batch;
mod authority;
mod fork;
mod scheduling_context_grants;

pub(super) use activation_batch::syscall_linux_rustos_proc_activate_batch_broker;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use authority::{current_loader_process_id, prepare_owned_by, procd_process_prepare_policy};
use core::sync::atomic::{AtomicU64, Ordering};
use heapless::index_map::FnvIndexMap;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;

use crate::user::handles::KernelHandle;
use crate::user::memfd::MemfdHandle;
use rustos_user_abi::syscall::{
    IPC_SERVICE_CAP_PROCESS_LOADER, IPC_SERVICE_CAP_PROCESS_POLICY,
    IPC_SERVICE_CAP_ROOT_SUPERVISOR, IPC_SERVICE_INITD, IPC_SERVICE_PROCD, IPC_SERVICE_ROOTD,
    IPC_SERVICE_SESSIOND, LOADER_SPAWN_ARG_BYTES, LOADER_SPAWN_ENV_BYTES,
    LOADER_SPAWN_FLAG_DEFER_START, LOADER_SPAWN_FLAG_IMMEDIATE_HANDOFF, LOADER_SPAWN_MAX_ARG_COUNT,
    LOADER_SPAWN_MAX_ENV_COUNT, PROC_BROKER_ABI_VERSION, PROC_BROKER_BATCH_CAPACITY,
    PROC_BROKER_FORMAT_ELF64, PROC_BROKER_FORMAT_PE64, PROC_BROKER_LINUX_INTERP_PATH_CAPACITY,
    PROC_BROKER_MAP_EXEC, PROC_BROKER_MAP_PRIVATE, PROC_BROKER_MAP_READ, PROC_BROKER_MAP_WRITE,
    PROC_BROKER_PREPARE_FLAG_EXEC_TICKET, PROC_BROKER_USER_SPACE_BASE,
    PROC_BROKER_USER_SPACE_END_EXCLUSIVE, RustosProcAbortBrokerArgs, RustosProcActivateBrokerArgs,
    RustosProcAuthorizeExecBrokerArgs, RustosProcCancelExecBrokerArgs, RustosProcCommitBrokerArgs,
    RustosProcExecTargetBrokerArgs, RustosProcForkBrokerArgs, RustosProcMapDataBrokerArgs,
    RustosProcMapFileBatchBrokerArgs, RustosProcMapFileBrokerArgs, RustosProcMapZeroedBrokerArgs,
    RustosProcPrepareBrokerArgs, RustosProcSetLinuxRuntimeBrokerArgs,
    RustosProcSetWindowsRuntimeBrokerArgs, RustosProcSignalQueueBrokerArgs,
    RustosProcValidateDeferredSpawnBrokerArgs, RustosUserRegisters,
    loader_service_role_allows_operation,
};
const PAGE_SIZE: u64 = 4096;
const SPAWN_FLAG_LOGICAL_ADMIN: u64 = 1;
const MAX_PROC_PREPARES: usize = 128;
const MAX_MAPPINGS_PER_PREPARE: usize = 4096;
const MAX_EXEC_TICKETS: usize = 128;
const MAX_EXEC_TRANSITIONS: usize = MAX_EXEC_TICKETS;
const MAX_DEFERRED_ACTIVATIONS: usize = 128;
const FILE_COPY_CHUNK: usize = 64 * 1024;

static NEXT_PREPARE_HANDLE: AtomicU64 = AtomicU64::new(1);
static NEXT_EXEC_TICKET: AtomicU64 = AtomicU64::new(1);

type PinnedFileBacking = MemfdHandle;

#[derive(Clone)]
enum MappingEntry {
    File {
        backing: PinnedFileBacking,
        file_offset: u64,
        file_len: u64,
        target_addr: u64,
        mem_len: u64,
        flags: u64,
    },
    Zeroed {
        target_addr: u64,
        mem_len: u64,
        flags: u64,
    },
    Data {
        target_addr: u64,
        mem_len: u64,
        flags: u64,
        data_offset: u64,
        data: Vec<u8>,
    },
}

struct ProcPrepareState {
    owner_pid: u64,
    format: u16,
    exec_ticket: Option<u64>,
    #[allow(
        clippy::vec_box,
        reason = "mapping entries may carry a page payload; stable indirection avoids copying page-sized records when the bounded vector grows"
    )]
    mappings: Vec<Box<MappingEntry>>,
    windows_runtime: Option<crate::user::process::WindowsProcessLoaderRuntime>,
    linux_runtime: Option<(crate::user::linux::LinuxProcessImageInfo, u64)>,
}

#[derive(Clone, Copy)]
struct ExecTicketState {
    target_pid: u64,
    target_tid: u64,
}

#[derive(Clone, Copy)]
struct ExecTransitionState {
    target_pid: u64,
    target_tid: u64,
    registers: RustosUserRegisters,
}

type ProcPrepareRegistry = FnvIndexMap<u64, ProcPrepareState, MAX_PROC_PREPARES>;
type ExecTicketRegistry = FnvIndexMap<u64, ExecTicketState, MAX_EXEC_TICKETS>;
type ExecTransitionRegistry = FnvIndexMap<u64, ExecTransitionState, MAX_EXEC_TRANSITIONS>;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DeferredActivationAuthority {
    owner: multitask::ProcessIdentity,
    target: multitask::ProcessIdentity,
    qualification_required: bool,
}

type DeferredActivationRegistry =
    FnvIndexMap<u64, DeferredActivationAuthority, MAX_DEFERRED_ACTIVATIONS>;

static PROC_PREPARES: TrackedSpinLock<
    ProcPrepareRegistry,
    { LockClass::ProcBrokerRegistry as u8 },
> = TrackedSpinLock::new(FnvIndexMap::new());
static EXEC_TICKETS: TrackedSpinLock<ExecTicketRegistry, { LockClass::ProcBrokerRegistry as u8 }> =
    TrackedSpinLock::new(FnvIndexMap::new());
static EXEC_TRANSITIONS: TrackedSpinLock<
    ExecTransitionRegistry,
    { LockClass::ProcBrokerRegistry as u8 },
> = TrackedSpinLock::new(FnvIndexMap::new());
static DEFERRED_ACTIVATIONS: TrackedSpinLock<
    DeferredActivationRegistry,
    { LockClass::ProcBrokerRegistry as u8 },
> = TrackedSpinLock::new(FnvIndexMap::new());
pub(super) fn syscall_linux_rustos_scheduling_context_grant_broker(args_ptr: u64) -> u64 {
    scheduling_context_grants::grant(args_ptr)
}

pub(super) fn consume_direct_bootstrap_scheduling_context(
    requester_pid: u64,
    exec_path: &str,
) -> Result<rustos_user_abi::syscall::RustosSchedulingContextPolicy, i64> {
    scheduling_context_grants::consume_direct_bootstrap(requester_pid, exec_path)
}

pub(super) fn syscall_linux_rustos_proc_prepare_broker(args_ptr: u64) -> u64 {
    let Some(loader_pid) = current_loader_process_id() else {
        return linux_errno(LINUX_EPERM);
    };
    let args = match usermem::read_current_user_struct::<RustosProcPrepareBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != PROC_BROKER_ABI_VERSION
        || args.flags & !PROC_BROKER_PREPARE_FLAG_EXEC_TICKET != 0
        || (args.flags == 0) != (args.reserved0 == 0)
    {
        return linux_errno(LINUX_EINVAL);
    }
    let exec_ticket =
        (args.flags & PROC_BROKER_PREPARE_FLAG_EXEC_TICKET != 0).then_some(args.reserved0);
    let owner_pid = match exec_ticket {
        Some(ticket) => {
            if !EXEC_TICKETS.lock().contains_key(&ticket) {
                return linux_errno(LINUX_EPERM);
            }
            // procd already minted this exact one-shot exec authority before
            // calling loaderd. Re-entering procd here would deadlock the
            // procd -> loaderd -> prepare call chain.
            loader_pid
        }
        None => match procd_process_prepare_policy(args.format) {
            Ok(owner_pid) => owner_pid,
            Err(errno) => return linux_errno(errno),
        },
    };
    {
        let prepares = PROC_PREPARES.lock();
        if let Err(errno) = proc_prepare_publication_status(
            multitask::is_user_process_exiting(owner_pid),
            prepares.len(),
        ) {
            return linux_errno(errno);
        }
    }
    // Allocate all pointer slots before publication. Mapping payloads are
    // individually allocated before taking the registry lock, so subsequent
    // append operations cannot enter the allocator while holding it.
    let state = ProcPrepareState {
        owner_pid,
        format: args.format,
        exec_ticket,
        mappings: Vec::with_capacity(MAX_MAPPINGS_PER_PREPARE),
        windows_runtime: None,
        linux_runtime: None,
    };
    let mut prepares = PROC_PREPARES.lock();
    if let Err(errno) = proc_prepare_publication_status(
        multitask::is_user_process_exiting(owner_pid),
        prepares.len(),
    ) {
        return linux_errno(errno);
    }
    let Some(handle) = allocate_prepare_handle(&prepares) else {
        return linux_errno(LINUX_EAGAIN);
    };
    match prepares.insert(handle, state) {
        Ok(None) => {}
        Ok(Some(replaced)) => {
            drop(prepares);
            drop(replaced);
            return linux_errno(LINUX_EAGAIN);
        }
        Err((_handle, rejected)) => {
            drop(prepares);
            drop(rejected);
            return linux_errno(LINUX_EAGAIN);
        }
    }
    drop(prepares);
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "proc-prepare-published",
        handle,
        u64::from(args.format),
    );
    handle
}

pub(super) fn syscall_linux_rustos_proc_map_file_broker(args_ptr: u64) -> u64 {
    let Some(loader_pid) = current_loader_process_id() else {
        return linux_errno(LINUX_EPERM);
    };
    let args = match usermem::read_current_user_struct::<RustosProcMapFileBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let backing = match pinned_file_backing_from_current(args.fd) {
        Ok(b) => b,
        Err(e) => return linux_errno(e),
    };
    if let Err(errno) = validate_mapping_region(args.target_addr, args.mem_len, args.flags) {
        return linux_errno(errno);
    }
    if let Err(errno) = validate_file_mapping_len(args.mem_len, args.file_len) {
        return linux_errno(errno);
    }
    let mapping = Box::new(MappingEntry::File {
        backing,
        file_offset: args.file_offset,
        file_len: args.file_len,
        target_addr: args.target_addr,
        mem_len: args.mem_len,
        flags: args.flags,
    });
    let mut prepares = PROC_PREPARES.lock();
    let Some(state) = prepares.get_mut(&args.prepare_handle) else {
        return linux_errno(LINUX_EINVAL);
    };
    if !prepare_owned_by(state, loader_pid) {
        return linux_errno(LINUX_EPERM);
    }
    if state.mappings.len() >= MAX_MAPPINGS_PER_PREPARE {
        return linux_errno(LINUX_EINVAL);
    }
    state.mappings.push(mapping);
    0
}

pub(super) fn syscall_linux_rustos_proc_map_file_batch_broker(args_ptr: u64) -> u64 {
    let Some(current_pid) = current_loader_process_id() else {
        nucleus_core::debug::println_serialized(format_args!(
            "proc-map-file-batch denied stage=capability pid={:?} preempt_depth={}",
            multitask::current_user_process_id(),
            nucleus_core::util::lockdep::preemption_depth()
        ));
        return linux_errno(LINUX_EPERM);
    };
    let args = match usermem::read_current_user_struct::<RustosProcMapFileBatchBrokerArgs>(args_ptr)
    {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    let count = args.count as usize;
    if args.reserved0 != 0 || count == 0 || count > PROC_BROKER_BATCH_CAPACITY {
        return linux_errno(LINUX_EINVAL);
    }
    // Resolve and allocate the entire batch before locking the registry.
    let mut mappings: [Option<Box<MappingEntry>>; PROC_BROKER_BATCH_CAPACITY] =
        [const { None }; PROC_BROKER_BATCH_CAPACITY];
    for (index, entry) in args.entries[..count].iter().enumerate() {
        if entry.reserved0 != 0 {
            return linux_errno(LINUX_EINVAL);
        }
        if let Err(errno) = validate_mapping_region(entry.target_addr, entry.mem_len, entry.flags) {
            return linux_errno(errno);
        }
        if let Err(errno) = validate_file_mapping_len(entry.mem_len, entry.file_len) {
            return linux_errno(errno);
        }
        let backing = match pinned_file_backing_from_current(entry.fd) {
            Ok(backing) => backing,
            Err(errno) => return linux_errno(errno),
        };
        mappings[index] = Some(Box::new(MappingEntry::File {
            backing,
            file_offset: entry.file_offset,
            file_len: entry.file_len,
            target_addr: entry.target_addr,
            mem_len: entry.mem_len,
            flags: entry.flags,
        }));
    }
    let mut prepares = PROC_PREPARES.lock();
    let Some(state) = prepares.get_mut(&args.prepare_handle) else {
        return linux_errno(LINUX_EINVAL);
    };
    if current_pid != state.owner_pid {
        let owner_pid = state.owner_pid;
        drop(prepares);
        nucleus_core::debug::println_serialized(format_args!(
            "proc-map-file-batch denied stage=owner handle={} owner_pid={} current_pid={}",
            args.prepare_handle, owner_pid, current_pid
        ));
        return linux_errno(LINUX_EPERM);
    }
    if state.mappings.len() + count > MAX_MAPPINGS_PER_PREPARE {
        return linux_errno(LINUX_EINVAL);
    }
    for mapping in &mut mappings[..count] {
        state
            .mappings
            .push(mapping.take().expect("validated broker batch entry"));
    }
    drop(prepares);
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "proc-map-file-batch",
        args.prepare_handle,
        count as u64,
    );
    0
}

pub(super) fn syscall_linux_rustos_proc_set_linux_runtime_broker(args_ptr: u64) -> u64 {
    let Some(loader_pid) = current_loader_process_id() else {
        return linux_errno(LINUX_EPERM);
    };
    let args =
        match usermem::read_current_user_struct::<RustosProcSetLinuxRuntimeBrokerArgs>(args_ptr) {
            Ok(args) => args,
            Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
        };
    if args.abi_version != PROC_BROKER_ABI_VERSION || args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let interp_path_len = args.interp_path_len as usize;
    if interp_path_len > PROC_BROKER_LINUX_INTERP_PATH_CAPACITY {
        return linux_errno(LINUX_EINVAL);
    }
    let initial_tls = if args.has_tls != 0 {
        Some(crate::user::linux::LinuxInitialTlsInfo {
            template_addr: args.tls_template_addr,
            template_size: args.tls_template_size,
            mem_size: args.tls_mem_size,
            align: args.tls_align,
            mapping_base: args.tls_mapping_base,
            mapping_size: args.tls_mapping_size,
            tls_block_base: args.tls_block_base,
            thread_pointer: args.tls_thread_pointer,
            tcb_base: args.tls_tcb_base,
            dtv_base: args.tls_dtv_base,
        })
    } else {
        None
    };
    let interpreter_path = if interp_path_len > 0 {
        match core::str::from_utf8(&args.interp_path[..interp_path_len]) {
            Ok(s) => Some(String::from(s)),
            Err(_) => return linux_errno(LINUX_EINVAL),
        }
    } else {
        None
    };
    let info = crate::user::linux::LinuxProcessImageInfo {
        entry: args.entry,
        interpreter_base: args.interpreter_base,
        interpreter_path,
        program_headers: args.phdr_addr,
        program_header_entry_size: args.phent,
        program_header_count: args.phnum,
        brk_start: args.brk_start,
        bootstrap_heap_base: 0,
        bootstrap_heap_len: 0,
        initial_tls,
        image_mappings: Vec::new(),
        runtime_search_paths: Vec::new(),
    };
    let mut prepares = PROC_PREPARES.lock();
    let Some(state) = prepares.get_mut(&args.prepare_handle) else {
        return linux_errno(LINUX_EINVAL);
    };
    if !prepare_owned_by(state, loader_pid) {
        return linux_errno(LINUX_EPERM);
    }
    if state.format != PROC_BROKER_FORMAT_ELF64 || state.linux_runtime.is_some() {
        return linux_errno(LINUX_EINVAL);
    }
    state.linux_runtime = Some((info, args.actual_entry));
    drop(prepares);
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "proc-linux-runtime-published",
        args.prepare_handle,
        args.phnum,
    );
    0
}

pub(super) fn syscall_linux_rustos_proc_map_zeroed_broker(args_ptr: u64) -> u64 {
    let Some(loader_pid) = current_loader_process_id() else {
        return linux_errno(LINUX_EPERM);
    };
    let args = match usermem::read_current_user_struct::<RustosProcMapZeroedBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(errno) = validate_mapping_region(args.target_addr, args.mem_len, args.flags) {
        return linux_errno(errno);
    }
    let mapping = Box::new(MappingEntry::Zeroed {
        target_addr: args.target_addr,
        mem_len: args.mem_len,
        flags: args.flags,
    });
    let mut prepares = PROC_PREPARES.lock();
    let Some(state) = prepares.get_mut(&args.prepare_handle) else {
        return linux_errno(LINUX_EINVAL);
    };
    if !prepare_owned_by(state, loader_pid) {
        return linux_errno(LINUX_EPERM);
    }
    if state.mappings.len() >= MAX_MAPPINGS_PER_PREPARE {
        return linux_errno(LINUX_EINVAL);
    }
    state.mappings.push(mapping);
    0
}

pub(super) fn syscall_linux_rustos_proc_map_data_broker(args_ptr: u64) -> u64 {
    let Some(loader_pid) = current_loader_process_id() else {
        return linux_errno(LINUX_EPERM);
    };
    let args = match usermem::read_current_user_struct::<RustosProcMapDataBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    let data_len = args.data_len as usize;
    if args.reserved0 != 0
        || data_len > args.data.len()
        || args.data_offset.checked_add(args.data_len as u64).is_none()
        || args.data_offset + args.data_len as u64 > args.mem_len
    {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(errno) = validate_mapping_region(args.target_addr, args.mem_len, args.flags) {
        return linux_errno(errno);
    }
    let mapping = Box::new(MappingEntry::Data {
        target_addr: args.target_addr,
        mem_len: args.mem_len,
        flags: args.flags,
        data_offset: args.data_offset,
        data: args.data[..data_len].to_vec(),
    });
    let mut prepares = PROC_PREPARES.lock();
    let Some(state) = prepares.get_mut(&args.prepare_handle) else {
        return linux_errno(LINUX_EINVAL);
    };
    if !prepare_owned_by(state, loader_pid) {
        return linux_errno(LINUX_EPERM);
    }
    if state.mappings.len() >= MAX_MAPPINGS_PER_PREPARE {
        return linux_errno(LINUX_EINVAL);
    }
    state.mappings.push(mapping);
    0
}

pub(super) fn syscall_linux_rustos_proc_set_windows_runtime_broker(args_ptr: u64) -> u64 {
    let Some(loader_pid) = current_loader_process_id() else {
        return linux_errno(LINUX_EPERM);
    };
    let args = match usermem::read_current_user_struct::<RustosProcSetWindowsRuntimeBrokerArgs>(
        args_ptr,
    ) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != PROC_BROKER_ABI_VERSION
        || args.reserved0 != 0
        || args.reserved1 != 0
        || args.reserved2 != 0
        || args.loader_module_count == 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    let runtime_base = args.runtime_base;
    let runtime_size = args.runtime_size;
    if let Err(errno) = validate_user_range(runtime_base, runtime_size) {
        return linux_errno(errno);
    }
    if args.image_size == 0
        || args.image_base % PAGE_SIZE != 0
        || args.image_size % PAGE_SIZE != 0
        || !range_contains(args.image_base, args.image_size, args.entry_point, 1)
        || !range_contains(runtime_base, runtime_size, args.public_runtime_address, 1)
        || !range_contains(runtime_base, runtime_size, args.peb_address, 1)
        || !range_contains(runtime_base, runtime_size, args.teb_address, 1)
        || !range_contains(
            runtime_base,
            runtime_size,
            args.process_parameters_address,
            1,
        )
        || !range_contains(runtime_base, runtime_size, args.loader_data_address, 1)
        || !range_contains(
            runtime_base,
            runtime_size,
            args.loader_module_array_address,
            1,
        )
        || !range_contains(runtime_base, runtime_size, args.teb_process_id_ptr, 8)
        || !range_contains(runtime_base, runtime_size, args.teb_thread_id_ptr, 8)
    {
        return linux_errno(LINUX_EINVAL);
    }
    let mut prepares = PROC_PREPARES.lock();
    let Some(state) = prepares.get_mut(&args.prepare_handle) else {
        return linux_errno(LINUX_EINVAL);
    };
    if !prepare_owned_by(state, loader_pid) {
        return linux_errno(LINUX_EPERM);
    }
    if state.format != PROC_BROKER_FORMAT_PE64 || state.windows_runtime.is_some() {
        return linux_errno(LINUX_EINVAL);
    }
    state.windows_runtime = Some(crate::user::process::WindowsProcessLoaderRuntime {
        entry_point: args.entry_point,
        runtime: crate::user::process_state::WindowsProcessRuntimeState {
            image_base: args.image_base,
            image_size: args.image_size,
            allocation_base_hint: runtime_base.saturating_add(runtime_size),
            public_runtime_address: args.public_runtime_address,
            peb_address: args.peb_address,
            teb_address: args.teb_address,
            process_parameters_address: args.process_parameters_address,
            loader_data_address: args.loader_data_address,
            loader_module_array_address: args.loader_module_array_address,
            loader_module_count: args.loader_module_count,
            loader_reserved: 0,
            main_module_entry_address: args.main_module_entry_address,
            command_line_w_ptr: args.command_line_w_ptr,
            command_line_a_ptr: args.command_line_a_ptr,
            environment_w_ptr: args.environment_w_ptr,
            environment_a_ptr: args.environment_a_ptr,
            module_path_w_ptr: args.module_path_w_ptr,
            module_path_a_ptr: args.module_path_a_ptr,
            module_directory_w_ptr: args.module_directory_w_ptr,
            module_directory_a_ptr: args.module_directory_a_ptr,
            main_module_base_name_w_ptr: args.main_module_base_name_w_ptr,
            main_module_base_name_a_ptr: args.main_module_base_name_a_ptr,
            argc: args.argc,
            argc_ptr: args.argc_ptr,
            argv_ptr_ptr: args.argv_ptr_ptr,
            environ_ptr_ptr: args.environ_ptr_ptr,
            argv_ptr: args.argv_ptr,
            environ_ptr: args.environ_ptr,
            initial_narrow_environment_ptr: args.initial_narrow_environment_ptr,
            initenv_ptr: args.initenv_ptr,
            errno_ptr: args.errno_ptr,
            last_error_ptr: args.last_error_ptr,
            commode_ptr: args.commode_ptr,
            fmode_ptr: args.fmode_ptr,
            iob_array_ptr: args.iob_array_ptr,
            stdin_file_ptr: args.stdin_file_ptr,
            stdout_file_ptr: args.stdout_file_ptr,
            stderr_file_ptr: args.stderr_file_ptr,
            localeconv_ptr: args.localeconv_ptr,
            strerror_einval_ptr: args.strerror_einval_ptr,
            strerror_enomem_ptr: args.strerror_enomem_ptr,
            strerror_eio_ptr: args.strerror_eio_ptr,
            strerror_erange_ptr: args.strerror_erange_ptr,
            strerror_unknown_ptr: args.strerror_unknown_ptr,
        },
    });
    0
}

pub(super) fn syscall_linux_rustos_proc_commit_broker(args_ptr: u64) -> u64 {
    let Some(loader_pid) = current_loader_process_id() else {
        return linux_errno(LINUX_EPERM);
    };
    let args = match usermem::read_current_user_struct::<RustosProcCommitBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    const KNOWN_SPAWN_FLAGS: u64 = SPAWN_FLAG_LOGICAL_ADMIN
        | LOADER_SPAWN_FLAG_IMMEDIATE_HANDOFF as u64
        | LOADER_SPAWN_FLAG_DEFER_START as u64;
    if args.requester_pid == 0
        || args.prepare_handle == 0
        || args.flags & !KNOWN_SPAWN_FLAGS != 0
        || args.flags & LOADER_SPAWN_FLAG_IMMEDIATE_HANDOFF as u64 != 0
            && args.flags & LOADER_SPAWN_FLAG_DEFER_START as u64 != 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    if multitask::is_user_process_exiting(args.requester_pid) {
        return linux_errno(LINUX_ESRCH);
    }
    if !requester_owns_live_spawn_role(args.requester_pid) {
        return linux_errno(LINUX_EPERM);
    }
    let state = {
        let mut prepares = PROC_PREPARES.lock();
        match prepares.get(&args.prepare_handle) {
            None => return linux_errno(LINUX_EINVAL),
            Some(s) if !prepare_owned_by(s, loader_pid) => return linux_errno(LINUX_EPERM),
            Some(s) if s.exec_ticket.is_some() => return linux_errno(LINUX_EPERM),
            _ => {}
        }
        prepares.remove(&args.prepare_handle).unwrap()
    };
    if !matches!(
        state.format,
        PROC_BROKER_FORMAT_ELF64 | PROC_BROKER_FORMAT_PE64
    ) {
        return linux_errno(LINUX_EINVAL);
    }
    let exec_path = match read_user_text(args.exec_path_ptr, args.exec_path_len) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let scheduling_policy = match scheduling_context_grants::consume(
        args.scheduling_context,
        args.requester_pid,
        exec_path.as_str(),
    ) {
        Ok(policy) => policy,
        Err(errno) => return linux_errno(errno),
    };
    let qualification_required =
        super::smp_qualification_ops::smp_qualification_exec_path_matches(exec_path.as_str());
    let argv_storage = match read_user_string_vector(
        args.argv_ptr,
        LOADER_SPAWN_MAX_ARG_COUNT,
        LOADER_SPAWN_ARG_BYTES,
    ) {
        Ok(values) => values,
        Err(errno) => return linux_errno(errno),
    };
    let env_storage = match read_user_string_vector(
        args.envp_ptr,
        LOADER_SPAWN_MAX_ENV_COUNT,
        LOADER_SPAWN_ENV_BYTES,
    ) {
        Ok(values) => values,
        Err(errno) => return linux_errno(errno),
    };
    let argv_refs = argv_storage.iter().map(String::as_str).collect::<Vec<_>>();
    let env_refs = env_storage.iter().map(String::as_str).collect::<Vec<_>>();
    let linux_launch = crate::user::linux::LinuxProcessLaunch {
        exec_path: &exec_path,
        argv: argv_refs.as_slice(),
        env: env_refs.as_slice(),
    };
    let spawn_transaction = match crate::user::process::reserve_process_spawn_transaction() {
        Ok(transaction) => transaction,
        Err(err) => return linux_errno(process_load_error_to_linux_errno(err)),
    };
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "proc-commit-address-space-begin",
        args.prepare_handle,
        state.mappings.len() as u64,
    );
    let address_space = match address_space_from_mappings(&state.mappings) {
        Ok(address_space) => address_space,
        Err(errno) => return linux_errno(errno),
    };
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "proc-commit-address-space-done",
        args.prepare_handle,
        state.mappings.len() as u64,
    );
    let session = if args.console_session == 0 {
        match multitask::current_user_snapshot() {
            Some(snapshot) => snapshot.console_session(),
            None => return linux_errno(LINUX_EINVAL),
        }
    } else {
        crate::io::session::ConsoleSessionHandle::from_raw(args.console_session)
    };
    let logical_admin = args.flags & SPAWN_FLAG_LOGICAL_ADMIN != 0;
    let launch = crate::user::process::ProcessLaunchOptions {
        registers: crate::user::process::ProcessStartRegisters::new(),
        linux: linux_launch,
        console_session: session,
        logical_admin,
    };
    let prepared = match state.format {
        PROC_BROKER_FORMAT_ELF64 => {
            // loaderd always populates `linux_runtime` before issuing commit;
            // the bytes-image fallback that used to call into ring0 ELF parsing
            // was retired with the PE bytes path on 2026-05-20.
            let Some((info, actual_entry)) = state.linux_runtime else {
                return linux_errno(LINUX_EINVAL);
            };
            match crate::user::process::prepare_linux_process_with_metadata(
                info,
                actual_entry,
                address_space,
                crate::user::process::ProcessLaunchOptions {
                    linux: linux_launch,
                    ..launch
                },
            ) {
                Ok(prepared) => prepared,
                Err(err) => return linux_errno(process_load_error_to_linux_errno(err)),
            }
        }
        PROC_BROKER_FORMAT_PE64 => {
            let Some(metadata) = state.windows_runtime else {
                return linux_errno(LINUX_EINVAL);
            };
            match crate::user::process::prepare_windows_process_with_address_space(
                metadata,
                address_space,
                launch,
            ) {
                Ok(prepared) => prepared,
                Err(err) => return linux_errno(process_load_error_to_linux_errno(err)),
            }
        }
        _ => return linux_errno(LINUX_EINVAL),
    };
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "proc-commit-prepare-done",
        args.prepare_handle,
        state.format as u64,
    );
    let prepared = crate::user::process::bind_prepared_spawn(prepared, spawn_transaction);
    let spawned = if args.flags & LOADER_SPAWN_FLAG_DEFER_START as u64 != 0 {
        crate::user::process::spawn_prepared_process_suspended_with_scheduling_context(
            prepared,
            args.weight_micros,
            scheduling_policy,
        )
    } else if args.flags & LOADER_SPAWN_FLAG_IMMEDIATE_HANDOFF as u64 != 0 {
        crate::user::process::spawn_prepared_process_with_scheduling_context(
            prepared,
            args.weight_micros,
            scheduling_policy,
        )
    } else {
        crate::user::process::spawn_prepared_process_for_loader_reply_with_scheduling_context(
            prepared,
            args.weight_micros,
            scheduling_policy,
        )
    };
    match spawned {
        Ok(spawned) => {
            if args.flags & LOADER_SPAWN_FLAG_DEFER_START as u64 != 0 {
                let Some(owner) = multitask::live_user_process_identity_by_pid(args.requester_pid)
                else {
                    let _ = multitask::terminate_user_process(spawned.pid);
                    return linux_errno(LINUX_ESRCH);
                };
                let Some(target) = multitask::live_user_process_identity_by_pid(spawned.pid) else {
                    let _ = multitask::terminate_user_process(spawned.pid);
                    return linux_errno(LINUX_ESRCH);
                };
                let authority = DeferredActivationAuthority {
                    owner,
                    target,
                    qualification_required,
                };
                let mut activations = DEFERRED_ACTIVATIONS.lock();
                if activations.len() >= MAX_DEFERRED_ACTIVATIONS
                    || activations.contains_key(&spawned.pid)
                {
                    drop(activations);
                    let _ = multitask::terminate_user_process(spawned.pid);
                    return linux_errno(LINUX_EAGAIN);
                }
                if activations.insert(spawned.pid, authority).is_err() {
                    drop(activations);
                    let _ = multitask::terminate_user_process(spawned.pid);
                    return linux_errno(LINUX_EAGAIN);
                }
                drop(activations);
                // Close the requester-exit race on both sides of publication:
                // cleanup either observes the new entry, or this recheck
                // consumes it and retires the still-suspended target.
                if multitask::live_user_process_identity_by_pid(args.requester_pid) != Some(owner) {
                    DEFERRED_ACTIVATIONS.lock().remove(&spawned.pid);
                    let _ = multitask::terminate_user_process(spawned.pid);
                    return linux_errno(LINUX_ESRCH);
                }
            }
            spawned.pid
        }
        Err(err) => linux_errno(process_load_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_proc_activate_broker(args_ptr: u64) -> u64 {
    if !current_process_can_load() {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosProcActivateBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != PROC_BROKER_ABI_VERSION
        || args.reserved0 != 0
        || args.flags != 0
        || args.target_pid == 0
        || args.requester_pid == 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    // This syscall runs in loaderd's service context, but activation authority
    // belongs to the original requester captured at deferred-spawn commit.
    // Re-resolve that requester as a live, generation-bound identity so a
    // restarted loaderd neither denies a valid request nor adopts its owner.
    let (owner, target) = match resolve_deferred_activation_identities(
        args.requester_pid,
        args.target_pid,
        multitask::live_user_process_identity_by_pid,
    ) {
        Ok(identities) => identities,
        Err(errno) => return linux_errno(errno),
    };
    let mut activations = DEFERRED_ACTIVATIONS.lock();
    if !deferred_spawn_authority_matches(&activations, args.target_pid, owner, target) {
        return linux_errno(LINUX_EPERM);
    }
    let authority = *activations
        .get(&args.target_pid)
        .expect("preflighted deferred activation authority");
    let qualification_armed =
        match super::smp_qualification_ops::prepare_smp_qualification_activation(
            owner,
            target,
            authority.qualification_required,
        ) {
            Ok(armed) => armed,
            Err(errno) => return linux_errno(errno),
        };
    if !multitask::activate_suspended_user_tasks_with_commit(
        core::slice::from_ref(&args.target_pid),
        || {
            assert_eq!(
                activations.remove(&args.target_pid),
                Some(authority),
                "proc activation invariant: preflighted authority disappeared while locked"
            );
        },
    ) {
        // Scheduler preflight failed before the callback consumed the
        // authority. Retire both records; a bound child must never remain
        // eligible after a failed runnable publication.
        let removed = activations.remove(&args.target_pid);
        drop(activations);
        if qualification_armed {
            super::smp_qualification_ops::abort_smp_qualification_activation(target);
        }
        let _ = removed;
        let _ = multitask::terminate_user_process(args.target_pid);
        return linux_errno(LINUX_ESRCH);
    }
    drop(activations);
    // One-shot capability consumption precedes runnable publication inside
    // one ProcBrokerRegistry -> Scheduler critical section. Requester cleanup
    // can win before it or observe the committed child after it, never between.
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "proc-activate-committed",
        args.target_pid,
        args.requester_pid,
    );
    0
}

pub(super) fn syscall_linux_rustos_proc_validate_deferred_spawn_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_ROOT_SUPERVISOR) {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosProcValidateDeferredSpawnBrokerArgs>(
        args_ptr,
    ) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != PROC_BROKER_ABI_VERSION
        || args.reserved0 != 0
        || args.flags != 0
        || args.target_pid == 0
        || args.requester_pid == 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(owner) = multitask::live_user_process_identity_by_pid(args.requester_pid) else {
        return linux_errno(LINUX_ESRCH);
    };
    let Some(target) = multitask::live_user_process_identity_by_pid(args.target_pid) else {
        return linux_errno(LINUX_ESRCH);
    };
    if !deferred_spawn_authority_matches(
        &DEFERRED_ACTIVATIONS.lock(),
        args.target_pid,
        owner,
        target,
    ) {
        return linux_errno(LINUX_EPERM);
    }
    0
}

pub(super) fn syscall_linux_rustos_proc_authorize_exec_broker(args_ptr: u64) -> u64 {
    if !current_process_can_policy() {
        return linux_errno(LINUX_EPERM);
    }
    let args =
        match usermem::read_current_user_struct::<RustosProcAuthorizeExecBrokerArgs>(args_ptr) {
            Ok(args) => args,
            Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
        };
    if args.abi_version != PROC_BROKER_ABI_VERSION
        || args.reserved0 != 0
        || args.reserved1 != 0
        || args.target_pid == 0
        || args.target_tid == 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    if multitask::linux_thread_snapshot_by_ids(args.target_pid, args.target_tid).is_none()
        || multitask::is_user_process_exiting(args.target_pid)
    {
        return linux_errno(LINUX_ESRCH);
    }
    let ticket = {
        let mut tickets = EXEC_TICKETS.lock();
        if tickets.len() >= MAX_EXEC_TICKETS {
            return linux_errno(LINUX_EAGAIN);
        }
        let Some(ticket) = allocate_exec_ticket(&tickets) else {
            return linux_errno(LINUX_EAGAIN);
        };
        if tickets
            .insert(
                ticket,
                ExecTicketState {
                    target_pid: args.target_pid,
                    target_tid: args.target_tid,
                },
            )
            .is_err()
        {
            return linux_errno(LINUX_EAGAIN);
        }
        ticket
    };
    // Process teardown runs independently of procd authorization. Recheck
    // after publication so an exit between the first snapshot and insertion
    // cannot retain one of the bounded ticket slots.
    if multitask::linux_thread_snapshot_by_ids(args.target_pid, args.target_tid).is_none()
        || multitask::is_user_process_exiting(args.target_pid)
    {
        EXEC_TICKETS.lock().remove(&ticket);
        return linux_errno(LINUX_ESRCH);
    }
    ticket
}

pub(super) fn syscall_linux_rustos_proc_cancel_exec_broker(args_ptr: u64) -> u64 {
    if !current_process_can_policy() {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosProcCancelExecBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != PROC_BROKER_ABI_VERSION
        || args.reserved0 != 0
        || args.reserved1 != 0
        || args.exec_ticket == 0
        || args.target_pid == 0
        || args.target_tid == 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    let mut tickets = EXEC_TICKETS.lock();
    let Some(ticket) = tickets.get(&args.exec_ticket).copied() else {
        return linux_errno(LINUX_EINVAL);
    };
    if ticket.target_pid != args.target_pid || ticket.target_tid != args.target_tid {
        return linux_errno(LINUX_EPERM);
    }
    tickets.remove(&args.exec_ticket);
    0
}

pub(super) fn syscall_linux_rustos_proc_exec_target_broker(args_ptr: u64) -> u64 {
    let Some(loader_pid) = current_loader_process_id() else {
        return linux_errno(LINUX_EPERM);
    };
    let args = match usermem::read_current_user_struct::<RustosProcExecTargetBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != PROC_BROKER_ABI_VERSION
        || args.reserved0 != 0
        || args.requester_pid == 0
        || args.target_pid == 0
        || args.target_tid == 0
        || args.exec_ticket == 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    if multitask::is_user_process_exiting(args.requester_pid) {
        return linux_errno(LINUX_ESRCH);
    }
    if !ipc_ops::process_owns_live_service_endpoint(args.requester_pid, IPC_SERVICE_PROCD) {
        return linux_errno(LINUX_EPERM);
    }
    {
        let mut tickets = EXEC_TICKETS.lock();
        let Some(ticket) = tickets.get(&args.exec_ticket).copied() else {
            return linux_errno(LINUX_EINVAL);
        };
        if ticket.target_pid != args.target_pid || ticket.target_tid != args.target_tid {
            return linux_errno(LINUX_EPERM);
        }
        tickets.remove(&args.exec_ticket);
    }
    let state = {
        let mut prepares = PROC_PREPARES.lock();
        match prepares.get(&args.prepare_handle) {
            None => return linux_errno(LINUX_EINVAL),
            Some(s) if !prepare_owned_by(s, loader_pid) => return linux_errno(LINUX_EPERM),
            _ => {}
        }
        prepares.remove(&args.prepare_handle).unwrap()
    };
    if !exec_prepare_ticket_matches(state.exec_ticket, args.exec_ticket)
        || state.format != PROC_BROKER_FORMAT_ELF64
        || state.windows_runtime.is_some()
    {
        return linux_errno(LINUX_EINVAL);
    }
    let exec_path = match read_user_text(args.exec_path_ptr, args.exec_path_len) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let argv_storage = match read_user_string_vector(
        args.argv_ptr,
        LOADER_SPAWN_MAX_ARG_COUNT,
        LOADER_SPAWN_ARG_BYTES,
    ) {
        Ok(values) => values,
        Err(errno) => return linux_errno(errno),
    };
    let env_storage = match read_user_string_vector(
        args.envp_ptr,
        LOADER_SPAWN_MAX_ENV_COUNT,
        LOADER_SPAWN_ENV_BYTES,
    ) {
        Ok(values) => values,
        Err(errno) => return linux_errno(errno),
    };
    let argv_refs = argv_storage.iter().map(String::as_str).collect::<Vec<_>>();
    let env_refs = env_storage.iter().map(String::as_str).collect::<Vec<_>>();
    let linux_launch = crate::user::linux::LinuxProcessLaunch {
        exec_path: &exec_path,
        argv: argv_refs.as_slice(),
        env: env_refs.as_slice(),
    };
    let address_space = match address_space_from_mappings(&state.mappings) {
        Ok(address_space) => address_space,
        Err(errno) => return linux_errno(errno),
    };
    let session = if args.console_session == 0 {
        match multitask::linux_thread_snapshot_by_ids(args.target_pid, args.target_tid) {
            Some(snapshot) => snapshot.console_session,
            None => return linux_errno(LINUX_EINVAL),
        }
    } else {
        crate::io::session::ConsoleSessionHandle::from_raw(args.console_session)
    };
    let logical_admin = multitask::with_process_state_by_pid(args.target_pid, |state| {
        state.security().is_logical_admin()
    })
    .unwrap_or(false);
    let launch = crate::user::process::ProcessLaunchOptions {
        registers: crate::user::process::ProcessStartRegisters::new(),
        linux: linux_launch,
        console_session: session,
        logical_admin,
    };
    // Same loaderd contract as prepare: linux_runtime must be populated; the
    // bytes-image fallback into ring0 ELF parsing was retired.
    let Some((info, actual_entry)) = state.linux_runtime else {
        return linux_errno(LINUX_EINVAL);
    };
    let prepared = match crate::user::process::prepare_linux_process_with_metadata(
        info,
        actual_entry,
        address_space,
        crate::user::process::ProcessLaunchOptions {
            linux: linux_launch,
            ..launch
        },
    ) {
        Ok(prepared) => prepared,
        Err(err) => return linux_errno(process_load_error_to_linux_errno(err)),
    };
    let transition = exec_transition_from_prepared(args.target_pid, args.target_tid, &prepared);
    {
        let mut transitions = EXEC_TRANSITIONS.lock();
        // Publishing the transition before replacing the target address space
        // closes the scheduler-visible window where a new image has no saved
        // user-register handoff. A second concurrent exec for the same thread
        // must not overwrite that handoff.
        if transitions.contains_key(&args.target_tid) {
            return linux_errno(LINUX_EBUSY);
        }
        if transitions.insert(args.target_tid, transition).is_err() {
            return linux_errno(LINUX_EAGAIN);
        }
    }
    if let Some(closed_handles) = multitask::exec_user_process_by_pid(
        args.target_pid,
        args.target_tid,
        prepared.address_space,
        prepared.bootstrap,
    ) {
        // The process-table commit has already removed CLOEXEC descriptors.
        // Derive cleanup from the exact handles retired by that commit, never
        // from an earlier fd snapshot that a sibling can close/reuse before
        // replacement. A provider restart independently revokes any call that
        // cannot complete this bounded cleanup.
        let cloexec_service_refs: Vec<_> = closed_handles
            .iter()
            .filter_map(service_handle_ref_for_handle)
            .collect();
        let cloexec_console_handles: Vec<_> = closed_handles
            .into_iter()
            .filter_map(|handle| match handle {
                multitask::KernelHandle::Console(console) => Some(console),
                _ => None,
            })
            .collect();
        release_service_handle_refs_bounded(&cloexec_service_refs);
        purge_closed_console_handles(cloexec_console_handles, true);
        // Linux exec retires every sibling thread. Their exact tickets and
        // handoffs can no longer become valid after the scheduler clears those
        // slots, so release them with the same target identity boundary.
        cleanup_proc_broker_exec_state_for_siblings(args.target_pid, args.target_tid);
        args.target_pid
    } else {
        EXEC_TRANSITIONS.lock().remove(&args.target_tid);
        linux_errno(LINUX_ESRCH)
    }
}

fn exec_prepare_ticket_matches(bound_ticket: Option<u64>, requested_ticket: u64) -> bool {
    requested_ticket != 0 && bound_ticket == Some(requested_ticket)
}

pub(super) fn apply_pending_exec_transition(frame: &mut SyscallFrame) -> bool {
    let Some(tid) = multitask::current_user_thread_id() else {
        return false;
    };
    let Some(transition) = EXEC_TRANSITIONS.lock().remove(&tid) else {
        return false;
    };
    if multitask::current_user_process_id() != Some(transition.target_pid)
        || tid != transition.target_tid
    {
        return false;
    }
    frame.user_rsp = transition.registers.rsp;
    frame.user_rip = transition.registers.rip;
    frame.user_rflags = transition.registers.rflags;
    frame.rax = transition.registers.rax;
    frame.rdi = transition.registers.rdi;
    frame.rsi = transition.registers.rsi;
    frame.rdx = transition.registers.rdx;
    frame.r8 = transition.registers.r8;
    frame.r9 = transition.registers.r9;
    frame.r10 = transition.registers.r10;
    frame.rbx = transition.registers.rbx;
    frame.rbp = transition.registers.rbp;
    frame.r12 = transition.registers.r12;
    frame.r13 = transition.registers.r13;
    frame.r14 = transition.registers.r14;
    frame.r15 = transition.registers.r15;
    true
}

pub(super) fn syscall_linux_rustos_proc_fork_broker(args_ptr: u64) -> u64 {
    fork::syscall_linux_rustos_proc_fork_broker(args_ptr)
}

fn valid_process_fork_plan_locally(args: &RustosProcForkBrokerArgs) -> bool {
    fork::valid_process_fork_plan_locally(args)
}

pub(super) fn syscall_linux_rustos_proc_signal_queue_broker(args_ptr: u64) -> u64 {
    if !current_process_can_policy() {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosProcSignalQueueBrokerArgs>(args_ptr)
    {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != PROC_BROKER_ABI_VERSION
        || args.reserved0 != 0
        || args.signal > crate::user::linux::MAX_SIGNAL_NUMBER as u32
        || args.target_pid == 0
        || args.target_tid == 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    if multitask::queue_linux_signal(args.target_pid, args.target_tid, args.signal as u64) {
        0
    } else {
        linux_errno(LINUX_ESRCH)
    }
}

pub(super) fn syscall_linux_rustos_proc_abort_broker(args_ptr: u64) -> u64 {
    let Some(loader_pid) = current_loader_process_id() else {
        return linux_errno(LINUX_EPERM);
    };
    let args = match usermem::read_current_user_struct::<RustosProcAbortBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let mut prepares = PROC_PREPARES.lock();
    if let Some(state) = prepares.get(&args.prepare_handle)
        && !prepare_owned_by(state, loader_pid)
    {
        return linux_errno(LINUX_EPERM);
    }
    let removed = prepares.remove(&args.prepare_handle);
    drop(prepares);
    drop(removed);
    0
}

/// Process-broker state is bounded and process-scoped. A loader that exits
/// between PREPARE and COMMIT cannot reach ABORT, and a target that exits after
/// procd authorization cannot consume a later exec ticket. Process teardown
/// therefore removes owner-bound prepares plus target-bound tickets and saved
/// register transitions before the process table retires that process.
pub(super) fn cleanup_proc_broker_state_for_process(process_id: u64) -> (usize, usize, usize) {
    // The process table has already linearized terminal teardown before this
    // cleanup entry. Revoke the generation-bound qualification first so a
    // reused PID cannot inherit an owner or target evidence grant.
    super::smp_qualification_ops::revoke_smp_qualification_for_process(process_id);
    scheduling_context_grants::revoke_for_process(process_id);
    let mut deferred_targets = [0_u64; MAX_DEFERRED_ACTIVATIONS];
    let deferred_target_count = {
        let mut activations = DEFERRED_ACTIVATIONS.lock();
        let mut count = 0usize;
        for (target_pid, authority) in activations.iter() {
            if authority.owner.process_id() == process_id && *target_pid != process_id {
                deferred_targets[count] = *target_pid;
                count += 1;
            }
        }
        activations.retain(|target_pid, authority| {
            *target_pid != process_id
                && authority.owner.process_id() != process_id
                && authority.target.process_id() != process_id
        });
        count
    };
    for target_pid in deferred_targets[..deferred_target_count].iter().copied() {
        let _ = multitask::terminate_user_process(target_pid);
    }

    // ProcPrepareState owns heap allocations and pinned file references. Remove
    // one state at a time so its destructor never runs under the registry lock.
    let mut removed_prepares = 0usize;
    loop {
        let removed = {
            let mut prepares = PROC_PREPARES.lock();
            let key = prepares
                .iter()
                .find_map(|(key, state)| (state.owner_pid == process_id).then_some(*key));
            key.and_then(|key| prepares.remove(&key))
        };
        let Some(state) = removed else {
            break;
        };
        removed_prepares += 1;
        drop(state);
    }

    let mut tickets = EXEC_TICKETS.lock();
    let tickets_before = tickets.len();
    tickets.retain(|_, state| state.target_pid != process_id);
    let removed_tickets = tickets_before.saturating_sub(tickets.len());
    drop(tickets);

    let mut transitions = EXEC_TRANSITIONS.lock();
    let transitions_before = transitions.len();
    transitions.retain(|_, state| state.target_pid != process_id);
    let removed_transitions = transitions_before.saturating_sub(transitions.len());

    (removed_prepares, removed_tickets, removed_transitions)
}

/// A non-final Linux thread exit, or a sibling retirement caused by exec,
/// leaves the process table alive but invalidates the exact TID binding of its
/// tickets and saved user-register handoff.
pub(super) fn cleanup_proc_broker_exec_state_for_thread(
    process_id: u64,
    thread_id: u64,
) -> (usize, usize) {
    let mut tickets = EXEC_TICKETS.lock();
    let tickets_before = tickets.len();
    tickets.retain(|_, state| state.target_pid != process_id || state.target_tid != thread_id);
    let removed_tickets = tickets_before.saturating_sub(tickets.len());
    drop(tickets);

    let mut transitions = EXEC_TRANSITIONS.lock();
    let transitions_before = transitions.len();
    transitions.retain(|_, state| state.target_pid != process_id || state.target_tid != thread_id);
    let removed_transitions = transitions_before.saturating_sub(transitions.len());

    (removed_tickets, removed_transitions)
}

fn cleanup_proc_broker_exec_state_for_siblings(process_id: u64, surviving_thread_id: u64) {
    let mut tickets = EXEC_TICKETS.lock();
    tickets.retain(|_, state| {
        state.target_pid != process_id || state.target_tid == surviving_thread_id
    });
    drop(tickets);

    let mut transitions = EXEC_TRANSITIONS.lock();
    transitions.retain(|_, state| {
        state.target_pid != process_id || state.target_tid == surviving_thread_id
    });
}

fn current_process_can_load() -> bool {
    ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_PROCESS_LOADER)
}

/// Revalidate the original loader client at the terminal ring0 commit. This
/// closes the long image-load window: a service restart or revoke after
/// loaderd's admission check withdraws authority before any process is born.
fn requester_owns_live_spawn_role(requester_pid: u64) -> bool {
    [IPC_SERVICE_ROOTD, IPC_SERVICE_INITD, IPC_SERVICE_SESSIOND]
        .into_iter()
        .any(|service_id| {
            loader_service_role_allows_operation(
                rustos_user_abi::syscall::LOADER_OP_SPAWN_EXEC,
                service_id,
            ) && ipc_ops::process_owns_live_service_endpoint(requester_pid, service_id)
        })
}

fn current_process_can_policy() -> bool {
    ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_PROCESS_POLICY)
}

fn user_registers_to_task_registers(
    registers: RustosUserRegisters,
) -> multitask::UserTaskRegisters {
    multitask::UserTaskRegisters {
        rax: registers.rax,
        rbx: registers.rbx,
        rcx: registers.rcx,
        rdx: registers.rdx,
        rsi: registers.rsi,
        rdi: registers.rdi,
        rbp: registers.rbp,
        r8: registers.r8,
        r9: registers.r9,
        r10: registers.r10,
        r11: registers.r11,
        r12: registers.r12,
        r13: registers.r13,
        r14: registers.r14,
        r15: registers.r15,
    }
}

fn exec_transition_from_prepared(
    target_pid: u64,
    target_tid: u64,
    prepared: &crate::user::process::PreparedProcessImage,
) -> ExecTransitionState {
    let registers = prepared.bootstrap.registers;
    ExecTransitionState {
        target_pid,
        target_tid,
        registers: RustosUserRegisters {
            rax: registers.rax,
            rbx: registers.rbx,
            rcx: prepared.entry.as_u64(),
            rdx: registers.rdx,
            rsi: registers.rsi,
            rdi: registers.rdi,
            rbp: registers.rbp,
            rsp: prepared.bootstrap.stack_pointer.as_u64(),
            rip: prepared.entry.as_u64(),
            r8: registers.r8,
            r9: registers.r9,
            r10: registers.r10,
            r11: 0x202,
            r12: registers.r12,
            r13: registers.r13,
            r14: registers.r14,
            r15: registers.r15,
            rflags: 0x202,
        },
    }
}

fn address_space_from_mappings(
    mappings: &[Box<MappingEntry>],
) -> Result<crate::memory::paging::ProcessAddressSpace, i64> {
    let mut address_space = crate::memory::paging::ProcessAddressSpace::new()
        .map_err(address_space_error_to_linux_errno)?;
    for (index, mapping) in mappings.iter().enumerate() {
        match mapping.as_ref() {
            MappingEntry::Zeroed {
                target_addr,
                mem_len,
                flags,
            } => {
                map_zeroed(&mut address_space, *target_addr, *mem_len, *flags).inspect_err(
                    |errno| {
                        nucleus_core::debug::write_debugcon_only_line(
                            alloc::format!(
                                "proc-commit: mapping rejected stage=zeroed index={index} target={target_addr:#x} mem_len={mem_len:#x} errno={errno}"
                            )
                            .as_bytes(),
                        );
                    },
                )?;
            }
            MappingEntry::Data {
                target_addr,
                mem_len,
                flags,
                data_offset,
                data,
            } => {
                map_zeroed(&mut address_space, *target_addr, *mem_len, *flags).inspect_err(
                    |errno| {
                        nucleus_core::debug::write_debugcon_only_line(
                            alloc::format!(
                                "proc-commit: mapping rejected stage=data-map index={index} target={target_addr:#x} mem_len={mem_len:#x} data_offset={data_offset:#x} data_len={:#x} errno={errno}",
                                data.len()
                            )
                            .as_bytes(),
                        );
                    },
                )?;
                let write_addr = target_addr
                    .checked_add(*data_offset)
                    .ok_or_else(|| {
                        nucleus_core::debug::write_debugcon_only_line(
                            alloc::format!(
                                "proc-commit: mapping rejected stage=data-address index={index} target={target_addr:#x} data_offset={data_offset:#x}"
                            )
                            .as_bytes(),
                        );
                        LINUX_EOVERFLOW
                    })?;
                address_space
                    .initialize_user_bytes(VirtAddr::new(write_addr), data)
                    .map_err(address_space_error_to_linux_errno)?;
            }
            MappingEntry::File {
                backing,
                file_offset,
                file_len,
                target_addr,
                mem_len,
                flags,
            } => {
                map_zeroed(&mut address_space, *target_addr, *mem_len, *flags).inspect_err(
                    |errno| {
                        nucleus_core::debug::write_debugcon_only_line(
                            alloc::format!(
                                "proc-commit: mapping rejected stage=file-map index={index} target={target_addr:#x} mem_len={mem_len:#x} file_offset={file_offset:#x} file_len={file_len:#x} errno={errno}"
                            )
                            .as_bytes(),
                        );
                    },
                )?;
                copy_file_into_address_space(
                    &mut address_space,
                    backing,
                    *target_addr,
                    *file_offset,
                    *file_len,
                )
                .inspect_err(|errno| {
                    nucleus_core::debug::write_debugcon_only_line(
                        alloc::format!(
                            "proc-commit: mapping rejected stage=file-copy index={index} target={target_addr:#x} file_offset={file_offset:#x} file_len={file_len:#x} errno={errno}"
                        )
                        .as_bytes(),
                    );
                })?;
            }
        }
    }
    Ok(address_space)
}

fn copy_file_into_address_space(
    address_space: &mut crate::memory::paging::ProcessAddressSpace,
    backing: &PinnedFileBacking,
    target_addr: u64,
    file_offset: u64,
    file_len: u64,
) -> Result<(), i64> {
    let total = usize::try_from(file_len).map_err(|_| LINUX_EOVERFLOW)?;
    let mut chunk = alloc::vec![0_u8; FILE_COPY_CHUNK.min(total.max(1))];
    let mut copied = 0usize;
    while copied < total {
        let count = (total - copied).min(chunk.len());
        let off = usize::try_from(file_offset)
            .map_err(|_| LINUX_EINVAL)?
            .checked_add(copied)
            .ok_or(LINUX_EOVERFLOW)?;
        let read = backing.read_at(off, &mut chunk[..count]);
        if read == 0 {
            break;
        }
        address_space
            .initialize_user_bytes(VirtAddr::new(target_addr + copied as u64), &chunk[..read])
            .map_err(address_space_error_to_linux_errno)?;
        copied += read;
    }
    validate_complete_file_copy(total, copied)
}

fn validate_complete_file_copy(expected: usize, copied: usize) -> Result<(), i64> {
    (copied == expected).then_some(()).ok_or(LINUX_EIO)
}

fn validate_file_mapping_len(mem_len: u64, file_len: u64) -> Result<(), i64> {
    if file_len > mem_len {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

fn pinned_file_backing_from_current(fd: u64) -> Result<PinnedFileBacking, i64> {
    let Some(result) = multitask::with_current_process_state(|_, process_state| {
        let entry = process_state.handles().get_entry(fd).ok_or(LINUX_EBADF)?;
        if !entry.rights().allows_read() {
            return Err(LINUX_EACCES);
        }
        match entry.handle() {
            KernelHandle::Memfd(memfd) if executable_snapshot_is_immutable(memfd) => {
                Ok(memfd.clone())
            }
            KernelHandle::Memfd(_) => Err(LINUX_EACCES),
            _ => Err(LINUX_EINVAL),
        }
    }) else {
        return Err(LINUX_ESRCH);
    };
    result
}

fn executable_snapshot_is_immutable(memfd: &MemfdHandle) -> bool {
    let required = (linux_abi::F_SEAL_WRITE
        | linux_abi::F_SEAL_GROW
        | linux_abi::F_SEAL_SHRINK
        | linux_abi::F_SEAL_SEAL) as u32;
    memfd.seals() & required == required
}

fn map_zeroed(
    address_space: &mut crate::memory::paging::ProcessAddressSpace,
    target_addr: u64,
    mem_len: u64,
    flags: u64,
) -> Result<(), i64> {
    let page_count = usize::try_from(mem_len / PAGE_SIZE).map_err(|_| LINUX_EOVERFLOW)?;
    address_space
        .map_zeroed_user_pages_at(VirtAddr::new(target_addr), page_count, page_flags(flags)?)
        .map_err(address_space_error_to_linux_errno)?;
    Ok(())
}

fn page_flags(flags: u64) -> Result<PageTableFlags, i64> {
    let mut page_flags = PageTableFlags::empty();
    if flags & PROC_BROKER_MAP_WRITE != 0 {
        page_flags |= PageTableFlags::WRITABLE;
    }
    if flags & PROC_BROKER_MAP_EXEC == 0 {
        page_flags |= PageTableFlags::NO_EXECUTE;
    }
    Ok(page_flags)
}

fn allocate_prepare_handle(prepares: &ProcPrepareRegistry) -> Option<u64> {
    for _ in 0..MAX_PROC_PREPARES {
        let handle = allocate_nonwrapping_broker_identity(&NEXT_PREPARE_HANDLE)?;
        if !prepares.contains_key(&handle) {
            return Some(handle);
        }
    }
    None
}

fn proc_prepare_publication_status(owner_exiting: bool, pending: usize) -> Result<(), i64> {
    // procd authorization runs before the registry lock. Process teardown sets
    // the exit marker before taking this same lock for cleanup, so publication
    // must revalidate the marker here or it can recreate a prepare after the
    // final cleanup pass and permanently consume one of the bounded slots.
    if owner_exiting {
        return Err(LINUX_ESRCH);
    }
    if pending >= MAX_PROC_PREPARES {
        return Err(LINUX_EAGAIN);
    }
    Ok(())
}

fn allocate_exec_ticket(tickets: &ExecTicketRegistry) -> Option<u64> {
    for _ in 0..MAX_EXEC_TICKETS {
        let ticket = allocate_nonwrapping_broker_identity(&NEXT_EXEC_TICKET)?;
        if !tickets.contains_key(&ticket) {
            return Some(ticket);
        }
    }
    None
}

fn resolve_deferred_activation_identities<I>(
    requester_pid: u64,
    target_pid: u64,
    mut resolve_live_identity: impl FnMut(u64) -> Option<I>,
) -> Result<(I, I), i64> {
    let Some(owner) = resolve_live_identity(requester_pid) else {
        return Err(LINUX_ESRCH);
    };
    let Some(target) = resolve_live_identity(target_pid) else {
        return Err(LINUX_ESRCH);
    };
    Ok((owner, target))
}

fn deferred_activation_identities_match<I: Eq>(
    stored_owner: &I,
    stored_target: &I,
    owner: &I,
    target: &I,
) -> bool {
    stored_owner == owner && stored_target == target
}

fn deferred_spawn_authority_matches(
    activations: &DeferredActivationRegistry,
    target_pid: u64,
    owner: multitask::ProcessIdentity,
    target: multitask::ProcessIdentity,
) -> bool {
    target_pid != 0
        && owner.process_id() != 0
        && target.process_id() == target_pid
        && activations.get(&target_pid).is_some_and(|authority| {
            deferred_activation_identities_match(
                &authority.owner,
                &authority.target,
                &owner,
                &target,
            )
        })
}

/// Linearize qualification binding with the deferred-child one-shot record.
/// The callback runs while ProcBrokerRegistry is retained, and may acquire
/// only the higher-ranked qualification binding lock. Therefore either bind
/// installs `BoundSuspended` before activation can consume the child, or
/// activation consumes the record first and a later bind fails closed.
pub(super) fn with_deferred_activation_authority_for_smp_bind<R>(
    target_pid: u64,
    owner: multitask::ProcessIdentity,
    target: multitask::ProcessIdentity,
    register: impl FnOnce() -> Result<R, i64>,
) -> Result<R, i64> {
    let activations = DEFERRED_ACTIVATIONS.lock();
    if !deferred_spawn_authority_matches(&activations, target_pid, owner, target)
        || !activations
            .get(&target_pid)
            .is_some_and(|authority| authority.qualification_required)
    {
        return Err(LINUX_EPERM);
    }
    let result = register();
    // Keep the authority guard live until the registration has returned. This
    // is the bind-vs-activate serialization edge, not a best-effort lookup.
    let _still_bound = activations.get(&target_pid).is_some();
    drop(activations);
    result
}

fn allocate_nonwrapping_broker_identity(counter: &AtomicU64) -> Option<u64> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            (current != 0).then(|| current.checked_add(1)).flatten()
        })
        .ok()
}

fn validate_mapping_region(target_addr: u64, mem_len: u64, flags: u64) -> Result<(), i64> {
    if mem_len == 0
        || !target_addr.is_multiple_of(PAGE_SIZE)
        || !mem_len.is_multiple_of(PAGE_SIZE)
        || target_addr < PROC_BROKER_USER_SPACE_BASE
        || target_addr
            .checked_add(mem_len)
            .is_none_or(|end| end > PROC_BROKER_USER_SPACE_END_EXCLUSIVE)
    {
        return Err(LINUX_EINVAL);
    }
    let supported = PROC_BROKER_MAP_READ
        | PROC_BROKER_MAP_WRITE
        | PROC_BROKER_MAP_EXEC
        | PROC_BROKER_MAP_PRIVATE;
    if flags & !supported != 0
        || flags & PROC_BROKER_MAP_PRIVATE == 0
        || flags & PROC_BROKER_MAP_WRITE != 0 && flags & PROC_BROKER_MAP_EXEC != 0
    {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

fn validate_user_range(start: u64, len: u64) -> Result<(), i64> {
    if len == 0
        || !start.is_multiple_of(PAGE_SIZE)
        || !len.is_multiple_of(PAGE_SIZE)
        || start < PROC_BROKER_USER_SPACE_BASE
        || start
            .checked_add(len)
            .is_none_or(|end| end > PROC_BROKER_USER_SPACE_END_EXCLUSIVE)
    {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

fn range_contains(base: u64, len: u64, ptr: u64, access_len: u64) -> bool {
    ptr >= base
        && ptr
            .checked_add(access_len)
            .is_some_and(|end| end <= base.saturating_add(len))
}

fn process_load_error_to_linux_errno(error: crate::user::process::ProcessLoadError) -> i64 {
    match error {
        crate::user::process::ProcessLoadError::MissingSchedulingContext => LINUX_EPERM,
        crate::user::process::ProcessLoadError::AddressSpace(err) => {
            address_space_error_to_linux_errno(err)
        }
        crate::user::process::ProcessLoadError::Spawn(err) => match err {
            multitask::SpawnTaskError::InvalidWeightMicros => LINUX_EINVAL,
            multitask::SpawnTaskError::NoFreeTaskSlot => LINUX_EAGAIN,
        },
        crate::user::process::ProcessLoadError::InterpreterLoad { .. } => LINUX_ENOEXEC,
        crate::user::process::ProcessLoadError::InvalidElf(_)
        | crate::user::process::ProcessLoadError::InvalidPe(_)
        | crate::user::process::ProcessLoadError::UnsupportedImport { .. } => LINUX_ENOEXEC,
    }
}

fn process_spawn_error_to_linux_errno(error: multitask::SpawnTaskError) -> i64 {
    match error {
        multitask::SpawnTaskError::InvalidWeightMicros => LINUX_EINVAL,
        multitask::SpawnTaskError::NoFreeTaskSlot => LINUX_EAGAIN,
    }
}

fn read_user_text(ptr: u64, len: u64) -> Result<String, i64> {
    let len = usize::try_from(len).map_err(|_| LINUX_EINVAL)?;
    if ptr == 0 || len == 0 || len > 4096 {
        return Err(LINUX_EINVAL);
    }
    let mut bytes = alloc::vec![0_u8; len];
    usermem::copy_from_current_user_exact(ptr, &mut bytes)
        .map_err(address_space_error_to_linux_errno)?;
    if bytes.contains(&0) {
        return Err(LINUX_EINVAL);
    }
    String::from_utf8(bytes).map_err(|_| LINUX_EINVAL)
}

fn read_user_string_vector(
    vector_ptr: u64,
    max_count: usize,
    max_bytes: usize,
) -> Result<Vec<String>, i64> {
    let mut bytes = alloc::vec![0_u8; max_bytes];
    let mut bytes_len = 0_u32;
    let mut count = 0_u16;
    copy_string_vector(
        vector_ptr,
        max_count,
        &mut bytes,
        &mut bytes_len,
        &mut count,
    )?;

    let bytes_len = bytes_len as usize;
    let mut offset = 0usize;
    let mut values = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let Some(relative_end) = bytes[offset..bytes_len].iter().position(|byte| *byte == 0) else {
            return Err(LINUX_EINVAL);
        };
        let end = offset + relative_end;
        let value = core::str::from_utf8(&bytes[offset..end]).map_err(|_| LINUX_EINVAL)?;
        values.push(String::from(value));
        offset = end.checked_add(1).ok_or(LINUX_EINVAL)?;
    }
    if offset != bytes_len {
        return Err(LINUX_EINVAL);
    }
    Ok(values)
}

#[cfg(test)]
mod tests;
