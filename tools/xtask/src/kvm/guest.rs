// SPDX-License-Identifier: MIT

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
            "2",
        ])
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
            "file,id=debugcon,path={},append=off",
            layout.debugcon_log.display()
        ))
        .arg("-device")
        .arg("isa-debugcon,iobase=0xe9,chardev=debugcon")
        .arg("-chardev")
        .arg(format!(
            "file,id=serial,path={},append=off",
            layout.rustos_serial_log.display()
        ))
        .args(["-serial", "chardev:serial"]);
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
    rustos_command
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(
            &layout.rustos_stderr_log,
        )?));
    let rustos = rustos_command
        .spawn()
        .context("failed to start RustOS QEMU/KVM guest")?;

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
            "file,id=serial,path={},append=off",
            layout.dvm_serial_log.display()
        ))
        .args(["-serial", "chardev:serial"])
        .arg("-device")
        .arg(format!("vhost-vsock-pci,guest-cid={DVM_GUEST_CID}"))
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
    dvm_command
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&layout.dvm_stderr_log)?));
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
    let dvm = match dvm_command.spawn() {
        Ok(dvm) => dvm,
        Err(error) => {
            let mut rustos = rustos;
            stop_guest(&mut rustos);
            return Err(error).context("failed to start Linux DVM QEMU/KVM guest");
        }
    };
    Ok((rustos, dvm))
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
            && boot_started.elapsed() >= Duration::from_millis(BOOT_TO_UI_HARD_LIMIT_MS)
        {
            bail!(
                "RustOS interactive UI missed the {} ms boot acceptance limit; missing={RUSTOS_GPU_ACTIVE_MARKER:?}; inspect {} and {}",
                BOOT_TO_UI_HARD_LIMIT_MS,
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
                if control_ready.is_some() {
                    bail!("Linux DVM input relay failed after readiness: {error:#}");
                }
                bail!("Linux DVM input relay failed before readiness: {error:#}");
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
            bail!(
                "KVM parallel boot did not reach readiness within {:?}; RustOS missing={:?}; Linux-DVM missing={:?}; dvm-gpu-ready={}; ui-fps-ready={} (render={} input={} wayclick={} rustos-runtime={} dvm-relay={} dvm-runtime={}); wayclick-observed={:?}; dvm-display-ready={}; physical-gpu-frames-ready={}; dvm-network-ready={}; dvm-network-traffic-ready={}; host-input-relay-pending={}; input-ring={}/{} flags={:#x}; network-ring={:?}; inspect {}, {}, {}, and {}",
                options.timeout,
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
                layout.debugcon_log.display(),
                layout.dvm_serial_log.display(),
                layout.rustos_stderr_log.display(),
                layout.dvm_stderr_log.display(),
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}
