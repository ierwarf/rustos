use alloc::vec::Vec;
use x86_64::instructions::port::Port;

use rustos_fault_injection::{
    clear_runtime_rules, configured_runtime_rule_count, install_runtime_rules,
    runtime_rules_enabled,
};

const FW_CFG_SIGNATURE: u16 = 0x0000;
const FW_CFG_FILE_DIR: u16 = 0x0019;
const FW_CFG_SELECTOR_PORT: u16 = 0x0510;
const FW_CFG_DATA_PORT: u16 = 0x0511;
const FW_CFG_FILE_NAME_LEN: usize = 56;
const FAULT_FW_CFG_NAME: &[u8] = b"opt/rustos/fault-injection";
const MAX_FAULT_SPEC_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultInitStatus {
    NotPresent,
    Loaded,
    InvalidSpec,
    TooLarge,
    UnsupportedFwCfg,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FaultInitReport {
    pub status: FaultInitStatus,
    pub rule_count: usize,
    pub spec_len: usize,
}

pub fn init_from_qemu_fw_cfg() -> FaultInitReport {
    let Some(spec) = read_qemu_fw_cfg_file(FAULT_FW_CFG_NAME) else {
        return FaultInitReport {
            status: FaultInitStatus::NotPresent,
            rule_count: 0,
            spec_len: 0,
        };
    };
    let spec_len = spec.len();
    if spec_len > MAX_FAULT_SPEC_BYTES {
        clear_rules();
        return FaultInitReport {
            status: FaultInitStatus::TooLarge,
            rule_count: 0,
            spec_len,
        };
    }
    let Ok(spec) = core::str::from_utf8(spec.as_slice()) else {
        clear_rules();
        return FaultInitReport {
            status: FaultInitStatus::InvalidSpec,
            rule_count: 0,
            spec_len,
        };
    };
    let Ok(rule_count) = install_runtime_rules(spec) else {
        clear_rules();
        return FaultInitReport {
            status: FaultInitStatus::InvalidSpec,
            rule_count: 0,
            spec_len,
        };
    };
    FaultInitReport {
        status: FaultInitStatus::Loaded,
        rule_count,
        spec_len,
    }
}

pub fn is_enabled() -> bool {
    runtime_rules_enabled()
}

pub fn should_fail(location: &str) -> bool {
    rustos_fault_injection::should_fail(location)
}

pub fn configured_rule_count() -> usize {
    configured_runtime_rule_count()
}

fn clear_rules() {
    clear_runtime_rules();
}

fn read_qemu_fw_cfg_file(name: &[u8]) -> Option<Vec<u8>> {
    if !fw_cfg_signature_is_qemu() {
        return None;
    }

    fw_cfg_select(FW_CFG_FILE_DIR);
    let count = fw_cfg_read_be_u32() as usize;
    for _ in 0..count {
        let size = fw_cfg_read_be_u32() as usize;
        let selector = fw_cfg_read_be_u16();
        let _reserved = fw_cfg_read_be_u16();
        let mut entry_name = [0u8; FW_CFG_FILE_NAME_LEN];
        for byte in &mut entry_name {
            *byte = fw_cfg_read_u8();
        }
        if fw_cfg_name_matches(&entry_name, name) {
            if size > MAX_FAULT_SPEC_BYTES {
                return Some(alloc::vec![0; MAX_FAULT_SPEC_BYTES + 1]);
            }
            fw_cfg_select(selector);
            let mut data = Vec::with_capacity(size);
            for _ in 0..size {
                data.push(fw_cfg_read_u8());
            }
            return Some(data);
        }
    }
    None
}

fn fw_cfg_signature_is_qemu() -> bool {
    fw_cfg_select(FW_CFG_SIGNATURE);
    let mut signature = [0u8; 4];
    for byte in &mut signature {
        *byte = fw_cfg_read_u8();
    }
    &signature == b"QEMU"
}

fn fw_cfg_select(selector: u16) {
    let mut port: Port<u16> = Port::new(FW_CFG_SELECTOR_PORT);
    unsafe {
        port.write(selector);
    }
}

fn fw_cfg_read_u8() -> u8 {
    let mut port: Port<u8> = Port::new(FW_CFG_DATA_PORT);
    unsafe { port.read() }
}

fn fw_cfg_read_be_u16() -> u16 {
    let bytes = [fw_cfg_read_u8(), fw_cfg_read_u8()];
    u16::from_be_bytes(bytes)
}

fn fw_cfg_read_be_u32() -> u32 {
    let bytes = [
        fw_cfg_read_u8(),
        fw_cfg_read_u8(),
        fw_cfg_read_u8(),
        fw_cfg_read_u8(),
    ];
    u32::from_be_bytes(bytes)
}

fn fw_cfg_name_matches(entry_name: &[u8; FW_CFG_FILE_NAME_LEN], expected: &[u8]) -> bool {
    let actual_len = entry_name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(entry_name.len());
    &entry_name[..actual_len] == expected
}

#[cfg(test)]
mod tests {
    use rustos_fault_injection::{clear_runtime_rules, install_runtime_rules, should_fail};
    use std::sync::Mutex;

    static TEST_RULES: Mutex<()> = Mutex::new(());

    #[test]
    fn drop_every_rule_fires_on_interval() {
        let _guard = TEST_RULES.lock().expect("fault-rule test lock");
        clear_runtime_rules();
        install_runtime_rules("display.present=drop-every:3").unwrap();
        assert!(!should_fail("display.present"));
        assert!(!should_fail("display.present"));
        assert!(should_fail("display.present"));
        clear_runtime_rules();
    }

    #[test]
    fn fail_after_rule_fires_after_threshold() {
        let _guard = TEST_RULES.lock().expect("fault-rule test lock");
        clear_runtime_rules();
        install_runtime_rules("process.spawn=fail-after:1").unwrap();
        assert!(!should_fail("process.spawn"));
        assert!(should_fail("process.spawn"));
        clear_runtime_rules();
    }
}
