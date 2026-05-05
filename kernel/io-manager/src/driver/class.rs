use driver_abi::DriverClass;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ClassCoreDescriptor {
    pub(crate) class: DriverClass,
    pub(crate) name: &'static str,
    pub(crate) devnode_namespace: Option<&'static str>,
}

const SUPPORTED_CLASS_CORES: &[ClassCoreDescriptor] = &[
    ClassCoreDescriptor {
        class: DriverClass::Display,
        name: "display",
        devnode_namespace: Some("/dev/display"),
    },
    ClassCoreDescriptor {
        class: DriverClass::Input,
        name: "input",
        devnode_namespace: Some("/dev/input"),
    },
    ClassCoreDescriptor {
        class: DriverClass::Network,
        name: "network",
        devnode_namespace: Some("/dev/net"),
    },
];

pub(crate) fn descriptor(class: DriverClass) -> Option<&'static ClassCoreDescriptor> {
    SUPPORTED_CLASS_CORES
        .iter()
        .find(|descriptor| descriptor.class == class)
}

pub(crate) fn is_supported(class: DriverClass) -> bool {
    descriptor(class).is_some()
}

// Class names are currently consumed by diagnostics and generated registry validation.
#[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
pub(crate) fn name(class: DriverClass) -> &'static str {
    descriptor(class)
        .map(|descriptor| descriptor.name)
        .unwrap_or("unknown")
}

pub(crate) fn parse(name: &str) -> Option<DriverClass> {
    SUPPORTED_CLASS_CORES
        .iter()
        .find(|descriptor| descriptor.name.eq_ignore_ascii_case(name))
        .map(|descriptor| descriptor.class)
}
