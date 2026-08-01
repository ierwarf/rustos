//! Persistent epoll interest-control deadline classification.
//!
//! - **Owner:** Compat classifies the vfsd mutation; vfsd owns interest state.
//! - **Boundary:** Create, ADD, MOD, DEL, retire, and purge are mutations, not readiness probes.
//! - **Lifecycle:** One exact operation identity is retained across bounded retries and recovery.
//! - **Concurrency:** The caller's open-description guard spans the complete foreground mutation.
//! - **Failure:** Deadline expiry cancels the exact reply and leaves replay state explicit.
//! - **Forbidden:** Never charge a state-changing epoll operation to the 16 ms readiness rail.
//! - **Evidence:** `waitset` and `service-mutation-recovery`.

pub(super) const fn deadline_ms() -> u64 {
    rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_epoll_mutation_uses_the_interactive_deadline() {
        assert_eq!(
            rustos_user_abi::performance::IPC_READINESS_QUERY_HARD_LIMIT_MS,
            16
        );
        assert_eq!(deadline_ms(), 100);
        assert!(deadline_ms() > rustos_user_abi::performance::IPC_READINESS_QUERY_HARD_LIMIT_MS);
    }
}
