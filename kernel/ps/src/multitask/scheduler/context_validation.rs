//! Stable diagnostics for saved-context validation failures.
//!
//! The scheduler owns frame validation. This small table owns only the
//! allocation-free reason-code ABI consumed by SMP qualification logs.

/// Stable, allocation-free codes for the only failures emitted by
/// `Scheduler::validate_saved_context`. Keep this exhaustive: adding a new
/// validation branch without a code is an internal scheduler-contract change,
/// not an ordinary logging change.
pub(super) fn context_validation_reason_code(reason: &'static str) -> u8 {
    match reason {
        "saved context pointer is outside the task stack" => 1,
        "kernel stack guard was corrupted" => 2,
        "saved context pointer is invalid" => 3,
        "saved rflags lost the reserved bit" => 4,
        "kernel task cannot return directly to user mode" => 5,
        "user return frame carries an unexpected stack selector" => 6,
        "user return frame points outside user space" => 7,
        "saved code selector does not match any supported return mode" => 8,
        "kernel return RIP is not canonical" => 9,
        "kernel return RIP points into user space" => 10,
        "kernel return RIP points into scheduler storage" => 11,
        "kernel return frame has an invalid stack layout" => 12,
        "kernel return RSP does not belong to the task stack" => 13,
        _ => panic!(
            "scheduler context validation emitted an unregistered diagnostic reason: {reason}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::context_validation_reason_code;

    #[test]
    fn context_validation_failure_codes_are_stable_and_exhaustive() {
        assert_eq!(
            context_validation_reason_code("saved context pointer is outside the task stack"),
            1
        );
        assert_eq!(
            context_validation_reason_code("kernel stack guard was corrupted"),
            2
        );
        assert_eq!(
            context_validation_reason_code("saved context pointer is invalid"),
            3
        );
        assert_eq!(
            context_validation_reason_code("saved rflags lost the reserved bit"),
            4
        );
        assert_eq!(
            context_validation_reason_code("kernel task cannot return directly to user mode"),
            5
        );
        assert_eq!(
            context_validation_reason_code(
                "user return frame carries an unexpected stack selector"
            ),
            6
        );
        assert_eq!(
            context_validation_reason_code("user return frame points outside user space"),
            7
        );
        assert_eq!(
            context_validation_reason_code(
                "saved code selector does not match any supported return mode"
            ),
            8
        );
        assert_eq!(
            context_validation_reason_code("kernel return RIP is not canonical"),
            9
        );
        assert_eq!(
            context_validation_reason_code("kernel return RIP points into user space"),
            10
        );
        assert_eq!(
            context_validation_reason_code("kernel return RIP points into scheduler storage"),
            11
        );
        assert_eq!(
            context_validation_reason_code("kernel return frame has an invalid stack layout"),
            12
        );
        assert_eq!(
            context_validation_reason_code("kernel return RSP does not belong to the task stack"),
            13
        );
    }
}
