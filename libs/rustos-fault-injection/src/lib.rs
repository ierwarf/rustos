#![no_std]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

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

#[cfg(test)]
mod tests {
    use super::{FaultAction, parse_rule, parse_rules};

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
}
