use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;

use crate::debug;
use crate::fat;
use crate::multitask;
use crate::paging::{self, AddressSpaceError, ProcessAddressSpace};
use crate::session::ConsoleSessionId;
use crate::user::abi::UserAbi;
use crate::user::linux::{LinuxProcessImageInfo, LinuxProcessLaunch};

mod linux;
mod windows;

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
        error: fatfs::Error<fat::DiskIoError>,
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
            Self::UnsupportedImport { .. } => "PE import is not supported yet",
            Self::Spawn(err) => err.summary(),
        }
    }

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

pub struct SpawnedProcess {
    pub abi: UserAbi,
    pub pid: u64,
    pub entry: VirtAddr,
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
    #[allow(dead_code)]
    pub const fn with_sysv_args(arg0: u64, arg1: u64) -> Self {
        Self {
            rdi: arg0,
            rsi: arg1,
            ..Self::new()
        }
    }

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
    Windows,
}

#[derive(Debug, Clone, Copy)]
pub struct ProcessLaunchOptions<'a> {
    pub registers: ProcessStartRegisters,
    pub linux: LinuxProcessLaunch<'a>,
    pub console_session: ConsoleSessionId,
    pub logical_admin: bool,
}

impl<'a> Default for ProcessLaunchOptions<'a> {
    fn default() -> Self {
        Self {
            registers: ProcessStartRegisters::new(),
            linux: LinuxProcessLaunch::new(""),
            console_session: ConsoleSessionId::PRIMARY,
            logical_admin: false,
        }
    }
}

pub fn load_elf(image: &[u8]) -> Result<LoadedProcessImage, ProcessLoadError> {
    linux::load_elf(image)
}

pub fn load_image(image: &[u8]) -> Result<LoadedProcessImage, ProcessLoadError> {
    if image.starts_with(b"\x7FELF") {
        return linux::load_elf(image);
    }
    if image.starts_with(b"MZ") {
        return windows::load_pe(image);
    }

    Err(ProcessLoadError::InvalidPe(
        "unknown executable image format",
    ))
}

#[allow(dead_code)]
pub fn spawn_process(
    image: &[u8],
    weight_micros: u64,
    arg0: u64,
    arg1: u64,
) -> Result<SpawnedProcess, ProcessLoadError> {
    let launch = ProcessLaunchOptions {
        registers: ProcessStartRegisters::with_sysv_args(arg0, arg1),
        console_session: multitask::current_console_session(),
        ..ProcessLaunchOptions::default()
    };
    spawn_process_with_launch(image, weight_micros, launch)
}

#[allow(dead_code)]
pub fn spawn_process_with_registers(
    image: &[u8],
    weight_micros: u64,
    registers: ProcessStartRegisters,
) -> Result<SpawnedProcess, ProcessLoadError> {
    let launch = ProcessLaunchOptions {
        registers,
        console_session: multitask::current_console_session(),
        ..ProcessLaunchOptions::default()
    };
    spawn_process_with_launch(image, weight_micros, launch)
}

pub fn spawn_linux_process(
    image: &[u8],
    weight_micros: u64,
    exec_path: &str,
) -> Result<SpawnedProcess, ProcessLoadError> {
    spawn_linux_process_in_session(
        image,
        weight_micros,
        exec_path,
        multitask::current_console_session(),
    )
}

pub fn spawn_linux_process_in_session(
    image: &[u8],
    weight_micros: u64,
    exec_path: &str,
    console_session: ConsoleSessionId,
) -> Result<SpawnedProcess, ProcessLoadError> {
    let argv = [exec_path];
    spawn_linux_process_with_args_in_session(
        image,
        weight_micros,
        exec_path,
        &argv,
        &[],
        console_session,
    )
}

pub fn spawn_linux_process_with_args(
    image: &[u8],
    weight_micros: u64,
    exec_path: &str,
    argv: &[&str],
    env: &[&str],
) -> Result<SpawnedProcess, ProcessLoadError> {
    spawn_linux_process_with_args_in_session(
        image,
        weight_micros,
        exec_path,
        argv,
        env,
        multitask::current_console_session(),
    )
}

pub fn spawn_linux_process_with_args_in_session(
    image: &[u8],
    weight_micros: u64,
    exec_path: &str,
    argv: &[&str],
    env: &[&str],
    console_session: ConsoleSessionId,
) -> Result<SpawnedProcess, ProcessLoadError> {
    let launch = ProcessLaunchOptions {
        linux: LinuxProcessLaunch {
            exec_path,
            argv,
            env,
        },
        console_session,
        ..ProcessLaunchOptions::default()
    };
    spawn_process_with_launch(image, weight_micros, launch)
}

pub fn spawn_process_with_launch(
    image: &[u8],
    weight_micros: u64,
    launch: ProcessLaunchOptions<'_>,
) -> Result<SpawnedProcess, ProcessLoadError> {
    debug::println!("process spawn: load_image begin");
    let mut loaded = load_image(image)?;
    debug::println!(
        "process spawn: load_image done entry={:#x}",
        loaded.entry.as_u64()
    );

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
    debug::println!(
        "process spawn: user stack reserve=[{:#x}, {:#x}) initial_commit=[{:#x}, {:#x})",
        stack_state.reserve_start,
        stack_state.reserve_end,
        stack_start.as_u64(),
        stack_region.end().as_u64(),
    );
    let bootstrap = build_process_bootstrap(
        loaded.runtime,
        loaded.abi,
        loaded.entry,
        &mut loaded.address_space,
        stack_region.end(),
        Some(stack_state),
        launch,
    )?;
    debug::println!(
        "process spawn: bootstrap prepared rsp={:#x}",
        bootstrap.stack_pointer.as_u64()
    );

    let pid = multitask::spawn_user_process(loaded.address_space, bootstrap, weight_micros)?;
    debug::println!("process spawn: user task spawned pid={}", pid);

    Ok(SpawnedProcess {
        abi: loaded.abi,
        pid,
        entry: loaded.entry,
    })
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
    let (stack_pointer, mut linux_process_state, linux_memory_map, linux_thread_state) =
        match (abi, runtime) {
        (UserAbi::Linux, LoadedProcessRuntime::Linux(image)) => {
            linux::initialize_linux_initial_tls(address_space, &image)?;
            (
                linux::initialize_linux_user_stack(address_space, stack_end, &image, launch.linux)?,
                Some(image.initial_process_state()),
                Some(linux::build_initial_memory_map(
                    &image,
                    launch.linux.exec_path,
                    user_stack,
                )),
                Some(image.initial_thread_state()),
            )
        }
        (UserAbi::Windows, LoadedProcessRuntime::Windows) => {
            (initial_user_stack_top(stack_end)?, None, None, None)
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
    bootstrap.linux_thread_state = linux_thread_state;
    bootstrap.console_session = launch.console_session;
    bootstrap.logical_admin = launch.logical_admin;
    bootstrap.set_exec_path(launch.linux.exec_path);
    Ok(bootstrap)
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
