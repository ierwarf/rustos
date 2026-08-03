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
| Endpoints, replies, transferred handles, shared regions | `kernel_ipc_runtime::api as ipc_api` | `kernel/ipc-runtime/src/api.rs` |
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
  Exec uses the staged lifecycle APIs: reserve the exact scheduler target,
  stage under ProcessState alone, publish under Scheduler alone, then finalize
  visibility. No caller may nest either lock or retire an exec-reserved target;
  exit is latched as `exit_pending`.
- **Scheduler wait primitives:** use `current_task_id`,
  `arm_block_current_task`, condition publication/recheck,
  `commit_block_current_task_and_yield`, and `wake_task`. The commit and
  software reschedule trap are one interrupt-excluded transition; callers
  cannot split them with `commit_block_current_task(); yield_now()`. HAL
  snapshots registered function pointers and releases its hook-registry guard
  before invoking any callback. An expired RTC waiter remains published and is
  re-notified on later ticks until the resumed task disarms its exact task
  record; notification alone never destroys the last deadline authority. Direct
  `block_current_task`/`block_current_user_task` entry points do not exist.
  Retired user slots are intentionally unreapable until executive
  housekeeping completes the exact
  `RetiredTaskCleanup`/`cleanup_retired_task_runtime_state` handshake; new
  task-scoped waiter registries must join that cleanup boundary. The retirement
  value also freezes `clear_child_tid` and the Linux robust-list head/length so
  forced and exec-sibling cleanup does not depend on current-task state.
  RustOS now boots admitted APs, publishes disjoint per-CPU scheduler/syscall
  state, uses fixed reschedule/TLB vectors, and arms one local TSC-deadline
  clockevent per AP. Commercial admission still requires compatibility
  topology/affinity ABI plus complete 1/2/4/8 runtime and recovery evidence.
- **Raw spin critical sections:** use
  `nucleus_core::util::lockdep::TrackedSpinLock` with one stable class per
  logical lock type. On the BSP boot image its guard increments the task
  preemption depth but leaves unrelated device interrupts enabled. Guarded work
  must be bounded, non-blocking, and allocation-free; destroy removed objects
  after releasing the guard. IDT leaves enter tracked IRQ context before taking
  a class: the graph rejects a class used from both IRQ and ordinary
  interrupt-enabled context and rejects IRQ-safe-to-IRQ-unsafe dependency
  paths. `KernelWaitLock` uses stable task-owned classes in the same
  allocation-free dependency graph. The scheduler publishes the current task
  identity before every handoff and sleepable nesting survives blocking.
  Blocking acquisition while a raw class is held is forbidden. A bounded raw
  leaf may be taken after a sleepable lock; that ordering is recorded and
  cycle-checked, and nonblocking external try-locks join the same raw graph.
  A wait lock may not be acquired from IRQ context. Host behavior is selected
  by absence of `rustos_boot_image`, not by `target_os`.
- **IPC object admission:** process/task endpoint creation must use the
  owner-aware API and process-backed display/DRM regions must use
  `ipc_api::shared_region::create_for_process`. Owner quota is reserved before
  allocation/publication. A shared-region drop does not return quota; deferred
  physical reclaim does. Kernel/anonymous bootstrap objects use the explicit
  ownerless entry points and remain within their separately reserved global
  capacity.
- **DVM block transport:** use `kernel_io_manager::api::block::{dvm_info,
  submit_dvm_read,submit_dvm_write,submit_dvm_flush,poll_dvm,cancel_dvm,
  finish_dvm}` only from the `STORAGE_POLICY`-gated compat broker. MSI-X code
  calls only the wake leaf; `storaged` owns timeout, retry, durability, and
  volume policy. Do not expose the raw ivshmem physical range to a service or
  route ordinary VFS callers directly to this API.
  Initial admission and post-revoke rebind both require an L0 Ed25519 epoch
  signature under the verifying key in the signed early-system header; shared
  memory generation values alone are never restart authority.
- **Bootstrap image:** `kernel_io_manager::api::vfs::{read_path_to_vec_for_kernel,
  boot_path_file_len_for_kernel}` and
  `kernel_io_manager::api::block::read_bootstrap_file_range` may read only the
  immutable early-system allowlist. No physical block descriptor, extent,
  partition, FAT, DMA, AHCI, or NVMe API exists in io-manager.
- **Input poll waits:** compat `poll()` only asks inputd's deadline-bounded
  `STATS` request and arms `input::event_queue` waiters. Only inputd's
  capability-gated ingest broker drains the bounded L0-owned input ring. The
  one MSI-X leaf only wakes; it never decodes. There is no USB, PS/2, serial,
  or native fallback. Compat process-exit cleanup invokes
  `input::transport::withdraw_policy_consumer` only when the live inputd
  endpoint owner exits; this clears producer admission and old-owner records
  without moving decode or input policy into ring0.
- **DVM transport lifecycle:** display submit/query and input drain acquire an
  exact `TransportClaim` for the current epoch before touching shared memory.
  Revoke first closes admission (`Active -> Draining`), waits boundedly for
  `in_flight == 0`, and only then resets slots, mappings, or cursors and
  publishes the next epoch. IRQ leaves request revoke but never wait.
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
