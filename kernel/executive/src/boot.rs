use boot_protocol::{BootInfo, BootVolumeTransport};
use core::hint::spin_loop;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};
use kernel_compat::api as compat_api;
use kernel_compat::api::console_host::{self, ConsoleProgramSpec};
use kernel_hal::api as hal_api;
use kernel_mm::api as mm_api;
use kernel_ps::api as ps_api;
use nucleus_core::util::{fault_injection, random};

use crate::{announce_ready, debug, fatal, flow_debug, flow_info, hal_hooks, io_services, tasks};

const ROOTD_EXEC_PATH: &str = "services/rootd/rootd.elf";
const ROOTD_BOOTSTRAP_WEIGHT_MICROS: u64 = 4_000;
const MAX_RETIRED_TASK_CLEANUPS_PER_TURN: usize = 4;
const AP_ONLINE_PARKED_TIMEOUT_NS: u64 = 2_000_000_000;
static PANIC_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

macro_rules! boot_log {
    ($level:expr, $event_id:expr, $object_id:expr, $($arg:tt)+) => {{
        debug::record_milestone(
            debug::LogCategory::Boot,
            "boot",
            ($event_id) as u64,
            ($object_id) as u64,
        );
        match $level {
            debug::LogLevel::Trace => debug::trace!(boot, $($arg)+),
            debug::LogLevel::Debug => debug::debug!(boot, $($arg)+),
            debug::LogLevel::Info => debug::info!(boot, $($arg)+),
            debug::LogLevel::Warn => debug::warn!(boot, $($arg)+),
            debug::LogLevel::Error | debug::LogLevel::Fatal => debug::error!(boot, $($arg)+),
        }
    }};
}

pub fn handle_kernel_panic(info: &PanicInfo<'_>) -> ! {
    x86_64::instructions::interrupts::disable();
    if PANIC_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        debug::println_emergency(format_args!("[NESTED PANIC]"));
        loop {
            x86_64::instructions::hlt();
        }
    }
    write_panic_site_marker(info);
    // The first panic evidence must not acquire a display, allocator, or
    // scheduler-dependent lock. A lock-contract panic can occur while any of
    // those are inconsistent; trying to paint first recursively panics and
    // turns a diagnosable failure into an x86 triple fault.
    debug::println_emergency(format_args!("[PANIC]"));
    // Print the allocation-free static location before formatting arbitrary
    // panic arguments. Corrupted argument state must not hide the source site
    // behind a nested exception.
    if let Some(location) = info.location() {
        debug::println_emergency(format_args!(
            "location: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        ));
    } else {
        debug::println_emergency(format_args!("location: <unknown>"));
    }
    debug::println_emergency(format_args!("message: {}", info.message()));
    loop {
        x86_64::instructions::hlt();
    }
}

/// Print the first panic site without formatting, locks, or allocation.
///
/// A second CPU can fault while the first panic is being rendered. Emitting
/// the static file and fixed-width hexadecimal line first preserves the
/// initiating site even when later diagnostics interleave or fault.
fn write_panic_site_marker(info: &PanicInfo<'_>) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let Some(location) = info.location() else {
        emergency_debugcon_bytes(b"\n!PANIC-SITE:<unknown>\n");
        return;
    };
    emergency_debugcon_bytes(b"\n!PANIC-SITE:");
    for shift in (0..8).rev() {
        let nibble = ((location.line() >> (shift * 4)) & 0xf) as usize;
        emergency_debugcon_bytes(&[HEX[nibble]]);
    }
    emergency_debugcon_bytes(b":");
    emergency_debugcon_bytes(location.file().as_bytes());
    emergency_debugcon_bytes(b"\n");
}

fn emergency_debugcon_bytes(bytes: &[u8]) {
    for &byte in bytes {
        unsafe {
            core::arch::asm!(
                "out dx, al",
                in("dx") 0x00e9_u16,
                in("al") byte,
                options(nomem, nostack, preserves_flags)
            );
        }
    }
}

/// Initializes the kernel from the immutable boot-protocol handoff.
///
/// # Safety
///
/// `boot_info_ptr` must name a readable, boot-protocol-validated [`BootInfo`]
/// that remains valid through early kernel initialization.
pub unsafe fn initialize_kernel(boot_info_ptr: *const BootInfo) {
    flow_info(1, "kernel initialize begin");
    let boot_info = match unsafe { BootInfo::from_ptr(boot_info_ptr) } {
        Ok(boot_info) => boot_info,
        Err(error) => panic!("{}", error.as_str()),
    };

    io_services::gui_init(boot_info_ptr);
    debug::init(boot_info_ptr);
    boot_log!(
        debug::LogLevel::Info,
        100,
        0,
        "kernel higher-half entry rip={:#x}",
        hal_api::current_rip(),
    );
    boot_log!(debug::LogLevel::Info, 101, 0, "gdt initialized");
    boot_log!(debug::LogLevel::Info, 102, 0, "idt initialized");
    boot_log!(debug::LogLevel::Info, 103, 0, "paging initialized");
    boot_log!(
        debug::LogLevel::Info,
        118,
        0,
        "boot framebuffer addr={:#x} size={} width={} height={} stride={} bpp={}",
        boot_info.framebuffer.addr,
        boot_info.framebuffer.size,
        boot_info.framebuffer.width,
        boot_info.framebuffer.height,
        boot_info.framebuffer.stride,
        boot_info.framebuffer.bytes_per_pixel,
    );
    mm_api::boot::init_phys(boot_info_ptr);
    if hal_api::cpu::discovered_count() > 1 {
        mm_api::phys::claim_fixed_range(
            nucleus_core::ap_trampoline::TRAMPOLINE_PHYS,
            nucleus_core::ap_trampoline::RESERVED_BYTES,
        )
        .expect("AP trampoline low-memory range is unavailable or already owned");
    }
    boot_log!(
        debug::LogLevel::Info,
        104,
        0,
        "physical memory usable_kib={} free_kib={}",
        mm_api::phys::usable_bytes() / 1024,
        mm_api::phys::free_bytes() / 1024,
    );
    mm_api::alloc::init_heap();
    boot_log!(debug::LogLevel::Info, 105, 0, "heap initialized");
    let fault_report = fault_injection::init_from_qemu_fw_cfg();
    boot_log!(
        debug::LogLevel::Info,
        119,
        fault_report.rule_count as u64,
        "fault injection status={:?} rules={} spec_len={}",
        fault_report.status,
        fault_report.rule_count,
        fault_report.spec_len,
    );
    hal_api::init_simd();
    boot_log!(
        debug::LogLevel::Info,
        106,
        0,
        "simd initialized mode={}",
        hal_api::simd_mode_name(),
    );
    io_services::init_boot_info(boot_info_ptr);
    let transport_hint =
        io_services::boot_volume_transport_hint().unwrap_or(BootVolumeTransport::Unknown);
    match io_services::boot_volume_identity() {
        Some(identity) => boot_log!(
            debug::LogLevel::Info,
            107,
            identity.fat_volume_id as u64,
            "boot volume transport={:?} serial={:#010x} start_lba={} sectors={}",
            identity.transport(),
            identity.fat_volume_id,
            identity.volume_start_lba,
            identity.volume_sector_count,
        ),
        None if transport_hint != BootVolumeTransport::Unknown => boot_log!(
            debug::LogLevel::Warn,
            108,
            0,
            "boot volume identity unavailable transport_hint={:?}",
            transport_hint,
        ),
        None => boot_log!(
            debug::LogLevel::Warn,
            109,
            0,
            "boot volume identity unavailable",
        ),
    }
    boot_log!(
        debug::LogLevel::Info,
        110,
        0,
        "native block providers absent; early-system bootstrap and storage DVM required",
    );
    boot_log!(debug::LogLevel::Debug, 114, 0, "block descriptor scan done");
    hal_hooks::register();

    announce_ready("GUI", b"GUI initialized.\r\n");
    announce_ready("Heap", b"Heap initialized.\r\n");

    hal_api::init_acpi(boot_info_ptr);
    announce_ready("ACPI", b"ACPI initialized.\r\n");

    let clocksource = hal_api::init_clocksource().unwrap_or_else(|| {
        panic!(
            "no validated monotonic clocksource (invariant TSC or 64-bit HPET); acpi_hpet={:?}",
            hal_api::arch::acpi::hpet_address(),
        )
    });
    boot_log!(
        debug::LogLevel::Info,
        104,
        clocksource.frequency_hz,
        "monotonic clocksource={} frequency_hz={}",
        clocksource.name,
        clocksource.frequency_hz,
    );
    debug::record_milestone(
        debug::LogCategory::Boot,
        "clocksource-ready",
        clocksource.frequency_hz,
        u64::from(clocksource.name == "invariant-tsc"),
    );
    announce_ready("Clocksource", b"Monotonic clocksource initialized.\r\n");

    let local_apic_phys = hal_api::cpu::local_apic_physical_base()
        .expect("CPUID/MSR did not expose a supported xAPIC physical page");
    if let Some(topology) = hal_api::cpu::topology() {
        assert_eq!(
            topology.local_apic_address(),
            local_apic_phys,
            "ACPI MADT and IA32_APIC_BASE disagree on the local APIC page"
        );
    }
    let local_apic_virt = mm_api::paging::map_mmio_range(local_apic_phys, 4096)
        .expect("kernel-mm could not admit the local APIC MMIO page");
    assert!(
        hal_api::cpu::configure_local_apic_mmio(local_apic_phys, local_apic_virt),
        "kernel-hal rejected the admitted local APIC MMIO mapping"
    );

    // The DVM display receiver programs a masked MSI-X table entry. Its
    // bounded xAPIC/MSI substrate must therefore exist before PCI probing;
    // probing after ACPI but before `init_pic` would correctly fail closed on
    // every otherwise valid ivshmem function.
    hal_api::init_pic();
    announce_ready("PIC", b"PIC initialized.\r\n");

    // ivshmem is a PCI function. The DVM providers must be probed only after
    // ACPI has published the PCI bus regions; probing earlier silently sees no
    // device and can make a firmware framebuffer look like a live DVM path.
    if io_services::init_dvm_display_provider() {
        boot_log!(
            debug::LogLevel::Info,
            105,
            0,
            "DVM shared display transport initialized"
        );
    } else {
        boot_log!(
            debug::LogLevel::Warn,
            105,
            0,
            "DVM shared display transport unavailable; UI remains disabled"
        );
    }
    if io_services::init_dvm_network_provider() {
        boot_log!(
            debug::LogLevel::Info,
            106,
            0,
            "DVM shared network transport initialized"
        );
    } else {
        boot_log!(
            debug::LogLevel::Warn,
            106,
            0,
            "DVM shared network transport unavailable; network remains disabled"
        );
    }
    if io_services::init_dvm_block_provider() {
        boot_log!(
            debug::LogLevel::Info,
            107,
            0,
            "DVM shared block transport initialized"
        );
    } else {
        boot_log!(
            debug::LogLevel::Warn,
            107,
            0,
            "DVM shared block transport unavailable; persistent storage remains disabled"
        );
    }

    io_services::init_input();
    announce_ready("DVM input", b"DVM input transport initialized.\r\n");

    io_services::tty_init();
    hal_api::init_rtc();
    announce_ready("RTC", b"RTC initialized.\r\n");
    mm_api::boot::paging_smoke_test();

    assert!(
        random::init(boot_info),
        "refusing to initialize the CSPRNG without boot entropy"
    );
    announce_ready("Random", b"Random initialized.\r\n");

    compat_api::init_syscalls();
    announce_ready("Syscall", b"Syscall initialized.\r\n");
    initialize_application_processors();

    let _ = boot_info;
    flow_info(2, "kernel initialize complete");
}

fn initialize_application_processors() {
    use hal_api::cpu::CpuLifecycleState;
    use nucleus_core::ap_trampoline::{
        self, ApStartupMailbox, MAILBOX_PHYS, PAGE_SIZE, RESERVED_BYTES, STARTUP_VECTOR,
        TRAMPOLINE_PHYS,
    };

    let cpu_count = hal_api::cpu::discovered_count();
    assert!(
        (1..=hal_api::cpu::MAX_SUPPORTED_CPUS).contains(&cpu_count),
        "commercial boot requires one completely admitted CPU topology"
    );
    debug::record_milestone(
        debug::LogCategory::Boot,
        "smp-bringup-begin",
        cpu_count as u64,
        0,
    );
    let bsp =
        hal_api::cpu::lifecycle_snapshot(0).expect("admitted CPU topology omitted the logical BSP");
    assert_eq!(
        bsp.state,
        CpuLifecycleState::Starting,
        "BSP private publication occurred outside Starting"
    );
    hal_api::cpu::transition_lifecycle(0, bsp.generation, CpuLifecycleState::OnlineParked);
    if cpu_count == 1 {
        return;
    }

    ap_trampoline::install();
    debug::record_milestone(
        debug::LogCategory::Boot,
        "smp-trampoline-installed",
        TRAMPOLINE_PHYS,
        RESERVED_BYTES,
    );
    assert!(
        mm_api::paging::mark_direct_map_range_executable(TRAMPOLINE_PHYS, PAGE_SIZE),
        "kernel-mm could not publish the AP trampoline RX page"
    );
    assert!(
        mm_api::paging::mark_direct_map_range_writable_noexec(MAILBOX_PHYS, PAGE_SIZE),
        "kernel-mm could not preserve the AP mailbox RW/NX page"
    );

    let entry = mm_api::higher_half_addr(rustos_ap_entry as *const () as usize as u64);
    let cr3 = mm_api::paging::kernel_root_phys().as_u64();
    // Worst cross-CPU TSC warp observed while admitting the application
    // processors. `None` means at least one rendezvous did not complete and the
    // multiprocessor monotonic source stays on the validated MMIO counter.
    let mut worst_tsc_warp_nanos: Option<u64> = Some(0);
    for logical_index in 1..cpu_count {
        let logical_index =
            u8::try_from(logical_index).expect("logical CPU index exceeds u8 capacity");
        let discovered = hal_api::cpu::lifecycle_snapshot(logical_index)
            .expect("admitted AP lacks a lifecycle slot");
        assert_eq!(
            discovered.state,
            CpuLifecycleState::Discovered,
            "AP startup began outside Discovered"
        );
        hal_api::cpu::transition_lifecycle(
            logical_index,
            discovered.generation,
            CpuLifecycleState::Starting,
        );
        let stack_top = hal_api::cpu::ap_bootstrap_stack_top(logical_index, discovered.generation);
        ap_trampoline::publish_mailbox(ApStartupMailbox::new(
            discovered.generation,
            stack_top,
            entry,
            cr3,
            logical_index,
            discovered.apic_id,
        ));
        hal_api::cpu::start_application_processor(discovered.apic_id, STARTUP_VECTOR)
            .unwrap_or_else(|error| {
                panic!(
                    "AP startup IPI failed: logical_cpu={} apic_id={:#x} error={:?}",
                    logical_index, discovered.apic_id, error
                )
            });
        debug::record_milestone(
            debug::LogCategory::Boot,
            "smp-startup-ipi-sent",
            u64::from(logical_index),
            u64::from(discovered.apic_id),
        );

        let started_at = hal_api::arch::clock::monotonic_nanos();
        loop {
            let state = hal_api::cpu::lifecycle_snapshot(logical_index)
                .expect("starting AP lost its lifecycle slot")
                .state;
            if state == CpuLifecycleState::OnlineParked {
                debug::record_milestone(
                    debug::LogCategory::Boot,
                    "smp-ap-online-parked",
                    u64::from(logical_index),
                    u64::from(discovered.apic_id),
                );
                // This AP is parked in its admission loop and is the only other
                // running CPU, which makes it the exact bounded window in which
                // the boot processor can prove the pair never observes time
                // moving backwards. A rejected or incomplete rendezvous is
                // fail-closed: the topology simply keeps the MMIO clocksource.
                let measured =
                    hal_api::arch::clock::measure_ap_tsc_warp_nanos(u32::from(logical_index));
                debug::record_milestone(
                    debug::LogCategory::Boot,
                    "smp-ap-tsc-warp",
                    u64::from(logical_index),
                    measured.unwrap_or(u64::MAX),
                );
                worst_tsc_warp_nanos = match (worst_tsc_warp_nanos, measured) {
                    (Some(worst), Some(observed)) => Some(worst.max(observed)),
                    _ => None,
                };
                break;
            }
            if hal_api::arch::clock::monotonic_nanos().saturating_sub(started_at)
                >= AP_ONLINE_PARKED_TIMEOUT_NS
            {
                hal_api::cpu::transition_lifecycle(
                    logical_index,
                    discovered.generation,
                    CpuLifecycleState::Failed,
                );
                panic!(
                    "AP did not publish OnlineParked before deadline: logical_cpu={} apic_id={:#x}",
                    logical_index, discovered.apic_id
                );
            }
            spin_loop();
        }
    }

    ap_trampoline::seal();
    assert!(
        mm_api::paging::mark_direct_map_range_readonly_noexec(
            TRAMPOLINE_PHYS,
            RESERVED_BYTES as usize,
        ),
        "kernel-mm could not retire AP startup pages R/NX"
    );

    // Every AP is still parked before `SchedulerReady`, so the boot processor is
    // the only CPU that can read the monotonic source. That makes this the one
    // point where the global time domain can be replaced without any CPU
    // observing the two domains out of order.
    let promoted = worst_tsc_warp_nanos.and_then(hal_api::arch::clock::promote_smp_tsc_clocksource);
    match promoted {
        Some(clocksource) => debug::record_milestone(
            debug::LogCategory::Boot,
            "smp-clocksource-promoted",
            clocksource.frequency_hz,
            worst_tsc_warp_nanos.unwrap_or(u64::MAX),
        ),
        None => debug::record_milestone(
            debug::LogCategory::Boot,
            "smp-clocksource-retained",
            hal_api::arch::clock::current_source()
                .map(|clocksource| clocksource.frequency_hz)
                .unwrap_or(0),
            worst_tsc_warp_nanos.unwrap_or(u64::MAX),
        ),
    }
}

extern "C" fn rustos_ap_entry(
    logical_index: u64,
    generation: u64,
    expected_apic_id: u64,
    mailbox_magic: u64,
) -> ! {
    use hal_api::cpu::CpuLifecycleState;

    hal_api::disable_interrupts();
    assert_eq!(
        mailbox_magic,
        nucleus_core::ap_trampoline::MAILBOX_MAGIC,
        "AP observed a stale or torn startup mailbox"
    );
    let logical_index =
        u8::try_from(logical_index).expect("AP mailbox logical index exceeds u8 capacity");
    let expected_apic_id =
        u32::try_from(expected_apic_id).expect("AP mailbox APIC ID exceeds u32 capacity");
    assert_eq!(
        nucleus_core::util::lockdep::hardware_apic_id(),
        expected_apic_id,
        "AP startup mailbox targeted the wrong hardware CPU"
    );
    assert_eq!(
        nucleus_core::util::lockdep::current_cpu_index(),
        usize::from(logical_index),
        "AP startup mailbox targeted the wrong logical CPU"
    );
    nucleus_core::util::lockdep::bind_current_cpu_identity(logical_index, expected_apic_id);
    let snapshot = hal_api::cpu::lifecycle_snapshot(logical_index)
        .expect("AP startup has no published lifecycle slot");
    assert_eq!(
        snapshot.generation, generation,
        "AP startup used a stale CPU generation"
    );
    assert_eq!(
        snapshot.state,
        CpuLifecycleState::Starting,
        "AP startup entered outside Starting"
    );
    debug::record_milestone(
        debug::LogCategory::Boot,
        "smp-ap-rust-entry",
        u64::from(logical_index),
        u64::from(expected_apic_id),
    );

    hal_api::boot::init_gdt_for_cpu(usize::from(logical_index));
    hal_api::init_idt();
    hal_api::init_simd();
    assert!(
        hal_api::cpu::init_local_apic(),
        "AP could not initialize its local APIC"
    );
    compat_api::init_syscalls();
    hal_api::cpu::transition_lifecycle(logical_index, generation, CpuLifecycleState::OnlineParked);
    debug::record_milestone(
        debug::LogCategory::Boot,
        "smp-ap-private-ready",
        u64::from(logical_index),
        generation,
    );

    loop {
        let snapshot = hal_api::cpu::lifecycle_snapshot(logical_index)
            .expect("AP scheduler admission lost its lifecycle slot");
        assert_eq!(
            snapshot.generation, generation,
            "AP scheduler admission observed a stale CPU generation"
        );
        match snapshot.state {
            CpuLifecycleState::OnlineParked => {
                // Parked admission is also the bounded window in which the boot
                // processor proves this CPU never observes time moving
                // backwards. Participation owns no lock and returns as soon as
                // the source closes the window.
                hal_api::arch::clock::tsc_sync_participate(u32::from(logical_index));
                spin_loop();
            }
            CpuLifecycleState::SchedulerReady => ps_api::boot::start_secondary_cpu(),
            unexpected => {
                panic!("AP scheduler admission observed invalid lifecycle state {unexpected:?}")
            }
        }
    }
}

pub fn kernel_initialization_complete() -> bool {
    matches!(
        io_services::bootstrap_phase(),
        io_services::BootstrapPhase::KernelVfsReady | io_services::BootstrapPhase::UserspaceReady
    )
}

pub fn userspace_start_allowed() -> bool {
    io_services::userspace_ready()
}

pub fn finalize_kernel_initialization() {
    flow_info(10, "kernel finalize begin");
    boot_log!(debug::LogLevel::Info, 130, 0, "kernel finalize begin");
    boot_log!(debug::LogLevel::Debug, 131, 0, "vfs init begin");
    flow_debug(11, "kernel finalize: vfs init begin");
    io_services::init_vfs();
    boot_log!(debug::LogLevel::Info, 132, 0, "vfs init done");
    flow_info(12, "kernel finalize: vfs init done");
    io_services::enter_kernel_vfs_runtime();
    flow_info(13, "kernel finalize: kernel vfs runtime active");
    flow_info(14, "kernel finalize: DVM-only driver topology active");

    let service_thread = ps_api::Thread::new(tasks::nucleus_housekeeping_task, 100);
    service_thread.start();
    flow_info(21, "kernel finalize: housekeeping task started");
    boot_log!(debug::LogLevel::Info, 135, 0, "housekeeping task started");

    let init_thread = ps_api::Thread::new(tasks::init_bootstrap_task, 90);
    init_thread.start();
    flow_info(22, "kernel finalize: init bootstrap task started");
    boot_log!(debug::LogLevel::Info, 136, 0, "init bootstrap task started");
}

pub fn bootstrap_init_process() {
    debug::record_milestone(debug::LogCategory::Boot, "init-bootstrap-enter", 0, 0);
    if !userspace_start_allowed() {
        debug::record_milestone(debug::LogCategory::Boot, "init-bootstrap-blocked", 0, 0);
        flow_debug(30, "init bootstrap blocked until userspace runtime ready");
        boot_log!(
            debug::LogLevel::Debug,
            140,
            0,
            "init bootstrap blocked phase={:?}",
            io_services::bootstrap_phase(),
        );
        return;
    }
    debug::record_milestone(debug::LogCategory::Boot, "init-bootstrap-allowed", 0, 0);
    flow_info(31, "root bootstrap begin");
    boot_log!(debug::LogLevel::Info, 141, 0, "root bootstrap begin");
    io_services::console_write(b"Bootstrapping root process...\r\n");
    boot_log!(
        debug::LogLevel::Debug,
        142,
        0,
        "init bootstrap console line written",
    );
    flow_debug(32, "init bootstrap loading rootd");
    debug::record_milestone(debug::LogCategory::Boot, "init-bootstrap-load-begin", 0, 0);
    boot_log!(
        debug::LogLevel::Info,
        143,
        0,
        "init bootstrap loading path={ROOTD_EXEC_PATH}",
    );
    let loaded = match console_host::load_executable_image_by_path(ROOTD_EXEC_PATH) {
        Ok(loaded) => loaded,
        Err(err) => fatal::fatal_init_bootstrap_load(err),
    };
    debug::record_milestone(
        debug::LogCategory::Boot,
        "init-bootstrap-load-done",
        loaded.bytes.len() as u64,
        0,
    );
    boot_log!(
        debug::LogLevel::Info,
        144,
        loaded.bytes.len() as u64,
        "init bootstrap image loaded path={} bytes={}",
        loaded.path,
        loaded.bytes.len(),
    );
    flow_debug(33, "init bootstrap image loaded");
    let program = ConsoleProgramSpec::new(
        &loaded.bytes,
        ROOTD_EXEC_PATH,
        ROOTD_BOOTSTRAP_WEIGHT_MICROS,
    )
    .with_logical_admin(true);
    match console_host::spawn_program_in_session(io_services::system_console_session_raw(), program)
    {
        Ok(_spawned) => {}
        Err(err) => fatal::fatal_init_bootstrap_spawn(err),
    }
    flow_info(34, "root bootstrap spawn complete");
    boot_log!(
        debug::LogLevel::Info,
        145,
        0,
        "root bootstrap spawn complete",
    );
    io_services::console_write(b"Root process ready.\r\n");
}

pub fn run_nucleus_loop() -> ! {
    flow_info(40, "kernel run loop entered");
    boot_log!(
        debug::LogLevel::Info,
        150,
        0,
        "nucleus loop enter init_complete={} phase={:?}",
        kernel_initialization_complete(),
        io_services::bootstrap_phase(),
    );
    ps_api::mark_root_idle();
    loop {
        x86_64::instructions::interrupts::enable_and_hlt();
    }
}

pub fn housekeeping_once() -> usize {
    let mut work = 0;

    trace_service_phase("reap");
    // Retirement can include bounded service reconciliation. Bound the number
    // of complete lifecycle transactions per scheduler turn so a process-exit
    // storm cannot concatenate individually bounded waits into an unbounded UI
    // or input stall. Unacknowledged records remain owned by the scheduler.
    for _ in 0..MAX_RETIRED_TASK_CLEANUPS_PER_TURN {
        let Some(cleanup) = ps_api::next_retired_task_cleanup() else {
            break;
        };
        work += compat_api::syscall::cleanup_retired_task_runtime_state(
            cleanup.task_id(),
            cleanup.process_id(),
            cleanup.process_terminal(),
            cleanup.clear_child_tid(),
            cleanup.robust_list_head(),
            cleanup.robust_list_len(),
        );
        if !ps_api::complete_retired_task_cleanup(cleanup) {
            panic!(
                "retired task cleanup acknowledgement lost: task_id={} process_id={}",
                cleanup.task_id(),
                cleanup.process_id()
            );
        }
        work += 1;
    }
    work += ps_api::service_deferred_work();
    work += compat_api::syscall::service_deferred_transfer_releases();
    // Shared display mappings may own large contiguous frame sets. Reclaim a
    // bounded page quantum outside process/handle locks so close, exec, and
    // exit cannot turn into a multi-megabyte allocator critical section.
    work += ps_api::service_deferred_shared_region_reclaims(64);
    work += ps_api::drain_scheduler_runtime_profile();

    trace_service_phase("heartbeat");
    // Emit the once-per-second wall-clock heartbeat outside IRQ context. The
    // RTC interrupt only marks the second as pending; the actual snapshot +
    // format + debugcon write happens here so a 700-byte log line full of
    // single-byte outb VMExits doesn't drop frames inside the IRQ handler.
    work += hal_api::arch::rtc::drain_pending_heartbeat();

    work
}

/// Transfers the validated architecture handoff into scheduled kernel startup.
///
/// # Safety
///
/// `boot_info_ptr` must satisfy [`initialize_kernel`]'s boot handoff contract.
pub unsafe fn kernel_main_bootstrap(boot_info_ptr: *const BootInfo) -> ! {
    x86_64::instructions::interrupts::disable();
    debug::boot_trace::println_fmt(format_args!("kernel: higher half entry"));
    flow_info(3, "kernel bootstrap higher-half entry");
    // SAFETY: the architecture entry preserves the boot-protocol handoff
    // pointer until scheduled bootstrap has copied all required metadata.
    unsafe { initialize_kernel(boot_info_ptr) };
    ps_api::boot::start(scheduled_kernel_main)
}

fn scheduled_kernel_main(_id: u64) {
    flow_info(4, "multitask initialized");
    finalize_kernel_initialization();
    announce_ready("Multitask", b"Multitask initialized.\r\n");
    run_nucleus_loop()
}

fn trace_service_phase(_phase: &'static str) {
    if debug::enabled!(heartbeat, debug) {
        debug::debug!(heartbeat, "service loop phase: {}", _phase);
    }
}
