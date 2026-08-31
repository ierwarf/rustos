use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use runtime_control::{
    load_runtime_launch_program_entries, DesktopProgramEntry, StartupMode,
    DEFAULT_RUNTIME_LAUNCH_REGISTRY_PATH,
};

use super::{
    boot_line, debug_line, RETRY_BACKOFF, UI_SERVER_BOOTSTRAP_ENV, UI_SERVER_CATALOG_WEIGHT_MICROS,
    UI_SERVER_DESKTOP_FILE_ID, UI_SERVER_EXEC_PATH,
};
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
    let started_at = Instant::now();
    // The registry is read off the loop for the same reason as the
    // qualification contract: it is storage, it measured 70 ms here, and the
    // loop is the console's only receiver. The policy applied to the result is
    // unchanged and still applied here.
    let Some(loaded) = request_launch_catalog(state) else {
        return false;
    };
    let (programs, launch_entries) = match loaded {
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

/// The launch catalog as the worker returns it.
pub(crate) type LaunchCatalogLoad =
    Result<(BTreeMap<String, ProgramMetadata>, Vec<LaunchEntry>), i32>;

/// The worker's entry point. A plain function so the offload worker can hold it
/// without borrowing anything the loop owns.
pub(crate) fn load_launch_catalog_off_loop() -> LaunchCatalogLoad {
    load_launch_catalog()
}

/// The catalog read's answer, if one is in hand; `None` while it is
/// outstanding. Falls back to reading inline when there is no worker, so a
/// broker that could not create a thread still boots.
fn request_launch_catalog(state: &mut BrokerState) -> Option<LaunchCatalogLoad> {
    let Some(load) = state.launch_catalog_load.as_ref() else {
        announce_launch_catalog_load_begin();
        return Some(load_launch_catalog());
    };
    if let Some(loaded) = load.poll() {
        return Some(loaded);
    }
    if !load.busy() {
        announce_launch_catalog_load_begin();
        load.request();
    }
    None
}

/// Announces one *dispatched* catalog load.
///
/// This used to run once per call of `load_launch_catalog_into_state`, which
/// the broker loop calls on every pass while an off-loop load is still in
/// flight. A slow storage read therefore did not produce one begin line, it
/// produced one per pass: a single 30 s window logged 2,204 of them, 4,408
/// debugcon lines in total. Each line is a synchronous port write taken under
/// a global lock with interrupts disabled, so the diagnostic became the
/// dominant machine-wide stall and extended the very read it was reporting on.
/// A poll of work already in flight is not a new attempt and says nothing new.
fn announce_launch_catalog_load_begin() {
    debug_line("runtimed: launch catalog load begin");
    boot_line("runtimed: launch catalog load begin");
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
    validate_ui_bootstrap_metadata(&programs)?;
    let autostart_entries = registry_entries
        .iter()
        .filter(|entry| entry.autostart_enabled && !entry.hidden && !entry.no_display)
        .cloned()
        .collect::<Vec<_>>();

    let launch_started = Instant::now();
    let mut launch_entries = load_launch_entries(&programs, autostart_entries)?;
    sort_launch_entries(&mut launch_entries);
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

/// Reconciles the private, DVM-volume qualification policy after the signed
/// early-system catalog has committed. A transient storage failure never
/// publishes a partial policy and never rolls back ordinary launch progress.
pub(super) fn reconcile_kvm_smp_qualification_into_state(state: &mut BrokerState) -> bool {
    if !state.launch_catalog_loaded || state.qualification_catalog_resolved {
        return false;
    }
    if state
        .qualification_catalog_retry_after
        .is_some_and(|retry_after| Instant::now() < retry_after)
    {
        return false;
    }

    // The contract lives on a DVM volume, so reading it is a storage round
    // trip - measured at 104 ms, which the broker loop paid in full while every
    // console caller waited for the pass to end. Ask the worker and come back
    // for the answer; the policy decisions below are unchanged and still made
    // here.
    let Some(loaded) = request_qualification_contract(state) else {
        return false;
    };
    let candidate = match qualification_catalog_candidate(&state.launch_entries, loaded) {
        Ok(candidate) => candidate,
        Err(errno) => return defer_qualification_catalog_retry(state, errno),
    };
    let injected = candidate.len() != state.launch_entries.len();
    state.launch_entries = candidate;
    state.qualification_catalog_resolved = true;
    state.qualification_catalog_retry_after = None;
    state.qualification_catalog_last_error = None;
    state.qualification_catalog_failures = 0;
    if injected {
        debug_line("runtimed: private SMP qualification policy injected");
        boot_line("runtimed: private SMP qualification policy injected");
    }
    true
}

/// The contract read's answer, if one is in hand.
///
/// Returns `None` while the read is outstanding - the caller simply comes back
/// next pass - and performs the read inline only when there is no worker to do
/// it, which keeps a broker that could not create a thread working rather than
/// stuck.
fn request_qualification_contract(
    state: &mut BrokerState,
) -> Option<Result<Option<super::kvm_smp_qualification::KvmSmpQualificationContract>, i32>> {
    let Some(load) = state.qualification_load.as_ref() else {
        return Some(super::kvm_smp_qualification::load_kvm_smp_qualification_contract());
    };
    if let Some(loaded) = load.poll() {
        return Some(loaded);
    }
    if !load.busy() {
        load.request();
    }
    None
}

fn defer_qualification_catalog_retry(state: &mut BrokerState, errno: i32) -> bool {
    state.qualification_catalog_failures = state.qualification_catalog_failures.saturating_add(1);
    state.qualification_catalog_retry_after = Some(
        Instant::now() + qualification_catalog_retry_backoff(state.qualification_catalog_failures),
    );
    if state.qualification_catalog_last_error != Some(errno) {
        debug_line(format!("runtimed: qualification catalog pending errno={errno}").as_str());
        state.qualification_catalog_last_error = Some(errno);
    }
    false
}

fn qualification_catalog_retry_backoff(consecutive_failures: u32) -> std::time::Duration {
    let exponent = consecutive_failures.saturating_sub(1).min(6);
    super::STORAGE_NOT_READY_RETRY_BACKOFF
        .saturating_mul(1_u32 << exponent)
        .min(super::MAX_LAUNCH_RETRY_BACKOFF)
}

fn sort_launch_entries(launch_entries: &mut [LaunchEntry]) {
    launch_entries.sort_by(|lhs, rhs| {
        launch_entry_priority(lhs)
            .cmp(&launch_entry_priority(rhs))
            .then_with(|| lhs.desktop_file_id.cmp(&rhs.desktop_file_id))
            .then_with(|| lhs.display_name.cmp(&rhs.display_name))
            .then_with(|| lhs.exec.cmp(&rhs.exec))
    });
}

fn qualification_catalog_candidate(
    published_entries: &[LaunchEntry],
    contract: Result<Option<super::kvm_smp_qualification::KvmSmpQualificationContract>, i32>,
) -> Result<Vec<LaunchEntry>, i32> {
    // Resolve the external snapshot before cloning the published ordinary
    // catalog. This keeps the normal no-DVM retry path allocation-free.
    let contract = contract?;
    let mut candidate = published_entries.to_vec();
    super::kvm_smp_qualification::inject_kvm_smp_qualification_launch(&mut candidate, contract)
        .map_err(|()| libc::EINVAL)?;
    sort_launch_entries(&mut candidate);
    Ok(candidate)
}

fn validate_ui_bootstrap_metadata(programs: &BTreeMap<String, ProgramMetadata>) -> Result<(), i32> {
    let metadata = programs
        .get(UI_SERVER_DESKTOP_FILE_ID)
        .ok_or(libc::ENOENT)?;
    let expected_env = UI_SERVER_BOOTSTRAP_ENV
        .iter()
        .map(|value| String::from(*value))
        .collect::<Vec<_>>();
    if metadata.exec != UI_SERVER_EXEC_PATH
        || metadata.weight_micros != UI_SERVER_CATALOG_WEIGHT_MICROS
        || metadata.logical_admin
        || metadata.console_hosted
        || !metadata.args.is_empty()
        || metadata.env != expected_env
    {
        return Err(libc::EINVAL);
    }
    Ok(())
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
            private_smp_qualification: None,
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
            private_smp_qualification: None,
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

fn launch_entry_priority(entry: &LaunchEntry) -> (u8, u8, u8, &str) {
    let service_rank = if entry.exec == UI_SERVER_EXEC_PATH {
        0
    } else if entry.exec.starts_with("services/") {
        3
    } else if entry.console_hosted {
        2
    } else {
        1
    };
    // netprobe is an acceptance/background traffic generator. It must not
    // occupy the single serialized loader/VFS path before an interactive
    // desktop client such as WayClick. Its explicit runtime dependency and
    // KVM contract still decide whether the probe succeeds after launch.
    let background_probe_rank = u8::from(entry.desktop_file_id == "netprobe.desktop");
    let restart_rank = u8::from(entry.restart);
    (
        service_rank,
        background_probe_rank,
        restart_rank,
        entry.desktop_file_id.as_str(),
    )
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

#[cfg(test)]
mod tests {
    use super::{
        launch_entry_priority, qualification_catalog_candidate,
        qualification_catalog_retry_backoff, validate_ui_bootstrap_metadata, LaunchEntry,
        ProgramMetadata, StartupMode, UI_SERVER_BOOTSTRAP_ENV, UI_SERVER_CATALOG_WEIGHT_MICROS,
        UI_SERVER_DESKTOP_FILE_ID, UI_SERVER_EXEC_PATH,
    };
    use crate::kvm_smp_qualification::KvmSmpQualificationContract;
    use std::collections::BTreeMap;

    fn app(desktop_file_id: &str) -> LaunchEntry {
        LaunchEntry {
            package_id: desktop_file_id.into(),
            desktop_file_id: desktop_file_id.into(),
            display_name: desktop_file_id.into(),
            exec: format!("apps/{desktop_file_id}/app.elf"),
            runtime_deps: Vec::new(),
            restart: false,
            weight_micros: 100,
            logical_admin: false,
            console_hosted: false,
            args: Vec::new(),
            env: Vec::new(),
            private_smp_qualification: None,
        }
    }

    #[test]
    fn background_network_probe_never_precedes_interactive_desktop() {
        assert!(
            launch_entry_priority(&app("wayclick.desktop"))
                < launch_entry_priority(&app("netprobe.desktop"))
        );
    }

    #[test]
    fn qualification_load_failure_leaves_published_ordinary_entries_unchanged() {
        let entries = vec![app("ordinary.desktop")];
        let original = entries.clone();
        assert_eq!(
            qualification_catalog_candidate(&entries, Err(libc::EAGAIN)),
            Err(libc::EAGAIN)
        );
        assert_eq!(entries, original);
    }

    #[test]
    fn qualification_retry_backoff_is_bounded_and_monotonic() {
        let first = qualification_catalog_retry_backoff(1);
        let second = qualification_catalog_retry_backoff(2);
        let saturated = qualification_catalog_retry_backoff(u32::MAX);
        assert_eq!(first, crate::STORAGE_NOT_READY_RETRY_BACKOFF);
        assert!(second > first);
        assert_eq!(saturated, crate::MAX_LAUNCH_RETRY_BACKOFF);
    }

    #[test]
    fn absent_qualification_contract_keeps_ordinary_catalog_unchanged() {
        let entries = vec![app("ordinary.desktop")];
        let original = entries.clone();
        let candidate =
            qualification_catalog_candidate(&entries, Ok(None)).expect("normal product catalog");
        assert_eq!(candidate, original);
    }

    #[test]
    fn exact_qualification_contract_adds_one_catalog_policy() {
        let entries = vec![app("ordinary.desktop")];
        let entries = qualification_catalog_candidate(
            &entries,
            Ok(Some(KvmSmpQualificationContract {
                workers: 1,
                work_units: 1,
                deadline_ms: 100,
            })),
        )
        .expect("exact qualification contract");
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.private_smp_qualification.is_some())
                .count(),
            1
        );
    }

    #[test]
    fn sealed_ui_bootstrap_defaults_must_match_the_catalog() {
        let metadata = ProgramMetadata {
            package_id: String::from("uiserver"),
            desktop_file_id: String::from(UI_SERVER_DESKTOP_FILE_ID),
            display_name: String::from("UI Server"),
            exec: String::from(UI_SERVER_EXEC_PATH),
            runtime_deps: Vec::new(),
            startup: StartupMode::Desktop,
            weight_micros: UI_SERVER_CATALOG_WEIGHT_MICROS,
            logical_admin: false,
            console_hosted: false,
            args: Vec::new(),
            env: UI_SERVER_BOOTSTRAP_ENV
                .iter()
                .map(|value| String::from(*value))
                .collect(),
        };
        let mut programs = BTreeMap::from([(String::from(UI_SERVER_DESKTOP_FILE_ID), metadata)]);
        assert_eq!(validate_ui_bootstrap_metadata(&programs), Ok(()));
        programs
            .get_mut(UI_SERVER_DESKTOP_FILE_ID)
            .expect("uiserver metadata")
            .env
            .push(String::from("UNSEALED=1"));
        assert_eq!(validate_ui_bootstrap_metadata(&programs), Err(libc::EINVAL));
    }
}
