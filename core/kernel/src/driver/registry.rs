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
    Validated,
    Deferred,
    Loaded,
    LoadFailed,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DriverRecord {
    pub(crate) name: &'static str,
    pub(crate) class: DriverClass,
    pub(crate) bus: DriverBus,
    pub(crate) model: DriverExecutionModel,
    pub(crate) load_priority: i32,
    pub(crate) image_path: Option<&'static str>,
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
}

static DRIVER_REGISTRY: Mutex<Vec<DriverRecord>> = Mutex::new(Vec::new());

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
        if record.module_state != Some(DriverModuleState::Validated) {
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
        });
    }
    pending.sort_by_key(|candidate| candidate.load_priority);
    pending
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
}
