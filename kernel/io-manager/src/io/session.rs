// RING3-MIGRATION-REFERENCE START: sessiond owns console-session lifecycle
// and routing policy. Ring0 shares one compact handle representation.
pub use kernel_object::api::session::ConsoleSessionHandle;
// RING3-MIGRATION-REFERENCE END: sessiond-owned console session handle substrate.
