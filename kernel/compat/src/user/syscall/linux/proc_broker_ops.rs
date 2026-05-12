use super::*;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;

use lazy_static::lazy_static;
use rustos_user_abi::syscall::{
    IPC_SERVICE_CAP_PROCESS_LOADER, PROC_BROKER_ABI_VERSION, PROC_BROKER_FORMAT_ELF64,
    PROC_BROKER_FORMAT_PE64, PROC_BROKER_MAP_EXEC, PROC_BROKER_MAP_PRIVATE, PROC_BROKER_MAP_READ,
    PROC_BROKER_MAP_WRITE, PROC_BROKER_USER_SPACE_BASE, PROC_BROKER_USER_SPACE_END_EXCLUSIVE,
    RustosProcAbortBrokerArgs, RustosProcCommitBrokerArgs, RustosProcMapDataBrokerArgs,
    RustosProcMapFileBrokerArgs, RustosProcMapZeroedBrokerArgs, RustosProcPrepareBrokerArgs,
};
use spin::Mutex;

const PAGE_SIZE: u64 = 4096;
const SPAWN_FLAG_LOGICAL_ADMIN: u64 = 1;
const MAX_PROC_PREPARES: usize = 128;
const MAX_MAPPINGS_PER_PREPARE: usize = 4096;

static NEXT_PREPARE_HANDLE: AtomicU64 = AtomicU64::new(1);

lazy_static! {
    static ref PROC_PREPARES: Mutex<BTreeMap<u64, ProcPrepareState>> = Mutex::new(BTreeMap::new());
}

#[derive(Clone)]
enum MappingEntry {
    File {
        fd: u64,
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
    format: u16,
    mappings: Vec<MappingEntry>,
}

pub(super) fn syscall_linux_rustos_proc_prepare_broker(args_ptr: u64) -> u64 {
    if !current_process_can_load() {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosProcPrepareBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != PROC_BROKER_ABI_VERSION
        || args.reserved0 != 0
        || !matches!(
            args.format,
            PROC_BROKER_FORMAT_ELF64 | PROC_BROKER_FORMAT_PE64
        )
    {
        return linux_errno(LINUX_EINVAL);
    }

    let mut prepares = PROC_PREPARES.lock();
    if prepares.len() >= MAX_PROC_PREPARES {
        return linux_errno(LINUX_EAGAIN);
    }
    let Some(handle) = allocate_prepare_handle(&prepares) else {
        return linux_errno(LINUX_EAGAIN);
    };
    prepares.insert(
        handle,
        ProcPrepareState {
            format: args.format,
            mappings: Vec::new(),
        },
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
    let mut prepares = PROC_PREPARES.lock();
    let Some(state) = prepares.get_mut(&args.prepare_handle) else {
        return linux_errno(LINUX_EINVAL);
    };
    if let Err(errno) = validate_mapping_region(args.target_addr, args.mem_len, args.flags) {
        return linux_errno(errno);
    }
    if state.mappings.len() >= MAX_MAPPINGS_PER_PREPARE {
        return linux_errno(LINUX_EINVAL);
    }
    state.mappings.push(MappingEntry::File {
        fd: args.fd,
        file_offset: args.file_offset,
        file_len: args.file_len,
        target_addr: args.target_addr,
        mem_len: args.mem_len,
        flags: args.flags,
    });
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
    let mut prepares = PROC_PREPARES.lock();
    let Some(state) = prepares.get_mut(&args.prepare_handle) else {
        return linux_errno(LINUX_EINVAL);
    };
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

pub(super) fn syscall_linux_rustos_proc_commit_broker(args_ptr: u64) -> u64 {
    if !current_process_can_load() {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosProcCommitBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    let Some(state) = PROC_PREPARES.lock().remove(&args.prepare_handle) else {
        return linux_errno(LINUX_EINVAL);
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
    let loaded = match crate::user::console_host::load_executable_image_by_path(&exec_path, None) {
        Ok(loaded) => loaded,
        Err(err) => return linux_errno(console_host_error_to_linux_errno(err)),
    };
    let address_space = match address_space_from_mappings(&state.mappings) {
        Ok(address_space) => address_space,
        Err(errno) => return linux_errno(errno),
    };
    let session = if args.console_session == 0 {
        multitask::current_user_snapshot()
            .map(|snapshot| snapshot.console_session())
            .unwrap_or(crate::io::session::ConsoleSessionHandle::SYSTEM)
    } else {
        crate::io::session::ConsoleSessionHandle::from_raw(args.console_session)
    };
    let logical_admin = args.flags & SPAWN_FLAG_LOGICAL_ADMIN != 0;
    let launch = crate::user::process::ProcessLaunchOptions {
        linux: crate::user::linux::LinuxProcessLaunch::new(loaded.path),
        console_session: session,
        logical_admin,
        ..crate::user::process::ProcessLaunchOptions::default()
    };
    let prepared = match crate::user::process::prepare_process_with_address_space(
        &loaded.bytes,
        address_space,
        launch,
    ) {
        Ok(prepared) => prepared,
        Err(err) => return linux_errno(process_load_error_to_linux_errno(err)),
    };
    match crate::user::process::spawn_prepared_process(prepared, args.weight_micros) {
        Ok(spawned) => spawned.pid,
        Err(err) => linux_errno(process_load_error_to_linux_errno(err)),
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
    PROC_PREPARES.lock().remove(&args.prepare_handle);
    0
}

fn current_process_can_load() -> bool {
    ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_PROCESS_LOADER)
}

fn address_space_from_mappings(
    mappings: &[MappingEntry],
) -> Result<crate::memory::paging::ProcessAddressSpace, i64> {
    let mut address_space = crate::memory::paging::ProcessAddressSpace::new()
        .map_err(address_space_error_to_linux_errno)?;
    for mapping in mappings {
        match mapping {
            MappingEntry::Zeroed {
                target_addr,
                mem_len,
                flags,
            } => {
                map_zeroed(&mut address_space, *target_addr, *mem_len, *flags)?;
            }
            MappingEntry::Data {
                target_addr,
                mem_len,
                flags,
                data_offset,
                data,
            } => {
                map_zeroed(&mut address_space, *target_addr, *mem_len, *flags)?;
                let write_addr = target_addr
                    .checked_add(*data_offset)
                    .ok_or(LINUX_EOVERFLOW)?;
                address_space
                    .initialize_user_bytes(VirtAddr::new(write_addr), data)
                    .map_err(address_space_error_to_linux_errno)?;
            }
            MappingEntry::File { .. } => {}
        }
    }
    Ok(address_space)
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
        let handle = NEXT_PREPARE_HANDLE.fetch_add(1, Ordering::Relaxed).max(1);
        if !prepares.contains_key(&handle) {
            return Some(handle);
        }
    }
    None
}

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

fn console_host_error_to_linux_errno(error: crate::user::console_host::ConsoleHostError) -> i64 {
    match error {
        crate::user::console_host::ConsoleHostError::BootstrapBlocked => LINUX_EAGAIN,
        crate::user::console_host::ConsoleHostError::Load { error, .. } => match error {
            crate::vfs::VfsError::BadFileDescriptor => LINUX_EBADF,
            crate::vfs::VfsError::InvalidArgument => LINUX_EINVAL,
            crate::vfs::VfsError::NotFound => LINUX_ENOENT,
            crate::vfs::VfsError::PermissionDenied => LINUX_EACCES,
            crate::vfs::VfsError::NotDirectory => LINUX_ENOTDIR,
            crate::vfs::VfsError::ReadOnlyFilesystem => LINUX_EROFS,
            crate::vfs::VfsError::Unsupported => LINUX_ENOSYS,
        },
        crate::user::console_host::ConsoleHostError::Spawn { error } => {
            process_load_error_to_linux_errno(error)
        }
    }
}
