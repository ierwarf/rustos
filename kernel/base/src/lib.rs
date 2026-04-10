#![feature(abi_x86_interrupt)]
#![feature(alloc_error_handler)]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub use kernel_lowlevel as lowlevel;

#[path = "../../hal/src/arch/mod.rs"]
pub mod arch;
#[path = "debug/mod.rs"]
pub mod debug;
#[path = "../../io-manager/src/internal/driver/mod.rs"]
pub mod driver;
#[path = "../../io-manager/src/internal/input/mod.rs"]
pub mod input;
#[path = "../../io-manager/src/internal/input_core.rs"]
pub mod input_core;
#[path = "../../io-manager/src/internal/io/mod.rs"]
pub mod io;
#[path = "../../ipc-runtime/src/internal/ipc/mod.rs"]
pub mod ipc;
#[path = "../../ipc-runtime/src/internal/ipc_core.rs"]
pub mod ipc_core;
#[path = "../../mm/src/internal_lowlevel/memory/mod.rs"]
pub mod memory;
#[path = "../../ps/src/internal_lowlevel/multitask/mod.rs"]
pub mod multitask;
#[path = "object_tokens.rs"]
pub mod object_tokens;
#[path = "../../compat/src/internal/user/linux.rs"]
pub mod process_linux;
#[path = "../../compat/src/internal/user/process_state.rs"]
pub mod process_state;
#[path = "../../../settings.rs"]
pub mod settings;
#[path = "../../io-manager/src/internal/storage/mod.rs"]
pub mod storage;
#[cfg(test)]
#[path = "test_support.rs"]
pub mod test_support;
#[path = "../../io-manager/src/internal/usb/mod.rs"]
pub mod usb;
#[path = "../../compat/src/internal/user/mod.rs"]
pub mod user;
#[path = "../../compat/src/internal/user/windows.rs"]
pub mod user_windows;
#[path = "user_abi.rs"]
pub mod user_abi;
#[path = "util/mod.rs"]
pub mod util;
#[path = "../../io-manager/src/internal/vfs/mod.rs"]
pub mod vfs;
#[path = "../../io-manager/src/internal/vfs_core.rs"]
pub mod vfs_core;
