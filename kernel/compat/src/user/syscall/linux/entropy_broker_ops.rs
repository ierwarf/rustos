// RING3-MIGRATION-REFERENCE START: entropy policy (Linux flags, blocking and
// per-object admission) remains in user services. Ring0 retains only the
// boot-seeded CSPRNG and bounded user-copy substrate.
use super::*;

use rustos_user_abi::syscall::{ServiceCapability,
    };

const ENTROPY_BROKER_MAX_BYTES: usize = SYSCALL_OFFLOAD_PAYLOAD_CAPACITY;

fn validate_entropy_request(out_ptr: u64, len: u64) -> Result<usize, i64> {
    let len = usize::try_from(len).map_err(|_| LINUX_EINVAL)?;
    if len == 0 {
        return Ok(0);
    }
    if out_ptr == 0 || len > ENTROPY_BROKER_MAX_BYTES {
        return Err(LINUX_EINVAL);
    }
    Ok(len)
}

pub(super) fn syscall_linux_rustos_entropy_broker(out_ptr: u64, len: u64) -> u64 {
    linux_result((|| {
        require_any_service_capability(&[
            ServiceCapability::LinuxSyscallPolicy,
            ServiceCapability::NetPolicy,
            ServiceCapability::ProcessLoader,
        ])?;
        let len = validate_entropy_request(out_ptr, len)?;
        if len == 0 {
            return Ok(0);
        }
        let mut bytes = alloc::vec![0_u8; len];
        nucleus_core::util::random::Random::new().fill_bytes(&mut bytes);
        write_user_bytes(out_ptr, &bytes)?;
        Ok(len as u64)
    })())
}
// RING3-MIGRATION-REFERENCE END: bounded entropy substrate.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entropy_copyout_is_zero_safe_and_strictly_bounded() {
        assert_eq!(validate_entropy_request(0, 0), Ok(0));
        assert_eq!(validate_entropy_request(0, 1), Err(LINUX_EINVAL));
        assert_eq!(
            validate_entropy_request(1, ENTROPY_BROKER_MAX_BYTES as u64),
            Ok(ENTROPY_BROKER_MAX_BYTES)
        );
        assert_eq!(
            validate_entropy_request(1, ENTROPY_BROKER_MAX_BYTES as u64 + 1),
            Err(LINUX_EINVAL)
        );
    }
}
