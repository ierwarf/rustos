mod class;

pub mod dma;
pub mod iommu;
pub mod irq;
pub mod mmio;

use driver_abi::DriverClass;

pub fn initialize_loadable_modules_for_class(class: DriverClass) -> bool {
    class::is_supported(class)
}

pub fn load_module_image_from_policy(
    _name: &'static str,
    _class: u32,
    _bus: u32,
    _image_path: &'static str,
    _linux_driver_names: &'static str,
) -> Result<(), &'static str> {
    Err("driver policy is owned by driverd")
}

pub fn device_alias_present_from_policy(_alias: &str, _class: u32, _bus: u32) -> bool {
    false
}

pub fn provider_group_active_from_policy(_group: &str) -> bool {
    false
}
