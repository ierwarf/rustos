use super::*;

#[test]
fn netd_session_policy_is_exact_idempotent_and_stale_safe() {
    assert_eq!(
        admit_dvm_session_transition(0, 7, NETD_DVM_SESSION_GRANT),
        Ok(DvmSessionTransition::Grant)
    );
    assert_eq!(
        admit_dvm_session_transition(7, 7, NETD_DVM_SESSION_GRANT),
        Ok(DvmSessionTransition::Grant)
    );
    assert_eq!(
        admit_dvm_session_transition(7, 9, NETD_DVM_SESSION_GRANT),
        Err(libc::EBUSY)
    );
    assert_eq!(
        admit_dvm_session_transition(7, 9, NETD_DVM_SESSION_REVOKE),
        Err(libc::ESTALE)
    );
    assert_eq!(
        admit_dvm_session_transition(7, 7, NETD_DVM_SESSION_REVOKE),
        Ok(DvmSessionTransition::Revoke)
    );
}
