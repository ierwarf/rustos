#![no_main]

use driver_domain_protocol::validate_dvm_ethernet_frame;
use libfuzzer_sys::fuzz_target;
use rustos_image_admission::{admit_elf64_image, ELF64_HEADER_SIZE};

fuzz_target!(|data: &[u8]| {
    let _ = validate_dvm_ethernet_frame(data);
    if let Some((header, phdrs)) = data.split_at_checked(ELF64_HEADER_SIZE) {
        let header: &[u8; ELF64_HEADER_SIZE] = header.try_into().expect("fixed split");
        let _ = admit_elf64_image(header, phdrs, 0x20_0000, 0x10_0000, 0x8000_0000);
    }
});
