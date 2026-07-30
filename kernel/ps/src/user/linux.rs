// RING3-MIGRATION-REFERENCE START: Linux ABI constants are shared ABI substrate.
// Policy belongs in syscalld/loaderd/procd; this file is a re-export only.
// ABI: Keeping one re-export boundary prevents kernel policy from forking ABI
// definitions; individual build profiles use different subsets.
#![allow(dead_code)]

pub use rustos_user_abi::linux::*;
// RING3-MIGRATION-REFERENCE END: service-owned Linux ABI policy.
