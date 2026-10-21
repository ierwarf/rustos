# Network, process policy, display and scheduler contracts

[Contract index](../contracts-abi.md).

## Network Surface (`netd`)

Routed Linux ops after bootstrap: `socket`, `socketpair`, socket `dup`/`dup2`/`dup3`, socket `close`, socket `read`/`write`/`writev`, `bind`, `listen`, `accept`/`accept4`, `connect`, `sendto`, `recvfrom`, `sendmsg`, `recvmsg`, `getsockname`, `getpeername`, `setsockopt`, `getsockopt`, `shutdown`.

netd invokes gated `SYS_RUSTOS_NET_BROKER` with target pid. Net broker arg struct carries six 64-bit syscall arg slots. Kernel performs handle install and target user-memory validation/copy; AF_UNIX socket lifecycle, binding/listen queues, byte queues, and socket option policy belong to netd. Version-7 `NetdIpcRequest`/`Response` frames copy an exact 136/32-byte header plus only the actual bounded payload, rather than a fixed maximum-size buffer. Request offset 112 carries a mandatory nonzero absolute `CLOCK_MONOTONIC` deadline; the frames also carry `socket_token`, fd `status_flags`, and a bounded inline payload for this service-owned socket path.

The producer fixes that transaction end before payload preparation and reuses
it for every foreground retry. The kernel reply wait floors the same monotonic
nanosecond value into its RTC tick domain and takes the earlier of the wire end
and its already-admitted service-class end. Netd then tightens the request once
at admission to 16 ms for readiness, 100 ms for DVM session and reference
control, or 30 s for bulk work. Queueing, worker dequeue, provider polling, and
retry may consume the stamped remainder but cannot mint another class budget.
An expired detached poll makes one terminal `ETIMEDOUT` reply attempt and
releases its slot; a reply consumed after the caller deadline is discarded,
including transferred descriptors, instead of reviving the completed call.

This deliberately combines three established microkernel properties without
copying their ABI: L4Re exposes absolute IPC timeouts and warns that a finite
receive timeout can leave a server holding a temporary reply capability;
seL4 MCS uses explicit one-shot reply objects to track reply authority and
scheduling-context donation; QNX ties synchronous send/receive/reply state to
priority inheritance. RustOS therefore keeps reply authority explicit and
one-shot, keeps scheduling donation bounded by that reply, and additionally
propagates one end instant through ring3 queues where a kernel-only relative
timeout cannot bound service-side resource ownership. See the official
[L4Re IPC documentation](https://l4re.org/doc/l4re_concepts_ipc.html),
[seL4 MCS documentation](https://docs.sel4.systems/Tutorials/mcs), and
[QNX message-passing documentation](https://qnx.com/developers/docs/8.0/com.qnx.doc.neutrino.sys_arch/topic/ipc.html).

Four fixed netd request receivers prevent an unrelated caller from serializing
all local AF_UNIX work. Blocking INET connect/send/recv waits run in a separate
fixed eight-worker pool that releases the shared network-state lock between
polls; there is no thread-per-request fallback. All socket poll/epoll waits now
query netd readiness generation, register the shared bounded wait token, and
re-query before sleeping and after wake. Because the DVM packet ring currently
exposes no userspace interrupt edge, netd owns one fixed 1 ms INET ingress
worker; one bounded smoltcp poll per turn publishes a generation only when
smoltcp reports a socket-state transition. The worker admits at most 32
ingress packets before yielding. This lets new external packets wake
an already-sleeping generic wait without moving network policy into ring0.

This removes avoidable copies and waits, but standard AF_UNIX data still makes
a synchronous compat-to-netd service round trip for every send/receive. The
current WayClick KVM gate nevertheless proves this standard path sustains the
55 FPS contract without a client-specific route. Its first accepted three
windows matched 57, 65, and 68 commit/callback/release cycles over 3.060
seconds (62.092 FPS aggregate) with a 45 ms maximum callback gap. The generic
wait-set gives ordinary Linux applications a bounded aggregate wait over
vfsd-owned interests backed by netd/inputd readiness generations, exact
provider epochs, and monotonic
deadlines. It deliberately leaves socket data and namespace/options policy in
netd and does not add a WayClick-specific route. Wayland-server's existing
backend epoll fd aggregates the changing client set; uiserver duplicates that
open description into a demoted readiness worker, merges its single-capacity
wake with the input reader's wake, and rearms only after main-loop dispatch. A
client commit can therefore wake the main loop before its runtime deadline.
Runtime/QEMU evidence remains a release gate; source/model completion alone
does not satisfy the 55 FPS gate.

## Process Policy Surface (`procd`)

procd owns Linux `execve`, `fork`/`clone`, `wait4`, `rt_sigaction`, `rt_sigprocmask`, `sigaltstack`, `tgkill`, signal selection. Procd IPC ABI v4 removes the copied cwd payload from exec requests: the raw path and `AT_FDCWD` basis continue through loaderd to vfsd, which resolves them against the target PID's namespace. It retains v3's distinct default-stop disposition and exact coalesced SIGCHLD cause snapshot; ring0 rejects a stop signal mapped to termination, a non-stop signal mapped to stop, and every stale or out-of-mask cause selection.

`wait4` routes through procd for ownership validation; kernel still performs narrow process-table wait + status/rusage copyout. `WUNTRACED` consumes one exact `(signal << 8) | 0x7f` child stop state and `WCONTINUED` consumes `0xffff`; without the matching option the state remains pending. A default stop gates every Linux task in the process while preserving its ready/block state. SIGCONT resumes all of them before its own disposition or mask is considered, and SIGKILL also removes the stop gate before termination delivery. Transient membership/job-control contention returns `EAGAIN` before signal publication; a refused Current stop preserves its pending cause for retry rather than acknowledging an uncommitted stop. Stop, continue, and exit queue process-directed SIGCHLD with exact coalesced cause bits. A receiving thread transfers those causes to a live sibling before retirement. Procd applies `SA_NOCLDSTOP` only when no exit cause coalesced, while ring0 revalidates and removes only the selected snapshot so a cause queued during the policy round trip remains pending.

Kernel keeps only: user-copy, address-space replacement, scheduler mutation, pending-signal wakeup, Linux x86_64 `rt_sigframe`/`rt_sigreturn`.

## Syscalld Residual Surface

- Per-process credentials + `RLIMIT_STACK` policy DB: source of truth for Linux-visible `get*id`, `set*id`, `prlimit64`. Kernel process credentials = gated bootstrap/security primitive; **must not** be mutated by Linux `set*id`.
- Linux `nanosleep`, `clock_gettime`, and `clock_nanosleep` use local
  fixed-envelope validation in compat's timer substrate, not syscalld policy
  IPC or per-process cached grants. Clock IDs, timespec bounds, and
  `TIMER_ABSTIME` are validated before sleep/clock work. Syscalld operations
  51–53 are retired; this hot path must not acquire shared process state or
  synchronously call a policy service (see `core.md`). `ppoll` likewise
  validates its relative timespec locally before composing the wait.
- Linux `sched_getaffinity` and `sched_setaffinity` are versioned two-owner
  operations. Ring0 resolves the target TID inside the authenticated caller
  process, snapshots its effective task mask and the exact dense commercial
  `Online` mask, and stamps `(online bitmap, popcount, observation_version,
  target_process_id, effective_task_mask)` into the syscalld request. Syscalld
  admits the ABI shape and exact target owner; ring0 re-resolves and commits
  after the policy reply. `sched_getaffinity` returns exactly eight kernel mask
  bytes, while `sched_setaffinity` follows Linux semantics by intersecting the
  requested mask with Online and rejecting only an empty result. A short
  `cpusetsize`, forged owner, empty effective mask, stale version, count
  mismatch, or task mask outside Online fails closed. Returning the whole
  Online mask for a pinned thread or fabricating CPU zero is forbidden.
- Linux futex WAIT and WAIT_BITSET accept validated relative/absolute timeout
  timespecs and block on the RTC waiter substrate; timeout-bearing futex calls
  must not return `ENOSYS` or spin in libc retry loops. Any waiter table read by
  an IRQ handler must be mutated from process context with interrupts excluded
  while its spin lock is held.
- Linux `set_robust_list` accepts only the native 24-byte robust-list head and
  stores it in the exact thread state that retirement snapshots. Ordinary,
  fault, forced, and exec-sibling retirement all run the same bounded cleanup:
  signed futex offsets and user ranges are checked, at most 2048 entries plus
  `list_op_pending` are examined, PI-tagged entries remain explicitly
  unsupported, a word changes only while its TID bits still name the retiring
  thread, `FUTEX_WAITERS` is preserved, `FUTEX_OWNER_DIED` is installed, and
  one matching waiter is woken. The live implementation retains the exact
  process mapping and uses address-space-aware acquire-load plus bounded
  AcqRel atomic user-u32 compare-exchange; a BSP read/modify/write sequence is
  forbidden.
- `exit_group` and default fatal-signal termination retire every thread in the
  target process only after publishing the process-exiting state. New thread
  attachment and process-scoped authority admission must reject that state;
  teardown may then retire siblings and complete the exact deferred runtime
  cleanup without a publication window. It must fail every thread-owned IPC
  wait and endpoint before the current thread halts. Never revoke process
  service endpoints while sibling threads remain runnable. A targeted
  task termination marks the process exiting only when that task is its last
  live thread; non-final thread termination must preserve process authority.
- uiserver opens input nonblocking and waits through bounded `poll`; RustOS must
  not replace readiness with unconditional success followed by high-rate inputd
  reads.
- `SYS_RUSTOS_SCHED_DEMOTE_SELF` is a narrow scheduler substrate for an
  already-running user thread to irreversibly surrender its inherited base
  System class and cap its permanent fair weight at `NICE_0_LOAD`. It takes no
  priority argument and cannot raise a lower weight. Live reply-scoped IPC
  inheritance remains independent until that exact reply's normal release.
  uiserver uses it for background and untrusted client-accept workers; failure
  terminates the UI process instead of letting those workers retain either
  System priority or the compositor's elevated CPU share.
- Linux `memfd_create`: policy validation in syscalld; kernel performs handle install + read/write/truncate/seal (current handles, user memory).
- Static-PIE policy services receive one kernel-mapped bootstrap heap, but its
  runtime allocator is reclaiming rather than bump-only. Every allocation owns
  an exact header-described span; `dealloc` returns that span to an
  address-ordered free list and adjacent spans coalesce. Additional anonymous
  regions remain mapped for the service generation to avoid pager re-entry,
  but are requested only after no reusable span fits. The bootstrap region is
  admitted once, a consumed allocation cookie cannot be released twice, and
  the allocator spin lock is released before the synchronous grow syscall.
  Therefore a syscalld/pager wait cannot make every other service thread spin
  behind the heap lock, and cumulative UI, IPC, and parsing traffic is bounded
  by peak live service memory rather than process lifetime traffic.
- `syscalld` owns the post-bootstrap Linux mapping cursor. A non-fixed
  `mmap(NULL, ...)` performs next-fit search from the cursor and, after reaching
  the user limit, wraps once to the first post-heap gap. `munmap` holes are
  reusable; a monotonically increasing hint is not allocation ownership and
  cannot by itself cause `ENOMEM`.
- `syscalld` classifies mmap flags and validates the backing descriptor into a
  closed mapping plan before any `MAP_FIXED` unmap. Invalid file/device/shared
  combinations return their Linux errno without deleting a live mapping.
- Kernel `mprotect` validates that every page in the requested span is a live
  4-KiB user mapping before changing the first leaf PTE. A hole or huge-page
  conflict rejects the whole request without leaving a partially protected
  range.
- User unmap preflights every leaf, frame-ownership entry, and the complete
  post-unmap region ledger before removing the first PTE. Metadata allocation
  failure is reported as `OutOfFrames`; it cannot abort mid-commit or leave
  page tables and the address-space region ledger describing different spans.
- An allocator failure, `fatal service endpoint not ready`, watchdog, or
  scheduler-stall marker is a failed KVM health outcome and is retained in the
  bounded causal evidence. No interactive frozen window may remain classified
  as a healthy session merely because QEMU itself is still running.
- Windows syscall policy: `Win32SyscallOffloadRequest`/`Response` + `SYSCALL_OFFLOAD_OP_WIN32_*` range. Kernel dispatcher calls service policy first, validates ABI/status, and must fail closed with a non-success NTSTATUS on malformed or denied responses before performing only the narrow privileged action.
- Windows CPU affinity uses the documented current-process and current-thread
  pseudo handles in the first fixed single-group envelope.
  `GetProcessAffinityMask` returns the scheduler-owned process mask and exact
  Online system mask; `SetProcessAffinityMask` atomically updates every live
  thread in the current process; `SetThreadAffinityMask` returns the previous
  nonzero mask and rejects escape from the process mask; and
  `GetCurrentProcessorNumber` returns the dense logical CPU that executes the
  syscall return path. The private ntdll transport normalizes NTSTATUS failures
  before kernelbase applies Win32 BOOL/zero/previous-mask semantics. Other
  process or thread handles return `ERROR_INVALID_HANDLE`; they are not
  silently treated as current.

## Commercial-Max Protocol

`rustos-user-abi::syscall::CommercialMaxProtocol*` reserves versioned protocol/op
ids for core services. Retired driverd values are permanent tombstones; they
do not admit a service, a module-loading path, or a future ABI reuse.

**Shared ABI scaffolding only** — ring0 exposes new privileged actions only when a narrow broker is implemented and capability-gated.

## Display Surface

- `device::DisplayInfo.flags`: `DISPLAY_INFO_FLAG_PRIMARY_PROVIDER` distinguishes a real primary provider from GRUB/firmware framebuffers (default = early console + panic output only).
- Firmware framebuffer data is diagnostic-only and is never a presentation
  fallback. The only accepted normal provider is the validated DVM aperture.
- Built-in framebuffer registration accepts only the DVM primary-provider flag;
  do not infer ownership from geometry or `display_info()` presence.
- Display readiness is a two-stage observation. The bootstrap `display_get_info`
  snapshot must carry both primary-provider and DVM-scanout provenance; it may
  precede GPU-atlas creation. The later kernel-stamped
  `product-display-ready` milestone proves successful current-epoch GPU
  promotion. Acceptance must require both observations and must not require the
  later GPU-compositor bit to have existed in the earlier immutable snapshot.
- Surface present = kernel fast path: copies validated shared-surface contents into active framebuffer + queues provider flush for bounded housekeeping. **Do not reintroduce synchronous virtio-gpu command waits into app syscall context** for normal uiserver presents.
- Display ioctl is an explicit two-phase transaction: provider discovery,
  aperture mapping, and any wait-capable setup run before acquiring process
  descriptor state; the commit phase reacquires and revalidates the exact fd,
  handle generation, request, and cache-only provider snapshot. No implicit
  transport discovery or mapping is permitted below that process-state lock.
- The KVM virtio-gpu path is Linux DVM DRM/KMS over the fixed display aperture.
  RustOS has no in-kernel virtio-gpu `.ko` path and must not regain a second
  display provider.
- If the DVM provider is unavailable, normal presentation fails closed. It
  must not synthesize a boot-framebuffer or direct-GPU fallback. During boot,
  `uiserver` treats only `EAGAIN`/`ENODEV` from display-info lookup as transient
  provider-publication ordering and retries within the 1 s product-boot
  deadline; every other error is terminal and deadline expiry remains fatal.
- A DVM-backed physical scanout is usable for normal desktop rendering but is
  not a trusted-attention display. The trusted-UI status endpoint fails closed
  until an independently attested scanout and input path are deployed; drawing
  an overlay in `uiserver` does not change that boundary.
- `uiserver` partial dirty rects should stay split unless merged union is nearly as small as separate areas. Over-coalescing disjoint topbar/taskbar/window updates → large framebuffer copies + delayed input feedback.

## Scheduler

- Linux CFS-like: fixed tick, nanosecond vruntime, weighted share, bounded sleeper credit. Weights affect vruntime only — **never reprogram hardware timer**.
- Timer IRQ hitting a user-task kernel frame: set deferred reschedule; **do not preempt arbitrary kernel frames**.
- Task weights = microsecond vruntime budgets (default 100 µs). `uiserver` gets a longer render/present slice, and latency-critical brokers it calls in-frame, especially `inputd`, must stay in the same order of weight so UI loops do not block behind input IPC. Runtime catalog metadata is not a realtime capability: `runtimed` pins only the exact UI executable to System weight and clamps every other launch below System admission before calling `loaderd`.
- The max-burst guard rotates to another ready peer within the current
  scheduling class even when the current task's weighted vruntime still wins.
  Separately, after two consecutive System dispatches, one ready User task must
  run before System selection resumes. Each ready User task also has a 2 ms
  age bound, so a single busy User task cannot consume every reserved turn.
  Authenticated User-only event handoffs use a deduplicated 16-entry FIFO and
  are capped at eight consecutive picks. These are the explicit recovery and
  application CPU reservations under DVM/UI load.
- Normal and voluntary-yield task selection each scan the fixed 128-slot table
  once and retain one minimum-vruntime candidate per class. Do not restore a
  separate full-table pass for each empty higher class: effective class
  resolution also walks live reply donations, so repeated scans multiply the
  timer-tick cost under User-only and recovery workloads.
- Strict admission for bootstrap syscall/VFS/loader/process/pager brokers comes
  only from `rootd`'s fixed manifest. Dynamic package metadata cannot set
  `TASK_WEIGHT_INTERACTIVE_FLAG`; admitted ready brokers are covered by the
  bounded System-class wait rail. Loaderd and vfsd surrender that base
  admission after the exact authenticated uiserver image transaction, and
  runtimed does so after uiserver bootstrap; later strict latency is
  reply-scoped priority donation, not a permanent bulk-I/O privilege. In the
  non-blocked selection lane, an overdue
  System continuation wins one turn before a generic IPC hint without
  discarding that hint; otherwise reply floods can defeat the stated wait
  bound even though each individual handoff is valid.
- Privileged loader requests use service roles, never caller-supplied PIDs.
  `SPAWN_EXEC` requires the current rootd, initd, or sessiond endpoint owner;
  `EXEC_TARGET` requires procd. Initd's endpoint is identity-only and has no
  receive surface. Loaderd checks on ingress and ring0 repeats the check at the
  final commit, after image loading, so role restart/revoke invalidates the
  request before flags, weight, console session, or target image can commit.
- Post-init readiness is not a PID announcement. Rootd requires the exact
  declared executable path and asks a rootd-only process broker to prove the
  still-unconsumed deferred-spawn `(target_pid, reporter_pid)` binding before
  publishing the lease. Capability registration then independently requires
  the reported PID to own its endpoint; reporter exit revokes descendants.
- A tracked spin lock must not be held across
  disk/filesystem/IPC/framebuffer-copy loops. Use `KernelWaitLock` or split the
  section; add `cond_resched` in long loops. Do not introduce subsystem-local
  raw-spin diagnostics in place of the shared lock-class graph.
- `KernelWaitLock` contention uses the scheduler's exact
  arm-publish-recheck-commit protocol. Publishing a waiter before an ordinary
  `block_current_task` is invalid on SMP: an unlock on another CPU can consume
  the only wake before the waiter becomes blocked. A racing or unrelated wake
  cancels the arm and removes the queue entry idempotently. The legacy direct
  `block_current_task` / `block_current_user_task` entry points are removed;
  bootstrap TTY and every other wait must use the same exact arm/commit
  lifecycle.
- The current SMP correctness substrate includes AP bring-up, a CPU-online
  state machine, CPU-local architectural/lockdep/current-task state,
  reschedule IPI, TLB shootdown, cross-CPU lifetime barriers, and translated
  atomic robust-futex cleanup. Independent per-CPU run queues, targeted
  reschedule IPIs, and bounded balancing now own dispatch; the global
  `Scheduler` catalog, guard, and consumers are deleted. Source deletion is
  not hardware or performance qualification. `RUSTOS_SMP_READINESS` remains a
  source-bound admission gate, not a release claim: multi-vCPU commercial
  acceptance remains closed until the full contract and 1/2/4/8-vCPU
  qualification matrix in `smp-contract.md` pass.
- Process-broker registries are fixed-capacity and allocation-free while
  locked. A prepare reserves its mapping-pointer capacity before publication;
  file/data/zero mappings are constructed before acquisition. Removing a
  prepare transfers it out of the registry before destroying pinned backing or
  heap state. Deferred VFS/netd queues advertise only their public capacity and
  retain one inaccessible slot for a concurrently popped item's exact retry,
  preventing both capacity off-by-one failure and retry loss.
- MMIO mapping publication, direct-map cache-mode override, reference update,
  and final unmap are serialized by one scheduler-aware transaction lock.
  Allocation and page-table range updates never run under the former raw
  spinlock plus whole-transaction IRQ exclusion; direct-map override entries
  remain physical-address ordered and reference increments cannot wrap.
- Kernel IPC endpoint, message, reply, and shared-region registries are
  independent fixed-capacity generational slabs. Handles encode a nonzero
  generation and slot index; removal advances the generation and permanently
  retires a slot rather than permitting ABA reuse after generation exhaustion.
  Endpoint queues reserve their complete admitted capacity before publication,
  and request/reply byte and handle storage is prepared with fallible reserve
  before an object slot lock is taken. Failed publication keeps ownership of
  that storage outside the slot guards so destruction cannot enter allocator
  metadata from the critical section. Ring0 retirement cancels each task-owned
  call separately and transfers its already-owned descriptor vector directly
  to the deferred-drop owner; it never builds an aggregate vector under IRQ
  exclusion. Slab slots are cache-line aligned so independent reply or region
  locks do not share their lock word.
- A display surface owns one shared-region descriptor reference. `dup`,
  descriptor transfer, and `fork` acquire distinct references; close, exec, and
  process teardown release them. Every successful VMA mapping owns a separate
  mapping hold, so closing the last descriptor cannot invalidate mapped pages.
  Partial unmap splits that hold with the surviving VMA pieces, and the last
  descriptor-or-mapping release removes the generational object and transfers
  its backing to a fixed-capacity reclaim queue. Housekeeping returns at most
  64 pages per turn; admission counts live plus queued objects, so delayed
  reclamation cannot overrun the queue or permit unbounded region creation.
  Failed slab publication and failed descriptor installation enter the same
  owned reclaim path.
- IPC slot locks have explicit runtime classes ordered
  `endpoint -> message -> reply`; region locks are independent leaves.
  `nucleus-core` records observed per-CPU class dependencies without
  allocation, rejects recursive acquisition and inverse dependency cycles,
  requires strict LIFO release, and bounds both IRQ-off and IRQ-on spinning.
  IDT interrupt entry additionally marks IRQ context before any tracked lock:
  a class first observed from IRQ context becomes IRQ-safe, a class first
  acquired in ordinary interrupt-enabled context becomes IRQ-unsafe, and the
  same class or a dependency path may not cross from IRQ-safe to IRQ-unsafe.
  `KernelWaitLock` classes share the same dependency graph through a
  fixed-capacity task-identity stack that survives a scheduler handoff.
  Inverse sleepable ordering is rejected. Blocking sleepable acquisition under
  raw state remains forbidden; a bounded raw leaf under sleepable ownership is
  learned as an ordinary dependency and rejected if it closes a cycle.
  Successful fail-fast external try-locks are tracked as raw leaves and cannot
  become a hidden blocking path.
  Lock-class stack mutation plus physical unlock is interrupt-atomic and an
  APIC ID outside the tracked CPU capacity is a direct diagnostic, never an
  aliased CPU slot.
- Boot service order separates UI-core liveness from mutable-volume readiness.
  `runtimed` waits on the foundation, `netd`, `devmgrd`, and `inputd`; it does
  not wait for `storaged`, because its signed early-system closure contains the
  first compositor and runtime registries. Applications, assets, and other
  mutable DVM-volume paths remain storage-gated by their owning policy flow.
