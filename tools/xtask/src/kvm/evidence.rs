// SPDX-License-Identifier: MIT

fn required_dvm_gpu_ready(
    options: &SmokeOptions,
    log: &str,
    expected: GpuEvidenceExpectation,
) -> bool {
    options.storage_only || dvm_gpu_compositor_ready(log, expected)
}

fn dvm_gpu_compositor_ready(log: &str, expected: GpuEvidenceExpectation) -> bool {
    let failure_after_ready = [
        "rustos-dvm-gpu: context lost",
        "rustos-dvm-gpu: executor unavailable",
        "rustos-dvm-gpu: pipeline prime failed",
        "rustos-dvm-gpu: proof failed",
        "rustos-dvm-gpu: evidence publish failed",
        "rustos-dvm-gpu: contract negative selftest failed",
    ];
    let mut ready = false;
    let mut health_sequence = 0_u64;
    for line in log.lines() {
        if failure_after_ready
            .iter()
            .any(|marker| line.contains(marker))
        {
            ready = false;
            health_sequence = 0;
            continue;
        }
        if let Some((_, fields)) = line.split_once("rustos-dvm-gpu: health ") {
            let sequence = log_u64(fields, "sequence");
            let completion = log_u64(fields, "completion_us");
            if sequence.is_some_and(|value| value == health_sequence + 1)
                && completion.is_some_and(|value| value > 0 && value <= 16_667)
                && fields
                    .split_whitespace()
                    .any(|field| field == "acquire-fence=1")
            {
                health_sequence += 1;
            } else {
                ready = false;
                health_sequence = 0;
            }
            continue;
        }
        let Some((_, fields)) = line.split_once(DVM_GPU_COMPOSITOR_MARKER) else {
            continue;
        };
        let frames = log_u64(fields, "frames");
        let prime = log_u64(fields, "prime_us");
        let fps = log_u64(fields, "fps_milli");
        let average = log_u64(fields, "avg_us");
        let maximum = log_u64(fields, "max_us");
        let wall_maximum = log_u64(fields, "wall_max_us");
        let frame_hash_a = fields
            .split_whitespace()
            .find_map(|field| field.strip_prefix("frame_hash_a="))
            .filter(|value| value.len() == 16 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .and_then(|value| u64::from_str_radix(value, 16).ok());
        let frame_hash_b = fields
            .split_whitespace()
            .find_map(|field| field.strip_prefix("frame_hash_b="))
            .filter(|value| value.len() == 16 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .and_then(|value| u64::from_str_radix(value, 16).ok());
        ready = log_text_field_is(fields, "driver", expected.drm_driver)
            && log_text_field_is(fields, "backend-class", expected.backend_class)
            && log_text_field_is(fields, "certification", "registered")
            && fields.split_whitespace().any(|field| {
                field
                    .strip_prefix("renderer=")
                    .is_some_and(|value| !value.is_empty())
            })
            && (expected.backend_class != "virtual-staged"
                || fields
                    .split_whitespace()
                    .find_map(|field| field.strip_prefix("renderer="))
                    .is_some_and(|renderer| renderer.to_ascii_lowercase().contains("virgl")))
            && fields.split_whitespace().any(|field| field == "commands=3")
            && fields
                .split_whitespace()
                .any(|field| field == "gpu-fence=1")
            && fields
                .split_whitespace()
                .any(|field| field == "acquire-fence=1")
            && fields.split_whitespace().any(|field| field == "negative=5")
            && fields.split_whitespace().any(|field| field == "software=0")
            && fields
                .split_whitespace()
                .any(|field| field == "scheduler=rr")
            && fields.split_whitespace().any(|field| field == "priority=8")
            && fields
                .split_whitespace()
                .any(|field| field == "rttime-soft-us=50000")
            && fields
                .split_whitespace()
                .any(|field| field == "rttime-hard-us=100000")
            && fields
                .split_whitespace()
                .any(|field| field == "rttime-hard-action=terminate")
            && fields
                .split_whitespace()
                .any(|field| field == "scheduler-restored=normal")
            && fields
                .split_whitespace()
                .any(|field| field == "performance-target=1")
            && fields
                .split_whitespace()
                .any(|field| field == "scope-public-abi=0")
            && fields
                .split_whitespace()
                .any(|field| field == "scope-ui-connected=0")
            && fields
                .split_whitespace()
                .any(|field| field == "scope-scanout=0")
            && frames.is_some_and(|value| value >= 120)
            && prime.is_some_and(|value| value > 0 && value <= DVM_GPU_PIPELINE_PRIME_TIMEOUT_US)
            && fps.is_some_and(|value| value >= 60_000)
            && maximum.is_some_and(|value| value <= 16_667)
            && wall_maximum.is_some_and(|value| value > 0 && value <= 16_667)
            && average.zip(maximum).is_some_and(|(avg, max)| avg <= max)
            && frame_hash_a
                .zip(frame_hash_b)
                .is_some_and(|(left, right)| left != 0 && right != 0 && left != right)
            && fields
                .split_whitespace()
                .any(|field| field == "hash-stable=1")
            && fields
                .split_whitespace()
                .any(|field| field == "hash-dynamic=1");
        health_sequence = 0;
    }
    ready && health_sequence >= DVM_GPU_HEALTH_SAMPLES
}

fn dvm_display_failure(log: &str, physical_gpu: bool) -> Option<String> {
    if let Some(line) = log
        .lines()
        .find(|line| line.contains("rustos-dvm-display: gpu-compositor offline"))
    {
        return Some(format!(
            "Linux DVM GPU compositor went offline during readiness detail={}",
            line.trim()
        ));
    }
    if let Some(line) = log
        .lines()
        .rev()
        .find(|line| line.contains("rustos-dvm-display: GPU KMS setup unavailable stage="))
    {
        return Some(line.trim().to_owned());
    }
    for marker in [
        "rustos-dvm-gpu: pipeline prime evidence unavailable",
        "rustos-dvm-gpu: evidence publish failed",
    ] {
        if let Some(line) = log.lines().find(|line| line.contains(marker)) {
            return Some(format!(
                "Linux DVM GPU evidence publication failed detail={}",
                line.trim()
            ));
        }
    }
    if physical_gpu && log.contains("PSP create ring failed") {
        return Some(
            "physical GPU kernel probe failed stage=device-security-processor; the assigned device did not enter a reusable post-reset state"
                .to_owned(),
        );
    }
    if physical_gpu {
        for marker in [
            "Fatal error during GPU init",
            "probe with driver ",
            "rustos-dvm-gpu: executor unavailable",
        ] {
            if let Some(line) = log.lines().find(|line| line.contains(marker)) {
                return Some(format!(
                    "physical GPU kernel probe failed stage=driver-init detail={}",
                    line.trim()
                ));
            }
        }
    }
    None
}

fn dvm_physical_frames_ready(log: &str) -> bool {
    let mut frame_count = 0_usize;
    let mut last_sequence = None;
    let mut last_submit = None;
    for line in log.lines() {
        let Some((_, fields)) = line.split_once("rustos-dvm-display: gpu-frame ") else {
            continue;
        };
        let sequence = log_u64(fields, "sequence");
        let submit = log_u64(fields, "submit");
        let output = log_u64(fields, "output");
        let render_us = log_u64(fields, "render_us");
        let contract_ok = fields
            .split_whitespace()
            .any(|field| field == "source-path=dmabuf")
            && fields
                .split_whitespace()
                .any(|field| field == "zero-copy=1")
            && fields
                .split_whitespace()
                .any(|field| field == "gpu-fence=1")
            && fields
                .split_whitespace()
                .any(|field| field == "present-fence=1");
        let Some((sequence, submit, output, render_us)) =
            sequence.zip(submit).zip(output).zip(render_us).map(
                |(((sequence, submit), output), render_us)| (sequence, submit, output, render_us),
            )
        else {
            return false;
        };
        if !contract_ok
            || sequence == 0
            || submit == 0
            || output >= 3
            || render_us == 0
            || render_us > 16_667
            || last_sequence.is_some_and(|prior| sequence != prior + 1)
            || last_submit.is_some_and(|prior| submit != prior + 1)
        {
            return false;
        }
        last_sequence = Some(sequence);
        last_submit = Some(submit);
        frame_count += 1;
    }
    frame_count >= PHYSICAL_GPU_SMOKE_MIN_FRAMES
}

/// The kernel's bootstrap trace intentionally does not promise runtime
/// debugcon delivery. The userspace display-info ABI is the authoritative
/// observation: the runner's fixed ivshmem header must emerge unchanged as the
/// active primary display provider.
fn dvm_display_provider_ready(log: &str) -> bool {
    let expected_stride = DvmGuiSurfacePoolHeader::new(
        DVM_DISPLAY_REGION_BYTES,
        DVM_DISPLAY_WIDTH,
        DVM_DISPLAY_HEIGHT,
    )
    .stride_bytes;
    log.lines().any(|line| {
        let Some((_, fields)) = line.split_once("uiserver: display_get_info ") else {
            return false;
        };
        uiserver_display_field_is(fields, "width", DVM_DISPLAY_WIDTH)
            && uiserver_display_field_is(fields, "height", DVM_DISPLAY_HEIGHT)
            && uiserver_display_field_is(fields, "stride", expected_stride)
            && uiserver_display_field_is(fields, "bpp", 4)
            && uiserver_display_field_is(fields, "fmt", 1)
            // A DVM scanout is still the active primary provider. Requiring
            // both provenance bits prevents the smoke from accepting either a
            // generic primary framebuffer or a non-primary DVM aperture.
            && fields
                .split_whitespace()
                .any(|field| field == "flags=0xe")
    })
}

fn dvm_display_relay_ready(log: &str, physical_amdgpu: bool) -> bool {
    let expected_stride = DvmGuiSurfacePoolHeader::new(
        DVM_DISPLAY_REGION_BYTES,
        DVM_DISPLAY_WIDTH,
        DVM_DISPLAY_HEIGHT,
    )
    .stride_bytes;
    let active = log.lines().any(|line| {
        let has_interrupt = line
            .split_whitespace()
            .find_map(|field| field.strip_prefix("irq_count="))
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|count| count > 0);
        let transport = if physical_amdgpu {
            line.contains("source-path=dmabuf")
                && line.contains("zero-copy=1")
                && line.contains("staged-damage-copy=0")
        } else {
            line.contains("source-path=staged-copy")
                && line.contains("zero-copy=0")
                && line.contains("staged-damage-copy=1")
        };
        line.contains("rustos-dvm-display: active")
            && line.contains(&format!("width={DVM_DISPLAY_WIDTH}"))
            && line.contains(&format!("height={DVM_DISPLAY_HEIGHT}"))
            && line.contains(&format!("stride={expected_stride}"))
            && line.contains("event=ivshmem-msix-uio")
            && has_interrupt
            && line.contains("format=BGRA8888")
            && transport
            && line.contains("gpu-composition=1")
            && line.contains("explicit-fence=1")
            && line.contains("scanout_buffers=3")
            && line.contains("cpu-final-compose=0")
    });
    active
        && log.lines().any(|line| {
            line.contains(
                "rustos-dvm-display: host confirmed peer readiness event=ivshmem-msix-uio",
            )
        })
        && log.lines().any(|line| {
            line.contains(
                "rustos-dvm-display: scheduler admitted policy=rr priority=9 rttime_soft_us=50000 rttime_hard_us=100000 rttime_hard_action=terminate",
            )
        })
}

fn read_runtime_log_if_present(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(log) => Ok(log),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("read runtime log {}", path.display())),
    }
}

#[derive(Debug, Serialize)]
struct KvmFailureLog {
    path: String,
    bytes: usize,
    sha256: String,
    latest_guest_ts_us: Option<u64>,
}

#[derive(Debug, Serialize)]
struct KvmCausalEvent {
    guest: &'static str,
    source_line: usize,
    guest_ts_us: Option<u64>,
    record: String,
}

#[derive(Debug, Serialize)]
struct KvmFailureSummary<'a> {
    schema: &'static str,
    status: &'static str,
    reason: &'a str,
    boot_elapsed_ms: u64,
    missing_rustos_markers: &'a [String],
    missing_dvm_markers: &'a [String],
    rustos_log: KvmFailureLog,
    dvm_log: KvmFailureLog,
    causal_tail: Vec<KvmCausalEvent>,
}

fn structured_guest_timestamp_us(line: &str) -> Option<u64> {
    line.split_ascii_whitespace().find_map(|field| {
        field
            .strip_prefix("ts_us=")
            .and_then(|value| value.parse().ok())
    })
}

fn latest_guest_timestamp_us(log: &str) -> Option<u64> {
    log.lines().filter_map(structured_guest_timestamp_us).max()
}

fn guest_deadline_reached(log: &str, deadline_ms: u64) -> bool {
    latest_guest_timestamp_us(log)
        .is_some_and(|timestamp_us| timestamp_us >= deadline_ms.saturating_mul(1_000))
}

fn runtime_log_sha256(log: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(log.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn causal_record(line: &str) -> bool {
    const MARKERS: &[&str] = &[
        "milestone: name=",
        "loaderd: executable snapshot",
        "loaderd: immutable executable snapshot",
        "vfsd: executable snapshot",
        "vfsd: volume-read",
        "storaged:",
        "dvm-block:",
        "rootd:",
        "uiserver: gpu",
        "wayclick:",
        "inputd: DVM",
        "rustos-dvm-",
        "watchdog",
        "scheduler long ready wait:",
        "memory allocation of ",
        "fatal service endpoint not ready",
        "ipc-service-exit-revoke",
    ];
    MARKERS.iter().any(|marker| line.contains(marker))
}

fn causal_tail(rustos_log: &str, dvm_log: &str) -> Vec<KvmCausalEvent> {
    const MAX_EVENTS: usize = 64;
    const MAX_RECORD_BYTES: usize = 512;

    let mut events = [("rustos", rustos_log), ("linux-dvm", dvm_log)]
        .into_iter()
        .flat_map(|(guest, log)| {
            log.lines().enumerate().filter_map(move |(index, line)| {
                if !causal_record(line) {
                    return None;
                }
                let mut record = line.trim().to_owned();
                record.truncate(record.floor_char_boundary(MAX_RECORD_BYTES));
                Some(KvmCausalEvent {
                    guest,
                    source_line: index + 1,
                    guest_ts_us: structured_guest_timestamp_us(line),
                    record,
                })
            })
        })
        .collect::<Vec<_>>();
    events.sort_by_key(|event| {
        (
            event.guest_ts_us.unwrap_or(u64::MAX),
            event.guest,
            event.source_line,
        )
    });
    if events.len() > MAX_EVENTS {
        events.drain(..events.len() - MAX_EVENTS);
    }
    events
}

fn write_kvm_failure_summary(
    layout: &KvmLayout,
    reason: &str,
    boot_elapsed: Duration,
    rustos_log: &str,
    dvm_log: &str,
    missing_rustos_markers: &[String],
    missing_dvm_markers: &[String],
) -> Result<PathBuf> {
    let summary = KvmFailureSummary {
        schema: "rustos-kvm-failure-evidence-v1",
        status: "failed",
        reason,
        boot_elapsed_ms: boot_elapsed.as_millis().try_into().unwrap_or(u64::MAX),
        missing_rustos_markers,
        missing_dvm_markers,
        rustos_log: KvmFailureLog {
            path: layout.debugcon_log.display().to_string(),
            bytes: rustos_log.len(),
            sha256: runtime_log_sha256(rustos_log),
            latest_guest_ts_us: latest_guest_timestamp_us(rustos_log),
        },
        dvm_log: KvmFailureLog {
            path: layout.dvm_serial_log.display().to_string(),
            bytes: dvm_log.len(),
            sha256: runtime_log_sha256(dvm_log),
            latest_guest_ts_us: latest_guest_timestamp_us(dvm_log),
        },
        causal_tail: causal_tail(rustos_log, dvm_log),
    };
    let path = layout.run_dir.join("failure-summary.json");
    let temporary = layout.run_dir.join("failure-summary.json.tmp");
    let mut encoded = serde_json::to_vec_pretty(&summary)?;
    encoded.push(b'\n');
    fs::write(&temporary, encoded)
        .with_context(|| format!("write temporary KVM failure evidence {}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("publish KVM failure evidence {}", path.display()))?;
    Ok(path)
}

fn uiserver_idle_ticks_healthy(log: &str, required_ticks: usize) -> bool {
    let mut consecutive = 0_usize;
    for line in log.lines() {
        let Some((_, fields)) = line.split_once("uiserver: update tick ") else {
            continue;
        };
        let healthy = [
            "backlog=false",
            "input_drops=0",
            "input_slow=0",
            "input_errors=0",
        ]
        .into_iter()
        .all(|field| fields.split_whitespace().any(|observed| observed == field));
        if healthy {
            consecutive = consecutive.saturating_add(1);
            if consecutive >= required_ticks {
                return true;
            }
        } else {
            consecutive = 0;
        }
    }
    false
}

fn interactive_display_ready(layout: &KvmLayout, rustos_log: &str, dvm_log: &str) -> bool {
    let surface_ready = match (
        layout.gui_dvm_surfaces.as_deref(),
        layout.gui_dvm_pixels.as_deref(),
    ) {
        (Some(control), Some(pixels)) => verify_dvm_display_surface(control, pixels).is_ok(),
        _ => false,
    };
    let block_ready = rustos_log.contains(RUSTOS_DVM_BLOCK_MARKER)
        && rustos_log.contains(RUSTOS_DVM_BLOCK_E2E_MARKER)
        && dvm_log.contains(DVM_BLOCK_READY_MARKER)
        && verify_dvm_block_ready(layout).is_ok();
    dvm_display_provider_ready(rustos_log)
        && dvm_display_relay_ready(dvm_log, false)
        && surface_ready
        && block_ready
        && rustos_log.contains(WAYCLICK_FIRST_FRAME_MARKER)
}

fn validate_interactive_session(layout: &KvmLayout, pointer_observed: bool) -> Result<()> {
    let rustos_log = read_runtime_log_if_present(&layout.debugcon_log)?;
    let dvm_log = read_runtime_log_if_present(&layout.dvm_serial_log)?;
    if runtime_stall_or_crash_observed(&rustos_log) || runtime_stall_or_crash_observed(&dvm_log) {
        bail!(
            "interactive KVM DVM acceptance found a watchdog, stall, crash, or relay stop; inspect {} and {}",
            layout.debugcon_log.display(),
            layout.dvm_serial_log.display(),
        );
    }
    if !dvm_display_provider_ready(&rustos_log) || !dvm_display_relay_ready(&dvm_log, false) {
        bail!(
            "interactive KVM DVM acceptance lacks the active atomic GUI-DVM display contract; inspect {} and {}",
            layout.debugcon_log.display(),
            layout.dvm_serial_log.display(),
        );
    }
    if !rustos_log.contains(RUSTOS_DVM_BLOCK_MARKER)
        || !rustos_log.contains(RUSTOS_DVM_BLOCK_E2E_MARKER)
        || !dvm_log.contains(DVM_BLOCK_READY_MARKER)
    {
        bail!("interactive KVM DVM acceptance lacks the exact block transport readiness contract");
    }
    if !rustos_log.contains(WAYCLICK_FIRST_FRAME_MARKER) {
        bail!("interactive KVM DVM acceptance lacks a presented WayClick first frame");
    }
    verify_dvm_block_ready(layout)?;
    verify_dvm_display_surface(
        layout
            .gui_dvm_surfaces
            .as_deref()
            .context("interactive session lost GUI-DVM control backing")?,
        layout
            .gui_dvm_pixels
            .as_deref()
            .context("interactive session lost GUI-DVM pixel backing")?,
    )?;
    if !uiserver_idle_ticks_healthy(&rustos_log, INTERACTIVE_IDLE_TICKS) {
        bail!(
            "interactive KVM DVM acceptance lacks {} consecutive healthy idle update ticks; inspect {}",
            INTERACTIVE_IDLE_TICKS,
            layout.debugcon_log.display(),
        );
    }
    if !pointer_observed || !rustos_log.contains(DVM_POINTER_INGRESS_MARKER) {
        bail!(
            "interactive KVM DVM acceptance did not observe a real absolute-pointer event; move the host pointer over the DVM window before closing it"
        );
    }
    println!(
        "xtask: interactive KVM DVM acceptance passed (atomic display, non-black source frame, healthy idle ticks, real pointer ingress)"
    );
    Ok(())
}

fn dvm_display_relay_meets_fps(log: &str, minimum_fps: u32, required_windows: usize) -> bool {
    let required_milli = u64::from(minimum_fps).saturating_mul(1_000);
    let mut consecutive_samples = 0_usize;
    for line in log.lines() {
        let Some((_, fields)) = line.split_once("rustos-dvm-display: stats ") else {
            continue;
        };
        let pageflip_completions = fields.split_whitespace().find_map(|field| {
            field
                .strip_prefix("pageflip_completions=")
                .and_then(|value| value.parse::<u64>().ok())
        });
        let frame_hz_milli = fields.split_whitespace().find_map(|field| {
            field
                .strip_prefix("frame_hz_milli=")
                .and_then(|value| value.parse::<u64>().ok())
        });
        let relay_cpu_copy_us = log_u64(fields, "relay_cpu_copy_us_avg");
        let atomic_commit_us = log_u64(fields, "atomic_commit_us_avg");
        let gpu_render_us_avg = log_u64(fields, "gpu_render_us_avg");
        let gpu_render_us_max = log_u64(fields, "gpu_render_us_max");
        let gpu_fence_completions = log_u64(fields, "gpu_fence_completions");
        let present_fence_completions = log_u64(fields, "present_fence_completions");
        let Some((
            pageflip_completions,
            frame_hz_milli,
            relay_cpu_copy_us,
            atomic_commit_us,
            gpu_render_us_avg,
            gpu_render_us_max,
            gpu_fence_completions,
            present_fence_completions,
        )) = pageflip_completions
            .zip(frame_hz_milli)
            .zip(relay_cpu_copy_us.zip(atomic_commit_us))
            .zip(gpu_render_us_avg.zip(gpu_render_us_max))
            .zip(gpu_fence_completions.zip(present_fence_completions))
            .map(
                |(
                    (((submissions, hz), (copy, commit)), (gpu_avg, gpu_max)),
                    (gpu_fences, present_fences),
                )| {
                    (
                        submissions,
                        hz,
                        copy,
                        commit,
                        gpu_avg,
                        gpu_max,
                        gpu_fences,
                        present_fences,
                    )
                },
            )
        else {
            continue;
        };
        if pageflip_completions == 0
            || frame_hz_milli < required_milli
            || relay_cpu_copy_us != 0
            || atomic_commit_us > MAX_DVM_DISPLAY_RELAY_US
            || gpu_render_us_avg == 0
            || gpu_render_us_avg > MAX_DVM_DISPLAY_RELAY_US
            || gpu_render_us_max == 0
            || gpu_render_us_max > MAX_DVM_GPU_RENDER_US
            || gpu_fence_completions != pageflip_completions
            || present_fence_completions != pageflip_completions
        {
            consecutive_samples = 0;
            continue;
        }
        consecutive_samples = consecutive_samples.saturating_add(1);
        if consecutive_samples >= required_windows {
            return true;
        }
    }
    false
}

fn log_u64(fields: &str, name: &str) -> Option<u64> {
    fields.split_whitespace().find_map(|field| {
        field
            .strip_prefix(name)
            .and_then(|value| value.strip_prefix('='))
            .and_then(|value| value.parse::<u64>().ok())
    })
}

fn log_text_field_is(fields: &str, name: &str, expected: &str) -> bool {
    fields.split_whitespace().any(|field| {
        field
            .strip_prefix(name)
            .and_then(|value| value.strip_prefix('='))
            == Some(expected)
    })
}

fn log_point(fields: &str, name: &str) -> Option<(u64, u64)> {
    let value = fields
        .split_whitespace()
        .find_map(|field| field.strip_prefix(name)?.strip_prefix('='))?;
    let (x, y) = value.split_once(',')?;
    Some((x.parse().ok()?, y.parse().ok()?))
}

#[derive(Clone, Copy, Debug)]
struct UiProfileInputWindow {
    elapsed_ms: u64,
    frame_hz_milli: u64,
    input_events: u64,
    backlog: u64,
    cursor_moves: u64,
    input_gap_ms: u64,
    input_last_age_ms: u64,
    input_drops: u64,
    input_slow: u64,
    input_errors: u64,
    cursor_mismatches: u64,
    cursor_x: u64,
    cursor_y: u64,
    presented_cursor_x: u64,
    presented_cursor_y: u64,
    background_thread_demotions: u64,
}

fn parse_ui_profile_input_window(line: &str) -> Option<UiProfileInputWindow> {
    if let Some((_, fields)) = line.split_once("uiserver profile: ") {
        let (cursor_x, cursor_y) = log_point(fields, "cursor")?;
        let (presented_cursor_x, presented_cursor_y) = log_point(fields, "presented_cursor")?;
        return Some(UiProfileInputWindow {
            elapsed_ms: log_u64(fields, "elapsed_ms")?.max(1),
            frame_hz_milli: log_u64(fields, "frame_hz_milli")?,
            input_events: log_u64(fields, "input_events")?,
            backlog: log_u64(fields, "backlog")?,
            cursor_moves: log_u64(fields, "cursor_moves")?,
            input_gap_ms: log_u64(fields, "input_gap_ms")?,
            input_last_age_ms: log_u64(fields, "input_last_age_ms")?,
            input_drops: log_u64(fields, "input_drops")?,
            input_slow: log_u64(fields, "input_slow")?,
            input_errors: log_u64(fields, "input_errors")?,
            cursor_mismatches: log_u64(fields, "cursor_mismatches")?,
            cursor_x,
            cursor_y,
            presented_cursor_x,
            presented_cursor_y,
            background_thread_demotions: log_u64(fields, "background_thread_demotions")?,
        });
    }
    // uiserver starts before the mutable DVM volume by design, so the private
    // KVM profile contract may not yet be visible. The production heartbeat is
    // an equivalent bounded one-second source: it carries elapsed time, frame
    // and input window counts, cursor synchronization, backlog, cumulative
    // error counters, and scheduler demotion evidence.
    let (_, fields) = line.split_once("uiserver: update tick ")?;
    let elapsed_ms = log_u64(fields, "elapsed_ms")?.max(1);
    let frames = log_u64(fields, "frames")?;
    let (cursor_x, cursor_y) = log_point(fields, "cursor")?;
    let (presented_cursor_x, presented_cursor_y) = log_point(fields, "presented_cursor")?;
    Some(UiProfileInputWindow {
        elapsed_ms,
        frame_hz_milli: frames.saturating_mul(1_000_000).saturating_div(elapsed_ms),
        input_events: log_u64(fields, "input_loop_events")?,
        backlog: u64::from(log_text_field_is(fields, "backlog", "true")),
        cursor_moves: log_u64(fields, "cursor_moves")?,
        input_gap_ms: log_u64(fields, "input_gap_ms")?,
        input_last_age_ms: log_u64(fields, "input_last_age_ms")?,
        input_drops: log_u64(fields, "input_drops_window")
            .or_else(|| log_u64(fields, "input_drops"))?,
        input_slow: log_u64(fields, "input_slow_window")
            .or_else(|| log_u64(fields, "input_slow"))?,
        input_errors: log_u64(fields, "input_errors_window")
            .or_else(|| log_u64(fields, "input_errors"))?,
        cursor_mismatches: u64::from(
            cursor_x != presented_cursor_x || cursor_y != presented_cursor_y,
        ),
        cursor_x,
        cursor_y,
        presented_cursor_x,
        presented_cursor_y,
        background_thread_demotions: log_u64(fields, "background_thread_demotions")?,
    })
}

fn uiserver_profile_input_pipeline_healthy(
    log: &str,
    required_windows: usize,
    minimum_fps: Option<u32>,
) -> bool {
    let required_frame_hz_milli = minimum_fps.map(|fps| u64::from(fps).saturating_mul(1_000));
    let per_window_input_floor = MIN_UI_FPS_INPUT_EVENTS.saturating_mul(4).saturating_div(5);
    let per_window_cursor_floor = MIN_UI_FPS_CURSOR_MOVES.saturating_mul(4).saturating_div(5);
    let mut windows = Vec::new();
    for window in log.lines().filter_map(parse_ui_profile_input_window) {
        if required_frame_hz_milli.is_some_and(|minimum| window.frame_hz_milli < minimum)
            || window.input_events < per_window_input_floor
            || window.cursor_moves < per_window_cursor_floor
            || window.backlog != 0
            || window.input_gap_ms > MAX_UI_INPUT_GAP_MS
            || window.input_last_age_ms > MAX_UI_INPUT_GAP_MS
            || window.input_drops != 0
            || window.input_slow != 0
            || window.input_errors != 0
            || window.cursor_mismatches != 0
            || window.background_thread_demotions == 0
            || window.cursor_x != window.presented_cursor_x
            || window.cursor_y != window.presented_cursor_y
        {
            windows.clear();
            continue;
        }
        windows.push(window);
        if windows.len() > required_windows {
            windows.remove(0);
        }
        if windows.len() == required_windows {
            let elapsed_ms = windows
                .iter()
                .map(|window| window.elapsed_ms)
                .sum::<u64>()
                .max(1);
            let input_hz_milli = windows
                .iter()
                .map(|window| window.input_events)
                .sum::<u64>()
                .saturating_mul(1_000_000)
                .saturating_div(elapsed_ms);
            let cursor_hz_milli = windows
                .iter()
                .map(|window| window.cursor_moves)
                .sum::<u64>()
                .saturating_mul(1_000_000)
                .saturating_div(elapsed_ms);
            let min_x = windows
                .iter()
                .map(|window| window.cursor_x)
                .min()
                .unwrap_or(0);
            let max_x = windows
                .iter()
                .map(|window| window.cursor_x)
                .max()
                .unwrap_or(0);
            let min_y = windows
                .iter()
                .map(|window| window.cursor_y)
                .min()
                .unwrap_or(0);
            let max_y = windows
                .iter()
                .map(|window| window.cursor_y)
                .max()
                .unwrap_or(0);
            if input_hz_milli >= MIN_UI_FPS_INPUT_EVENTS.saturating_mul(1_000)
                && cursor_hz_milli >= MIN_UI_FPS_CURSOR_MOVES.saturating_mul(1_000)
                && max_x.saturating_sub(min_x) >= MIN_UI_CURSOR_SPAN
                && max_y.saturating_sub(min_y) >= MIN_UI_CURSOR_SPAN
            {
                return true;
            }
        }
    }
    false
}

fn runtime_stall_or_crash_observed(log: &str) -> bool {
    const FAILURES: &[&str] = &[
        "uiserver watchdog panic:",
        "uiserver input watchdog panic:",
        "uiserver panic:",
        "scheduler long ready wait:",
        "scheduler stall:",
        "rustos-dvm-display: relay stopped",
        "[drm:virtio_gpu_dequeue_ctrl_func] *ERROR*",
        "panicked at ",
        "fatal runtime error",
        "memory allocation of ",
        "fatal service endpoint not ready",
        "BUG:",
    ];
    log.lines()
        .any(|line| FAILURES.iter().any(|failure| line.contains(failure)))
}

fn uiserver_display_field_is(fields: &str, name: &str, expected: u32) -> bool {
    fields.split_whitespace().any(|field| {
        field
            .split_once('=')
            .is_some_and(|(key, value)| key == name && value == expected.to_string())
    })
}

fn uiserver_profile_meets_fps(log: &str, minimum_fps: u32, required_windows: usize) -> bool {
    let required_milli = u64::from(minimum_fps).saturating_mul(1_000);
    // An idle desktop has no presents by design, so only a window that
    // actually processed the acceptance input stream is an FPS sample.
    let active_windows = log
        .lines()
        .filter_map(|line| {
            if let Some((_, fields)) = line.split_once("uiserver profile: ") {
                return Some((
                    log_u64(fields, "frame_hz_milli")?,
                    log_u64(fields, "input_events")?,
                ));
            }
            let window = parse_ui_profile_input_window(line)?;
            Some((window.frame_hz_milli, window.input_events))
        })
        .filter_map(|(frame_hz_milli, input_events)| {
            (input_events >= MIN_UI_FPS_INPUT_EVENTS).then_some(frame_hz_milli)
        });
    let mut count = 0_usize;
    for frame_hz_milli in active_windows {
        if frame_hz_milli < required_milli {
            count = 0;
            continue;
        }
        count = count.saturating_add(1);
        if count >= required_windows {
            return true;
        }
    }
    false
}

fn wayclick_profile_meets_fps(log: &str, minimum_fps: u32, required_windows: usize) -> bool {
    let required_milli = u64::from(minimum_fps).saturating_mul(1_000);
    // Per-second windows are not phase-aligned with compositor callbacks, so
    // requiring every independently cut window to round above the target can
    // reject a stream whose contiguous multi-second rate is above target with
    // no stall. Keep the stronger latency and balance rails per window, then
    // calculate the exact aggregate rate over the requested consecutive
    // windows. The 80% per-window floor prevents one burst from hiding a
    // throughput collapse even when no single gap crosses the 50 ms rail.
    let per_window_floor = required_milli.saturating_mul(4).saturating_div(5);
    let mut windows = Vec::new();
    for fields in log.lines().filter_map(|line| {
        line.split_once("wayclick profile: ")
            .map(|(_, fields)| fields)
    }) {
        let Some(elapsed_ms) = log_u64(fields, "elapsed_ms") else {
            windows.clear();
            continue;
        };
        let Some(commit_hz) = log_u64(fields, "commit_hz_milli") else {
            windows.clear();
            continue;
        };
        let Some(callback_hz) = log_u64(fields, "callback_hz_milli") else {
            windows.clear();
            continue;
        };
        let Some(commits) = log_u64(fields, "commits") else {
            windows.clear();
            continue;
        };
        let Some(callbacks) = log_u64(fields, "callbacks") else {
            windows.clear();
            continue;
        };
        let Some(releases) = log_u64(fields, "buffer_releases") else {
            windows.clear();
            continue;
        };
        let Some(max_gap_ms) = log_u64(fields, "max_callback_gap_ms") else {
            windows.clear();
            continue;
        };
        let balanced = commits.abs_diff(callbacks) <= 2 && callbacks.abs_diff(releases) <= 2;
        if commit_hz < per_window_floor
            || callback_hz < per_window_floor
            || commits == 0
            || callbacks == 0
            || releases == 0
            || elapsed_ms == 0
            || max_gap_ms > MAX_UI_INPUT_GAP_MS
            || !balanced
        {
            windows.clear();
            continue;
        }
        windows.push((elapsed_ms, commits, callbacks));
        if windows.len() > required_windows {
            windows.remove(0);
        }
        if windows.len() == required_windows {
            let elapsed_ms = windows
                .iter()
                .map(|(elapsed_ms, _, _)| *elapsed_ms)
                .sum::<u64>()
                .max(1);
            let commits = windows
                .iter()
                .map(|(_, commits, _)| *commits)
                .sum::<u64>();
            let callbacks = windows
                .iter()
                .map(|(_, _, callbacks)| *callbacks)
                .sum::<u64>();
            let commit_hz_milli = commits.saturating_mul(1_000_000).saturating_div(elapsed_ms);
            let callback_hz_milli = callbacks
                .saturating_mul(1_000_000)
                .saturating_div(elapsed_ms);
            if commit_hz_milli >= required_milli && callback_hz_milli >= required_milli {
                return true;
            }
        }
    }
    false
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WayclickProfileObservation {
    windows: usize,
    commit_hz_milli_min: u64,
    commit_hz_milli_max: u64,
    callback_hz_milli_min: u64,
    callback_hz_milli_max: u64,
    max_callback_gap_ms: u64,
    max_redraw_ms: u64,
}

fn wayclick_profile_observation(log: &str) -> Option<WayclickProfileObservation> {
    let mut observation = WayclickProfileObservation {
        windows: 0,
        commit_hz_milli_min: u64::MAX,
        commit_hz_milli_max: 0,
        callback_hz_milli_min: u64::MAX,
        callback_hz_milli_max: 0,
        max_callback_gap_ms: 0,
        max_redraw_ms: 0,
    };
    for fields in log.lines().filter_map(|line| {
        line.split_once("wayclick profile: ")
            .map(|(_, fields)| fields)
    }) {
        let Some(commit_hz) = log_u64(fields, "commit_hz_milli") else {
            continue;
        };
        let Some(callback_hz) = log_u64(fields, "callback_hz_milli") else {
            continue;
        };
        let Some(max_gap_ms) = log_u64(fields, "max_callback_gap_ms") else {
            continue;
        };
        observation.windows = observation.windows.saturating_add(1);
        observation.commit_hz_milli_min = observation.commit_hz_milli_min.min(commit_hz);
        observation.commit_hz_milli_max = observation.commit_hz_milli_max.max(commit_hz);
        observation.callback_hz_milli_min = observation.callback_hz_milli_min.min(callback_hz);
        observation.callback_hz_milli_max = observation.callback_hz_milli_max.max(callback_hz);
        observation.max_callback_gap_ms = observation.max_callback_gap_ms.max(max_gap_ms);
        observation.max_redraw_ms = observation
            .max_redraw_ms
            .max(log_u64(fields, "max_redraw_ms").unwrap_or(0));
    }
    (observation.windows != 0).then_some(observation)
}

fn validate_ui_fps_proof(layout: &KvmLayout, options: &SmokeOptions) -> Result<()> {
    let Some(minimum_fps) = options.min_ui_fps else {
        return Ok(());
    };
    let log = fs::read_to_string(&layout.debugcon_log)?;
    if !uiserver_profile_meets_fps(&log, minimum_fps, options.ui_proof_windows) {
        bail!(
            "KVM UI FPS proof failed after guest shutdown: require {} high-volume input windows at or above {} FPS; inspect {}",
            options.ui_proof_windows,
            minimum_fps,
            layout.debugcon_log.display(),
        );
    }
    if uiserver_has_interactive_slow_loop(&log) {
        bail!(
            "KVM UI FPS proof found an interactive slow uiserver loop; inspect {}",
            layout.debugcon_log.display(),
        );
    }
    if !uiserver_profile_input_pipeline_healthy(&log, options.ui_proof_windows, Some(minimum_fps)) {
        bail!(
            "KVM UI proof found no single consecutive window set satisfying both FPS and input loss/backlog/gap/cursor requirements; inspect {}",
            layout.debugcon_log.display(),
        );
    }
    if !wayclick_profile_meets_fps(&log, minimum_fps, options.ui_proof_windows) {
        bail!(
            "KVM WayClick FPS proof found no consecutive commit/frame-callback/release window set at or above {} FPS; observed={:?}; inspect {}",
            minimum_fps,
            wayclick_profile_observation(&log),
            layout.debugcon_log.display(),
        );
    }
    if runtime_stall_or_crash_observed(&log) {
        bail!(
            "KVM UI proof found a uiserver/scheduler watchdog, stall, or crash marker; inspect {}",
            layout.debugcon_log.display(),
        );
    }
    if options.gui_dvm_surfaces {
        let dvm_log = fs::read_to_string(&layout.dvm_serial_log)?;
        if !dvm_display_relay_meets_fps(&dvm_log, minimum_fps, options.ui_proof_windows) {
            bail!(
                "KVM UI FPS proof failed after guest shutdown: require {} external DVM atomic-page-flip relay samples at or above {} FPS; inspect {}",
                options.ui_proof_windows,
                minimum_fps,
                layout.dvm_serial_log.display(),
            );
        }
        if runtime_stall_or_crash_observed(&dvm_log) {
            bail!(
                "KVM UI proof found a DVM display relay crash marker; inspect {}",
                layout.dvm_serial_log.display(),
            );
        }
    }
    Ok(())
}

const UI_TOPOLOGY_TRANSITION_HARD_LIMIT_MS: u64 = 100;

fn uiserver_has_interactive_slow_loop(log: &str) -> bool {
    let mut wayclick_steady_window_started = false;
    log.lines().any(|line| {
        if line.contains("wayclick profile: ") {
            wayclick_steady_window_started = true;
            return false;
        }
        let Some((_, fields)) = line.split_once("uiserver: slow loop ") else {
            return false;
        };
        let interactive = uiserver_log_field_is_nonzero(fields, "console_windows")
            || uiserver_log_field_is_nonzero(fields, "wayland_windows");
        if !interactive {
            return false;
        }
        // Creating the initial console/Wayland topology is not a sustained
        // frame sample: it repacks retained textures exactly once before the
        // first client profile window. Keep that transition under its own
        // explicit 100 ms response bound. Once the first one-second WayClick
        // window starts, every >50 ms loop remains a hard FPS failure.
        wayclick_steady_window_started
            || log_u64(fields, "iter_ms")
                .is_none_or(|elapsed| elapsed > UI_TOPOLOGY_TRANSITION_HARD_LIMIT_MS)
    })
}

fn uiserver_log_field_is_nonzero(fields: &str, name: &str) -> bool {
    fields.split_whitespace().any(|field| {
        field
            .split_once('=')
            .and_then(|(key, value)| (key == name).then_some(value))
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|value| value != 0)
    })
}

fn check_guest_running(guest: &mut Child, label: &str, stderr_log: &Path) -> Result<()> {
    if let Some(status) = guest.try_wait()? {
        bail!(
            "{label} QEMU/KVM guest exited before readiness with {status}; inspect {}",
            stderr_log.display()
        );
    }
    Ok(())
}

fn stop_guest(guest: &mut Child) {
    if guest.try_wait().ok().flatten().is_none() {
        let _ = guest.kill();
    }
    let _ = guest.wait();
}
