use alloc::string::String;
use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;

use crate::debug;
use crate::io::session::ConsoleSessionHandle;
use crate::memory::paging::{self, AddressSpaceError, ProcessAddressSpace};
use crate::multitask;
use crate::user::abi::UserAbi;
use crate::user::linux::{LinuxProcessImageInfo, LinuxProcessLaunch};
use crate::user::process_state::{
    ProcessSecurityContext, WindowsProcessRuntimeState, WindowsThreadRuntimeState,
};
use crate::vfs;

mod linux;

const PAGE_SIZE: u64 = 4096;
const MAX_LOAD_SEGMENTS: usize = 32;
const USER_STACK_GUARD_PAGES: usize = 1;
const USER_STACK_RESERVE_PAGES: usize = 256;
// Exception context cannot wait for ProcessStateLock: another thread may hold
// it when a valid stack-growth fault arrives. Until a deferred fault worker
// exists, map every usable page eagerly and retain one permanent guard page.
const USER_STACK_INITIAL_COMMIT_PAGES: usize = USER_STACK_RESERVE_PAGES - USER_STACK_GUARD_PAGES;
const USER_STACK_TOP_EXCLUSIVE: u64 = paging::USER_SPACE_END_EXCLUSIVE;

#[derive(Debug)]
pub enum ProcessLoadError {
    MissingSchedulingContext,
    InvalidElf(&'static str),
    InvalidPe(&'static str),
    InterpreterLoad {
        path: String,
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
    pub fn summary(&self) -> &'static str {
        match self {
            Self::MissingSchedulingContext => "user process has no admitted scheduling context",
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
            Self::InterpreterLoad { path, error } => {
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
    pub pid: u64,
}

pub struct PreparedProcessImage {
    pub abi: UserAbi,
    pub entry: VirtAddr,
    pub address_space: ProcessAddressSpace,
    pub bootstrap: multitask::UserTaskBootstrap,
}

/// RAII custody for an invisible process slot and its non-reusable lifecycle
/// transaction. Mapping and stack preparation may fail at many points; Drop
/// makes every such return cancel the exact reservation without duplicating
/// cleanup branches throughout loader policy.
pub struct ProcessSpawnTransaction {
    reservation: Option<multitask::SpawnReservation>,
}

impl ProcessSpawnTransaction {
    fn take(&mut self) -> multitask::SpawnReservation {
        self.reservation
            .take()
            .expect("process spawn transaction consumed more than once")
    }
}

impl Drop for ProcessSpawnTransaction {
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            let cancelled = multitask::cancel_process_spawn(reservation);
            assert!(
                cancelled,
                "live spawn transaction failed exact cancellation"
            );
        }
    }
}

pub struct PreparedSpawnImage {
    prepared: PreparedProcessImage,
    transaction: ProcessSpawnTransaction,
}

pub fn reserve_process_spawn_transaction() -> Result<ProcessSpawnTransaction, ProcessLoadError> {
    let reservation = multitask::reserve_process_spawn().ok_or(ProcessLoadError::Spawn(
        multitask::SpawnTaskError::NoFreeTaskSlot,
    ))?;
    Ok(ProcessSpawnTransaction {
        reservation: Some(reservation),
    })
}

pub fn bind_prepared_spawn(
    prepared: PreparedProcessImage,
    transaction: ProcessSpawnTransaction,
) -> PreparedSpawnImage {
    PreparedSpawnImage {
        prepared,
        transaction,
    }
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
    runtime: WindowsProcessRuntimeState,
}

#[derive(Clone, Copy, Debug)]
pub struct WindowsProcessLoaderRuntime {
    pub entry_point: u64,
    pub runtime: WindowsProcessRuntimeState,
}

#[derive(Debug, Clone, Copy)]
pub struct ProcessLaunchOptions<'a> {
    pub registers: ProcessStartRegisters,
    pub linux: LinuxProcessLaunch<'a>,
    pub(crate) console_session: ConsoleSessionHandle,
    pub logical_admin: bool,
}

// RING3-MIGRATION-REFERENCE START: loaderd/procd should own bootstrap Linux
// image loading policy. Ring0 keeps this direct ELF path only for pre-loaderd
// bootstrap services; normal launches arrive as prepared metadata.
pub fn spawn_bootstrap_linux_process_with_launch_and_scheduling_context(
    image: &[u8],
    weight_micros: u64,
    launch: ProcessLaunchOptions<'_>,
    policy: rustos_user_abi::syscall::RustosSchedulingContextPolicy,
) -> Result<SpawnedProcess, ProcessLoadError> {
    let transaction = reserve_process_spawn_transaction()?;
    let prepared = prepare_bootstrap_linux_process_with_launch(image, launch)?;
    spawn_prepared_process_with_scheduling_context(
        bind_prepared_spawn(prepared, transaction),
        weight_micros,
        policy,
    )
}

fn prepare_bootstrap_linux_process_with_launch(
    image: &[u8],
    launch: ProcessLaunchOptions<'_>,
) -> Result<PreparedProcessImage, ProcessLoadError> {
    prepare_loaded_process_with_launch(linux::load_elf(image)?, launch)
}
// RING3-MIGRATION-REFERENCE END: loaderd/procd-owned bootstrap Linux image loading policy.

pub fn prepare_windows_process_with_address_space(
    metadata: WindowsProcessLoaderRuntime,
    address_space: ProcessAddressSpace,
    launch: ProcessLaunchOptions<'_>,
) -> Result<PreparedProcessImage, ProcessLoadError> {
    let loaded = LoadedProcessImage {
        abi: UserAbi::Windows,
        address_space,
        entry: VirtAddr::new(metadata.entry_point),
        runtime: LoadedProcessRuntime::Windows(WindowsProcessImageInfo {
            runtime: metadata.runtime,
        }),
    };
    prepare_loaded_process_with_launch(loaded, launch)
}

pub fn prepare_linux_process_with_metadata(
    info: LinuxProcessImageInfo,
    actual_entry: u64,
    address_space: ProcessAddressSpace,
    launch: ProcessLaunchOptions<'_>,
) -> Result<PreparedProcessImage, ProcessLoadError> {
    let loaded = LoadedProcessImage {
        abi: UserAbi::Linux,
        address_space,
        entry: VirtAddr::new(actual_entry),
        runtime: LoadedProcessRuntime::Linux(info),
    };
    prepare_loaded_process_with_launch(loaded, launch)
}

fn scheduling_context_admission(
    policy: rustos_user_abi::syscall::RustosSchedulingContextPolicy,
) -> kernel_ps::api::SchedulingContextAdmission {
    kernel_ps::api::SchedulingContextAdmission {
        budget_ns: policy.budget_ns,
        period_ns: policy.period_ns,
        refill_capacity: policy.refill_capacity,
        cpu_mask: policy.cpu_mask,
        criticality: policy.criticality,
        domain: policy.domain,
        policy_epoch: policy.policy_epoch,
        timeout_endpoint_cap: policy.timeout_endpoint_cap,
    }
}

pub fn spawn_prepared_process_with_scheduling_context(
    mut spawn: PreparedSpawnImage,
    weight_micros: u64,
    policy: rustos_user_abi::syscall::RustosSchedulingContextPolicy,
) -> Result<SpawnedProcess, ProcessLoadError> {
    let reservation = spawn.transaction.take();
    let prepared = spawn.prepared;
    let pid = kernel_ps::api::process::spawn_user_process_with_scheduling_context(
        prepared.address_space,
        prepared.bootstrap,
        weight_micros,
        scheduling_context_admission(policy),
        reservation,
    )?;
    Ok(SpawnedProcess { pid })
}

pub fn spawn_prepared_process_for_loader_reply_with_scheduling_context(
    mut spawn: PreparedSpawnImage,
    weight_micros: u64,
    policy: rustos_user_abi::syscall::RustosSchedulingContextPolicy,
) -> Result<SpawnedProcess, ProcessLoadError> {
    let reservation = spawn.transaction.take();
    let prepared = spawn.prepared;
    let pid = kernel_ps::api::process::spawn_user_process_without_deferred_reschedule_with_scheduling_context(
        prepared.address_space,
        prepared.bootstrap,
        weight_micros,
        scheduling_context_admission(policy),
        reservation,
    )?;
    Ok(SpawnedProcess { pid })
}

pub fn spawn_prepared_process_suspended_with_scheduling_context(
    mut spawn: PreparedSpawnImage,
    weight_micros: u64,
    policy: rustos_user_abi::syscall::RustosSchedulingContextPolicy,
) -> Result<SpawnedProcess, ProcessLoadError> {
    let reservation = spawn.transaction.take();
    let prepared = spawn.prepared;
    let pid = kernel_ps::api::process::spawn_user_process_suspended_with_scheduling_context(
        prepared.address_space,
        prepared.bootstrap,
        weight_micros,
        scheduling_context_admission(policy),
        reservation,
    )?;
    Ok(SpawnedProcess { pid })
}

fn release_user_stack_state(reserve_start: VirtAddr) -> multitask::UserStackState {
    let usable_start = reserve_start
        + u64::try_from(USER_STACK_GUARD_PAGES).expect("stack guard-page count overflow")
            * PAGE_SIZE;
    multitask::UserStackState::new(
        usable_start.as_u64(),
        USER_STACK_TOP_EXCLUSIVE,
        usable_start.as_u64(),
    )
}

fn prepare_loaded_process_with_launch(
    mut loaded: LoadedProcessImage,
    launch: ProcessLaunchOptions<'_>,
) -> Result<PreparedProcessImage, ProcessLoadError> {
    const {
        assert!(USER_STACK_RESERVE_PAGES > USER_STACK_GUARD_PAGES);
        assert!(
            USER_STACK_INITIAL_COMMIT_PAGES + USER_STACK_GUARD_PAGES == USER_STACK_RESERVE_PAGES
        );
    }

    let reserve_start =
        VirtAddr::new(USER_STACK_TOP_EXCLUSIVE - USER_STACK_RESERVE_PAGES as u64 * PAGE_SIZE);
    ensure_unmapped_user_pages(
        &loaded.address_space,
        reserve_start,
        USER_STACK_RESERVE_PAGES,
        "user stack reserve address overflow",
        "user stack reserve overlaps an existing mapping",
    )?;

    let stack_state = release_user_stack_state(reserve_start);
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

// Note: file-backed image loading (`load_image_file`/`load_elf_file`) has been
// retired from ring0. loaderd reads images via VFS fd and prepares the runtime
// metadata in user space; ring0 only consumes prepared metadata for commit.

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
            (stack_pointer, None, None, None, None, Some(image.runtime))
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
    bootstrap.linux_process_state = linux_process_state;
    bootstrap.linux_memory_map = linux_memory_map;
    bootstrap.linux_runtime_profile = linux_runtime_profile;
    bootstrap.linux_thread_state = linux_thread_state;
    bootstrap.windows_runtime = windows_runtime;
    if let Some(runtime) = bootstrap.windows_runtime {
        bootstrap.windows_thread_state =
            Some(WindowsThreadRuntimeState::new(0, runtime.teb_address));
    }
    bootstrap.console_session = launch.console_session;
    bootstrap.logical_admin = launch.logical_admin;
    bootstrap.set_exec_path(launch.linux.exec_path);
    Ok(bootstrap)
}

// PE header validation, image-base selection, and Windows runtime metadata
// derivation now live in `loaderd` (see `load_pe_image_fd`). Loaderd populates
// `windows_runtime` via PROC_BROKER_OP_SET_WINDOWS_RUNTIME, and ring0 only
// consumes the prepared metadata in `prepare_windows_process_with_address_space`.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_stack_maps_every_usable_page_above_one_guard() {
        assert_eq!(USER_STACK_GUARD_PAGES, 1);
        assert_eq!(USER_STACK_RESERVE_PAGES, 256);
        assert_eq!(USER_STACK_INITIAL_COMMIT_PAGES, 255);

        let reserve_start = USER_STACK_TOP_EXCLUSIVE
            - u64::try_from(USER_STACK_RESERVE_PAGES).expect("stack pages") * PAGE_SIZE;
        let usable_start =
            reserve_start + u64::try_from(USER_STACK_GUARD_PAGES).expect("guard pages") * PAGE_SIZE;
        let state = release_user_stack_state(VirtAddr::new(reserve_start));
        assert_eq!(state.reserve_start, usable_start);
        assert_eq!(state.reserve_start, state.committed_start);
        assert_eq!(state.reserve_end, USER_STACK_TOP_EXCLUSIVE);
    }

    #[test]
    fn production_process_spawn_surface_requires_scheduling_authority() {
        let source = include_str!("mod.rs")
            .split_once("#[cfg(test)]")
            .expect("process tests remain below production")
            .0;
        assert!(source.contains("spawn_prepared_process_with_scheduling_context"));
        assert!(!source.contains("pub fn spawn_prepared_process("));
        assert!(!source.contains("pub fn spawn_bootstrap_linux_process_with_launch("));
        assert!(source.contains("multitask::cancel_process_spawn(reservation)"));
        assert!(source.contains("spawn.transaction.take()"));
    }
}
