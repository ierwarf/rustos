# AI Kernel API Map

Use this before `docs/api/kernel.md`.

Rule: cross-crate kernel calls should go through `kernel_*::api`.

| Need | Import | Read |
| --- | --- | --- |
| GDT/IDT/ACPI/PIC/RTC/SIMD/hooks | `kernel_hal::api as hal_api` | `kernel/hal/src/api.rs` |
| Heap/paging/frames/higher-half | `kernel_mm::api as mm_api` | `kernel/mm/src/api.rs` |
| Handles/rights/session ids | `kernel_object::api as object_api` | `kernel/object/src/api.rs` |
| Shared memory regions | `kernel_ipc_runtime::api as ipc_api` | `kernel/ipc-runtime/src/api.rs` |
| Scheduler/process/user state | `kernel_ps::api as ps_api` | `kernel/ps/src/api.rs` |
| VFS/devices/console/drivers/input/USB | `kernel_io_manager::api as io_api` | `kernel/io-manager/src/api.rs` |
| Linux/Windows compat/syscalls | `kernel_compat::api as compat_api` | `kernel/compat/src/api.rs` |
| Boot orchestration/hooks | `kernel_executive::boot` | `kernel/executive/src/boot.rs`, `kernel/executive/src/lib.rs` |
| Logging/panic/boot trace | `nucleus_core::debug` | `kernel/nucleus-core/src/debug/mod.rs` |

Boot order from `kernel/src/main.rs`:

1. `hal_api::disable_interrupts`
2. `debug::boot_trace::init`
3. `hal_api::init_gdt`
4. `hal_api::init_idt`
5. `mm_api::init_paging`
6. `hal_api::enter_higher_half`
7. `hal_api::call_with_stack`
8. `boot::kernel_main_bootstrap`

Do not reorder without reading `kernel/src/main.rs` and `kernel/executive/src/boot.rs`.

High-risk APIs:

- `unsafe` boot transfer helpers: `enter_higher_half`, `call_with_stack`.
- user-memory IO: `read_to_current_user`, `read_to_user`, `ioctl_from_user`.
- process state mutation: `with_current_user_process_state_mut`, `with_process_state_by_pid_mut`.
- VFS mount/unmount/open path helpers: require current process context.

Docs:

- Human reference: `docs/api/kernel.md`.
- AI contract: `docs/ai/contracts.md`.
