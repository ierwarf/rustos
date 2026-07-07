use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use runtime_control::{
    load_desktop_program_entries, load_runtime_launch_program_entries, DesktopProgramEntry,
    StartupMode, DEFAULT_APPLICATIONS_DIR, DEFAULT_RUNTIME_LAUNCH_REGISTRY_PATH,
};

use super::{boot_line, DEFAULT_USER_TASK_WEIGHT_MICROS, UI_SERVER_EXEC_PATH};
use super::{BrokerState, LaunchEntry, ProgramMetadata};

pub(super) fn load_launch_catalog_into_state(state: &mut BrokerState) -> bool {
    if state.launch_catalog_loaded {
        return false;
    }
    boot_line("runtimed: launch catalog load begin");
    let started_at = Instant::now();
    let (programs, launch_entries) = load_launch_catalog();
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
    state.programs = programs;
    state.launch_entries = launch_entries;
    state.launch_catalog_loaded = true;
    boot_line("runtimed: launch catalog load done");
    true
}

pub(super) fn load_launch_catalog() -> (BTreeMap<String, ProgramMetadata>, Vec<LaunchEntry>) {
    let load_started = Instant::now();
    let registry_entries =
        load_runtime_launch_program_entries(DEFAULT_RUNTIME_LAUNCH_REGISTRY_PATH)
            .unwrap_or_default();
    let registry_elapsed = load_started.elapsed().as_millis();
    boot_line(
        format!(
            "runtimed: launch registry entries={} elapsed_ms={}",
            registry_entries.len(),
            registry_elapsed
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
    let launch_entries = load_launch_entries(&programs, autostart_entries);
    let launch_elapsed = launch_started.elapsed().as_millis();
    boot_line(
        format!(
            "runtimed: launch policies={} elapsed_ms={}",
            launch_entries.len(),
            launch_elapsed
        )
        .as_str(),
    );

    (programs, launch_entries)
}

fn load_launch_entries(
    programs: &BTreeMap<String, ProgramMetadata>,
    autostart_entries: Vec<DesktopProgramEntry>,
) -> Vec<LaunchEntry> {
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
            .unwrap_or_else(|| program_metadata_from_desktop_entry(entry.clone()));
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

    entries
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

fn load_program_metadata() -> BTreeMap<String, ProgramMetadata> {
    let mut map = BTreeMap::new();
    if let Ok(entries) = load_desktop_program_entries(DEFAULT_APPLICATIONS_DIR) {
        for entry in entries {
            insert_program_metadata(&mut map, entry);
        }
    }
    map
}

fn load_program_metadata_for_target(target: &str) -> Option<ProgramMetadata> {
    let mut programs = load_program_metadata();
    programs.remove(target).or_else(|| {
        programs
            .into_values()
            .find(|program| program.exec == target)
    })
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
        display_name: if entry.display_name.is_empty() {
            fallback_display_name(entry.exec.as_str())
        } else {
            entry.display_name
        },
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

pub(super) fn resolve_program_request(state: &BrokerState, target: &str) -> ProgramMetadata {
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
        .or_else(|| load_program_metadata_for_target(target))
        .unwrap_or_else(|| ProgramMetadata {
            package_id: package_id_from_target(target),
            desktop_file_id: target.to_string(),
            display_name: fallback_display_name(target),
            exec: target.to_string(),
            runtime_deps: Vec::new(),
            startup: StartupMode::None,
            weight_micros: DEFAULT_USER_TASK_WEIGHT_MICROS,
            logical_admin: false,
            console_hosted: false,
            args: Vec::new(),
            env: Vec::new(),
        })
}

fn fallback_display_name(exec: &str) -> String {
    std::path::Path::new(exec)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or(exec)
        .to_string()
}

fn package_id_from_target(target: &str) -> String {
    std::path::Path::new(target)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or(target)
        .strip_suffix(".desktop")
        .unwrap_or_else(|| {
            std::path::Path::new(target)
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or(target)
        })
        .to_string()
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
