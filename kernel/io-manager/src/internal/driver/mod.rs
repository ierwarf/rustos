use alloc::boxed::Box;
use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

mod bus;
mod class;
mod devres;
pub mod dma;
mod export;
pub mod input;
pub mod iommu;
pub mod irq;
mod kernel_api;
pub mod linux;
mod loader;
pub mod mmio;
mod module_registry;
pub mod pci;
mod registry;
pub mod serio;

use diag_abi::{DebugModuleInfo, DiagLevel, DiagProvider};
use driver_abi::{DriverBus, DriverClass, DriverKernelApiV1};

use loader::load_module_image;
pub(crate) use registry::{DriverModuleState, DriverRecord};

const LOADABLE_DRIVER_REGISTRY_PATH: &str = "system/registry/kernel/loadable-drivers.tsv";

static LOADABLE_DRIVER_REGISTRY_LOADED: AtomicBool = AtomicBool::new(false);
static LOADABLE_DRIVER_REGISTRY_LOCK: spin::Mutex<()> = spin::Mutex::new(());

pub(crate) fn exported_kernel_api() -> *const DriverKernelApiV1 {
    kernel_api::exported_kernel_api()
}

pub(crate) fn runtime_executable_addr_is_known(addr: usize) -> bool {
    loader::runtime_executable_addr_is_known(addr)
}

pub(crate) fn snapshot_loaded_modules(dest: &mut [DebugModuleInfo]) -> usize {
    loader::snapshot_loaded_modules(dest)
}

#[cfg(test)]
pub(crate) fn parse_driver_class(name: &str) -> Option<DriverClass> {
    class::parse(name)
}

#[cfg(test)]
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
    if let Some(skip_reason) = loadable_registration_skip_reason(name, class, bus) {
        crate::debug::println!(
            "driver module skipped: name={} class={} bus={} path={} reason={}",
            name,
            class::name(class),
            bus::name(bus),
            image_path,
            skip_reason
        );
        registry::insert_loadable_elf(
            name,
            class,
            bus,
            load_priority,
            image_path,
            Some(DriverModuleState::Skipped),
            None,
            Some(skip_reason),
        );
        return;
    }

    crate::debug::println!(
        "driver module registered: name={} class={} bus={} path={} priority={}",
        name,
        class::name(class),
        bus::name(bus),
        image_path,
        load_priority
    );

    registry::insert_loadable_elf(
        name,
        class,
        bus,
        load_priority,
        image_path,
        None,
        None,
        None,
    );
}

pub(crate) fn loadable_registration_skip_reason(
    name: &str,
    class: DriverClass,
    bus: DriverBus,
) -> Option<&'static str> {
    if name == "amdgpu" && class == DriverClass::Display && bus == DriverBus::Pci {
        return (!amd_display_hardware_present()).then_some("hardware not present");
    }

    if name == "bootfb"
        && class == DriverClass::Display
        && bus == DriverBus::Platform
        && crate::io::gui::display_info().is_some()
    {
        return Some("boot framebuffer already active");
    }

    None
}

fn amd_display_hardware_present() -> bool {
    let mut present = false;
    crate::arch::pci::visit_devices(|address| {
        if address.vendor_id() == 0x1002 && address.class_code() == 0x03 {
            present = true;
            return true;
        }
        false
    });
    present
}

// Retained as a convenience entry point for broad "load everything" bring-up flows.
#[allow(dead_code)]
pub(crate) fn initialize_loadable_modules() {
    initialize_loadable_modules_matching(|_| true);
}

pub fn initialize_loadable_modules_for_class(class: DriverClass) -> bool {
    if !ensure_loadable_driver_registry_loaded() {
        return false;
    }
    initialize_loadable_modules_matching(|record| record.class == class);
    true
}

fn ensure_loadable_driver_registry_loaded() -> bool {
    if LOADABLE_DRIVER_REGISTRY_LOADED.load(Ordering::Acquire) {
        return true;
    }

    let _guard = LOADABLE_DRIVER_REGISTRY_LOCK.lock();
    if LOADABLE_DRIVER_REGISTRY_LOADED.load(Ordering::Relaxed) {
        return true;
    }

    let bytes = match crate::vfs::read_path_to_vec_for_kernel(LOADABLE_DRIVER_REGISTRY_PATH) {
        Ok(bytes) => bytes,
        Err(error) => {
            crate::debug::println!(
                "driver registry load failed: path={} error={:?}",
                LOADABLE_DRIVER_REGISTRY_PATH,
                error,
            );
            return false;
        }
    };
    let text = match core::str::from_utf8(bytes.as_slice()) {
        Ok(text) => text,
        Err(_) => {
            crate::debug::println!(
                "driver registry load failed: {} is not valid UTF-8",
                LOADABLE_DRIVER_REGISTRY_PATH
            );
            return false;
        }
    };

    let mut loaded_records = 0_u64;
    for (line_index, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some(name) = registry_field(line, "name") else {
            crate::debug::println!(
                "driver registry parse failed: line={} missing name",
                line_index + 1
            );
            return false;
        };
        let Some(class_name) = registry_field(line, "class") else {
            crate::debug::println!(
                "driver registry parse failed: line={} missing class",
                line_index + 1
            );
            return false;
        };
        let Some(bus_name) = registry_field(line, "bus") else {
            crate::debug::println!(
                "driver registry parse failed: line={} missing bus",
                line_index + 1
            );
            return false;
        };
        let Some(path) = registry_field(line, "path") else {
            crate::debug::println!(
                "driver registry parse failed: line={} missing path",
                line_index + 1
            );
            return false;
        };
        let priority = registry_field(line, "priority")
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or(0);

        let Some(class) = class::parse(class_name) else {
            crate::debug::println!(
                "driver registry parse failed: line={} invalid class={}",
                line_index + 1,
                class_name
            );
            return false;
        };
        let Some(bus) = bus::parse(bus_name) else {
            crate::debug::println!(
                "driver registry parse failed: line={} invalid bus={}",
                line_index + 1,
                bus_name
            );
            return false;
        };

        let leaked_name: &'static str = Box::leak(name.to_string().into_boxed_str());
        let leaked_path: &'static str = Box::leak(path.to_string().into_boxed_str());
        register_loadable_elf_with_priority(leaked_name, class, bus, priority, leaked_path);
        loaded_records = loaded_records.saturating_add(1);
    }

    LOADABLE_DRIVER_REGISTRY_LOADED.store(true, Ordering::Release);
    crate::debug::emit_text(
        DiagProvider::Driver,
        DiagLevel::Info,
        40,
        0,
        loaded_records,
        format!("driver registry loaded entries={loaded_records}").as_str(),
    );
    crate::debug::println!("driver registry loaded: entries={}", loaded_records);
    true
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
            crate::debug::println!(
                "driver module load begin: name={} class={} bus={} path={}",
                name,
                class::name(class),
                bus::name(bus),
                image_path
            );
            match load_module_image(name, class, bus, image_path) {
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
        DriverBus, DriverClass, DriverRecord, register_kernel_builtin, register_loadable_elf,
        reset_for_tests, snapshot_registered_drivers,
    };
    use super::registry::DriverExecutionModel;

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
    fn missing_module_is_registered_lazily() {
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
        assert_eq!(records[0].module_state, None);
        assert_eq!(records[0].validation_error, None);
    }
}
