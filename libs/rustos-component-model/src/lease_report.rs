//! Generated-IDL (schema 522) post-init service lease report: the exact
//! (service id, pid, executable path) a supervisor tells rootd about a child.
//! The IDL only validates canonical bytes; rootd still authenticates the
//! kernel-stamped sender, the reporter ancestry and the catalog path.
use crate::generated_idl::SERVICE_LEASE_REPORT;
use crate::idl::{Builder, IdlError, Message};
use alloc::string::String;
use alloc::vec::Vec;

pub const MAX_PATH: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeaseReport {
    pub service: u64,
    pub pid: u64,
    pub exec_path: String,
}

fn valid(service: u64, pid: u64, path: &str) -> bool {
    service != 0 && pid != 0 && !path.is_empty() && path.len() <= MAX_PATH
        && !path.as_bytes().contains(&0)
}

pub fn encode(service: u64, pid: u64, exec_path: &str) -> Result<Vec<u8>, IdlError> {
    if !valid(service, pid, exec_path) {
        return Err(IdlError::Malformed);
    }
    let mut b = Builder::new(SERVICE_LEASE_REPORT)?;
    b.field(1, &service.to_le_bytes())?;
    b.field(2, &pid.to_le_bytes())?;
    b.field(3, exec_path.as_bytes())?;
    b.finish(pid)
}

pub fn decode(bytes: &[u8]) -> Result<LeaseReport, IdlError> {
    let m = Message::decode(SERVICE_LEASE_REPORT, bytes)?;
    let service = m.field(1).ok_or(IdlError::Missing)?.as_u64()?;
    let pid = m.field(2).ok_or(IdlError::Missing)?.as_u64()?;
    let path = m.field(3).ok_or(IdlError::Missing)?.as_str()?;
    if m.header().request_id != pid || !valid(service, pid, path) {
        return Err(IdlError::Malformed);
    }
    let mut exec_path = String::new();
    exec_path.try_reserve_exact(path.len()).map_err(|_| IdlError::OutOfMemory)?;
    exec_path.push_str(path);
    Ok(LeaseReport { service, pid, exec_path })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_and_malformed_inputs_fail_closed() {
        let wire = encode(3, 77, "services/netd/netd.elf").unwrap();
        let report = decode(&wire).unwrap();
        assert_eq!((report.service, report.pid, report.exec_path.as_str()),
            (3, 77, "services/netd/netd.elf"));
        assert!(encode(0, 1, "x").is_err());
        assert!(encode(1, 0, "x").is_err());
        assert!(encode(1, 1, "").is_err());
        assert!(encode(1, 1, "a\0b").is_err());
        let mut longer = wire.clone(); longer.push(0);
        assert!(decode(&longer).is_err());
        assert!(decode(&wire[..wire.len() - 1]).is_err());
        let mut skew = wire.clone(); skew[16] ^= 1;
        assert!(decode(&skew).is_err());
        assert!(decode(&[0u8; 40]).is_err());
    }
}
