use super::*;

pub fn initialize_kernel(boot_info_ptr: *const BootInfo) {
    flow_info(1, "kernel initialize begin");
    let boot_info = match unsafe { BootInfo::from_ptr(boot_info_ptr) } {
        Ok(boot_info) => boot_info,
        Err(error) => panic!("{}", error.as_str()),
    };

    io_manager_api::api::init_gui(boot_info_ptr);
    debug::init(boot_info_ptr);
    debug::println!(
        "RUST OS loaded in higher half: rip={:#x}",
        hal_api::current_rip()
    );
    debug::println!("GDT loaded.");
    debug::println!("IDT loaded.");
    debug::println!("Paging initialized.");
    mm_api::init_phys(boot_info_ptr);
    debug::println!(
        "Physical memory initialized: usable={} KiB free={} KiB",
        mm_api::usable_bytes() / 1024,
        mm_api::free_bytes() / 1024
    );
    mm_api::init_heap();
    debug::println!("Heap initialized.");
    hal_api::init_simd();
    debug::println!("SIMD initialized ({}).", hal_api::simd_mode_name());

    io_manager_api::api::init_boot_info(boot_info_ptr);
    let transport_hint =
        io_manager_api::api::boot_volume_transport_hint().unwrap_or(BootVolumeTransport::Unknown);
    match io_manager_api::api::boot_volume_identity() {
        Some(identity) => debug::println!(
            "boot volume identity: transport={:?} serial={:#010x} start_lba={} sectors={}",
            identity.transport(),
            identity.fat_volume_id,
            identity.volume_start_lba,
            identity.volume_sector_count
        ),
        None if transport_hint != BootVolumeTransport::Unknown => debug::println!(
            "boot volume identity: unavailable, transport hint={:?}",
            transport_hint
        ),
        None => debug::println!("boot volume identity: unavailable"),
    }
    io_manager_api::api::register_boot_volume_opener();
    debug::println!("boot stage: block init begin");
    io_manager_api::api::init_block_devices();
    debug::println!("boot stage: block init done");
    debug::println!("boot stage: block descriptors begin");
    for descriptor in io_manager_api::api::block_descriptors() {
        debug::println!(
            "storage descriptor: id={} path={} transport={:?} readonly={} block_size={} start_block={} blocks={}",
            descriptor.id,
            descriptor.path,
            descriptor.transport,
            descriptor.readonly,
            descriptor.logical_block_size,
            descriptor.start_block,
            descriptor.block_count
        );
    }
    debug::println!("boot stage: block descriptors done");
    debug::println!("boot stage: cpu-local begin");
    compat_api::linux::init_cpu_local_symbols();
    debug::println!("boot stage: cpu-local done");
    debug::println!(
        "Linux compat CPU-local symbols initialized: current_task_off={:#x} stack_guard_off={:#x}",
        compat_api::linux::current_task_offset(),
        compat_api::linux::stack_guard_offset()
    );

    announce_ready("GUI", b"GUI initialized.\r\n");
    announce_ready("Heap", b"Heap initialized.\r\n");

    hal_api::init_acpi(boot_info_ptr);
    announce_ready("ACPI", b"ACPI initialized.\r\n");

    hal_api::init_pic();
    announce_ready("PIC", b"PIC initialized.\r\n");

    io_manager_api::api::init_usb();
    announce_ready("USB", b"USB initialized.\r\n");

    io_manager_api::api::init_input();
    announce_ready("Input", b"Input initialized.\r\n");

    io_manager_api::api::init_console();
    announce_ready("Console", b"Console initialized.\r\n");

    io_manager_api::api::init_tty();
    hal_api::init_rtc();
    announce_ready("RTC", b"RTC initialized.\r\n");
    mm_api::paging_smoke_test();

    random::init(boot_info_ptr);
    announce_ready("Random", b"Random initialized.\r\n");

    compat_api::init_syscalls();
    announce_ready("Syscall", b"Syscall initialized.\r\n");

    let _ = boot_info;
    flow_info(2, "kernel initialize complete");
}

pub fn kernel_initialization_complete() -> bool {
    matches!(
        bootstrap_phase(),
        BootstrapPhase::KernelVfsReady | BootstrapPhase::UserspaceReady
    )
}

pub fn userspace_start_allowed() -> bool {
    io_manager_api::api::userspace_ready()
}

pub fn finalize_kernel_initialization() {
    flow_info(10, "kernel finalize begin");
    debug::println!("kernel init: finalize begin");
    debug::println!("kernel init: vfs::init begin");
    flow_debug(11, "kernel finalize: vfs init begin");
    io_manager_api::api::init_vfs();
    debug::println!("kernel init: vfs::init done");
    flow_info(12, "kernel finalize: vfs init done");
    io_manager_api::api::enter_kernel_vfs_runtime();
    flow_info(13, "kernel finalize: kernel vfs runtime active");
    flow_debug(14, "kernel finalize: display driver init begin");
    if !io_manager_api::api::initialize_loadable_modules_for_class(DriverClass::Display) {
        flow_info(15, "kernel finalize: display driver init failed");
        debug::println!("kernel init: display driver load failed");
        io_manager_api::api::write_console(b"Display driver load failed.\r\n");
        panic!("display driver load failed");
    }
    flow_info(16, "kernel finalize: display driver init done");
    flow_debug(17, "kernel finalize: input driver init begin");
    if !io_manager_api::api::initialize_loadable_modules_for_class(DriverClass::Input) {
        flow_info(18, "kernel finalize: input driver init failed");
        debug::println!("kernel init: input driver load failed");
        io_manager_api::api::write_console(b"Input driver load failed.\r\n");
        panic!("input driver load failed");
    }
    flow_info(19, "kernel finalize: input driver init done");

    let service_thread = ps_api::api::Thread::new(tasks::nucleus_housekeeping_task, 100);
    service_thread.start();
    core::mem::forget(service_thread);
    flow_info(21, "kernel finalize: housekeeping task started");
    debug::println!("kernel init: housekeeping task started");

    let init_thread = ps_api::api::Thread::new(tasks::init_bootstrap_task, 90);
    init_thread.start();
    core::mem::forget(init_thread);
    flow_info(22, "kernel finalize: init bootstrap task started");
    debug::println!("kernel init: init bootstrap task started");
}

pub fn bootstrap_init_process() {
    if !userspace_start_allowed() {
        flow_debug(30, "init bootstrap blocked until userspace runtime ready");
        debug::println!(
            "init bootstrap: blocked until userspace runtime ready phase={:?}",
            bootstrap_phase()
        );
        return;
    }
    flow_info(31, "init bootstrap begin");
    debug::println!("init bootstrap: begin");
    io_manager_api::api::write_console(b"Bootstrapping init process...\r\n");
    debug::println!("init bootstrap: console line written");
    flow_debug(32, "init bootstrap loading initd");
    debug::println!("init bootstrap: loading {}", INITD_EXEC_PATH);
    let loaded = match console_host::load_executable_image_by_path(INITD_EXEC_PATH, None) {
        Ok(loaded) => loaded,
        Err(err) => fatal::fatal_init_bootstrap_load(err),
    };
    debug::println!(
        "init bootstrap: image loaded path={} bytes={}",
        loaded.path,
        loaded.bytes.len(),
    );
    flow_debug(33, "init bootstrap image loaded");
    let program =
        ConsoleProgramSpec::new(&loaded.bytes, INITD_EXEC_PATH, 50).with_logical_admin(true);
    match console_host::spawn_program_in_session(
        io_manager_api::api::system_console_session(),
        program,
    ) {
        Ok(_spawned) => {}
        Err(err) => fatal::fatal_init_bootstrap_spawn(err),
    }
    flow_info(34, "init bootstrap spawn complete");
    debug::println!("init bootstrap: spawn done");
    io_manager_api::api::write_console(b"Init process ready.\r\n");
}

pub fn run_nucleus_loop() -> ! {
    flow_info(40, "kernel run loop entered");
    debug::println!(
        "nucleus loop: enter init_complete={} phase={:?}",
        kernel_initialization_complete(),
        bootstrap_phase()
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
    work += io_manager_api::api::service_input_pending();

    trace_service_phase("reap");
    work += ps_api::api::service_deferred_work();

    trace_service_phase("console");
    work += io_manager_api::api::service_console();

    work
}

pub fn kernel_main_bootstrap(boot_info_ptr: *const BootInfo) -> ! {
    x86_64::instructions::interrupts::disable();
    debug::boot_trace::println_fmt(format_args!("kernel: higher half entry"));
    flow_info(3, "kernel bootstrap higher-half entry");
    initialize_kernel(boot_info_ptr);
    ps_api::api::init();
    flow_info(4, "multitask initialized");
    finalize_kernel_initialization();
    announce_ready("Multitask", b"Multitask initialized.\r\n");
    run_nucleus_loop()
}

fn trace_service_phase(phase: &'static str) {
    if debug::should_emit(
        diag_abi::DiagProvider::Heartbeat,
        diag_abi::DiagLevel::Debug,
    ) {
        debug::emit_text(
            diag_abi::DiagProvider::Heartbeat,
            diag_abi::DiagLevel::Debug,
            10,
            0,
            0,
            format!("service loop phase: {}", phase).as_str(),
        );
    }
}
