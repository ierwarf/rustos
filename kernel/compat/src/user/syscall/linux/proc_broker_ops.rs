use super::*;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;

use crate::user::handles::KernelHandle;
use crate::user::memfd::MemfdHandle;
use lazy_static::lazy_static;
use rustos_user_abi::syscall::{
    COMMERCIAL_MAX_PROCD_OP_PROCESS_PREPARE, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_PROCD, CommercialMaxProtocolRequest, CommercialMaxProtocolResponse,
    IPC_SERVICE_CAP_PROCESS_LOADER, IPC_SERVICE_CAP_PROCESS_POLICY,
    IPC_SERVICE_CAP_ROOT_SUPERVISOR, IPC_SERVICE_INITD, IPC_SERVICE_PROCD, IPC_SERVICE_ROOTD,
    IPC_SERVICE_SESSIOND, LOADER_SPAWN_ARG_BYTES, LOADER_SPAWN_ENV_BYTES,
    LOADER_SPAWN_FLAG_DEFER_START, LOADER_SPAWN_FLAG_IMMEDIATE_HANDOFF, LOADER_SPAWN_MAX_ARG_COUNT,
    LOADER_SPAWN_MAX_ENV_COUNT, PROC_BROKER_ABI_VERSION, PROC_BROKER_BATCH_CAPACITY,
    PROC_BROKER_FORMAT_ELF64, PROC_BROKER_FORMAT_PE64, PROC_BROKER_LINUX_INTERP_PATH_CAPACITY,
    PROC_BROKER_MAP_EXEC, PROC_BROKER_MAP_PRIVATE, PROC_BROKER_MAP_READ, PROC_BROKER_MAP_WRITE,
    PROC_BROKER_USER_SPACE_BASE, PROC_BROKER_USER_SPACE_END_EXCLUSIVE, RustosProcAbortBrokerArgs,
    RustosProcActivateBrokerArgs, RustosProcAuthorizeExecBrokerArgs,
    RustosProcCancelExecBrokerArgs, RustosProcCommitBrokerArgs, RustosProcExecTargetBrokerArgs,
    RustosProcForkBrokerArgs, RustosProcMapDataBrokerArgs, RustosProcMapFileBatchBrokerArgs,
    RustosProcMapFileBrokerArgs, RustosProcMapZeroedBrokerArgs, RustosProcPrepareBrokerArgs,
    RustosProcSetLinuxRuntimeBrokerArgs, RustosProcSetWindowsRuntimeBrokerArgs,
    RustosProcSignalQueueBrokerArgs, RustosProcValidateDeferredSpawnBrokerArgs,
    RustosUserRegisters, loader_service_role_allows_operation,
};
use spin::Mutex;

const PAGE_SIZE: u64 = 4096;
const SPAWN_FLAG_LOGICAL_ADMIN: u64 = 1;
const MAX_PROC_PREPARES: usize = 128;
const MAX_MAPPINGS_PER_PREPARE: usize = 4096;
const MAX_EXEC_TICKETS: usize = 128;
const MAX_DEFERRED_ACTIVATIONS: usize = 128;
const FILE_COPY_CHUNK: usize = 64 * 1024;

static NEXT_PREPARE_HANDLE: AtomicU64 = AtomicU64::new(1);
static NEXT_EXEC_TICKET: AtomicU64 = AtomicU64::new(1);

lazy_static! {
    static ref PROC_PREPARES: Mutex<BTreeMap<u64, ProcPrepareState>> = Mutex::new(BTreeMap::new());
    static ref EXEC_TICKETS: Mutex<BTreeMap<u64, ExecTicketState>> = Mutex::new(BTreeMap::new());
    static ref EXEC_TRANSITIONS: Mutex<BTreeMap<u64, ExecTransitionState>> =
        Mutex::new(BTreeMap::new());
    /// A deferred process is inert until the exact process that requested its
    /// creation consumes this one-shot authority. Keeping this in ring0 makes
    /// the authority survive loaderd restart without trusting a replayed
    /// userspace PID claim.
    static ref DEFERRED_ACTIVATIONS: Mutex<BTreeMap<u64, u64>> =
        Mutex::new(BTreeMap::new());
}

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
    mappings: Vec<MappingEntry>,
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

pub(super) fn syscall_linux_rustos_proc_prepare_broker(args_ptr: u64) -> u64 {
    if !current_process_can_load() {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosProcPrepareBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != PROC_BROKER_ABI_VERSION || args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(errno) = procd_process_prepare_policy(args.format) {
        return linux_errno(errno);
    }

    let Some(owner_pid) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_EPERM);
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
    prepares.insert(
        handle,
        ProcPrepareState {
            owner_pid,
            format: args.format,
            mappings: Vec::new(),
            windows_runtime: None,
            linux_runtime: None,
        },
    );
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "proc-prepare-published",
        handle,
        u64::from(args.format),
    );
    handle
}

pub(super) fn syscall_linux_rustos_proc_map_file_broker(args_ptr: u64) -> u64 {
    if !current_process_can_load() {
        return linux_errno(LINUX_EPERM);
    }
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
    let mut prepares = PROC_PREPARES.lock();
    let Some(state) = prepares.get_mut(&args.prepare_handle) else {
        return linux_errno(LINUX_EINVAL);
    };
    if !prepare_owned_by_current(state) {
        return linux_errno(LINUX_EPERM);
    }
    if let Err(errno) = validate_mapping_region(args.target_addr, args.mem_len, args.flags) {
        return linux_errno(errno);
    }
    if let Err(errno) = validate_file_mapping_len(args.mem_len, args.file_len) {
        return linux_errno(errno);
    }
    if state.mappings.len() >= MAX_MAPPINGS_PER_PREPARE {
        return linux_errno(LINUX_EINVAL);
    }
    state.mappings.push(MappingEntry::File {
        backing,
        file_offset: args.file_offset,
        file_len: args.file_len,
        target_addr: args.target_addr,
        mem_len: args.mem_len,
        flags: args.flags,
    });
    0
}

pub(super) fn syscall_linux_rustos_proc_map_file_batch_broker(args_ptr: u64) -> u64 {
    if !current_process_can_load() {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosProcMapFileBatchBrokerArgs>(args_ptr)
    {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    let count = args.count as usize;
    if args.reserved0 != 0 || count == 0 || count > PROC_BROKER_BATCH_CAPACITY {
        return linux_errno(LINUX_EINVAL);
    }
    // Resolve all fds before locking PROC_PREPARES
    let mut backings: Vec<PinnedFileBacking> = Vec::with_capacity(count);
    for entry in &args.entries[..count] {
        match pinned_file_backing_from_current(entry.fd) {
            Ok(b) => backings.push(b),
            Err(e) => return linux_errno(e),
        }
    }
    let mut prepares = PROC_PREPARES.lock();
    let Some(state) = prepares.get_mut(&args.prepare_handle) else {
        return linux_errno(LINUX_EINVAL);
    };
    if !prepare_owned_by_current(state) {
        return linux_errno(LINUX_EPERM);
    }
    for entry in &args.entries[..count] {
        if entry.reserved0 != 0 {
            return linux_errno(LINUX_EINVAL);
        }
        if let Err(errno) = validate_mapping_region(entry.target_addr, entry.mem_len, entry.flags) {
            return linux_errno(errno);
        }
        if let Err(errno) = validate_file_mapping_len(entry.mem_len, entry.file_len) {
            return linux_errno(errno);
        }
    }
    if state.mappings.len() + count > MAX_MAPPINGS_PER_PREPARE {
        return linux_errno(LINUX_EINVAL);
    }
    for (i, entry) in args.entries[..count].iter().enumerate() {
        state.mappings.push(MappingEntry::File {
            backing: backings[i].clone(),
            file_offset: entry.file_offset,
            file_len: entry.file_len,
            target_addr: entry.target_addr,
            mem_len: entry.mem_len,
            flags: entry.flags,
        });
    }
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "proc-map-file-batch",
        args.prepare_handle,
        count as u64,
    );
    0
}

pub(super) fn syscall_linux_rustos_proc_set_linux_runtime_broker(args_ptr: u64) -> u64 {
    if !current_process_can_load() {
        return linux_errno(LINUX_EPERM);
    }
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
    let mut prepares = PROC_PREPARES.lock();
    let Some(state) = prepares.get_mut(&args.prepare_handle) else {
        return linux_errno(LINUX_EINVAL);
    };
    if !prepare_owned_by_current(state) {
        return linux_errno(LINUX_EPERM);
    }
    if state.format != PROC_BROKER_FORMAT_ELF64 || state.linux_runtime.is_some() {
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
    state.linux_runtime = Some((info, args.actual_entry));
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "proc-linux-runtime-published",
        args.prepare_handle,
        args.phnum,
    );
    0
}

pub(super) fn syscall_linux_rustos_proc_map_zeroed_broker(args_ptr: u64) -> u64 {
    if !current_process_can_load() {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosProcMapZeroedBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let mut prepares = PROC_PREPARES.lock();
    let Some(state) = prepares.get_mut(&args.prepare_handle) else {
        return linux_errno(LINUX_EINVAL);
    };
    if !prepare_owned_by_current(state) {
        return linux_errno(LINUX_EPERM);
    }
    if let Err(errno) = validate_mapping_region(args.target_addr, args.mem_len, args.flags) {
        return linux_errno(errno);
    }
    if state.mappings.len() >= MAX_MAPPINGS_PER_PREPARE {
        return linux_errno(LINUX_EINVAL);
    }
    state.mappings.push(MappingEntry::Zeroed {
        target_addr: args.target_addr,
        mem_len: args.mem_len,
        flags: args.flags,
    });
    0
}

pub(super) fn syscall_linux_rustos_proc_map_data_broker(args_ptr: u64) -> u64 {
    if !current_process_can_load() {
        return linux_errno(LINUX_EPERM);
    }
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
    let mut prepares = PROC_PREPARES.lock();
    let Some(state) = prepares.get_mut(&args.prepare_handle) else {
        return linux_errno(LINUX_EINVAL);
    };
    if !prepare_owned_by_current(state) {
        return linux_errno(LINUX_EPERM);
    }
    if let Err(errno) = validate_mapping_region(args.target_addr, args.mem_len, args.flags) {
        return linux_errno(errno);
    }
    if state.mappings.len() >= MAX_MAPPINGS_PER_PREPARE {
        return linux_errno(LINUX_EINVAL);
    }
    state.mappings.push(MappingEntry::Data {
        target_addr: args.target_addr,
        mem_len: args.mem_len,
        flags: args.flags,
        data_offset: args.data_offset,
        data: args.data[..data_len].to_vec(),
    });
    0
}

pub(super) fn syscall_linux_rustos_proc_set_windows_runtime_broker(args_ptr: u64) -> u64 {
    if !current_process_can_load() {
        return linux_errno(LINUX_EPERM);
    }
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
    if !prepare_owned_by_current(state) {
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
    if !current_process_can_load() {
        return linux_errno(LINUX_EPERM);
    }
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
            Some(s) if !prepare_owned_by_current(s) => return linux_errno(LINUX_EPERM),
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
    let spawned = if args.flags & LOADER_SPAWN_FLAG_DEFER_START as u64 != 0 {
        crate::user::process::spawn_prepared_process_suspended(prepared, args.weight_micros)
    } else if args.flags & LOADER_SPAWN_FLAG_IMMEDIATE_HANDOFF as u64 != 0 {
        crate::user::process::spawn_prepared_process(prepared, args.weight_micros)
    } else {
        crate::user::process::spawn_prepared_process_for_loader_reply(prepared, args.weight_micros)
    };
    match spawned {
        Ok(spawned) => {
            if args.flags & LOADER_SPAWN_FLAG_DEFER_START as u64 != 0 {
                let mut activations = DEFERRED_ACTIVATIONS.lock();
                if activations.len() >= MAX_DEFERRED_ACTIVATIONS
                    || activations.contains_key(&spawned.pid)
                {
                    drop(activations);
                    let _ = multitask::terminate_user_process(spawned.pid);
                    return linux_errno(LINUX_EAGAIN);
                }
                activations.insert(spawned.pid, args.requester_pid);
                drop(activations);
                // Close the requester-exit race on both sides of publication:
                // cleanup either observes the new entry, or this recheck
                // consumes it and retires the still-suspended target.
                if multitask::is_user_process_exiting(args.requester_pid) {
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
    {
        let mut activations = DEFERRED_ACTIVATIONS.lock();
        if !consume_deferred_activation_authority(
            &mut activations,
            args.target_pid,
            args.requester_pid,
        ) {
            return linux_errno(LINUX_EPERM);
        }
        // Capability consumption is the linearization point. A concurrent
        // requester exit/revoke may win before this removal; once removed,
        // this exact activation has already committed and cannot be replayed.
    }
    if !multitask::activate_suspended_user_task(args.target_pid) {
        return linux_errno(LINUX_ESRCH);
    }
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
    if multitask::is_user_process_exiting(args.target_pid)
        || multitask::is_user_process_exiting(args.requester_pid)
    {
        return linux_errno(LINUX_ESRCH);
    }
    if !deferred_spawn_provenance_matches(
        &DEFERRED_ACTIVATIONS.lock(),
        args.target_pid,
        args.requester_pid,
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
        tickets.insert(
            ticket,
            ExecTicketState {
                target_pid: args.target_pid,
                target_tid: args.target_tid,
            },
        );
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
    if !current_process_can_load() {
        return linux_errno(LINUX_EPERM);
    }
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
            Some(s) if !prepare_owned_by_current(s) => return linux_errno(LINUX_EPERM),
            _ => {}
        }
        prepares.remove(&args.prepare_handle).unwrap()
    };
    if state.format != PROC_BROKER_FORMAT_ELF64 || state.windows_runtime.is_some() {
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
        transitions.insert(args.target_tid, transition);
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
    if !current_process_can_policy() {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosProcForkBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != PROC_BROKER_ABI_VERSION
        || args.reserved0 != 0
        || args.flags != 0
        || args.source_pid == 0
        || args.source_tid == 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(thread_snapshot) =
        multitask::linux_thread_snapshot_by_ids(args.source_pid, args.source_tid)
    else {
        return linux_errno(LINUX_ESRCH);
    };
    let child_state = match multitask::with_process_state_by_pid(args.source_pid, |parent| {
        let address_space = parent.address_space().clone_user_space()?;
        Ok::<_, crate::memory::paging::AddressSpaceError>(parent.fork_clone(address_space, None))
    }) {
        Some(Ok(state)) => state,
        Some(Err(err)) => return linux_errno(address_space_error_to_linux_errno(err)),
        None => return linux_errno(LINUX_ESRCH),
    };
    let mut child_thread_state = thread_snapshot.thread_state;
    child_thread_state.clear_child_tid = 0;
    child_thread_state.robust_list_head = 0;
    child_thread_state.robust_list_len = 0;
    child_thread_state.rseq_area = 0;
    child_thread_state.rseq_len = 0;
    child_thread_state.rseq_signature = 0;
    child_thread_state.pending_signals = 0;

    let mut bootstrap = multitask::UserTaskBootstrap::new(
        crate::user::abi::UserAbi::Linux,
        VirtAddr::new(args.registers.rip),
        VirtAddr::new(if args.stack_ptr != 0 {
            args.stack_ptr
        } else {
            args.registers.rsp
        }),
    );
    bootstrap.registers = user_registers_to_task_registers(args.registers);
    bootstrap.registers.rax = 0;
    bootstrap.registers.rcx = args.registers.rip;
    bootstrap.registers.r11 = args.registers.rflags;
    bootstrap.user_stack = thread_snapshot.user_stack;
    bootstrap.console_session = thread_snapshot.console_session;
    bootstrap.logical_admin = child_state.security().is_logical_admin();
    bootstrap.linux_process_state = child_state.linux_process_state().copied();
    bootstrap.linux_memory_map = child_state.linux_memory_map().cloned();
    bootstrap.linux_runtime_profile = child_state.linux_runtime_profile().cloned();
    bootstrap.linux_thread_state = Some(child_thread_state);
    bootstrap.set_exec_path(child_state.exec_path());

    // Reserve every service-owned open-description reference before the child
    // becomes runnable. A child close must not be able to retire a socket,
    // remote VFS handle, or epoll object still referenced by its parent.
    let inherited_service_refs = match acquire_cloned_service_handle_refs(&child_state) {
        Ok(refs) => refs,
        Err(errno) => return linux_errno(errno),
    };
    match multitask::spawn_user_process_state_with_parent(
        child_state,
        bootstrap,
        Some(args.source_pid),
        multitask::DEFAULT_USER_TASK_WEIGHT_MICROS,
    ) {
        Ok(pid) => pid,
        Err(err) => {
            release_service_handle_refs(&inherited_service_refs);
            linux_errno(process_spawn_error_to_linux_errno(err))
        }
    }
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
    if !current_process_can_load() {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosProcAbortBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let mut prepares = PROC_PREPARES.lock();
    if let Some(state) = prepares.get(&args.prepare_handle) {
        if !prepare_owned_by_current(state) {
            return linux_errno(LINUX_EPERM);
        }
    }
    prepares.remove(&args.prepare_handle);
    0
}

/// Process-broker state is bounded and process-scoped. A loader that exits
/// between PREPARE and COMMIT cannot reach ABORT, and a target that exits after
/// procd authorization cannot consume a later exec ticket. Process teardown
/// therefore removes owner-bound prepares plus target-bound tickets and saved
/// register transitions before the process table retires that process.
pub(super) fn cleanup_proc_broker_state_for_process(process_id: u64) -> (usize, usize, usize) {
    let deferred_targets = {
        let mut activations = DEFERRED_ACTIVATIONS.lock();
        let targets = activations
            .iter()
            .filter_map(|(target_pid, requester_pid)| {
                (*requester_pid == process_id && *target_pid != process_id).then_some(*target_pid)
            })
            .collect::<Vec<_>>();
        activations.retain(|target_pid, requester_pid| {
            *target_pid != process_id && *requester_pid != process_id
        });
        targets
    };
    for target_pid in deferred_targets {
        let _ = multitask::terminate_user_process(target_pid);
    }

    let mut prepares = PROC_PREPARES.lock();
    let prepares_before = prepares.len();
    prepares.retain(|_, state| state.owner_pid != process_id);
    let removed_prepares = prepares_before.saturating_sub(prepares.len());
    drop(prepares);

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

fn prepare_owned_by_current(state: &ProcPrepareState) -> bool {
    multitask::current_user_process_id() == Some(state.owner_pid)
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
    mappings: &[MappingEntry],
) -> Result<crate::memory::paging::ProcessAddressSpace, i64> {
    let mut address_space = crate::memory::paging::ProcessAddressSpace::new()
        .map_err(address_space_error_to_linux_errno)?;
    for (index, mapping) in mappings.iter().enumerate() {
        match mapping {
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

fn allocate_prepare_handle(prepares: &BTreeMap<u64, ProcPrepareState>) -> Option<u64> {
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

fn allocate_exec_ticket(tickets: &BTreeMap<u64, ExecTicketState>) -> Option<u64> {
    for _ in 0..MAX_EXEC_TICKETS {
        let ticket = allocate_nonwrapping_broker_identity(&NEXT_EXEC_TICKET)?;
        if !tickets.contains_key(&ticket) {
            return Some(ticket);
        }
    }
    None
}

fn consume_deferred_activation_authority(
    activations: &mut BTreeMap<u64, u64>,
    target_pid: u64,
    requester_pid: u64,
) -> bool {
    if target_pid == 0
        || requester_pid == 0
        || activations.get(&target_pid).copied() != Some(requester_pid)
    {
        return false;
    }
    activations.remove(&target_pid);
    true
}

fn deferred_spawn_provenance_matches(
    activations: &BTreeMap<u64, u64>,
    target_pid: u64,
    requester_pid: u64,
) -> bool {
    target_pid != 0
        && requester_pid != 0
        && activations.get(&target_pid).copied() == Some(requester_pid)
}

fn allocate_nonwrapping_broker_identity(counter: &AtomicU64) -> Option<u64> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            (current != 0).then(|| current.checked_add(1)).flatten()
        })
        .ok()
}

// RING3-MIGRATION-REFERENCE START: capability-broker exception: procd owns
// process-prepare admission policy. Ring0 keeps the capability-gated broker
// handle table and calls procd before allocating privileged prepare state.
fn procd_process_prepare_policy(format: u16) -> Result<(), i64> {
    let Some(snapshot) = multitask::current_user_snapshot() else {
        return Err(LINUX_EPERM);
    };
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_PROCD;
    request.header.op = COMMERCIAL_MAX_PROCD_OP_PROCESS_PREPARE;
    request.header.service_id = rustos_user_abi::syscall::IPC_SERVICE_PROCD;
    request.header.subject_pid = snapshot.process_id();
    request.header.subject_tid = snapshot.thread_id();
    request.arg0 = u64::from(format);
    let response = match ipc_ops::call_service_endpoint_with_class(
        rustos_user_abi::syscall::IPC_SERVICE_PROCD,
        as_bytes(&request),
        ipc_ops::ServiceIpcClass::BootControl,
    ) {
        Ok(response) => response,
        Err(errno) => return Err(errno),
    };
    if response.len() != core::mem::size_of::<CommercialMaxProtocolResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<CommercialMaxProtocolResponse>(response.as_slice());
    ipc_ops::validate_commercial_response_envelope(&request, &response)?;
    if response.payload_len != 0
        || response.descriptor_count != 1
        || response.value0 != u64::from(format)
        || response.value1 != PROC_BROKER_ABI_VERSION as u64
    {
        return Err(LINUX_EINVAL);
    }
    if response.status == 0 {
        Ok(())
    } else {
        Err(response.status.unsigned_abs() as i64)
    }
}
// RING3-MIGRATION-REFERENCE END: procd-owned process-prepare admission substrate exception.

fn validate_mapping_region(target_addr: u64, mem_len: u64, flags: u64) -> Result<(), i64> {
    if mem_len == 0
        || target_addr % PAGE_SIZE != 0
        || mem_len % PAGE_SIZE != 0
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
        || start % PAGE_SIZE != 0
        || len % PAGE_SIZE != 0
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
mod tests {
    use super::*;

    #[test]
    fn broker_authority_identity_exhaustion_never_wraps() {
        let counter = AtomicU64::new(u64::MAX);
        assert_eq!(allocate_nonwrapping_broker_identity(&counter), None);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn file_mapping_len_must_fit_inside_memory_mapping() {
        assert_eq!(validate_file_mapping_len(4096, 4096), Ok(()));
        assert_eq!(validate_file_mapping_len(4096, 0), Ok(()));
        assert_eq!(validate_file_mapping_len(4096, 4097), Err(LINUX_EINVAL));
    }

    #[test]
    fn truncated_file_mapping_never_commits_zero_filled_tail() {
        assert_eq!(validate_complete_file_copy(4096, 4096), Ok(()));
        assert_eq!(validate_complete_file_copy(4096, 4095), Err(LINUX_EIO));
        assert_eq!(validate_complete_file_copy(1, 0), Err(LINUX_EIO));
    }

    #[test]
    fn executable_file_backing_requires_a_terminally_sealed_snapshot() {
        let snapshot = MemfdHandle::new(String::from("loader-test"), true);
        assert!(!executable_snapshot_is_immutable(&snapshot));
        snapshot
            .add_seals(
                (linux_abi::F_SEAL_WRITE | linux_abi::F_SEAL_GROW | linux_abi::F_SEAL_SHRINK)
                    as u32,
            )
            .expect("partial seals");
        assert!(!executable_snapshot_is_immutable(&snapshot));
        snapshot
            .add_seals(linux_abi::F_SEAL_SEAL as u32)
            .expect("terminal seal");
        assert!(executable_snapshot_is_immutable(&snapshot));
    }

    #[test]
    fn exited_prepare_owner_cannot_republish_after_cleanup() {
        assert_eq!(proc_prepare_publication_status(true, 0), Err(LINUX_ESRCH));
        assert_eq!(
            proc_prepare_publication_status(false, MAX_PROC_PREPARES),
            Err(LINUX_EAGAIN)
        );
        assert_eq!(
            proc_prepare_publication_status(false, MAX_PROC_PREPARES - 1),
            Ok(())
        );
    }

    #[test]
    fn deferred_activation_authority_is_exact_one_shot_and_nontransferable() {
        let mut activations = BTreeMap::from([(41, 7)]);
        assert!(deferred_spawn_provenance_matches(&activations, 41, 7));
        assert!(!deferred_spawn_provenance_matches(&activations, 41, 8));
        assert!(!consume_deferred_activation_authority(
            &mut activations,
            41,
            8
        ));
        assert_eq!(activations.get(&41), Some(&7));
        assert!(consume_deferred_activation_authority(
            &mut activations,
            41,
            7
        ));
        assert!(!consume_deferred_activation_authority(
            &mut activations,
            41,
            7
        ));
        assert!(!deferred_spawn_provenance_matches(&activations, 41, 7));
    }

    #[test]
    fn loader_commit_revalidates_live_requester_role_before_consuming_authority() {
        let source = include_str!("proc_broker_ops.rs");
        let spawn_commit = source
            .split("pub(super) fn syscall_linux_rustos_proc_commit_broker")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(super) fn syscall_linux_rustos_proc_activate_broker")
                    .next()
            })
            .expect("spawn commit broker");
        let spawn_role = spawn_commit
            .find("requester_owns_live_spawn_role(args.requester_pid)")
            .expect("live spawn role recheck");
        let prepare_consume = spawn_commit
            .find("let state = {")
            .expect("prepare authority consumption");
        assert!(spawn_role < prepare_consume);

        let exec_commit = source
            .split("pub(super) fn syscall_linux_rustos_proc_exec_target_broker")
            .nth(1)
            .and_then(|rest| rest.split("fn exec_transition_from_prepared").next())
            .expect("exec target broker");
        let procd_role = exec_commit
            .find("process_owns_live_service_endpoint(args.requester_pid, IPC_SERVICE_PROCD)")
            .expect("live procd role recheck");
        let ticket_consume = exec_commit
            .find("let mut tickets = EXEC_TICKETS.lock()")
            .expect("exec ticket consumption");
        assert!(procd_role < ticket_consume);
    }
}
