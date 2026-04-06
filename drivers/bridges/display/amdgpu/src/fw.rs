use crate::api;

const MAX_FIRMWARE_BYTES: usize = 384 * 1024;
static mut FIRMWARE_SCRATCH: [u8; MAX_FIRMWARE_BYTES] = [0; MAX_FIRMWARE_BYTES];

const REQUIRED_FIRMWARE: [&str; 4] = [
    "system/firmware/amdgpu/dcn_3_1_4_dmcub.bin",
    "system/firmware/amdgpu/psp_13_0_10_sos.bin",
    "system/firmware/amdgpu/psp_13_0_10_ta.bin",
    "system/firmware/amdgpu/smu_13_0_10.bin",
];

pub fn load_required_firmware() -> Result<(), &'static str> {
    for path in REQUIRED_FIRMWARE {
        let len =
            api::boot_file_len(path).map_err(|_| "amdgpu: required firmware file is missing")?;
        if len == 0 || len as usize > MAX_FIRMWARE_BYTES {
            return Err("amdgpu: firmware blob is empty or larger than the staging window");
        }

        let scratch = unsafe {
            core::slice::from_raw_parts_mut(
                core::ptr::addr_of_mut!(FIRMWARE_SCRATCH) as *mut u8,
                MAX_FIRMWARE_BYTES,
            )
        };
        let read = api::read_boot_file(path, &mut scratch[..len as usize])
            .map_err(|_| "amdgpu: required firmware blob could not be read")?;
        if read != len as usize {
            return Err("amdgpu: firmware read returned a truncated blob");
        }
    }

    Ok(())
}
