use std::io::Write;
use std::mem::size_of;
use std::{collections::BTreeSet, fs, thread, time::Duration};

use rustos_user_abi::syscall::{
    LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse, RustosDriverLoadModuleBrokerArgs,
    RustosDriverProbeAliasBrokerArgs, RustosDriverProviderActiveBrokerArgs, DRIVER_BUS_PCI,
    DRIVER_BUS_PLATFORM, DRIVER_BUS_SERIO, DRIVER_BUS_USB, DRIVER_BUS_VIRTIO, DRIVER_CLASS_DISPLAY,
    DRIVER_CLASS_INPUT, DRIVER_CLASS_NETWORK, IPC_SERVICE_DRIVERD, SYSCALL_OFFLOAD_ABI_VERSION,
    SYSCALL_OFFLOAD_OP_DRIVER_LOAD_POLICY, SYSCALL_OFFLOAD_PATH_CAPACITY, SYS_RUSTOS_DEBUG_PRINT,
    SYS_RUSTOS_DRIVER_LOAD_MODULE_BROKER, SYS_RUSTOS_DRIVER_PROBE_ALIAS_BROKER,
    SYS_RUSTOS_DRIVER_PROVIDER_ACTIVE_BROKER, SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV,
    SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REPLY,
};

const RECV_BACKOFF: Duration = Duration::from_millis(10);
const LOADABLE_DRIVER_REGISTRY_PATH: &str = "system/registry/kernel/loadable-drivers.tsv";

fn main() {
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

    debug_line("driverd: driver policy endpoint registered");
    autoload_from_registry();
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
    let text = match fs::read_to_string(LOADABLE_DRIVER_REGISTRY_PATH) {
        Ok(text) => text,
        Err(err) => {
            debug_line(&format!("driverd: registry read failed error={err:?}"));
            return;
        }
    };
    let mut records = match parse_registry(text.as_str()) {
        Ok(records) => records,
        Err(err) => {
            debug_line(&format!("driverd: registry parse failed error={err}"));
            return;
        }
    };
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
    let mut loaded = BTreeSet::new();
    let mut skipped = BTreeSet::new();
    let mut provider_groups = BTreeSet::new();
    let mut pending = records;
    while !pending.is_empty() {
        let mut progress = false;
        let mut deferred = Vec::new();
        for record in pending.into_iter() {
            match load_record(
                &record,
                &known_names,
                &mut loaded,
                &mut skipped,
                &mut provider_groups,
            ) {
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
    if loaded.contains(record.name.as_str()) || skipped.contains(record.name.as_str()) {
        return LoadResult::Progress;
    }
    for dep in comma_fields(record.deps.as_str()) {
        if skipped.contains(dep) {
            skipped.insert(record.name.clone());
            debug_line(&format!(
                "driverd: skipped name={} reason=dependency skipped dep={}",
                record.name, dep
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
    if !record.provider_group.is_empty()
        && (provider_groups.contains(record.provider_group.as_str())
            || provider_group_active(record.provider_group.as_str()))
    {
        skipped.insert(record.name.clone());
        debug_line(&format!(
            "driverd: skipped name={} reason=provider active group={}",
            record.name, record.provider_group
        ));
        return LoadResult::Progress;
    }
    if !aliases_match(record) {
        skipped.insert(record.name.clone());
        debug_line(&format!(
            "driverd: skipped name={} reason=no matching alias aliases={}",
            record.name, record.aliases
        ));
        return LoadResult::Progress;
    }
    let result = load_module(record);
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
        debug_line(&format!(
            "driverd: load failed name={} path={} errno={}",
            record.name, record.image_path, -result
        ));
    }
    LoadResult::Progress
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

fn load_module(record: &DriverRecord) -> i64 {
    let args = RustosDriverLoadModuleBrokerArgs {
        name_ptr: record.name.as_ptr() as u64,
        name_len: record.name.len() as u64,
        class: record.class,
        bus: record.bus,
        path_ptr: record.image_path.as_ptr() as u64,
        path_len: record.image_path.len() as u64,
        linux_driver_names_ptr: record.linux_driver_names.as_ptr() as u64,
        linux_driver_names_len: record.linux_driver_names.len() as u64,
        reserved0: 0,
    };
    syscall1(
        SYS_RUSTOS_DRIVER_LOAD_MODULE_BROKER,
        (&args as *const RustosDriverLoadModuleBrokerArgs) as u64,
    )
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

fn provider_group_active(provider_group: &str) -> bool {
    let args = RustosDriverProviderActiveBrokerArgs {
        provider_group_ptr: provider_group.as_ptr() as u64,
        provider_group_len: provider_group.len() as u64,
        reserved0: 0,
    };
    syscall1(
        SYS_RUSTOS_DRIVER_PROVIDER_ACTIVE_BROKER,
        (&args as *const RustosDriverProviderActiveBrokerArgs) as u64,
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
        let mut request = LinuxSyscallOffloadRequest::default();
        let mut reply_cap = 0_u64;
        let received = syscall4(
            SYS_RUSTOS_IPC_RECV,
            endpoint,
            (&mut request as *mut LinuxSyscallOffloadRequest) as u64,
            size_of::<LinuxSyscallOffloadRequest>() as u64,
            (&mut reply_cap as *mut u64) as u64,
        );
        if received < 0 {
            thread::sleep(RECV_BACKOFF);
            continue;
        }

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

fn debug_line(message: &str) {
    let _ = syscall2(
        SYS_RUSTOS_DEBUG_PRINT,
        message.as_ptr() as u64,
        message.len() as u64,
    );
    let _ = syscall2(SYS_RUSTOS_DEBUG_PRINT, b"\n".as_ptr() as u64, 1);
}
