// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this old ring0 implementation as source material for userspace services; do not restore it without an explicit privileged-boundary decision.

// mod api;
// mod dispatch;
// 
// pub(crate) use api::Api;
// pub(crate) use dispatch::dispatch_syscall;
