use alloc::vec::Vec;

use driver_abi::{DriverBus, DriverClass, DriverModuleHeader};
use spin::Mutex;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DriverExecutionModel {
    KernelBuiltin,
    LoadableElf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DriverModuleState {
    Skipped,
    Deferred,
    Loaded,
    LoadFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DriverRecord {
    pub(crate) name: &'static str,
    pub(crate) class: DriverClass,
    pub(crate) bus: DriverBus,
    pub(crate) model: DriverExecutionModel,
    pub(crate) load_priority: i32,
    pub(crate) image_path: Option<&'static str>,
    pub(crate) aliases: &'static str,
    pub(crate) deps: &'static str,
    pub(crate) softdeps: &'static str,
    pub(crate) provider_group: Option<&'static str>,
    pub(crate) fallback_only: bool,
    pub(crate) module_state: Option<DriverModuleState>,
    pub(crate) module_header: Option<DriverModuleHeader>,
    pub(crate) validation_error: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LoadableDriverCandidate {
    pub(crate) name: &'static str,
    pub(crate) class: DriverClass,
    pub(crate) bus: DriverBus,
    pub(crate) image_path: &'static str,
    pub(crate) load_priority: i32,
    pub(crate) aliases: &'static str,
    pub(crate) deps: &'static str,
    pub(crate) softdeps: &'static str,
    pub(crate) provider_group: Option<&'static str>,
    pub(crate) fallback_only: bool,
}

static DRIVER_REGISTRY: Mutex<Vec<DriverRecord>> = Mutex::new(Vec::new());
static ACTIVE_PROVIDER_GROUPS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
const BUILTIN_COMPAT_MODULES: &[&str] = &["virtio_dma_buf"];

pub(super) fn insert_kernel_builtin(name: &'static str, class: DriverClass, bus: DriverBus) {
    let mut registry = DRIVER_REGISTRY.lock();
    if registry
        .iter()
        .any(|record| record.name == name && record.class == class && record.bus == bus)
    {
        return;
    }

    registry.push(DriverRecord {
        name,
        class,
        bus,
        model: DriverExecutionModel::KernelBuiltin,
        load_priority: 0,
        image_path: None,
        aliases: "",
        deps: "",
        softdeps: "",
        provider_group: None,
        fallback_only: false,
        module_state: None,
        module_header: None,
        validation_error: None,
    });
}

pub(super) fn insert_loadable_elf(
    name: &'static str,
    class: DriverClass,
    bus: DriverBus,
    load_priority: i32,
    image_path: &'static str,
    aliases: &'static str,
    deps: &'static str,
    softdeps: &'static str,
    provider_group: Option<&'static str>,
    fallback_only: bool,
    module_state: Option<DriverModuleState>,
    module_header: Option<DriverModuleHeader>,
    validation_error: Option<&'static str>,
) {
    let mut registry = DRIVER_REGISTRY.lock();
    if registry.iter().any(|record| {
        record.name == name
            && record.class == class
            && record.bus == bus
            && record.model == DriverExecutionModel::LoadableElf
            && record.image_path == Some(image_path)
    }) {
        return;
    }

    registry.push(DriverRecord {
        name,
        class,
        bus,
        model: DriverExecutionModel::LoadableElf,
        load_priority,
        image_path: Some(image_path),
        aliases,
        deps,
        softdeps,
        provider_group,
        fallback_only,
        module_state,
        module_header,
        validation_error,
    });
}

pub(super) fn pending_loadable_records(
    filter: impl Fn(&DriverRecord) -> bool,
) -> Vec<LoadableDriverCandidate> {
    let registry = DRIVER_REGISTRY.lock();
    let mut pending = Vec::new();
    for record in registry.iter() {
        if record.model != DriverExecutionModel::LoadableElf {
            continue;
        }
        if matches!(
            record.module_state,
            Some(
                DriverModuleState::Skipped
                    | DriverModuleState::Loaded
                    | DriverModuleState::LoadFailed
            )
        ) {
            continue;
        }
        if !filter(record) {
            continue;
        }
        let Some(image_path) = record.image_path else {
            continue;
        };
        pending.push(LoadableDriverCandidate {
            name: record.name,
            class: record.class,
            bus: record.bus,
            image_path,
            load_priority: record.load_priority,
            aliases: record.aliases,
            deps: record.deps,
            softdeps: record.softdeps,
            provider_group: record.provider_group,
            fallback_only: record.fallback_only,
        });
    }
    pending.sort_by_key(|candidate| {
        (
            !candidate.softdeps.trim().is_empty(),
            candidate.fallback_only,
            candidate.load_priority,
            candidate.name,
        )
    });
    pending
}

pub(super) fn loadable_candidate_by_name(name: &str) -> Option<LoadableDriverCandidate> {
    let registry = DRIVER_REGISTRY.lock();
    registry.iter().find_map(|record| {
        if record.model != DriverExecutionModel::LoadableElf || record.name != name {
            return None;
        }
        let image_path = record.image_path?;
        Some(LoadableDriverCandidate {
            name: record.name,
            class: record.class,
            bus: record.bus,
            image_path,
            load_priority: record.load_priority,
            aliases: record.aliases,
            deps: record.deps,
            softdeps: record.softdeps,
            provider_group: record.provider_group,
            fallback_only: record.fallback_only,
        })
    })
}

pub(super) fn update_loadable_module_status(
    name: &'static str,
    image_path: &'static str,
    state: DriverModuleState,
    validation_error: Option<&'static str>,
) {
    let mut registry = DRIVER_REGISTRY.lock();
    if let Some(record) = registry.iter_mut().find(|record| {
        record.name == name
            && record.model == DriverExecutionModel::LoadableElf
            && record.image_path == Some(image_path)
    }) {
        record.module_state = Some(state);
        record.validation_error = validation_error;
    }
}

pub(super) fn contains_loadable_elf(
    name: &'static str,
    class: DriverClass,
    bus: DriverBus,
    image_path: &'static str,
) -> bool {
    let registry = DRIVER_REGISTRY.lock();
    registry.iter().any(|record| {
        record.name == name
            && record.class == class
            && record.bus == bus
            && record.model == DriverExecutionModel::LoadableElf
            && record.image_path == Some(image_path)
    })
}

pub(super) fn module_dependency_available(name: &str) -> bool {
    if BUILTIN_COMPAT_MODULES.contains(&name) {
        return true;
    }

    let registry = DRIVER_REGISTRY.lock();
    registry.iter().any(|record| {
        record.name == name
            && (record.model == DriverExecutionModel::KernelBuiltin
                || record.module_state == Some(DriverModuleState::Loaded))
    })
}

pub(super) fn loadable_provider_group_loaded(group: &str) -> bool {
    let registry = DRIVER_REGISTRY.lock();
    registry.iter().any(|record| {
        record.model == DriverExecutionModel::LoadableElf
            && record.provider_group == Some(group)
            && record.module_state == Some(DriverModuleState::Loaded)
    })
}

pub(super) fn mark_provider_group_active(group: &'static str) {
    let mut groups = ACTIVE_PROVIDER_GROUPS.lock();
    if !groups.contains(&group) {
        groups.push(group);
    }
}

pub(super) fn provider_group_active(group: &str) -> bool {
    ACTIVE_PROVIDER_GROUPS
        .lock()
        .iter()
        .any(|active| *active == group)
        || loadable_provider_group_loaded(group)
}

pub(super) fn loadable_records() -> Vec<DriverRecord> {
    let registry = DRIVER_REGISTRY.lock();
    registry
        .iter()
        .copied()
        .filter(|record| record.model == DriverExecutionModel::LoadableElf)
        .collect()
}

#[cfg(test)]
pub(super) fn snapshot_registered_drivers(dest: &mut [DriverRecord]) -> usize {
    let registry = DRIVER_REGISTRY.lock();
    let count = core::cmp::min(dest.len(), registry.len());
    dest[..count].copy_from_slice(&registry[..count]);
    count
}

#[cfg(test)]
pub(super) fn reset_for_tests() {
    DRIVER_REGISTRY.lock().clear();
    ACTIVE_PROVIDER_GROUPS.lock().clear();
}
