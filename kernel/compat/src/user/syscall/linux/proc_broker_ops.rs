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
mod deferred_activation;
mod fork;
mod job_placement;
mod scheduling_context_grants;

pub(super) use activation_batch::syscall_linux_rustos_proc_activate_batch_broker;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use authority::{current_loader_process_id, prepare_owned_by};
use core::sync::atomic::{AtomicU64, Ordering};
use heapless::index_map::FnvIndexMap;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use x86_64::structures::paging::PageTableFlags;
use x86_64::VirtAddr;

use crate::user::handles::KernelHandle;
use crate::user::memfd::MemfdHandle;
use rustos_user_abi::syscall::{ServiceCapability,
    loader_service_role_allows_operation, RustosProcAbortBrokerArgs, RustosProcActivateBrokerArgs,
    RustosProcAuthorizeExecBrokerArgs, RustosProcCancelExecBrokerArgs, RustosProcCommitBrokerArgs,
    RustosProcExecTargetBrokerArgs, RustosProcForkBrokerArgs, RustosProcMapDataBrokerArgs,
    RustosProcMapFileBatchBrokerArgs, RustosProcMapFileBrokerArgs, RustosProcMapZeroedBrokerArgs,
    RustosProcPrepareBrokerArgs, RustosProcSetLinuxRuntimeBrokerArgs,
    RustosProcSetWindowsRuntimeBrokerArgs, RustosProcSignalQueueBrokerArgs,
    RustosProcValidateDeferredSpawnBrokerArgs, RustosUserRegisters, IPC_SERVICE_INITD,
    IPC_SERVICE_PROCD, IPC_SERVICE_ROOTD, IPC_SERVICE_SESSIOND, LOADER_SPAWN_ARG_BYTES,
    LOADER_SPAWN_ENV_BYTES, LOADER_SPAWN_FLAG_DEFER_START, LOADER_SPAWN_FLAG_IMMEDIATE_HANDOFF,
    LOADER_SPAWN_MAX_ARG_COUNT, LOADER_SPAWN_MAX_ENV_COUNT, PROC_BROKER_ABI_VERSION,
    PROC_BROKER_BATCH_CAPACITY, PROC_BROKER_FORMAT_ELF64, PROC_BROKER_FORMAT_PE64,
    PROC_BROKER_LINUX_INTERP_PATH_CAPACITY, PROC_BROKER_MAP_EXEC, PROC_BROKER_MAP_PRIVATE,
    PROC_BROKER_MAP_READ, PROC_BROKER_MAP_WRITE, PROC_BROKER_PREPARE_FLAG_EXEC_TICKET,
    PROC_BROKER_USER_SPACE_BASE, PROC_BROKER_USER_SPACE_END_EXCLUSIVE,
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
    stack_top: u64,
    exec_ticket: Option<u64>,
    /// Exact identity resolved from CREATE_PROCESS authority at prepare time.
    job: Option<multitask::JobId>,
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
#[derive(Debug, Eq, PartialEq)]
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

/// Shared typed descriptor admission for rootd's pre-loader bootstrap and the
/// ordinary loader prepare transaction. Neither path can mint job authority.
pub(super) fn resolve_spawn_job_descriptor(owner: u64, descriptor: u64) -> Result<multitask::JobId, i64> {
    job_placement::resolve_descriptor(owner, descriptor)
}

fn current_loader_and_args<T: Copy + Default>(args_ptr: u64) -> LinuxResult<(u64, T)> {
    let loader_pid = current_loader_process_id().ok_or(LINUX_EPERM)?;
    let args = read_user_struct_as(args_ptr)?;
    Ok((loader_pid, args))
}

fn with_owned_prepare_mut<R>(
    prepare_handle: u64,
    loader_pid: u64,
    f: impl FnOnce(&mut ProcPrepareState) -> LinuxResult<R>,
) -> LinuxResult<R> {
    let mut prepares = PROC_PREPARES.lock();
    let state = prepares.get_mut(&prepare_handle).ok_or(LINUX_EINVAL)?;
    if !prepare_owned_by(state, loader_pid) {
        return Err(LINUX_EPERM);
    }
    f(state)
}

fn push_prepare_mappings(
    state: &mut ProcPrepareState,
    mappings: &mut [Option<Box<MappingEntry>>],
) -> LinuxResult<()> {
    if state
        .mappings
        .len()
        .checked_add(mappings.len())
        .is_none_or(|total| total > MAX_MAPPINGS_PER_PREPARE)
    {
        return Err(LINUX_EINVAL);
    }
    for mapping in mappings {
        state
            .mappings
            .push(mapping.take().expect("validated broker batch entry"));
    }
    Ok(())
}

fn push_prepare_mapping(
    state: &mut ProcPrepareState,
    mapping: &mut Option<Box<MappingEntry>>,
) -> LinuxResult<()> {
    push_prepare_mappings(state, core::slice::from_mut(mapping))
}

fn build_file_mapping_entry(
    fd: u64,
    file_offset: u64,
    target_addr: u64,
    file_len: u64,
    mem_len: u64,
    flags: u64,
    reserved0: u64,
) -> LinuxResult<Box<MappingEntry>> {
    if reserved0 != 0 {
        return Err(LINUX_EINVAL);
    }
    validate_mapping_region(target_addr, mem_len, flags)?;
    validate_file_mapping_len(mem_len, file_len)?;
    let backing = pinned_file_backing_from_current(fd)?;
    Ok(Box::new(MappingEntry::File {
        backing,
        file_offset,
        file_len,
        target_addr,
        mem_len,
        flags,
    }))
}

pub(super) fn syscall_linux_rustos_proc_prepare_broker(args_ptr: u64) -> u64 {
    linux_result((|| {
        let (loader_pid, args): (u64, RustosProcPrepareBrokerArgs) =
            current_loader_and_args(args_ptr)?;
        if !job_placement::prepare_shape_valid(&args) {
            return Err(LINUX_EINVAL);
        }
        let job = job_placement::resolve_prepare_job(loader_pid, &args)?;
        let exec_ticket =
            (args.flags & PROC_BROKER_PREPARE_FLAG_EXEC_TICKET != 0).then_some(args.reserved0);
        if let Some(ticket) = exec_ticket
            && !EXEC_TICKETS.lock().contains_key(&ticket)
        {
            return Err(LINUX_EPERM);
        }
        // The PROCESS_LOADER capability is the prepare authority. Loaderd already
        // completed image-format admission in ring3; a loaderd -> procd policy
        // call here can close a synchronous procd -> loaderd -> procd cycle when
        // an exec races an ordinary spawn.
        let owner_pid = loader_pid;
        {
            let prepares = PROC_PREPARES.lock();
            proc_prepare_publication_status(
                multitask::is_user_process_exiting(owner_pid),
                prepares.len(),
            )?;
        }
        // Allocate all pointer slots before publication. Mapping payloads are
        // individually allocated before taking the registry lock, so subsequent
        // append operations cannot enter the allocator while holding it.
        let mut mappings = Vec::new();
        mappings
            .try_reserve_exact(MAX_MAPPINGS_PER_PREPARE)
            .map_err(|_| LINUX_ENOMEM)?;
        let state = ProcPrepareState {
            owner_pid,
            format: args.format,
            stack_top: args.stack_top,
            exec_ticket,
            job,
            mappings,
            windows_runtime: None,
            linux_runtime: None,
        };
        let mut prepares = PROC_PREPARES.lock();
        proc_prepare_publication_status(
            multitask::is_user_process_exiting(owner_pid),
            prepares.len(),
        )?;
        let handle = allocate_prepare_handle(&prepares).ok_or(LINUX_EAGAIN)?;
        match prepares.insert(handle, state) {
            Ok(None) => {}
            Ok(Some(replaced)) => {
                drop(prepares);
                drop(replaced);
                return Err(LINUX_EAGAIN);
            }
            Err((_handle, rejected)) => {
                drop(prepares);
                drop(rejected);
                return Err(LINUX_EAGAIN);
            }
        }
        drop(prepares);
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "proc-prepare-published",
            handle,
            u64::from(args.format),
        );
        Ok(handle)
    })())
}

pub(super) fn syscall_linux_rustos_proc_map_file_broker(args_ptr: u64) -> u64 {
    linux_unit_result((|| {
        let (loader_pid, args): (u64, RustosProcMapFileBrokerArgs) =
            current_loader_and_args(args_ptr)?;
        let mut mapping = Some(build_file_mapping_entry(
            args.fd,
            args.file_offset,
            args.target_addr,
            args.file_len,
            args.mem_len,
            args.flags,
            args.reserved0,
        )?);
        with_owned_prepare_mut(args.prepare_handle, loader_pid, |state| {
            push_prepare_mapping(state, &mut mapping)
        })
    })())
}

pub(super) fn syscall_linux_rustos_proc_map_file_batch_broker(args_ptr: u64) -> u64 {
    linux_unit_result((|| {
        let Some(current_pid) = current_loader_process_id() else {
            nucleus_core::debug::println_serialized(format_args!(
                "proc-map-file-batch denied stage=capability pid={:?} preempt_depth={}",
                multitask::current_user_process_id(),
                nucleus_core::util::lockdep::preemption_depth()
            ));
            return Err(LINUX_EPERM);
        };
        let args: RustosProcMapFileBatchBrokerArgs = read_user_struct_as(args_ptr)?;
        let count = args.count as usize;
        if args.reserved0 != 0 || count == 0 || count > PROC_BROKER_BATCH_CAPACITY {
            return Err(LINUX_EINVAL);
        }
        // Resolve and allocate the entire batch before locking the registry.
        let mut mappings: [Option<Box<MappingEntry>>; PROC_BROKER_BATCH_CAPACITY] =
            [const { None }; PROC_BROKER_BATCH_CAPACITY];
        for (slot, entry) in mappings[..count].iter_mut().zip(&args.entries[..count]) {
            *slot = Some(build_file_mapping_entry(
                entry.fd,
                entry.file_offset,
                entry.target_addr,
                entry.file_len,
                entry.mem_len,
                entry.flags,
                entry.reserved0,
            )?);
        }
        with_owned_prepare_mut(args.prepare_handle, current_pid, |state| {
            push_prepare_mappings(state, &mut mappings[..count])
        })
        .inspect_err(|&errno| {
            if errno == LINUX_EPERM {
                nucleus_core::debug::println_serialized(format_args!(
                    "proc-map-file-batch denied stage=owner handle={} current_pid={}",
                    args.prepare_handle, current_pid
                ));
            }
        })?;
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "proc-map-file-batch",
            args.prepare_handle,
            count as u64,
        );
        Ok(())
    })())
}

pub(super) fn syscall_linux_rustos_proc_set_linux_runtime_broker(args_ptr: u64) -> u64 {
    linux_unit_result((|| {
        let (loader_pid, args): (u64, RustosProcSetLinuxRuntimeBrokerArgs) =
            current_loader_and_args(args_ptr)?;
        if args.abi_version != PROC_BROKER_ABI_VERSION || args.reserved0 != 0 {
            return Err(LINUX_EINVAL);
        }
        let interp_path_len = args.interp_path_len as usize;
        if interp_path_len > PROC_BROKER_LINUX_INTERP_PATH_CAPACITY {
            return Err(LINUX_EINVAL);
        }
        // `actual_entry` becomes the first user RIP and the TLS fields become
        // `VirtAddr::new` operands during commit, which panics on a non-canonical
        // address. Reject anything outside the loader's user window here.
        if !user_span_in_window(args.actual_entry, 1) || !linux_tls_fields_valid(&args) {
            return Err(LINUX_EINVAL);
        }
        let initial_tls = (args.has_tls != 0).then_some(crate::user::linux::LinuxInitialTlsInfo {
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
        });
        let interpreter_path = if interp_path_len > 0 {
            Some(String::from(
                core::str::from_utf8(&args.interp_path[..interp_path_len])
                    .map_err(|_| LINUX_EINVAL)?,
            ))
        } else {
            None
        };
        let mut info = Some(crate::user::linux::LinuxProcessImageInfo {
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
        });
        with_owned_prepare_mut(args.prepare_handle, loader_pid, |state| {
            if state.format != PROC_BROKER_FORMAT_ELF64 || state.linux_runtime.is_some() {
                return Err(LINUX_EINVAL);
            }
            state.linux_runtime = Some((info.take().expect("validated linux runtime"), args.actual_entry));
            Ok(())
        })?;
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "proc-linux-runtime-published",
            args.prepare_handle,
            args.phnum,
        );
        Ok(())
    })())
}

pub(super) fn syscall_linux_rustos_proc_map_zeroed_broker(args_ptr: u64) -> u64 {
    linux_unit_result((|| {
        let (loader_pid, args): (u64, RustosProcMapZeroedBrokerArgs) =
            current_loader_and_args(args_ptr)?;
        if args.reserved0 != 0 {
            return Err(LINUX_EINVAL);
        }
        validate_mapping_region(args.target_addr, args.mem_len, args.flags)?;
        let mut mapping = Some(Box::new(MappingEntry::Zeroed {
            target_addr: args.target_addr,
            mem_len: args.mem_len,
            flags: args.flags,
        }));
        with_owned_prepare_mut(args.prepare_handle, loader_pid, |state| {
            push_prepare_mapping(state, &mut mapping)
        })
    })())
}

pub(super) fn syscall_linux_rustos_proc_map_data_broker(args_ptr: u64) -> u64 {
    linux_unit_result((|| {
        let (loader_pid, args): (u64, RustosProcMapDataBrokerArgs) =
            current_loader_and_args(args_ptr)?;
        let data_len = args.data_len as usize;
        if args.reserved0 != 0
            || data_len > args.data.len()
            || args
                .data_offset
                .checked_add(u64::from(args.data_len))
                .is_none_or(|end| end > args.mem_len)
        {
            return Err(LINUX_EINVAL);
        }
        validate_mapping_region(args.target_addr, args.mem_len, args.flags)?;
        let mut data = Vec::new();
        data.try_reserve_exact(data_len).map_err(|_| LINUX_ENOMEM)?;
        data.extend_from_slice(&args.data[..data_len]);
        let mut mapping = Some(Box::new(MappingEntry::Data {
            target_addr: args.target_addr,
            mem_len: args.mem_len,
            flags: args.flags,
            data_offset: args.data_offset,
            data,
        }));
        with_owned_prepare_mut(args.prepare_handle, loader_pid, |state| {
            push_prepare_mapping(state, &mut mapping)
        })
    })())
}

fn validate_windows_runtime_args(args: &RustosProcSetWindowsRuntimeBrokerArgs) -> LinuxResult<()> {
    if args.abi_version != PROC_BROKER_ABI_VERSION
        || args.reserved0 != 0
        || args.reserved1 != 0
        || args.reserved2 != 0
        || args.loader_module_count == 0
    {
        return Err(LINUX_EINVAL);
    }
    let runtime_base = args.runtime_base;
    let runtime_size = args.runtime_size;
    validate_user_range(runtime_base, runtime_size)?;
    validate_user_range(args.image_base, args.image_size)?;
    if !range_contains(args.image_base, args.image_size, args.entry_point, 1)
        || (args.bootstrap_entry_point != 0
            && !user_span_in_window(args.bootstrap_entry_point, 1))
        || (args.kuser_shared_data_address != 0
            && !range_contains(runtime_base, runtime_size, args.kuser_shared_data_address, 4096))
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
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

pub(super) fn syscall_linux_rustos_proc_set_windows_runtime_broker(args_ptr: u64) -> u64 {
    linux_unit_result((|| {
        let (loader_pid, args): (u64, RustosProcSetWindowsRuntimeBrokerArgs) =
            current_loader_and_args(args_ptr)?;
        validate_windows_runtime_args(&args)?;
        let allocation_base_hint = args
            .runtime_base
            .checked_add(args.runtime_size)
            .ok_or(LINUX_EINVAL)?;
        with_owned_prepare_mut(args.prepare_handle, loader_pid, |state| {
            if state.format != PROC_BROKER_FORMAT_PE64 || state.windows_runtime.is_some() {
                return Err(LINUX_EINVAL);
            }
            state.windows_runtime = Some(crate::user::process::WindowsProcessLoaderRuntime {
                entry_point: args.entry_point,
                runtime: crate::user::process_state::WindowsProcessRuntimeState {
                    image_base: args.image_base,
                    image_size: args.image_size,
                    allocation_base_hint,
                    public_runtime_address: args.public_runtime_address,
                    peb_address: args.peb_address,
                    teb_address: args.teb_address,
                    process_parameters_address: args.process_parameters_address,
                    loader_data_address: args.loader_data_address,
                    loader_module_array_address: args.loader_module_array_address,
                    loader_module_count: args.loader_module_count,
                    loader_reserved: 0,
                    main_module_entry_address: args.main_module_entry_address,
                    bootstrap_entry_point: args.bootstrap_entry_point,
                    kuser_shared_data_address: args.kuser_shared_data_address,
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
            Ok(())
        })
    })())
}

struct ExecVectors {
    exec_path: String,
    argv: Vec<String>,
    env: Vec<String>,
}

fn read_exec_vectors(
    exec_path_ptr: u64,
    exec_path_len: u64,
    argv_ptr: u64,
    envp_ptr: u64,
) -> LinuxResult<ExecVectors> {
    let exec_path = read_user_text(exec_path_ptr, exec_path_len)?;
    let argv = read_user_string_vector(
        argv_ptr,
        LOADER_SPAWN_MAX_ARG_COUNT,
        LOADER_SPAWN_ARG_BYTES,
    )?;
    let env = read_user_string_vector(
        envp_ptr,
        LOADER_SPAWN_MAX_ENV_COUNT,
        LOADER_SPAWN_ENV_BYTES,
    )?;
    Ok(ExecVectors {
        exec_path,
        argv,
        env,
    })
}

fn prepare_linux_process_from_state(
    mappings: &[Box<MappingEntry>],
    linux_runtime: Option<(crate::user::linux::LinuxProcessImageInfo, u64)>,
    stack_top: u64,
    address_space: crate::memory::paging::ProcessAddressSpace,
    launch: crate::user::process::ProcessLaunchOptions<'_>,
) -> LinuxResult<crate::user::process::PreparedProcessImage> {
    // loaderd always populates `linux_runtime` before issuing commit/exec;
    // the bytes-image fallback that used to call into ring0 ELF parsing
    // was retired with the PE bytes path on 2026-05-20.
    let (mut info, actual_entry) = linux_runtime.ok_or(LINUX_EINVAL)?;
    info.image_mappings = initial_elf_mappings(mappings, &info, actual_entry);
    crate::user::process::prepare_linux_process_with_metadata(
        info,
        actual_entry,
        address_space,
        launch,
        stack_top,
    )
    .map_err(process_load_error_to_linux_errno)
}

pub(super) fn syscall_linux_rustos_proc_commit_broker(args_ptr: u64) -> u64 {
    linux_result((|| {
        let (loader_pid, args): (u64, RustosProcCommitBrokerArgs) =
            current_loader_and_args(args_ptr)?;
        const KNOWN_SPAWN_FLAGS: u64 = SPAWN_FLAG_LOGICAL_ADMIN
            | LOADER_SPAWN_FLAG_IMMEDIATE_HANDOFF as u64
            | LOADER_SPAWN_FLAG_DEFER_START as u64;
        if args.requester_pid == 0
            || args.prepare_handle == 0
            || args.flags & !KNOWN_SPAWN_FLAGS != 0
            || args.flags & LOADER_SPAWN_FLAG_IMMEDIATE_HANDOFF as u64 != 0
                && args.flags & LOADER_SPAWN_FLAG_DEFER_START as u64 != 0
        {
            return Err(LINUX_EINVAL);
        }
        if multitask::is_user_process_exiting(args.requester_pid) {
            return Err(LINUX_ESRCH);
        }
        if !requester_owns_live_spawn_role(args.requester_pid) {
            return Err(LINUX_EPERM);
        }
        let state = {
            let mut prepares = PROC_PREPARES.lock();
            match prepares.get(&args.prepare_handle) {
                None => return Err(LINUX_EINVAL),
                Some(s) if !prepare_owned_by(s, loader_pid) => return Err(LINUX_EPERM),
                Some(s) if s.exec_ticket.is_some() => return Err(LINUX_EPERM),
                _ => {}
            }
            prepares.remove(&args.prepare_handle).unwrap()
        };
        if !matches!(
            (
                state.format,
                state.linux_runtime.is_some(),
                state.windows_runtime.is_some(),
            ),
            (PROC_BROKER_FORMAT_ELF64, true, false) | (PROC_BROKER_FORMAT_PE64, false, true)
        ) {
            return Err(LINUX_EINVAL);
        }
        let vectors = read_exec_vectors(
            args.exec_path_ptr,
            args.exec_path_len,
            args.argv_ptr,
            args.envp_ptr,
        )?;
        let exec_path = vectors.exec_path;
        let scheduling_policy = scheduling_context_grants::consume(
            args.scheduling_context,
            args.requester_pid,
            exec_path.as_str(),
        )?;
        let qualification_required =
            super::smp_qualification_ops::smp_qualification_exec_path_matches(exec_path.as_str());
        let argv_refs = vectors.argv.iter().map(String::as_str).collect::<Vec<_>>();
        let env_refs = vectors.env.iter().map(String::as_str).collect::<Vec<_>>();
        let linux_launch = crate::user::linux::LinuxProcessLaunch {
            exec_path: &exec_path,
            argv: argv_refs.as_slice(),
            env: env_refs.as_slice(),
        };
        let job = job_placement::spawn_job(state.job, args.requester_pid)?;
        let spawn_transaction =
            crate::user::process::reserve_process_spawn_transaction_in(Some(job))
                .map_err(process_load_error_to_linux_errno)?;
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "proc-commit-address-space-begin",
            args.prepare_handle,
            state.mappings.len() as u64,
        );
        let address_space = address_space_from_mappings(&state.mappings)?;
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "proc-commit-address-space-done",
            args.prepare_handle,
            state.mappings.len() as u64,
        );
        let session = if args.console_session == 0 {
            multitask::current_user_snapshot()
                .ok_or(LINUX_EINVAL)?
                .console_session()
        } else {
            crate::io::session::ConsoleSessionHandle::from_raw(args.console_session)
        };
        let logical_admin = args.flags & SPAWN_FLAG_LOGICAL_ADMIN != 0;
        let launch = crate::user::process::ProcessLaunchOptions {
            registers: crate::user::process::ProcessStartRegisters::new(),
            linux: linux_launch,
            console_session: session,
            logical_admin,
            root_job_authority: false,
        };
        let prepared = match state.format {
            PROC_BROKER_FORMAT_ELF64 => prepare_linux_process_from_state(
                &state.mappings,
                state.linux_runtime,
                state.stack_top,
                address_space,
                launch,
            )?,
            PROC_BROKER_FORMAT_PE64 => {
                let metadata = state.windows_runtime.ok_or(LINUX_EINVAL)?;
                crate::user::process::prepare_windows_process_with_address_space(
                    metadata,
                    address_space,
                    launch,
                    state.stack_top,
                )
                .map_err(process_load_error_to_linux_errno)?
            }
            _ => return Err(LINUX_EINVAL),
        };
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "proc-commit-prepare-done",
            args.prepare_handle,
            state.format as u64,
        );
        let prepared = crate::user::process::bind_prepared_spawn(prepared, spawn_transaction);
        let spawn_fn: fn(
            crate::user::process::PreparedSpawnImage,
            u64,
            rustos_user_abi::syscall::RustosSchedulingContextPolicy,
        ) -> Result<
            crate::user::process::SpawnedProcess,
            crate::user::process::ProcessLoadError,
        > = if args.flags & LOADER_SPAWN_FLAG_DEFER_START as u64 != 0 {
            crate::user::process::spawn_prepared_process_suspended_with_scheduling_context
        } else if args.flags & LOADER_SPAWN_FLAG_IMMEDIATE_HANDOFF as u64 != 0 {
            crate::user::process::spawn_prepared_process_with_scheduling_context
        } else {
            crate::user::process::spawn_prepared_process_for_loader_reply_with_scheduling_context
        };
        let spawned = spawn_fn(prepared, args.weight_micros, scheduling_policy)
            .map_err(process_load_error_to_linux_errno)?;
        if state.format == PROC_BROKER_FORMAT_ELF64 {
            // Best effort: the resident private image pages become ordinary
            // VMAs so a later `fork` shares them copy-on-write.
            super::mm_broker_ops::publish_exec_image_regions(
                spawned.pid,
                &exec_image_private_ranges(&state.mappings),
            );
        }
        if args.flags & LOADER_SPAWN_FLAG_DEFER_START as u64 != 0 {
            register_deferred_activation(
                args.requester_pid,
                spawned.pid,
                qualification_required,
            )?;
        }
        Ok(spawned.pid)
    })())
}

/// Records the one authority that may later start a process created suspended:
/// its creator (`requester_pid`) and the exact target incarnation. Both the
/// loader path and the rootd bootstrap spawn register here, and both start the
/// process through `activate_deferred`, so there is one place that checks
/// "only the creator, only this incarnation, only once". On any failure the
/// still-suspended target is terminated, never left parked.
pub(in crate::user::syscall::linux) fn register_deferred_activation(
    requester_pid: u64,
    target_pid: u64,
    qualification_required: bool,
) -> Result<(), i64> {
    let terminate = || {
        let _ = multitask::terminate_user_process(target_pid);
    };
    let Some(owner) = multitask::live_user_process_identity_by_pid(requester_pid) else {
        terminate();
        return Err(LINUX_ESRCH);
    };
    let Some(target) = multitask::live_user_process_identity_by_pid(target_pid) else {
        terminate();
        return Err(LINUX_ESRCH);
    };
    let authority = DeferredActivationAuthority { owner, target, qualification_required };
    let mut activations = DEFERRED_ACTIVATIONS.lock();
    if activations.len() >= MAX_DEFERRED_ACTIVATIONS || activations.contains_key(&target_pid) {
        drop(activations);
        terminate();
        return Err(LINUX_EAGAIN);
    }
    if activations.insert(target_pid, authority).is_err() {
        drop(activations);
        terminate();
        return Err(LINUX_EAGAIN);
    }
    drop(activations);
    // Close the requester-exit race on both sides of publication: cleanup
    // either observes the new entry, or this recheck consumes it and retires
    // the still-suspended target.
    if multitask::live_user_process_identity_by_pid(requester_pid) != Some(owner) {
        DEFERRED_ACTIVATIONS.lock().remove(&target_pid);
        terminate();
        return Err(LINUX_ESRCH);
    }
    Ok(())
}

pub(super) fn syscall_linux_rustos_proc_activate_broker(args_ptr: u64) -> u64 {
    if !current_process_can_load() {
        return linux_errno(LINUX_EPERM);
    }
    let args: RustosProcActivateBrokerArgs = match read_user_struct_as(args_ptr) {
        Ok(args) => args,
        Err(errno) => return linux_errno(errno),
    };
    if args.abi_version != PROC_BROKER_ABI_VERSION
        || args.reserved0 != 0
        || args.flags != 0
        || args.target_pid == 0
        || args.requester_pid == 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    match activate_deferred(args.requester_pid, args.target_pid) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}


/// Starts a process that was created suspended and registered with
/// `register_deferred_activation`. `requester_pid` must be the creator and
/// `target_pid` the exact registered incarnation; the authority is consumed
/// once, atomically with the runnable publication.
pub(in crate::user::syscall::linux) fn activate_deferred(
    requester_pid: u64,
    target_pid: u64,
) -> Result<(), i64> {
    // Requester policy identity is not loaderd's identity. These observations
    // do not replace the original physical Cohort/readers retained by PS.
    let (owner, target) = match resolve_deferred_activation_identities(
        requester_pid,
        target_pid,
        multitask::live_user_process_identity_by_pid,
    ) {
        Ok(identities) => identities,
        Err(errno) => return Err(errno),
    };
    let mut qualification_armed = false;
    let mut callback_seen = false;
    let mut failure = LINUX_ESRCH;
    let activated = multitask::activate_suspended_user_tasks_with_commit(
        core::slice::from_ref(&target_pid),
        || {
            // Bounded permission transfer AFTER PS retains the original caller
            // and targets. Cleanup competes at this SAME registry leaf, not in
            // an unprotected gap between copied preflight and consumption.
            callback_seen = true;
            let mut activations = DEFERRED_ACTIVATIONS.lock();
            if !deferred_spawn_authority_matches(&activations, target_pid, owner, target) {
                failure = LINUX_EPERM;
                return None;
            }
            let required = activations.get(&target_pid).unwrap().qualification_required;
            qualification_armed =
                match super::smp_qualification_ops::prepare_smp_qualification_activation(
                    owner, target, required,
                ) {
                    Ok(armed) => armed,
                    Err(errno) => {
                        failure = errno;
                        return None;
                    }
                };
            let grant = deferred_activation::take_exact::<1, _, MAX_DEFERRED_ACTIVATIONS>(
                &mut activations,
                core::slice::from_ref(&target_pid),
                |_, authority| {
                    deferred_activation_identities_match(
                        &authority.owner,
                        &authority.target,
                        &owner,
                        &target,
                    )
                },
            )
            .expect("validated single activation lost its original grant");
            drop(activations);
            Some(grant)
        },
    );
    if !activated {
        // Registry/qualification refusal did not consume any authority.
        if callback_seen {
            return Err(failure);
        }
        // No runnable effect preceded refusal. Remove only OUR still-pending
        // original record; cleanup or another accepted activation may have won.
        let removed = {
            let mut activations = DEFERRED_ACTIVATIONS.lock();
            let removed =
                if deferred_spawn_authority_matches(&activations, target_pid, owner, target) {
                    activations.remove(&target_pid)
                } else {
                    None
                };
            drop(activations);
            removed
        };
        if qualification_armed {
            super::smp_qualification_ops::abort_smp_qualification_activation(target);
        }
        if removed.is_some() {
            let _ = multitask::terminate_user_process(target_pid);
        }
        return Err(failure);
    }
    // Permission was moved once, not copied; the SAME grant and original PS
    // Cohort survived all runnable publication. No registry raw lock crossed PS.
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "proc-activate-committed",
        target_pid,
        requester_pid,
    );
    Ok(())
}

pub(super) fn syscall_linux_rustos_proc_validate_deferred_spawn_broker(args_ptr: u64) -> u64 {
    linux_unit_result((|| {
        let args: RustosProcValidateDeferredSpawnBrokerArgs =
            require_capability_and_read_args_as(ServiceCapability::RootSupervisor, args_ptr)?;
        if args.abi_version != PROC_BROKER_ABI_VERSION
            || args.reserved0 != 0
            || args.flags != 0
            || args.target_pid == 0
            || args.requester_pid == 0
        {
            return Err(LINUX_EINVAL);
        }
        let owner =
            multitask::live_user_process_identity_by_pid(args.requester_pid).ok_or(LINUX_ESRCH)?;
        let target =
            multitask::live_user_process_identity_by_pid(args.target_pid).ok_or(LINUX_ESRCH)?;
        if !deferred_spawn_authority_matches(
            &DEFERRED_ACTIVATIONS.lock(),
            args.target_pid,
            owner,
            target,
        ) {
            return Err(LINUX_EPERM);
        }
        Ok(())
    })())
}

pub(super) fn syscall_linux_rustos_proc_authorize_exec_broker(args_ptr: u64) -> u64 {
    linux_result((|| {
        if !current_process_can_policy() {
            return Err(LINUX_EPERM);
        }
        let args: RustosProcAuthorizeExecBrokerArgs = read_user_struct_as(args_ptr)?;
        if args.abi_version != PROC_BROKER_ABI_VERSION
            || args.reserved0 != 0
            || args.reserved1 != 0
            || args.target_pid == 0
            || args.target_tid == 0
        {
            return Err(LINUX_EINVAL);
        }
        if multitask::linux_thread_snapshot_by_ids(args.target_pid, args.target_tid).is_none()
            || multitask::is_user_process_exiting(args.target_pid)
        {
            return Err(LINUX_ESRCH);
        }
        let ticket = {
            let mut tickets = EXEC_TICKETS.lock();
            if tickets.len() >= MAX_EXEC_TICKETS {
                return Err(LINUX_EAGAIN);
            }
            let ticket = allocate_exec_ticket(&tickets).ok_or(LINUX_EAGAIN)?;
            tickets
                .insert(
                    ticket,
                    ExecTicketState {
                        target_pid: args.target_pid,
                        target_tid: args.target_tid,
                    },
                )
                .map_err(|_| LINUX_EAGAIN)?;
            ticket
        };
        // Process teardown runs independently of procd authorization. Recheck
        // after publication so an exit between the first snapshot and insertion
        // cannot retain one of the bounded ticket slots.
        if multitask::linux_thread_snapshot_by_ids(args.target_pid, args.target_tid).is_none()
            || multitask::is_user_process_exiting(args.target_pid)
        {
            EXEC_TICKETS.lock().remove(&ticket);
            return Err(LINUX_ESRCH);
        }
        Ok(ticket)
    })())
}

pub(super) fn syscall_linux_rustos_proc_cancel_exec_broker(args_ptr: u64) -> u64 {
    linux_unit_result((|| {
        if !current_process_can_policy() {
            return Err(LINUX_EPERM);
        }
        let args: RustosProcCancelExecBrokerArgs = read_user_struct_as(args_ptr)?;
        if args.abi_version != PROC_BROKER_ABI_VERSION
            || args.reserved0 != 0
            || args.reserved1 != 0
            || args.exec_ticket == 0
            || args.target_pid == 0
            || args.target_tid == 0
        {
            return Err(LINUX_EINVAL);
        }
        let mut tickets = EXEC_TICKETS.lock();
        let ticket = tickets.get(&args.exec_ticket).copied().ok_or(LINUX_EINVAL)?;
        if ticket.target_pid != args.target_pid || ticket.target_tid != args.target_tid {
            return Err(LINUX_EPERM);
        }
        tickets.remove(&args.exec_ticket);
        Ok(())
    })())
}

pub(super) fn syscall_linux_rustos_proc_exec_target_broker(args_ptr: u64) -> u64 {
    linux_result((|| {
        let (loader_pid, args): (u64, RustosProcExecTargetBrokerArgs) =
            current_loader_and_args(args_ptr)?;
        if args.abi_version != PROC_BROKER_ABI_VERSION
            || args.reserved0 != 0
            || args.requester_pid == 0
            || args.target_pid == 0
            || args.target_tid == 0
            || args.exec_ticket == 0
            || args.prepare_handle == 0
        {
            return Err(LINUX_EINVAL);
        }
        if multitask::is_user_process_exiting(args.requester_pid) {
            return Err(LINUX_ESRCH);
        }
        if !ipc_ops::process_owns_live_service_endpoint(args.requester_pid, IPC_SERVICE_PROCD) {
            return Err(LINUX_EPERM);
        }
        {
            let mut tickets = EXEC_TICKETS.lock();
            let ticket = tickets.get(&args.exec_ticket).copied().ok_or(LINUX_EINVAL)?;
            if ticket.target_pid != args.target_pid || ticket.target_tid != args.target_tid {
                return Err(LINUX_EPERM);
            }
            tickets.remove(&args.exec_ticket);
        }
        let state = {
            let mut prepares = PROC_PREPARES.lock();
            match prepares.get(&args.prepare_handle) {
                None => return Err(LINUX_EINVAL),
                Some(s) if !prepare_owned_by(s, loader_pid) => return Err(LINUX_EPERM),
                _ => {}
            }
            prepares.remove(&args.prepare_handle).unwrap()
        };
        if !exec_prepare_ticket_matches(state.exec_ticket, args.exec_ticket)
            || state.format != PROC_BROKER_FORMAT_ELF64
            || state.windows_runtime.is_some()
            || state.linux_runtime.is_none()
        {
            return Err(LINUX_EINVAL);
        }
        let vectors = read_exec_vectors(
            args.exec_path_ptr,
            args.exec_path_len,
            args.argv_ptr,
            args.envp_ptr,
        )?;
        let exec_path = vectors.exec_path;
        let argv_refs = vectors.argv.iter().map(String::as_str).collect::<Vec<_>>();
        let env_refs = vectors.env.iter().map(String::as_str).collect::<Vec<_>>();
        let linux_launch = crate::user::linux::LinuxProcessLaunch {
            exec_path: &exec_path,
            argv: argv_refs.as_slice(),
            env: env_refs.as_slice(),
        };
        #[cfg(rustos_lifecycle_trace)]
        let exec_started = crate::arch::clock::monotonic_nanos();
        let address_space = address_space_from_mappings(&state.mappings)?;
        #[cfg(rustos_lifecycle_trace)]
        let mapped_at = crate::arch::clock::monotonic_nanos();
        let session = if args.console_session == 0 {
            multitask::linux_thread_snapshot_by_ids(args.target_pid, args.target_tid)
                .ok_or(LINUX_EINVAL)?
                .console_session
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
            root_job_authority: false,
        };
        let prepared = prepare_linux_process_from_state(
            &state.mappings,
            state.linux_runtime,
            state.stack_top,
            address_space,
            launch,
        )?;
        #[cfg(rustos_lifecycle_trace)]
        let prepared_at = crate::arch::clock::monotonic_nanos();
        let transition = exec_transition_from_prepared(args.target_pid, args.target_tid, &prepared);
        {
            let mut transitions = EXEC_TRANSITIONS.lock();
            // Publishing the transition before replacing the target address space
            // closes the scheduler-visible window where a new image has no saved
            // user-register handoff. A second concurrent exec for the same thread
            // must not overwrite that handoff.
            if transitions.contains_key(&args.target_tid) {
                return Err(LINUX_EBUSY);
            }
            if transitions.insert(args.target_tid, transition).is_err() {
                return Err(LINUX_EAGAIN);
            }
        }
        let exec_outcome = multitask::exec_user_process_by_pid_checked(
            args.target_pid,
            args.target_tid,
            prepared.address_space,
            prepared.bootstrap,
        );
        #[cfg(rustos_lifecycle_trace)]
        {
            // No prepare/transition/process guard spans diagnostic output. Capture
            // all durations first so console transport is not charged to a phase.
            let committed_at = crate::arch::clock::monotonic_nanos();
            nucleus_core::debug::println_serialized(format_args!(
                "proc-exec-work pid={} map_ns={} prepare_ns={} commit_ns={} ok={}",
                args.target_pid,
                mapped_at.saturating_sub(exec_started),
                prepared_at.saturating_sub(mapped_at),
                committed_at.saturating_sub(prepared_at),
                exec_outcome.is_ok(),
            ));
        }
        match exec_outcome {
            Ok(closed_handles) => {
                // The exec'd thread cannot resume until this call returns, so
                // the new image's VMAs exist before it can touch or fork them.
                if state.format == PROC_BROKER_FORMAT_ELF64 {
                    super::mm_broker_ops::publish_exec_image_regions(
                        args.target_pid,
                        &exec_image_private_ranges(&state.mappings),
                    );
                }
                offload_ops::record_process_exec(args.target_pid);
                // The process-table commit has already removed CLOEXEC descriptors.
                // Derive cleanup from the exact handles retired by that commit, never
                // from an earlier fd snapshot that a sibling can close/reuse before
                // replacement. A provider restart independently revokes any call that
                // cannot complete this bounded cleanup.
                // POSIX locks die with a close of any fd of the file, including this
                // one; the descriptions are still referenced here, so vfsd still knows
                // them. Runs in the loader's context, hence the explicit pid.
                file_lock_ops::note_exec_cloexec_closed(args.target_pid, &closed_handles);
                release_closed_handles(closed_handles);
                // Linux exec retires every sibling thread. Their exact tickets and
                // handoffs can no longer become valid after the scheduler clears those
                // slots, so release them with the same target identity boundary.
                cleanup_proc_broker_exec_state_for_siblings(args.target_pid, args.target_tid);
                Ok(args.target_pid)
            }
            Err(refusal) => {
                // Names the refusing stage without collapsing quota into a missing target.
                nucleus_core::debug::record_milestone(
                    nucleus_core::debug::LogCategory::Compat,
                    "proc-exec-refused",
                    args.target_pid,
                    refusal.code(),
                );
                EXEC_TRANSITIONS.lock().remove(&args.target_tid);
                Err(exec_refusal_errno(refusal))
            }
        }
    })())
}

fn exec_refusal_errno(refusal: multitask::ExecRefusal) -> i64 {
    match refusal {
        multitask::ExecRefusal::MappingQuota => LINUX_ENOMEM,
        _ => LINUX_ESRCH,
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

#[cfg(test)]
fn valid_process_fork_plan_locally(args: &RustosProcForkBrokerArgs) -> bool {
    fork::valid_process_fork_plan_locally(args)
}

pub(super) fn syscall_linux_rustos_proc_signal_queue_broker(args_ptr: u64) -> u64 {
    linux_unit_result((|| {
        if !current_process_can_policy() {
            return Err(LINUX_EPERM);
        }
        let args: RustosProcSignalQueueBrokerArgs = read_user_struct_as(args_ptr)?;
        if args.abi_version != PROC_BROKER_ABI_VERSION
            || args.reserved0 != 0
            || args.signal > crate::user::linux::MAX_SIGNAL_NUMBER as u32
            || args.target_pid == 0
        {
            return Err(LINUX_EINVAL);
        }
        // `target_tid == 0` is process-directed (`kill`): any live member of the
        // thread group, so a retired leader does not hide its siblings.
        let queued = if args.target_tid == 0 {
            multitask::queue_linux_process_signal(args.target_pid, args.signal as u64)
        } else {
            multitask::queue_linux_signal(args.target_pid, args.target_tid, args.signal as u64)
        };
        // SIGKILL cannot wait for a recipient syscall boundary: a thread spinning
        // in user mode never reaches signal delivery. A process-directed SIGKILL
        // from another process ends the whole group through the same terminal path
        // rootd terminate uses, which also publishes the SIGKILL wait status.
        if matches!(queued, Ok(true))
            && args.target_tid == 0
            && args.signal == linux_abi::SIGKILL as u32
            && args.sender_pid != args.target_pid
        {
            terminate_process_once_and_finalize(args.target_pid);
            return Ok(());
        }
        match queued {
            Ok(true) => Ok(()),
            Ok(false) => {
                if let Some(target_abi) =
                    multitask::user_target_abi_if_alive(args.target_pid, args.target_tid)
                {
                    if args.signal == 0 {
                        return Ok(());
                    }
                    if target_abi == multitask::UserAbi::Windows
                        && args.sender_pid != args.target_pid
                        && args.signal == linux_abi::SIGKILL as u32
                    {
                        terminate_process_once_and_finalize(args.target_pid);
                        return Ok(());
                    }
                }
                Err(LINUX_ESRCH)
            }
            Err(()) => Err(LINUX_EAGAIN),
        }
    })())
}

pub(super) fn syscall_linux_rustos_proc_abort_broker(args_ptr: u64) -> u64 {
    linux_unit_result((|| {
        let (loader_pid, args): (u64, RustosProcAbortBrokerArgs) =
            current_loader_and_args(args_ptr)?;
        if args.reserved0 != 0 {
            return Err(LINUX_EINVAL);
        }
        let mut prepares = PROC_PREPARES.lock();
        if let Some(state) = prepares.get(&args.prepare_handle)
            && !prepare_owned_by(state, loader_pid)
        {
            return Err(LINUX_EPERM);
        }
        let removed = prepares.remove(&args.prepare_handle);
        drop(prepares);
        drop(removed);
        Ok(())
    })())
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
        drop(activations);
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
    ipc_ops::current_process_has_service_capability(ServiceCapability::ProcessLoader)
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
    ipc_ops::current_process_has_service_capability(ServiceCapability::ProcessPolicy)
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

/// Describe only the mappings actually installed from the loader's admitted
/// plan. Tie the ELF names to the pinned backing object of each entry point,
/// not a VA heuristic (a fixed ET_EXEC can be linked above the interpreter).
fn initial_elf_mappings(
    mappings: &[Box<MappingEntry>],
    info: &crate::user::linux::LinuxProcessImageInfo,
    actual_entry: u64,
) -> Vec<crate::user::linux::LinuxImageMapping> {
    use crate::user::linux::{LinuxImageMapping, LinuxImageMappingPathKind as Kind, LinuxVmaFlags};

    let file_at = |address| {
        mappings.iter().find_map(|mapping| match mapping.as_ref() {
            MappingEntry::File { backing, target_addr, file_len, .. }
                if address >= *target_addr && address - *target_addr < *file_len =>
            {
                Some(backing.object_id())
            }
            _ => None,
        })
    };
    let executable = file_at(info.entry).or_else(|| file_at(info.program_headers));
    let interpreter = (info.interpreter_path.is_some() && info.interpreter_base != 0)
        .then(|| file_at(actual_entry))
        .flatten();
    mappings.iter().flat_map(|mapping| {
        let (start, len, flags, file) = match mapping.as_ref() {
            MappingEntry::File { backing, file_offset, file_len, target_addr, mem_len, flags } => {
                let id = Some(backing.object_id());
                let kind = if id == executable {
                    Kind::Executable
                } else if id == interpreter {
                    Kind::Interpreter
                } else {
                    Kind::None
                };
                let backed = (file_len.div_ceil(PAGE_SIZE) * PAGE_SIZE).min(*mem_len);
                (*target_addr, *mem_len, *flags, Some((backed, *file_offset, kind)))
            }
            MappingEntry::Zeroed { target_addr, mem_len, flags }
            | MappingEntry::Data { target_addr, mem_len, flags, .. } =>
                (*target_addr, *mem_len, *flags, None),
        };
        let flags = LinuxVmaFlags::private_anon(
            flags & PROC_BROKER_MAP_READ != 0,
            flags & PROC_BROKER_MAP_WRITE != 0,
            flags & PROC_BROKER_MAP_EXEC != 0,
        );
        let (backed, offset, path_kind) = file.unwrap_or((0, 0, Kind::None));
        [
            (backed != 0).then_some(LinuxImageMapping {
                start, end: start + backed, offset, flags, path_kind,
            }),
            (backed < len).then_some(LinuxImageMapping {
                start: start + backed, end: start + len, offset: 0, flags, path_kind: Kind::None,
            }),
        ].into_iter().flatten()
    }).collect()
}

/// `(start, end, linux prot)` of every exec-image range that
/// `address_space_from_mappings` made private and resident, in image order:
/// writable segments, bss and any file prefix that could not be aliased to the
/// immutable snapshot. Immutable aliases are tracked by the address space itself
/// and are not listed.
fn exec_image_private_ranges(mappings: &[Box<MappingEntry>]) -> Vec<(u64, u64, u64)> {
    let prot_of = |flags: u64| {
        (if flags & PROC_BROKER_MAP_READ != 0 { linux_abi::PROT_READ } else { 0 })
            | (if flags & PROC_BROKER_MAP_WRITE != 0 { linux_abi::PROT_WRITE } else { 0 })
            | (if flags & PROC_BROKER_MAP_EXEC != 0 { linux_abi::PROT_EXEC } else { 0 })
    };
    let mut ranges = Vec::new();
    for mapping in mappings {
        let (start, len, flags) = match mapping.as_ref() {
            MappingEntry::Zeroed { target_addr, mem_len, flags }
            | MappingEntry::Data { target_addr, mem_len, flags, .. } => (*target_addr, *mem_len, *flags),
            MappingEntry::File { backing, file_offset, file_len, target_addr, mem_len, flags } => {
                let shared = if executable_snapshot_is_immutable(backing) {
                    immutable_file_prefix(*file_offset, *file_len, *flags, backing.len() as u64)
                        .unwrap_or(0)
                } else {
                    0
                };
                (target_addr + shared, mem_len.saturating_sub(shared), *flags)
            }
        };
        let Some(end) = start.checked_add(len) else { continue };
        if len != 0 && start.is_multiple_of(PAGE_SIZE) && end.is_multiple_of(PAGE_SIZE) {
            if ranges.try_reserve(1).is_err() {
                break;
            }
            ranges.push((start, end, prot_of(flags)));
        }
    }
    ranges
}

fn address_space_from_mappings(
    mappings: &[Box<MappingEntry>],
) -> Result<crate::memory::paging::ProcessAddressSpace, i64> {
    let mut address_space = crate::memory::paging::ProcessAddressSpace::new()
        .map_err(address_space_error_to_linux_errno)?;
    let mut shared_bytes = 0u64;
    let mut copied_bytes = 0u64;
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
                if !data.is_empty() {
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
            }
            MappingEntry::File {
                backing,
                file_offset,
                file_len,
                target_addr,
                mem_len,
                flags,
            } => {
                // loaderd supplied ranges, not format policy. Share only full
                // immutable pages; partial file pages and zero-fill remain
                // private so no byte beyond the admitted range is exposed.
                let shared = if executable_snapshot_is_immutable(backing) {
                    immutable_file_prefix(*file_offset, *file_len, *flags, backing.len() as u64)?
                } else { 0 };
                if shared != 0 {
                    let (frames, hold) = backing.acquire_mapping(
                        usize::try_from(*file_offset).map_err(|_| LINUX_EOVERFLOW)?,
                        usize::try_from(shared).map_err(|_| LINUX_EOVERFLOW)?, false,
                    ).map_err(|err| match err {
                        crate::user::memfd::MemfdError::NoMemory => LINUX_ENOMEM,
                        _ => LINUX_EINVAL,
                    })?;
                    let backing = crate::memory::paging::ImmutableBacking::new(
                        hold.object_id(), hold.resident_pages(), alloc::sync::Arc::new(hold),
                    );
                    address_space.map_immutable_user_pages_at(
                        VirtAddr::new(*target_addr), &frames, page_flags(*flags)?, backing,
                    ).map_err(address_space_error_to_linux_errno)?;
                }
                if *mem_len > shared {
                    map_zeroed(&mut address_space, *target_addr + shared, *mem_len - shared, *flags).inspect_err(
                    |errno| {
                        nucleus_core::debug::write_debugcon_only_line(
                            alloc::format!(
                                "proc-commit: mapping rejected stage=file-map index={index} target={target_addr:#x} mem_len={mem_len:#x} file_offset={file_offset:#x} file_len={file_len:#x} errno={errno}"
                            )
                            .as_bytes(),
                        );
                    },
                )?;
                }
                shared_bytes += shared;
                copied_bytes += *file_len - shared;
                copy_file_into_address_space(
                    &mut address_space,
                    backing,
                    *target_addr + shared,
                    *file_offset + shared,
                    *file_len - shared,
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
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Memory,
        "proc-image-bytes", shared_bytes, copied_bytes,
    );
    Ok(address_space)
}

fn immutable_file_prefix(file_offset: u64, file_len: u64, flags: u64, backing_len: u64) -> Result<u64, i64> {
    if file_offset.checked_add(file_len).is_none_or(|end| end > backing_len) {
        return Err(LINUX_EIO);
    }
    if flags & PROC_BROKER_MAP_WRITE != 0 || !file_offset.is_multiple_of(PAGE_SIZE) {
        return Ok(0);
    }
    Ok(file_len / PAGE_SIZE * PAGE_SIZE)
}

fn copy_file_into_address_space(
    address_space: &mut crate::memory::paging::ProcessAddressSpace,
    backing: &PinnedFileBacking,
    target_addr: u64,
    file_offset: u64,
    file_len: u64,
) -> Result<(), i64> {
    let total = usize::try_from(file_len).map_err(|_| LINUX_EOVERFLOW)?;
    if total == 0 { return Ok(()); }
    let mut chunk = Vec::new();
    let chunk_len = FILE_COPY_CHUNK.min(total);
    chunk.try_reserve_exact(chunk_len).map_err(|_| LINUX_ENOMEM)?;
    chunk.resize(chunk_len, 0);
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
    validate_user_range(target_addr, mem_len)?;
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

/// Whether `[addr, addr + len)` lies wholly inside the broker user window.
fn user_span_in_window(addr: u64, len: u64) -> bool {
    addr >= PROC_BROKER_USER_SPACE_BASE
        && addr
            .checked_add(len)
            .is_some_and(|end| end <= PROC_BROKER_USER_SPACE_END_EXCLUSIVE)
}

fn linux_tls_fields_valid(args: &RustosProcSetLinuxRuntimeBrokerArgs) -> bool {
    if args.has_tls == 0 {
        return true;
    }
    // The template is copied into a kernel buffer sized by `tls_template_size`,
    // so bound it by the mapping that must hold the block.
    args.tls_template_size <= args.tls_mapping_size
        && user_span_in_window(args.tls_mapping_base, args.tls_mapping_size)
        && user_span_in_window(args.tls_template_addr, args.tls_template_size)
        && user_span_in_window(args.tls_block_base, args.tls_template_size)
        && user_span_in_window(args.tls_tcb_base, 24)
}

fn range_contains(base: u64, len: u64, ptr: u64, access_len: u64) -> bool {
    base.checked_add(len).is_some_and(|range_end| {
        ptr >= base
            && ptr
                .checked_add(access_len)
                .is_some_and(|end| end <= range_end)
    })
}

fn process_load_error_to_linux_errno(error: crate::user::process::ProcessLoadError) -> i64 {
    match error {
        crate::user::process::ProcessLoadError::MissingSchedulingContext => LINUX_EPERM,
        crate::user::process::ProcessLoadError::AddressSpace(err) => {
            address_space_error_to_linux_errno(err)
        }
        crate::user::process::ProcessLoadError::Spawn(err) => {
            process_spawn_error_to_linux_errno(err)
        }
        crate::user::process::ProcessLoadError::InterpreterLoad { .. } => LINUX_ENOEXEC,
        crate::user::process::ProcessLoadError::InvalidElf(_)
        | crate::user::process::ProcessLoadError::InvalidPe(_)
        | crate::user::process::ProcessLoadError::UnsupportedImport { .. } => LINUX_ENOEXEC,
    }
}

/// The one `SpawnTaskError` -> Linux errno table, shared by fork/exec spawns
/// and `clone(CLONE_THREAD)`.
///
/// Memory and kernel-stack/charge failures are `ENOMEM`. Count limits (task
/// table, per-process ceiling, Job `threads.max`) are `EAGAIN`, the errno
/// `clone`/`fork` document for RLIMIT_NPROC, the pids cgroup and the system
/// thread limit. A process that is exiting or running exec is also `EAGAIN`:
/// Linux `copy_process` refuses `CLONE_THREAD` with `-EAGAIN` once the signal
/// group is exiting. A refused creator or publication is a transient race, so
/// `EAGAIN` as well; none of these are programming errors of the caller.
pub(super) fn process_spawn_error_to_linux_errno(error: multitask::SpawnTaskError) -> i64 {
    use multitask::SpawnTaskError as E;
    match error {
        E::InvalidWeightMicros => LINUX_EINVAL,
        E::MemoryQuota | E::ThreadStackAllocation | E::JobThreadMemory => LINUX_ENOMEM,
        E::NoFreeTaskSlot
        | E::ProcessLimit
        | E::ProcessThreadLimit
        | E::JobThreadLimit
        | E::ProcessExiting
        | E::ProcessExecInProgress
        | E::ThreadCreatorUnavailable
        | E::ThreadPublicationRefused
        | E::ThreadMaterializationFailed => LINUX_EAGAIN,
    }
}

fn read_user_text(ptr: u64, len: u64) -> Result<String, i64> {
    let len = usize::try_from(len).map_err(|_| LINUX_EINVAL)?;
    if ptr == 0 || len == 0 || len > 4096 {
        return Err(LINUX_EINVAL);
    }
    let mut bytes = alloc::vec![0_u8; len];
    read_user_bytes_as(ptr, &mut bytes)?;
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

#[cfg(test)]
mod refactor_tests {
    use super::*;

    fn empty_prepare_state() -> ProcPrepareState {
        ProcPrepareState {
            owner_pid: 1,
            format: PROC_BROKER_FORMAT_ELF64,
            stack_top: PROC_BROKER_USER_SPACE_END_EXCLUSIVE,
            exec_ticket: None,
            job: None,
            mappings: Vec::new(),
            windows_runtime: None,
            linux_runtime: None,
        }
    }

    fn zeroed_mapping(offset_pages: u64) -> Box<MappingEntry> {
        Box::new(MappingEntry::Zeroed {
            target_addr: PROC_BROKER_USER_SPACE_BASE + offset_pages * PAGE_SIZE,
            mem_len: PAGE_SIZE,
            flags: PROC_BROKER_MAP_READ | PROC_BROKER_MAP_PRIVATE,
        })
    }

    #[test]
    fn range_contains_rejects_range_overflow_and_out_of_bounds_pointers() {
        assert!(range_contains(0x1000, 0x1000, 0x1000, 8));
        assert!(range_contains(0x1000, 0x1000, 0x1ff8, 8));
        assert!(!range_contains(0x1000, 0x1000, 0x1ff9, 8));
        assert!(!range_contains(u64::MAX - 15, 32, u64::MAX - 8, 4));
        assert!(!range_contains(0x1000, 0x1000, u64::MAX - 3, 8));
    }

    #[test]
    fn validate_mapping_region_enforces_window_alignment_and_wx_exclusion() {
        let base = PROC_BROKER_USER_SPACE_BASE;
        assert_eq!(
            validate_mapping_region(base, PAGE_SIZE, PROC_BROKER_MAP_READ | PROC_BROKER_MAP_PRIVATE),
            Ok(())
        );
        assert_eq!(
            validate_mapping_region(base, PAGE_SIZE, PROC_BROKER_MAP_READ),
            Err(LINUX_EINVAL)
        );
        assert_eq!(
            validate_mapping_region(
                base,
                PAGE_SIZE,
                PROC_BROKER_MAP_PRIVATE | PROC_BROKER_MAP_WRITE | PROC_BROKER_MAP_EXEC
            ),
            Err(LINUX_EINVAL)
        );
        assert_eq!(
            validate_mapping_region(base + 1, PAGE_SIZE, PROC_BROKER_MAP_PRIVATE),
            Err(LINUX_EINVAL)
        );
        assert_eq!(
            validate_mapping_region(
                PROC_BROKER_USER_SPACE_END_EXCLUSIVE,
                PAGE_SIZE,
                PROC_BROKER_MAP_PRIVATE
            ),
            Err(LINUX_EINVAL)
        );
    }

    #[test]
    fn push_prepare_mapping_and_batch_enforce_capacity_without_consuming_rejected_entries() {
        let mut state = empty_prepare_state();
        let mut single = Some(zeroed_mapping(0));
        assert_eq!(push_prepare_mapping(&mut state, &mut single), Ok(()));
        assert!(single.is_none());
        assert_eq!(state.mappings.len(), 1);

        while state.mappings.len() < MAX_MAPPINGS_PER_PREPARE - 1 {
            state.mappings.push(zeroed_mapping(1));
        }

        let mut batch = [Some(zeroed_mapping(2)), Some(zeroed_mapping(3))];
        assert_eq!(push_prepare_mappings(&mut state, &mut batch), Err(LINUX_EINVAL));
        assert_eq!(state.mappings.len(), MAX_MAPPINGS_PER_PREPARE - 1);
        assert!(batch[0].is_some() && batch[1].is_some());

        let mut last = Some(zeroed_mapping(4));
        assert_eq!(push_prepare_mapping(&mut state, &mut last), Ok(()));
        assert!(last.is_none());
        assert_eq!(state.mappings.len(), MAX_MAPPINGS_PER_PREPARE);

        let mut extra = Some(zeroed_mapping(5));
        assert_eq!(push_prepare_mapping(&mut state, &mut extra), Err(LINUX_EINVAL));
        assert!(extra.is_some());
    }

    #[test]
    fn validate_windows_runtime_args_checks_abi_ranges_and_optional_pointers() {
        let image_base = PROC_BROKER_USER_SPACE_BASE + 0x10_0000;
        let runtime_base = PROC_BROKER_USER_SPACE_BASE + 0x40_0000;
        let valid = RustosProcSetWindowsRuntimeBrokerArgs {
            abi_version: PROC_BROKER_ABI_VERSION,
            loader_module_count: 1,
            image_base,
            image_size: 0x10_000,
            entry_point: image_base + 0x1000,
            runtime_base,
            runtime_size: 0x10_000,
            public_runtime_address: runtime_base,
            peb_address: runtime_base + 0x100,
            teb_address: runtime_base + 0x200,
            process_parameters_address: runtime_base + 0x300,
            loader_data_address: runtime_base + 0x400,
            loader_module_array_address: runtime_base + 0x500,
            teb_process_id_ptr: runtime_base + 0x240,
            teb_thread_id_ptr: runtime_base + 0x248,
            ..RustosProcSetWindowsRuntimeBrokerArgs::default()
        };
        assert_eq!(validate_windows_runtime_args(&valid), Ok(()));

        let mut bad = valid;
        bad.abi_version = PROC_BROKER_ABI_VERSION - 1;
        assert_eq!(validate_windows_runtime_args(&bad), Err(LINUX_EINVAL));

        bad = valid;
        bad.reserved2 = 1;
        assert_eq!(validate_windows_runtime_args(&bad), Err(LINUX_EINVAL));

        bad = valid;
        bad.teb_thread_id_ptr = runtime_base + valid.runtime_size - 4;
        assert_eq!(validate_windows_runtime_args(&bad), Err(LINUX_EINVAL));

        bad = valid;
        bad.kuser_shared_data_address = runtime_base + valid.runtime_size - 2048;
        assert_eq!(validate_windows_runtime_args(&bad), Err(LINUX_EINVAL));
    }
}
