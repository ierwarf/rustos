use alloc::vec;

use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;

use crate::debug;
use crate::io::session::ConsoleSessionHandle;
use crate::memory::paging::{self, AddressSpaceError, ProcessAddressSpace};
use crate::multitask;
use crate::user::abi::UserAbi;
use crate::user::handles::VfsFileHandle;
use crate::user::linux::{LinuxProcessImageInfo, LinuxProcessLaunch};
use crate::user::process_state::{ProcessSecurityContext, WindowsProcessRuntimeState};
use crate::vfs;

mod linux;

const PAGE_SIZE: u64 = 4096;
const MAX_LOAD_SEGMENTS: usize = 32;
const USER_STACK_GUARD_PAGES: usize = 1;
const USER_STACK_RESERVE_PAGES: usize = 256;
// Rust std userspace binaries can reserve tens of KiB in a single frame,
// especially around Vec/array-heavy state synchronization paths.
const USER_STACK_INITIAL_COMMIT_PAGES: usize = 64;
const USER_STACK_TOP_EXCLUSIVE: u64 = paging::USER_SPACE_END_EXCLUSIVE;

#[derive(Debug)]
pub enum ProcessLoadError {
    InvalidElf(&'static str),
    InvalidPe(&'static str),
    InterpreterLoad {
        path: [u8; 128],
        path_len: usize,
        error: vfs::VfsError,
    },
    AddressSpace(AddressSpaceError),
    UnsupportedImport {
        dll: [u8; 32],
        dll_len: usize,
        function: [u8; 64],
        function_len: usize,
    },
    Spawn(multitask::SpawnTaskError),
}

impl From<AddressSpaceError> for ProcessLoadError {
    fn from(value: AddressSpaceError) -> Self {
        Self::AddressSpace(value)
    }
}

impl From<multitask::SpawnTaskError> for ProcessLoadError {
    fn from(value: multitask::SpawnTaskError) -> Self {
        Self::Spawn(value)
    }
}

impl ProcessLoadError {
    #[allow(dead_code)]
    pub fn summary(&self) -> &'static str {
        match self {
            Self::InvalidElf(reason) => reason,
            Self::InvalidPe(reason) => reason,
            Self::InterpreterLoad { .. } => "failed to load ELF interpreter",
            Self::AddressSpace(AddressSpaceError::ZeroSizedAllocation) => {
                "zero-sized user allocation"
            }
            Self::AddressSpace(AddressSpaceError::AddressOverflow) => {
                "user address calculation overflow"
            }
            Self::AddressSpace(AddressSpaceError::AddressOutOfRange) => {
                "user address outside supported range"
            }
            Self::AddressSpace(AddressSpaceError::AddressNotPageAligned) => {
                "user address is not page aligned"
            }
            Self::AddressSpace(AddressSpaceError::AlreadyMapped) => {
                "user page range is already mapped"
            }
            Self::AddressSpace(AddressSpaceError::NotMapped) => "user page range is not mapped",
            Self::AddressSpace(AddressSpaceError::ProtectionViolation) => {
                "user page access permissions are invalid"
            }
            Self::AddressSpace(AddressSpaceError::HugePageConflict) => {
                "user mapping conflicts with huge page"
            }
            Self::AddressSpace(AddressSpaceError::OutOfFrames) => {
                "process frame allocator is exhausted"
            }
            Self::AddressSpace(AddressSpaceError::InvalidFrameOwnership) => {
                "process address space frame ownership is corrupted"
            }
            Self::UnsupportedImport { .. } => "PE import is not supported yet",
            Self::Spawn(err) => err.summary(),
        }
    }

    #[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
    pub fn log_debug_details(&self) {
        match self {
            Self::InterpreterLoad {
                path,
                path_len,
                error,
            } => {
                let path = core::str::from_utf8(&path[..*path_len]).unwrap_or("<non-utf8>");
                debug::println!("failed to load ELF interpreter {}: {:?}", path, error);
            }
            Self::UnsupportedImport {
                dll,
                dll_len,
                function,
                function_len,
            } => {
                let dll_name = core::str::from_utf8(&dll[..*dll_len]).unwrap_or("<non-utf8>");
                let function_name =
                    core::str::from_utf8(&function[..*function_len]).unwrap_or("<non-utf8>");
                debug::println!("unsupported PE import: {}!{}", dll_name, function_name,);
            }
            Self::Spawn(err) => {
                debug::println!("failed to spawn user process: {:?}", err);
            }
            _ => {
                debug::println!("failed to load user process: {}", self.summary());
            }
        }
    }
}

pub struct LoadedProcessImage {
    pub abi: UserAbi,
    pub address_space: ProcessAddressSpace,
    pub entry: VirtAddr,
    runtime: LoadedProcessRuntime,
}

// Spawn metadata stays richer than current call sites so loader/runtime APIs can grow without
// reshaping this return type.
#[allow(dead_code)]
pub struct SpawnedProcess {
    pub abi: UserAbi,
    pub pid: u64,
    pub entry: VirtAddr,
}

pub struct PreparedProcessImage {
    pub abi: UserAbi,
    pub entry: VirtAddr,
    pub address_space: ProcessAddressSpace,
    pub bootstrap: multitask::UserTaskBootstrap,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessStartRegisters {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}

impl ProcessStartRegisters {
    pub const fn new() -> Self {
        Self {
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rbp: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
        }
    }

    fn into_task_registers(self) -> multitask::UserTaskRegisters {
        multitask::UserTaskRegisters {
            rax: self.rax,
            rbx: self.rbx,
            rcx: self.rcx,
            rdx: self.rdx,
            rsi: self.rsi,
            rdi: self.rdi,
            rbp: self.rbp,
            r8: self.r8,
            r9: self.r9,
            r10: self.r10,
            r11: self.r11,
            r12: self.r12,
            r13: self.r13,
            r14: self.r14,
            r15: self.r15,
        }
    }
}

#[derive(Clone)]
enum LoadedProcessRuntime {
    Linux(LinuxProcessImageInfo),
    Windows(WindowsProcessImageInfo),
}

#[derive(Clone)]
struct WindowsProcessImageInfo {
    image_base: u64,
    image_size: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct ProcessLaunchOptions<'a> {
    pub registers: ProcessStartRegisters,
    pub linux: LinuxProcessLaunch<'a>,
    pub(crate) console_session: ConsoleSessionHandle,
    pub logical_admin: bool,
}

impl<'a> Default for ProcessLaunchOptions<'a> {
    fn default() -> Self {
        Self {
            registers: ProcessStartRegisters::new(),
            linux: LinuxProcessLaunch::new(""),
            console_session: ConsoleSessionHandle::SYSTEM,
            logical_admin: false,
        }
    }
}

#[inline(never)]
pub fn load_image(image: &[u8]) -> Result<LoadedProcessImage, ProcessLoadError> {
    if image.starts_with(b"\x7FELF") {
        return linux::load_elf(image);
    }
    if image.starts_with(b"MZ") {
        return load_pe_metadata(image);
    }

    Err(ProcessLoadError::InvalidPe(
        "unknown executable image format",
    ))
}

pub fn spawn_process_with_launch(
    image: &[u8],
    weight_micros: u64,
    launch: ProcessLaunchOptions<'_>,
) -> Result<SpawnedProcess, ProcessLoadError> {
    let prepared = prepare_process_with_launch(image, launch)?;
    spawn_prepared_process(prepared, weight_micros)
}

pub fn spawn_process_file_with_launch(
    file: VfsFileHandle,
    weight_micros: u64,
    launch: ProcessLaunchOptions<'_>,
) -> Result<SpawnedProcess, ProcessLoadError> {
    let prepared = prepare_process_file_with_launch(file, launch)?;
    spawn_prepared_process(prepared, weight_micros)
}

pub fn prepare_process_with_launch(
    image: &[u8],
    launch: ProcessLaunchOptions<'_>,
) -> Result<PreparedProcessImage, ProcessLoadError> {
    prepare_loaded_process_with_launch(load_image(image)?, launch)
}

pub fn prepare_process_with_address_space(
    image: &[u8],
    address_space: ProcessAddressSpace,
    launch: ProcessLaunchOptions<'_>,
) -> Result<PreparedProcessImage, ProcessLoadError> {
    let mut loaded = load_image(image)?;
    if matches!(loaded.abi, UserAbi::Linux | UserAbi::Windows) {
        loaded.address_space = address_space;
    }
    prepare_loaded_process_with_launch(loaded, launch)
}

pub fn prepare_process_file_with_launch(
    file: VfsFileHandle,
    launch: ProcessLaunchOptions<'_>,
) -> Result<PreparedProcessImage, ProcessLoadError> {
    prepare_loaded_process_with_launch(load_image_file(file)?, launch)
}

pub fn prepare_process_file_with_address_space(
    file: VfsFileHandle,
    address_space: ProcessAddressSpace,
    launch: ProcessLaunchOptions<'_>,
) -> Result<PreparedProcessImage, ProcessLoadError> {
    let mut loaded = load_image_file(file)?;
    if loaded.abi == UserAbi::Linux {
        loaded.address_space = address_space;
    }
    prepare_loaded_process_with_launch(loaded, launch)
}

pub fn spawn_prepared_process(
    prepared: PreparedProcessImage,
    weight_micros: u64,
) -> Result<SpawnedProcess, ProcessLoadError> {
    let pid =
        multitask::spawn_user_process(prepared.address_space, prepared.bootstrap, weight_micros)?;

    Ok(SpawnedProcess {
        abi: prepared.abi,
        pid,
        entry: prepared.entry,
    })
}

fn prepare_loaded_process_with_launch(
    mut loaded: LoadedProcessImage,
    launch: ProcessLaunchOptions<'_>,
) -> Result<PreparedProcessImage, ProcessLoadError> {
    debug_assert!(USER_STACK_RESERVE_PAGES > USER_STACK_INITIAL_COMMIT_PAGES);
    debug_assert!(
        USER_STACK_RESERVE_PAGES - USER_STACK_INITIAL_COMMIT_PAGES >= USER_STACK_GUARD_PAGES
    );

    let reserve_start =
        VirtAddr::new(USER_STACK_TOP_EXCLUSIVE - USER_STACK_RESERVE_PAGES as u64 * PAGE_SIZE);
    ensure_unmapped_user_pages(
        &loaded.address_space,
        reserve_start,
        USER_STACK_RESERVE_PAGES,
        "user stack reserve address overflow",
        "user stack reserve overlaps an existing mapping",
    )?;

    let stack_state = multitask::UserStackState::new(
        reserve_start.as_u64(),
        USER_STACK_TOP_EXCLUSIVE,
        USER_STACK_TOP_EXCLUSIVE - USER_STACK_INITIAL_COMMIT_PAGES as u64 * PAGE_SIZE,
    );
    let stack_start = VirtAddr::new(stack_state.committed_start);
    let stack_region = loaded.address_space.map_zeroed_user_pages_at(
        stack_start,
        USER_STACK_INITIAL_COMMIT_PAGES,
        PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
    )?;
    let bootstrap = build_process_bootstrap(
        loaded.runtime,
        loaded.abi,
        loaded.entry,
        &mut loaded.address_space,
        stack_region.end(),
        Some(stack_state),
        launch,
    )?;

    Ok(PreparedProcessImage {
        abi: loaded.abi,
        entry: loaded.entry,
        address_space: loaded.address_space,
        bootstrap,
    })
}

pub fn load_image_file(file: VfsFileHandle) -> Result<LoadedProcessImage, ProcessLoadError> {
    let mut header = [0_u8; 4];
    let read = file.read_at(0, &mut header);
    if read < header.len() {
        return Err(ProcessLoadError::InvalidPe(
            "unknown executable image format",
        ));
    }

    if header == *b"\x7FELF" {
        return linux::load_elf_file(file);
    }
    if &header[..2] == b"MZ" {
        let mut image = vec![0_u8; file.len()];
        let read = file.read_at(0, &mut image);
        if read != image.len() {
            return Err(ProcessLoadError::InvalidPe("short PE image read"));
        }
        return load_pe_metadata(&image);
    }

    Err(ProcessLoadError::InvalidPe(
        "unknown executable image format",
    ))
}

fn build_process_bootstrap(
    runtime: LoadedProcessRuntime,
    abi: UserAbi,
    entry: VirtAddr,
    address_space: &mut ProcessAddressSpace,
    stack_end: VirtAddr,
    user_stack: Option<multitask::UserStackState>,
    launch: ProcessLaunchOptions<'_>,
) -> Result<multitask::UserTaskBootstrap, ProcessLoadError> {
    let (
        stack_pointer,
        mut linux_process_state,
        linux_memory_map,
        linux_runtime_profile,
        linux_thread_state,
        windows_runtime,
    ) = match (abi, runtime) {
        (UserAbi::Linux, LoadedProcessRuntime::Linux(image)) => {
            linux::initialize_linux_initial_tls(address_space, &image)?;
            let security = ProcessSecurityContext::new(launch.logical_admin);
            (
                linux::initialize_linux_user_stack(
                    address_space,
                    stack_end,
                    &image,
                    launch.linux,
                    security,
                )?,
                Some(image.initial_process_state()),
                Some(linux::build_initial_memory_map(
                    &image,
                    launch.linux.exec_path,
                    user_stack,
                )),
                Some(linux::build_runtime_profile(&image, launch.linux)),
                Some(image.initial_thread_state()),
                None,
            )
        }
        (UserAbi::Windows, LoadedProcessRuntime::Windows(image)) => {
            let stack_pointer = initial_user_stack_top(stack_end)?;
            let runtime = WindowsProcessRuntimeState {
                image_base: image.image_base,
                image_size: image.image_size,
                allocation_base_hint: image.image_base.saturating_add(image.image_size),
                public_runtime_address: 0,
                peb_address: 0,
                teb_address: 0,
                process_parameters_address: 0,
                loader_data_address: 0,
                loader_module_array_address: 0,
                loader_module_count: 1,
                loader_reserved: 0,
                main_module_entry_address: entry.as_u64(),
                command_line_w_ptr: 0,
                command_line_a_ptr: 0,
                environment_w_ptr: 0,
                environment_a_ptr: 0,
                module_path_w_ptr: 0,
                module_path_a_ptr: 0,
                module_directory_w_ptr: 0,
                module_directory_a_ptr: 0,
                main_module_base_name_w_ptr: 0,
                main_module_base_name_a_ptr: 0,
                argc: 0,
                argc_ptr: 0,
                argv_ptr_ptr: 0,
                environ_ptr_ptr: 0,
                argv_ptr: 0,
                environ_ptr: 0,
                initial_narrow_environment_ptr: 0,
                initenv_ptr: 0,
                errno_ptr: 0,
                last_error_ptr: 0,
                commode_ptr: 0,
                fmode_ptr: 0,
                iob_array_ptr: 0,
                stdin_file_ptr: 0,
                stdout_file_ptr: 0,
                stderr_file_ptr: 0,
                localeconv_ptr: 0,
                strerror_einval_ptr: 0,
                strerror_enomem_ptr: 0,
                strerror_eio_ptr: 0,
                strerror_erange_ptr: 0,
                strerror_unknown_ptr: 0,
            };
            (stack_pointer, None, None, None, None, Some(runtime))
        }
        _ => {
            return Err(ProcessLoadError::InvalidElf(
                "process runtime metadata does not match ABI",
            ));
        }
    };

    let mut bootstrap = multitask::UserTaskBootstrap::new(abi, entry, stack_pointer);
    bootstrap.registers = launch.registers.into_task_registers();
    bootstrap.user_stack = user_stack;
    if let (Some(stack), Some(state)) = (user_stack, linux_process_state.as_mut()) {
        state
            .reserve_range(stack.reserve_start, stack.committed_start)
            .map_err(|_| {
                ProcessLoadError::InvalidElf("failed to reserve Linux user stack range")
            })?;
    }
    bootstrap.linux_process_state = linux_process_state;
    bootstrap.linux_memory_map = linux_memory_map;
    bootstrap.linux_runtime_profile = linux_runtime_profile;
    bootstrap.linux_thread_state = linux_thread_state;
    bootstrap.windows_runtime = windows_runtime;
    bootstrap.console_session = launch.console_session;
    bootstrap.logical_admin = launch.logical_admin;
    bootstrap.set_exec_path(launch.linux.exec_path);
    Ok(bootstrap)
}

fn load_pe_metadata(image: &[u8]) -> Result<LoadedProcessImage, ProcessLoadError> {
    const PE_LOAD_OFFSET: u64 = 0x0040_0000;
    if image.len() < 0x100 {
        return Err(ProcessLoadError::InvalidPe("PE image is too small"));
    }
    let pe_offset = read_u32(image, 0x3c)? as usize;
    if pe_offset
        .checked_add(0x88)
        .is_none_or(|end| end > image.len())
    {
        return Err(ProcessLoadError::InvalidPe("PE header outside image"));
    }
    if &image[pe_offset..pe_offset + 4] != b"PE\0\0" {
        return Err(ProcessLoadError::InvalidPe("missing PE signature"));
    }
    if read_u16(image, pe_offset + 4)? != 0x8664 {
        return Err(ProcessLoadError::InvalidPe("unsupported PE machine"));
    }
    let optional = pe_offset + 24;
    if read_u16(image, optional)? != 0x20b {
        return Err(ProcessLoadError::InvalidPe(
            "unsupported PE optional header",
        ));
    }
    let entry_rva = read_u32(image, optional + 16)? as u64;
    let size_of_image = read_u32(image, optional + 56)? as u64;
    if entry_rva == 0 || size_of_image == 0 {
        return Err(ProcessLoadError::InvalidPe(
            "invalid PE entry or image size",
        ));
    }
    let image_base = rustos_user_abi::syscall::PROC_BROKER_USER_SPACE_BASE
        .checked_add(PE_LOAD_OFFSET)
        .ok_or(ProcessLoadError::InvalidPe("PE load base overflow"))?;
    Ok(LoadedProcessImage {
        abi: UserAbi::Windows,
        address_space: ProcessAddressSpace::new()?,
        entry: VirtAddr::new(
            image_base
                .checked_add(entry_rva)
                .ok_or(ProcessLoadError::InvalidPe("PE entry overflow"))?,
        ),
        runtime: LoadedProcessRuntime::Windows(WindowsProcessImageInfo {
            image_base,
            image_size: align_up(size_of_image, PAGE_SIZE)
                .ok_or(ProcessLoadError::InvalidPe("PE image size overflow"))?,
        }),
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ProcessLoadError> {
    let end = offset
        .checked_add(2)
        .ok_or(ProcessLoadError::InvalidPe("PE read overflow"))?;
    if end > bytes.len() {
        return Err(ProcessLoadError::InvalidPe("PE read outside image"));
    }
    Ok(u16::from_le_bytes([bytes[offset], bytes[offset + 1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ProcessLoadError> {
    let end = offset
        .checked_add(4)
        .ok_or(ProcessLoadError::InvalidPe("PE read overflow"))?;
    if end > bytes.len() {
        return Err(ProcessLoadError::InvalidPe("PE read outside image"));
    }
    Ok(u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}

fn page_ranges_overlap(page_base: u64, page_end: u64, existing_ranges: &[(u64, u64)]) -> bool {
    for &(other_start, other_end) in existing_ranges {
        if page_base < other_end && other_start < page_end {
            return true;
        }
    }
    false
}

fn ensure_unmapped_user_pages(
    address_space: &ProcessAddressSpace,
    start: VirtAddr,
    page_count: usize,
    overflow_reason: &'static str,
    overlap_reason: &'static str,
) -> Result<(), ProcessLoadError> {
    for page_index in 0..page_count {
        let page_addr = start
            .as_u64()
            .checked_add(page_index as u64 * PAGE_SIZE)
            .ok_or(ProcessLoadError::InvalidPe(overflow_reason))?;
        if address_space
            .translate_user(VirtAddr::new(page_addr))
            .is_some()
        {
            return Err(ProcessLoadError::InvalidPe(overlap_reason));
        }
    }

    Ok(())
}

fn initial_user_stack_top(stack_end: VirtAddr) -> Result<VirtAddr, ProcessLoadError> {
    let aligned_top = align_down(stack_end.as_u64(), 16);
    let user_stack_top = aligned_top
        .checked_sub(8)
        .ok_or(ProcessLoadError::InvalidElf(
            "user stack top calculation underflow",
        ))?;
    Ok(VirtAddr::new(user_stack_top))
}

fn align_down(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    value & !(align - 1)
}

fn align_up(value: u64, align: u64) -> Option<u64> {
    debug_assert!(align.is_power_of_two());
    value
        .checked_add(align - 1)
        .map(|aligned| align_down(aligned, align))
}
