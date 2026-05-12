// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this old ring0 implementation as source material for userspace services; do not restore it without an explicit privileged-boundary decision.

// use crate::multitask;
// 
// pub(crate) fn exit_process(_exit_code: u64) -> u64 {
//     multitask::exit_current_user_task()
// }
