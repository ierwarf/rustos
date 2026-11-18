//! Wine 11.22 Windows x64 NT system call substrate for RustOS Ring0 + syscalld.
//!
//! Translates genuine Windows NT x64 `syscall` frames (`0x0000..0x02ff`) using
//! Ring3 admission policy (`syscalld`) and Ring0 mechanisms (`vfsd` VFS, VM,
//! PE section mapping, NLS tables, time/topology, synchronization objects).

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::size_of;
use core::slice;

use rustos_user_abi::linux as linux_abi;
use rustos_user_abi::syscall::{
    LinuxTimespecWire, SYSCALL_OFFLOAD_OP_WIN32_ALLOC_VIRTUAL_MEMORY,
    SYSCALL_OFFLOAD_OP_WIN32_CLOSE, SYSCALL_OFFLOAD_OP_WIN32_CONTINUE,
    SYSCALL_OFFLOAD_OP_WIN32_CREATE_EVENT, SYSCALL_OFFLOAD_OP_WIN32_CREATE_SECTION,
    SYSCALL_OFFLOAD_OP_WIN32_DELAY_EXECUTION, SYSCALL_OFFLOAD_OP_WIN32_DEVICE_IOCTL,
    SYSCALL_OFFLOAD_OP_WIN32_DUPLICATE_OBJECT, SYSCALL_OFFLOAD_OP_WIN32_EXIT_PROCESS,
    SYSCALL_OFFLOAD_OP_WIN32_FREE_VIRTUAL_MEMORY,
    SYSCALL_OFFLOAD_OP_WIN32_GET_CURRENT_PROCESSOR_NUMBER,
    SYSCALL_OFFLOAD_OP_WIN32_GET_NLS_SECTION, SYSCALL_OFFLOAD_OP_WIN32_INIT_NLS,
    SYSCALL_OFFLOAD_OP_WIN32_MAP_VIEW, SYSCALL_OFFLOAD_OP_WIN32_OPEN_FILE,
    SYSCALL_OFFLOAD_OP_WIN32_OPEN_KEY, SYSCALL_OFFLOAD_OP_WIN32_PROTECT_VIRTUAL_MEMORY,
    SYSCALL_OFFLOAD_OP_WIN32_QUERY_FILE, SYSCALL_OFFLOAD_OP_WIN32_QUERY_INFO_PROCESS,
    SYSCALL_OFFLOAD_OP_WIN32_QUERY_INFO_THREAD, SYSCALL_OFFLOAD_OP_WIN32_QUERY_LOCALE,
    SYSCALL_OFFLOAD_OP_WIN32_QUERY_SYSTEM_INFORMATION, SYSCALL_OFFLOAD_OP_WIN32_QUERY_TIME,
    SYSCALL_OFFLOAD_OP_WIN32_QUERY_VIRTUAL_MEMORY, SYSCALL_OFFLOAD_OP_WIN32_READ_FILE,
    SYSCALL_OFFLOAD_OP_WIN32_SET_FILE, SYSCALL_OFFLOAD_OP_WIN32_SET_INFO_PROCESS,
    SYSCALL_OFFLOAD_OP_WIN32_SET_INFO_THREAD, SYSCALL_OFFLOAD_OP_WIN32_TERMINATE_THREAD,
    SYSCALL_OFFLOAD_OP_WIN32_UNMAP_VIEW, SYSCALL_OFFLOAD_OP_WIN32_WAIT_OBJECT,
    SYSCALL_OFFLOAD_OP_WIN32_WRITE_FILE, SYSCALL_OFFLOAD_OP_WIN32_YIELD,
    WIN32_SYSCALL_OFFLOAD_ABI_VERSION, Win32SyscallOffloadRequest,
    Win32SyscallOffloadResponse,
};
use rustos_user_abi::windows::{
    HANDLE_CURRENT_PROCESS, HANDLE_CURRENT_THREAD, HANDLE_STDERR, HANDLE_STDIN, HANDLE_STDOUT,
    MEM_COMMIT, MEM_DECOMMIT, MEM_FREE, MEM_IMAGE, MEM_MAPPED, MEM_PRIVATE, MEM_RELEASE,
    MEM_RESERVE, PAGE_EXECUTE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY,
    PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE, PAGE_SIZE, PAGE_WRITECOPY, STATUS_ACCESS_DENIED,
    STATUS_END_OF_FILE, STATUS_IMAGE_NOT_AT_BASE,
    STATUS_INFO_LENGTH_MISMATCH, STATUS_INVALID_HANDLE, STATUS_INVALID_INFO_CLASS,
    STATUS_INVALID_PARAMETER, STATUS_INVALID_SYSTEM_SERVICE, STATUS_NOT_IMPLEMENTED,
    STATUS_NO_MEMORY, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND,
    STATUS_SUCCESS, WIN32_AFD_HANDLE_BASE, WIN32_EVENT_HANDLE_BASE,
    WIN32_FD_HANDLE_BASE, WIN32_HANDLE_ALLOCATED_END, WIN32_KEY_HANDLE_BASE,
    WIN32_MUTANT_HANDLE_BASE, WIN32_NSI_HANDLE_BASE, WIN32_OBJ_DIR_HANDLE_BASE,
    WIN32_PROCESS_HANDLE_BASE, WIN32_SECTION_HANDLE_BASE, WIN32_SEMAPHORE_HANDLE_BASE,
    WIN32_SYMLINK_HANDLE_BASE, WIN32_THREAD_HANDLE_BASE, WindowsSystemBasicInformation,
};
use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;

use super::super::linux::error_ops::{
    LINUX_EACCES, LINUX_EBADF, LINUX_ENOENT, LINUX_ENOMEM, LINUX_ENOTDIR, LINUX_EPERM,
};
use super::super::linux::service_ops::{process_time, vfs_meta, vfs_socket};
use super::super::{
    SyscallFrame, USER_RFLAGS_FORBIDDEN_MASK, USER_RFLAGS_RESERVED_BIT_1, syscall_return_contract,
};
use super::api::WineNtApi;
use super::dispatch::{
    affinity_error_to_ntstatus, current_processor_number, policy_status_to_ntstatus,
    stamp_windows_topology,
};
use super::{fs_object, net_socket, process_thread, registry, sync};
use crate::memory::paging;
use crate::multitask;
use crate::user::process_state::{Win32MappedView, Win32SectionObject};
use crate::user::sysops::usermem;

const STATUS_NOT_MAPPED_VIEW: u64 = 0xc000_0019;
const STATUS_FREE_VM_NOT_AT_BASE: u64 = 0xc000_009f;
const STATUS_MEMORY_NOT_ALLOCATED: u64 = 0xc000_00a0;
const WIN32_VA_FLOOR: u64 = 0x0000_0000_0010_0000;
const WIN32_DYNAMIC_VA_START: u64 = 0x0000_0001_0000_0000;
const WIN32_DYNAMIC_VA_END: u64 = 0x0000_7fff_0000_0000;
const SEC_IMAGE: u64 = 0x0100_0000;
const MAX_PE_SECTION_FILE_BYTES: usize = 32 * 1024 * 1024;
const MAX_WIN32_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_NLS_FILE_BYTES: usize = 8 * 1024 * 1024;
const FILE_EPOCH_BIAS_100NS: i64 = 116_444_736_000_000_000;
const STDIN_RAW_PSEUDO: u64 = 0xffff_ffff_ffff_fff6;
const STDOUT_RAW_PSEUDO: u64 = 0xffff_ffff_ffff_fff5;
const STDERR_RAW_PSEUDO: u64 = 0xffff_ffff_ffff_fff4;
const STDIN_32_PSEUDO: u64 = 0xffff_fff6;
const STDOUT_32_PSEUDO: u64 = 0xffff_fff5;
const STDERR_32_PSEUDO: u64 = 0xffff_fff4;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct UnicodeString64 {
    pub(super) length: u16,
    pub(super) maximum_length: u16,
    _pad: u32,
    pub(super) buffer: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct ObjectAttributes64 {
    pub(super) length: u32,
    _pad0: u32,
    pub(super) root_directory: u64,
    pub(super) object_name: u64,
    pub(super) attributes: u32,
    _pad1: u32,
    pub(super) security_descriptor: u64,
    pub(super) security_qos: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IoStatusBlock64 {
    status: u64,
    information: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MemoryBasicInformation64 {
    base_address: u64,
    allocation_base: u64,
    allocation_protect: u32,
    partition_id: u16,
    _pad0: u16,
    region_size: u64,
    state: u32,
    protect: u32,
    mem_type: u32,
    _pad1: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessBasicInformation64 {
    exit_status: u64,
    peb_base_address: u64,
    affinity_mask: u64,
    base_priority: i64,
    unique_process_id: u64,
    inherited_from_unique_process_id: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SectionImageInformation64 {
    transfer_address: u64,
    zero_bits: u32,
    _pad0: u32,
    maximum_stack_size: u64,
    committed_stack_size: u64,
    subsystem_type: u32,
    minor_subsystem_version: u16,
    major_subsystem_version: u16,
    major_os_version: u16,
    minor_os_version: u16,
    image_characteristics: u16,
    dll_characteristics: u16,
    machine: u16,
    image_contains_code: u8,
    image_flags: u8,
    loader_flags: u32,
    image_file_size: u32,
    check_sum: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SectionBasicInformation64 {
    base_address: u64,
    attributes: u32,
    _pad: u32,
    size: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileStandardInformation64 {
    allocation_size: i64,
    end_of_file: i64,
    number_of_links: u32,
    delete_pending: u8,
    directory: u8,
    _pad: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileBasicInformation64 {
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    file_attributes: u32,
    _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileNetworkOpenInformation64 {
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    allocation_size: i64,
    end_of_file: i64,
    file_attributes: u32,
    _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct LdrDataTableEntryLite64 {
    pub(super) dll_base: u64,
    pub(super) entry_point: u64,
    pub(super) size_of_image: u32,
    pub(super) reserved: u32,
    pub(super) full_dll_name_w: u64,
    pub(super) base_dll_name_w: u64,
    pub(super) full_dll_name_a: u64,
    pub(super) base_dll_name_a: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SystemTimeOfDayInformation64 {
    boot_time: i64,
    current_time: i64,
    time_zone_bias: i64,
    time_zone_id: u32,
    reserved: u32,
    boot_time_bias: u64,
    sleep_time_bias: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SystemLogicalProcessorInfoExHeader {
    relationship: u32,
    size: u32,
}

rustos_user_abi::user_wire!(IoStatusBlock64 { status: u64, information: u64 });
rustos_user_abi::user_wire!(MemoryBasicInformation64 {
    base_address: u64,
    allocation_base: u64,
    allocation_protect: u32,
    partition_id: u16,
    _pad0: u16,
    region_size: u64,
    state: u32,
    protect: u32,
    mem_type: u32,
    _pad1: u32,
});
rustos_user_abi::user_wire!(ProcessBasicInformation64 {
    exit_status: u64,
    peb_base_address: u64,
    affinity_mask: u64,
    base_priority: i64,
    unique_process_id: u64,
    inherited_from_unique_process_id: u64,
});
rustos_user_abi::user_wire!(SectionImageInformation64 {
    transfer_address: u64,
    zero_bits: u32,
    _pad0: u32,
    maximum_stack_size: u64,
    committed_stack_size: u64,
    subsystem_type: u32,
    minor_subsystem_version: u16,
    major_subsystem_version: u16,
    major_os_version: u16,
    minor_os_version: u16,
    image_characteristics: u16,
    dll_characteristics: u16,
    machine: u16,
    image_contains_code: u8,
    image_flags: u8,
    loader_flags: u32,
    image_file_size: u32,
    check_sum: u32,
});
rustos_user_abi::user_wire!(SectionBasicInformation64 {
    base_address: u64,
    attributes: u32,
    _pad: u32,
    size: i64,
});
rustos_user_abi::user_wire!(FileStandardInformation64 {
    allocation_size: i64,
    end_of_file: i64,
    number_of_links: u32,
    delete_pending: u8,
    directory: u8,
    _pad: u16,
});
rustos_user_abi::user_wire!(FileBasicInformation64 {
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    file_attributes: u32,
    _pad: u32,
});
rustos_user_abi::user_wire!(FileNetworkOpenInformation64 {
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    allocation_size: i64,
    end_of_file: i64,
    file_attributes: u32,
    _pad: u32,
});
rustos_user_abi::user_wire!(SystemTimeOfDayInformation64 {
    boot_time: i64,
    current_time: i64,
    time_zone_bias: i64,
    time_zone_id: u32,
    reserved: u32,
    boot_time_bias: u64,
    sleep_time_bias: u64,
});

pub(super) fn dispatch_wine_nt_syscall(frame: &mut SyscallFrame, api: WineNtApi) -> u64 {
    let Some(current) = multitask::current_user_snapshot() else {
        return STATUS_INVALID_PARAMETER;
    };
    let (arg0, arg1, arg2, arg3) = (frame.r10, frame.rdx, frame.r8, frame.r9);
    let mut request = Win32SyscallOffloadRequest {
        version: WIN32_SYSCALL_OFFLOAD_ABI_VERSION,
        op: offload_op_for(api),
        arg0,
        arg1,
        arg2,
        arg3,
        arg4: 0,
        arg5: 0,
        pid: current.process_id(),
        tid: current.thread_id(),
        session_handle: current.console_session().raw(),
        ..Win32SyscallOffloadRequest::default()
    };

    match api {
        WineNtApi::NtQuerySystemInformation => {
            let online_mask = kernel_hal::api::cpu::admitted_online_mask();
            stamp_windows_topology(&mut request, online_mask);
            if arg0 == 0 {
                // Keep exact formal topology check path for SystemBasicInformation.
                if let Err(err) = admit_syscalld(&request) {
                    return err;
                }
                return nt_query_system_information(arg0, arg1, arg2, arg3, online_mask);
            }
            request.op = SYSCALL_OFFLOAD_OP_WIN32_QUERY_LOCALE;
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_query_system_information(arg0, arg1, arg2, arg3, online_mask)
        }
        WineNtApi::NtGetCurrentProcessorNumber => {
            let online_mask = kernel_hal::api::cpu::admitted_online_mask();
            request.arg0 = 0;
            request.arg1 = 0;
            request.arg2 = 0;
            request.arg3 = 0;
            stamp_windows_topology(&mut request, online_mask);
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            current_processor_number(
                kernel_hal::api::cpu::admitted_online_mask() | request.arg4,
                nucleus_core::util::lockdep::current_cpu_index(),
            )
        }
        WineNtApi::NtAllocateVirtualMemory | WineNtApi::NtAllocateVirtualMemoryEx => {
            nt_allocate_virtual_memory(frame, api, &current)
        }
        WineNtApi::NtFreeVirtualMemory => nt_free_virtual_memory(frame, &current),
        WineNtApi::NtProtectVirtualMemory => nt_protect_virtual_memory(frame, &current),
        WineNtApi::NtQueryVirtualMemory => {
            request.arg0 = arg1;
            request.arg1 = arg3;
            request.arg2 = read_stack_arg(frame, 4).unwrap_or(0);
            request.arg3 = arg2;
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_query_virtual_memory(frame)
        }
        WineNtApi::NtFlushVirtualMemory | WineNtApi::NtFlushInstructionCache => {
            let _ = admit_syscalld(&request);
            STATUS_SUCCESS
        }
        WineNtApi::NtOpenFile | WineNtApi::NtCreateFile => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_open_or_create_file(frame, api == WineNtApi::NtCreateFile)
        }
        WineNtApi::NtReadFile | WineNtApi::NtWriteFile => {
            let is_write = api == WineNtApi::NtWriteFile;
            let len = (read_stack_arg(frame, 6).unwrap_or(0) as u32) as u64;
            request.arg1 = read_stack_arg(frame, 5).unwrap_or(0);
            request.arg2 = len;
            request.arg4 = 0;
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_read_or_write_file(frame, is_write)
        }
        WineNtApi::NtClose => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_close(arg0)
        }
        WineNtApi::NtDuplicateObject => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_duplicate_object(frame)
        }
        WineNtApi::NtQueryInformationFile => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_query_information_file(frame)
        }
        WineNtApi::NtSetInformationFile => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_set_information_file(frame)
        }
        WineNtApi::NtQueryVolumeInformationFile => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_query_volume_information_file(frame)
        }
        WineNtApi::NtQueryAttributesFile | WineNtApi::NtQueryFullAttributesFile => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_query_attributes_file(arg0, arg1, api == WineNtApi::NtQueryFullAttributesFile)
        }
        WineNtApi::NtQueryDirectoryFile => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            fs_object::nt_query_directory_file(frame)
        }
        WineNtApi::NtFlushBuffersFile => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_flush_buffers_file(frame)
        }
        WineNtApi::NtCreateNamedPipeFile => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            fs_object::nt_create_named_pipe_file(frame)
        }
        WineNtApi::NtDeviceIoControlFile => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            net_socket::nt_device_io_control_file(frame)
        }
        WineNtApi::NtFsControlFile => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            fs_object::nt_fs_control_file(frame)
        }
        WineNtApi::NtCreateSection | WineNtApi::NtCreateSectionEx => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_create_section(frame)
        }
        WineNtApi::NtOpenSection => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            fs_object::nt_open_section(frame)
        }
        WineNtApi::NtQuerySection => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_query_section(frame)
        }
        WineNtApi::NtMapViewOfSection | WineNtApi::NtMapViewOfSectionEx => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_map_view_of_section(frame, api == WineNtApi::NtMapViewOfSectionEx)
        }
        WineNtApi::NtUnmapViewOfSection | WineNtApi::NtUnmapViewOfSectionEx => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_unmap_view_of_section(arg0, arg1)
        }
        WineNtApi::NtAreMappedFilesTheSame => {
            let _ = admit_syscalld(&request);
            nt_are_mapped_files_the_same(arg0, arg1)
        }
        WineNtApi::NtInitializeNlsFiles => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_initialize_nls_files(arg0, arg1, arg2)
        }
        WineNtApi::NtGetNlsSectionPtr => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_get_nls_section_ptr(frame)
        }
        WineNtApi::NtQueryInformationProcess => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_query_information_process(frame)
        }
        WineNtApi::NtSetInformationProcess => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_set_information_process(arg0, arg1, arg2, arg3)
        }
        WineNtApi::NtQueryInformationThread => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_query_information_thread(frame)
        }
        WineNtApi::NtSetInformationThread => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_set_information_thread(arg0, arg1, arg2, arg3)
        }
        WineNtApi::NtTerminateProcess => {
            let _ = admit_syscalld(&request);
            nt_terminate_process(arg0, arg1)
        }
        WineNtApi::NtTerminateThread => {
            let _ = admit_syscalld(&request);
            nt_terminate_thread(arg0, arg1)
        }
        WineNtApi::NtDelayExecution => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_delay_execution(arg0, arg1)
        }
        WineNtApi::NtYieldExecution => {
            let _ = admit_syscalld(&request);
            multitask::request_yield_reschedule();
            STATUS_SUCCESS
        }
        WineNtApi::NtQuerySystemTime => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            write_user_i64(arg0, current_filetime_100ns())
        }
        WineNtApi::NtQueryPerformanceCounter => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_query_performance_counter(arg0, arg1)
        }
        WineNtApi::NtQueryDefaultLocale => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            write_user_u32_checked(arg1, 0x0409)
        }
        WineNtApi::NtQueryDefaultUILanguage | WineNtApi::NtQueryInstallUILanguage => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            let _ = usermem::write_current_user_bytes(arg0, &0x0409_u16.to_le_bytes());
            STATUS_SUCCESS
        }
        WineNtApi::NtSetDefaultLocale | WineNtApi::NtSetDefaultUILanguage => {
            let _ = admit_syscalld(&request);
            STATUS_NOT_IMPLEMENTED
        }
        WineNtApi::NtContinue | WineNtApi::NtContinueEx => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            nt_continue(frame, arg0)
        }
        WineNtApi::NtCreateEvent | WineNtApi::NtOpenEvent => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_create_event(frame, api == WineNtApi::NtCreateEvent)
        }
        WineNtApi::NtSetEvent => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_modify_event(arg0, arg1, true, false)
        }
        WineNtApi::NtResetEvent | WineNtApi::NtClearEvent => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_modify_event(arg0, arg1, false, false)
        }
        WineNtApi::NtPulseEvent => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_modify_event(arg0, arg1, true, true)
        }
        WineNtApi::NtQueryEvent => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_query_event(arg0, arg1, arg2, arg3)
        }
        WineNtApi::NtCreateMutant | WineNtApi::NtOpenMutant => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_create_or_open_mutant(frame, api == WineNtApi::NtCreateMutant)
        }
        WineNtApi::NtReleaseMutant => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_release_mutant(arg0, arg1)
        }
        WineNtApi::NtQueryMutant => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_query_mutant(frame)
        }
        WineNtApi::NtCreateSemaphore | WineNtApi::NtOpenSemaphore => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_create_or_open_semaphore(frame, api == WineNtApi::NtCreateSemaphore)
        }
        WineNtApi::NtReleaseSemaphore => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_release_semaphore(arg0, (arg1 as u32) as i32, arg2)
        }
        WineNtApi::NtQuerySemaphore => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_query_semaphore(frame)
        }
        WineNtApi::NtWaitForSingleObject => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_wait_for_single_object(arg0, arg2)
        }
        WineNtApi::NtWaitForMultipleObjects => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            let timeout_ptr = read_stack_arg(frame, 4).unwrap_or(0);
            sync::nt_wait_for_multiple_objects(
                (arg0 as u32) as u64,
                arg1,
                (arg2 as u32) as u64,
                timeout_ptr,
            )
        }
        WineNtApi::NtWaitForAlertByThreadId => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_wait_for_alert_by_thread_id(arg0, arg1)
        }
        WineNtApi::NtAlertThreadByThreadId => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_alert_thread_by_thread_id(arg0)
        }
        WineNtApi::NtTestAlert => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_test_alert()
        }
        WineNtApi::NtOpenKeyedEvent => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_open_keyed_event(arg0)
        }
        WineNtApi::NtWaitForKeyedEvent => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_keyed_event_op(arg1, arg3, false)
        }
        WineNtApi::NtReleaseKeyedEvent => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            sync::nt_keyed_event_op(arg1, arg3, true)
        }
        WineNtApi::NtCreateThreadEx => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            process_thread::nt_create_thread_ex(frame)
        }
        WineNtApi::NtCreateThread => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            process_thread::nt_create_thread(frame)
        }
        WineNtApi::NtResumeThread => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            process_thread::nt_resume_thread(arg0, arg1)
        }
        WineNtApi::NtSuspendThread => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            process_thread::nt_suspend_thread(arg0, arg1)
        }
        WineNtApi::NtOpenThread => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            process_thread::nt_open_thread(frame)
        }
        WineNtApi::NtOpenProcess => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            process_thread::nt_open_process(frame)
        }
        WineNtApi::NtReadVirtualMemory | WineNtApi::NtWriteVirtualMemory => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            process_thread::nt_read_or_write_virtual_memory(
                frame,
                api == WineNtApi::NtWriteVirtualMemory,
            )
        }
        WineNtApi::NtGetContextThread => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            process_thread::nt_get_context_thread(frame, arg0, arg1)
        }
        WineNtApi::NtSetContextThread => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            process_thread::nt_set_context_thread(frame, arg0, arg1)
        }
        WineNtApi::NtRaiseException => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            process_thread::nt_raise_exception(frame, arg0, arg1, arg2)
        }
        WineNtApi::NtOpenKey | WineNtApi::NtOpenKeyEx | WineNtApi::NtCreateKey => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            registry::nt_open_or_create_key(frame, api == WineNtApi::NtCreateKey)
        }
        WineNtApi::NtSetValueKey => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            registry::nt_set_value_key(frame)
        }
        WineNtApi::NtQueryValueKey => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            registry::nt_query_value_key(frame)
        }
        WineNtApi::NtQueryKey => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            registry::nt_query_key(frame)
        }
        WineNtApi::NtEnumerateKey => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            registry::nt_enumerate_key(frame)
        }
        WineNtApi::NtEnumerateValueKey => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            registry::nt_enumerate_value_key(frame)
        }
        WineNtApi::NtDeleteKey => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            registry::nt_delete_key(arg0)
        }
        WineNtApi::NtDeleteValueKey => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            registry::nt_delete_value_key(arg0, arg1)
        }
        WineNtApi::NtFlushKey => {
            let _ = admit_syscalld(&request);
            STATUS_SUCCESS
        }
        WineNtApi::NtOpenDirectoryObject | WineNtApi::NtCreateDirectoryObject => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            registry::nt_open_or_create_directory_object(
                frame,
                api == WineNtApi::NtCreateDirectoryObject,
            )
        }
        WineNtApi::NtQueryDirectoryObject => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            registry::nt_query_directory_object(frame)
        }
        WineNtApi::NtOpenSymbolicLinkObject => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            registry::nt_open_symbolic_link_object(frame)
        }
        WineNtApi::NtQuerySymbolicLinkObject => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            registry::nt_query_symbolic_link_object(arg0, arg1, arg2)
        }
        WineNtApi::NtQueryObject => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            fs_object::nt_query_object(frame)
        }
        WineNtApi::NtSetInformationObject => {
            let _ = admit_syscalld(&request);
            STATUS_SUCCESS
        }
        WineNtApi::NtAllocateLocallyUniqueId => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            process_thread::nt_allocate_locally_unique_id(arg0)
        }
        WineNtApi::NtOpenProcessToken
        | WineNtApi::NtOpenProcessTokenEx
        | WineNtApi::NtOpenThreadToken
        | WineNtApi::NtOpenThreadTokenEx => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            process_thread::nt_open_token(frame)
        }
        WineNtApi::NtQueryInformationToken => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            process_thread::nt_query_information_token(frame)
        }
        WineNtApi::NtQuerySecurityObject => {
            if let Err(err) = admit_syscalld(&request) {
                return err;
            }
            process_thread::nt_query_security_object(frame)
        }
    }
}

fn offload_op_for(api: WineNtApi) -> u16 {
    match api {
        WineNtApi::NtAllocateVirtualMemory | WineNtApi::NtAllocateVirtualMemoryEx => {
            SYSCALL_OFFLOAD_OP_WIN32_ALLOC_VIRTUAL_MEMORY
        }
        WineNtApi::NtFreeVirtualMemory => SYSCALL_OFFLOAD_OP_WIN32_FREE_VIRTUAL_MEMORY,
        WineNtApi::NtProtectVirtualMemory => SYSCALL_OFFLOAD_OP_WIN32_PROTECT_VIRTUAL_MEMORY,
        WineNtApi::NtQueryVirtualMemory
        | WineNtApi::NtReadVirtualMemory
        | WineNtApi::NtWriteVirtualMemory
        | WineNtApi::NtFlushVirtualMemory
        | WineNtApi::NtFlushInstructionCache => SYSCALL_OFFLOAD_OP_WIN32_QUERY_VIRTUAL_MEMORY,
        WineNtApi::NtOpenFile
        | WineNtApi::NtCreateFile
        | WineNtApi::NtCreateNamedPipeFile
        | WineNtApi::NtQueryAttributesFile
        | WineNtApi::NtQueryFullAttributesFile => SYSCALL_OFFLOAD_OP_WIN32_OPEN_FILE,
        WineNtApi::NtReadFile => SYSCALL_OFFLOAD_OP_WIN32_READ_FILE,
        WineNtApi::NtWriteFile => SYSCALL_OFFLOAD_OP_WIN32_WRITE_FILE,
        WineNtApi::NtClose => SYSCALL_OFFLOAD_OP_WIN32_CLOSE,
        WineNtApi::NtDuplicateObject => SYSCALL_OFFLOAD_OP_WIN32_DUPLICATE_OBJECT,
        WineNtApi::NtQueryInformationFile
        | WineNtApi::NtQueryVolumeInformationFile
        | WineNtApi::NtQueryDirectoryFile => SYSCALL_OFFLOAD_OP_WIN32_QUERY_FILE,
        WineNtApi::NtSetInformationFile | WineNtApi::NtFlushBuffersFile => {
            SYSCALL_OFFLOAD_OP_WIN32_SET_FILE
        }
        WineNtApi::NtDeviceIoControlFile | WineNtApi::NtFsControlFile => {
            SYSCALL_OFFLOAD_OP_WIN32_DEVICE_IOCTL
        }
        WineNtApi::NtCreateSection
        | WineNtApi::NtCreateSectionEx
        | WineNtApi::NtOpenSection
        | WineNtApi::NtQuerySection => SYSCALL_OFFLOAD_OP_WIN32_CREATE_SECTION,
        WineNtApi::NtMapViewOfSection | WineNtApi::NtMapViewOfSectionEx => {
            SYSCALL_OFFLOAD_OP_WIN32_MAP_VIEW
        }
        WineNtApi::NtUnmapViewOfSection
        | WineNtApi::NtUnmapViewOfSectionEx
        | WineNtApi::NtAreMappedFilesTheSame => SYSCALL_OFFLOAD_OP_WIN32_UNMAP_VIEW,
        WineNtApi::NtInitializeNlsFiles => SYSCALL_OFFLOAD_OP_WIN32_INIT_NLS,
        WineNtApi::NtGetNlsSectionPtr => SYSCALL_OFFLOAD_OP_WIN32_GET_NLS_SECTION,
        WineNtApi::NtQuerySystemInformation => SYSCALL_OFFLOAD_OP_WIN32_QUERY_SYSTEM_INFORMATION,
        WineNtApi::NtGetCurrentProcessorNumber => {
            SYSCALL_OFFLOAD_OP_WIN32_GET_CURRENT_PROCESSOR_NUMBER
        }
        WineNtApi::NtQueryInformationProcess | WineNtApi::NtOpenProcess => {
            SYSCALL_OFFLOAD_OP_WIN32_QUERY_INFO_PROCESS
        }
        WineNtApi::NtSetInformationProcess => SYSCALL_OFFLOAD_OP_WIN32_SET_INFO_PROCESS,
        WineNtApi::NtQueryInformationThread
        | WineNtApi::NtOpenThread
        | WineNtApi::NtGetContextThread => SYSCALL_OFFLOAD_OP_WIN32_QUERY_INFO_THREAD,
        WineNtApi::NtSetInformationThread
        | WineNtApi::NtCreateThread
        | WineNtApi::NtCreateThreadEx
        | WineNtApi::NtResumeThread
        | WineNtApi::NtSuspendThread
        | WineNtApi::NtSetContextThread => SYSCALL_OFFLOAD_OP_WIN32_SET_INFO_THREAD,
        WineNtApi::NtTerminateProcess => SYSCALL_OFFLOAD_OP_WIN32_EXIT_PROCESS,
        WineNtApi::NtTerminateThread => SYSCALL_OFFLOAD_OP_WIN32_TERMINATE_THREAD,
        WineNtApi::NtDelayExecution => SYSCALL_OFFLOAD_OP_WIN32_DELAY_EXECUTION,
        WineNtApi::NtYieldExecution => SYSCALL_OFFLOAD_OP_WIN32_YIELD,
        WineNtApi::NtQuerySystemTime | WineNtApi::NtQueryPerformanceCounter => {
            SYSCALL_OFFLOAD_OP_WIN32_QUERY_TIME
        }
        WineNtApi::NtQueryDefaultLocale
        | WineNtApi::NtQueryDefaultUILanguage
        | WineNtApi::NtQueryInstallUILanguage
        | WineNtApi::NtSetDefaultLocale
        | WineNtApi::NtSetDefaultUILanguage => SYSCALL_OFFLOAD_OP_WIN32_QUERY_LOCALE,
        WineNtApi::NtContinue | WineNtApi::NtContinueEx | WineNtApi::NtRaiseException => {
            SYSCALL_OFFLOAD_OP_WIN32_CONTINUE
        }
        WineNtApi::NtCreateEvent
        | WineNtApi::NtOpenEvent
        | WineNtApi::NtSetEvent
        | WineNtApi::NtResetEvent
        | WineNtApi::NtClearEvent
        | WineNtApi::NtPulseEvent
        | WineNtApi::NtQueryEvent
        | WineNtApi::NtCreateMutant
        | WineNtApi::NtOpenMutant
        | WineNtApi::NtReleaseMutant
        | WineNtApi::NtQueryMutant
        | WineNtApi::NtCreateSemaphore
        | WineNtApi::NtOpenSemaphore
        | WineNtApi::NtReleaseSemaphore
        | WineNtApi::NtQuerySemaphore => SYSCALL_OFFLOAD_OP_WIN32_CREATE_EVENT,
        WineNtApi::NtWaitForSingleObject
        | WineNtApi::NtWaitForMultipleObjects
        | WineNtApi::NtWaitForAlertByThreadId
        | WineNtApi::NtAlertThreadByThreadId
        | WineNtApi::NtWaitForKeyedEvent
        | WineNtApi::NtReleaseKeyedEvent
        | WineNtApi::NtTestAlert
        | WineNtApi::NtOpenKeyedEvent => SYSCALL_OFFLOAD_OP_WIN32_WAIT_OBJECT,
        WineNtApi::NtOpenKey
        | WineNtApi::NtOpenKeyEx
        | WineNtApi::NtCreateKey
        | WineNtApi::NtSetValueKey
        | WineNtApi::NtQueryValueKey
        | WineNtApi::NtQueryKey
        | WineNtApi::NtEnumerateKey
        | WineNtApi::NtEnumerateValueKey
        | WineNtApi::NtDeleteKey
        | WineNtApi::NtDeleteValueKey
        | WineNtApi::NtFlushKey
        | WineNtApi::NtOpenDirectoryObject
        | WineNtApi::NtQueryDirectoryObject
        | WineNtApi::NtOpenSymbolicLinkObject
        | WineNtApi::NtQuerySymbolicLinkObject
        | WineNtApi::NtCreateDirectoryObject
        | WineNtApi::NtQueryObject
        | WineNtApi::NtSetInformationObject
        | WineNtApi::NtAllocateLocallyUniqueId
        | WineNtApi::NtOpenProcessToken
        | WineNtApi::NtOpenProcessTokenEx
        | WineNtApi::NtOpenThreadToken
        | WineNtApi::NtOpenThreadTokenEx
        | WineNtApi::NtQueryInformationToken
        | WineNtApi::NtQuerySecurityObject => SYSCALL_OFFLOAD_OP_WIN32_OPEN_KEY,
    }
}

fn admit_syscalld(request: &Win32SyscallOffloadRequest) -> Result<u64, u64> {
    match super::super::linux::call_syscalld_raw(as_bytes(request)) {
        Ok(bytes) if bytes.len() == size_of::<Win32SyscallOffloadResponse>() => {
            let response = read_unaligned::<Win32SyscallOffloadResponse>(bytes.as_slice());
            if response.version != WIN32_SYSCALL_OFFLOAD_ABI_VERSION
                || response.op != request.op
                || response.reserved0 != 0
            {
                return Err(STATUS_INVALID_SYSTEM_SERVICE);
            }
            if response.status == 0 {
                Ok(response.result)
            } else {
                Err(policy_status_to_ntstatus(response.status))
            }
        }
        _ => Err(STATUS_INVALID_SYSTEM_SERVICE),
    }
}

pub(super) fn read_stack_arg(frame: &SyscallFrame, index: u64) -> Result<u64, u64> {
    // At `syscall` inside `call NtFoo`: `[rsp] = return RIP`, `[rsp+0x28] = arg4`, `[rsp+0x30] = arg5`.
    let offset = 8_u64
        .checked_add(index.checked_mul(8).ok_or(STATUS_INVALID_PARAMETER)?)
        .ok_or(STATUS_INVALID_PARAMETER)?;
    let addr = frame
        .user_rsp
        .checked_add(offset)
        .ok_or(STATUS_INVALID_PARAMETER)?;
    usermem::read_current_user_struct::<u64>(addr).map_err(|_| STATUS_INVALID_PARAMETER)
}

pub(super) fn read_user_u64(ptr: u64) -> Result<u64, u64> {
    if ptr == 0 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    usermem::read_current_user_struct::<u64>(ptr).map_err(|_| STATUS_INVALID_PARAMETER)
}

pub(super) fn write_user_u64(ptr: u64, val: u64) -> u64 {
    if ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    match usermem::write_current_user_bytes(ptr, &val.to_le_bytes()) {
        Ok(()) => STATUS_SUCCESS,
        Err(_) => STATUS_INVALID_PARAMETER,
    }
}

pub(super) fn write_user_i64(ptr: u64, val: i64) -> u64 {
    if ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    match usermem::write_current_user_bytes(ptr, &val.to_le_bytes()) {
        Ok(()) => STATUS_SUCCESS,
        Err(_) => STATUS_INVALID_PARAMETER,
    }
}

pub(super) fn write_user_u32_checked(ptr: u64, val: u32) -> u64 {
    if ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    match usermem::write_current_user_u32(ptr, val) {
        Ok(()) => STATUS_SUCCESS,
        Err(_) => STATUS_INVALID_PARAMETER,
    }
}

pub(super) fn write_io_status(ptr: u64, status: u64, information: u64) {
    if ptr != 0 {
        let iosb = IoStatusBlock64 {
            status,
            information,
        };
        let _ = usermem::write_current_user_struct(ptr, &iosb);
    }
}

fn align_down_page(addr: u64) -> u64 {
    addr & !(PAGE_SIZE - 1)
}

fn align_up_page(size: u64) -> Option<u64> {
    let mask = PAGE_SIZE - 1;
    Some(size.checked_add(mask)? & !mask)
}

fn protect_to_page_flags(protect: u64) -> Option<PageTableFlags> {
    let base = protect & 0xff;
    match base {
        PAGE_NOACCESS => Some(PageTableFlags::NO_EXECUTE),
        PAGE_READONLY => Some(PageTableFlags::NO_EXECUTE),
        PAGE_READWRITE | PAGE_WRITECOPY => {
            Some(PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE)
        }
        PAGE_EXECUTE | PAGE_EXECUTE_READ => Some(PageTableFlags::empty()),
        PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY => Some(PageTableFlags::WRITABLE),
        _ => None,
    }
}

fn page_flags_to_protect(flags: PageTableFlags) -> u32 {
    let writable = flags.contains(PageTableFlags::WRITABLE);
    let no_exec = flags.contains(PageTableFlags::NO_EXECUTE);
    match (writable, no_exec) {
        (true, false) => PAGE_EXECUTE_READWRITE as u32,
        (false, false) => PAGE_EXECUTE_READ as u32,
        (true, true) => PAGE_READWRITE as u32,
        (false, true) => PAGE_READONLY as u32,
    }
}

/// End of a user-supplied decommit range, or `None` if it wraps or leaves user space.
fn decommit_range_end(base: u64, size: u64) -> Option<u64> {
    base.checked_add(size)
        .filter(|end| *end <= paging::USER_SPACE_END_EXCLUSIVE)
}

fn vmas_overlap(vmas: &[Win32MappedView], start: u64, end: u64) -> bool {
    vmas.iter().any(|v| {
        let v_end = v.base.saturating_add(v.size);
        start < v_end && end > v.base
    })
}

fn find_free_win32_va(
    address_space: &paging::ProcessAddressSpace,
    vmas: &[Win32MappedView],
    min_addr: u64,
    max_addr: u64,
    len: u64,
    top_down: bool,
) -> Option<u64> {
    let step = 0x1_0000_u64; // 64 KiB Windows allocation granularity.
    let pages = usize::try_from(len / PAGE_SIZE).ok()?;
    if pages == 0 {
        return None;
    }
    if !top_down {
        let mut cursor = (min_addr.max(WIN32_VA_FLOOR) + (step - 1)) & !(step - 1);
        while let Some(end) = cursor.checked_add(len) {
            if end > max_addr {
                return None;
            }
            if !vmas_overlap(vmas, cursor, end)
                && address_space
                    .user_range_is_unmapped(VirtAddr::new(cursor), pages)
                    .unwrap_or(false)
            {
                return Some(cursor);
            }
            if let Some(conflicting) = vmas.iter().find(|v| cursor < v.base + v.size && end > v.base)
            {
                cursor = ((conflicting.base + conflicting.size) + (step - 1)) & !(step - 1);
            } else {
                cursor = cursor.checked_add(step)?;
            }
        }
        None
    } else {
        let mut cursor = (max_addr.checked_sub(len)?) & !(step - 1);
        while cursor >= min_addr.max(WIN32_VA_FLOOR) {
            let end = cursor + len;
            if !vmas_overlap(vmas, cursor, end)
                && address_space
                    .user_range_is_unmapped(VirtAddr::new(cursor), pages)
                    .unwrap_or(false)
            {
                return Some(cursor);
            }
            if cursor < step {
                break;
            }
            cursor -= step;
        }
        None
    }
}

pub(super) fn alloc_win32_zeroed_pages(
    size: u64,
    protect: u32,
    page_flags: PageTableFlags,
) -> Option<u64> {
    let map_len = align_up_page(size.max(PAGE_SIZE))?;
    let pages = usize::try_from(map_len / PAGE_SIZE).ok()?;
    let flags = page_flags & (PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE);
    multitask::with_current_user_process_state_mut(|_, _, st| {
        let base = find_free_win32_va(
            st.address_space(),
            &st.win32_objects().vmas,
            WIN32_DYNAMIC_VA_START,
            WIN32_DYNAMIC_VA_END,
            map_len,
            false,
        )?;
        st.address_space_mut()
            .map_zeroed_user_pages_at(VirtAddr::new(base), pages, flags)
            .ok()?;
        st.win32_objects_mut().vmas.push(Win32MappedView {
            base,
            size: map_len,
            allocation_base: base,
            state: MEM_COMMIT as u32,
            protect,
            mem_type: MEM_PRIVATE as u32,
        });
        Some(base)
    })
    .flatten()
}

pub(super) fn find_ntdll_export_address(
    st: &crate::user::process_state::UserProcessState,
    export_name: &str,
) -> Option<u64> {
    let rt = st.windows_runtime()?;
    if rt.loader_module_array_address == 0 || rt.loader_module_count == 0 {
        return None;
    }
    let count = rt.loader_module_count.min(32) as usize;
    let aspace = st.address_space();
    for idx in 0..count {
        let entry_va =
            rt.loader_module_array_address + (idx * size_of::<LdrDataTableEntryLite64>()) as u64;
        let mut entry_bytes = [0_u8; size_of::<LdrDataTableEntryLite64>()];
        if aspace
            .copy_from_user(VirtAddr::new(entry_va), &mut entry_bytes)
            .is_err()
        {
            continue;
        }
        let entry = read_unaligned::<LdrDataTableEntryLite64>(&entry_bytes);
        if entry.dll_base == 0 || entry.base_dll_name_a == 0 {
            continue;
        }
        let mut name_buf = [0_u8; 32];
        if aspace
            .copy_from_user(VirtAddr::new(entry.base_dll_name_a), &mut name_buf)
            .is_err()
        {
            continue;
        }
        let name_len = name_buf.iter().position(|&b| b == 0).unwrap_or(name_buf.len());
        let mod_name = core::str::from_utf8(&name_buf[..name_len]).ok()?;
        if !mod_name.eq_ignore_ascii_case("ntdll.dll") && !mod_name.eq_ignore_ascii_case("ntdll") {
            continue;
        }

        let mut dos = [0_u8; 0x40];
        aspace
            .copy_from_user(VirtAddr::new(entry.dll_base), &mut dos)
            .ok()?;
        if &dos[0..2] != b"MZ" {
            return None;
        }
        let pe_off = u32::from_le_bytes(dos[0x3c..0x40].try_into().ok()?) as u64;
        let mut export_dir_entry = [0_u8; 8];
        aspace
            .copy_from_user(VirtAddr::new(entry.dll_base + pe_off + 0x88), &mut export_dir_entry)
            .ok()?;
        let exp_rva = u32::from_le_bytes(export_dir_entry[0..4].try_into().ok()?) as u64;
        if exp_rva == 0 {
            return None;
        }
        let mut exp_hdr = [0_u8; 40];
        aspace
            .copy_from_user(VirtAddr::new(entry.dll_base + exp_rva), &mut exp_hdr)
            .ok()?;
        let num_names = u32::from_le_bytes(exp_hdr[24..28].try_into().ok()?) as usize;
        let funcs_rva = u32::from_le_bytes(exp_hdr[28..32].try_into().ok()?) as u64;
        let names_rva = u32::from_le_bytes(exp_hdr[32..36].try_into().ok()?) as u64;
        let ords_rva = u32::from_le_bytes(exp_hdr[36..40].try_into().ok()?) as u64;
        for i in 0..num_names.min(4096) {
            let mut nrva_b = [0_u8; 4];
            if aspace
                .copy_from_user(VirtAddr::new(entry.dll_base + names_rva + (i as u64) * 4), &mut nrva_b)
                .is_err()
            {
                break;
            }
            let nrva = u32::from_le_bytes(nrva_b) as u64;
            let mut fn_buf = [0_u8; 64];
            if aspace
                .copy_from_user(VirtAddr::new(entry.dll_base + nrva), &mut fn_buf)
                .is_err()
            {
                continue;
            }
            let flen = fn_buf.iter().position(|&b| b == 0).unwrap_or(fn_buf.len());
            if core::str::from_utf8(&fn_buf[..flen]).ok() == Some(export_name) {
                let mut ord_b = [0_u8; 2];
                aspace
                    .copy_from_user(
                        VirtAddr::new(entry.dll_base + ords_rva + (i as u64) * 2),
                        &mut ord_b,
                    )
                    .ok()?;
                let ord = u16::from_le_bytes(ord_b) as u64;
                let mut fn_rva_b = [0_u8; 4];
                aspace
                    .copy_from_user(
                        VirtAddr::new(entry.dll_base + funcs_rva + ord * 4),
                        &mut fn_rva_b,
                    )
                    .ok()?;
                let fn_rva = u32::from_le_bytes(fn_rva_b) as u64;
                return (fn_rva != 0).then_some(entry.dll_base + fn_rva);
            }
        }
        return None;
    }
    None
}

fn nt_allocate_virtual_memory(
    frame: &SyscallFrame,
    api: WineNtApi,
    current: &multitask::CurrentUserSnapshot,
) -> u64 {
    let (process_handle, base_ptr, region_size_ptr, alloc_type, protect) = match api {
        WineNtApi::NtAllocateVirtualMemory => {
            let Ok(alloc_type) = read_stack_arg(frame, 4) else {
                return STATUS_INVALID_PARAMETER;
            };
            let Ok(protect) = read_stack_arg(frame, 5) else {
                return STATUS_INVALID_PARAMETER;
            };
            (
                frame.r10,
                frame.rdx,
                frame.r9,
                (alloc_type as u32) as u64,
                (protect as u32) as u64,
            )
        }
        _ => {
            let Ok(alloc_type) = read_stack_arg(frame, 4) else {
                return STATUS_INVALID_PARAMETER;
            };
            let Ok(protect) = read_stack_arg(frame, 5) else {
                return STATUS_INVALID_PARAMETER;
            };
            (
                frame.r10,
                frame.rdx,
                frame.r8,
                (alloc_type as u32) as u64,
                (protect as u32) as u64,
            )
        }
    };
    if process_handle != HANDLE_CURRENT_PROCESS {
        return STATUS_INVALID_HANDLE;
    }
    let Ok(requested_base) = read_user_u64(base_ptr) else {
        return STATUS_INVALID_PARAMETER;
    };
    let Ok(requested_size) = read_user_u64(region_size_ptr) else {
        return STATUS_INVALID_PARAMETER;
    };
    if requested_size == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let top_down = (alloc_type & 0x10_0000) != 0;
    let clean_type = alloc_type & !(0x10_0000_u64 | 0x20_0000_u64);
    let request = Win32SyscallOffloadRequest {
        version: WIN32_SYSCALL_OFFLOAD_ABI_VERSION,
        op: SYSCALL_OFFLOAD_OP_WIN32_ALLOC_VIRTUAL_MEMORY,
        arg0: align_down_page(requested_base),
        arg1: requested_size,
        arg2: clean_type,
        arg3: protect & 0xff,
        pid: current.process_id(),
        tid: current.thread_id(),
        session_handle: current.console_session().raw(),
        ..Win32SyscallOffloadRequest::default()
    };
    if let Err(err) = admit_syscalld(&request) {
        return err;
    }
    let Some(page_flags) = protect_to_page_flags(protect) else {
        return STATUS_INVALID_PARAMETER;
    };
    let commit = (clean_type & MEM_COMMIT) != 0;
    let reserve = (clean_type & MEM_RESERVE) != 0;
    if !commit && !reserve {
        return STATUS_INVALID_PARAMETER;
    }

    let aligned_base = if requested_base != 0 {
        align_down_page(requested_base)
    } else {
        0
    };
    let end_raw = if requested_base != 0 {
        requested_base
            .checked_add(requested_size)
            .and_then(align_up_page)
    } else {
        align_up_page(requested_size)
    };
    let Some(end_or_len) = end_raw else {
        return STATUS_INVALID_PARAMETER;
    };
    let len = if requested_base != 0 {
        end_or_len - aligned_base
    } else {
        end_or_len
    };
    let Ok(page_count) = usize::try_from(len / PAGE_SIZE) else {
        return STATUS_NO_MEMORY;
    };

    let outcome = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        if aligned_base != 0 && !reserve && commit {
            // Committing inside an existing reserved/committed VMA or unmapped range.
            let target_end = aligned_base + len;
            let in_reserved = process_state.win32_objects().vmas.iter().find(|v| {
                aligned_base >= v.base && target_end <= v.base.saturating_add(v.size)
            }).cloned();
            if let Some(region_vma) = in_reserved {
                for page_idx in 0..page_count {
                    let page_va = VirtAddr::new(aligned_base + (page_idx as u64) * PAGE_SIZE);
                    if process_state.address_space().translate_user(page_va).is_none() {
                        process_state
                            .address_space_mut()
                            .map_zeroed_user_pages_at(page_va, 1, page_flags)
                            .map_err(|_| STATUS_NO_MEMORY)?;
                    } else {
                        process_state
                            .address_space_mut()
                            .protect_user_pages_at(page_va, 1, page_flags)
                            .map_err(|_| STATUS_NO_MEMORY)?;
                    }
                }
                let alloc_base = region_vma.allocation_base;
                if aligned_base == region_vma.base && target_end == region_vma.base + region_vma.size
                {
                    if let Some(slot) = process_state
                        .win32_objects_mut()
                        .vmas
                        .iter_mut()
                        .find(|v| v.base == aligned_base && v.size == len)
                    {
                        slot.state = MEM_COMMIT as u32;
                        slot.protect = (protect & 0xff) as u32;
                    }
                } else {
                    process_state.win32_objects_mut().vmas.push(Win32MappedView {
                        base: aligned_base,
                        size: len,
                        allocation_base: alloc_base,
                        state: MEM_COMMIT as u32,
                        protect: (protect & 0xff) as u32,
                        mem_type: MEM_PRIVATE as u32,
                    });
                }
                return Ok((aligned_base, len));
            }
        }

        let target_base = if aligned_base != 0 {
            let end = aligned_base.checked_add(len).ok_or(STATUS_INVALID_PARAMETER)?;
            if end >= paging::USER_SPACE_END_EXCLUSIVE || aligned_base < paging::USER_SPACE_BASE {
                return Err(STATUS_INVALID_PARAMETER);
            }
            if vmas_overlap(&process_state.win32_objects().vmas, aligned_base, end)
                || !process_state
                    .address_space()
                    .user_range_is_unmapped(VirtAddr::new(aligned_base), page_count)
                    .unwrap_or(false)
            {
                return Err(STATUS_NO_MEMORY);
            }
            aligned_base
        } else {
            find_free_win32_va(
                process_state.address_space(),
                &process_state.win32_objects().vmas,
                WIN32_DYNAMIC_VA_START,
                WIN32_DYNAMIC_VA_END,
                len,
                top_down,
            )
            .ok_or(STATUS_NO_MEMORY)?
        };

        if commit {
            process_state
                .address_space_mut()
                .map_zeroed_user_pages_at(VirtAddr::new(target_base), page_count, page_flags)
                .map_err(|_| STATUS_NO_MEMORY)?;
        }
        process_state.win32_objects_mut().vmas.push(Win32MappedView {
            base: target_base,
            size: len,
            allocation_base: target_base,
            state: if commit {
                MEM_COMMIT as u32
            } else {
                MEM_RESERVE as u32
            },
            protect: (protect & 0xff) as u32,
            mem_type: MEM_PRIVATE as u32,
        });
        Ok((target_base, len))
    });

    let Some(inner) = outcome else {
        return STATUS_INVALID_PARAMETER;
    };
    match inner {
        Ok((out_base, out_size)) => {
            if write_user_u64(base_ptr, out_base) != STATUS_SUCCESS
                || write_user_u64(region_size_ptr, out_size) != STATUS_SUCCESS
            {
                return STATUS_INVALID_PARAMETER;
            }
            STATUS_SUCCESS
        }
        Err(status) => status,
    }
}

fn nt_free_virtual_memory(
    frame: &SyscallFrame,
    current: &multitask::CurrentUserSnapshot,
) -> u64 {
    let (process_handle, base_ptr, region_size_ptr, free_type) = (
        frame.r10,
        frame.rdx,
        frame.r8,
        (frame.r9 as u32) as u64,
    );
    if process_handle != HANDLE_CURRENT_PROCESS {
        return STATUS_INVALID_HANDLE;
    }
    let Ok(base) = read_user_u64(base_ptr) else {
        return STATUS_INVALID_PARAMETER;
    };
    let Ok(size) = read_user_u64(region_size_ptr) else {
        return STATUS_INVALID_PARAMETER;
    };
    let request = Win32SyscallOffloadRequest {
        version: WIN32_SYSCALL_OFFLOAD_ABI_VERSION,
        op: SYSCALL_OFFLOAD_OP_WIN32_FREE_VIRTUAL_MEMORY,
        arg0: base,
        arg1: size,
        arg2: free_type,
        pid: current.process_id(),
        tid: current.thread_id(),
        session_handle: current.console_session().raw(),
        ..Win32SyscallOffloadRequest::default()
    };
    if let Err(err) = admit_syscalld(&request) {
        return err;
    }

    let aligned_base = align_down_page(base);
    let outcome = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        if free_type == MEM_RELEASE {
            if size != 0 {
                return Err(STATUS_INVALID_PARAMETER);
            }
            // Windows releases a whole allocation and only by its exact
            // allocation base; a base inside an allocation is rejected rather
            // than releasing a sub-view.
            let matching: Vec<Win32MappedView> = process_state
                .win32_objects()
                .vmas
                .iter()
                .filter(|v| v.allocation_base == aligned_base)
                .cloned()
                .collect();
            let release_size = if let Some(first) = matching.first() {
                let mut total = first.size;
                for m in &matching {
                    let pages = usize::try_from(m.size / PAGE_SIZE).unwrap_or(0);
                    if pages > 0 {
                        let _ = process_state
                            .address_space_mut()
                            .unmap_present_user_pages_at(VirtAddr::new(m.base), pages);
                    }
                    total = total.max((m.base + m.size).saturating_sub(aligned_base));
                }
                process_state
                    .win32_objects_mut()
                    .vmas
                    .retain(|v| v.allocation_base != aligned_base);
                total
            } else if process_state
                .win32_objects()
                .vmas
                .iter()
                .any(|v| aligned_base >= v.base && aligned_base < v.base.saturating_add(v.size))
            {
                return Err(STATUS_FREE_VM_NOT_AT_BASE);
            } else if process_state
                .windows_runtime()
                .is_none_or(|rt| rt.process_parameters_address != aligned_base)
            {
                // Pages outside every Win32 VMA belong to the loader (image,
                // PEB/TEB, stacks); only the loader-mapped ProcessParameters
                // block may be released without a VMA record.
                return Err(STATUS_MEMORY_NOT_ALLOCATED);
            } else {
                let mut pages = 0usize;
                while process_state
                    .address_space()
                    .translate_user(VirtAddr::new(aligned_base + (pages as u64) * PAGE_SIZE))
                    .is_some()
                {
                    pages += 1;
                    if pages >= 1024 {
                        break;
                    }
                }
                if pages == 0 {
                    return Err(STATUS_MEMORY_NOT_ALLOCATED);
                }
                let _ = process_state
                    .address_space_mut()
                    .unmap_present_user_pages_at(VirtAddr::new(aligned_base), pages);
                (pages as u64) * PAGE_SIZE
            };
            Ok((aligned_base, release_size))
        } else if free_type == MEM_DECOMMIT {
            let aligned_size = align_up_page(size.max(PAGE_SIZE)).ok_or(STATUS_INVALID_PARAMETER)?;
            let pages = usize::try_from(aligned_size / PAGE_SIZE).map_err(|_| STATUS_INVALID_PARAMETER)?;
            // `base`/`size` are user supplied: an unchecked sum would wrap (or
            // panic in debug builds) and decommit/relabel the wrong range.
            let decommit_end =
                decommit_range_end(aligned_base, aligned_size).ok_or(STATUS_INVALID_PARAMETER)?;
            let _ = process_state
                .address_space_mut()
                .unmap_present_user_pages_at(VirtAddr::new(aligned_base), pages);
            for v in &mut process_state.win32_objects_mut().vmas {
                if v.base >= aligned_base && v.base.saturating_add(v.size) <= decommit_end {
                    v.state = MEM_RESERVE as u32;
                }
            }
            Ok((aligned_base, aligned_size))
        } else {
            Err(STATUS_INVALID_PARAMETER)
        }
    });
    let Some(inner) = outcome else {
        return STATUS_INVALID_PARAMETER;
    };
    match inner {
        Ok((out_base, out_size)) => {
            let _ = write_user_u64(base_ptr, out_base);
            let _ = write_user_u64(region_size_ptr, out_size);
            STATUS_SUCCESS
        }
        Err(status) => status,
    }
}

fn nt_protect_virtual_memory(
    frame: &SyscallFrame,
    current: &multitask::CurrentUserSnapshot,
) -> u64 {
    let (process_handle, base_ptr, size_ptr, new_protect) = (
        frame.r10,
        frame.rdx,
        frame.r8,
        (frame.r9 as u32) as u64,
    );
    let Ok(old_protect_ptr) = read_stack_arg(frame, 4) else {
        return STATUS_INVALID_PARAMETER;
    };
    if process_handle != HANDLE_CURRENT_PROCESS {
        return STATUS_INVALID_HANDLE;
    }
    let Ok(base) = read_user_u64(base_ptr) else {
        return STATUS_INVALID_PARAMETER;
    };
    let Ok(size) = read_user_u64(size_ptr) else {
        return STATUS_INVALID_PARAMETER;
    };
    let request = Win32SyscallOffloadRequest {
        version: WIN32_SYSCALL_OFFLOAD_ABI_VERSION,
        op: SYSCALL_OFFLOAD_OP_WIN32_PROTECT_VIRTUAL_MEMORY,
        arg0: base,
        arg1: size,
        arg2: new_protect & 0xff,
        arg3: old_protect_ptr,
        pid: current.process_id(),
        tid: current.thread_id(),
        session_handle: current.console_session().raw(),
        ..Win32SyscallOffloadRequest::default()
    };
    if let Err(err) = admit_syscalld(&request) {
        return err;
    }
    let Some(flags) = protect_to_page_flags(new_protect) else {
        return STATUS_INVALID_PARAMETER;
    };
    let aligned_base = align_down_page(base);
    let Some(end_aligned) = base.checked_add(size).and_then(align_up_page) else {
        return STATUS_INVALID_PARAMETER;
    };
    let aligned_size = end_aligned.saturating_sub(aligned_base);
    if aligned_size == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let Ok(pages) = usize::try_from(aligned_size / PAGE_SIZE) else {
        return STATUS_INVALID_PARAMETER;
    };

    let outcome = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let prev_protect = process_state
            .address_space()
            .translate_user_with_flags(VirtAddr::new(aligned_base))
            .map(|(_, f)| page_flags_to_protect(f))
            .unwrap_or(PAGE_READWRITE as u32);
        process_state
            .address_space_mut()
            .protect_user_pages_at(VirtAddr::new(aligned_base), pages, flags)
            .map_err(|_| STATUS_INVALID_PARAMETER)?;
        for v in &mut process_state.win32_objects_mut().vmas {
            if aligned_base >= v.base && aligned_base < v.base.saturating_add(v.size) {
                v.protect = (new_protect & 0xff) as u32;
            }
        }
        Ok(prev_protect)
    });
    let Some(inner) = outcome else {
        return STATUS_INVALID_PARAMETER;
    };
    match inner {
        Ok(prev) => {
            let _ = write_user_u64(base_ptr, aligned_base);
            let _ = write_user_u64(size_ptr, aligned_size);
            if old_protect_ptr != 0 {
                let _ = write_user_u32_checked(old_protect_ptr, prev);
            }
            STATUS_SUCCESS
        }
        Err(status) => status,
    }
}

fn nt_query_virtual_memory(frame: &SyscallFrame) -> u64 {
    let (process_handle, query_addr, info_class, buf_ptr) = (
        frame.r10,
        frame.rdx,
        (frame.r8 as u32) as u64,
        frame.r9,
    );
    let buf_len = read_stack_arg(frame, 4).unwrap_or(0);
    let ret_len_ptr = read_stack_arg(frame, 5).unwrap_or(0);
    if process_handle != HANDLE_CURRENT_PROCESS {
        return STATUS_INVALID_HANDLE;
    }
    if info_class != 0 {
        return STATUS_INVALID_INFO_CLASS;
    }
    let mbi_size = size_of::<MemoryBasicInformation64>() as u64;
    if buf_len < mbi_size {
        if ret_len_ptr != 0 {
            let _ = write_user_u64(ret_len_ptr, mbi_size);
        }
        return STATUS_INFO_LENGTH_MISMATCH;
    }

    let page_addr = align_down_page(query_addr);
    let mbi = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        if let Some(vma) = process_state
            .win32_objects()
            .vmas
            .iter()
            .find(|v| page_addr >= v.base && page_addr < v.base.saturating_add(v.size))
            .cloned()
        {
            let offset = page_addr - vma.base;
            return MemoryBasicInformation64 {
                base_address: page_addr,
                allocation_base: vma.allocation_base,
                allocation_protect: vma.protect,
                partition_id: 0,
                _pad0: 0,
                region_size: vma.size - offset,
                state: vma.state,
                protect: vma.protect,
                mem_type: vma.mem_type,
                _pad1: 0,
            };
        }
        if let Some(rt) = process_state.windows_runtime() {
            // Check preloaded module table (main.exe + ntdll.dll) written by loaderd.
            if rt.loader_module_array_address != 0 && rt.loader_module_count > 0 {
                let count = rt.loader_module_count.min(32) as usize;
                for idx in 0..count {
                    let entry_va = rt.loader_module_array_address
                        + (idx * size_of::<LdrDataTableEntryLite64>()) as u64;
                    let mut entry_bytes = [0_u8; size_of::<LdrDataTableEntryLite64>()];
                    if process_state
                        .address_space()
                        .copy_from_user(VirtAddr::new(entry_va), &mut entry_bytes)
                        .is_ok()
                    {
                        let entry = read_unaligned::<LdrDataTableEntryLite64>(&entry_bytes);
                        let img_end = entry.dll_base.saturating_add(entry.size_of_image as u64);
                        if page_addr >= entry.dll_base && page_addr < img_end {
                            let protect = process_state
                                .address_space()
                                .translate_user_with_flags(VirtAddr::new(page_addr))
                                .map(|(_, f)| page_flags_to_protect(f))
                                .unwrap_or(PAGE_EXECUTE_READ as u32);
                            return MemoryBasicInformation64 {
                                base_address: page_addr,
                                allocation_base: entry.dll_base,
                                allocation_protect: PAGE_EXECUTE_READ as u32,
                                partition_id: 0,
                                _pad0: 0,
                                region_size: align_up_page(img_end).unwrap_or(img_end) - page_addr,
                                state: MEM_COMMIT as u32,
                                protect,
                                mem_type: MEM_IMAGE as u32,
                                _pad1: 0,
                            };
                        }
                    }
                }
            }
            // Check TEB stack range (+0x08 StackBase, +0x10 StackLimit, +0x1478 DeallocationStack).
            if rt.teb_address != 0 {
                let mut stack_buf = [0_u8; 24];
                let mut dealloc_buf = [0_u8; 8];
                if process_state
                    .address_space()
                    .copy_from_user(VirtAddr::new(rt.teb_address), &mut stack_buf)
                    .is_ok()
                {
                    let stack_base = u64::from_le_bytes(stack_buf[8..16].try_into().unwrap());
                    let stack_limit = u64::from_le_bytes(stack_buf[16..24].try_into().unwrap());
                    let dealloc_stack = if process_state
                        .address_space()
                        .copy_from_user(VirtAddr::new(rt.teb_address + 0x1478), &mut dealloc_buf)
                        .is_ok()
                    {
                        u64::from_le_bytes(dealloc_buf)
                    } else {
                        stack_limit
                    };
                    if page_addr >= dealloc_stack && page_addr < stack_base {
                        return MemoryBasicInformation64 {
                            base_address: page_addr,
                            allocation_base: dealloc_stack,
                            allocation_protect: PAGE_READWRITE as u32,
                            partition_id: 0,
                            _pad0: 0,
                            region_size: stack_base - page_addr,
                            state: MEM_COMMIT as u32,
                            protect: PAGE_READWRITE as u32,
                            mem_type: MEM_PRIVATE as u32,
                            _pad1: 0,
                        };
                    }
                }
            }
        }
        if let Some((_, flags)) = process_state
            .address_space()
            .translate_user_with_flags(VirtAddr::new(page_addr))
        {
            let prot = page_flags_to_protect(flags);
            let mut end = page_addr + PAGE_SIZE;
            for _ in 0..256 {
                if let Some((_, f2)) = process_state
                    .address_space()
                    .translate_user_with_flags(VirtAddr::new(end))
                    && f2 == flags
                {
                    end += PAGE_SIZE;
                } else {
                    break;
                }
            }
            return MemoryBasicInformation64 {
                base_address: page_addr,
                allocation_base: page_addr,
                allocation_protect: prot,
                partition_id: 0,
                _pad0: 0,
                region_size: end - page_addr,
                state: MEM_COMMIT as u32,
                protect: prot,
                mem_type: MEM_PRIVATE as u32,
                _pad1: 0,
            };
        }
        MemoryBasicInformation64 {
            base_address: page_addr,
            allocation_base: 0,
            allocation_protect: 0,
            partition_id: 0,
            _pad0: 0,
            region_size: PAGE_SIZE,
            state: MEM_FREE as u32,
            protect: PAGE_NOACCESS as u32,
            mem_type: 0,
            _pad1: 0,
        }
    });
    let Some(info) = mbi else {
        return STATUS_INVALID_PARAMETER;
    };
    if usermem::write_current_user_struct(buf_ptr, &info).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    if ret_len_ptr != 0 {
        let _ = write_user_u64(ret_len_ptr, mbi_size);
    }
    STATUS_SUCCESS
}

fn nt_desired_access_wants_write(desired_access: u32) -> bool {
    // GENERIC_WRITE (0x4000_0000) | GENERIC_ALL (0x1000_0000) |
    // DELETE (0x0001_0000) | FILE_WRITE_DATA (0x2) | FILE_APPEND_DATA (0x4)
    desired_access & 0x5001_0006 != 0
}

fn nt_open_or_create_file(frame: &SyscallFrame, is_create: bool) -> u64 {
    let (file_handle_ptr, desired_access, oa_ptr, io_status_ptr) =
        (frame.r10, frame.rdx, frame.r8, frame.r9);
    if file_handle_ptr == 0 || oa_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    if let Ok((_, raw_name)) = read_object_attributes_unicode(oa_ptr) {
        let lower_raw = raw_name.to_ascii_lowercase();
        if lower_raw.starts_with("\\device\\afd") {
            return net_socket::open_afd_device(file_handle_ptr, io_status_ptr);
        }
        if lower_raw == "\\??\\nsi" || lower_raw == "\\device\\nsi" {
            return net_socket::open_nsi_device(file_handle_ptr, io_status_ptr);
        }
    }
    let create_disposition = if is_create {
        (read_stack_arg(frame, 7).unwrap_or(1) as u32) as u64
    } else {
        1 // FILE_OPEN
    };
    let (primary, fallback) = match resolve_object_attributes_posix_path(oa_ptr) {
        Ok(paths) => paths,
        Err(status) => return status,
    };

    let wants_write = nt_desired_access_wants_write(desired_access as u32);
    let mut open_flags = if wants_write {
        linux_abi::O_RDWR
    } else {
        linux_abi::O_RDONLY
    };
    const LINUX_O_EXCL: u64 = 0o200;
    match create_disposition {
        0 => open_flags |= linux_abi::O_CREAT | linux_abi::O_TRUNC, // FILE_SUPERSEDE
        2 => open_flags |= linux_abi::O_CREAT | LINUX_O_EXCL,       // FILE_CREATE
        3 => open_flags |= linux_abi::O_CREAT,                      // FILE_OPEN_IF
        4 => open_flags |= linux_abi::O_TRUNC,                      // FILE_OVERWRITE
        5 => open_flags |= linux_abi::O_CREAT | linux_abi::O_TRUNC, // FILE_OVERWRITE_IF
        _ => {}
    }

    let opened = open_vfs_with_fallback(&primary, fallback.as_deref(), open_flags);
    match opened {
        Ok((fd, used_path)) => {
            if let Ok(stat) = vfs_meta::kernel_vfs_fstat(fd)
                && (stat.st_mode & 0o170_000) == 0o040_000
            {
                let _ = multitask::with_current_user_process_state_mut(|_, _, st| {
                    let paths = &mut st.win32_objects_mut().dir_paths;
                    paths.retain(|(f, _)| *f != fd);
                    paths.push((fd, used_path));
                });
            }
            let win_handle = WIN32_FD_HANDLE_BASE + fd;
            if write_user_u64(file_handle_ptr, win_handle) != STATUS_SUCCESS {
                let _ = multitask::with_current_user_process_state_mut(|_, _, st| {
                    st.win32_objects_mut().dir_paths.retain(|(f, _)| *f != fd);
                });
                let _ = vfs_socket::syscall_linux_vfs_close(fd);
                return STATUS_INVALID_PARAMETER;
            }
            write_io_status(io_status_ptr, STATUS_SUCCESS, 1 /* FILE_OPENED */);
            STATUS_SUCCESS
        }
        Err(status) => {
            write_io_status(io_status_ptr, status, 0);
            status
        }
    }
}

fn open_vfs_with_fallback(
    primary: &str,
    fallback: Option<&str>,
    flags: u64,
) -> Result<(u64, String), u64> {
    match vfs_socket::syscall_linux_vfs_openat_str(linux_abi::AT_FDCWD as u64, primary, flags, 0o644)
    {
        Ok(fd) => Ok((fd, String::from(primary))),
        Err(errno) => {
            if let Some(alt) = fallback
                && let Ok(fd) = vfs_socket::syscall_linux_vfs_openat_str(
                    linux_abi::AT_FDCWD as u64,
                    alt,
                    flags,
                    0o644,
                )
            {
                return Ok((fd, String::from(alt)));
            }
            Err(linux_errno_to_ntstatus(errno))
        }
    }
}

pub(super) fn read_object_attributes_unicode(oa_ptr: u64) -> Result<(u64, String), u64> {
    let oa = usermem::read_current_user_struct::<ObjectAttributes64>(oa_ptr)
        .map_err(|_| STATUS_INVALID_PARAMETER)?;
    if oa.object_name == 0 {
        return Err(STATUS_OBJECT_NAME_NOT_FOUND);
    }
    let us = usermem::read_current_user_struct::<UnicodeString64>(oa.object_name)
        .map_err(|_| STATUS_INVALID_PARAMETER)?;
    let byte_len = usize::from(us.length);
    if byte_len == 0 || us.buffer == 0 || (byte_len & 1) != 0 || byte_len > 2048 {
        return Err(STATUS_OBJECT_NAME_NOT_FOUND);
    }
    let mut raw = vec![0_u8; byte_len];
    usermem::copy_from_current_user_exact(us.buffer, &mut raw)
        .map_err(|_| STATUS_INVALID_PARAMETER)?;
    let u16s: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok((oa.root_directory, String::from_utf16_lossy(&u16s)))
}

fn resolve_object_attributes_posix_path(oa_ptr: u64) -> Result<(String, Option<String>), u64> {
    let (root_dir, raw_name) = read_object_attributes_unicode(oa_ptr)?;
    Ok(resolve_nt_path_to_posix(root_dir, &raw_name))
}

fn resolve_nt_path_to_posix(root_dir: u64, raw_name: &str) -> (String, Option<String>) {
    let mut normalized = raw_name.replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();

    if let Some(rest) = lower
        .strip_prefix("/??/")
        .or_else(|| lower.strip_prefix("/dosdevices/"))
    {
        let tail = &normalized[normalized.len() - rest.len()..];
        return translate_dos_path(tail);
    }
    if lower.starts_with("/systemroot/") || lower == "/systemroot" {
        let tail = &normalized["/systemroot".len()..];
        return translate_systemroot_subpath(tail);
    }
    if root_dir >= WIN32_FD_HANDLE_BASE && root_dir < WIN32_SECTION_HANDLE_BASE {
        let dir_fd = root_dir - WIN32_FD_HANDLE_BASE;
        if let Some(base_dir) = multitask::with_current_user_process_state_mut(|_, _, st| {
            st.handles()
                .get(dir_fd)
                .and_then(|h| match h {
                    multitask::KernelHandle::RemoteVfs(r) => Some(String::from(r.path())),
                    _ => None,
                })
        })
        .flatten()
        {
            let trimmed = normalized.trim_start_matches('/');
            let combined = format!("{}/{}", base_dir.trim_end_matches('/'), trimmed);
            let combined_lower = format!(
                "{}/{}",
                base_dir.trim_end_matches('/'),
                trimmed.to_ascii_lowercase()
            );
            return (
                combined.clone(),
                (combined_lower != combined).then_some(combined_lower),
            );
        }
    }
    if !normalized.starts_with('/') {
        normalized.insert(0, '/');
    }
    let alt = format!(
        "/compat/windows/System32/{}",
        normalized.trim_start_matches('/').to_ascii_lowercase()
    );
    (normalized, Some(alt))
}

fn translate_dos_path(dos_path: &str) -> (String, Option<String>) {
    let lower = dos_path.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("c:/") {
        let orig_rest = &dos_path[3..];
        let trimmed = orig_rest.trim_start_matches('/');
        if trimmed.eq_ignore_ascii_case("windows/system32") {
            return (String::from("/compat/windows/System32"), None);
        }
        if rest.starts_with("windows/system32/") {
            let file_tail = &orig_rest["windows/system32/".len()..];
            let primary = format!("/compat/windows/System32/{file_tail}");
            let lower_path = format!(
                "/compat/windows/System32/{}",
                file_tail.to_ascii_lowercase()
            );
            return (
                primary.clone(),
                (lower_path != primary).then_some(lower_path),
            );
        }
        if rest.starts_with("windows/") {
            let tail = &orig_rest["windows/".len()..];
            let primary = format!("/compat/windows/{tail}");
            let lower_path = format!("/compat/windows/{}", tail.to_ascii_lowercase());
            return (
                primary.clone(),
                (lower_path != primary).then_some(lower_path),
            );
        }
        let primary = format!("/compat/windows/{trimmed}");
        let lower_path = format!("/compat/windows/{}", trimmed.to_ascii_lowercase());
        return (
            primary.clone(),
            (lower_path != primary).then_some(lower_path),
        );
    }
    if lower.starts_with("z:/") {
        let orig_rest = &dos_path[2..];
        return (String::from(orig_rest), None);
    }
    let prefixed = format!("/{}", dos_path.trim_start_matches('/'));
    (prefixed, None)
}

fn translate_systemroot_subpath(tail: &str) -> (String, Option<String>) {
    let clean = tail.trim_start_matches('/');
    let lower = clean.to_ascii_lowercase();
    if lower == "system32" {
        return (String::from("/compat/windows/System32"), None);
    }
    if lower.starts_with("system32/") {
        let rest = &clean["system32/".len()..];
        let primary = format!("/compat/windows/System32/{rest}");
        let lower_path = format!("/compat/windows/System32/{}", rest.to_ascii_lowercase());
        return (
            primary.clone(),
            (lower_path != primary).then_some(lower_path),
        );
    }
    let primary = format!("/compat/windows/{clean}");
    let lower_path = format!("/compat/windows/{}", clean.to_ascii_lowercase());
    (
        primary.clone(),
        (lower_path != primary).then_some(lower_path),
    )
}

pub(super) fn map_win32_file_handle(handle: u64) -> Option<u64> {
    match handle {
        HANDLE_STDIN | STDIN_RAW_PSEUDO | STDIN_32_PSEUDO => Some(0),
        HANDLE_STDOUT | STDOUT_RAW_PSEUDO | STDOUT_32_PSEUDO => Some(1),
        HANDLE_STDERR | STDERR_RAW_PSEUDO | STDERR_32_PSEUDO => Some(2),
        h if h >= WIN32_FD_HANDLE_BASE && h < WIN32_SECTION_HANDLE_BASE => {
            Some(h - WIN32_FD_HANDLE_BASE)
        }
        _ => None,
    }
}

fn nt_read_or_write_file(frame: &SyscallFrame, is_write: bool) -> u64 {
    let (file_handle, event_handle, _apc, _apc_ctx) =
        (frame.r10, frame.rdx, frame.r8, frame.r9);
    let io_status_ptr = read_stack_arg(frame, 4).unwrap_or(0);
    let buffer_ptr = read_stack_arg(frame, 5).unwrap_or(0);
    let length = (read_stack_arg(frame, 6).unwrap_or(0) as u32) as u64;
    let byte_offset_ptr = read_stack_arg(frame, 7).unwrap_or(0);

    let Some(fd) = map_win32_file_handle(file_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let offset = if byte_offset_ptr != 0 {
        match usermem::read_current_user_struct::<i64>(byte_offset_ptr) {
            Ok(v) if v >= 0 => Some(v as u64),
            Ok(_) => None,
            Err(_) => return STATUS_INVALID_PARAMETER,
        }
    } else {
        None
    };

    let io_res = if is_write {
        vfs_socket::kernel_vfs_write_user(fd, buffer_ptr, length, offset)
    } else {
        vfs_socket::kernel_vfs_read_user(fd, buffer_ptr, length, offset)
    };
    if event_handle >= WIN32_EVENT_HANDLE_BASE && event_handle < WIN32_MUTANT_HANDLE_BASE {
        let _ = sync::nt_modify_event(event_handle, 0, true, false);
    }
    match io_res {
        Ok(0) if !is_write && length > 0 => {
            write_io_status(io_status_ptr, STATUS_END_OF_FILE, 0);
            STATUS_END_OF_FILE
        }
        Ok(bytes) => {
            write_io_status(io_status_ptr, STATUS_SUCCESS, bytes);
            STATUS_SUCCESS
        }
        Err(errno) => {
            let status = linux_errno_to_ntstatus(errno);
            write_io_status(io_status_ptr, status, 0);
            status
        }
    }
}

fn nt_close(handle: u64) -> u64 {
    if matches!(
        handle,
        HANDLE_CURRENT_PROCESS
            | HANDLE_CURRENT_THREAD
            | HANDLE_STDIN
            | HANDLE_STDOUT
            | HANDLE_STDERR
            | STDIN_RAW_PSEUDO
            | STDOUT_RAW_PSEUDO
            | STDERR_RAW_PSEUDO
            | STDIN_32_PSEUDO
            | STDOUT_32_PSEUDO
            | STDERR_32_PSEUDO
    ) {
        return STATUS_SUCCESS;
    }
    if net_socket::is_afd_handle(handle) || net_socket::is_nsi_handle(handle) {
        return net_socket::close_afd_or_nsi_handle(handle);
    }
    if handle >= WIN32_SECTION_HANDLE_BASE && handle < WIN32_EVENT_HANDLE_BASE {
        let removed_fd = multitask::with_current_user_process_state_mut(|_, _, st| {
            let secs = &mut st.win32_objects_mut().sections;
            let idx = secs.iter().position(|s| s.handle == handle)?;
            Some(secs.remove(idx).fd)
        })
        .flatten();
        return match removed_fd {
            Some(Some(fd)) => {
                let _ = vfs_socket::syscall_linux_vfs_close(fd);
                STATUS_SUCCESS
            }
            Some(None) => STATUS_SUCCESS,
            None => STATUS_INVALID_HANDLE,
        };
    }
    if handle >= WIN32_EVENT_HANDLE_BASE && handle < WIN32_HANDLE_ALLOCATED_END {
        let removed = multitask::with_current_user_process_state_mut(|_, _, st| {
            let objs = st.win32_objects_mut();
            if handle < WIN32_MUTANT_HANDLE_BASE {
                let idx = objs.events.iter().position(|e| e.handle == handle)?;
                objs.events.remove(idx);
            } else if handle < WIN32_SEMAPHORE_HANDLE_BASE {
                let idx = objs.mutants.iter().position(|m| m.handle == handle)?;
                objs.mutants.remove(idx);
            } else if handle < WIN32_THREAD_HANDLE_BASE {
                let idx = objs.semaphores.iter().position(|s| s.handle == handle)?;
                objs.semaphores.remove(idx);
            } else if handle < WIN32_PROCESS_HANDLE_BASE {
                let idx = objs.threads.iter().position(|t| t.handle == handle)?;
                objs.threads.remove(idx);
            } else if handle < WIN32_KEY_HANDLE_BASE {
                let idx = objs.processes.iter().position(|p| p.handle == handle)?;
                objs.processes.remove(idx);
            } else if handle < WIN32_OBJ_DIR_HANDLE_BASE {
                let idx = objs.keys.iter().position(|k| k.handle == handle)?;
                objs.keys.remove(idx);
            } else if handle < WIN32_SYMLINK_HANDLE_BASE {
                let idx = objs.obj_dirs.iter().position(|d| d.handle == handle)?;
                objs.obj_dirs.remove(idx);
            } else if handle < WIN32_AFD_HANDLE_BASE {
                let idx = objs.symlinks.iter().position(|s| s.handle == handle)?;
                objs.symlinks.remove(idx);
            } else {
                return None;
            }
            Some(())
        })
        .flatten();
        return if removed.is_some() {
            STATUS_SUCCESS
        } else {
            STATUS_INVALID_HANDLE
        };
    }
    if let Some(fd) = map_win32_file_handle(handle) {
        let delete_target = take_delete_on_close_target(fd);
        let _ = multitask::with_current_user_process_state_mut(|_, _, st| {
            let objs = st.win32_objects_mut();
            objs.dir_cursors.retain(|c| c.fd != fd);
            objs.dir_paths.retain(|(f, _)| *f != fd);
        });
        return if vfs_socket::kernel_vfs_result(vfs_socket::syscall_linux_vfs_close(fd)).is_ok() {
            if let Some((path, is_dir)) = delete_target {
                let flags = if is_dir {
                    linux_abi::AT_REMOVEDIR as u64
                } else {
                    0
                };
                let _ = vfs_meta::kernel_vfs_unlink_path(&path, flags);
            }
            STATUS_SUCCESS
        } else {
            STATUS_INVALID_HANDLE
        };
    }
    STATUS_INVALID_HANDLE
}

fn nt_duplicate_object(frame: &SyscallFrame) -> u64 {
    let (src_proc, src_handle, dst_proc, dst_handle_ptr) =
        (frame.r10, frame.rdx, frame.r8, frame.r9);
    if src_proc != HANDLE_CURRENT_PROCESS || dst_proc != HANDLE_CURRENT_PROCESS {
        return STATUS_INVALID_HANDLE;
    }
    if dst_handle_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    if matches!(
        src_handle,
        HANDLE_CURRENT_PROCESS
            | HANDLE_CURRENT_THREAD
            | HANDLE_STDIN
            | HANDLE_STDOUT
            | HANDLE_STDERR
    ) {
        return write_user_u64(dst_handle_ptr, src_handle);
    }
    if let Some(fd) = map_win32_file_handle(src_handle) {
        return match vfs_socket::kernel_vfs_dup(fd) {
            Ok(new_fd) => write_user_u64(dst_handle_ptr, WIN32_FD_HANDLE_BASE + new_fd),
            Err(errno) => linux_errno_to_ntstatus(errno),
        };
    }
    if src_handle >= WIN32_SECTION_HANDLE_BASE && src_handle < WIN32_EVENT_HANDLE_BASE {
        let cloned = multitask::with_current_user_process_state_mut(|_, _, st| {
            let obj = st
                .win32_objects()
                .sections
                .iter()
                .find(|s| s.handle == src_handle)?
                .clone();
            Some(obj)
        })
        .flatten();
        let Some(mut obj) = cloned else {
            return STATUS_INVALID_HANDLE;
        };
        if let Some(fd) = obj.fd {
            match vfs_socket::kernel_vfs_dup(fd) {
                Ok(new_fd) => obj.fd = Some(new_fd),
                Err(errno) => return linux_errno_to_ntstatus(errno),
            }
        }
        let new_h = multitask::with_current_user_process_state_mut(|_, _, st| {
            let objs = st.win32_objects_mut();
            let h = WIN32_SECTION_HANDLE_BASE + objs.next_section_seq + 1;
            objs.next_section_seq += 1;
            obj.handle = h;
            objs.sections.push(obj);
            h
        });
        return match new_h {
            Some(h) => write_user_u64(dst_handle_ptr, h),
            None => STATUS_NO_MEMORY,
        };
    }
    if src_handle >= WIN32_EVENT_HANDLE_BASE && src_handle < WIN32_AFD_HANDLE_BASE {
        let new_h = multitask::with_current_user_process_state_mut(|_, _, st| {
            let objs = st.win32_objects_mut();
            if src_handle < WIN32_MUTANT_HANDLE_BASE {
                let mut obj = objs.events.iter().find(|e| e.handle == src_handle)?.clone();
                let h = WIN32_EVENT_HANDLE_BASE + objs.next_event_seq + 1;
                objs.next_event_seq += 1;
                obj.handle = h;
                objs.events.push(obj);
                Some(h)
            } else if src_handle < WIN32_SEMAPHORE_HANDLE_BASE {
                let mut obj = objs.mutants.iter().find(|m| m.handle == src_handle)?.clone();
                let h = WIN32_MUTANT_HANDLE_BASE + objs.next_mutant_seq + 1;
                objs.next_mutant_seq += 1;
                obj.handle = h;
                objs.mutants.push(obj);
                Some(h)
            } else if src_handle < WIN32_THREAD_HANDLE_BASE {
                let mut obj = objs.semaphores.iter().find(|s| s.handle == src_handle)?.clone();
                let h = WIN32_SEMAPHORE_HANDLE_BASE + objs.next_semaphore_seq + 1;
                objs.next_semaphore_seq += 1;
                obj.handle = h;
                objs.semaphores.push(obj);
                Some(h)
            } else if src_handle < WIN32_PROCESS_HANDLE_BASE {
                let mut obj = objs.threads.iter().find(|t| t.handle == src_handle)?.clone();
                let h = WIN32_THREAD_HANDLE_BASE + objs.next_thread_seq + 1;
                objs.next_thread_seq += 1;
                obj.handle = h;
                objs.threads.push(obj);
                Some(h)
            } else if src_handle < WIN32_KEY_HANDLE_BASE {
                let mut obj = objs.processes.iter().find(|p| p.handle == src_handle)?.clone();
                let h = WIN32_PROCESS_HANDLE_BASE + objs.next_process_seq + 1;
                objs.next_process_seq += 1;
                obj.handle = h;
                objs.processes.push(obj);
                Some(h)
            } else if src_handle < WIN32_OBJ_DIR_HANDLE_BASE {
                let mut obj = objs.keys.iter().find(|k| k.handle == src_handle)?.clone();
                let h = WIN32_KEY_HANDLE_BASE + objs.next_key_seq + 1;
                objs.next_key_seq += 1;
                obj.handle = h;
                objs.keys.push(obj);
                Some(h)
            } else if src_handle < WIN32_SYMLINK_HANDLE_BASE {
                let mut obj = objs.obj_dirs.iter().find(|d| d.handle == src_handle)?.clone();
                let h = WIN32_OBJ_DIR_HANDLE_BASE + objs.next_obj_dir_seq + 1;
                objs.next_obj_dir_seq += 1;
                obj.handle = h;
                objs.obj_dirs.push(obj);
                Some(h)
            } else {
                let mut obj = objs.symlinks.iter().find(|s| s.handle == src_handle)?.clone();
                let h = WIN32_SYMLINK_HANDLE_BASE + objs.next_symlink_seq + 1;
                objs.next_symlink_seq += 1;
                obj.handle = h;
                objs.symlinks.push(obj);
                Some(h)
            }
        })
        .flatten();
        return match new_h {
            Some(h) => write_user_u64(dst_handle_ptr, h),
            None => STATUS_INVALID_HANDLE,
        };
    }
    let _ = WIN32_NSI_HANDLE_BASE;
    STATUS_INVALID_HANDLE
}

fn nt_query_information_file(frame: &SyscallFrame) -> u64 {
    let (file_handle, io_status_ptr, info_ptr, length) = (
        frame.r10,
        frame.rdx,
        frame.r8,
        (frame.r9 as u32) as u64,
    );
    let info_class = (read_stack_arg(frame, 4).unwrap_or(0) as u32) as u64;
    let Some(fd) = map_win32_file_handle(file_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let stat = match vfs_meta::kernel_vfs_fstat(fd) {
        Ok(stat) => stat,
        Err(_) if fd <= 2 && multitask::current_user_snapshot().is_some_and(|s| s.console_session().raw() != 0) => {
            let mut stat = linux_abi::LinuxStat::default();
            stat.st_mode = 0o020_666;
            stat.st_nlink = 1;
            stat
        }
        Err(_) => return STATUS_INVALID_HANDLE,
    };
    let is_dir = ((stat.st_mode & 0o170_000) == 0o040_000) as u8;
    let file_size = stat.st_size.max(0);
    let alloc_size = file_size.min(i64::MAX - 4095).saturating_add(4095) & !4095;
    let mtime = stat
        .st_mtim
        .tv_sec
        .saturating_mul(10_000_000)
        .saturating_add(FILE_EPOCH_BIAS_100NS);

    match info_class {
        4 /* FileBasicInformation */ => {
            if (length as usize) < size_of::<FileBasicInformation64>() {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let info = FileBasicInformation64 {
                creation_time: mtime,
                last_access_time: mtime,
                last_write_time: mtime,
                change_time: mtime,
                file_attributes: if is_dir != 0 { 0x10 } else { 0x80 },
                _pad: 0,
            };
            if usermem::write_current_user_struct(info_ptr, &info).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            write_io_status(io_status_ptr, STATUS_SUCCESS, size_of::<FileBasicInformation64>() as u64);
            STATUS_SUCCESS
        }
        5 /* FileStandardInformation */ => {
            if (length as usize) < size_of::<FileStandardInformation64>() {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let info = FileStandardInformation64 {
                allocation_size: alloc_size,
                end_of_file: file_size,
                number_of_links: 1,
                delete_pending: 0,
                directory: is_dir,
                _pad: 0,
            };
            if usermem::write_current_user_struct(info_ptr, &info).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            write_io_status(io_status_ptr, STATUS_SUCCESS, size_of::<FileStandardInformation64>() as u64);
            STATUS_SUCCESS
        }
        14 /* FilePositionInformation */ => {
            if length < 8 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let pos = vfs_socket::kernel_vfs_result(vfs_meta::syscall_linux_vfs_lseek(
                fd,
                0,
                linux_abi::SEEK_CUR,
            ))
            .unwrap_or(0) as i64;
            let status = write_user_i64(info_ptr, pos);
            if status == STATUS_SUCCESS {
                write_io_status(io_status_ptr, STATUS_SUCCESS, 8);
            }
            status
        }
        34 /* FileNetworkOpenInformation */ => {
            if (length as usize) < size_of::<FileNetworkOpenInformation64>() {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let info = FileNetworkOpenInformation64 {
                creation_time: mtime,
                last_access_time: mtime,
                last_write_time: mtime,
                change_time: mtime,
                allocation_size: alloc_size,
                end_of_file: file_size,
                file_attributes: if is_dir != 0 { 0x10 } else { 0x80 },
                _pad: 0,
            };
            if usermem::write_current_user_struct(info_ptr, &info).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            write_io_status(io_status_ptr, STATUS_SUCCESS, size_of::<FileNetworkOpenInformation64>() as u64);
            STATUS_SUCCESS
        }
        _ => STATUS_INVALID_INFO_CLASS,
    }
}

static DELETE_ON_CLOSE_FDS: spin::Mutex<Vec<(u64, u64)>> = spin::Mutex::new(Vec::new());

fn current_win32_pid() -> u64 {
    multitask::current_user_snapshot()
        .map(|s| s.process_id())
        .unwrap_or(0)
}

fn set_delete_on_close_fd(fd: u64, delete: bool) {
    let pid = current_win32_pid();
    let mut guard = DELETE_ON_CLOSE_FDS.lock();
    guard.retain(|&(p, f)| !(p == pid && f == fd));
    if delete {
        guard.push((pid, fd));
    }
}

fn take_delete_on_close_target(fd: u64) -> Option<(String, bool)> {
    let pid = current_win32_pid();
    let marked = {
        let mut guard = DELETE_ON_CLOSE_FDS.lock();
        let pos = guard.iter().position(|&(p, f)| p == pid && f == fd);
        if let Some(idx) = pos {
            guard.remove(idx);
            true
        } else {
            false
        }
    };
    if !marked {
        return None;
    }
    let remote = crate::user::syscall::linux::service_ops::current_remote_vfs_handle(fd)?;
    let is_dir = remote.kind() == multitask::RemoteVfsHandleKind::Directory;
    Some((String::from(remote.path()), is_dir))
}

fn nt_flush_buffers_file(frame: &SyscallFrame) -> u64 {
    let (file_handle, io_status_ptr) = (frame.r10, frame.rdx);
    if matches!(
        file_handle,
        HANDLE_STDIN
            | HANDLE_STDOUT
            | HANDLE_STDERR
            | STDIN_RAW_PSEUDO
            | STDOUT_RAW_PSEUDO
            | STDERR_RAW_PSEUDO
            | STDIN_32_PSEUDO
            | STDOUT_32_PSEUDO
            | STDERR_32_PSEUDO
    ) {
        write_io_status(io_status_ptr, STATUS_SUCCESS, 0);
        return STATUS_SUCCESS;
    }
    let Some(fd) = map_win32_file_handle(file_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    match vfs_socket::kernel_vfs_result(vfs_socket::syscall_linux_vfs_fsync(fd)) {
        Ok(_) => {
            write_io_status(io_status_ptr, STATUS_SUCCESS, 0);
            STATUS_SUCCESS
        }
        Err(e) => linux_errno_to_ntstatus(e),
    }
}

fn parse_file_rename_info(bytes: &[u8]) -> Option<(bool, u64, String)> {
    if bytes.len() < 20 {
        return None;
    }
    let replace_if_exists = bytes[0] != 0;
    let root_dir = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
    let name_len = u32::from_le_bytes(bytes[16..20].try_into().ok()?) as usize;
    if name_len == 0 || name_len % 2 != 0 || 20usize.checked_add(name_len)? > bytes.len() {
        return None;
    }
    let mut u16s = Vec::with_capacity(name_len / 2);
    for chunk in bytes[20..20 + name_len].chunks_exact(2) {
        u16s.push(u16::from_le_bytes([chunk[0], chunk[1]]));
    }
    let name = String::from_utf16_lossy(&u16s);
    Some((replace_if_exists, root_dir, name))
}

fn resolve_rename_target_posix(old_path: &str, root_dir: u64, raw_name: &str) -> String {
    let trimmed = raw_name.trim_matches('\0');
    let normalized = trimmed.replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();
    if root_dir == 0
        && !lower.starts_with("/??/")
        && !lower.starts_with("/dosdevices/")
        && !lower.starts_with("/systemroot")
        && !normalized.starts_with('/')
        && !(normalized.len() >= 2 && normalized.as_bytes()[1] == b':')
    {
        let parent = old_path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        if parent.is_empty() {
            format!("/{normalized}")
        } else {
            format!("{parent}/{normalized}")
        }
    } else {
        resolve_nt_path_to_posix(root_dir, trimmed).0
    }
}

fn nt_set_information_file(frame: &SyscallFrame) -> u64 {
    let (file_handle, io_status_ptr, info_ptr, length) = (
        frame.r10,
        frame.rdx,
        frame.r8,
        (frame.r9 as u32) as u64,
    );
    let info_class = (read_stack_arg(frame, 4).unwrap_or(0) as u32) as u64;
    let Some(fd) = map_win32_file_handle(file_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    match info_class {
        10 /* FileRenameInformation */ => {
            if info_ptr == 0 || length < 20 || length > 4096 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let mut buf = alloc::vec![0u8; length as usize];
            if usermem::copy_from_current_user_exact(info_ptr, &mut buf).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            let Some((replace_if_exists, root_dir, raw_name)) = parse_file_rename_info(&buf) else {
                return STATUS_INVALID_PARAMETER;
            };
            let Some(remote) = crate::user::syscall::linux::service_ops::current_remote_vfs_handle(fd) else {
                return STATUS_INVALID_HANDLE;
            };
            let old_path = remote.path();
            let new_path = resolve_rename_target_posix(&old_path, root_dir, &raw_name);
            let flags = if replace_if_exists {
                0
            } else {
                linux_abi::RENAME_NOREPLACE as u64
            };
            match vfs_meta::kernel_vfs_rename_paths(&old_path, &new_path, flags) {
                Ok(()) => {
                    write_io_status(io_status_ptr, STATUS_SUCCESS, 0);
                    STATUS_SUCCESS
                }
                Err(e) => linux_errno_to_ntstatus(e),
            }
        }
        13 /* FileDispositionInformation */ => {
            if info_ptr == 0 || length < 1 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let Ok(delete_file) = usermem::read_current_user_struct::<u8>(info_ptr) else {
                return STATUS_INVALID_PARAMETER;
            };
            if crate::user::syscall::linux::service_ops::current_remote_vfs_handle(fd).is_none() {
                return STATUS_INVALID_HANDLE;
            }
            set_delete_on_close_fd(fd, delete_file != 0);
            write_io_status(io_status_ptr, STATUS_SUCCESS, 0);
            STATUS_SUCCESS
        }
        14 /* FilePositionInformation */ => {
            if length < 8 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let Ok(pos) = usermem::read_current_user_struct::<i64>(info_ptr) else {
                return STATUS_INVALID_PARAMETER;
            };
            if pos < 0 {
                return STATUS_INVALID_PARAMETER;
            }
            match vfs_socket::kernel_vfs_result(vfs_meta::syscall_linux_vfs_lseek(
                fd,
                pos,
                linux_abi::SEEK_SET,
            )) {
                Ok(_) => {
                    write_io_status(io_status_ptr, STATUS_SUCCESS, 0);
                    STATUS_SUCCESS
                }
                Err(e) => linux_errno_to_ntstatus(e),
            }
        }
        19 /* FileEndOfFileInformation */ => {
            if length < 8 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let Ok(eof) = usermem::read_current_user_struct::<i64>(info_ptr) else {
                return STATUS_INVALID_PARAMETER;
            };
            if eof < 0 {
                return STATUS_INVALID_PARAMETER;
            }
            match vfs_socket::kernel_vfs_result(vfs_meta::syscall_linux_vfs_ftruncate(
                fd,
                eof as u64,
            )) {
                Ok(_) => {
                    write_io_status(io_status_ptr, STATUS_SUCCESS, 0);
                    STATUS_SUCCESS
                }
                Err(e) => linux_errno_to_ntstatus(e),
            }
        }
        20 /* FileAllocationInformation */ => {
            if length < 8 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let Ok(alloc_size) = usermem::read_current_user_struct::<i64>(info_ptr) else {
                return STATUS_INVALID_PARAMETER;
            };
            if alloc_size < 0 {
                return STATUS_INVALID_PARAMETER;
            }
            write_io_status(io_status_ptr, STATUS_SUCCESS, 0);
            STATUS_SUCCESS
        }
        _ => {
            write_io_status(io_status_ptr, STATUS_SUCCESS, 0);
            STATUS_SUCCESS
        }
    }
}

fn encode_file_fs_size_info(statfs: &rustos_user_abi::syscall::VfsStatFsWire) -> [u8; 24] {
    let bsize = statfs.f_bsize.max(512) as u64;
    let bytes_per_sector = 512_u32;
    let sectors_per_unit = (bsize / u64::from(bytes_per_sector)).max(1) as u32;
    let mut out = [0_u8; 24];
    out[0..8].copy_from_slice(&statfs.f_blocks.to_le_bytes());
    out[8..16].copy_from_slice(&statfs.f_bavail.to_le_bytes());
    out[16..20].copy_from_slice(&sectors_per_unit.to_le_bytes());
    out[20..24].copy_from_slice(&bytes_per_sector.to_le_bytes());
    out
}

fn encode_file_fs_attribute_info(
    statfs: &rustos_user_abi::syscall::VfsStatFsWire,
    max_len: usize,
) -> Result<(Vec<u8>, bool), u64> {
    if max_len < 12 {
        return Err(STATUS_INFO_LENGTH_MISMATCH);
    }
    const MSDOS_SUPER_MAGIC: i64 = 0x4d44;
    let (fs_name, attrs): (&str, u32) = if statfs.f_type == MSDOS_SUPER_MAGIC {
        ("FAT32", 0x0000_0006) // FILE_CASE_PRESERVED_NAMES | FILE_UNICODE_ON_DISK
    } else {
        ("VFS", 0x0000_0007) // FILE_CASE_SENSITIVE_SEARCH | FILE_CASE_PRESERVED_NAMES | FILE_UNICODE_ON_DISK
    };
    let name_bytes_len = fs_name.len() * 2;
    let max_comp = if statfs.f_namelen > 0 {
        statfs.f_namelen.min(i32::MAX as i64) as i32
    } else {
        255
    };
    let total_needed = 12 + name_bytes_len;
    let copy_len = total_needed.min(max_len);
    let mut out = alloc::vec![0_u8; copy_len];
    out[0..4].copy_from_slice(&attrs.to_le_bytes());
    out[4..8].copy_from_slice(&max_comp.to_le_bytes());
    out[8..12].copy_from_slice(&(name_bytes_len as u32).to_le_bytes());
    let mut idx = 12;
    for ch in fs_name.encode_utf16() {
        let le = ch.to_le_bytes();
        if idx + 2 <= copy_len {
            out[idx..idx + 2].copy_from_slice(&le);
            idx += 2;
        } else {
            break;
        }
    }
    Ok((out, copy_len < total_needed))
}

fn nt_query_volume_information_file(frame: &SyscallFrame) -> u64 {
    let (file_handle, io_status_ptr, info_ptr, length) = (
        frame.r10,
        frame.rdx,
        frame.r8,
        (frame.r9 as u32) as u64,
    );
    let info_class = (read_stack_arg(frame, 4).unwrap_or(0) as u32) as u64;
    if info_class == 4 /* FileFsDeviceInformation */ && length >= 8 {
        let is_console_or_char = map_win32_file_handle(file_handle).is_some_and(|fd| {
            match vfs_meta::kernel_vfs_fstat(fd) {
                Ok(stat) => (stat.st_mode & 0o170_000) == 0o020_000,
                Err(_) => {
                    fd <= 2
                        && multitask::current_user_snapshot()
                            .is_some_and(|s| s.console_session().raw() != 0)
                }
            }
        });
        let (device_type, characteristics) = if is_console_or_char {
            (0x0000_0050_u32 /* FILE_DEVICE_CONSOLE */, 0_u32)
        } else {
            (0x0000_0001_u32 /* FILE_DEVICE_DISK */, 0x20_u32)
        };
        let dev_info = [device_type, characteristics];
        let bytes: [u8; 8] = unsafe { core::mem::transmute(dev_info) };
        if usermem::write_current_user_bytes(info_ptr, &bytes).is_ok() {
            write_io_status(io_status_ptr, STATUS_SUCCESS, 8);
            return STATUS_SUCCESS;
        }
        return STATUS_INVALID_PARAMETER;
    }
    if matches!(info_class, 3 /* FileFsSizeInformation */ | 5 /* FileFsAttributeInformation */) {
        let statfs = match map_win32_file_handle(file_handle) {
            Some(fd) => vfs_meta::kernel_vfs_fstatfs(fd),
            None => return STATUS_INVALID_HANDLE,
        };
        let statfs = match statfs {
            Ok(s) => s,
            Err(e) => return linux_errno_to_ntstatus(e),
        };
        if info_class == 3 {
            if length < 24 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let bytes = encode_file_fs_size_info(&statfs);
            if usermem::write_current_user_bytes(info_ptr, &bytes).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            write_io_status(io_status_ptr, STATUS_SUCCESS, 24);
            return STATUS_SUCCESS;
        }
        let (bytes, overflow) = match encode_file_fs_attribute_info(&statfs, length as usize) {
            Ok(res) => res,
            Err(status) => return status,
        };
        let written = bytes.len() as u64;
        if usermem::write_current_user_bytes(info_ptr, &bytes).is_err() {
            return STATUS_INVALID_PARAMETER;
        }
        let status = if overflow {
            rustos_user_abi::windows::STATUS_BUFFER_OVERFLOW
        } else {
            STATUS_SUCCESS
        };
        write_io_status(io_status_ptr, status, written);
        return status;
    }
    STATUS_INVALID_INFO_CLASS
}

fn nt_query_attributes_file(oa_ptr: u64, info_ptr: u64, full: bool) -> u64 {
    if oa_ptr == 0 || info_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let Ok((primary, fallback)) = resolve_object_attributes_posix_path(oa_ptr) else {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    };
    let statx = vfs_meta::kernel_vfs_statx_path(&primary).or_else(|_| {
        if let Some(alt) = fallback.as_deref() {
            vfs_meta::kernel_vfs_statx_path(alt)
        } else {
            Err(1)
        }
    });
    let Ok(st) = statx else {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    };
    let is_dir = (st.stx_mode & 0o170_000) == 0o040_000;
    let mtime = st
        .stx_mtime
        .tv_sec
        .saturating_mul(10_000_000)
        .saturating_add(FILE_EPOCH_BIAS_100NS);
    let attrs = if is_dir { 0x10 } else { 0x80 };
    if full {
        let info = FileNetworkOpenInformation64 {
            creation_time: mtime,
            last_access_time: mtime,
            last_write_time: mtime,
            change_time: mtime,
            allocation_size: (st.stx_size.min(i64::MAX as u64 - 4095) as i64 + 4095) & !4095,
            end_of_file: st.stx_size.min(i64::MAX as u64) as i64,
            file_attributes: attrs,
            _pad: 0,
        };
        if usermem::write_current_user_struct(info_ptr, &info).is_err() {
            return STATUS_INVALID_PARAMETER;
        }
    } else {
        let info = FileBasicInformation64 {
            creation_time: mtime,
            last_access_time: mtime,
            last_write_time: mtime,
            change_time: mtime,
            file_attributes: attrs,
            _pad: 0,
        };
        if usermem::write_current_user_struct(info_ptr, &info).is_err() {
            return STATUS_INVALID_PARAMETER;
        }
    }
    STATUS_SUCCESS
}

fn nt_create_section(frame: &SyscallFrame) -> u64 {
    let (sec_handle_ptr, _access, oa_ptr, max_size_ptr) =
        (frame.r10, frame.rdx, frame.r8, frame.r9);
    let page_protect = (read_stack_arg(frame, 4).unwrap_or(PAGE_READONLY) as u32) as u64;
    let alloc_attrs = (read_stack_arg(frame, 5).unwrap_or(0) as u32) as u64;
    let file_handle = read_stack_arg(frame, 6).unwrap_or(0);
    if sec_handle_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }

    let sec_name = if oa_ptr != 0 {
        read_object_attributes_unicode(oa_ptr)
            .ok()
            .map(|(_, s)| s)
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    let is_image = (alloc_attrs & SEC_IMAGE) != 0;
    let mut size = if max_size_ptr != 0 {
        usermem::read_current_user_struct::<u64>(max_size_ptr).unwrap_or(0)
    } else {
        0
    };
    if size == 0 && file_handle == 0 && sec_name.is_none() {
        size = PAGE_SIZE;
    }

    if let Some(ref name) = sec_name && file_handle == 0 {
        if size == 0 {
            size = PAGE_SIZE;
        }
        let (fd, sec_size, sec_prot) =
            match fs_object::register_or_open_named_section(name, size, page_protect, true) {
                Ok(v) => v,
                Err(status) => return status,
            };
        let handle = multitask::with_current_user_process_state_mut(|_, _, st| {
            let objs = st.win32_objects_mut();
            let h = WIN32_SECTION_HANDLE_BASE + objs.next_section_seq + 1;
            objs.next_section_seq += 1;
            objs.sections.push(Win32SectionObject {
                handle: h,
                fd: Some(fd),
                size: sec_size,
                protect: sec_prot,
                is_image: false,
                name: Some(name.clone()),
            });
            h
        });
        let Some(handle) = handle else {
            let _ = vfs_socket::syscall_linux_vfs_close(fd);
            return STATUS_NO_MEMORY;
        };
        if write_user_u64(sec_handle_ptr, handle) != STATUS_SUCCESS {
            let _ = nt_close(handle);
            return STATUS_INVALID_PARAMETER;
        }
        return STATUS_SUCCESS;
    }

    let dup_fd = if file_handle != 0 {
        let Some(fd) = map_win32_file_handle(file_handle) else {
            return STATUS_INVALID_HANDLE;
        };
        match vfs_socket::kernel_vfs_dup(fd) {
            Ok(d) => Some(d),
            Err(errno) => return linux_errno_to_ntstatus(errno),
        }
    } else {
        None
    };

    if size == 0
        && let Some(fd) = dup_fd
        && let Ok(stat) = vfs_meta::kernel_vfs_fstat(fd)
    {
        size = stat.st_size.max(0) as u64;
    }
    if size == 0 {
        size = PAGE_SIZE;
    }

    let handle = multitask::with_current_user_process_state_mut(|_, _, st| {
        let objs = st.win32_objects_mut();
        let h = WIN32_SECTION_HANDLE_BASE + objs.next_section_seq + 1;
        objs.next_section_seq += 1;
        objs.sections.push(Win32SectionObject {
            handle: h,
            fd: dup_fd,
            size,
            protect: page_protect,
            is_image,
            name: sec_name.clone(),
        });
        h
    });
    let Some(handle) = handle else {
        if let Some(fd) = dup_fd {
            let _ = vfs_socket::syscall_linux_vfs_close(fd);
        }
        return STATUS_NO_MEMORY;
    };
    if write_user_u64(sec_handle_ptr, handle) != STATUS_SUCCESS {
        let _ = nt_close(handle);
        return STATUS_INVALID_PARAMETER;
    }
    STATUS_SUCCESS
}

struct ParsedPeHeader {
    image_base: u64,
    size_of_image: u32,
    entry_rva: u32,
    stack_reserve: u64,
    stack_commit: u64,
    subsystem: u16,
    minor_subsys: u16,
    major_subsys: u16,
    major_os: u16,
    minor_os: u16,
    characteristics: u16,
    dll_characteristics: u16,
    machine: u16,
    checksum: u32,
    size_of_headers: u32,
    sections: Vec<PeSectionHeader>,
}

#[derive(Clone, Copy)]
struct PeSectionHeader {
    virtual_size: u32,
    virtual_address: u32,
    size_of_raw_data: u32,
    pointer_to_raw_data: u32,
    characteristics: u32,
}

fn parse_pe64_headers(hdr: &[u8]) -> Option<ParsedPeHeader> {
    if hdr.len() < 0x100 || &hdr[0..2] != b"MZ" {
        return None;
    }
    let e_lfanew = u32::from_le_bytes(hdr[0x3c..0x40].try_into().ok()?) as usize;
    if e_lfanew.checked_add(0x108)? > hdr.len() || &hdr[e_lfanew..e_lfanew + 4] != b"PE\0\0" {
        return None;
    }
    let coff = e_lfanew + 4;
    let machine = u16::from_le_bytes(hdr[coff..coff + 2].try_into().ok()?);
    let num_sections = u16::from_le_bytes(hdr[coff + 2..coff + 4].try_into().ok()?) as usize;
    let size_opt = u16::from_le_bytes(hdr[coff + 16..coff + 18].try_into().ok()?) as usize;
    let characteristics = u16::from_le_bytes(hdr[coff + 18..coff + 20].try_into().ok()?);
    let opt = coff + 20;
    if opt.checked_add(size_opt)? > hdr.len() || size_opt < 0x70 {
        return None;
    }
    let magic = u16::from_le_bytes(hdr[opt..opt + 2].try_into().ok()?);
    if magic != 0x020b {
        return None;
    }
    let entry_rva = u32::from_le_bytes(hdr[opt + 16..opt + 20].try_into().ok()?);
    let image_base = u64::from_le_bytes(hdr[opt + 24..opt + 32].try_into().ok()?);
    let major_os = u16::from_le_bytes(hdr[opt + 40..opt + 42].try_into().ok()?);
    let minor_os = u16::from_le_bytes(hdr[opt + 42..opt + 44].try_into().ok()?);
    let major_subsys = u16::from_le_bytes(hdr[opt + 48..opt + 50].try_into().ok()?);
    let minor_subsys = u16::from_le_bytes(hdr[opt + 50..opt + 52].try_into().ok()?);
    let size_of_image = u32::from_le_bytes(hdr[opt + 56..opt + 60].try_into().ok()?);
    let size_of_headers = u32::from_le_bytes(hdr[opt + 60..opt + 64].try_into().ok()?);
    let checksum = u32::from_le_bytes(hdr[opt + 64..opt + 68].try_into().ok()?);
    let subsystem = u16::from_le_bytes(hdr[opt + 68..opt + 70].try_into().ok()?);
    let dll_characteristics = u16::from_le_bytes(hdr[opt + 70..opt + 72].try_into().ok()?);
    let stack_reserve = u64::from_le_bytes(hdr[opt + 72..opt + 80].try_into().ok()?);
    let stack_commit = u64::from_le_bytes(hdr[opt + 80..opt + 88].try_into().ok()?);

    let sec_table = opt + size_opt;
    let mut sections = Vec::new();
    for i in 0..num_sections.min(64) {
        let off = sec_table + i * 40;
        if off + 40 > hdr.len() {
            break;
        }
        sections.push(PeSectionHeader {
            virtual_size: u32::from_le_bytes(hdr[off + 8..off + 12].try_into().ok()?),
            virtual_address: u32::from_le_bytes(hdr[off + 12..off + 16].try_into().ok()?),
            size_of_raw_data: u32::from_le_bytes(hdr[off + 16..off + 20].try_into().ok()?),
            pointer_to_raw_data: u32::from_le_bytes(hdr[off + 20..off + 24].try_into().ok()?),
            characteristics: u32::from_le_bytes(hdr[off + 36..off + 40].try_into().ok()?),
        });
    }

    Some(ParsedPeHeader {
        image_base,
        size_of_image,
        entry_rva,
        stack_reserve,
        stack_commit,
        subsystem,
        minor_subsys,
        major_subsys,
        major_os,
        minor_os,
        characteristics,
        dll_characteristics,
        machine,
        checksum,
        size_of_headers,
        sections,
    })
}

fn nt_query_section(frame: &SyscallFrame) -> u64 {
    let (sec_handle, info_class, info_ptr, length) = (frame.r10, frame.rdx, frame.r8, frame.r9);
    let ret_len_ptr = read_stack_arg(frame, 4).unwrap_or(0);
    let section = multitask::with_current_user_process_state_mut(|_, _, st| {
        st.win32_objects()
            .sections
            .iter()
            .find(|s| s.handle == sec_handle)
            .cloned()
    })
    .flatten();
    let Some(section) = section else {
        return STATUS_INVALID_HANDLE;
    };
    match info_class {
        0 /* SectionBasicInformation */ => {
            let need = size_of::<SectionBasicInformation64>() as u64;
            if length < need {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let info = SectionBasicInformation64 {
                base_address: 0,
                attributes: if section.is_image { SEC_IMAGE as u32 } else { 0 },
                _pad: 0,
                size: section.size as i64,
            };
            if usermem::write_current_user_struct(info_ptr, &info).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            if ret_len_ptr != 0 {
                let _ = write_user_u64(ret_len_ptr, need);
            }
            STATUS_SUCCESS
        }
        1 /* SectionImageInformation */ => {
            let need = size_of::<SectionImageInformation64>() as u64;
            if length < need {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let Some(fd) = section.fd else {
                return STATUS_INVALID_PARAMETER;
            };
            let mut hdr = vec![0_u8; 4096];
            let Ok(read) = vfs_socket::kernel_vfs_pread_bytes(fd, 0, &mut hdr) else {
                return STATUS_INVALID_PARAMETER;
            };
            let Some(pe) = parse_pe64_headers(&hdr[..read]) else {
                return STATUS_INVALID_PARAMETER;
            };
            let info = SectionImageInformation64 {
                transfer_address: pe.image_base.saturating_add(pe.entry_rva as u64),
                zero_bits: 0,
                _pad0: 0,
                maximum_stack_size: pe.stack_reserve,
                committed_stack_size: pe.stack_commit,
                subsystem_type: pe.subsystem as u32,
                minor_subsystem_version: pe.minor_subsys,
                major_subsystem_version: pe.major_subsys,
                major_os_version: pe.major_os,
                minor_os_version: pe.minor_os,
                image_characteristics: pe.characteristics,
                dll_characteristics: pe.dll_characteristics,
                machine: pe.machine,
                image_contains_code: 1,
                image_flags: 0,
                loader_flags: 0,
                image_file_size: section.size.min(u32::MAX as u64) as u32,
                check_sum: pe.checksum,
            };
            if usermem::write_current_user_struct(info_ptr, &info).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            if ret_len_ptr != 0 {
                let _ = write_user_u64(ret_len_ptr, need);
            }
            STATUS_SUCCESS
        }
        _ => STATUS_INVALID_INFO_CLASS,
    }
}

fn nt_map_view_of_section(frame: &SyscallFrame, is_ex: bool) -> u64 {
    let (sec_handle, proc_handle, base_ptr) = (frame.r10, frame.rdx, frame.r8);
    if proc_handle != HANDLE_CURRENT_PROCESS {
        return STATUS_INVALID_HANDLE;
    }
    if base_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let (offset_ptr, view_size_ptr, win32_protect) = if is_ex {
        let offset_ptr = frame.r9;
        let view_size_ptr = read_stack_arg(frame, 4).unwrap_or(0);
        let protect = (read_stack_arg(frame, 6).unwrap_or(PAGE_READONLY) as u32) as u64;
        (offset_ptr, view_size_ptr, protect)
    } else {
        let offset_ptr = read_stack_arg(frame, 5).unwrap_or(0);
        let view_size_ptr = read_stack_arg(frame, 6).unwrap_or(0);
        let protect = (read_stack_arg(frame, 9).unwrap_or(PAGE_READONLY) as u32) as u64;
        (offset_ptr, view_size_ptr, protect)
    };
    if view_size_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }

    let section = multitask::with_current_user_process_state_mut(|_, _, st| {
        st.win32_objects()
            .sections
            .iter()
            .find(|s| s.handle == sec_handle)
            .cloned()
    })
    .flatten();
    let Some(section) = section else {
        return STATUS_INVALID_HANDLE;
    };

    let requested_base = read_user_u64(base_ptr).unwrap_or(0);
    let requested_view_size = read_user_u64(view_size_ptr).unwrap_or(0);
    let offset = if offset_ptr != 0 {
        usermem::read_current_user_struct::<u64>(offset_ptr).unwrap_or(0)
    } else {
        0
    };

    if section.is_image {
        return map_pe_image_section(
            section,
            requested_base,
            base_ptr,
            view_size_ptr,
        );
    }

    let avail = section.size.saturating_sub(offset);
    let view_bytes = if requested_view_size == 0 {
        avail
    } else {
        requested_view_size.min(avail)
    }
    .max(1);
    let Some(mapped_len) = align_up_page(view_bytes) else {
        return STATUS_NO_MEMORY;
    };
    if mapped_len > MAX_WIN32_IMAGE_BYTES {
        return STATUS_NO_MEMORY;
    }
    let Ok(pages) = usize::try_from(mapped_len / PAGE_SIZE) else {
        return STATUS_NO_MEMORY;
    };
    let final_flags = protect_to_page_flags(win32_protect).unwrap_or(PageTableFlags::NO_EXECUTE);

    let file_data = if let Some(fd) = section.fd {
        let to_read = (view_bytes as usize).min(MAX_PE_SECTION_FILE_BYTES);
        let mut buf = vec![0_u8; to_read];
        match vfs_socket::kernel_vfs_pread_bytes(fd, offset, &mut buf) {
            Ok(n) => {
                buf.truncate(n);
                Some(buf)
            }
            Err(_) => return STATUS_INVALID_PARAMETER,
        }
    } else {
        None
    };

    let mapped = multitask::with_current_user_process_state_mut(|_, _, st| {
        let base = if requested_base != 0 {
            let b = align_down_page(requested_base);
            let Some(b_end) = b.checked_add(mapped_len) else {
                return Err(STATUS_INVALID_PARAMETER);
            };
            if b < paging::USER_SPACE_BASE || b_end >= paging::USER_SPACE_END_EXCLUSIVE {
                return Err(STATUS_INVALID_PARAMETER);
            }
            if vmas_overlap(&st.win32_objects().vmas, b, b_end)
                || !st
                    .address_space()
                    .user_range_is_unmapped(VirtAddr::new(b), pages)
                    .unwrap_or(false)
            {
                return Err(STATUS_NO_MEMORY);
            }
            b
        } else {
            find_free_win32_va(
                st.address_space(),
                &st.win32_objects().vmas,
                WIN32_DYNAMIC_VA_START,
                WIN32_DYNAMIC_VA_END,
                mapped_len,
                false,
            )
            .ok_or(STATUS_NO_MEMORY)?
        };

        let temp_flags = if file_data.is_some() {
            PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE
        } else {
            final_flags
        };
        st.address_space_mut()
            .map_zeroed_user_pages_at(VirtAddr::new(base), pages, temp_flags)
            .map_err(|_| STATUS_NO_MEMORY)?;
        if let Some(ref data) = file_data
            && !data.is_empty()
        {
            st.address_space()
                .copy_into_user(VirtAddr::new(base), data)
                .map_err(|_| STATUS_NO_MEMORY)?;
            if temp_flags != final_flags {
                st.address_space_mut()
                    .protect_user_pages_at(VirtAddr::new(base), pages, final_flags)
                    .map_err(|_| STATUS_NO_MEMORY)?;
            }
        }
        st.win32_objects_mut().vmas.push(Win32MappedView {
            base,
            size: mapped_len,
            allocation_base: base,
            state: MEM_COMMIT as u32,
            protect: (win32_protect & 0xff) as u32,
            mem_type: MEM_MAPPED as u32,
        });
        Ok(base)
    });

    match mapped {
        Some(Ok(base)) => {
            let _ = write_user_u64(base_ptr, base);
            let _ = write_user_u64(view_size_ptr, mapped_len);
            STATUS_SUCCESS
        }
        Some(Err(e)) => e,
        None => STATUS_NO_MEMORY,
    }
}

fn map_pe_image_section(
    section: Win32SectionObject,
    requested_base: u64,
    base_ptr: u64,
    view_size_ptr: u64,
) -> u64 {
    let Some(fd) = section.fd else {
        return STATUS_INVALID_PARAMETER;
    };
    let file_len = usize::try_from(section.size).unwrap_or(0);
    if file_len == 0 || file_len > MAX_PE_SECTION_FILE_BYTES {
        return STATUS_NO_MEMORY;
    }
    let mut image_bytes = vec![0_u8; file_len];
    let Ok(read) = vfs_socket::kernel_vfs_pread_bytes(fd, 0, &mut image_bytes) else {
        return STATUS_INVALID_PARAMETER;
    };
    let data = &image_bytes[..read];
    let Some(pe) = parse_pe64_headers(data) else {
        return STATUS_INVALID_PARAMETER;
    };
    let Some(map_size) = align_up_page(pe.size_of_image as u64) else {
        return STATUS_NO_MEMORY;
    };
    if map_size == 0 || map_size > MAX_WIN32_IMAGE_BYTES {
        return STATUS_NO_MEMORY;
    }
    let Ok(pages) = usize::try_from(map_size / PAGE_SIZE) else {
        return STATUS_NO_MEMORY;
    };

    let preferred = if requested_base != 0 {
        align_down_page(requested_base)
    } else if pe.image_base >= WIN32_VA_FLOOR && pe.image_base < WIN32_DYNAMIC_VA_END {
        align_down_page(pe.image_base)
    } else {
        0
    };

    let result = multitask::with_current_user_process_state_mut(|_, _, st| {
        let can_use_preferred = preferred != 0
            && preferred
                .checked_add(map_size)
                .map_or(false, |end| end < paging::USER_SPACE_END_EXCLUSIVE)
            && !vmas_overlap(&st.win32_objects().vmas, preferred, preferred + map_size)
            && st
                .address_space()
                .user_range_is_unmapped(VirtAddr::new(preferred), pages)
                .unwrap_or(false);

        let target_base = if can_use_preferred {
            preferred
        } else {
            find_free_win32_va(
                st.address_space(),
                &st.win32_objects().vmas,
                WIN32_DYNAMIC_VA_START,
                WIN32_DYNAMIC_VA_END,
                map_size,
                false,
            )
            .ok_or(STATUS_NO_MEMORY)?
        };

        st.address_space_mut()
            .map_zeroed_user_pages_at(
                VirtAddr::new(target_base),
                pages,
                PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
            )
            .map_err(|_| STATUS_NO_MEMORY)?;

        let hdr_len = (pe.size_of_headers as usize).min(data.len()).min(map_size as usize);
        if hdr_len > 0 {
            st.address_space()
                .copy_into_user(VirtAddr::new(target_base), &data[..hdr_len])
                .map_err(|_| STATUS_NO_MEMORY)?;
        }
        let _ = st.address_space_mut().protect_user_pages_at(
            VirtAddr::new(target_base),
            align_up_page(hdr_len.max(1) as u64).unwrap_or(PAGE_SIZE) as usize / PAGE_SIZE as usize,
            PageTableFlags::NO_EXECUTE,
        );

        for sec in &pe.sections {
            let va = sec.virtual_address as u64;
            let span = (sec.virtual_size.max(sec.size_of_raw_data)) as u64;
            if va == 0 || span == 0 || va >= map_size {
                continue;
            }
            let copy_len = (sec.size_of_raw_data as usize)
                .min(data.len().saturating_sub(sec.pointer_to_raw_data as usize))
                .min((map_size - va) as usize);
            if copy_len > 0 {
                let raw_start = sec.pointer_to_raw_data as usize;
                st.address_space()
                    .copy_into_user(
                        VirtAddr::new(target_base + va),
                        &data[raw_start..raw_start + copy_len],
                    )
                    .map_err(|_| STATUS_NO_MEMORY)?;
            }
            let aligned_span = align_up_page(span.min(map_size - va)).unwrap_or(PAGE_SIZE);
            let sec_pages = usize::try_from(aligned_span / PAGE_SIZE).unwrap_or(0);
            if sec_pages > 0 {
                let is_exec = (sec.characteristics & 0x2000_0000) != 0;
                let is_write = (sec.characteristics & 0x8000_0000) != 0;
                let mut sec_flags = PageTableFlags::empty();
                if is_write {
                    sec_flags |= PageTableFlags::WRITABLE;
                }
                if !is_exec {
                    sec_flags |= PageTableFlags::NO_EXECUTE;
                }
                let _ = st.address_space_mut().protect_user_pages_at(
                    VirtAddr::new(target_base + align_down_page(va)),
                    sec_pages,
                    sec_flags,
                );
            }
        }

        st.win32_objects_mut().vmas.push(Win32MappedView {
            base: target_base,
            size: map_size,
            allocation_base: target_base,
            state: MEM_COMMIT as u32,
            protect: PAGE_EXECUTE_READ as u32,
            mem_type: MEM_IMAGE as u32,
        });
        Ok(target_base)
    });

    match result {
        Some(Ok(base)) => {
            let _ = write_user_u64(base_ptr, base);
            let _ = write_user_u64(view_size_ptr, map_size);
            if base != pe.image_base {
                STATUS_IMAGE_NOT_AT_BASE
            } else {
                STATUS_SUCCESS
            }
        }
        Some(Err(e)) => e,
        None => STATUS_NO_MEMORY,
    }
}

fn nt_unmap_view_of_section(proc_handle: u64, base_addr: u64) -> u64 {
    if proc_handle != HANDLE_CURRENT_PROCESS {
        return STATUS_INVALID_HANDLE;
    }
    let aligned = align_down_page(base_addr);
    let removed = multitask::with_current_user_process_state_mut(|_, _, st| {
        // Only a mapped/image view may be unmapped here; private VirtualAlloc
        // regions are released through NtFreeVirtualMemory.
        let idx = st.win32_objects().vmas.iter().position(|v| {
            v.base == aligned
                && v.allocation_base == aligned
                && v.mem_type != MEM_PRIVATE as u32
        })?;
        let view = st.win32_objects_mut().vmas.remove(idx);
        let pages = usize::try_from(view.size / PAGE_SIZE).unwrap_or(0);
        if pages > 0 {
            let _ = st
                .address_space_mut()
                .unmap_present_user_pages_at(VirtAddr::new(aligned), pages);
        }
        Some(())
    })
    .flatten()
    .is_some();
    if removed {
        STATUS_SUCCESS
    } else {
        STATUS_NOT_MAPPED_VIEW
    }
}

fn nt_are_mapped_files_the_same(addr1: u64, addr2: u64) -> u64 {
    if align_down_page(addr1) == align_down_page(addr2) {
        STATUS_SUCCESS
    } else {
        0xC000_00D4 // STATUS_NOT_SAME_DEVICE
    }
}

fn map_nls_file_readonly(path: &str) -> Result<(u64, usize), u64> {
    let fd = vfs_socket::syscall_linux_vfs_openat_str(
        linux_abi::AT_FDCWD as u64,
        path,
        linux_abi::O_RDONLY,
        0,
    )
    .map_err(linux_errno_to_ntstatus)?;
    let stat = match vfs_meta::kernel_vfs_fstat(fd) {
        Ok(s) => s,
        Err(e) => {
            let _ = vfs_socket::syscall_linux_vfs_close(fd);
            return Err(linux_errno_to_ntstatus(e));
        }
    };
    let len = usize::try_from(stat.st_size.max(0)).unwrap_or(0);
    if len == 0 || len > MAX_NLS_FILE_BYTES {
        let _ = vfs_socket::syscall_linux_vfs_close(fd);
        return Err(STATUS_OBJECT_NAME_NOT_FOUND);
    }
    let mut data = vec![0_u8; len];
    let read_res = vfs_socket::kernel_vfs_pread_bytes(fd, 0, &mut data);
    let _ = vfs_socket::syscall_linux_vfs_close(fd);
    let n = read_res.map_err(linux_errno_to_ntstatus)?;
    data.truncate(n);

    let map_len = align_up_page(data.len() as u64).ok_or(STATUS_NO_MEMORY)?;
    let pages = usize::try_from(map_len / PAGE_SIZE).map_err(|_| STATUS_NO_MEMORY)?;
    let mapped = multitask::with_current_user_process_state_mut(|_, _, st| {
        let base = find_free_win32_va(
            st.address_space(),
            &st.win32_objects().vmas,
            WIN32_DYNAMIC_VA_START,
            WIN32_DYNAMIC_VA_END,
            map_len,
            false,
        )
        .ok_or(STATUS_NO_MEMORY)?;
        st.address_space_mut()
            .map_zeroed_user_pages_at(
                VirtAddr::new(base),
                pages,
                PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
            )
            .map_err(|_| STATUS_NO_MEMORY)?;
        st.address_space()
            .copy_into_user(VirtAddr::new(base), &data)
            .map_err(|_| STATUS_NO_MEMORY)?;
        st.address_space_mut()
            .protect_user_pages_at(VirtAddr::new(base), pages, PageTableFlags::NO_EXECUTE)
            .map_err(|_| STATUS_NO_MEMORY)?;
        st.win32_objects_mut().vmas.push(Win32MappedView {
            base,
            size: map_len,
            allocation_base: base,
            state: MEM_COMMIT as u32,
            protect: PAGE_READONLY as u32,
            mem_type: MEM_MAPPED as u32,
        });
        Ok((base, data.len()))
    });
    mapped.unwrap_or(Err(STATUS_NO_MEMORY))
}

fn nt_initialize_nls_files(addr_ptr: u64, lcid_ptr: u64, size_ptr: u64) -> u64 {
    if addr_ptr == 0 || lcid_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let (base, len) = match map_nls_file_readonly("/compat/windows/System32/locale.nls") {
        Ok(v) => v,
        Err(status) => return status,
    };
    if write_user_u64(addr_ptr, base) != STATUS_SUCCESS
        || write_user_u32_checked(lcid_ptr, 0x0409) != STATUS_SUCCESS
    {
        return STATUS_INVALID_PARAMETER;
    }
    if size_ptr != 0 {
        let _ = write_user_u64(size_ptr, len as u64);
    }
    STATUS_SUCCESS
}

fn nt_get_nls_section_ptr(frame: &SyscallFrame) -> u64 {
    let (sec_type, sec_id, _unk, ptr_out) = (
        (frame.r10 as u32) as u64,
        (frame.rdx as u32) as u64,
        frame.r8,
        frame.r9,
    );
    let size_out = read_stack_arg(frame, 4).unwrap_or(0);
    if ptr_out == 0 || size_out == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let path = match sec_type {
        9 => String::from("/compat/windows/System32/sortdefault.nls"),
        10 => String::from("/compat/windows/System32/l_intl.nls"),
        11 => format!("/compat/windows/System32/c_{:03}.nls", sec_id as u32),
        12 => {
            let name = match sec_id {
                1 => "normnfc.nls",
                2 => "normnfd.nls",
                5 => "normnfkc.nls",
                6 => "normnfkd.nls",
                13 => "normidna.nls",
                _ => return STATUS_INVALID_PARAMETER,
            };
            format!("/compat/windows/System32/{name}")
        }
        _ => return STATUS_INVALID_PARAMETER,
    };
    let (base, len) = match map_nls_file_readonly(&path) {
        Ok(v) => v,
        Err(status) => return status,
    };
    if write_user_u64(ptr_out, base) != STATUS_SUCCESS
        || write_user_u64(size_out, len as u64) != STATUS_SUCCESS
    {
        return STATUS_INVALID_PARAMETER;
    }
    STATUS_SUCCESS
}

fn nt_query_system_information(
    info_class: u64,
    buf_ptr: u64,
    buf_len: u64,
    ret_len_ptr: u64,
    online_mask: u64,
) -> u64 {
    let info_class = (info_class as u32) as u64;
    let buf_len = (buf_len as u32) as u64;
    let online_count = online_mask.count_ones() as u8;
    match info_class {
        0 /* SystemBasicInformation */ => {
            let info = WindowsSystemBasicInformation::from_online_count(online_count);
            let need = WindowsSystemBasicInformation::BYTES as u64;
            if ret_len_ptr != 0 {
                let _ = write_user_u32_checked(ret_len_ptr, need as u32);
            }
            if buf_len < need {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            if usermem::write_current_user_bytes(buf_ptr, as_bytes(&info)).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            STATUS_SUCCESS
        }
        3 /* SystemTimeOfDayInformation */ => {
            let need = size_of::<SystemTimeOfDayInformation64>() as u64;
            if ret_len_ptr != 0 {
                let _ = write_user_u32_checked(ret_len_ptr, need as u32);
            }
            if buf_len < need {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let info = SystemTimeOfDayInformation64 {
                boot_time: FILE_EPOCH_BIAS_100NS,
                current_time: current_filetime_100ns(),
                ..SystemTimeOfDayInformation64::default()
            };
            if usermem::write_current_user_struct(buf_ptr, &info).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            STATUS_SUCCESS
        }
        73 /* SystemLogicalProcessorInformation */ => {
            // Single RelationProcessorCore record (32 bytes on x64).
            let need = 32_u64;
            if ret_len_ptr != 0 {
                let _ = write_user_u32_checked(ret_len_ptr, need as u32);
            }
            if buf_len < need {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let mut rec = [0_u8; 32];
            rec[0..8].copy_from_slice(&online_mask.to_le_bytes());
            // relationship = 0 (RelationProcessorCore), rest = 0
            if usermem::write_current_user_bytes(buf_ptr, &rec).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            STATUS_SUCCESS
        }
        107 /* SystemLogicalProcessorInformationEx */ => {
            if ret_len_ptr != 0 {
                let _ = write_user_u32_checked(ret_len_ptr, 0);
            }
            let _ = size_of::<SystemLogicalProcessorInfoExHeader>();
            STATUS_NOT_IMPLEMENTED
        }
        _ => STATUS_INVALID_INFO_CLASS,
    }
}

fn nt_query_information_process(frame: &SyscallFrame) -> u64 {
    let (proc_handle, info_class, info_ptr, length) = (
        frame.r10,
        (frame.rdx as u32) as u64,
        frame.r8,
        (frame.r9 as u32) as u64,
    );
    let ret_len_ptr = read_stack_arg(frame, 4).unwrap_or(0);
    let Some(target_pid) = process_thread::resolve_process_id(proc_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let cur_pid = multitask::current_user_process_id().unwrap_or(1);
    if target_pid != cur_pid {
        if info_class == 0 /* ProcessBasicInformation */ {
            let need = size_of::<ProcessBasicInformation64>() as u64;
            if ret_len_ptr != 0 {
                let _ = write_user_u32_checked(ret_len_ptr, need as u32);
            }
            if length < need {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let exit_status = if multitask::user_target_abi_if_alive(target_pid, 0).is_some() {
                259
            } else {
                0
            };
            let pbi = ProcessBasicInformation64 {
                exit_status,
                affinity_mask: kernel_hal::api::cpu::admitted_online_mask(),
                base_priority: 8,
                unique_process_id: target_pid,
                inherited_from_unique_process_id: 1,
                ..ProcessBasicInformation64::default()
            };
            if usermem::write_current_user_struct(info_ptr, &pbi).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            return STATUS_SUCCESS;
        }
        return STATUS_INVALID_INFO_CLASS;
    }
    let pid = cur_pid;
    let rt = multitask::with_current_user_process_state_mut(|_, _, st| st.windows_runtime())
        .flatten();
    let Some(rt) = rt else {
        return STATUS_INVALID_HANDLE;
    };

    match info_class {
        0 /* ProcessBasicInformation */ => {
            let need = size_of::<ProcessBasicInformation64>() as u64;
            if ret_len_ptr != 0 {
                let _ = write_user_u32_checked(ret_len_ptr, need as u32);
            }
            if length < need {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let pbi = ProcessBasicInformation64 {
                exit_status: 259, // STILL_ACTIVE
                peb_base_address: rt.peb_address,
                affinity_mask: kernel_hal::api::cpu::admitted_online_mask(),
                base_priority: 8,
                unique_process_id: pid,
                inherited_from_unique_process_id: 1,
            };
            if usermem::write_current_user_struct(info_ptr, &pbi).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            STATUS_SUCCESS
        }
        7 /* ProcessDebugPort */ | 26 /* ProcessWow64Information */ => {
            if ret_len_ptr != 0 {
                let _ = write_user_u32_checked(ret_len_ptr, 8);
            }
            if length < 8 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            write_user_u64(info_ptr, 0)
        }
        30 /* ProcessDebugObjectHandle */ => STATUS_OBJECT_NAME_NOT_FOUND,
        31 /* ProcessDebugFlags */ => {
            if ret_len_ptr != 0 {
                let _ = write_user_u32_checked(ret_len_ptr, 4);
            }
            if length < 4 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            write_user_u32_checked(info_ptr, 1)
        }
        36 /* ProcessCookie */ => {
            if ret_len_ptr != 0 {
                let _ = write_user_u32_checked(ret_len_ptr, 4);
            }
            if length < 4 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let cookie = ((pid ^ 0x5255_5354) as u32).max(1);
            write_user_u32_checked(info_ptr, cookie)
        }
        37 /* ProcessImageInformation */ => {
            let need = size_of::<SectionImageInformation64>() as u64;
            if ret_len_ptr != 0 {
                let _ = write_user_u32_checked(ret_len_ptr, need as u32);
            }
            if length < need {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let info = SectionImageInformation64 {
                transfer_address: rt.main_module_entry_address,
                maximum_stack_size: 1024 * 1024,
                committed_stack_size: 64 * 1024,
                subsystem_type: 3, // IMAGE_SUBSYSTEM_WINDOWS_CUI
                major_subsystem_version: 10,
                major_os_version: 10,
                machine: 0x8664,
                image_contains_code: 1,
                image_file_size: rt.image_size.min(u32::MAX as u64) as u32,
                ..SectionImageInformation64::default()
            };
            if usermem::write_current_user_struct(info_ptr, &info).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            STATUS_SUCCESS
        }
        _ => STATUS_INVALID_INFO_CLASS,
    }
}

fn nt_set_information_process(
    proc_handle: u64,
    info_class: u64,
    info_ptr: u64,
    length: u64,
) -> u64 {
    let info_class = (info_class as u32) as u64;
    let length = (length as u32) as u64;
    let Some(target_pid) = process_thread::resolve_process_id(proc_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    if target_pid != multitask::current_user_process_id().unwrap_or(1) {
        return STATUS_INVALID_HANDLE;
    }
    if info_class == 21 /* ProcessAffinityMask */ && length >= 8 {
        let Ok(mask) = read_user_u64(info_ptr) else {
            return STATUS_INVALID_PARAMETER;
        };
        let online = kernel_hal::api::cpu::admitted_online_mask();
        return match multitask::set_windows_process_affinity(mask, online) {
            Ok(_) => STATUS_SUCCESS,
            Err(e) => affinity_error_to_ntstatus(e),
        };
    }
    STATUS_SUCCESS
}

fn nt_query_information_thread(frame: &SyscallFrame) -> u64 {
    let (thread_handle, info_class, info_ptr, length) = (
        frame.r10,
        (frame.rdx as u32) as u64,
        frame.r8,
        (frame.r9 as u32) as u64,
    );
    let ret_len_ptr = read_stack_arg(frame, 4).unwrap_or(0);
    let Some(tid) = process_thread::resolve_thread_id(thread_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let pid = multitask::current_user_process_id().unwrap_or(1);
    let cur_tid = multitask::current_user_thread_id().unwrap_or(1);
    let teb_override = if tid != cur_tid {
        multitask::with_current_user_process_state_mut(|_, _, st| {
            st.win32_objects()
                .threads
                .iter()
                .find(|t| t.tid == tid)
                .map(|t| t.teb_address)
        })
        .flatten()
    } else {
        None
    };
    match info_class {
        0 /* ThreadBasicInformation (48 bytes on x64) */ => {
            if ret_len_ptr != 0 {
                let _ = write_user_u32_checked(ret_len_ptr, 48);
            }
            if length < 48 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let teb = teb_override.unwrap_or_else(|| {
                multitask::with_current_user_process_state_mut(|_, _, st| {
                    st.windows_runtime().map(|r| r.teb_address).unwrap_or(0)
                })
                .unwrap_or(0)
            });
            let alive = tid == cur_tid || multitask::is_user_task_alive(tid);
            let exit_status: u32 = if alive { 259 } else { 0 };
            let mut buf = [0_u8; 48];
            buf[0..4].copy_from_slice(&exit_status.to_le_bytes()); // STILL_ACTIVE if alive
            buf[8..16].copy_from_slice(&teb.to_le_bytes());
            buf[16..24].copy_from_slice(&pid.to_le_bytes());
            buf[24..32].copy_from_slice(&tid.to_le_bytes());
            buf[32..40].copy_from_slice(&kernel_hal::api::cpu::admitted_online_mask().to_le_bytes());
            if usermem::write_current_user_bytes(info_ptr, &buf).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            STATUS_SUCCESS
        }
        9 /* ThreadQuerySetWin32StartAddress */ => {
            if ret_len_ptr != 0 {
                let _ = write_user_u32_checked(ret_len_ptr, 8);
            }
            if length < 8 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let entry = multitask::with_current_user_process_state_mut(|_, _, st| {
                st.windows_runtime()
                    .map(|r| r.main_module_entry_address)
                    .unwrap_or(0)
            })
            .unwrap_or(0);
            write_user_u64(info_ptr, entry)
        }
        12 /* ThreadAmILastThread */ => {
            if length < 4 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let last = (multitask::current_user_process_thread_count().unwrap_or(1) <= 1) as u32;
            write_user_u32_checked(info_ptr, last)
        }
        _ => STATUS_INVALID_INFO_CLASS,
    }
}

fn nt_set_information_thread(
    thread_handle: u64,
    info_class: u64,
    info_ptr: u64,
    length: u64,
) -> u64 {
    let Some(target_tid) = process_thread::resolve_thread_id(thread_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let cur_tid = multitask::current_user_thread_id().unwrap_or(1);
    if target_tid == cur_tid && info_class == 4 /* ThreadAffinityMask */ && length >= 8 {
        let Ok(mask) = read_user_u64(info_ptr) else {
            return STATUS_INVALID_PARAMETER;
        };
        let online = kernel_hal::api::cpu::admitted_online_mask();
        return match multitask::set_windows_current_thread_affinity(mask, online) {
            Ok(_) => STATUS_SUCCESS,
            Err(e) => affinity_error_to_ntstatus(e),
        };
    }
    STATUS_SUCCESS
}

fn nt_terminate_process(proc_handle: u64, exit_status: u64) -> u64 {
    if proc_handle == 0 {
        // Wine RtlExitUserProcess calls NtTerminateProcess(0, status) first before
        // running LdrShutdownProcess(), then calls NtTerminateProcess(-1, status).
        return STATUS_SUCCESS;
    }
    if let Some(pid) = process_thread::resolve_process_id(proc_handle) {
        let cur_pid = multitask::current_user_process_id().unwrap_or(0);
        if pid == cur_pid {
            let tid = multitask::current_user_thread_id().unwrap_or(0);
            sync::notify_task_exited(pid, tid, true);
            let code = ((exit_status as u32) & 0xff) << 8;
            super::super::linux::record_linux_process_termination(pid, code as i32);
            multitask::exit_current_user_process()
        }
    }
    STATUS_INVALID_HANDLE
}

fn nt_terminate_thread(thread_handle: u64, _exit_status: u64) -> u64 {
    if thread_handle == 0 {
        if let (Some(pid), Some(tid)) = (
            multitask::current_user_process_id(),
            multitask::current_user_thread_id(),
        ) {
            sync::notify_task_exited(pid, tid, false);
        }
        multitask::exit_current_user_task();
    }
    if let Some(tid) = process_thread::resolve_thread_id(thread_handle) {
        let cur_tid = multitask::current_user_thread_id().unwrap_or(0);
        if tid == cur_tid {
            if let Some(pid) = multitask::current_user_process_id() {
                sync::notify_task_exited(pid, tid, false);
            }
            multitask::exit_current_user_task();
        }
    }
    STATUS_INVALID_HANDLE
}

fn nt_delay_execution(_alertable: u64, interval_ptr: u64) -> u64 {
    let Ok(interval) = usermem::read_current_user_struct::<i64>(interval_ptr) else {
        return STATUS_INVALID_PARAMETER;
    };
    let rel_100ns = if interval < 0 {
        interval.saturating_neg() as u64
    } else {
        let now = current_filetime_100ns();
        interval.saturating_sub(now).max(0) as u64
    };
    if rel_100ns == 0 {
        multitask::request_yield_reschedule();
        return STATUS_SUCCESS;
    }
    let nanos = rel_100ns.saturating_mul(100);
    let ts = LinuxTimespecWire {
        tv_sec: (nanos / 1_000_000_000) as i64,
        tv_nsec: (nanos % 1_000_000_000) as i64,
    };
    process_time::sleep_relative_timespec_substrate(ts);
    STATUS_SUCCESS
}

pub(super) fn current_filetime_100ns() -> i64 {
    let ts = process_time::realtime_timespec();
    FILE_EPOCH_BIAS_100NS
        .saturating_add(ts.tv_sec.saturating_mul(10_000_000))
        .saturating_add(ts.tv_nsec / 100)
}

fn nt_query_performance_counter(counter_ptr: u64, freq_ptr: u64) -> u64 {
    if counter_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let ts = process_time::monotonic_timespec();
    let ticks_10mhz = ts
        .tv_sec
        .saturating_mul(10_000_000)
        .saturating_add(ts.tv_nsec / 100);
    if write_user_i64(counter_ptr, ticks_10mhz) != STATUS_SUCCESS {
        return STATUS_INVALID_PARAMETER;
    }
    if freq_ptr != 0 && write_user_i64(freq_ptr, 10_000_000) != STATUS_SUCCESS {
        return STATUS_INVALID_PARAMETER;
    }
    STATUS_SUCCESS
}

pub(super) fn nt_continue(frame: &mut SyscallFrame, context_ptr: u64) -> u64 {
    if context_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    // Windows x64 CONTEXT struct is 1232 bytes; integer + RIP fields span 0x030..0x100.
    let mut ctx = [0_u8; 0x100];
    if usermem::copy_from_current_user_exact(context_ptr, &mut ctx).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    let rflags_raw = u32::from_le_bytes(ctx[0x44..0x48].try_into().unwrap()) as u64;
    let candidate = SyscallFrame {
        user_rsp: u64::from_le_bytes(ctx[0x98..0xa0].try_into().unwrap()),
        rax: u64::from_le_bytes(ctx[0x78..0x80].try_into().unwrap()),
        rbx: u64::from_le_bytes(ctx[0x90..0x98].try_into().unwrap()),
        user_rip: u64::from_le_bytes(ctx[0xf8..0x100].try_into().unwrap()),
        rdx: u64::from_le_bytes(ctx[0x88..0x90].try_into().unwrap()),
        rsi: u64::from_le_bytes(ctx[0xa8..0xb0].try_into().unwrap()),
        rdi: u64::from_le_bytes(ctx[0xb0..0xb8].try_into().unwrap()),
        rbp: u64::from_le_bytes(ctx[0xa0..0xa8].try_into().unwrap()),
        r8: u64::from_le_bytes(ctx[0xb8..0xc0].try_into().unwrap()),
        r9: u64::from_le_bytes(ctx[0xc0..0xc8].try_into().unwrap()),
        r10: u64::from_le_bytes(ctx[0xc8..0xd0].try_into().unwrap()),
        user_rflags: (rflags_raw & !USER_RFLAGS_FORBIDDEN_MASK)
            | USER_RFLAGS_RESERVED_BIT_1
            | (1 << 9),
        r12: u64::from_le_bytes(ctx[0xd8..0xe0].try_into().unwrap()),
        r13: u64::from_le_bytes(ctx[0xe0..0xe8].try_into().unwrap()),
        r14: u64::from_le_bytes(ctx[0xe8..0xf0].try_into().unwrap()),
        r15: u64::from_le_bytes(ctx[0xf0..0xf8].try_into().unwrap()),
    };
    if syscall_return_contract(&candidate).is_none() {
        return STATUS_INVALID_PARAMETER;
    }
    *frame = candidate;
    frame.rax
}

pub(super) fn linux_errno_to_ntstatus(errno: i64) -> u64 {
    match errno {
        LINUX_ENOENT | LINUX_ENOTDIR => STATUS_OBJECT_NAME_NOT_FOUND,
        LINUX_EACCES | LINUX_EPERM => STATUS_ACCESS_DENIED,
        LINUX_ENOMEM => STATUS_NO_MEMORY,
        LINUX_EBADF => STATUS_INVALID_HANDLE,
        // Linux x86-64 errno values; these have no shared constants here.
        16 /* EBUSY */ => 0xc000_0043,          // STATUS_SHARING_VIOLATION
        17 /* EEXIST */ => 0xc000_0035,         // STATUS_OBJECT_NAME_COLLISION
        18 /* EXDEV */ => 0xc000_00d4,          // STATUS_NOT_SAME_DEVICE
        21 /* EISDIR */ => 0xc000_00ba,         // STATUS_FILE_IS_A_DIRECTORY
        22 /* EINVAL */ => STATUS_INVALID_PARAMETER,
        23 | 24 /* ENFILE | EMFILE */ => 0xc000_011f, // STATUS_TOO_MANY_OPENED_FILES
        28 /* ENOSPC */ => 0xc000_007f,         // STATUS_DISK_FULL
        30 /* EROFS */ => 0xc000_00a2,          // STATUS_MEDIA_WRITE_PROTECTED
        32 /* EPIPE */ => 0xc000_014b,          // STATUS_PIPE_BROKEN
        36 /* ENAMETOOLONG */ => 0xc000_0033,   // STATUS_OBJECT_NAME_INVALID
        39 /* ENOTEMPTY */ => 0xc000_0101,      // STATUS_DIRECTORY_NOT_EMPTY
        _ => STATUS_OBJECT_PATH_NOT_FOUND,
    }
}

fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

fn read_unaligned<T: Copy>(bytes: &[u8]) -> T {
    debug_assert!(bytes.len() >= size_of::<T>());
    unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::syscall::{encode_two_path_payload, decode_two_path_payload, VfsStatFsWire};

    #[test]
    fn encode_two_path_payload_roundtrip_and_bounds() {
        let mut buf = [0u8; 512];
        let len = encode_two_path_payload(b"/tmp/old.txt", b"/tmp/new.txt", &mut buf)
            .expect("should encode two short paths");
        assert_eq!(len, 4 + 12 + 12);
        let (first, second) = decode_two_path_payload(&buf[..len]).expect("should decode");
        assert_eq!(first, b"/tmp/old.txt");
        assert_eq!(second, b"/tmp/new.txt");

        let mut small = [0u8; 10];
        assert!(encode_two_path_payload(b"/tmp/a", b"/tmp/b", &mut small).is_none());
    }

    #[test]
    fn parse_file_rename_info_and_resolve_target() {
        let name: Vec<u16> = "renamed.txt".encode_utf16().collect();
        let name_bytes_len = name.len() * 2;
        let mut buf = vec![0u8; 20 + name_bytes_len];
        buf[0] = 1; // ReplaceIfExists = TRUE
        buf[8..16].copy_from_slice(&0u64.to_le_bytes());
        buf[16..20].copy_from_slice(&(name_bytes_len as u32).to_le_bytes());
        for (i, ch) in name.iter().enumerate() {
            buf[20 + i * 2..22 + i * 2].copy_from_slice(&ch.to_le_bytes());
        }
        let (replace, root_dir, parsed) = parse_file_rename_info(&buf).expect("valid rename info");
        assert!(replace);
        assert_eq!(root_dir, 0);
        assert_eq!(parsed, "renamed.txt");
        assert_eq!(
            resolve_rename_target_posix("/data/dir/old.txt", root_dir, &parsed),
            "/data/dir/renamed.txt"
        );
    }

    #[test]
    fn encode_volume_size_and_attribute_info() {
        let statfs = VfsStatFsWire {
            f_type: 0x4d44, // MSDOS_SUPER_MAGIC
            f_bsize: 4096,
            f_blocks: 1024,
            f_bfree: 512,
            f_bavail: 500,
            f_namelen: 255,
            f_frsize: 4096,
            ..Default::default()
        };
        let size_bytes = encode_file_fs_size_info(&statfs);
        assert_eq!(u64::from_le_bytes(size_bytes[0..8].try_into().unwrap()), 1024);
        assert_eq!(u64::from_le_bytes(size_bytes[8..16].try_into().unwrap()), 500);
        assert_eq!(u32::from_le_bytes(size_bytes[16..20].try_into().unwrap()), 8);
        assert_eq!(u32::from_le_bytes(size_bytes[20..24].try_into().unwrap()), 512);

        let (attr_bytes, overflow) =
            encode_file_fs_attribute_info(&statfs, 64).expect("attribute info");
        assert!(!overflow);
        assert_eq!(u32::from_le_bytes(attr_bytes[0..4].try_into().unwrap()), 6);
        assert_eq!(i32::from_le_bytes(attr_bytes[4..8].try_into().unwrap()), 255);
        assert_eq!(u32::from_le_bytes(attr_bytes[8..12].try_into().unwrap()), 10);
        assert_eq!(attr_bytes.len(), 22);
    }

    #[test]
    fn desired_access_wants_write_includes_generic_all() {
        assert!(nt_desired_access_wants_write(0x1000_0000)); // GENERIC_ALL
        assert!(nt_desired_access_wants_write(0x4000_0000)); // GENERIC_WRITE
        assert!(nt_desired_access_wants_write(0x0000_0002)); // FILE_WRITE_DATA
        assert!(nt_desired_access_wants_write(0x0000_0004)); // FILE_APPEND_DATA
        assert!(nt_desired_access_wants_write(0x0001_0000)); // DELETE
        assert!(!nt_desired_access_wants_write(0x8000_0000)); // GENERIC_READ
        assert!(!nt_desired_access_wants_write(0x0000_0001)); // FILE_READ_DATA
    }
}
