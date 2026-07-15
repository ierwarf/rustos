use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use runtime_control::{
    load_runtime_launch_program_entries, DesktopProgramEntry, StartupMode,
    DEFAULT_RUNTIME_LAUNCH_REGISTRY_PATH,
};

use super::{boot_line, debug_line, RETRY_BACKOFF, UI_SERVER_EXEC_PATH};
use super::{BrokerState, LaunchEntry, ProgramMetadata};

pub(super) fn load_launch_catalog_into_state(state: &mut BrokerState) -> bool {
    if state.launch_catalog_loaded {
        return false;
    }
    if state
        .launch_catalog_retry_after
        .is_some_and(|retry_after| Instant::now() < retry_after)
    {
        return false;
    }
    debug_line("runtimed: launch catalog load begin");
    boot_line("runtimed: launch catalog load begin");
    let started_at = Instant::now();
    let (programs, launch_entries) = match load_launch_catalog() {
        Ok(catalog) => catalog,
        Err(errno) => {
            state.launch_catalog_retry_after = Some(Instant::now() + RETRY_BACKOFF);
            if state.launch_catalog_last_error != Some(errno) {
                debug_line(format!("runtimed: launch catalog load failed errno={errno}").as_str());
                observability_client::warn!(
                    "runtimed",
                    service,
                    "launch catalog load failed: errno={errno}; retrying"
                );
                state.launch_catalog_last_error = Some(errno);
            }
            return false;
        }
    };
    let elapsed_ms = started_at.elapsed().as_millis();
    observability_client::info!(
        "runtimed",
        service,
        "launch catalog summary programs={} policies={} elapsed_ms={}",
        programs.len(),
        launch_entries.len(),
        elapsed_ms
    );
    boot_line(
        format!(
            "runtimed: launch catalog summary programs={} policies={} elapsed_ms={}",
            programs.len(),
            launch_entries.len(),
            elapsed_ms
        )
        .as_str(),
    );
    debug_line(
        format!(
            "runtimed: launch catalog summary programs={} policies={}",
            programs.len(),
            launch_entries.len()
        )
        .as_str(),
    );
    state.programs = programs;
    state.launch_entries = launch_entries;
    state.launch_catalog_loaded = true;
    state.launch_catalog_retry_after = None;
    state.launch_catalog_last_error = None;
    debug_line("runtimed: launch catalog load done");
    boot_line("runtimed: launch catalog load done");
    true
}

pub(super) fn load_launch_catalog(
) -> Result<(BTreeMap<String, ProgramMetadata>, Vec<LaunchEntry>), i32> {
    let registry_entries =
        load_runtime_launch_program_entries(DEFAULT_RUNTIME_LAUNCH_REGISTRY_PATH)
            .map_err(runtime_registry_errno)?;
    debug_line(
        format!(
            "runtimed: launch registry snapshot entries={}",
            registry_entries.len()
        )
        .as_str(),
    );

    let mut programs = BTreeMap::new();
    for entry in registry_entries.iter().cloned() {
        insert_program_metadata(&mut programs, entry);
    }
    let autostart_entries = registry_entries
        .iter()
        .filter(|entry| entry.autostart_enabled && !entry.hidden && !entry.no_display)
        .cloned()
        .collect::<Vec<_>>();

    let launch_started = Instant::now();
    let launch_entries = load_launch_entries(&programs, autostart_entries)?;
    let launch_elapsed = launch_started.elapsed().as_millis();
    boot_line(
        format!(
            "runtimed: launch policies={} elapsed_ms={}",
            launch_entries.len(),
            launch_elapsed
        )
        .as_str(),
    );

    Ok((programs, launch_entries))
}

fn load_launch_entries(
    programs: &BTreeMap<String, ProgramMetadata>,
    autostart_entries: Vec<DesktopProgramEntry>,
) -> Result<Vec<LaunchEntry>, i32> {
    let mut seen = BTreeSet::<String>::new();
    let mut entries = Vec::<LaunchEntry>::new();

    for metadata in programs.values() {
        if !matches!(
            metadata.startup,
            StartupMode::Session | StartupMode::Desktop
        ) {
            continue;
        }
        if !seen.insert(metadata.desktop_file_id.clone()) {
            continue;
        }
        entries.push(LaunchEntry {
            package_id: metadata.package_id.clone(),
            desktop_file_id: metadata.desktop_file_id.clone(),
            display_name: metadata.display_name.clone(),
            exec: metadata.exec.clone(),
            runtime_deps: metadata.runtime_deps.clone(),
            restart: metadata.exec.starts_with("services/"),
            weight_micros: metadata.weight_micros,
            logical_admin: metadata.logical_admin,
            console_hosted: metadata.console_hosted,
            args: metadata.args.clone(),
            env: metadata.env.clone(),
        });
    }

    for entry in autostart_entries {
        if !seen.insert(entry.desktop_file_id.clone()) {
            continue;
        }
        let metadata = programs
            .get(entry.desktop_file_id.as_str())
            .cloned()
            .ok_or(libc::EINVAL)?;
        entries.push(LaunchEntry {
            package_id: metadata.package_id,
            desktop_file_id: metadata.desktop_file_id,
            display_name: metadata.display_name,
            exec: metadata.exec.clone(),
            runtime_deps: metadata.runtime_deps,
            restart: metadata.exec.starts_with("services/"),
            weight_micros: metadata.weight_micros,
            logical_admin: metadata.logical_admin,
            console_hosted: metadata.console_hosted,
            args: metadata.args,
            env: metadata.env,
        });
    }

    entries.sort_by(|lhs, rhs| {
        launch_entry_priority(lhs)
            .cmp(&launch_entry_priority(rhs))
            .then_with(|| lhs.desktop_file_id.cmp(&rhs.desktop_file_id))
            .then_with(|| lhs.display_name.cmp(&rhs.display_name))
            .then_with(|| lhs.exec.cmp(&rhs.exec))
    });

    Ok(entries)
}

fn launch_entry_priority(entry: &LaunchEntry) -> (u8, u8, &str) {
    let service_rank = if entry.exec == UI_SERVER_EXEC_PATH {
        0
    } else if entry.exec.starts_with("services/") {
        3
    } else if entry.console_hosted {
        2
    } else {
        1
    };
    let restart_rank = u8::from(entry.restart);
    (service_rank, restart_rank, entry.desktop_file_id.as_str())
}

fn insert_program_metadata(
    map: &mut BTreeMap<String, ProgramMetadata>,
    entry: DesktopProgramEntry,
) {
    let key = entry.desktop_file_id.clone();
    map.entry(key)
        .or_insert_with(|| program_metadata_from_desktop_entry(entry));
}

fn program_metadata_from_desktop_entry(entry: DesktopProgramEntry) -> ProgramMetadata {
    ProgramMetadata {
        package_id: entry.package_id,
        desktop_file_id: entry.desktop_file_id,
        display_name: entry.display_name,
        exec: entry.exec,
        runtime_deps: entry.runtime_deps,
        startup: entry.startup,
        weight_micros: entry.weight_micros,
        logical_admin: entry.logical_admin,
        console_hosted: entry.console_hosted,
        args: entry.args,
        env: entry.env,
    }
}

pub(super) fn resolve_program_request(
    state: &BrokerState,
    target: &str,
) -> Result<ProgramMetadata, i32> {
    state
        .programs
        .get(target)
        .cloned()
        .or_else(|| {
            state
                .programs
                .values()
                .find(|program| program.exec == target)
                .cloned()
        })
        .ok_or(libc::ENOENT)
}

fn runtime_registry_errno(error: std::io::Error) -> i32 {
    match error.raw_os_error() {
        Some(errno) if errno > 0 => errno,
        _ => libc::EIO,
    }
}

pub(super) fn running_packages(state: &BrokerState) -> std::collections::BTreeSet<String> {
    state
        .running
        .values()
        .map(|program| program.package_id.clone())
        .collect()
}

pub(super) fn runtime_deps_satisfied(
    deps: &[String],
    running_packages: &BTreeSet<String>,
    launched_once_packages: &BTreeSet<String>,
) -> bool {
    deps.iter()
        .all(|dep| running_packages.contains(dep) || launched_once_packages.contains(dep))
}
