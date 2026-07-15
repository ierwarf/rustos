use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use spin::Mutex;
use x86_64::instructions::port::Port;

use rustos_fault_injection::{FaultAction, parse_rules};

const FW_CFG_SIGNATURE: u16 = 0x0000;
const FW_CFG_FILE_DIR: u16 = 0x0019;
const FW_CFG_SELECTOR_PORT: u16 = 0x0510;
const FW_CFG_DATA_PORT: u16 = 0x0511;
const FW_CFG_FILE_NAME_LEN: usize = 56;
const FAULT_FW_CFG_NAME: &[u8] = b"opt/rustos/fault-injection";
const MAX_FAULT_SPEC_BYTES: usize = 4096;

static ENABLED: AtomicBool = AtomicBool::new(false);
static RULES: Mutex<Vec<RuntimeFaultRule>> = Mutex::new(Vec::new());

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

struct RuntimeFaultRule {
    location: String,
    action: FaultAction,
    hits: u64,
    rng: u64,
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
    let Ok(parsed) = parse_rules(spec) else {
        clear_rules();
        return FaultInitReport {
            status: FaultInitStatus::InvalidSpec,
            rule_count: 0,
            spec_len,
        };
    };

    let mut runtime_rules = RULES.lock();
    runtime_rules.clear();
    for rule in parsed {
        runtime_rules.push(RuntimeFaultRule {
            rng: seed_for_location(rule.location.as_bytes()),
            location: rule.location,
            action: rule.action,
            hits: 0,
        });
    }
    let rule_count = runtime_rules.len();
    ENABLED.store(rule_count != 0, Ordering::Release);
    FaultInitReport {
        status: FaultInitStatus::Loaded,
        rule_count,
        spec_len,
    }
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

pub fn should_fail(location: &str) -> bool {
    if !is_enabled() {
        return false;
    }
    let mut rules = RULES.lock();
    for rule in rules.iter_mut() {
        if rule.location != location {
            continue;
        }
        return rule.next_should_fail();
    }
    false
}

pub fn configured_rule_count() -> usize {
    RULES.lock().len()
}

fn clear_rules() {
    RULES.lock().clear();
    ENABLED.store(false, Ordering::Release);
}

impl RuntimeFaultRule {
    fn next_should_fail(&mut self) -> bool {
        self.hits = self.hits.saturating_add(1);
        match self.action {
            FaultAction::Fail => true,
            FaultAction::Off => false,
            FaultAction::DropEvery(n) => n != 0 && self.hits.is_multiple_of(u64::from(n)),
            FaultAction::FailAfter(n) => self.hits > n,
            FaultAction::RatePermille(rate) => {
                if rate == 0 {
                    return false;
                }
                if rate >= 1000 {
                    return true;
                }
                next_rng(&mut self.rng) % 1000 < u64::from(rate)
            }
        }
    }
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

fn seed_for_location(location: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in location {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash | 1
}

fn next_rng(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

#[cfg(test)]
mod tests {
    use super::{RuntimeFaultRule, seed_for_location};
    use rustos_fault_injection::FaultAction;

    #[test]
    fn drop_every_rule_fires_on_interval() {
        let mut rule = RuntimeFaultRule {
            location: "display.present".into(),
            action: FaultAction::DropEvery(3),
            hits: 0,
            rng: seed_for_location(b"display.present"),
        };
        assert!(!rule.next_should_fail());
        assert!(!rule.next_should_fail());
        assert!(rule.next_should_fail());
    }

    #[test]
    fn fail_after_rule_fires_after_threshold() {
        let mut rule = RuntimeFaultRule {
            location: "pci.config.read".into(),
            action: FaultAction::FailAfter(1),
            hits: 0,
            rng: seed_for_location(b"pci.config.read"),
        };
        assert!(!rule.next_should_fail());
        assert!(rule.next_should_fail());
    }
}
