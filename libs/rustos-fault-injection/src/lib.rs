#![no_std]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

/// Closed set of fault boundaries implemented by the current RustOS source.
///
/// Configuration admission and the guest runtime both reject unknown names so
/// a misspelled or retired rule cannot masquerade as exercised recovery.
pub const REGISTERED_FAULT_POINTS: &[&str] = &[
    "alloc.frame",
    "block.flush",
    "block.read",
    "block.write",
    "display.present",
    "display.provider.register",
    "handle.commit",
    "handle.reserve",
    "ipc.endpoint.enqueue",
    "ipc.endpoint.reply",
    "process.spawn",
    "waitset.register",
];

pub fn is_registered_fault_point(location: &str) -> bool {
    REGISTERED_FAULT_POINTS.contains(&location)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FaultRule {
    pub location: String,
    pub action: FaultAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FaultAction {
    Fail,
    Off,
    DropEvery(u32),
    FailAfter(u64),
    RatePermille(u16),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseFaultRuleError {
    Empty,
    MissingEquals,
    InvalidLocation,
    InvalidAction,
    InvalidNumber,
    RateOutOfRange,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstallFaultRulesError {
    Parse(ParseFaultRuleError),
    UnknownLocation(String),
    DuplicateLocation(String),
    OutOfMemory,
}

impl fmt::Display for InstallFaultRulesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => write!(f, "{error}"),
            Self::UnknownLocation(location) => {
                write!(f, "unknown fault location {location}")
            }
            Self::DuplicateLocation(location) => {
                write!(f, "duplicate fault location {location}")
            }
            Self::OutOfMemory => f.write_str("fault rule installation ran out of memory"),
        }
    }
}

impl fmt::Display for ParseFaultRuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("fault rule is empty"),
            Self::MissingEquals => f.write_str("fault rule must be location=action"),
            Self::InvalidLocation => f.write_str("fault location contains an invalid character"),
            Self::InvalidAction => f.write_str("fault action is not supported"),
            Self::InvalidNumber => f.write_str("fault action number is invalid"),
            Self::RateOutOfRange => f.write_str("fault action rate must be in 0..=1000 permille"),
        }
    }
}

pub fn parse_rules(spec: &str) -> Result<Vec<FaultRule>, ParseFaultRuleError> {
    let mut rules = Vec::new();
    for item in spec.split([';', ',', '\n']) {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            continue;
        }
        rules.push(parse_rule(trimmed)?);
    }
    Ok(rules)
}

pub fn parse_rule(spec: &str) -> Result<FaultRule, ParseFaultRuleError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(ParseFaultRuleError::Empty);
    }
    let Some((location, action)) = spec.split_once('=') else {
        return Err(ParseFaultRuleError::MissingEquals);
    };
    let location = location.trim();
    let action = action.trim();
    if location.is_empty() || !location.bytes().all(is_location_byte) {
        return Err(ParseFaultRuleError::InvalidLocation);
    }
    Ok(FaultRule {
        location: location.to_string(),
        action: parse_action(action)?,
    })
}

fn parse_action(action: &str) -> Result<FaultAction, ParseFaultRuleError> {
    match action {
        "fail" => return Ok(FaultAction::Fail),
        "off" => return Ok(FaultAction::Off),
        _ => {}
    }
    if let Some(value) = action.strip_prefix("drop-every:") {
        let value = parse_u32(value)?;
        if value == 0 {
            return Err(ParseFaultRuleError::InvalidNumber);
        }
        return Ok(FaultAction::DropEvery(value));
    }
    if let Some(value) = action.strip_prefix("fail-after:") {
        return Ok(FaultAction::FailAfter(parse_u64(value)?));
    }
    if let Some(value) = action.strip_prefix("rate:") {
        let value = parse_u16(value)?;
        if value > 1000 {
            return Err(ParseFaultRuleError::RateOutOfRange);
        }
        return Ok(FaultAction::RatePermille(value));
    }
    Err(ParseFaultRuleError::InvalidAction)
}

fn is_location_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'/')
}

fn parse_u16(value: &str) -> Result<u16, ParseFaultRuleError> {
    value
        .parse::<u16>()
        .map_err(|_| ParseFaultRuleError::InvalidNumber)
}

fn parse_u32(value: &str) -> Result<u32, ParseFaultRuleError> {
    value
        .parse::<u32>()
        .map_err(|_| ParseFaultRuleError::InvalidNumber)
}

fn parse_u64(value: &str) -> Result<u64, ParseFaultRuleError> {
    value
        .parse::<u64>()
        .map_err(|_| ParseFaultRuleError::InvalidNumber)
}

struct RuntimeFaultRule {
    location: String,
    action: FaultAction,
    hits: u64,
    rng: u64,
}

static RUNTIME_ENABLED: AtomicBool = AtomicBool::new(false);
static RUNTIME_RULES: Mutex<Vec<RuntimeFaultRule>> = Mutex::new(Vec::new());

/// Atomically replaces the process-local development fault campaign.
///
/// Each linked image gets its own rule engine. The kernel initializes its
/// instance from the QEMU-only transport, while a service may initialize its
/// own instance from an owner-controlled launch contract. Invalid or duplicate
/// rules leave the previous campaign untouched.
pub fn install_runtime_rules(spec: &str) -> Result<usize, InstallFaultRulesError> {
    let parsed = parse_rules(spec).map_err(InstallFaultRulesError::Parse)?;
    for (index, rule) in parsed.iter().enumerate() {
        if !is_registered_fault_point(rule.location.as_str()) {
            return Err(InstallFaultRulesError::UnknownLocation(
                rule.location.clone(),
            ));
        }
        if parsed[..index]
            .iter()
            .any(|prior| prior.location == rule.location)
        {
            return Err(InstallFaultRulesError::DuplicateLocation(
                rule.location.clone(),
            ));
        }
    }
    let mut replacement = Vec::new();
    replacement
        .try_reserve_exact(parsed.len())
        .map_err(|_| InstallFaultRulesError::OutOfMemory)?;
    for rule in parsed {
        replacement.push(RuntimeFaultRule {
            rng: seed_for_location(rule.location.as_bytes()),
            location: rule.location,
            action: rule.action,
            hits: 0,
        });
    }
    let count = replacement.len();
    *RUNTIME_RULES.lock() = replacement;
    RUNTIME_ENABLED.store(count != 0, Ordering::Release);
    Ok(count)
}

pub fn clear_runtime_rules() {
    RUNTIME_RULES.lock().clear();
    RUNTIME_ENABLED.store(false, Ordering::Release);
}

pub fn runtime_rules_enabled() -> bool {
    RUNTIME_ENABLED.load(Ordering::Acquire)
}

pub fn configured_runtime_rule_count() -> usize {
    RUNTIME_RULES.lock().len()
}

pub fn should_fail(location: &str) -> bool {
    if !runtime_rules_enabled() {
        return false;
    }
    let mut rules = RUNTIME_RULES.lock();
    for rule in rules.iter_mut() {
        if rule.location == location {
            return rule.next_should_fail();
        }
    }
    false
}

impl RuntimeFaultRule {
    fn next_should_fail(&mut self) -> bool {
        self.hits = self.hits.saturating_add(1);
        match self.action {
            FaultAction::Fail => true,
            FaultAction::Off => false,
            FaultAction::DropEvery(interval) => {
                interval != 0 && self.hits.is_multiple_of(u64::from(interval))
            }
            FaultAction::FailAfter(hits) => self.hits > hits,
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
    use super::{
        FaultAction, REGISTERED_FAULT_POINTS, clear_runtime_rules, configured_runtime_rule_count,
        install_runtime_rules, is_registered_fault_point, parse_rule, parse_rules, should_fail,
    };

    #[test]
    fn parses_fault_rule_actions() {
        assert_eq!(
            parse_rule("display.present=drop-every:20").unwrap().action,
            FaultAction::DropEvery(20)
        );
        assert_eq!(
            parse_rule("pci.config.read=fail-after:3").unwrap().action,
            FaultAction::FailAfter(3)
        );
        assert_eq!(
            parse_rule("alloc.page=rate:25").unwrap().action,
            FaultAction::RatePermille(25)
        );
        assert!(parse_rule("virtio.queue=unsupported-action").is_err());
    }

    #[test]
    fn parses_multiple_rules() {
        let rules = parse_rules("display.present=fail; pci.read=off\nvirtio.queue=rate:1")
            .expect("fault rules");
        assert_eq!(rules.len(), 3);
    }

    #[test]
    fn rejects_invalid_rules() {
        assert!(parse_rule("display present=fail").is_err());
        assert!(parse_rule("display.present=drop-every:0").is_err());
        assert!(parse_rule("display.present=rate:1001").is_err());
    }

    #[test]
    fn registered_fault_points_are_sorted_unique_and_closed() {
        assert!(
            REGISTERED_FAULT_POINTS
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        assert!(is_registered_fault_point("block.flush"));
        assert!(!is_registered_fault_point("socket.send"));
        assert!(!is_registered_fault_point("virtio-gpu.control.submit"));
    }

    #[test]
    fn runtime_campaign_replacement_is_atomic_and_deterministic() {
        clear_runtime_rules();
        assert_eq!(
            install_runtime_rules("process.spawn=fail-after:1").unwrap(),
            1
        );
        assert!(!should_fail("process.spawn"));
        assert!(should_fail("process.spawn"));
        assert!(install_runtime_rules("process.spawn=fail;process.spawn=off").is_err());
        assert_eq!(configured_runtime_rule_count(), 1);
        assert!(should_fail("process.spawn"));
        clear_runtime_rules();
        assert_eq!(configured_runtime_rule_count(), 0);
        assert!(!should_fail("process.spawn"));
    }
}
