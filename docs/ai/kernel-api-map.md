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

Page-cache victim selection, COW/writeback policy, and backing-generation
authority are not kernel APIs. They belong to `pagerd`; kernel-mm exposes only
opaque generation-bound all-atomic frame grants, normal-time pager-leaf
preparation, exception-time fixed-ledger PTE installation, page-table
transactions, and exact TLB-acknowledgement mechanisms required to commit or
reclaim the service's admitted decision. Physical frame numbers never cross the
pager boundary.
`kernel-ps` separately owns the all-atomic pager VMA publication: syscall-time
writers publish/revoke exact generations, while exception-time readers perform
only bounded atomic lookup and never acquire `ProcessStateLock`.  The public
target-process publish/revoke APIs retain the exact PID slot and stamp its live
process/MM generation before accepting an unbound region template; they are
broker-time only and never appear in the fault path. The bounded extent
prefilters recheck the same even per-slot sequence after reading relaxed
bounds; an odd or changed sequence delegates to the full snapshot instead of
returning a false negative from stale bounds. Range rewrites also retain a
per-process, non-wrapping consistency epoch from first withdrawal through PTE
mutation and every replacement or original rollback. Fault lookup encloses the
whole scan (including misses/permission errors) in that epoch; an odd/changed
interval returns `Unstable`, not a terminal `Stale` page fault. Readers do not
spin or acquire the writer lock. This follows the complete-read-interval rule
in Linux's [sequence counter documentation](https://docs.kernel.org/locking/seqlock.html).
An overlapping task-identity publication may use only the same CPU's original
Current-bank joined binding, as ordinary user copy already does; neither an
absent publication nor a scalar slot grants process/MM authority. Native tests
model the earlier masked IRQ entry and exercise the actual binding/VMA bodies,
not privileged CLI/STI hardware execution.

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
- **User-memory IO:** `kernel_ps::api::usermem::{copy_from_current_user_exact,
  copy_from_retained_user_exact, read_current_user_struct, read_current_user_u32,
  read_current_user_c_string}`, `ioctl_from_user`.
- **Process state mutation:** `with_current_user_process_state_mut`,
  `with_process_state_by_pid_mut`.
  Exec uses the staged lifecycle APIs: reserve the exact scheduler target,
  stage under ProcessState alone, publish under Scheduler alone, then finalize
  visibility. No caller may nest either lock or retire an exec-reserved target;
  exit is latched as `exit_pending`. In-place image replacement cannot rebind
  a live thread's kernel task ID to its process ID; mismatched identity refuses
  admission before sibling retirement or privileged image work. Exec/group
  policy remains in userspace.
- **CPU return custody:** `cpu_local/dispatch_turn.rs` owns unique non-migrating
  CPU/from tokens. Private scratch requires that CPU's Open cell, never the
  global diagnostic owner word. The return producer validates original Current,
  admission and remote exclusion; changed returns require exact slot/UID/Running
  generation in the active interval and stable Running custody. Unused prepared
  funding is cancelled outside the leaf; active/foreign funding remains owned.
  Changed publication keeps the local gate closed until assembly installs RSP,
  clears Transition and releases Outgoing. Duplicate commits cannot spend an
  Open producer; abandoned publication poisons instead of unsafe rollback.
  Independent `DispatchPreemption` pins CPU before the directory wait without
  blocking caller-enabled TLB/device IPIs; protected dispatch then masks IRQs
  continuously. Its unit is separate from raw held+pending accounting. Completion runs
  after raw unlock/deferred IPI, then restores caller IF only for unchanged
  returns. Changed returns park the unique unit until post-RSP Committing releases
  it without restoring outgoing IF. Cold directory/MM transactions still remain.
  Architecture consumes `ExecutionRootLease`: exact bank/context/MM-root capture
  retained outside leaves through hardware work, not a sampled boolean. A closed
  root refuses fresh capture; backing pins do not replace MM close arbitration.
- **Scheduler wait primitives:** use `current_task_id`,
  `arm_block_current_task`, condition publication/recheck,
  `commit_block_current_task_and_yield`, and `wake_task`. The commit and
  software reschedule trap are one interrupt-excluded transition; callers
  cannot split them with `commit_block_current_task(); yield_now()`. HAL
  snapshots registered function pointers and releases its hook-registry guard
  before invoking any callback. An expired RTC waiter remains published and is
  re-notified on later ticks until the resumed task disarms its exact task
  record; notification alone never destroys the last deadline authority. A
  refused wake is also only a notification attempt, not proof of task death:
  frame/CPU publication may be transient. HAL's `TaskHooks::wake_task` routes
  the same scheduler task-ID domain as deadline registration, including kernel
  reply consumers; user-qualified wake remains a separate PS API. No extra
  timer-table lock is acquired after invoking the callback. This follows the
  [condition/recheck separation of wait queues](https://docs.kernel.org/driver-api/basics.html#wait-queues-and-wake-events),
  not a claim that the abstract clock model proves CPU scheduling fairness.
  Direct `block_current_task`/`block_current_user_task` entry points do not exist.
  Retired user slots are intentionally unreapable until executive
  housekeeping completes the exact
  `RetiredTaskCleanup`/`cleanup_retired_task_runtime_state` handshake; new
  task-scoped waiter registries must join that cleanup boundary. The retirement
  Identity/lifecycle writes now use class-51 per-slot `current_identity`
  owners; metadata refresh cannot overwrite their newer gates. Four Scheduler
  flag arrays are test-only. Class-53 resource cells now own context metadata
  as well as ID/entry and allocations; production `contexts` is test-only.
  Readers copy records; narrow exact-ID exec/handle detach patches cannot replay
  unrelated fields. Cold frame/process/budget construction transactions and
  cross-owner execution/MM lifetime remain separate responsibilities. Exact
  context process-generation/PID pins now retain the physical state-lock box
  through descriptor/assembly custody; syscall own refs and direct callbacks
  acquire independent atomic pins before pointer reads. Reap closes admission
  before zero/detach and last release requests a rescan. Old exec MM/CR3/TLB
  quiescence is not proved by this allocation pin.
  Claim/ACK is now owned by independent per-slot `SchedulerRetirementCleanup`
  cells, not the Scheduler guard. The terminal directory producer freezes
  exec detach metadata and atomically publishes cleanup/kernel tokens last.
  Deferred/in-progress producers and either Pending/Claimed token prohibit
  reset. External ACK matches the full payload; the private non-Copy kernel
  claim ACKs its exact generation only after revocation/wake/detach finishes. No cell lock crosses external cleanup or user copy. The
  class-53 allocation owner also owns task ID/entry descriptors; production
  `starts` and stack Vec arrays are test-only. Exact allocation pins survive
  final claim and CPU Current/Outgoing publication; assembly releases the
  outgoing pin after installing RSP. Closing/references prohibit descriptor
  reset or UID reuse, and same-ID exec replaces entry only. Production kernel
  entry lookup is Scheduler-free: IRQ-excluded exact published identity/live
  lifecycle plus CPU allocation-bank Current, never Prepared/Outgoing or a
  catalog fallback. Scalar ABI fallback, stack-layout and console-session reads
  now use IRQ-excluded joined Current/context/start/funding identity, without
  Scheduler; copied metadata is not a retained process/MM reference. Cold runtime
  snapshots retain the SAME original Current funding interval across paired
  Context/Domain reads, even after close or donation metadata replacement. TLS
  cache and root-idle writes retain original Current banks through their last
  effect. Exact noncurrent snapshots retain an original UID loan. The unused
  `CurrentKernelStackScope` export and scalar-only setter/restore pair are removed;
  new alternate-stack users must supply original backing lifetime, not revive
  that API. Hint APIs now use independent original-binding ordering publication,
  with optional SAME Current interval loan for a non-raising floor. Process
  selection retains the original winning target; generic hints carry exact UID.
  Latency hints neither wake blocked work nor transfer class/funding. User wake
  retains its original user-qualified binding through the wake protocol. Actual
  activation uses the original Cohort/publication owner in native tests too;
  the second legacy Scheduler activation implementation is removed. Complete
  admission/exec transactions, process/MM quiescence and final slot release
  still need directory custody. Prepublication thread slots now use non-Copy
  exact UID/generation/phase resource tickets; ordinary storage/context/start
  operations cannot cross reservations. Cancel ACK follows process detach and
  storage destruction; commit ACK follows exact metadata and Blocked admission.
  Abandonment quarantines, never implicitly reopens. Production reservation and
  domain catalog arrays are gone; domain descriptors come from budget cells.
  Cross-domain/cold lifecycle arbitration remains guarded. Linux thread-state locks now have static
  owner storage; joined Current and exact TID bind callbacks. Signal hint
  synchronization remains inside that IRQ-excluded class-39 leaf, not Scheduler.
  Header diagnostics now return copied POD snapshots joined to exact published
  RSP/epoch, UID, allocation pin and bounds. Architecture return-mode inspection
  requires own Current/Prepared; unknown fails stop. Saved-header readers now
  register under the resource leaf that also executes the exact Local/Direct
  Running CAS. Nonexecuting primary observations need no directory byte-read
  exclusion; guarded reads retain allocation and final token checks. Own-CPU
  executing reads still require IRQ/preemption exclusion, and alternate boot
  backing still requires its external owner. No leaf spans copying, hardware or
  queue acquisition; a read-count check released before CAS is not exclusion.
  Affinity admission additionally joins exact descriptor UID, task/process masks
  and CPU bounds to Running CAS under Affinity54, after Identity51/Resources53.
  Writers/reset share that leaf; reset needs drained execution/no cold exec and
  removes UID authority. Provisional refusal releases custody outside leaves.
  Pending migration allows an included destination, never an excluded old CPU.
  Process-group atomic update and post-claim return/hardware arbitration remain
  genuinely directory-owned; this is not a guard-removal proof.
  Self exec requires sole exact IRQ-stabilized Current image custody before
  scheduler side effects, through new metadata and CR3/FS installation. Custody
  now extends through process-state staging and finalization in
  `scheduler/exec_transaction.rs`, coordinated by `current/exec.rs`. A non-Copy
  once-issued receipt binds the complete reservation and staged root, checks
  published root/Current CR3, and is consumed before final visibility and
  old-state release. `current_identity/exec_intent::Intent` separately binds
  slot/UID/full reservation/monotonic generation across sibling quiescence,
  image publication and typed cancellation/finalization. Unqualified gate/reset/
  image commands cannot spend it; abandonment poisons. Self stays schedulable
  through its sibling barrier, then completes after writer release before IRQ
  restoration. Cancellation closes with standalone CPU preemption while keeping
  caller-enabled IPIs serviceable during process-owner waits. Pending exit never
  reopens dispatch and consumes existing terminal retirement work.
  By-PID Current uses self exec rather than live-frame rewrite.
  Existing sibling quiescence, directory claim/admission and HAL inactive-root/
  shootdown ownership remain; the receipt alone is not an MM execution lifetime pin.
  `ProcessAddressSpace::{pin_execution,seal_execution,into_retirement_owner}`
  own checked root refs, cold close admission and payload transfer.
  `ExecutionPin::try_activate` returns a non-Copy `ActivationPin`; the shared
  atomic gate makes `seal_execution() -> bool` refuse without partial close
  while any CPU grant exists. Resource claim/architecture capture binds exact
  slot/UID/process/MM/owner currency and transfers activation to the CPU turn
  through publication/RSP ACK (or permanent poison), not just lexical CR3 work.
  Failed claim drops provisional custody outside leaves; exec staging refusal
  precedes state replacement and callers cancel the exact reservation.
  Exact process/MM-generation pins reside in resource contexts; selection and
  Current/Prepared/Outgoing retain them until context release. Architecture
  returns require bank-qualified exact open roots. Exec binds staged MM custody
  and transfers it after root/CR3 checks. Old state retires with the original
  owner so shared backing survives captured refs. Atomic last release queues
  only; the bounded IRQ-enabled drain runs HAL/ledger retirement before payload
  destruction. Existing directory/quiescence and complete claim/switch proof
  remain required; modeled host roots do not establish hardware acceptance.
  SIMD image bytes now use exact UID and exclusive RAII access independently
  of Scheduler. Save requires Current; restore additionally accepts Prepared.
  Replacement rejects Prepared/Outgoing/foreign Current and requires exact
  self or quiesced/terminal zero-reference custody. Frame-byte mutation and
  complete cross-owner CPU/image transactions remain guarded. Owned-primary
  frame construction/exec now use exclusive slot/UID-qualified writer leases;
  resource acquisition excludes frame pins and teardown, and byte callbacks
  run outside the leaf with IRQs excluded. Peer exec holds its lease through
  image/identity/TLS/MM publication and gate release. Old epochs are revoked
  before writing; frame pins recheck epoch before any byte read. Borrowed
  alternate/boot backing and old-MM quiescence retain their existing owners. The cleanup
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
