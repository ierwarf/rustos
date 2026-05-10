use boot_protocol::{BootInfo, BootVolumeTransport};
use core::arch::asm;
use core::fmt::{self, Write};
use core::panic::PanicInfo;
use driver_abi::DriverClass;
use kernel_compat::api as compat_api;
use kernel_compat::api::console_host::{self, ConsoleProgramSpec};
use kernel_hal::api as hal_api;
use kernel_mm::api as mm_api;
use kernel_ps::api as ps_api;
use nucleus_core::util::{fault_injection, random};
use x86_64::VirtAddr;

use crate::{announce_ready, debug, fatal, flow_debug, flow_info, hal_hooks, io_services, tasks};

const INITD_EXEC_PATH: &str = "services/initd/initd.elf";
const MAX_BACKTRACE_FRAME_STEP: u64 = 1024 * 1024;

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
    let _ = io_services::gui_try_present_panic_blackout();
    debug::println_emergency(format_args!("[PANIC]"));
    debug::println_emergency(format_args!("message: {}", info.message()));
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
    debug::report_panic(info);
    debug::println!();
    debug::println!("[PANIC]");
    gui_panic_line(format_args!("[PANIC]"));
    debug::println!("message: {}", info.message());
    gui_panic_line(format_args!("message: {}", info.message()));

    if let Some(location) = info.location() {
        debug::println!(
            "location: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        );
        gui_panic_line(format_args!(
            "location: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        ));
    } else {
        debug::println!("location: <unknown>");
        gui_panic_line(format_args!("location: <unknown>"));
    }

    debug::dump_recent_trace_locations("panic");
    print_backtrace();
    io_services::gui_flush_debug_console();

    loop {
        core::hint::spin_loop();
    }
}

pub fn initialize_kernel(boot_info_ptr: *const BootInfo) {
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
    io_services::register_boot_volume_opener();
    boot_log!(debug::LogLevel::Debug, 110, 0, "block init begin");
    io_services::init_block_devices();
    boot_log!(debug::LogLevel::Debug, 111, 0, "block init done");
    boot_log!(
        debug::LogLevel::Debug,
        112,
        0,
        "block descriptor scan begin",
    );
    for descriptor in io_services::block_descriptors() {
        boot_log!(
            debug::LogLevel::Info,
            113,
            descriptor.id as u64,
            "storage descriptor path={} transport={:?} readonly={} block_size={} start_block={} blocks={}",
            descriptor.path,
            descriptor.transport,
            descriptor.readonly,
            descriptor.logical_block_size,
            descriptor.start_block,
            descriptor.block_count,
        );
    }
    boot_log!(debug::LogLevel::Debug, 114, 0, "block descriptor scan done");
    boot_log!(debug::LogLevel::Debug, 115, 0, "cpu-local init begin");
    io_services::init_linux_cpu_local_symbols();
    boot_log!(debug::LogLevel::Debug, 116, 0, "cpu-local init done");
    boot_log!(
        debug::LogLevel::Info,
        117,
        0,
        "linux cpu-local current_task_off={:#x} stack_guard_off={:#x}",
        compat_api::linux::current_task_offset(),
        compat_api::linux::stack_guard_offset(),
    );
    hal_hooks::register();

    announce_ready("GUI", b"GUI initialized.\r\n");
    announce_ready("Heap", b"Heap initialized.\r\n");

    hal_api::init_acpi(boot_info_ptr);
    announce_ready("ACPI", b"ACPI initialized.\r\n");

    hal_api::init_pic();
    announce_ready("PIC", b"PIC initialized.\r\n");

    io_services::init_usb();
    announce_ready("USB", b"USB initialized.\r\n");

    io_services::init_input();
    announce_ready("Input", b"Input initialized.\r\n");

    io_services::console_init();
    announce_ready("Console", b"Console initialized.\r\n");

    io_services::tty_init();
    hal_api::init_rtc();
    announce_ready("RTC", b"RTC initialized.\r\n");
    mm_api::boot::paging_smoke_test();

    random::init(boot_info_ptr);
    announce_ready("Random", b"Random initialized.\r\n");

    compat_api::init_syscalls();
    announce_ready("Syscall", b"Syscall initialized.\r\n");

    let _ = boot_info;
    flow_info(2, "kernel initialize complete");
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
    flow_debug(14, "kernel finalize: display driver init begin");
    if !io_services::initialize_loadable_modules_for_class(DriverClass::Display) {
        flow_info(15, "kernel finalize: display driver init failed");
        boot_log!(debug::LogLevel::Error, 133, 0, "display driver load failed");
        io_services::console_write(b"Display driver load failed.\r\n");
        panic!("display driver load failed");
    }
    flow_info(16, "kernel finalize: display driver init done");
    flow_debug(17, "kernel finalize: input driver init begin");
    if !io_services::initialize_loadable_modules_for_class(DriverClass::Input) {
        flow_info(18, "kernel finalize: input driver init failed");
        boot_log!(debug::LogLevel::Error, 134, 0, "input driver load failed");
        io_services::console_write(b"Input driver load failed.\r\n");
        panic!("input driver load failed");
    }
    flow_info(19, "kernel finalize: input driver init done");
    flow_debug(20, "kernel finalize: network driver init begin");
    if !io_services::initialize_loadable_modules_for_class(DriverClass::Network) {
        flow_info(20, "kernel finalize: network driver init failed");
        boot_log!(debug::LogLevel::Warn, 137, 0, "network driver load failed");
        io_services::console_write(b"Network driver load failed.\r\n");
    } else {
        flow_info(20, "kernel finalize: network driver init done");
    }

    let service_thread = ps_api::Thread::new(tasks::nucleus_housekeeping_task, 100);
    service_thread.start();
    core::mem::forget(service_thread);
    flow_info(21, "kernel finalize: housekeeping task started");
    boot_log!(debug::LogLevel::Info, 135, 0, "housekeeping task started");

    let init_thread = ps_api::Thread::new(tasks::init_bootstrap_task, 90);
    init_thread.start();
    core::mem::forget(init_thread);
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
    flow_info(31, "init bootstrap begin");
    boot_log!(debug::LogLevel::Info, 141, 0, "init bootstrap begin");
    io_services::console_write(b"Bootstrapping init process...\r\n");
    boot_log!(
        debug::LogLevel::Debug,
        142,
        0,
        "init bootstrap console line written",
    );
    flow_debug(32, "init bootstrap loading initd");
    debug::record_milestone(debug::LogCategory::Boot, "init-bootstrap-load-begin", 0, 0);
    boot_log!(
        debug::LogLevel::Info,
        143,
        0,
        "init bootstrap loading path={INITD_EXEC_PATH}",
    );
    let loaded = match console_host::load_executable_image_by_path(INITD_EXEC_PATH, None) {
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
    let program =
        ConsoleProgramSpec::new(&loaded.bytes, INITD_EXEC_PATH, 50).with_logical_admin(true);
    match console_host::spawn_program_in_session(io_services::system_console_session_raw(), program)
    {
        Ok(_spawned) => {}
        Err(err) => fatal::fatal_init_bootstrap_spawn(err),
    }
    flow_info(34, "init bootstrap spawn complete");
    boot_log!(
        debug::LogLevel::Info,
        145,
        0,
        "init bootstrap spawn complete",
    );
    io_services::console_write(b"Init process ready.\r\n");
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
    loop {
        x86_64::instructions::interrupts::enable_and_hlt();
    }
}

pub fn housekeeping_once() -> usize {
    let mut work = 0;

    trace_service_phase("compat");
    compat_api::service_pending();

    trace_service_phase("tty");
    work += io_services::input_service_pending();

    trace_service_phase("reap");
    work += ps_api::service_deferred_work();

    trace_service_phase("console");
    work += io_services::console_service();

    work
}

pub fn kernel_main_bootstrap(boot_info_ptr: *const BootInfo) -> ! {
    x86_64::instructions::interrupts::disable();
    debug::boot_trace::println_fmt(format_args!("kernel: higher half entry"));
    flow_info(3, "kernel bootstrap higher-half entry");
    initialize_kernel(boot_info_ptr);
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

fn print_backtrace() {
    let mut frame = current_frame_pointer();
    debug::println!("backtrace:");
    gui_panic_line(format_args!("backtrace:"));

    for index in 0..16 {
        let Some(current) = frame else {
            break;
        };
        let Some((next_rbp, return_rip)) = read_backtrace_frame(current) else {
            break;
        };
        if return_rip == 0 {
            break;
        }
        debug::println!("  {:02}: rip={:#x} rbp={:#x}", index, return_rip, current);
        gui_panic_line(format_args!(
            "  {:02}: rip={:#x} rbp={:#x}",
            index, return_rip, current
        ));

        frame = if valid_next_frame_pointer(current, next_rbp) {
            Some(next_rbp)
        } else {
            None
        };
    }
}

fn current_frame_pointer() -> Option<u64> {
    let rbp: u64;
    unsafe {
        asm!("mov {}, rbp", out(reg) rbp, options(nomem, nostack, preserves_flags));
    }
    if rbp == 0 { None } else { Some(rbp) }
}

fn canonical_kernel_pointer(value: u64) -> Option<VirtAddr> {
    let addr = VirtAddr::new(value);
    if addr.as_u64() < 0xffff_8000_0000_0000 {
        return None;
    }
    Some(addr)
}

fn read_backtrace_frame(rbp: u64) -> Option<(u64, u64)> {
    if rbp % core::mem::align_of::<u64>() as u64 != 0 {
        return None;
    }
    let frame_end = rbp.checked_add((core::mem::size_of::<u64>() * 2 - 1) as u64)?;
    let rbp_addr = canonical_kernel_pointer(rbp)?;
    canonical_kernel_pointer(frame_end)?;
    let next_rbp = unsafe { core::ptr::read_volatile(rbp_addr.as_ptr::<u64>()) };
    let return_rip = unsafe { core::ptr::read_volatile(rbp_addr.as_ptr::<u64>().add(1)) };
    if return_rip != 0 {
        canonical_kernel_pointer(return_rip)?;
    }
    Some((next_rbp, return_rip))
}

fn valid_next_frame_pointer(current: u64, next: u64) -> bool {
    if next <= current || next - current > MAX_BACKTRACE_FRAME_STEP {
        return false;
    }
    if next % core::mem::align_of::<u64>() as u64 != 0 {
        return false;
    }
    canonical_kernel_pointer(next).is_some()
}

fn gui_panic_line(args: fmt::Arguments<'_>) {
    let mut line = PanicLine::new();
    let _ = line.write_fmt(args);
    let _ = io_services::gui_write_panic_console_line(line.as_bytes());
    io_services::console_write(line.as_bytes());
    io_services::console_write(b"\r\n");
}

struct PanicLine {
    bytes: [u8; 256],
    len: usize,
}

impl PanicLine {
    const fn new() -> Self {
        Self {
            bytes: [0; 256],
            len: 0,
        }
    }

    fn push(&mut self, byte: u8) {
        if self.len < self.bytes.len() {
            self.bytes[self.len] = byte;
            self.len += 1;
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl Write for PanicLine {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for ch in s.chars() {
            match ch {
                '\n' | '\r' | '\t' => self.push(b' '),
                ' '..='~' => self.push(ch as u8),
                _ => self.push(b'?'),
            }
        }
        Ok(())
    }
}
