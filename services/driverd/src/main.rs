use std::io::Write;
use std::mem::size_of;
use std::sync::Mutex;
use std::time::Instant;
use std::{collections::BTreeSet, fs, thread, time::Duration};

use rustos_user_abi::syscall::{
    CommercialMaxCapabilityLeaseWire, CommercialMaxProtocolDescriptorWire,
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, LinuxDriverSymbolEventWire,
    LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse, RustosDriverLoadModuleBrokerArgs,
    RustosDriverProbeAliasBrokerArgs, RustosDriverSymbolEventBrokerArgs,
    RustosServiceDriverResourceBrokerArgs, COMMERCIAL_MAX_DRIVERD_OP_DRIVER_PLAN,
    COMMERCIAL_MAX_DRIVERD_OP_FALLBACK_POLICY, COMMERCIAL_MAX_DRIVERD_OP_MODULE_LOAD_AUTHORIZE,
    COMMERCIAL_MAX_DRIVERD_OP_PROVIDER_SELECT, COMMERCIAL_MAX_DRIVERD_OP_RETRY_BUDGET,
    COMMERCIAL_MAX_DRIVERD_OP_SYMBOL_POLICY, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_DRIVERD, COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS,
    COMMERCIAL_MAX_PROTOCOL_SERVICE_DRIVERD, COMMERCIAL_MAX_SERVICE_DRIVERD_OP_DMA_BUFFER,
    COMMERCIAL_MAX_SERVICE_DRIVERD_OP_DRIVER_INSTANCE,
    COMMERCIAL_MAX_SERVICE_DRIVERD_OP_IO_PORT_LEASE, COMMERCIAL_MAX_SERVICE_DRIVERD_OP_IRQ_ROUTE,
    COMMERCIAL_MAX_SERVICE_DRIVERD_OP_MMIO_LEASE, DRIVER_BUS_PCI, DRIVER_BUS_PLATFORM,
    DRIVER_BUS_SERIO, DRIVER_BUS_USB, DRIVER_BUS_VIRTIO, DRIVER_CLASS_DISPLAY, DRIVER_CLASS_INPUT,
    DRIVER_CLASS_NETWORK, DRIVER_CLASS_STORAGE, DRIVER_CLASS_USB,
    DRIVER_LOAD_POLICY_DISPLAY_FALLBACK, DRIVER_LOAD_POLICY_DISPLAY_PRIMARY,
    DRIVER_SYMBOL_EVENT_BROKER_ABI_VERSION, DRIVER_SYMBOL_EVENT_OP_DRAIN, IPC_SERVICE_DRIVERD,
    IPC_SERVICE_SERVICE_DRIVERD, SERVICE_DRIVER_RESOURCE_BROKER_ABI_VERSION,
    SERVICE_DRIVER_RESOURCE_OP_DMA_BUFFER, SERVICE_DRIVER_RESOURCE_OP_IO_PORT_LEASE,
    SERVICE_DRIVER_RESOURCE_OP_IRQ_ROUTE, SERVICE_DRIVER_RESOURCE_OP_MMIO_LEASE,
    SYSCALL_OFFLOAD_ABI_VERSION, SYSCALL_OFFLOAD_OP_DRIVER_LOAD_POLICY,
    SYSCALL_OFFLOAD_PATH_CAPACITY, SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_DRIVER_LOAD_MODULE_BROKER,
    SYS_RUSTOS_DRIVER_PROBE_ALIAS_BROKER, SYS_RUSTOS_DRIVER_SYMBOL_EVENT_BROKER,
    SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV, SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
    SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_SERVICE_DRIVER_RESOURCE_BROKER,
};

const RECV_BACKOFF: Duration = Duration::from_millis(10);
const LOADABLE_DRIVER_REGISTRY_PATH: &str = "system/registry/kernel/loadable-drivers.tsv";
static SYMBOL_EVENTS: Mutex<Vec<LinuxDriverSymbolEventWire>> = Mutex::new(Vec::new());

fn main() {
    debug_line("driverd: service start");
    let endpoint = syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE);
    if endpoint < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "driverd: endpoint create failed errno={}",
            -endpoint
        );
        return;
    }
    let register = syscall2(
        SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
        IPC_SERVICE_DRIVERD,
        endpoint as u64,
    );
    if register < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "driverd: endpoint register failed errno={}",
            -register
        );
        return;
    }
    let service_driver_register = syscall2(
        SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
        IPC_SERVICE_SERVICE_DRIVERD,
        endpoint as u64,
    );
    if service_driver_register < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "driverd: service-driver endpoint register failed errno={}",
            -service_driver_register
        );
        return;
    }

    debug_line("driverd: driver policy endpoint registered");
    debug_line("driverd: service-driver policy endpoint registered");
    debug_line("driverd: autoload begin");
    autoload_from_registry();
    debug_line("driverd: autoload done");
    serve(endpoint as u64);
}

#[derive(Clone, Debug)]
struct DriverRecord {
    name: String,
    class: u32,
    bus: u32,
    load_priority: i32,
    image_path: String,
    aliases: String,
    deps: String,
    softdeps: String,
    linux_driver_names: String,
    provider_group: String,
    fallback_only: bool,
}

fn autoload_from_registry() {
    let started_at = Instant::now();
    debug_line("driverd: registry read begin");
    let mut records = match registry_records() {
        Ok(records) => records,
        Err(err) => {
            debug_line(&format!("driverd: registry load failed error={err}"));
            return;
        }
    };
    debug_line(&format!(
        "driverd: registry parsed count={} elapsed_ms={}",
        records.len(),
        started_at.elapsed().as_millis()
    ));
    records.sort_by_key(|record| {
        (
            !record.softdeps.trim().is_empty(),
            record.fallback_only,
            record.load_priority,
            record.name.clone(),
        )
    });

    let known_names = records
        .iter()
        .map(|record| record.name.clone())
        .collect::<BTreeSet<_>>();
    let mut primary_records = Vec::new();
    let mut fallback_records = Vec::new();
    for record in records {
        if record.fallback_only {
            fallback_records.push(record);
        } else {
            primary_records.push(record);
        }
    }
    let mut loaded = BTreeSet::new();
    let mut skipped = BTreeSet::new();
    let mut provider_groups = BTreeSet::new();
    autoload_queue(
        primary_records,
        &known_names,
        &mut loaded,
        &mut skipped,
        &mut provider_groups,
    );
    autoload_queue(
        fallback_records,
        &known_names,
        &mut loaded,
        &mut skipped,
        &mut provider_groups,
    );
    debug_line(&format!(
        "driverd: autoload registry complete loaded={} skipped={} elapsed_ms={}",
        loaded.len(),
        skipped.len(),
        started_at.elapsed().as_millis()
    ));
}

fn autoload_queue(
    mut pending: Vec<DriverRecord>,
    known_names: &BTreeSet<String>,
    loaded: &mut BTreeSet<String>,
    skipped: &mut BTreeSet<String>,
    provider_groups: &mut BTreeSet<String>,
) {
    while !pending.is_empty() {
        let mut progress = false;
        let mut deferred = Vec::new();
        for record in pending.into_iter() {
            match load_record(&record, &known_names, loaded, skipped, provider_groups) {
                LoadResult::Progress => progress = true,
                LoadResult::Deferred => deferred.push(record),
            }
        }
        if !progress {
            for record in deferred.iter() {
                debug_line(&format!(
                    "driverd: deferred name={} path={} reason=dependency unavailable",
                    record.name, record.image_path
                ));
            }
            break;
        }
        pending = deferred;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoadResult {
    Progress,
    Deferred,
}

fn load_record(
    record: &DriverRecord,
    known_names: &BTreeSet<String>,
    loaded: &mut BTreeSet<String>,
    skipped: &mut BTreeSet<String>,
    provider_groups: &mut BTreeSet<String>,
) -> LoadResult {
    let started_at = Instant::now();
    debug_line(&format!(
        "driverd: record begin name={} path={} class={} bus={}",
        record.name, record.image_path, record.class, record.bus
    ));
    if loaded.contains(record.name.as_str()) || skipped.contains(record.name.as_str()) {
        debug_line(&format!(
            "driverd: record already handled name={} elapsed_ms={}",
            record.name,
            started_at.elapsed().as_millis()
        ));
        return LoadResult::Progress;
    }
    for dep in comma_fields(record.deps.as_str()) {
        if skipped.contains(dep) {
            skipped.insert(record.name.clone());
            debug_line(&format!(
                "driverd: skipped name={} reason=dependency skipped dep={}",
                record.name, dep
            ));
            debug_line(&format!(
                "driverd: record skipped name={} reason=dependency skipped elapsed_ms={}",
                record.name,
                started_at.elapsed().as_millis()
            ));
            return LoadResult::Progress;
        }
        if !known_names.contains(dep) {
            skipped.insert(record.name.clone());
            debug_line(&format!(
                "driverd: skipped name={} reason=dependency missing dep={}",
                record.name, dep
            ));
            debug_line(&format!(
                "driverd: record skipped name={} reason=dependency missing elapsed_ms={}",
                record.name,
                started_at.elapsed().as_millis()
            ));
            return LoadResult::Progress;
        }
        if known_names.contains(dep) && !loaded.contains(dep) {
            return LoadResult::Deferred;
        }
    }
    for softdep in comma_fields(record.softdeps.as_str()) {
        if !loaded.contains(softdep) {
            debug_line(&format!(
                "driverd: softdep not loaded name={} softdep={}",
                record.name, softdep
            ));
        }
    }
    if !record.provider_group.is_empty() && provider_groups.contains(record.provider_group.as_str())
    {
        skipped.insert(record.name.clone());
        debug_line(&format!(
            "driverd: skipped name={} reason=provider active group={}",
            record.name, record.provider_group
        ));
        debug_line(&format!(
            "driverd: record skipped name={} reason=provider active elapsed_ms={}",
            record.name,
            started_at.elapsed().as_millis()
        ));
        return LoadResult::Progress;
    }
    if !aliases_match(record) {
        skipped.insert(record.name.clone());
        debug_line(&format!(
            "driverd: skipped name={} reason=no matching alias aliases={}",
            record.name, record.aliases
        ));
        debug_line(&format!(
            "driverd: record skipped name={} reason=no matching alias elapsed_ms={}",
            record.name,
            started_at.elapsed().as_millis()
        ));
        return LoadResult::Progress;
    }
    if requires_service_driver_authorization(record) && !authorize_service_driver(record) {
        skipped.insert(record.name.clone());
        debug_line(&format!(
            "driverd: skipped name={} reason=service-driver lease denied",
            record.name
        ));
        return LoadResult::Progress;
    }
    let load_started = Instant::now();
    let result = load_module(record);
    debug_line(&format!(
        "driverd: load module returned name={} elapsed_ms={} status={}",
        record.name,
        load_started.elapsed().as_millis(),
        result
    ));
    drain_symbol_events(record);
    if result == 0 {
        loaded.insert(record.name.clone());
        if !record.provider_group.is_empty() {
            provider_groups.insert(record.provider_group.clone());
        }
        debug_line(&format!(
            "driverd: loaded name={} class={} bus={} path={}",
            record.name, record.class, record.bus, record.image_path
        ));
    } else {
        skipped.insert(record.name.clone());
        // libc::syscall returns -1 and sets errno on failure, but RustOS bypass
        // paths can also return the raw negative errno. Prefer libc's errno when
        // result is -1 (the canonical libc signal), otherwise treat the raw
        // negative as the errno directly.
        let errno = if result == -1 {
            last_errno()
        } else if result < 0 {
            (-result) as i32
        } else {
            last_errno()
        };
        match errno {
            libc::ENOSYS => debug_line(&format!(
                "driverd: skipped name={} reason=loader unimplemented (pending user-space driver host) path={}",
                record.name, record.image_path
            )),
            libc::EOPNOTSUPP => debug_line(&format!(
                "driverd: skipped name={} reason=unsupported class/bus topology path={}",
                record.name, record.image_path
            )),
            libc::EIO
                if record.class == DRIVER_CLASS_DISPLAY
                    && !record.provider_group.is_empty()
                    && !record.fallback_only =>
            {
                debug_line(&format!(
                    "driverd: display provider not published name={} group={} path={}",
                    record.name, record.provider_group, record.image_path
                ));
                debug_line(&format!(
                    "driverd: load failed name={} path={} errno={errno}",
                    record.name, record.image_path
                ));
            }
            _ => debug_line(&format!(
                "driverd: load failed name={} path={} errno={errno}",
                record.name, record.image_path
            )),
        }
    }
    debug_line(&format!(
        "driverd: record done name={} elapsed_ms={}",
        record.name,
        started_at.elapsed().as_millis()
    ));
    LoadResult::Progress
}

fn drain_symbol_events(record: &DriverRecord) {
    let mut drained = 0usize;
    loop {
        let mut event = LinuxDriverSymbolEventWire::default();
        let args = RustosDriverSymbolEventBrokerArgs {
            abi_version: DRIVER_SYMBOL_EVENT_BROKER_ABI_VERSION,
            op: DRIVER_SYMBOL_EVENT_OP_DRAIN,
            flags: 0,
            out_ptr: (&mut event as *mut LinuxDriverSymbolEventWire) as u64,
            out_len: size_of::<LinuxDriverSymbolEventWire>() as u64,
            reserved0: 0,
        };
        let result = syscall1(
            SYS_RUSTOS_DRIVER_SYMBOL_EVENT_BROKER,
            (&args as *const RustosDriverSymbolEventBrokerArgs) as u64,
        );
        if result == 0 {
            break;
        }
        if result < 0 {
            let errno = if result == -1 {
                last_errno()
            } else {
                (-result) as i32
            };
            debug_line(&format!(
                "driverd: symbol event drain failed name={} errno={errno}",
                record.name
            ));
            break;
        }
        drained += 1;
        record_symbol_event(event);
        debug_line(&format!(
            "driverd: linux .ko slow-path symbol observed record={} module={} symbol={} context={} scope={} arg0=0x{:x} arg1=0x{:x} arg2=0x{:x} dropped_before={}",
            record.name,
            event_module(&event),
            event_symbol(&event),
            event.context,
            event.scope,
            event.arg0,
            event.arg1,
            event.arg2,
            event.dropped_before
        ));
    }
    if drained != 0 {
        debug_line(&format!(
            "driverd: symbol event drain complete name={} count={drained}",
            record.name
        ));
    }
}

fn record_symbol_event(event: LinuxDriverSymbolEventWire) {
    match SYMBOL_EVENTS.lock() {
        Ok(mut events) => events.push(event),
        Err(_) => debug_line("driverd: symbol event state poisoned"),
    }
}

fn symbol_event_count() -> u64 {
    SYMBOL_EVENTS
        .lock()
        .map(|events| events.len() as u64)
        .unwrap_or(0)
}

fn event_symbol(event: &LinuxDriverSymbolEventWire) -> String {
    event_text(&event.symbol, event.symbol_len)
}

fn event_module(event: &LinuxDriverSymbolEventWire) -> String {
    let value = event_text(&event.module, event.module_len);
    if value.is_empty() {
        "<unknown>".to_string()
    } else {
        value
    }
}

fn event_text(bytes: &[u8], len: u16) -> String {
    let len = usize::min(len as usize, bytes.len());
    String::from_utf8_lossy(&bytes[..len]).into_owned()
}

fn aliases_match(record: &DriverRecord) -> bool {
    let mut saw_alias = false;
    for alias in comma_fields(record.aliases.as_str()) {
        saw_alias = true;
        if probe_alias(alias, record.class, record.bus) {
            return true;
        }
    }
    !saw_alias
}

fn requires_service_driver_authorization(record: &DriverRecord) -> bool {
    !record.image_path.ends_with(".ko")
        && matches!(
            record.class,
            DRIVER_CLASS_DISPLAY
                | DRIVER_CLASS_INPUT
                | DRIVER_CLASS_NETWORK
                | DRIVER_CLASS_USB
                | DRIVER_CLASS_STORAGE
        )
}

fn authorize_service_driver(record: &DriverRecord) -> bool {
    if record.provider_group.trim().is_empty() || record.fallback_only {
        return false;
    }
    debug_line(&format!(
        "driverd: service-driver lease granted name={} group={} class={} bus={}",
        record.name, record.provider_group, record.class, record.bus
    ));
    true
}

fn load_module(record: &DriverRecord) -> i64 {
    let policy_flags = driver_load_policy(record);
    let args = RustosDriverLoadModuleBrokerArgs {
        name_ptr: record.name.as_ptr() as u64,
        name_len: record.name.len() as u64,
        class: record.class,
        bus: record.bus,
        path_ptr: record.image_path.as_ptr() as u64,
        path_len: record.image_path.len() as u64,
        linux_driver_names_ptr: record.linux_driver_names.as_ptr() as u64,
        linux_driver_names_len: record.linux_driver_names.len() as u64,
        policy_flags,
        preferred_width: 0,
        preferred_height: 0,
        reserved0: 0,
    };
    syscall1(
        SYS_RUSTOS_DRIVER_LOAD_MODULE_BROKER,
        (&args as *const RustosDriverLoadModuleBrokerArgs) as u64,
    )
}

fn driver_load_policy(record: &DriverRecord) -> u64 {
    let mut flags = 0;
    if record.class == DRIVER_CLASS_DISPLAY {
        if !record.provider_group.is_empty() {
            flags |= DRIVER_LOAD_POLICY_DISPLAY_PRIMARY;
            if record.fallback_only {
                flags |= DRIVER_LOAD_POLICY_DISPLAY_FALLBACK;
            }
        }
    }
    flags
}

fn probe_alias(alias: &str, class: u32, bus: u32) -> bool {
    let args = RustosDriverProbeAliasBrokerArgs {
        alias_ptr: alias.as_ptr() as u64,
        alias_len: alias.len() as u64,
        class,
        bus,
        reserved0: 0,
    };
    syscall1(
        SYS_RUSTOS_DRIVER_PROBE_ALIAS_BROKER,
        (&args as *const RustosDriverProbeAliasBrokerArgs) as u64,
    ) == 1
}

fn comma_fields(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn parse_registry(text: &str) -> Result<Vec<DriverRecord>, &'static str> {
    let mut records = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let name = registry_field(line, "name").ok_or("missing name")?;
        let class_name = registry_field(line, "class").ok_or("missing class")?;
        let bus_name = registry_field(line, "bus").ok_or("missing bus")?;
        let path = registry_field(line, "path").ok_or("missing path")?;
        records.push(DriverRecord {
            name: name.to_string(),
            class: parse_driver_class(class_name).ok_or("invalid class")?,
            bus: parse_driver_bus(bus_name).ok_or("invalid bus")?,
            load_priority: registry_field(line, "priority")
                .and_then(|value| value.parse::<i32>().ok())
                .unwrap_or(0),
            image_path: path.to_string(),
            aliases: registry_field(line, "aliases").unwrap_or("").to_string(),
            deps: registry_field(line, "deps").unwrap_or("").to_string(),
            softdeps: registry_field(line, "softdeps").unwrap_or("").to_string(),
            linux_driver_names: registry_field(line, "linux_driver_names")
                .unwrap_or(name)
                .to_string(),
            provider_group: registry_field(line, "provider_group")
                .unwrap_or("")
                .to_string(),
            fallback_only: registry_field(line, "fallback_only")
                .map(|value| matches!(value, "1" | "true" | "True" | "yes" | "Yes"))
                .unwrap_or(false),
        });
    }
    Ok(records)
}

fn registry_records() -> Result<Vec<DriverRecord>, String> {
    let text = fs::read_to_string(LOADABLE_DRIVER_REGISTRY_PATH)
        .map_err(|err| format!("read failed: {err:?}"))?;
    parse_registry(text.as_str()).map_err(str::to_string)
}

fn registry_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    for token in line.split('\t') {
        let (candidate, value) = token.split_once('=')?;
        if candidate == key {
            return Some(value);
        }
    }
    None
}

fn parse_driver_class(name: &str) -> Option<u32> {
    match name {
        "display" => Some(DRIVER_CLASS_DISPLAY),
        "input" => Some(DRIVER_CLASS_INPUT),
        "network" => Some(DRIVER_CLASS_NETWORK),
        "usb" => Some(rustos_user_abi::syscall::DRIVER_CLASS_USB),
        "storage" => Some(rustos_user_abi::syscall::DRIVER_CLASS_STORAGE),
        _ => None,
    }
}

fn parse_driver_bus(name: &str) -> Option<u32> {
    match name {
        "platform" => Some(DRIVER_BUS_PLATFORM),
        "serio" => Some(DRIVER_BUS_SERIO),
        "usb" => Some(DRIVER_BUS_USB),
        "pci" => Some(DRIVER_BUS_PCI),
        "virtio" => Some(DRIVER_BUS_VIRTIO),
        _ => None,
    }
}

fn serve(endpoint: u64) {
    loop {
        let mut request = CommercialMaxProtocolRequest::default();
        let mut reply_cap = 0_u64;
        let received = syscall4(
            SYS_RUSTOS_IPC_RECV,
            endpoint,
            (&mut request as *mut CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut reply_cap as *mut u64) as u64,
        );
        if received < 0 {
            thread::sleep(RECV_BACKOFF);
            continue;
        }

        if received as usize == size_of::<CommercialMaxProtocolRequest>() {
            let reply = reply_commercial_request(reply_cap, &request);
            if reply < 0 {
                let _ = writeln!(std::io::stderr(), "driverd: reply failed errno={}", -reply);
            }
            continue;
        }

        let request = unsafe {
            &*((&request as *const CommercialMaxProtocolRequest)
                .cast::<LinuxSyscallOffloadRequest>())
        };
        let mut response = LinuxSyscallOffloadResponse {
            op: request.op,
            ..LinuxSyscallOffloadResponse::default()
        };
        response.status = validate_request(received as usize, &request)
            .err()
            .unwrap_or(0);
        let reply = syscall3(
            SYS_RUSTOS_IPC_REPLY,
            reply_cap,
            (&response as *const LinuxSyscallOffloadResponse) as u64,
            size_of::<LinuxSyscallOffloadResponse>() as u64,
        );
        if reply < 0 {
            let _ = writeln!(std::io::stderr(), "driverd: reply failed errno={}", -reply);
        }
    }
}

fn reply_commercial_request(reply_cap: u64, request: &CommercialMaxProtocolRequest) -> i64 {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = match validate_commercial_request(request) {
        Ok(()) => dispatch_commercial_request(request, &mut response),
        Err(errno) => errno,
    };
    syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    )
}

fn validate_request(received: usize, request: &LinuxSyscallOffloadRequest) -> Result<(), i32> {
    if received != size_of::<LinuxSyscallOffloadRequest>()
        || request.version != SYSCALL_OFFLOAD_ABI_VERSION
        || request.reserved0 != 0
        || request.path_len as usize > SYSCALL_OFFLOAD_PATH_CAPACITY
    {
        return Err(libc::EINVAL);
    }
    match request.op {
        SYSCALL_OFFLOAD_OP_DRIVER_LOAD_POLICY => Ok(()),
        _ => Err(libc::EINVAL),
    }
}

fn validate_commercial_request(request: &CommercialMaxProtocolRequest) -> Result<(), i32> {
    if request.header.version != COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
        || request.path_len as usize > request.path.len()
        || request.payload_len as usize > request.payload.len()
    {
        return Err(libc::EINVAL);
    }
    match request.header.protocol {
        COMMERCIAL_MAX_PROTOCOL_DRIVERD => match request.header.op {
            COMMERCIAL_MAX_DRIVERD_OP_DRIVER_PLAN
            | COMMERCIAL_MAX_DRIVERD_OP_MODULE_LOAD_AUTHORIZE
            | COMMERCIAL_MAX_DRIVERD_OP_SYMBOL_POLICY
            | COMMERCIAL_MAX_DRIVERD_OP_PROVIDER_SELECT
            | COMMERCIAL_MAX_DRIVERD_OP_RETRY_BUDGET
            | COMMERCIAL_MAX_DRIVERD_OP_FALLBACK_POLICY => Ok(()),
            _ => Err(libc::EINVAL),
        },
        COMMERCIAL_MAX_PROTOCOL_SERVICE_DRIVERD => match request.header.op {
            COMMERCIAL_MAX_SERVICE_DRIVERD_OP_DRIVER_INSTANCE => Ok(()),
            COMMERCIAL_MAX_SERVICE_DRIVERD_OP_MMIO_LEASE => {
                if request.arg0 == 0
                    || request.arg1 == 0
                    || request.header.subject_pid == 0
                    || request.header.subject_tid == 0
                    || request.path_len == 0
                {
                    Err(libc::EINVAL)
                } else {
                    Ok(())
                }
            }
            COMMERCIAL_MAX_SERVICE_DRIVERD_OP_IRQ_ROUTE => {
                if request.arg0 > u32::MAX as u64
                    || request.arg1 > u32::MAX as u64
                    || request.header.subject_pid == 0
                    || request.header.subject_tid == 0
                    || request.path_len == 0
                {
                    Err(libc::EINVAL)
                } else {
                    Ok(())
                }
            }
            COMMERCIAL_MAX_SERVICE_DRIVERD_OP_DMA_BUFFER => {
                if request.arg0 == 0
                    || request.arg1 != 0 && !request.arg1.is_power_of_two()
                    || request.header.subject_pid == 0
                    || request.header.subject_tid == 0
                    || request.path_len == 0
                {
                    Err(libc::EINVAL)
                } else {
                    Ok(())
                }
            }
            COMMERCIAL_MAX_SERVICE_DRIVERD_OP_IO_PORT_LEASE => {
                if request.arg0 > u16::MAX as u64
                    || request.arg1 == 0
                    || request.arg1 > u16::MAX as u64
                    || request.arg0.saturating_add(request.arg1 - 1) > u16::MAX as u64
                    || request.header.subject_pid == 0
                    || request.header.subject_tid == 0
                    || request.path_len == 0
                {
                    Err(libc::EINVAL)
                } else {
                    Ok(())
                }
            }
            _ => Err(libc::EINVAL),
        },
        _ => Err(libc::EINVAL),
    }
}

fn dispatch_commercial_request(
    request: &CommercialMaxProtocolRequest,
    response: &mut CommercialMaxProtocolResponse,
) -> i32 {
    if request.header.protocol == COMMERCIAL_MAX_PROTOCOL_SERVICE_DRIVERD {
        return dispatch_service_driver_request(request, response);
    }
    let records = match registry_records() {
        Ok(records) => records,
        Err(_) => return libc::ENOENT,
    };
    match request.header.op {
        COMMERCIAL_MAX_DRIVERD_OP_DRIVER_PLAN => {
            fill_driver_descriptors(response, records.iter());
            0
        }
        COMMERCIAL_MAX_DRIVERD_OP_MODULE_LOAD_AUTHORIZE => {
            let path = match commercial_request_path(request) {
                Ok(path) => path,
                Err(errno) => return errno,
            };
            if let Some(record) = records.iter().find(|record| record.image_path == path) {
                response.descriptor_count = 1;
                response.value0 = record.class as u64;
                response.value1 = record.bus as u64;
                response.descriptors[0] = driver_descriptor(record);
                response.capability = driver_capability(record.name.as_str(), request.header.op);
                0
            } else {
                libc::EACCES
            }
        }
        COMMERCIAL_MAX_DRIVERD_OP_SYMBOL_POLICY => {
            fill_driver_descriptors(response, records.iter());
            response.value0 = symbol_event_count();
            response.capability = driver_capability("symbol-policy", request.header.op);
            0
        }
        COMMERCIAL_MAX_DRIVERD_OP_PROVIDER_SELECT => {
            let provider_records = records
                .iter()
                .filter(|record| !record.provider_group.is_empty());
            fill_driver_descriptors(response, provider_records);
            response.capability = driver_capability("provider-select", request.header.op);
            0
        }
        COMMERCIAL_MAX_DRIVERD_OP_RETRY_BUDGET => {
            fill_driver_descriptors(response, records.iter());
            response.value1 = 1;
            0
        }
        COMMERCIAL_MAX_DRIVERD_OP_FALLBACK_POLICY => {
            let fallback_records = records.iter().filter(|record| record.fallback_only);
            fill_driver_descriptors(response, fallback_records);
            response.capability = driver_capability("fallback-policy", request.header.op);
            0
        }
        _ => libc::EINVAL,
    }
}

fn dispatch_service_driver_request(
    request: &CommercialMaxProtocolRequest,
    response: &mut CommercialMaxProtocolResponse,
) -> i32 {
    if request.header.op != COMMERCIAL_MAX_SERVICE_DRIVERD_OP_DRIVER_INSTANCE
        && authorized_service_driver_request(request).is_err()
    {
        return libc::EACCES;
    }
    match request.header.op {
        COMMERCIAL_MAX_SERVICE_DRIVERD_OP_DRIVER_INSTANCE => {
            match registry_records() {
                Ok(records) => fill_driver_descriptors(response, records.iter()),
                Err(_) => return libc::ENOENT,
            }
            response.capability = service_driver_capability("driver-instance", request.header.op);
            0
        }
        COMMERCIAL_MAX_SERVICE_DRIVERD_OP_MMIO_LEASE => {
            response.descriptor_count = 1;
            response.descriptors[0] = service_driver_descriptor(
                "mmio-lease",
                request.header.op,
                request.arg0,
                request.arg1,
            );
            response.capability = service_driver_capability("mmio-lease", request.header.op);
            fill_service_driver_resource_payload(
                request,
                SERVICE_DRIVER_RESOURCE_OP_MMIO_LEASE,
                response,
            )
        }
        COMMERCIAL_MAX_SERVICE_DRIVERD_OP_IRQ_ROUTE => {
            response.descriptor_count = 1;
            response.descriptors[0] = service_driver_descriptor(
                "irq-route",
                request.header.op,
                request.arg0,
                request.arg1,
            );
            response.capability = service_driver_capability("irq-route", request.header.op);
            fill_service_driver_resource_payload(
                request,
                SERVICE_DRIVER_RESOURCE_OP_IRQ_ROUTE,
                response,
            )
        }
        COMMERCIAL_MAX_SERVICE_DRIVERD_OP_DMA_BUFFER => {
            response.descriptor_count = 1;
            response.descriptors[0] = service_driver_descriptor(
                "dma-buffer",
                request.header.op,
                request.arg0,
                request.arg1,
            );
            response.capability = service_driver_capability("dma-buffer", request.header.op);
            fill_service_driver_resource_payload(
                request,
                SERVICE_DRIVER_RESOURCE_OP_DMA_BUFFER,
                response,
            )
        }
        COMMERCIAL_MAX_SERVICE_DRIVERD_OP_IO_PORT_LEASE => {
            response.descriptor_count = 1;
            response.descriptors[0] = service_driver_descriptor(
                "io-port-lease",
                request.header.op,
                request.arg0,
                request.arg1,
            );
            response.capability = service_driver_capability("io-port-lease", request.header.op);
            fill_service_driver_resource_payload(
                request,
                SERVICE_DRIVER_RESOURCE_OP_IO_PORT_LEASE,
                response,
            )
        }
        _ => libc::EINVAL,
    }
}

fn authorized_service_driver_request(request: &CommercialMaxProtocolRequest) -> Result<(), i32> {
    let path = commercial_request_path(request)?;
    let records = registry_records().map_err(|_| libc::ENOENT)?;
    let Some(record) = records.iter().find(|record| record.image_path == path) else {
        return Err(libc::EACCES);
    };
    if requires_service_driver_authorization(record) && authorize_service_driver(record) {
        Ok(())
    } else {
        Err(libc::EACCES)
    }
}

fn fill_service_driver_resource_payload(
    request: &CommercialMaxProtocolRequest,
    broker_op: u16,
    response: &mut CommercialMaxProtocolResponse,
) -> i32 {
    let args = RustosServiceDriverResourceBrokerArgs {
        abi_version: SERVICE_DRIVER_RESOURCE_BROKER_ABI_VERSION,
        op: broker_op,
        flags: request.header.flags as u32,
        subject_pid: request.header.subject_pid,
        subject_tid: request.header.subject_tid,
        arg0: request.arg0,
        arg1: request.arg1,
        arg2: 0,
        out_ptr: response.payload.as_mut_ptr() as u64,
        out_len: response.payload.len() as u64,
        reserved0: 0,
    };
    let status = syscall1(
        SYS_RUSTOS_SERVICE_DRIVER_RESOURCE_BROKER,
        (&args as *const RustosServiceDriverResourceBrokerArgs) as u64,
    );
    if status < 0 {
        return (-status) as i32;
    }
    response.payload_len = service_driver_resource_payload_len(broker_op);
    0
}

fn service_driver_resource_payload_len(op: u16) -> u32 {
    match op {
        SERVICE_DRIVER_RESOURCE_OP_MMIO_LEASE => {
            size_of::<rustos_user_abi::syscall::ServiceDriverMmioLeaseWire>() as u32
        }
        SERVICE_DRIVER_RESOURCE_OP_IRQ_ROUTE => {
            size_of::<rustos_user_abi::syscall::ServiceDriverIrqRouteWire>() as u32
        }
        SERVICE_DRIVER_RESOURCE_OP_DMA_BUFFER => {
            size_of::<rustos_user_abi::syscall::ServiceDriverDmaBufferWire>() as u32
        }
        SERVICE_DRIVER_RESOURCE_OP_IO_PORT_LEASE => {
            size_of::<rustos_user_abi::syscall::ServiceDriverIoPortLeaseWire>() as u32
        }
        _ => 0,
    }
}

fn fill_driver_descriptors<'a, I>(response: &mut CommercialMaxProtocolResponse, records: I)
where
    I: Iterator<Item = &'a DriverRecord>,
{
    let mut total = 0_u64;
    for (index, record) in records.enumerate() {
        total += 1;
        if index < COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS {
            response.descriptors[index] = driver_descriptor(record);
            response.descriptor_count = (index + 1) as u16;
        }
    }
    response.value0 = total;
}

fn driver_descriptor(record: &DriverRecord) -> CommercialMaxProtocolDescriptorWire {
    let mut descriptor = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_DRIVERD,
        op: COMMERCIAL_MAX_DRIVERD_OP_DRIVER_PLAN,
        flags: u32::from(record.fallback_only),
        service_id: IPC_SERVICE_DRIVERD,
        capability_mask: driver_capability_mask(COMMERCIAL_MAX_DRIVERD_OP_DRIVER_PLAN),
        value0: record.class as u64,
        value1: record.bus as u64,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    copy_label(
        record.name.as_str(),
        &mut descriptor.name,
        &mut descriptor.name_len,
    );
    descriptor
}

fn driver_capability(label: &str, op: u16) -> CommercialMaxCapabilityLeaseWire {
    let mut capability = CommercialMaxCapabilityLeaseWire {
        lease_id: ((COMMERCIAL_MAX_PROTOCOL_DRIVERD as u64) << 32) | u64::from(op),
        service_id: IPC_SERVICE_DRIVERD,
        capability_mask: driver_capability_mask(op),
        rights_mask: driver_capability_mask(op),
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    copy_label(label, &mut capability.label, &mut capability.label_len);
    capability
}

fn driver_capability_mask(op: u16) -> u64 {
    match op {
        COMMERCIAL_MAX_DRIVERD_OP_DRIVER_PLAN => 1 << 0,
        COMMERCIAL_MAX_DRIVERD_OP_MODULE_LOAD_AUTHORIZE => 1 << 1,
        COMMERCIAL_MAX_DRIVERD_OP_SYMBOL_POLICY => 1 << 2,
        COMMERCIAL_MAX_DRIVERD_OP_PROVIDER_SELECT => 1 << 3,
        COMMERCIAL_MAX_DRIVERD_OP_RETRY_BUDGET => 1 << 4,
        COMMERCIAL_MAX_DRIVERD_OP_FALLBACK_POLICY => 1 << 5,
        _ => 0,
    }
}

fn service_driver_descriptor(
    label: &str,
    op: u16,
    value0: u64,
    value1: u64,
) -> CommercialMaxProtocolDescriptorWire {
    let mut descriptor = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_SERVICE_DRIVERD,
        op,
        flags: 0,
        service_id: IPC_SERVICE_SERVICE_DRIVERD,
        capability_mask: service_driver_capability_mask(op),
        value0,
        value1,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    copy_label(label, &mut descriptor.name, &mut descriptor.name_len);
    descriptor
}

fn service_driver_capability(label: &str, op: u16) -> CommercialMaxCapabilityLeaseWire {
    let mut capability = CommercialMaxCapabilityLeaseWire {
        lease_id: ((COMMERCIAL_MAX_PROTOCOL_SERVICE_DRIVERD as u64) << 32) | u64::from(op),
        service_id: IPC_SERVICE_SERVICE_DRIVERD,
        capability_mask: service_driver_capability_mask(op),
        rights_mask: service_driver_capability_mask(op),
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    copy_label(label, &mut capability.label, &mut capability.label_len);
    capability
}

fn service_driver_capability_mask(op: u16) -> u64 {
    match op {
        COMMERCIAL_MAX_SERVICE_DRIVERD_OP_DRIVER_INSTANCE => 1 << 0,
        COMMERCIAL_MAX_SERVICE_DRIVERD_OP_MMIO_LEASE => 1 << 1,
        COMMERCIAL_MAX_SERVICE_DRIVERD_OP_IRQ_ROUTE => 1 << 2,
        COMMERCIAL_MAX_SERVICE_DRIVERD_OP_DMA_BUFFER => 1 << 3,
        COMMERCIAL_MAX_SERVICE_DRIVERD_OP_IO_PORT_LEASE => 1 << 4,
        _ => 0,
    }
}

fn commercial_request_path(request: &CommercialMaxProtocolRequest) -> Result<&str, i32> {
    let len = request.path_len as usize;
    std::str::from_utf8(&request.path[..len]).map_err(|_| libc::EINVAL)
}

fn copy_label(label: &str, target: &mut [u8], len: &mut u16) {
    let bytes = label.as_bytes();
    let count = bytes.len().min(target.len());
    target[..count].copy_from_slice(&bytes[..count]);
    *len = count as u16;
}

fn syscall0(number: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long) as i64 }
}

fn syscall1(number: u64, arg0: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0) as i64 }
}

fn syscall2(number: u64, arg0: u64, arg1: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1) as i64 }
}

fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2) as i64 }
}

fn syscall4(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2, arg3) as i64 }
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn debug_line(message: &str) {
    let _ = syscall2(
        SYS_RUSTOS_DEBUG_PRINT,
        message.as_ptr() as u64,
        message.len() as u64,
    );
    let _ = syscall2(SYS_RUSTOS_DEBUG_PRINT, b"\n".as_ptr() as u64, 1);
}
