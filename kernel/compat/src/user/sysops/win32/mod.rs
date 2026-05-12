// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this old ring0 implementation as source material for userspace services; do not restore it without an explicit privileged-boundary decision.

// mod console;
// mod constants;
// mod memory;
// mod runtime;
// mod sync;
// mod util;
// 
// pub(crate) use self::console::{
//     close_handle, get_console_mode, read_file, set_console_mode, sleep, write_file,
// };
// pub(crate) use self::constants::{ERROR_INVALID_FUNCTION, HANDLE_PROCESS_HEAP};
// pub(crate) use self::memory::{virtual_alloc, virtual_free, virtual_protect, virtual_query};
// pub(crate) use self::runtime::set_last_error;
// pub(crate) use self::sync::exit_process;
