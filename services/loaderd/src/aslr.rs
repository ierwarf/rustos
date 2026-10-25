//! Loader-owned placement policy. Entropy comes only from the capability-gated
//! boot CSPRNG substrate; failures abort the launch rather than fixing a base.

use rustos_user_abi::syscall::{
    PROC_BROKER_STACK_ASLR_BYTES, PROC_BROKER_USER_SPACE_END_EXCLUSIVE, SYS_RUSTOS_ENTROPY_BROKER,
};

pub const MAIN_START: u64 = 1 << 21;
pub const IMAGE_WINDOW_BYTES: u64 = 1 << 36;
pub const INTERPRETER_START: u64 = 1 << 37;
pub const IMAGE_ALIGNMENT: u64 = 1 << 21;

/// Draw uniformly from a power-of-two number of aligned slots. Masking is
/// unbiased here; arbitrary modulo reduction of CSPRNG bytes is not used.
pub fn offset_from_entropy(start: u64, span: u64, alignment: u64, entropy: u64) -> Option<u64> {
    if !alignment.is_power_of_two()
        || !span.is_power_of_two()
        || alignment > span
        || start & (alignment - 1) != 0
    {
        return None;
    }
    start.checked_add((entropy & (span / alignment - 1)) * alignment)
}

pub fn random_offset(start: u64, span: u64, alignment: u64) -> Result<u64, i32> {
    let mut entropy = 0_u64;
    let result = unsafe {
        rustos_svc_runtime::syscall::syscall2(
            SYS_RUSTOS_ENTROPY_BROKER,
            core::ptr::addr_of_mut!(entropy) as u64,
            core::mem::size_of::<u64>() as u64,
        )
    };
    if result != core::mem::size_of::<u64>() as i64 {
        return Err(if result < 0 { (-result) as i32 } else { 5 });
    }
    offset_from_entropy(start, span, alignment, entropy).ok_or(8)
}

pub fn random_stack_top() -> Result<u64, i32> {
    let displacement = random_offset(0, PROC_BROKER_STACK_ASLR_BYTES, 4096)?;
    Ok(PROC_BROKER_USER_SPACE_END_EXCLUSIVE - displacement)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::syscall::proc_broker_stack_top_valid;

    #[test]
    fn image_slots_are_aligned_bounded_and_not_fixed() {
        for start in [MAIN_START, INTERPRETER_START] {
            let first = offset_from_entropy(start, IMAGE_WINDOW_BYTES, IMAGE_ALIGNMENT, 0).unwrap();
            let last =
                offset_from_entropy(start, IMAGE_WINDOW_BYTES, IMAGE_ALIGNMENT, u64::MAX).unwrap();
            assert_eq!(first, start);
            assert_eq!(last, start + IMAGE_WINDOW_BYTES - IMAGE_ALIGNMENT);
            assert_ne!(first, last);
            assert_eq!(last & (IMAGE_ALIGNMENT - 1), 0);
        }
        assert!(MAIN_START + IMAGE_WINDOW_BYTES < INTERPRETER_START);
    }

    #[test]
    fn all_stack_slots_preserve_the_broker_window_and_guard_room() {
        for slot in 0..PROC_BROKER_STACK_ASLR_BYTES / 4096 {
            let displacement =
                offset_from_entropy(0, PROC_BROKER_STACK_ASLR_BYTES, 4096, slot).unwrap();
            assert!(proc_broker_stack_top_valid(
                PROC_BROKER_USER_SPACE_END_EXCLUSIVE - displacement
            ));
        }
        for top in [
            0,
            PROC_BROKER_USER_SPACE_END_EXCLUSIVE + 4096,
            PROC_BROKER_USER_SPACE_END_EXCLUSIVE - PROC_BROKER_STACK_ASLR_BYTES,
            PROC_BROKER_USER_SPACE_END_EXCLUSIVE - 1,
        ] {
            assert!(!proc_broker_stack_top_valid(top));
        }
    }

    #[test]
    fn malformed_slot_ranges_fail_instead_of_biasing_entropy() {
        assert_eq!(offset_from_entropy(1, 4096, 4096, 0), None);
        assert_eq!(offset_from_entropy(0, 8191, 4096, 0), None);
        assert_eq!(offset_from_entropy(0, 4096, 8192, 0), None);
        assert_eq!(offset_from_entropy(u64::MAX - 4095, 8192, 4096, 1), None);
    }
}
