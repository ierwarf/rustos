// SPDX-License-Identifier: MIT

struct ElfMapResult {
    load_bias: u64,
    entry: u64,
    actual_entry: u64,
    phdr_addr: u64,
    phnum: u64,
    phent: u64,
    max_loaded_end: u64,
    interpreter_path: Option<String>,
    interpreter_base: u64,
    backing_fds: Vec<i32>,
}

struct PreparedExecutable {
    windows_runtime: Option<RustosProcSetWindowsRuntimeBrokerArgs>,
    linux_runtime: Option<ElfMapResult>,
    cleanup_fds: Vec<i32>,
}

enum ExecutableAdmission {
    Elf64 {
        header: [u8; ELF_HEADER_SIZE],
        program_headers: Vec<u8>,
        load_bias: u64,
    },
    Pe64,
}

impl ExecutableAdmission {
    fn format(&self) -> u16 {
        match self {
            Self::Elf64 { .. } => PROC_BROKER_FORMAT_ELF64,
            Self::Pe64 => PROC_BROKER_FORMAT_PE64,
        }
    }
}

fn map_executable_segments(
    fd: i32,
    exec_path: &str,
    prepare_handle: u64,
    admission: &ExecutableAdmission,
    argv: &[CString],
    env: &[CString],
) -> Result<PreparedExecutable, i32> {
    match admission {
        ExecutableAdmission::Elf64 {
            header,
            program_headers,
            load_bias,
        } => {
            trace_line(&format!(
                "loaderd: elf map header reused handle={prepare_handle}"
            ));
            trace_line(&format!(
                "loaderd: elf map phdrs reused handle={prepare_handle} bytes={}",
                program_headers.len()
            ));
            map_admitted_elf_segments_fd(
                fd,
                prepare_handle,
                true,
                header,
                program_headers,
                *load_bias,
            )
            .map(|result| {
                let mut cleanup_fds = result.backing_fds.clone();
                cleanup_fds.push(fd);
                PreparedExecutable {
                    windows_runtime: None,
                    linux_runtime: Some(result),
                    cleanup_fds,
                }
            })
        }
        ExecutableAdmission::Pe64 => map_pe_segments_fd(fd, prepare_handle, exec_path, argv, env)
            .map(|runtime| PreparedExecutable {
                windows_runtime: Some(runtime),
                linux_runtime: None,
                cleanup_fds: vec![fd],
            })
            .inspect_err(|errno| {
                debug_line(&format!(
                    "loaderd: map pe segments failed exec={exec_path} errno={errno}",
                ));
            }),
    }
}

fn validate_request(received: usize, request: &LoaderSpawnRequest) -> Result<(), i32> {
    if received != size_of::<LoaderSpawnRequest>()
        || request.version != LOADER_REQUEST_ABI_VERSION
        || request.requester_pid == 0
        || request.argv_count as usize > LOADER_SPAWN_MAX_ARG_COUNT
        || request.env_count as usize > LOADER_SPAWN_MAX_ENV_COUNT
        || request.argv_bytes_len as usize > LOADER_SPAWN_ARG_BYTES
        || request.env_bytes_len as usize > LOADER_SPAWN_ENV_BYTES
    {
        return Err(EINVAL);
    }
    if request.op == LOADER_OP_ACTIVATE {
        if request.flags != 0
            || request.console_session != 0
            || request.weight_micros != 0
            || request.target_pid == 0
            || request.target_tid != 0
            || request.exec_ticket != 0
            || request.exec_path_len != 0
            || request.argv_count != 0
            || request.env_count != 0
            || request.argv_bytes_len != 0
            || request.env_bytes_len != 0
        {
            return Err(EINVAL);
        }
        return Ok(());
    }
    if !matches!(request.op, LOADER_OP_SPAWN_EXEC | LOADER_OP_EXEC_TARGET)
        || request.exec_path_len == 0
        || request.exec_path_len as usize > LOADER_SPAWN_EXEC_PATH_CAPACITY
    {
        return Err(EINVAL);
    }
    if request.op == LOADER_OP_SPAWN_EXEC
        && (request.target_pid != 0 || request.target_tid != 0 || request.exec_ticket != 0)
    {
        return Err(EINVAL);
    }
    if request.op == LOADER_OP_EXEC_TARGET
        && (request.target_pid == 0 || request.target_tid == 0 || request.exec_ticket == 0)
    {
        return Err(EINVAL);
    }
    Ok(())
}

fn validate_commercial_request(request: &CommercialMaxProtocolRequest) -> Result<(), i32> {
    if !request.has_valid_envelope() || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_LOADERD {
        return Err(EINVAL);
    }
    match request.header.op {
        COMMERCIAL_MAX_LOADERD_OP_IMAGE_PROBE => {
            if request.path_len == 0
                || request.payload_len != 0
                || request.arg0 != 0
                || request.arg1 != 0
                || request.arg2 != 0
                || request.arg3 != 0
            {
                Err(EINVAL)
            } else {
                Ok(())
            }
        }
        COMMERCIAL_MAX_LOADERD_OP_ELF_RUNTIME_PLAN
        | COMMERCIAL_MAX_LOADERD_OP_PE_RUNTIME_PLAN
        | COMMERCIAL_MAX_LOADERD_OP_INTERPRETER_PLAN
        | COMMERCIAL_MAX_LOADERD_OP_IMPORT_POLICY
        | COMMERCIAL_MAX_LOADERD_OP_MAP_PLAN
        | COMMERCIAL_MAX_LOADERD_OP_AUXV_PLAN => {
            if request.path_len != 0
                || request.payload_len != 0
                || request.arg0 != 0
                || request.arg1 != 0
                || request.arg2 != 0
                || request.arg3 != 0
            {
                Err(EINVAL)
            } else {
                Ok(())
            }
        }
        _ => Err(EINVAL),
    }
}

fn sender_owns_any_loader_role(sender_pid: u64) -> bool {
    sender_pid != 0
        && [
            IPC_SERVICE_ROOTD,
            IPC_SERVICE_INITD,
            IPC_SERVICE_SESSIOND,
            IPC_SERVICE_PROCD,
        ]
        .into_iter()
        .any(|service_id| {
            rustos_svc_runtime::ipc::validate_service_owner(service_id, sender_pid) >= 0
        })
}

fn commercial_request_path(request: &CommercialMaxProtocolRequest) -> Result<&str, i32> {
    let len = request.path_len as usize;
    if len == 0 {
        return Err(EINVAL);
    }
    core::str::from_utf8(&request.path[..len]).map_err(|_| EINVAL)
}

fn loader_descriptor(label: &str, op: u16, value0: u64) -> CommercialMaxProtocolDescriptorWire {
    let mut descriptor = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_LOADERD,
        op,
        flags: 0,
        service_id: IPC_SERVICE_LOADERD,
        capability_mask: loader_capability_mask(op),
        value0,
        value1: 0,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    copy_label(label, &mut descriptor.name, &mut descriptor.name_len);
    descriptor
}

fn loader_capability(label: &str, op: u16) -> CommercialMaxCapabilityLeaseWire {
    let mut capability = CommercialMaxCapabilityLeaseWire {
        lease_id: ((COMMERCIAL_MAX_PROTOCOL_LOADERD as u64) << 32) | u64::from(op),
        service_id: IPC_SERVICE_LOADERD,
        capability_mask: loader_capability_mask(op),
        rights_mask: loader_capability_mask(op),
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    copy_label(label, &mut capability.label, &mut capability.label_len);
    capability
}

fn loader_capability_mask(op: u16) -> u64 {
    match op {
        COMMERCIAL_MAX_LOADERD_OP_IMAGE_PROBE => 1 << 0,
        COMMERCIAL_MAX_LOADERD_OP_ELF_RUNTIME_PLAN => 1 << 1,
        COMMERCIAL_MAX_LOADERD_OP_PE_RUNTIME_PLAN => 1 << 2,
        COMMERCIAL_MAX_LOADERD_OP_INTERPRETER_PLAN => 1 << 3,
        COMMERCIAL_MAX_LOADERD_OP_IMPORT_POLICY => 1 << 4,
        COMMERCIAL_MAX_LOADERD_OP_MAP_PLAN => 1 << 5,
        COMMERCIAL_MAX_LOADERD_OP_AUXV_PLAN => 1 << 6,
        _ => 0,
    }
}

fn copy_label(label: &str, target: &mut [u8], len: &mut u16) {
    let bytes = label.as_bytes();
    let count = bytes.len().min(target.len());
    target[..count].copy_from_slice(&bytes[..count]);
    *len = count as u16;
}

fn request_text(bytes: &[u8], len: usize) -> Result<&str, i32> {
    if len == 0 || len > bytes.len() || bytes[..len].contains(&0) {
        return Err(EINVAL);
    }
    core::str::from_utf8(&bytes[..len]).map_err(|_| EINVAL)
}

fn parse_blob(bytes: &[u8], len: usize, count: usize) -> Result<Vec<CString>, i32> {
    if len > bytes.len() {
        return Err(EINVAL);
    }
    if count == 0 {
        return (len == 0).then(Vec::new).ok_or(EINVAL);
    }
    let mut values = Vec::with_capacity(count);
    let mut start = 0usize;
    for index in 0..len {
        if bytes[index] != 0 {
            continue;
        }
        if start == index {
            return Err(EINVAL);
        }
        values.push(CString::new(&bytes[start..index]).map_err(|_| EINVAL)?);
        start = index + 1;
    }
    if start != len || values.len() != count {
        return Err(EINVAL);
    }
    Ok(values)
}

fn validate_executable_fd(fd: i32) -> Result<ExecutableAdmission, i32> {
    let mut header = [0_u8; ELF_HEADER_SIZE];
    read_exact_at(fd, 0, &mut header)?;
    if header[..4] == *b"\x7fELF" {
        let phdrs = read_program_headers(fd, &header)?;
        let load_bias = validate_elf_fd(fd, &header, &phdrs, ELF_MAIN_DYN_LOAD_OFFSET)?;
        return Ok(ExecutableAdmission::Elf64 {
            header,
            program_headers: phdrs,
            load_bias,
        });
    }
    if &header[..2] == b"MZ" {
        validate_pe_fd(fd, &header[..PE_DOS_HEADER_SIZE])?;
        return Ok(ExecutableAdmission::Pe64);
    }
    Err(ENOEXEC)
}

/// Reads the full program-header table in a single pread64. Loaderd used to
/// issue one IPC roundtrip per program header (×3, since validation, load-bias
/// computation, and segment mapping each re-walked the table). With a typical
/// dynamic ELF carrying ~10 program headers, that was ~30 wasted IPC bounces
/// per spawn. Reading the table once keeps the producer/consumer wakeup count
/// down and dominates spawn latency on TCG.
fn read_program_headers(fd: i32, header: &[u8; ELF_HEADER_SIZE]) -> Result<Vec<u8>, i32> {
    let phoff = read_u64(header, 32);
    let phentsize = read_u16(header, 54);
    let phnum = read_u16(header, 56);
    if phnum == 0 || phnum > ELF_MAX_PROGRAM_HEADERS {
        return Err(ENOEXEC);
    }
    if phentsize as usize != ELF_PROGRAM_HEADER_SIZE {
        return Err(ENOEXEC);
    }
    let table_len = u64::from(phentsize)
        .checked_mul(u64::from(phnum))
        .ok_or(EOVERFLOW)?;
    let table_end = phoff.checked_add(table_len).ok_or(EOVERFLOW)?;
    if table_end > i64::MAX as u64 {
        return Err(EOVERFLOW);
    }
    let mut buf = alloc::vec![0_u8; table_len as usize];
    if !buf.is_empty() {
        read_exact_at(fd, phoff, &mut buf)?;
    }
    Ok(buf)
}

fn program_header_at(phdrs: &[u8], index: u64) -> Result<&[u8], i32> {
    let start = usize::try_from(index)
        .map_err(|_| EOVERFLOW)?
        .checked_mul(ELF_PROGRAM_HEADER_SIZE)
        .ok_or(EOVERFLOW)?;
    let end = start
        .checked_add(ELF_PROGRAM_HEADER_SIZE)
        .ok_or(EOVERFLOW)?;
    phdrs.get(start..end).ok_or(ENOEXEC)
}

fn validate_elf_fd(
    fd: i32,
    header: &[u8; ELF_HEADER_SIZE],
    phdrs: &[u8],
    dyn_load_offset: u64,
) -> Result<u64, i32> {
    let summary = admit_elf64_image(
        header,
        phdrs,
        dyn_load_offset,
        PROC_BROKER_USER_SPACE_BASE,
        PROC_BROKER_USER_SPACE_END_EXCLUSIVE,
    )
    .map_err(byte_admission_errno)?;
    let phnum = u64::from(summary.program_headers);
    for index in 0..phnum {
        let ph_slice = program_header_at(phdrs, index)?;
        let mut ph = [0_u8; ELF_PROGRAM_HEADER_SIZE];
        ph.copy_from_slice(ph_slice);
        if read_u32(&ph, 0) == ELF_PT_INTERP {
            validate_elf_interp(fd, &ph)?;
        }
    }
    Ok(summary.load_bias)
}

fn map_elf_segments_fd(
    fd: i32,
    prepare_handle: u64,
    dyn_load_offset: u64,
    map_interpreter: bool,
) -> Result<ElfMapResult, i32> {
    let mut header = [0_u8; ELF_HEADER_SIZE];
    trace_line(&format!(
        "loaderd: elf map header begin handle={prepare_handle}"
    ));
    read_exact_at(fd, 0, &mut header)?;
    trace_line(&format!(
        "loaderd: elf map header done handle={prepare_handle}"
    ));
    let phdrs = read_program_headers(fd, &header)?;
    trace_line(&format!(
        "loaderd: elf map phdrs done handle={prepare_handle} bytes={}",
        phdrs.len()
    ));
    let load_bias = validate_elf_fd(fd, &header, &phdrs, dyn_load_offset)?;
    trace_line(&format!(
        "loaderd: elf map validation done handle={prepare_handle}"
    ));
    map_admitted_elf_segments_fd(
        fd,
        prepare_handle,
        map_interpreter,
        &header,
        &phdrs,
        load_bias,
    )
}

fn map_admitted_elf_segments_fd(
    fd: i32,
    prepare_handle: u64,
    map_interpreter: bool,
    header: &[u8; ELF_HEADER_SIZE],
    phdrs: &[u8],
    load_bias: u64,
) -> Result<ElfMapResult, i32> {
    let _phoff = read_u64(header, 32);
    let e_entry = read_u64(header, 24);
    let phentsize = read_u16(header, 54) as u64;
    let phnum = read_u16(header, 56) as u64;
    let entry = e_entry.checked_add(load_bias).ok_or(EOVERFLOW)?;
    let phdr_addr = program_header_table_addr_from_phdrs(header, phdrs, load_bias)?;

    let mut max_loaded_end: u64 = load_bias;
    let mut interpreter_path = None::<String>;
    let mut mappings = Vec::<ElfLoadMapping>::new();

    for index in 0..phnum {
        let ph_slice = program_header_at(phdrs, index)?;
        let mut ph = [0_u8; ELF_PROGRAM_HEADER_SIZE];
        ph.copy_from_slice(ph_slice);
        match read_u32(&ph, 0) {
            ELF_PT_LOAD => {
                mappings.push(elf_load_segment_mapping(fd, &ph, load_bias)?);
                let vaddr = read_u64(&ph, 16);
                let memsz = read_u64(&ph, 40);
                let end = vaddr
                    .checked_add(memsz)
                    .and_then(|e| e.checked_add(load_bias))
                    .ok_or(EOVERFLOW)?;
                max_loaded_end = max_loaded_end.max(end);
            }
            ELF_PT_INTERP if map_interpreter => {
                interpreter_path = Some(read_elf_interp_path(fd, &ph)?);
            }
            _ => {}
        }
    }

    publish_elf_mappings(prepare_handle, &mappings)?;
    trace_line(&format!(
        "loaderd: elf main mappings done handle={prepare_handle}"
    ));

    max_loaded_end = align_up(max_loaded_end, 4096)?;
    let mut backing_fds = Vec::new();

    let (interpreter_base, actual_entry, interp_path_out, interp_max_end) =
        if let Some(path) = interpreter_path.as_deref() {
            let interp = match map_elf_interpreter(path, prepare_handle) {
                Ok(interp) => interp,
                Err(errno) => {
                    close_fds(&backing_fds);
                    return Err(errno);
                }
            };
            let end = interp.max_loaded_end.max(max_loaded_end);
            backing_fds.extend(interp.backing_fds);
            (
                interp.load_bias,
                interp.entry,
                interpreter_path,
                end,
            )
        } else {
            (0, entry, None, max_loaded_end)
        };

    Ok(ElfMapResult {
        load_bias,
        entry,
        actual_entry,
        phdr_addr,
        phnum,
        phent: phentsize,
        max_loaded_end: interp_max_end,
        interpreter_path: interp_path_out,
        interpreter_base,
        backing_fds,
    })
}

fn map_elf_interpreter(path: &str, prepare_handle: u64) -> Result<ElfMapResult, i32> {
    let fd = open_immutable_file_snapshot(path)?;
    let mut result = match map_elf_segments_fd(fd, prepare_handle, ELF_INTERP_LOAD_OFFSET, false) {
        Ok(result) => result,
        Err(errno) => {
            let _ = syscall1(SYS_CLOSE, fd as u64);
            return Err(errno);
        }
    };
    result.backing_fds.push(fd);
    Ok(result)
}

fn program_header_table_addr_from_phdrs(
    header: &[u8; ELF_HEADER_SIZE],
    phdrs: &[u8],
    load_bias: u64,
) -> Result<u64, i32> {
    let phoff = read_u64(header, 32);
    let phentsize = read_u16(header, 54) as u64;
    let phnum = read_u16(header, 56) as u64;
    let ph_size = phentsize.checked_mul(phnum).ok_or(EOVERFLOW)?;
    let ph_end = phoff.checked_add(ph_size).ok_or(EOVERFLOW)?;

    for index in 0..phnum {
        let ph_slice = program_header_at(phdrs, index)?;
        let mut ph = [0_u8; ELF_PROGRAM_HEADER_SIZE];
        ph.copy_from_slice(ph_slice);
        if read_u32(&ph, 0) == ELF_PT_PHDR {
            return read_u64(&ph, 16).checked_add(load_bias).ok_or(EOVERFLOW);
        }
    }

    for index in 0..phnum {
        let ph_slice = program_header_at(phdrs, index)?;
        let mut ph = [0_u8; ELF_PROGRAM_HEADER_SIZE];
        ph.copy_from_slice(ph_slice);
        if read_u32(&ph, 0) != ELF_PT_LOAD || read_u64(&ph, 32) == 0 {
            continue;
        }
        let file_start = read_u64(&ph, 8);
        let file_end = file_start.checked_add(read_u64(&ph, 32)).ok_or(EOVERFLOW)?;
        if phoff < file_start || ph_end > file_end {
            continue;
        }
        let table_delta = phoff - file_start;
        return read_u64(&ph, 16)
            .checked_add(table_delta)
            .and_then(|value| value.checked_add(load_bias))
            .ok_or(EOVERFLOW);
    }

    Err(ENOEXEC)
}

enum ElfLoadMapping {
    File(RustosProcMapFileBatchEntry),
    Zeroed {
        target_addr: u64,
        mem_len: u64,
        flags: u64,
    },
}

fn elf_load_segment_mapping(
    fd: i32,
    ph: &[u8; ELF_PROGRAM_HEADER_SIZE],
    load_bias: u64,
) -> Result<ElfLoadMapping, i32> {
    let segment_offset = read_u64(ph, 8);
    let segment_vaddr = read_u64(ph, 16);
    let file_size = read_u64(ph, 32);
    let mem_size = read_u64(ph, 40);
    let page_delta = segment_vaddr & 0xfff;
    let target_addr = (segment_vaddr & !0xfff)
        .checked_add(load_bias)
        .ok_or(EOVERFLOW)?;
    let file_offset = segment_offset.checked_sub(page_delta).ok_or(ENOEXEC)?;
    let file_len = page_delta.checked_add(file_size).ok_or(EOVERFLOW)?;
    let mem_len = align_up(page_delta.checked_add(mem_size).ok_or(EOVERFLOW)?, 4096)?;
    let flags = proc_map_flags(read_u32(ph, 4));

    if file_size == 0 {
        return Ok(ElfLoadMapping::Zeroed {
            target_addr,
            mem_len,
            flags,
        });
    }

    Ok(ElfLoadMapping::File(RustosProcMapFileBatchEntry {
        fd: fd as u64,
        file_offset,
        target_addr,
        file_len,
        mem_len,
        flags,
        reserved0: 0,
    }))
}

fn publish_elf_mappings(
    prepare_handle: u64,
    mappings: &[ElfLoadMapping],
) -> Result<(), i32> {
    let mut file_maps = Vec::<RustosProcMapFileBatchEntry>::new();
    for mapping in mappings {
        match mapping {
            ElfLoadMapping::File(entry) => file_maps.push(*entry),
            ElfLoadMapping::Zeroed {
                target_addr,
                mem_len,
                flags,
            } => {
                flush_elf_file_map_batch(prepare_handle, &mut file_maps)?;
                map_elf_zeroed_segment(prepare_handle, *target_addr, *mem_len, *flags)?;
            }
        }
    }
    flush_elf_file_map_batch(prepare_handle, &mut file_maps)
}

fn flush_elf_file_map_batch(
    prepare_handle: u64,
    file_maps: &mut Vec<RustosProcMapFileBatchEntry>,
) -> Result<(), i32> {
    for chunk in file_maps.chunks(PROC_BROKER_BATCH_CAPACITY) {
        trace_line(&format!(
            "loaderd: elf file-map batch begin handle={prepare_handle} count={}",
            chunk.len()
        ));
        let mut args = RustosProcMapFileBatchBrokerArgs {
            prepare_handle,
            count: chunk.len() as u32,
            ..RustosProcMapFileBatchBrokerArgs::default()
        };
        args.entries[..chunk.len()].copy_from_slice(chunk);
        let status = syscall1(
            SYS_RUSTOS_PROC_MAP_FILE_BATCH_BROKER,
            (&args as *const RustosProcMapFileBatchBrokerArgs) as u64,
        );
        if status < 0 {
            file_maps.clear();
            return Err((-status) as i32);
        }
        trace_line(&format!(
            "loaderd: elf file-map batch done handle={prepare_handle} count={}",
            chunk.len()
        ));
    }
    file_maps.clear();
    Ok(())
}

fn map_elf_zeroed_segment(
    prepare_handle: u64,
    target_addr: u64,
    mem_len: u64,
    flags: u64,
) -> Result<(), i32> {
    let args = RustosProcMapZeroedBrokerArgs {
        prepare_handle,
        target_addr,
        mem_len,
        flags,
        reserved0: 0,
    };
    let status = syscall1(
        SYS_RUSTOS_PROC_MAP_ZEROED_BROKER,
        (&args as *const RustosProcMapZeroedBrokerArgs) as u64,
    );
    syscall_unit_result(status)
}

fn proc_map_flags(elf_flags: u32) -> u64 {
    let mut flags = PROC_BROKER_MAP_PRIVATE;
    if elf_flags & ELF_PF_R != 0 {
        flags |= PROC_BROKER_MAP_READ;
    }
    if elf_flags & ELF_PF_W != 0 {
        flags |= PROC_BROKER_MAP_WRITE;
    }
    if elf_flags & ELF_PF_X != 0 {
        flags |= PROC_BROKER_MAP_EXEC;
    }
    flags
}

fn validate_elf_interp(fd: i32, ph: &[u8; ELF_PROGRAM_HEADER_SIZE]) -> Result<(), i32> {
    read_elf_interp_path(fd, ph).map(|_| ())
}

fn read_elf_interp_path(fd: i32, ph: &[u8; ELF_PROGRAM_HEADER_SIZE]) -> Result<String, i32> {
    let offset = read_u64(ph, 8);
    let file_size = read_u64(ph, 32);
    if file_size < 2 || file_size as usize > LOADER_SPAWN_EXEC_PATH_CAPACITY {
        return Err(ENOEXEC);
    }
    let mut bytes = vec![0_u8; file_size as usize];
    read_exact_at(fd, offset, &mut bytes)?;
    if bytes.last().copied() != Some(0) || bytes[..bytes.len() - 1].contains(&0) {
        return Err(ENOEXEC);
    }
    let path = core::str::from_utf8(&bytes[..bytes.len() - 1]).map_err(|_| ENOEXEC)?;
    if !path.starts_with('/') {
        return Err(ENOEXEC);
    }
    Ok(path.to_string())
}
