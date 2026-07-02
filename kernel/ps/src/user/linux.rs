// RING3-MIGRATION-REFERENCE START: Linux ABI constants are shared ABI substrate.
// Policy belongs in syscalld/loaderd/procd; this file is a re-export only.
#![allow(dead_code)]

pub use rustos_user_abi::linux::*;
// RING3-MIGRATION-REFERENCE END: service-owned Linux ABI policy.
