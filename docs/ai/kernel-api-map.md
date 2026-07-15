# AI Kernel API Map

Use this before `docs/api/kernel.md`.

**Rule:** cross-crate kernel calls go through `kernel_*::api`. Do not reach
into another crate's private modules when `api.rs` exposes a wrapper.

## API surfaces

| Need | Import | Read |
| --- | --- | --- |
| GDT/IDT/ACPI/PIC/MSI-X receive substrate/RTC/SIMD/hooks | `kernel_hal::api as hal_api` | `kernel/hal/src/api.rs` |
| Heap/paging/frames/higher-half | `kernel_mm::api as mm_api` | `kernel/mm/src/api.rs` |
| Handles/rights/session ids | `kernel_object::api as object_api` | `kernel/object/src/api.rs` |
| Shared memory regions | `kernel_ipc_runtime::api as ipc_api` | `kernel/ipc-runtime/src/api.rs` |
| Scheduler/process/user state | `kernel_ps::api as ps_api` | `kernel/ps/src/api.rs` |
| VFS/devices/console/drivers/input/USB | `kernel_io_manager::api as io_api` | `kernel/io-manager/src/api.rs` |
| Linux/Windows compat/syscalls | `kernel_compat::api as compat_api` | `kernel/compat/src/api.rs` |
| Boot orchestration/hooks | `kernel_executive::boot` | `kernel/executive/src/boot.rs`, `kernel/executive/src/lib.rs` |
| Logging/panic/boot trace | `nucleus_core::debug` | `kernel/nucleus-core/src/debug/mod.rs` |

## Boot order

From `kernel/src/main.rs`:

1. `hal_api::disable_interrupts`
2. `debug::boot_trace::init`
3. `hal_api::init_gdt`
4. `hal_api::init_idt`
5. `mm_api::init_paging`
6. `hal_api::enter_higher_half`
7. `hal_api::call_with_stack`
8. `boot::kernel_main_bootstrap`

Do not reorder without reading `kernel/src/main.rs` and
`kernel/executive/src/boot.rs`.

## High-risk APIs

- **`unsafe` boot transfer:** `enter_higher_half`, `call_with_stack`.
- **User-memory IO:** `read_to_current_user`, `read_to_user`,
  `ioctl_from_user`.
- **Process state mutation:** `with_current_user_process_state_mut`,
  `with_process_state_by_pid_mut`.
- **Scheduler wait primitives:** use `current_task_id`, `block_current_task`,
  `wake_task` for kernel-capable wait queues; use `*_user_*` wrappers only
  for userspace-task waits.
- **Input poll waits:** compat `poll()` only asks inputd's deadline-bounded
  `STATS` request and arms `input::event_queue` waiters. Only inputd's
  capability-gated ingest broker drains the bounded L0-owned input ring. The
  one MSI-X leaf only wakes; it never decodes. There is no USB, PS/2, serial,
  or native fallback.
- **Scheduler preemption:** `cond_resched`/`reschedule_if_requested` only at
  Linux-style safe points outside spinlocked or IRQ-off regions. Timer IRQs
  should request reschedule for user-task kernel frames, not blindly switch
  away from arbitrary kernel code.
- **Scheduler fairness:** keep the hardware timer tick fixed and route
  service weights into vruntime/load accounting only. Root slot 0 is a fair
  task during bootstrap finalize, then becomes the idle fallback after
  `mark_root_idle()`.
- **MSI-X/device events:** allocate only a bounded vector through
  `hal_api::arch::msi`, register a lock-free leaf callback once, and program a
  masked PCI MSI-X table before enabling it. The IRQ callback may set pending
  state only; display/input/network policy and buffer work stay in their
  owning service/broker turn. x2APIC or interrupt-remapping absence fails
  closed rather than truncating a destination ID.
- **VFS mount/unmount/open path helpers:** require current process context.

## Docs

- Human reference: `docs/api/kernel.md`.
- AI contract: `docs/ai/contracts-infra.md` (Kernel API section).
