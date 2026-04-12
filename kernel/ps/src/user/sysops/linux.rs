pub const fn linux_signal_bit(signal: u64) -> Option<u64> {
    if signal == 0 || signal > 64 {
        None
    } else {
        Some(1_u64 << (signal - 1))
    }
}
