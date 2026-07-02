use crate::user::linux as linux_abi;

pub const fn linux_signal_bit(signal: u64) -> Option<u64> {
    if signal == 0 || signal > linux_abi::MAX_SIGNAL_NUMBER as u64 {
        return None;
    }
    Some(1_u64 << (signal - 1))
}
