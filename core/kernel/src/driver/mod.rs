use alloc::vec::Vec;

mod bus;
mod class;
mod devres;
pub(crate) mod dma;
mod export;
pub(crate) mod input;
pub(crate) mod iommu;
pub(crate) mod irq;
mod kernel_api;
pub(crate) mod linux;
mod loader;
pub(crate) mod mmio;
mod module_registry;
pub(crate) mod pci;
mod registry;
pub(crate) mod serio;

use driver_abi::{DriverBus, DriverClass, DriverKernelApiV1};

use loader::{load_module_image, validate_module_image};
#[cfg(test)]
pub(crate) use registry::DriverExecutionModel;
pub(crate) use registry::{DriverModuleState, DriverRecord};

pub(crate) fn exported_kernel_api() -> *const DriverKernelApiV1 {
    kernel_api::exported_kernel_api()
}

pub(crate) fn parse_driver_class(name: &str) -> Option<DriverClass> {
    class::parse(name)
}

pub(crate) fn parse_driver_bus(name: &str) -> Option<DriverBus> {
    bus::parse(name)
}

pub(crate) fn register_kernel_builtin(name: &'static str, class: DriverClass, bus: DriverBus) {
    debug_assert!(class::is_supported(class));
    debug_assert!(bus::is_supported(bus));

    registry::insert_kernel_builtin(name, class, bus);
}

// Thin compatibility wrapper retained for tests and generated registries that do not need
// explicit load priority.
#[allow(dead_code)]
pub(crate) fn register_loadable_elf(
    name: &'static str,
    class: DriverClass,
    bus: DriverBus,
    image_path: &'static str,
) {
    register_loadable_elf_with_priority(name, class, bus, 0, image_path);
}

pub(crate) fn register_loadable_elf_with_priority(
    name: &'static str,
    class: DriverClass,
    bus: DriverBus,
    load_priority: i32,
    image_path: &'static str,
) {
    let (module_state, module_header, validation_error) =
        match validate_module_image(image_path, name, class, bus) {
            Ok(header) => {
                crate::debug::println!(
                    "driver module validated: name={} class={} bus={} path={}",
                    name,
                    class::name(class),
                    bus::name(bus),
                    image_path
                );
                (Some(DriverModuleState::Validated), Some(header), None)
            }
            Err(error) => {
                crate::debug::println!(
                    "driver module validation failed: name={} class={} bus={} path={} error={}",
                    name,
                    class::name(class),
                    bus::name(bus),
                    image_path,
                    error
                );
                (Some(DriverModuleState::Invalid), None, Some(error))
            }
        };

    registry::insert_loadable_elf(
        name,
        class,
        bus,
        load_priority,
        image_path,
        module_state,
        module_header,
        validation_error,
    );
}

// Retained as a convenience entry point for broad "load everything" bring-up flows.
#[allow(dead_code)]
pub(crate) fn initialize_loadable_modules() {
    initialize_loadable_modules_matching(|_| true);
}

pub(crate) fn initialize_loadable_modules_for_class(class: DriverClass) {
    initialize_loadable_modules_matching(|record| record.class == class);
}

pub(crate) fn initialize_loadable_modules_for_bus(bus: DriverBus) {
    initialize_loadable_modules_matching(|record| record.bus == bus);
}

fn initialize_loadable_modules_matching(filter: impl Fn(&DriverRecord) -> bool) {
    let mut pending = registry::pending_loadable_records(filter);

    crate::debug::println!(
        "driver module initialization start: candidates={}",
        pending.len()
    );

    while !pending.is_empty() {
        let mut progress = false;
        let mut deferred = Vec::new();

        for candidate in pending.into_iter() {
            let name = candidate.name;
            let class = candidate.class;
            let bus = candidate.bus;
            let image_path = candidate.image_path;
            match x86_64::instructions::interrupts::without_interrupts(|| {
                load_module_image(name, class, bus, image_path)
            }) {
                Ok(_module) => {
                    registry::update_loadable_module_status(
                        name,
                        image_path,
                        DriverModuleState::Loaded,
                        None,
                    );

                    crate::debug::println!(
                        "driver module loaded: name={} class={} bus={} path={} base={:#x} host={:#x}",
                        _module.name,
                        class::name(class),
                        bus::name(bus),
                        _module.image_path,
                        _module.runtime_base,
                        _module.host_base
                    );
                    progress = true;
                }
                Err(error) => {
                    if error == "module references unsupported external symbol"
                        && can_defer_module_dependency(candidate)
                    {
                        registry::update_loadable_module_status(
                            name,
                            image_path,
                            DriverModuleState::Deferred,
                            Some(error),
                        );

                        deferred.push(candidate);
                        continue;
                    }

                    crate::debug::println!(
                        "driver module load failed: name={} class={} bus={} path={} error={}",
                        name,
                        class::name(class),
                        bus::name(bus),
                        image_path,
                        error
                    );
                    registry::update_loadable_module_status(
                        name,
                        image_path,
                        DriverModuleState::LoadFailed,
                        Some(error),
                    );
                }
            }
        }

        if !progress {
            for _candidate in deferred.iter().copied() {
                crate::debug::println!(
                    "driver module deferred: name={} class={} bus={} path={} reason=module references unsupported external symbol",
                    _candidate.name,
                    class::name(_candidate.class),
                    bus::name(_candidate.bus),
                    _candidate.image_path
                );
            }
            break;
        }

        pending = deferred;
    }

    for _record in registry::loadable_records() {
        crate::debug::println!(
            "driver module status: name={} class={} bus={} path={} state={:?} error={}",
            _record.name,
            class::name(_record.class),
            bus::name(_record.bus),
            _record.image_path.unwrap_or("-"),
            _record.module_state,
            _record.validation_error.unwrap_or("-")
        );
    }
}

fn can_defer_module_dependency(record: registry::LoadableDriverCandidate) -> bool {
    matches!(
        (record.class, record.bus),
        (DriverClass::Input, DriverBus::Usb)
    )
}

#[cfg(test)]
pub(crate) fn snapshot_registered_drivers(dest: &mut [DriverRecord]) -> usize {
    registry::snapshot_registered_drivers(dest)
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    registry::reset_for_tests();
    loader::reset_for_tests();
}

#[cfg(test)]
mod tests {
    use super::{
        DriverBus, DriverClass, DriverExecutionModel, DriverModuleState, DriverRecord,
        register_kernel_builtin, register_loadable_elf, reset_for_tests,
        snapshot_registered_drivers,
    };

    fn isolated() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::exclusive_test()
    }

    #[test]
    fn snapshot_contains_registered_builtin_drivers() {
        let _guard = isolated();
        reset_for_tests();
        register_kernel_builtin("uefi-gop", DriverClass::Display, DriverBus::Platform);
        register_kernel_builtin("legacy-keyboard", DriverClass::Input, DriverBus::Serio);

        let mut records = [DriverRecord {
            name: "",
            class: DriverClass::Display,
            bus: DriverBus::Platform,
            model: DriverExecutionModel::LoadableElf,
            load_priority: 0,
            image_path: None,
            module_state: None,
            module_header: None,
            validation_error: None,
        }; 4];
        let count = snapshot_registered_drivers(&mut records);

        assert_eq!(count, 2);
        assert_eq!(
            records[0],
            DriverRecord {
                name: "uefi-gop",
                class: DriverClass::Display,
                bus: DriverBus::Platform,
                model: DriverExecutionModel::KernelBuiltin,
                load_priority: 0,
                image_path: None,
                module_state: None,
                module_header: None,
                validation_error: None,
            }
        );
        assert_eq!(
            records[1],
            DriverRecord {
                name: "legacy-keyboard",
                class: DriverClass::Input,
                bus: DriverBus::Serio,
                model: DriverExecutionModel::KernelBuiltin,
                load_priority: 0,
                image_path: None,
                module_state: None,
                module_header: None,
                validation_error: None,
            }
        );
    }

    #[test]
    fn duplicate_registration_is_ignored() {
        let _guard = isolated();
        reset_for_tests();
        register_kernel_builtin("legacy-keyboard", DriverClass::Input, DriverBus::Serio);
        register_kernel_builtin("legacy-keyboard", DriverClass::Input, DriverBus::Serio);

        let mut records = [DriverRecord {
            name: "",
            class: DriverClass::Display,
            bus: DriverBus::Platform,
            model: DriverExecutionModel::LoadableElf,
            load_priority: 0,
            image_path: None,
            module_state: None,
            module_header: None,
            validation_error: None,
        }; 2];
        let count = snapshot_registered_drivers(&mut records);

        assert_eq!(count, 1);
        assert_eq!(records[0].name, "legacy-keyboard");
    }

    #[test]
    fn missing_module_is_recorded_as_invalid() {
        let _guard = isolated();
        reset_for_tests();
        register_loadable_elf(
            "missing",
            DriverClass::Input,
            DriverBus::Usb,
            "system/drivers/input/does-not-exist.ko",
        );

        let mut records = [DriverRecord {
            name: "",
            class: DriverClass::Display,
            bus: DriverBus::Platform,
            model: DriverExecutionModel::KernelBuiltin,
            load_priority: 0,
            image_path: None,
            module_state: None,
            module_header: None,
            validation_error: None,
        }; 1];
        let count = snapshot_registered_drivers(&mut records);

        assert_eq!(count, 1);
        assert_eq!(records[0].module_state, Some(DriverModuleState::Invalid));
        assert_eq!(records[0].validation_error, Some("module image not found"));
    }
}
