// SPDX-License-Identifier: MIT

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RustosSmpReadiness {
    rustos_vcpus: u8,
    per_cpu_scheduler: bool,
    per_cpu_syscall_state: bool,
    cpu_online_state_machine: bool,
    reschedule_ipi: bool,
    tlb_shootdown: bool,
    atomic_robust_futex_cleanup: bool,
}

const RUSTOS_SMP_READINESS: RustosSmpReadiness = RustosSmpReadiness {
    rustos_vcpus: 1,
    per_cpu_scheduler: false,
    per_cpu_syscall_state: false,
    cpu_online_state_machine: false,
    reschedule_ipi: false,
    tlb_shootdown: false,
    atomic_robust_futex_cleanup: false,
};

impl RustosSmpReadiness {
    const fn prerequisites_complete(self) -> bool {
        self.per_cpu_scheduler
            && self.per_cpu_syscall_state
            && self.cpu_online_state_machine
            && self.reschedule_ipi
            && self.tlb_shootdown
            && self.atomic_robust_futex_cleanup
    }

    fn validate(self) -> Result<()> {
        if self.rustos_vcpus == 0 {
            bail!("RustOS KVM vCPU count must be nonzero");
        }
        if self.rustos_vcpus > 1 && !self.prerequisites_complete() {
            bail!(
                "RustOS SMP topology requested before per-CPU scheduler/syscall state, CPU-online, reschedule IPI, TLB shootdown, and atomic robust-futex cleanup are complete"
            );
        }
        Ok(())
    }
}

// The paired launch keeps both guest, transport, display, and relay-gate
// ownership arguments explicit at the orchestration boundary.
#[allow(clippy::too_many_arguments)]
fn spawn_guests(
    qemu: &Path,
    config: &Config,
    artifacts: &DvmArtifacts,
    layout: &KvmLayout,
    options: &SmokeOptions,
    guest_display: GuestDisplay,
    host_render_node: Option<&Path>,
    display_doorbell: Option<&IvshmemDoorbellServer>,
    block_doorbell: Option<&IvshmemDoorbellServer>,
    input_doorbell: &IvshmemDoorbellServer,
    input_relay_gate: Arc<AtomicBool>,
) -> Result<(Child, Child)> {
    let rustos = spawn_rustos_guest(qemu, config, layout, false)?;

    if let Err(error) = input_doorbell.wait_for_peer_count(1, DVM_INPUT_FIRST_PEER_TIMEOUT) {
        let mut rustos = rustos;
        stop_guest(&mut rustos);
        return Err(error)
            .context("RustOS did not claim fixed input ivshmem peer ID 0 before DVM launch");
    }
    input_relay_gate.store(true, Ordering::Release);

    if let Some(display_doorbell) = display_doorbell
        && let Err(error) = display_doorbell.wait_for_peer_count(1, DVM_DISPLAY_FIRST_PEER_TIMEOUT)
    {
        let mut rustos = rustos;
        stop_guest(&mut rustos);
        return Err(error).context("RustOS did not claim ivshmem peer ID 0 before DVM launch");
    }
    if let Some(block_doorbell) = block_doorbell
        && let Err(error) = block_doorbell.wait_for_peer_count(1, DVM_BLOCK_FIRST_PEER_TIMEOUT)
    {
        let mut rustos = rustos;
        stop_guest(&mut rustos);
        return Err(error)
            .context("RustOS did not claim block ivshmem peer ID 0 before DVM launch");
    }

    let dvm = match spawn_dvm_guest(
        qemu,
        artifacts,
        layout,
        options,
        guest_display,
        host_render_node,
        false,
    ) {
        Ok(dvm) => dvm,
        Err(error) => {
            let mut rustos = rustos;
            stop_guest(&mut rustos);
            return Err(error);
        }
    };
    Ok((rustos, dvm))
}

fn spawn_rustos_guest(
    qemu: &Path,
    config: &Config,
    layout: &KvmLayout,
    append_logs: bool,
) -> Result<Child> {
    RUSTOS_SMP_READINESS.validate()?;
    if layout.rustos_monitor.exists() {
        fs::remove_file(&layout.rustos_monitor).with_context(|| {
            format!(
                "remove stale RustOS monitor before reboot {}",
                layout.rustos_monitor.display()
            )
        })?;
    }
    let mut rustos_command = Command::new(qemu);
    rustos_command
        .arg("-name")
        .arg("rustos-kvm")
        .args([
            "-machine",
            "q35,accel=kvm,hpet=on",
            "-cpu",
            RUSTOS_DVM_KVM_CPU,
            "-m",
            "2048M,maxmem=3G,slots=2",
            "-smp",
        ])
        // RustOS currently schedules all user work on the BSP. The readiness
        // contract above refuses a second vCPU until every AP prerequisite is
        // explicit rather than silently booting an unused processor.
        .arg(RUSTOS_SMP_READINESS.rustos_vcpus.to_string())
        .arg("-bios")
        .arg(&config.ovmf_path)
        .arg("-drive")
        .arg(format!(
            "file={},format=raw,if=ide",
            layout.runtime_disk.display()
        ))
        // Keep the smoke headless. The normal topology exposes a direct
        // virtio-gpu test device; the DVM-display topology gets a separately
        // initialized, fixed-layout ivshmem aperture below.
        .args([
            "-display",
            "none",
            "-vga",
            "none",
            "-nic",
            "none",
            "-no-reboot",
            "-no-shutdown",
            "-snapshot",
        ])
        .arg("-chardev")
        .arg(format!(
            "file,id=debugcon,path={},append={}",
            layout.debugcon_log.display(),
            if append_logs { "on" } else { "off" },
        ))
        .arg("-device")
        .arg("isa-debugcon,iobase=0xe9,chardev=debugcon")
        .arg("-chardev")
        .arg(format!(
            "file,id=serial,path={},append={}",
            layout.rustos_serial_log.display(),
            if append_logs { "on" } else { "off" },
        ))
        .args(["-serial", "chardev:serial"])
        .arg("-monitor")
        .arg(format!(
            "unix:{},server,nowait",
            layout.rustos_monitor.display()
        ));
    if std::env::var_os("RUSTOS_KVM_QEMU_INT_TRACE").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        rustos_command
            .args(["-d", "int,cpu_reset"])
            .arg("-D")
            .arg(
                layout
                    .rustos_serial_log
                    .with_file_name("rustos-qemu-int.log"),
            )
            .args([
                "-trace",
                "enable=kvm_run_exit",
                "-trace",
                "enable=kvm_run_exit_system_event",
            ]);
    }
    append_dvm_input_doorbell(&mut rustos_command, &layout.dvm_input_doorbell);
    if let Some(doorbell) = layout.dvm_display_doorbell.as_deref() {
        append_dvm_display_doorbell(&mut rustos_command, doorbell);
        append_dvm_display_pixels(
            &mut rustos_command,
            layout
                .gui_dvm_pixels
                .as_deref()
                .context("GUI-DVM control exists without a pixel backend")?,
            false,
        );
    } else {
        rustos_command
            .arg("-device")
            .arg("virtio-gpu-pci,id=rustos-virtio-gpu,xres=1280,yres=800");
    }
    if let Some(shared_network) = layout.dvm_network_shmem.as_deref() {
        append_dvm_network_ivshmem(&mut rustos_command, shared_network);
    }
    if let Some(doorbell) = layout.dvm_block_doorbell.as_deref() {
        append_dvm_block_doorbell(&mut rustos_command, doorbell);
    }
    append_fault_injection(config, &mut rustos_command);
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append_logs)
        .truncate(!append_logs)
        .open(&layout.rustos_stderr_log)?;
    rustos_command.stdout(Stdio::null()).stderr(Stdio::from(stderr));
    rustos_command
        .spawn()
        .context("failed to start RustOS QEMU/KVM guest")
}

fn spawn_dvm_guest(
    qemu: &Path,
    artifacts: &DvmArtifacts,
    layout: &KvmLayout,
    options: &SmokeOptions,
    guest_display: GuestDisplay,
    host_render_node: Option<&Path>,
    append_logs: bool,
) -> Result<Child> {
    let mut dvm_command = Command::new(qemu);
    let dvm_append = if options.exercise_input {
        "console=ttyS0 preempt=full rustos.dvm.input-selftest=1"
    } else {
        "console=ttyS0 preempt=full"
    };
    let dvm_display = qemu_display_backend(guest_display, host_render_node)?;
    if guest_display == GuestDisplay::DvmGtk {
        let render_node =
            host_render_node.context("GTK display lost its validated host render node")?;
        let dri_prime = mesa_dri_prime_for_render_node(render_node)?;
        eprintln!(
            "xtask: KVM GTK renderer pinned node={} DRI_PRIME={dri_prime}",
            render_node.display()
        );
        dvm_command.env("DRI_PRIME", dri_prime);
    }
    dvm_command
        .arg("-name")
        .arg("rustos-linux-dvm-kvm")
        .args([
            "-machine",
            dvm_machine(),
            "-cpu",
            "host",
            "-m",
            DVM_GUEST_MEMORY,
            "-smp",
            "2",
        ])
        .arg("-kernel")
        .arg(&artifacts.kernel)
        .arg("-initrd")
        .arg(&artifacts.rootfs)
        .args([
            "-append",
            dvm_append,
            "-display",
            &dvm_display,
            "-vga",
            "none",
            "-no-reboot",
        ])
        .arg("-chardev")
        .arg(format!(
            "file,id=serial,path={},append={}",
            layout.dvm_serial_log.display(),
            if append_logs { "on" } else { "off" },
        ))
        .args(["-serial", "chardev:serial"])
        .arg("-device")
        .arg(format!("vhost-vsock-pci,guest-cid={}", layout.guest_cid))
        .arg("-fw_cfg")
        .arg(format!(
            "name=opt/rustos/dvm-control-secret,file={}",
            layout.dvm_control_secret.display()
        ));
    append_dvm_network_device(&mut dvm_command, guest_display);
    if !append_dvm_virtual_gpu(&mut dvm_command, guest_display) {
        let bdf = options
            .physical_gpu_bdf
            .as_deref()
            .context("physical GPU display selected without a BDF")?;
        let profile = selected_physical_gpu_profile(options)?;
        let firmware = std::fs::canonicalize(
            options
                .physical_gpu_firmware
                .as_deref()
                .context("physical GPU display selected without profile firmware")?,
        )?;
        append_physical_gpu(&mut dvm_command, profile, bdf, &firmware);
    }
    append_dvm_input_devices(&mut dvm_command, guest_display);
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append_logs)
        .truncate(!append_logs)
        .open(&layout.dvm_stderr_log)?;
    dvm_command.stdout(Stdio::null()).stderr(Stdio::from(stderr));
    if let Some(doorbell) = layout.dvm_display_doorbell.as_deref() {
        append_dvm_display_doorbell(&mut dvm_command, doorbell);
        append_dvm_display_pixels(
            &mut dvm_command,
            layout
                .gui_dvm_pixels
                .as_deref()
                .context("GUI-DVM control exists without a pixel backend")?,
            true,
        );
    }
    if let Some(shared_network) = layout.dvm_network_shmem.as_deref() {
        append_dvm_network_ivshmem(&mut dvm_command, shared_network);
    }
    if let Some(doorbell) = layout.dvm_block_doorbell.as_deref() {
        append_dvm_block_doorbell(&mut dvm_command, doorbell);
        append_dvm_virtual_storage(
            &mut dvm_command,
            layout
                .dvm_block_disk
                .as_deref()
                .context("DVM block aperture exists without a private backing disk")?,
        );
    }
    dvm_command
        .spawn()
        .context("failed to start Linux DVM QEMU/KVM guest")
}

fn append_dvm_input_doorbell(command: &mut Command, socket_path: &Path) {
    command
        .arg("-chardev")
        .arg(format!(
            "socket,id=dvm-input-doorbell,path={}",
            socket_path.display(),
        ))
        .arg("-device")
        .arg("ivshmem-doorbell,vectors=1,chardev=dvm-input-doorbell");
}

fn append_dvm_display_doorbell(command: &mut Command, socket_path: &Path) {
    command
        .arg("-chardev")
        .arg(format!(
            "socket,id=dvm-display-doorbell,path={}",
            socket_path.display(),
        ))
        .arg("-device")
        .arg("ivshmem-doorbell,vectors=2,chardev=dvm-display-doorbell");
}

fn append_dvm_block_doorbell(command: &mut Command, socket_path: &Path) {
    command
        .arg("-chardev")
        .arg(format!(
            "socket,id=dvm-block-doorbell,path={}",
            socket_path.display(),
        ))
        .arg("-device")
        .arg("ivshmem-doorbell,vectors=1,chardev=dvm-block-doorbell");
}

fn append_dvm_virtual_storage(command: &mut Command, disk: &Path) {
    command
        .arg("-drive")
        .arg(format!(
            "file={},format=raw,if=none,id=dvm-storage-disk,cache=none,aio=threads",
            disk.display()
        ))
        .arg("-device")
        // q35 already owns exactly one ICH9 AHCI controller. Attach the
        // private namespace to that controller instead of adding a second
        // NVMe controller, which would correctly fail the storage DVM's
        // exact-single-controller admission.
        .arg("ide-hd,drive=dvm-storage-disk,bus=ide.0,unit=0,id=dvm-storage-disk-device");
}

fn append_dvm_display_pixels(command: &mut Command, path: &Path, read_only: bool) {
    let mut backend = format!(
        "memory-backend-file,id=dvm-display-pixels,mem-path={},size={},share=on",
        path.display(),
        DVM_DISPLAY_REGION_BYTES
    );
    if read_only {
        backend.push_str(",readonly=on,rom=on");
    } else {
        // Fault every tmpfs page before the VFIO device is attached in the
        // second QEMU process. IOMMUFD must never discover an unpinnable or
        // unpopulated source aperture only after device activation.
        backend.push_str(",prealloc=on");
    }
    command
        .arg("-object")
        .arg(backend)
        .arg("-device")
        .arg(format!(
            "virtio-pmem-pci,id=dvm-display-pmem,memdev=dvm-display-pixels,memaddr={DVM_DISPLAY_PIXEL_PHYS_ADDR}"
        ));
}

/// Add an already-bound device from the sealed physical-GPU profile registry.
///
/// QEMU 11.0 exposes each mmap-able VFIO PCI BAR through the kernel's
/// VFIO_DEVICE_FEATURE_DMA_BUF API before IOMMUFD maps it. Keep BAR mmap enabled:
/// `x-no-mmap=on` bypasses that API, falls back to slow MMIO, and recreates the
/// unsupported PCI-BAR mapping that caused QEMU 10.2.1 to abort before DVM boot.
fn append_physical_gpu(
    command: &mut Command,
    profile: PhysicalGpuProfile,
    bdf: &str,
    firmware: &Path,
) {
    command.args(["-object", "iommufd,id=iommufd0"]);
    match profile.firmware_kind {
        PhysicalGpuFirmwareKind::AmdVfct => {
            command
                .arg("-acpitable")
                .arg(format!("file={}", firmware.display()));
        }
    }
    command
        .args(["-trace", "enable=vfio_listener_region_add_ram"])
        .args(["-trace", "enable=iommufd_backend_map_dma"])
        .args(["-trace", "enable=iommufd_backend_map_file_dma"])
        .args(["-trace", "enable=vfio_region_dmabuf"])
        .arg("-device")
        .arg(format!(
            "vfio-pci,host={bdf},iommufd=iommufd0,addr={},rombar=0",
            profile.guest_address
        ));
}

fn append_dvm_network_ivshmem(command: &mut Command, path: &Path) {
    command
        .arg("-object")
        .arg(format!(
            "memory-backend-file,id=dvm-network-shm,mem-path={},size={},share=on",
            path.display(),
            DVM_NET_REGION_BYTES
        ))
        .arg("-device")
        .arg("ivshmem-plain,memdev=dvm-network-shm");
}

fn append_fault_injection(config: &Config, command: &mut Command) {
    if !config.project.fault_injection.enabled {
        return;
    }
    let payload = config.project.fault_injection.rules.join(";");
    if !payload.is_empty() {
        command
            .arg("-fw_cfg")
            .arg(format!("name=opt/rustos/fault-injection,string={payload}"));
    }
}

fn wait_for_parallel_boot(
    rustos: &mut Child,
    dvm: &mut Child,
    layout: &KvmLayout,
    options: &SmokeOptions,
    boot_started: Instant,
    deadline: Instant,
    control_relay: &Receiver<Result<ProbeResult>>,
) -> Result<ProbeResult> {
    let mut control_ready = None;
    let gpu_evidence = gpu_evidence_expectation(options)?;
    loop {
        check_guest_running(rustos, "RustOS", &layout.rustos_stderr_log)?;
        check_guest_running(dvm, "Linux DVM", &layout.dvm_stderr_log)?;
        let rustos_log = fs::read_to_string(&layout.debugcon_log)?;
        let dvm_log = fs::read_to_string(&layout.dvm_serial_log)?;
        if !options.storage_only
            && !rustos_log.contains(RUSTOS_GPU_ACTIVE_MARKER)
            && guest_deadline_reached(&rustos_log, BOOT_TO_UI_HARD_LIMIT_MS)
        {
            let reason = format!(
                "RustOS interactive UI missed the {} ms boot acceptance limit",
                BOOT_TO_UI_HARD_LIMIT_MS
            );
            let missing_rustos = vec![RUSTOS_GPU_ACTIVE_MARKER.to_owned()];
            let evidence = write_kvm_failure_summary(
                layout,
                &reason,
                boot_started.elapsed(),
                &rustos_log,
                &dvm_log,
                &missing_rustos,
                &[],
            )?;
            bail!(
                "{reason}; missing={RUSTOS_GPU_ACTIVE_MARKER:?}; evidence={}; inspect {} and {}",
                evidence.display(),
                layout.debugcon_log.display(),
                layout.dvm_serial_log.display(),
            );
        }
        if !options.storage_only
            && options.dvm_block_shmem
            && !rustos_log.contains(WAYCLICK_FIRST_FRAME_MARKER)
            && guest_deadline_reached(&rustos_log, BOOT_TO_UI_HARD_LIMIT_MS)
        {
            let reason = format!(
                "RustOS user-visible desktop missed the {} ms boot acceptance limit",
                BOOT_TO_UI_HARD_LIMIT_MS
            );
            let missing_rustos = vec![WAYCLICK_FIRST_FRAME_MARKER.to_owned()];
            let evidence = write_kvm_failure_summary(
                layout,
                &reason,
                boot_started.elapsed(),
                &rustos_log,
                &dvm_log,
                &missing_rustos,
                &[],
            )?;
            bail!(
                "{reason}; missing={WAYCLICK_FIRST_FRAME_MARKER:?}; evidence={}; inspect {} and {}",
                evidence.display(),
                layout.debugcon_log.display(),
                layout.dvm_serial_log.display(),
            );
        }
        if options.expect_block_flush_fault && rustos_log.contains(RUSTOS_DVM_BLOCK_E2E_MARKER) {
            bail!("storage-DVM flush fault proof observed an impossible E2E flush-success marker");
        }
        if options.gui_dvm_surfaces
            && let Some(failure) = dvm_display_failure(&dvm_log, options.physical_gpu_bdf.is_some())
        {
            bail!("Linux DVM display relay failed before readiness: {failure}");
        }
        let rustos_ready = options
            .expected_markers
            .iter()
            .all(|marker| rustos_log.contains(marker));
        let dvm_ready = options
            .expected_dvm_markers
            .iter()
            .all(|marker| dvm_log.contains(marker));
        let dvm_gpu_ready = required_dvm_gpu_ready(options, &dvm_log, gpu_evidence);
        match control_relay.try_recv() {
            Ok(Ok(probe)) => {
                if control_ready.replace(probe).is_some() {
                    bail!("Linux DVM input relay reported readiness more than once");
                }
            }
            Ok(Err(error)) => {
                let phase = if control_ready.is_some() {
                    "after readiness"
                } else {
                    "before readiness"
                };
                let reason = format!("Linux DVM input relay failed {phase}: {error:#}");
                let evidence = write_kvm_failure_summary(
                    layout,
                    &reason,
                    boot_started.elapsed(),
                    &rustos_log,
                    &dvm_log,
                    &[],
                    &[],
                )?;
                bail!("{reason}; evidence={}", evidence.display());
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) if control_ready.is_none() => {
                bail!("Linux DVM host input relay terminated without a readiness result")
            }
            Err(mpsc::TryRecvError::Disconnected) => {}
        }
        let ui_render_fps_ready = options.min_ui_fps.is_none_or(|minimum| {
            uiserver_profile_meets_fps(&rustos_log, minimum, options.ui_proof_windows)
        });
        let ui_input_ready = options.min_ui_fps.is_none_or(|minimum| {
            uiserver_profile_input_pipeline_healthy(
                &rustos_log,
                options.ui_proof_windows,
                Some(minimum),
            )
        });
        let wayclick_fps_ready = options.min_ui_fps.is_none_or(|minimum| {
            wayclick_profile_meets_fps(&rustos_log, minimum, options.ui_proof_windows)
        });
        let ui_runtime_ready = !runtime_stall_or_crash_observed(&rustos_log);
        let dvm_relay_fps_ready = options.min_ui_fps.is_none_or(|minimum| {
            !options.gui_dvm_surfaces
                || dvm_display_relay_meets_fps(&dvm_log, minimum, options.ui_proof_windows)
        });
        let dvm_runtime_ready = !runtime_stall_or_crash_observed(&dvm_log);
        let ui_fps_ready = ui_render_fps_ready
            && ui_input_ready
            && wayclick_fps_ready
            && ui_runtime_ready
            && dvm_relay_fps_ready
            && dvm_runtime_ready;
        let dvm_display_ready = !options.gui_dvm_surfaces
            || (dvm_display_provider_ready(&rustos_log)
                && dvm_display_relay_ready(&dvm_log, options.physical_gpu_bdf.is_some()));
        let physical_gpu_frames_ready = options
            .physical_gpu_bdf
            .as_ref()
            .is_none_or(|_| dvm_physical_frames_ready(&dvm_log));
        let dvm_network = if options.dvm_network_shmem {
            let shared_network = layout
                .dvm_network_shmem
                .as_deref()
                .context("network mode lost its shared DVM network aperture")?;
            Some(dvm_network_counters(shared_network)?)
        } else {
            None
        };
        let dvm_network_ready = dvm_network.is_none_or(|state| state.dvm_ready);
        let dvm_network_traffic_ready = !options.exercise_network
            || (dvm_network.is_some_and(DvmNetworkCounters::round_trip_observed)
                && rustos_log.contains(NETPROBE_QEMU_REACHABLE_MARKER));
        if rustos_ready
            && dvm_ready
            && dvm_gpu_ready
            && ui_fps_ready
            && dvm_display_ready
            && physical_gpu_frames_ready
            && dvm_network_ready
            && dvm_network_traffic_ready
            && let Some(control_ready) = control_ready
        {
            return Ok(control_ready);
        }
        if Instant::now() >= deadline {
            let input = dvm_input_counters(&layout.dvm_input_ring)?;
            let wayclick_observed = wayclick_profile_observation(&rustos_log);
            let missing_rustos = options
                .expected_markers
                .iter()
                .filter(|marker| !rustos_log.contains(marker.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            let missing_dvm = options
                .expected_dvm_markers
                .iter()
                .filter(|marker| !dvm_log.contains(marker.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            let reason = format!(
                "KVM parallel boot did not reach readiness within {:?}",
                options.timeout
            );
            let evidence = write_kvm_failure_summary(
                layout,
                &reason,
                boot_started.elapsed(),
                &rustos_log,
                &dvm_log,
                &missing_rustos,
                &missing_dvm,
            )?;
            bail!(
                "{reason}; RustOS missing={:?}; Linux-DVM missing={:?}; dvm-gpu-ready={}; ui-fps-ready={} (render={} input={} wayclick={} rustos-runtime={} dvm-relay={} dvm-runtime={}); wayclick-observed={:?}; dvm-display-ready={}; physical-gpu-frames-ready={}; dvm-network-ready={}; dvm-network-traffic-ready={}; host-input-relay-pending={}; input-ring={}/{} flags={:#x}; network-ring={:?}; evidence={}; inspect {}, {}, {}, and {}",
                missing_rustos,
                missing_dvm,
                dvm_gpu_ready,
                ui_fps_ready,
                ui_render_fps_ready,
                ui_input_ready,
                wayclick_fps_ready,
                ui_runtime_ready,
                dvm_relay_fps_ready,
                dvm_runtime_ready,
                wayclick_observed,
                dvm_display_ready,
                physical_gpu_frames_ready,
                dvm_network_ready,
                dvm_network_traffic_ready,
                control_ready.is_none(),
                input.producer,
                input.consumer,
                input.flags,
                dvm_network,
                evidence.display(),
                layout.debugcon_log.display(),
                layout.dvm_serial_log.display(),
                layout.rustos_stderr_log.display(),
                layout.dvm_stderr_log.display(),
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn runtime_log_len(path: &Path) -> Result<usize> {
    fs::metadata(path)
        .with_context(|| format!("stat runtime log {}", path.display()))?
        .len()
        .try_into()
        .context("runtime log length exceeds host address space")
}

fn runtime_log_suffix(path: &Path, offset: usize) -> Result<String> {
    let log = match fs::read_to_string(path) {
        Ok(log) => log,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("read runtime log {}", path.display()));
        }
    };
    let suffix = log
        .get(offset..)
        .context("runtime log was truncated during a recovery proof")?;
    Ok(suffix.to_owned())
}

fn archive_recovery_log(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let archive = path.with_extension("pre-recovery.log");
    if archive.exists() {
        fs::remove_file(&archive)
            .with_context(|| format!("remove stale recovery log {}", archive.display()))?;
    }
    fs::rename(path, &archive).with_context(|| {
        format!(
            "archive pre-recovery log {} as {}",
            path.display(),
            archive.display()
        )
    })
}

struct RecoveryHarness<'a> {
    qemu: &'a Path,
    config: &'a Config,
    artifacts: &'a DvmArtifacts,
    layout: &'a KvmLayout,
    options: &'a SmokeOptions,
    guest_display: GuestDisplay,
    host_render_node: Option<&'a Path>,
    input_doorbell: &'a IvshmemDoorbellServer,
    display_doorbell: Option<&'a IvshmemDoorbellServer>,
    block_doorbell: Option<&'a IvshmemDoorbellServer>,
    input_relay_gate: Arc<AtomicBool>,
}

impl RecoveryHarness<'_> {
    fn run(
        &self,
        rustos: &mut Child,
        dvm: &mut Child,
        mut probe: ProbeResult,
    ) -> Result<ProbeResult> {
        if self.options.recovery_probe.includes_rustos_reboot() {
            stop_guest(rustos);
            // Display, block, and network transports form one restart cohort:
            // their shared epochs are owned jointly by RustOS and the Linux
            // DVM. Keeping the old DVM alive would leave active headers that a
            // fresh kernel must correctly reject, but could never reset.
            stop_guest(dvm);
            thread::sleep(Duration::from_millis(150));
            if let Some(server) = self.display_doorbell {
                server
                    .wait_for_exact_peer_count(0, DVM_DISPLAY_FIRST_PEER_TIMEOUT)
                    .context("display peers did not retire before RustOS reboot")?;
            }
            if let Some(server) = self.block_doorbell {
                server
                    .wait_for_exact_peer_count(0, DVM_BLOCK_FIRST_PEER_TIMEOUT)
                    .context("block peers did not retire before RustOS reboot")?;
            }
            // LIFECYCLE: only the L0 owner may reinitialize a cross-domain
            // region, and only after every mapped guest in the restart cohort
            // has exited. Recreate the signed/headered initial state instead
            // of teaching either guest to accept a predecessor's active epoch.
            if let Some(path) = self.layout.gui_dvm_surfaces.as_deref() {
                create_gui_dvm_surfaces(path).context("reset GUI-DVM control region")?;
            }
            if let Some(path) = self.layout.gui_dvm_pixels.as_deref() {
                create_gui_dvm_surfaces(path).context("reset GUI-DVM pixel region")?;
            }
            if let Some(path) = self.layout.dvm_network_shmem.as_deref() {
                create_dvm_network_shmem(path).context("reset DVM network region")?;
            }
            if let (Some(aperture), Some(disk)) = (
                self.layout.dvm_block_aperture.as_deref(),
                self.layout.dvm_block_disk.as_deref(),
            ) {
                create_dvm_block_aperture(
                    aperture,
                    disk,
                    &self.config.storage_epoch_signing_key,
                )
                .context("reset signed DVM block region")?;
            }
            // QEMU's file chardev does not preserve append semantics uniformly
            // across supported distro builds. Give a fresh guest a fresh
            // capture and archive the predecessor instead of using a byte
            // offset that may point into a rewritten file.
            archive_recovery_log(&self.layout.debugcon_log)?;
            archive_recovery_log(&self.layout.rustos_serial_log)?;
            archive_recovery_log(&self.layout.dvm_serial_log)?;
            *rustos = spawn_rustos_guest(self.qemu, self.config, self.layout, false)?;
            self.input_doorbell
                .wait_for_exact_peer_count(1, DVM_INPUT_FIRST_PEER_TIMEOUT)
                .context("fresh RustOS guest did not reclaim the input peer")?;
            if let Some(server) = self.display_doorbell {
                server
                    .wait_for_exact_peer_count(1, DVM_DISPLAY_FIRST_PEER_TIMEOUT)
                    .context("fresh RustOS guest did not reclaim the display peer")?;
            }
            if let Some(server) = self.block_doorbell {
                server
                    .wait_for_exact_peer_count(1, DVM_BLOCK_FIRST_PEER_TIMEOUT)
                    .context("fresh RustOS guest did not reclaim the block peer")?;
            }
            let recovery_relay = start_dvm_input_relay(
                self.config,
                self.options.timeout,
                self.layout.guest_cid,
                self.layout.dvm_input_doorbell.clone(),
                self.layout.dvm_input_ring.clone(),
                self.layout.dvm_control_secret.clone(),
                Arc::clone(&self.input_relay_gate),
            )
            .context("start authenticated input relay for RustOS reboot cohort")?;
            self.input_doorbell
                .wait_for_exact_peer_count(2, DVM_INPUT_FIRST_PEER_TIMEOUT)
                .context("RustOS reboot input relay did not reclaim the producer peer")?;
            *dvm = spawn_dvm_guest(
                self.qemu,
                self.artifacts,
                self.layout,
                self.options,
                self.guest_display,
                self.host_render_node,
                false,
            )?;
            if let Some(server) = self.display_doorbell {
                server
                    .wait_for_exact_peer_count(2, DVM_DISPLAY_FIRST_PEER_TIMEOUT)
                    .context("restarted DVM did not reclaim the display peer")?;
            }
            if let Some(server) = self.block_doorbell {
                server
                    .wait_for_exact_peer_count(2, DVM_BLOCK_FIRST_PEER_TIMEOUT)
                    .context("restarted DVM did not reclaim the block peer")?;
            }
            probe = wait_for_rustos_reboot_recovery(
                rustos,
                dvm,
                self.layout,
                self.options,
                0,
                0,
                &recovery_relay,
            )?;
        }
        if self.options.recovery_probe.includes_dvm_restart() {
            let rustos_offset = runtime_log_len(&self.layout.debugcon_log)?;
            stop_guest(dvm);
            thread::sleep(Duration::from_millis(100));
            if let Some(server) = self.display_doorbell {
                server
                    .wait_for_exact_peer_count(1, DVM_DISPLAY_FIRST_PEER_TIMEOUT)
                    .context("display DVM peer did not retire before restart")?;
            }
            if let Some(server) = self.block_doorbell {
                server
                    .wait_for_exact_peer_count(1, DVM_BLOCK_FIRST_PEER_TIMEOUT)
                    .context("block DVM peer did not retire before restart")?;
            }
            if let (Some(aperture), Some(disk)) = (
                self.layout.dvm_block_aperture.as_deref(),
                self.layout.dvm_block_disk.as_deref(),
            ) {
                rotate_dvm_block_epoch(
                    aperture,
                    disk,
                    &self.config.storage_epoch_signing_key,
                )
                .context("publish signed successor DVM block epoch")?;
            }
            archive_recovery_log(&self.layout.dvm_serial_log)?;
            let recovery_relay = start_dvm_input_relay(
                self.config,
                self.options.timeout,
                self.layout.guest_cid,
                self.layout.dvm_input_doorbell.clone(),
                self.layout.dvm_input_ring.clone(),
                self.layout.dvm_control_secret.clone(),
                Arc::clone(&self.input_relay_gate),
            )
            .context("start authenticated input relay for DVM restart")?;
            *dvm = spawn_dvm_guest(
                self.qemu,
                self.artifacts,
                self.layout,
                self.options,
                self.guest_display,
                self.host_render_node,
                false,
            )?;
            probe = wait_for_dvm_restart_recovery(
                rustos,
                dvm,
                self.layout,
                self.options,
                rustos_offset,
                0,
                &recovery_relay,
            )?;
        }
        Ok(probe)
    }
}

fn wait_for_rustos_reboot_recovery(
    rustos: &mut Child,
    dvm: &mut Child,
    layout: &KvmLayout,
    options: &SmokeOptions,
    rustos_offset: usize,
    dvm_offset: usize,
    control_relay: &Receiver<Result<ProbeResult>>,
) -> Result<ProbeResult> {
    let deadline = Instant::now() + options.timeout;
    let mut control_ready = None;
    loop {
        check_guest_running(rustos, "fresh RustOS reboot", &layout.rustos_stderr_log)?;
        check_guest_running(dvm, "Linux DVM during RustOS reboot", &layout.dvm_stderr_log)?;
        let rustos_log = runtime_log_suffix(&layout.debugcon_log, rustos_offset)?;
        let dvm_log = runtime_log_suffix(&layout.dvm_serial_log, dvm_offset)?;
        if runtime_stall_or_crash_observed(&rustos_log)
            || runtime_stall_or_crash_observed(&dvm_log)
        {
            bail!("RustOS reboot recovery observed a watchdog, stall, crash, or relay stop");
        }
        match control_relay.try_recv() {
            Ok(Ok(probe)) => control_ready = Some(probe),
            Ok(Err(error)) => return Err(error).context("RustOS reboot cohort input relay"),
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) if control_ready.is_none() => {
                bail!("RustOS reboot cohort relay disconnected before authentication")
            }
            Err(mpsc::TryRecvError::Disconnected) => {}
        }
        let base_ready = [
            RUSTOS_REBOOT_ENTRY_MARKER,
            RUSTOS_BOOT_MARKER,
            RUSTOS_INIT_IDENTITY_MARKER,
            RUSTOS_POST_INIT_PROVENANCE_MARKER,
        ]
        .iter()
        .all(|marker| rustos_log.contains(marker));
        // A fresh RustOS process has no predecessor lease to revoke or
        // "rebind". The uiserver active marker is emitted only after the new
        // kernel admits the DVM prime completion and publishes GPU readiness;
        // require the explicit offline/rebound pair only for an in-place DVM
        // restart below.
        let display_ready =
            !options.gui_dvm_surfaces || rustos_log.contains(RUSTOS_GPU_ACTIVE_MARKER);
        let storage_ready = !options.dvm_block_shmem
            || (rustos_log.contains(RUSTOS_DVM_BLOCK_MARKER)
                && rustos_log.contains(RUSTOS_DVM_BLOCK_FIRST_COMPLETION_MARKER)
                && rustos_log.contains(RUSTOS_DVM_BLOCK_E2E_MARKER));
        let desktop_ready = !options.gui_dvm_surfaces
            || !options.dvm_block_shmem
            || rustos_log.contains(WAYCLICK_FIRST_FRAME_MARKER);
        let dvm_ready = options
            .expected_dvm_markers
            .iter()
            .all(|marker| dvm_log.contains(marker));
        if base_ready
            && display_ready
            && storage_ready
            && desktop_ready
            && dvm_ready
            && let Some(probe) = control_ready.as_ref()
        {
            println!("xtask: RustOS fresh-process reboot reached a full readiness epoch");
            return Ok(probe.clone());
        }
        if Instant::now() >= deadline {
            bail!(
                "RustOS fresh-process reboot did not reach full readiness within {:?}: \
                 base_ready={} display_ready={} storage_ready={} desktop_ready={} \
                 dvm_ready={} control_ready={} suffix_bytes={} offset={}",
                options.timeout,
                base_ready,
                display_ready,
                storage_ready,
                desktop_ready,
                dvm_ready,
                control_ready.is_some(),
                rustos_log.len(),
                rustos_offset,
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_dvm_restart_recovery(
    rustos: &mut Child,
    dvm: &mut Child,
    layout: &KvmLayout,
    options: &SmokeOptions,
    rustos_offset: usize,
    dvm_offset: usize,
    control_relay: &Receiver<Result<ProbeResult>>,
) -> Result<ProbeResult> {
    let deadline = Instant::now() + options.timeout;
    let mut control_ready = None;
    loop {
        check_guest_running(rustos, "RustOS during DVM restart", &layout.rustos_stderr_log)?;
        check_guest_running(dvm, "restarted Linux DVM", &layout.dvm_stderr_log)?;
        let rustos_log = runtime_log_suffix(&layout.debugcon_log, rustos_offset)?;
        let dvm_log = runtime_log_suffix(&layout.dvm_serial_log, dvm_offset)?;
        if runtime_stall_or_crash_observed(&rustos_log)
            || runtime_stall_or_crash_observed(&dvm_log)
        {
            bail!("DVM restart recovery observed a watchdog, stall, crash, or relay stop");
        }
        match control_relay.try_recv() {
            Ok(Ok(probe)) => control_ready = Some(probe),
            Ok(Err(error)) => return Err(error).context("restarted DVM input relay"),
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) if control_ready.is_none() => {
                bail!("restarted DVM input relay disconnected before authentication")
            }
            Err(mpsc::TryRecvError::Disconnected) => {}
        }
        let dvm_ready = options
            .expected_dvm_markers
            .iter()
            .all(|marker| dvm_log.contains(marker));
        let display_ready = !options.gui_dvm_surfaces
            || (rustos_log.contains(GUI_DVM_OFFLINE_MARKER)
                && rustos_log.contains(GUI_DVM_REBOUND_MARKER));
        let storage_ready = !options.dvm_block_shmem
            || rustos_log.contains("dvm-block: signed transport epoch rebound generation=");
        if dvm_ready
            && display_ready
            && storage_ready
            && let Some(probe) = control_ready
        {
            println!("xtask: Linux DVM abrupt-exit recovery reached a new authenticated epoch");
            return Ok(probe);
        }
        if Instant::now() >= deadline {
            bail!(
                "Linux DVM restart recovery did not reach a new authenticated epoch within {:?}",
                options.timeout
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}
