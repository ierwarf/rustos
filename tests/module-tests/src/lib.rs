#[cfg(test)]
mod tests {
    use boot_protocol::{
        BOOT_INFO_MAGIC, BOOT_INFO_VERSION, BootDiagBufferInfo, BootInfo, BootMemoryKind,
        BootMemoryMap, BootMemoryRegion, BootPixelFormat, BootVolumeIdentity, CrashStoreInfo,
        FramebufferInfo, NucleusImageInfo,
    };
    use boot_random::{Random, init as init_random};
    use driver_abi::{DRIVER_MODULE_ABI_VERSION, DriverBus, DriverClass, DriverModuleHeader};
    use keyboard_core::{KeyAction, KeyCode, KeyboardDriver, Modifiers, ScanCodeSet};

    #[test]
    fn keyboard_core_decodes_basic_typing() {
        let mut keyboard = KeyboardDriver::new();
        keyboard.feed_scancode(0x1e);

        let event = keyboard.pop_event().expect("keyboard event");
        assert_eq!(event.code, KeyCode::A);
        assert_eq!(event.action, KeyAction::Pressed);
        assert_eq!(event.modifiers, Modifiers::empty());
        assert_eq!(event.text, Some(b'a'));
    }

    #[test]
    fn keyboard_core_decodes_set2_extended_key() {
        let mut keyboard = KeyboardDriver::new();
        keyboard.set_scan_code_set(ScanCodeSet::Set2);
        keyboard.feed_scancode(0xe0);
        keyboard.feed_scancode(0x75);

        let event = keyboard.pop_event().expect("keyboard event");
        assert_eq!(event.code, KeyCode::ArrowUp);
        assert_eq!(event.action, KeyAction::Pressed);
        assert_eq!(event.text, None);
    }

    #[test]
    fn driver_module_header_round_trips_strings() {
        let header = DriverModuleHeader::new(
            DriverClass::Input,
            DriverBus::Usb,
            "system/drivers/input/usbhid.ko",
            "usbhid",
        );

        assert_eq!(header.abi_version, DRIVER_MODULE_ABI_VERSION);
        assert_eq!(header.class, DriverClass::Input);
        assert_eq!(header.bus, DriverBus::Usb);
        assert_eq!(
            header.module_path_str().unwrap(),
            "system/drivers/input/usbhid.ko"
        );
        assert_eq!(header.name_str().unwrap(), "usbhid");
    }

    #[test]
    fn boot_random_uses_boot_seed_for_ranges() {
        let memory_map = [BootMemoryRegion {
            phys_start: 0x1000,
            page_count: 16,
            kind: BootMemoryKind::Usable,
            _reserved0: 0,
        }];
        let boot_info = BootInfo {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            _reserved0: 0,
            rng_seed: [0x5a; 32],
            acpi_rsdp_addr: 0,
            boot_volume: BootVolumeIdentity::empty(),
            framebuffer: FramebufferInfo {
                addr: 0x8000,
                size: 16 * 16 * 4,
                back_buffer_addr: 0,
                back_buffer_size: 0,
                width: 16,
                height: 16,
                stride: 16,
                pixel_format: BootPixelFormat::Rgb,
                bytes_per_pixel: 4,
                _reserved: [0; 3],
            },
            nucleus_image: NucleusImageInfo {
                phys_start: 0x20_0000,
                size: 0x2000,
                load_bias: 0x20_0000,
                entry_point: 0x20_1000,
            },
            memory_map: BootMemoryMap {
                entries_ptr: memory_map.as_ptr() as u64,
                entry_count: memory_map.len() as u32,
                _reserved0: 0,
            },
            boot_diag: BootDiagBufferInfo::default(),
            crash_store: CrashStoreInfo::default(),
        };

        init_random(&boot_info);
        let mut rng = Random::new();
        let value = rng.randint(-8, 24);
        assert!((-8..24).contains(&value));
    }
}
