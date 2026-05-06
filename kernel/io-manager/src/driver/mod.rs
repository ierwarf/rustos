use alloc::boxed::Box;
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
pub mod virtio_gpu;

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
    register_loadable_elf_with_policy(
        name,
        class,
        bus,
        load_priority,
        image_path,
        "",
        "",
        "",
        None,
        false,
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn register_loadable_elf_with_policy(
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
) {
    crate::debug::println!(
        "driver module registered: name={} class={} bus={} path={} priority={} aliases={} deps={} softdeps={} provider_group={} fallback_only={}",
        name,
        class::name(class),
        bus::name(bus),
        image_path,
        load_priority,
        aliases,
        deps,
        softdeps,
        provider_group.unwrap_or("-"),
        fallback_only
    );

    registry::insert_loadable_elf(
        name,
        class,
        bus,
        load_priority,
        image_path,
        aliases,
        deps,
        softdeps,
        provider_group,
        fallback_only,
        None,
        None,
        None,
    );
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
    activate_builtin_providers_for_class(class);
    initialize_loadable_modules_matching(|record| record.class == class);
    class_has_active_loadable_provider(class)
}

fn activate_builtin_providers_for_class(class: DriverClass) {
    if class != DriverClass::Display {
        return;
    }

    if virtio_gpu::try_enable_primary_display() {
        registry::mark_provider_group_active("display-primary");
    }
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
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
        let aliases = registry_field(line, "aliases").unwrap_or("");
        let deps = registry_field(line, "deps").unwrap_or("");
        let softdeps = registry_field(line, "softdeps").unwrap_or("");
        let provider_group =
            registry_field(line, "provider_group").filter(|value| !value.is_empty());
        let fallback_only = registry_field(line, "fallback_only")
            .map(|value| matches!(value, "1" | "true" | "True" | "yes" | "Yes"))
            .unwrap_or(false);

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
        let leaked_aliases: &'static str = Box::leak(aliases.to_string().into_boxed_str());
        let leaked_deps: &'static str = Box::leak(deps.to_string().into_boxed_str());
        let leaked_softdeps: &'static str = Box::leak(softdeps.to_string().into_boxed_str());
        let leaked_provider_group: Option<&'static str> = provider_group
            .map(|value| Box::leak(value.to_string().into_boxed_str()) as &'static str);
        register_loadable_elf_with_policy(
            leaked_name,
            class,
            bus,
            priority,
            leaked_path,
            leaked_aliases,
            leaked_deps,
            leaked_softdeps,
            leaked_provider_group,
            fallback_only,
        );
        loaded_records = loaded_records.saturating_add(1);
    }

    LOADABLE_DRIVER_REGISTRY_LOADED.store(true, Ordering::Release);
    crate::debug::info!(driver, "driver registry loaded entries={}", loaded_records);
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
            match load_candidate_with_dependencies(candidate, &mut Vec::new()) {
                LoadAttempt::Loaded | LoadAttempt::Skipped => {
                    progress = true;
                }
                LoadAttempt::Deferred => {
                    deferred.push(candidate);
                }
                LoadAttempt::Failed => {
                    progress = true;
                }
            }
        }

        if !progress {
            for _candidate in deferred.iter().copied() {
                let reason = if loadable_candidate_deps_available(_candidate) {
                    "module references unsupported external symbol"
                } else {
                    "dependency unavailable"
                };
                crate::debug::println!(
                    "driver module deferred: name={} class={} bus={} path={} reason={}",
                    _candidate.name,
                    class::name(_candidate.class),
                    bus::name(_candidate.bus),
                    _candidate.image_path,
                    reason
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoadAttempt {
    Loaded,
    Skipped,
    Deferred,
    Failed,
}

fn load_candidate_with_dependencies(
    candidate: registry::LoadableDriverCandidate,
    stack: &mut Vec<&'static str>,
) -> LoadAttempt {
    let name = candidate.name;
    let class = candidate.class;
    let bus = candidate.bus;
    let image_path = candidate.image_path;

    if registry::module_dependency_available(name) {
        return LoadAttempt::Loaded;
    }

    if stack.contains(&name) {
        registry::update_loadable_module_status(
            name,
            image_path,
            DriverModuleState::LoadFailed,
            Some("dependency cycle"),
        );
        crate::debug::println!(
            "driver module load failed: name={} class={} bus={} path={} error=dependency cycle",
            name,
            class::name(class),
            bus::name(bus),
            image_path
        );
        return LoadAttempt::Failed;
    }

    if !loadable_candidate_alias_matches(candidate) {
        registry::update_loadable_module_status(
            name,
            image_path,
            DriverModuleState::Skipped,
            Some("no matching device"),
        );
        crate::debug::println!(
            "driver module skipped: name={} class={} bus={} path={} reason=no matching device aliases={}",
            name,
            class::name(class),
            bus::name(bus),
            image_path,
            candidate.aliases
        );
        return LoadAttempt::Skipped;
    }

    if loadable_candidate_provider_active(candidate) {
        registry::update_loadable_module_status(
            name,
            image_path,
            DriverModuleState::Skipped,
            Some("provider already active"),
        );
        crate::debug::println!(
            "driver module skipped: name={} class={} bus={} path={} reason=provider already active provider_group={}",
            name,
            class::name(class),
            bus::name(bus),
            image_path,
            candidate.provider_group.unwrap_or("-")
        );
        return LoadAttempt::Skipped;
    }

    stack.push(name);
    if !loadable_candidate_dependencies_loaded(candidate, stack) {
        stack.pop();
        registry::update_loadable_module_status(
            name,
            image_path,
            DriverModuleState::Deferred,
            Some("dependency unavailable"),
        );
        return LoadAttempt::Deferred;
    }
    stack.pop();

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
            LoadAttempt::Loaded
        }
        Err(error) => {
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
            LoadAttempt::Failed
        }
    }
}

fn loadable_candidate_deps_available(candidate: registry::LoadableDriverCandidate) -> bool {
    candidate
        .deps
        .split(',')
        .map(str::trim)
        .filter(|dep| !dep.is_empty())
        .all(registry::module_dependency_available)
}

fn loadable_candidate_dependencies_loaded(
    candidate: registry::LoadableDriverCandidate,
    stack: &mut Vec<&'static str>,
) -> bool {
    for dep in candidate
        .deps
        .split(',')
        .map(str::trim)
        .filter(|dep| !dep.is_empty())
    {
        if !load_dependency(dep, true, stack) {
            return false;
        }
    }
    for dep in candidate
        .softdeps
        .split(',')
        .map(str::trim)
        .filter(|dep| !dep.is_empty())
    {
        let _ = load_dependency(dep, false, stack);
    }
    true
}

fn load_dependency(dep: &str, required: bool, stack: &mut Vec<&'static str>) -> bool {
    if registry::module_dependency_available(dep) {
        return true;
    }
    let Some(candidate) = registry::loadable_candidate_by_name(dep) else {
        return !required;
    };
    match load_candidate_with_dependencies(candidate, stack) {
        LoadAttempt::Loaded => true,
        LoadAttempt::Skipped | LoadAttempt::Deferred | LoadAttempt::Failed => !required,
    }
}

fn class_has_active_loadable_provider(class: DriverClass) -> bool {
    if class == DriverClass::Display {
        return crate::io::gui::display_info().is_some();
    }

    registry::loadable_records().iter().any(|record| {
        record.class == class && !matches!(record.module_state, Some(DriverModuleState::LoadFailed))
    })
}

fn loadable_candidate_provider_active(candidate: registry::LoadableDriverCandidate) -> bool {
    let Some(group) = candidate.provider_group else {
        return false;
    };
    if group == "display-primary" {
        return crate::io::gui::display_info().is_some();
    }
    registry::provider_group_active(group)
}

fn loadable_candidate_alias_matches(candidate: registry::LoadableDriverCandidate) -> bool {
    let mut saw_alias = false;
    for alias in candidate.aliases.split(',').map(str::trim) {
        if alias.is_empty() {
            continue;
        }
        saw_alias = true;
        if device_alias_present(alias, candidate.class, candidate.bus) {
            return true;
        }
    }
    !saw_alias
}

fn device_alias_present(alias: &str, class: DriverClass, bus: DriverBus) -> bool {
    if alias == "platform:bootfb" {
        return crate::storage::boot_volume::boot_framebuffer_info()
            .is_some_and(|framebuffer| framebuffer.validate().is_ok());
    }

    if alias.starts_with("pci:") {
        return pci_alias_present(alias);
    }

    if alias.starts_with("virtio:") {
        return virtio_alias_present(alias);
    }

    if alias.starts_with("usb:") {
        return crate::usb::hid_interfaces_available() || crate::usb::host_controllers_available();
    }

    if alias.starts_with("hid:") {
        return class == DriverClass::Input && crate::usb::hid_interfaces_available();
    }

    if alias.starts_with("serio:") {
        return bus == DriverBus::Serio && serio::ports_available();
    }

    false
}

fn pci_alias_present(alias: &str) -> bool {
    let mut present = false;
    crate::arch::pci::visit_devices(|device| {
        if alias.contains("vendor=0x1002,class=0x03")
            && device.vendor_id() == 0x1002
            && device.class_code() == 0x03
        {
            present = true;
            return true;
        }

        if alias.starts_with("pci:v00001002")
            && device.vendor_id() == 0x1002
            && (!alias.contains("bc03") || device.class_code() == 0x03)
        {
            present = true;
            return true;
        }

        false
    });
    present
}

fn virtio_alias_present(alias: &str) -> bool {
    let wants_net = alias.starts_with("virtio:d00000001");
    let wants_gpu = alias.starts_with("virtio:d00000010");
    if !wants_net && !wants_gpu {
        return false;
    }

    let mut present = false;
    crate::arch::pci::visit_devices(|device| {
        if device.vendor_id() != 0x1af4 {
            return false;
        }
        if wants_gpu && device.class_code() == 0x03 {
            present = true;
            return true;
        }
        if wants_net && (device.device_id() == 0x1041 || device.device_id() == 0x1000) {
            present = true;
            return true;
        }
        false
    });
    present
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
    use super::registry::DriverExecutionModel;
    use super::{
        DriverBus, DriverClass, DriverRecord, register_kernel_builtin, register_loadable_elf,
        reset_for_tests, snapshot_registered_drivers,
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
            aliases: "",
            deps: "",
            softdeps: "",
            provider_group: None,
            fallback_only: false,
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
                aliases: "",
                deps: "",
                softdeps: "",
                provider_group: None,
                fallback_only: false,
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
                aliases: "",
                deps: "",
                softdeps: "",
                provider_group: None,
                fallback_only: false,
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
            aliases: "",
            deps: "",
            softdeps: "",
            provider_group: None,
            fallback_only: false,
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
            aliases: "",
            deps: "",
            softdeps: "",
            provider_group: None,
            fallback_only: false,
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
