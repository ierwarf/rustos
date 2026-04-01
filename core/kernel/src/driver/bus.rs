use driver_abi::DriverBus;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BusCoreDescriptor {
    pub(crate) bus: DriverBus,
    pub(crate) name: &'static str,
    pub(crate) hotplug: bool,
}

const SUPPORTED_BUS_CORES: &[BusCoreDescriptor] = &[
    BusCoreDescriptor {
        bus: DriverBus::Platform,
        name: "platform",
        hotplug: false,
    },
    BusCoreDescriptor {
        bus: DriverBus::Serio,
        name: "serio",
        hotplug: false,
    },
    BusCoreDescriptor {
        bus: DriverBus::Usb,
        name: "usb",
        hotplug: true,
    },
    BusCoreDescriptor {
        bus: DriverBus::Pci,
        name: "pci",
        hotplug: true,
    },
    BusCoreDescriptor {
        bus: DriverBus::Virtio,
        name: "virtio",
        hotplug: true,
    },
];

pub(crate) fn descriptor(bus: DriverBus) -> Option<&'static BusCoreDescriptor> {
    SUPPORTED_BUS_CORES
        .iter()
        .find(|descriptor| descriptor.bus == bus)
}

pub(crate) fn is_supported(bus: DriverBus) -> bool {
    descriptor(bus).is_some()
}

// Bus names are currently consumed by diagnostics and generated registry validation.
#[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
pub(crate) fn name(bus: DriverBus) -> &'static str {
    descriptor(bus)
        .map(|descriptor| descriptor.name)
        .unwrap_or("unknown")
}

pub(crate) fn parse(name: &str) -> Option<DriverBus> {
    SUPPORTED_BUS_CORES
        .iter()
        .find(|descriptor| descriptor.name.eq_ignore_ascii_case(name))
        .map(|descriptor| descriptor.bus)
}
