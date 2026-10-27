# IPC transfer, wait and performance contracts

[Contract index](../contracts-abi.md).

## Handle Transfer

- Bounded cap-transfer via `kernel_ipc_runtime::api::KernelTransferredHandle` + `*_with_handles` endpoint APIs.
- Byte-only recv/take wrappers must fail with `BufferTooSmall` when a queued message contains transferred handles. Older paths must never silently drop capabilities.
- Transferred handles require nonzero transfer ticket + `HandleRights::allows_transfer()` true.
- The only Ring3 handle-transfer authority wire is exactly 24 little-endian
  bytes: nonzero `transfer_id: u64`, nonzero random `nonce: u64`, and nonzero
  kernel-minted `batch_generation: u64`.
  Ring3 bytes are never copied into `KernelTransferredHandle`, a Rust enum,
  padding, discriminants, references, or pointers. Decode produces the typed
  ticket only after exact-length and nonzero checks; malformed bytes return a
  bounded ABI error without constructing or consuming authority.
- The registry matches ID, nonce, and batch generation and consumes the whole
  batch exactly once. Before netd sees it, the kernel binds the batch to the
  source process generation, intended receiver generation, netd service epoch,
  socket channel generation/direction, and exact byte-stream interval. Netd
  IPC ABI v7 carries only these opaque tickets; it cannot redirect authority by
  editing a PID, epoch, channel, or stream position.
  A forged, stale, zero, or replayed ticket cannot remove the live entry. The
  Kani parser proofs exhaust the 128-bit input partition and canonical
  round-trip; `ipc-handle-transfer` separately checks typed registry lifetime.
- Pending transferred-handle entries are owned by the process-handle substrate,
  not an isolated compat cache. An endpoint message that is cancelled,
  peer-closed before receive, rejected after receive-output validation, or
  abandoned by caller task exit must return every opaque descriptor for exactly
  one substrate drop. Successful receive installs the whole validated batch;
  duplicate/stale descriptors and partial installation must fail closed.
  AF_UNIX queues retain byte and ancillary control as one ordered stream
  segment. `recvmsg` may collect rights only from a segment whose bytes it
  consumes; ordinary `read` consumes and returns that segment's rights to the
  kernel release trailer instead of reattaching them to later bytes. Close,
  queue discard, peer exit, and service-epoch replacement release every queued
  batch. Destination FD numbers remain invisible reservations until data,
  control, and message-header copyout all succeed, then publish atomically.
  Local `recvmsg` snapshots options/credentials, sizes the complete reply,
  dequeues and encodes under one netd state transaction; bounds rejection must
  precede byte/control movement. Ancillary admission charges aligned segment
  lengths against a 16 KiB socket budget and a shared 16 KiB budget for each
  listener's unaccepted sockets, including in-flight accepts. Exceeding either
  returns `EAGAIN` without consuming bytes or sender authority. Successful
  accept publication ends listener-budget custody without dropping control.
  Final listener close retires queued unpublished sockets and marks clients
  peer-closed. An in-flight failed accept requeues if the listener is live;
  otherwise its cleanup-only error reply returns its unpublished peer's queued
  authority once. Compat validates the full wire envelope and consumes release
  trailers before returning errno. ACCEPT permits a trailer only on error and
  only when `reserved0 == payload_len`; successful ACCEPT permits none.
- Supervisor services draining independent brokers must use `SYS_RUSTOS_IPC_TRY_RECV`, not blocking `SYS_RUSTOS_IPC_RECV`. Their *idle* wait is `SYS_RUSTOS_IPC_RECV_WITH_SENDER_BOUNDED`, not a timer; see the IPC Wait Discipline section.
- FD-table transfer goes through `kernel_ps::api::TransferredHandleEntry` + `HandleTable::{duplicate_for_transfer, install_transferred}`. Source class + rights must permit descriptor transfer; directory FDs are file capabilities and transferable for VFS migration.
- Userspace handle-aware IPC: `SYS_RUSTOS_IPC_{CALL,RECV,REPLY}_WITH_HANDLES` with `Ipc*WithHandlesArgs`. Send handles = Linux fd arrays; received handles install into receiver fd table and return as `i32` fd arrays + `u16` count. `recv_fd_count_ptr` mandatory even when no handles returned. Counts bounded by `IPC_MAX_TRANSFER_HANDLES`.

## Register short-message ABI

Opt-in `SYS_RUSTOS_IPC_REGISTER_{CALL,RECV,REPLY_RECV,REPLY}` (`0x5255_0052..55`)
in default mode (`R9=0`) passes at most 16 bytes in GPRs without user pointers,
argument blocks, or handle transfer. The exact input/output register map, shape validator and
compiler-clobber-aware wrapper live in
`libs/rustos-user-abi/src/syscall/ipc_registers.rs` (re-exported through `syscall`).
CALL requests are 1..=16 bytes; replies may be empty. RECV has a fixed 16-byte
capacity. Oversized/handle-bearing traffic keeps the existing buffer/grant
semantics through the buffer slow mode below.

### Common service fastcall entry and buffer slow mode

`rustos-svc-runtime::syscall::{syscall0..syscall6}` routes IPC data operations
through these same four fastcall entries. Services with local syscall wrappers
use its `try_fast_ipc` bridge; libc-based data calls in initd/runtimed use the
common native wrappers directly. Ordinary Linux syscalls, endpoint registration,
lookup/wait and other control/broker operations retain their original entry.
The bridge returns raw `-errno`, never libc's `-1` plus thread-local errno.

`R9=IPC_REGISTER_BUFFER_MODE` (1) selects the buffer slow mode: RDI points to
`IpcRegisterBufferArgs` (56 bytes: operation followed by six u64 argument words),
and RSI/RDX/R10/R8 must be zero. Only CALL, bounded CALL, handle CALL variants,
RECV/TRY_RECV (including sender/bounded/handle variants), REPLY/handle REPLY and
fused REPLY_RECV are allowlisted, in their exact matching entry family.
Linux syscalls, control operations and recursive fastcalls are forbidden.
`kernel/compat/src/user/syscall/linux/ipc_ops/registers/buffer.rs` fault-revalidates
and snapshots the block before dispatch, validates the family, then executes the
original handler **once**. The block and closed allowlist are defined in
`libs/rustos-user-abi/src/syscall/ipc_fast_buffer.rs`. User-copy validation,
capability ownership, handle tickets, deadline budgets and pre/post-commit
error tags remain unchanged. Buffer mode returns only RAX; all register
payload/metadata outputs are zero on success and failure. It uses the same
opt-in FP/control-state contract and finalizers as the short mode.

Buffer-mode selection does not imply queued transport: bounded byte-only CALLs
may attempt the same inline rendezvous as ordinary CALLs. The absolute deadline
is fixed before transport selection and capped across rollback/fallback; expiry
before or during response observation wins over returning a response. Register
CALLs share one inline reply-wait protocol for queued and rendezvous replies,
including direct syscall-frame parking on initial and spurious waits. Plain
register RECV also parks directly; its errors remain ordinary errno, while
REPLY_RECV retains the reply-committed tag. Both queued and rendezvous byte-only
REPLY_RECV completions carry exact reply/context custody into the same deferred
settlement-and-receive path. Handle-bearing operations retain their owning
transfer cleanup; remote peers retain remote wake/IPI scheduling.

The typed `ipc::{call,recv_with_sender,reply,reply_recv_with_sender}` helpers
select pure registers when the exact shape fits, through
`libs/rustos-svc-runtime/src/ipc/fastcall.rs`. Receive capacity must be
**exactly 16**, not merely at most/at least 16: smaller buffers must not consume
an oversized request; larger buffers promise to accept traffic beyond the short
image. Other shapes, finite deadlines and handles select the buffer ABI mode
before execution; an eligible byte-only call may still use kernel rendezvous. A syscall error or committed reply is never retried in another mode.
The unsafe typed helpers require their own input/output ranges to remain valid;
raw syscall helpers retain kernel fault validation of arbitrary user pointers.
Large protocols are not silently truncated or migrated to a new wire format.

The register ABI adds no new authority or objectless rendezvous cell. Process
endpoint authorization, exact reply generation/rights, deadlines/cancellation,
funding custody, fairness and arm/register/recheck/commit remain unchanged.
A fast reservation decline rolls back before the existing queued fallback;
remote/unparked peers still work. REPLY_RECV authenticates the next endpoint
before consuming the old capability, and uses the existing post-commit errno
tag. Receive metadata is kernel-authenticated; error outputs and unused payload
bytes are zero. Both buffer/register interoperability directions are supported.
The buffer ABI retains its fault-recoverable user-access validation unchanged.
Pointer-free short handlers obtain `(task, exact live process identity)` from
`kernel_ps::api::current_user_ipc_identity`: IRQ-stable installed Current binding
plus the generation-bound live process publication, with the existing lifecycle
fallback for a missing publication. They acquire no retained MM/process
allocation loan and do not bind or walk user pages. This snapshot permits IPC
identity/owner checks only, never user-memory access. Exec/exit/stale-slot
refusals remain mandatory; diagnostic PID/TID publications are not authority.

A rendezvous frame carries up to one maximum IPC message (64 KiB) each way: the
inline 256-byte images plus an owned spill for the rest. A caller's declared reply
capacity above that is clamped. An unfused `REPLY` holds its caller's wake until
the same task's next blocking receive (or its next syscall or scheduler entry).

The transport still owns its existing fixed frame, but short request/response
return images are specialized to 16 bytes, rather than copying 256-byte arrays
through every Rust layer. Exact object-slot transactions, caller/receiver checks,
receive capacity refusal before consume, terminal response overflow retirement,
and queued/large-message fallback are shared with the legacy 256-byte API.
The scheduler's resident lane and direct syscall-frame resume now switch eligible
peers without the generic dispatch turn; see [IPC lane](../performance/ipc-lane.md).
The register image specialization does not itself remove reply objects or the
reservation/funding/custody layer. Objectless virtual rendezvous and paired-custody
switching remain [unimplemented design](../performance/ipc-rendezvous.md).

Only these four opt-in syscalls declare all enabled FP/vector **data**
(x87/MMX/XMM/YMM) caller-clobbered; x87 status/stack state is reset. The entering
x87 control word and MXCSR remain preserved. Entry retains those controls and
an immutable mode marker beyond the normalized scheduler frame, omitting the
ordinary XMM stack image only for these four Linux operations. A GS-private ABI
selector is installed with the exact retained architecture's kernel stack on
initial/current/generic/lane returns. Rust checks it against the entering ABI
and operation before dispatch; the unused legacy GS task pointer is not a
selector. Other ABIs/operations still save their entering XMM before Rust.
The exact-task
SIMD image owns the active marker across blocking/migration; image replacement
and terminal release clear it. Only admitted IPC lane save/restore may omit the
full image while that marker is active. The interrupt prefix still preserves
all kernel-continuation XMM. Ordinary/asynchronous scheduler transitions still
save/restore full state, including preemption inside a wide-SIMD bracket; no
wide bracket may span an admitted IPC lane yield.

Both SYSRET and deferred saved-user IRET finalize before exposing a user image:
mask IRQs, clear enabled vector data and all eight physical x87/MMX registers,
restore entering controls, retire the exact-task marker. Deferred IRET also
zeros its explicit XMM image before the generic scheduler capture. FNINIT alone
does not clear physical data. The linked-image audit permits only these named
custody instructions, not general x87/FP arithmetic. Ordinary syscall data
preservation is unchanged. Resident lane switches and direct parked-register
resumes leave through the same finalized syscall return boundary; generic
saved-user IRET remains available where direct return is not admitted.
`syscall-simd-lifecycle` distinguishes lane elision, full asynchronous custody,
marker retirement/rebinding and both finalized return routes.

References: [AMD64 psABI](https://gitlab.com/x86-psABIs/x86-64-ABI/-/blob/master/low-level-sys-info.tex),
[Rust clobber_abi](https://doc.rust-lang.org/reference/inline-assembly.html#abi-clobbers),
and [Intel FNINIT semantics](https://www.felixcloutier.com/x86/finit:fninit).

The isolated `ipc_rt_intra_process_register_reply_recv` probe keeps the buffer
control's 16-byte echo, ordered TSC stamps, 2,000 warmups, all 20,000 samples,
STOP/join checks and CPUID anchor. Its untimed preflight checks 1..=16-byte
padding, malformed input, queued fallback, rejected fused endpoint without reply
consumption, duplicate reply refusal and mixed-ABI peers. Untimed assembly
witnesses seed x87/vector data, capture immediately after SYSCALL and check
sanitization plus distinct preserved controls on caller/server, including
blocking/error paths; an ordinary syscall checks the preservation control.
`apps/ipcbench/src/ipc_service.rs` also preflights the buffer slow mode: bad
pointer/non-IPC rejection, 16/32-byte exact echoes with untouched destination
tails, authenticated PID/TID, finite deadlines, fused receive and large reply,
and physical FP clearing/control preservation through all four entry families.
These preflights are untimed, not additional measured probes.
It is a same-address-
space measurement, not evidence of a cross-process/CR3 cost reduction. Existing
IPC models cover the unchanged transport/custody transitions; this wrapper adds
no cell/epoch state to refine.

## IPC Wait Discipline

- A user-created endpoint and every reply capability queued through it are
  owned by the creating **process**, not a single creating task. Any worker in
  that process may receive/reply; a different process must receive `EPERM`
  without consuming the request or reply. Process exit closes all of its
  process-owned endpoints and wakes pending callers; individual task exit only
  removes that task's waiter/caller state. Do not weaken this to globally
  guessable endpoint or reply IDs, and do not regress service worker threads
  such as `uiserver-display-policy` to creator-task-only ownership.
- Endpoint creation reserves its fixed owner ledger before queue allocation or
  slab publication. A process owns at most 32 endpoints, a task-owned substrate
  endpoint owner at most 8, and 64 of the 512 global endpoint slots remain
  unavailable to process/task owners. Allocation or publication failure returns
  the exact reservation; process exit returns it only after the owned endpoint
  has been removed and its waiters have been settled.
- Shared-region admission is charged to the creating process through display
  and DRM surface creation. One object is at most 256 MiB, global admitted bytes
  are at most 512 MiB, and one process owns at most 64 objects and 128 MiB.
  Handle close transfers the object to deferred backing reclaim without
  returning object or byte quota. Only completed physical reclaim returns that
  charge. The process table separately caps one process at 32 attached tasks,
  so a single subject cannot consume all 128 scheduler slots.
- Public `SYS_RUSTOS_IPC_CALL` and handle-transfer call syscalls keep blocking
  Send/Receive/Reply semantics. Do not add a public timeout ABI for generic
  Linux ELF or Windows PE callers.
- Kernel compat calls into policy services may use a bounded internal deadline.
  On timeout, compat cancels the reply cap so queued and already-received
  endpoint calls cannot be completed by a late reply. Timeout cancellation
  records an `ipc-reply-timeout` milestone with the reply cap, caller task, and
  cancellation status. This high-frequency milestone stays in the bounded
  in-kernel milestone ring and appears in an explicit postmortem dump; it must
  not emit a synchronous debugcon line for every readiness timeout.
- Endpoint process-owner teardown wakes both pending callers and tasks blocked
  in receive on that endpoint. Task exit must also prune stale receiver
  waiters owned by the exiting task from every endpoint.
- A single-endpoint service may block in `SYS_RUSTOS_IPC_RECV`. Supervisors
  with multiple independent event sources must still *drain* with
  `SYS_RUSTOS_IPC_TRY_RECV` / `rustos_svc_runtime::ipc::try_recv`, because a
  drain pass has other sources to visit and may not block on any one of them.
- A supervisor that has finished a drain pass with no work must idle in
  `SYS_RUSTOS_IPC_RECV_WITH_SENDER_BOUNDED` on its endpoint, not in a timer.
  The deadline is the soonest of its own pending deadlines, so the sources with
  no wake object keep exactly the visiting interval they had, while a request
  that does arrive is serviced on arrival. A flat sleep between drain passes is
  no longer acceptable: it places the whole idle interval in front of every
  synchronous caller, which is how a 10 ms supervisor sleep became the dominant
  term in keystroke latency. `EAGAIN` from the bounded receive means the budget
  expired with nothing queued and is the same "no work" answer `try_recv` gives.
- A service that answers a request the caller is blocked on may hold the reply
  capability instead of answering "not yet", provided the wait budget it honours
  is statically below the caller's own IPC class deadline, the number of held
  capabilities is bounded, and every held capability's deadline is folded into
  the service's idle wait. Netd's deferred local polls and runtimed's parked
  console reads are the two kernel-IPC instances; both consume request state
  into the reply before it is sent, so a budget above the caller's class
  deadline would hand consumed bytes to a capability compat had already
  cancelled.
- The same discipline applies where the reply capability is a held AF_UNIX
  connection rather than a kernel reply cap, with one difference: there is no
  compat-side cancellation, so the bound that matters is the *caller's* socket
  deadline. Runtimed's `OP_WATCH_RUNNING_PROGRAMS` is the instance. A held
  socket and a hung server are indistinguishable to the caller, so a parked
  watch must re-arm on a budget the client's I/O deadline comfortably exceeds
  (`RUNTIME_WATCH_MAX_WAIT_MS`, asserted at no more than half of `RPC_IO_TIMEOUT`),
  and the number of parked connections is capped by `MAX_RUNTIME_WATCHERS`.
  Past that cap the server answers immediately, which degrades a watcher to the
  poller it replaced instead of pinning descriptors.
- devmgrd's ioctl **routing** answer is memoized in compat; its **authorization**
  answer never is. `ioctl_route(request_number)` is a pure total function of the
  request number - no fd, pid, credentials, or session reach the decision - so
  asking per call spends a broker round trip deriving a constant.
  `DEVMGRD_IPC_OP_IOCTL_AUTHORIZE` is the opposite: it reads the caller, and the
  forwarded path pays it on every call. The memo is keyed by devmgrd's service
  registration epoch, because the routing table is compiled into that service
  and a restart may be a different binary. Measured at 8 vCPU, the uncached
  query was half of roughly ten broker round trips per keystroke, and those
  round trips were 98% of a 36 ms median key-to-pixel latency - time invisible
  to every phase uiserver times, because it is spent waiting for the echo rather
  than doing work.
- A console has two observers with opposite interests, and each needs its own
  readiness subject. A shell waits to *read its own session*; a compositor waits
  for *anything it draws* to change. Only the first existed, so the second ran a
  fixed-interval snapshot loop - the last timer between a shell writing and a
  pixel changing. `COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_GRAPH_READINESS` is the
  second subject, published on the wait set under
  `SESSIOND_CONSOLE_GRAPH_OBJECT_ID` (zero, which no live session handle can
  be). This is the object-signal shape commercial microkernels use for exactly
  this problem: Genode's `Terminal::Session::read_avail_sigh`, a Zircon readable
  signal observed through a port, a QNX `ionotify` armed pulse.
- The graph token must advance in `advance_graph_generation` and nowhere else,
  and every compositor-visible mutation must go through it - session create and
  remove as well as output and echoed keys. A publication forgotten at a call
  site is silent, and it strands a waiter that has deliberately stopped polling.
- `CONSOLE_IOCTL_WAIT_GRAPH` takes the direct console-broker rail rather than
  the devmgrd forward every other console ioctl uses. devmgrd serves its clients
  from one loop, so a reply held there would stall every unrelated device ioctl
  for the whole park; console read, write, and per-session readiness already
  route direct for the same reason. Compat authorizes it instead, on the console
  handle itself, since a manager view is not session-bound.
- `SESSIOND_CONSOLE_GRAPH_WAIT_MAX_MS` is a re-arm interval, not a latency. The
  edge answers every parked wait in the pass that produced it; the bound only
  decides how often an idle compositor asks again, and it must stay statically
  below `IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS`, the class deadline that carries
  the call.
- A change token must not change when it is observed. Reporting one is a read:
  a handler that advances the value it reports tells every caller that
  everything changed, every time, and a caller that reacts to change then
  reacts continuously. `CONSOLE_IOCTL_SNAPSHOT_SESSIONS` violated this by
  raising its reported `output_generation` to a counter the handler itself
  incremented, which put uiserver's console-refresh worker and the render loop
  into a permanent spin against the endpoint carrying shell keystrokes. Bump a
  token where the state changes - `write_to_session`, an accepted key event -
  and nowhere else.
- A change edge must be derived from the bytes the reply would carry, not from a
  counter bumped at each mutation site. A counter has to be incremented
  everywhere the watched state is touched, and forgetting one site fails
  silently: the watcher parks straight through the change it asked about.
  `running_programs_digest` hashes the reply itself, so a mutation that does not
  alter the reply is correctly not an edge, and one that does cannot be missed.
  The digest never travels - server and client compute it over identical bytes.
- Rootd and runtimed use the shared `IPC_CONTROL_DRAIN_BUDGET` to process at
  most 32 already-queued control requests before returning to lifecycle,
  launch, catalog, and socket work. The kernel's 64-call endpoint admission
  ceiling is compile-time required to retain two such bursts. Do not sleep or
  run unrelated policy after every single ready request, and do not replace
  this bound with an unbounded drain; both changes can violate a synchronous
  caller's 100 ms interactive-control rail under SMP contention.
- Blocking receive has one endpoint-slot linearization point for the
  poll/arm/register race. If a message is already pending when receiver
  registration acquires that slot, the syscall re-polls without publishing a
  waiter. A waiter may exist only while the queue is empty; retaining one on
  the pending fast path creates stale wake and synchronous-handoff authority
  for a task that never blocked. The executable refinement is
  `formal/endpoint-receiver-wakeup/EndpointReceiverWakeup.tla`.
  A timer/ordinary wake may withdraw the exact typed receive phase before
  resumption removes its runtime waiter. Popping that stale waiter must not
  cancel an otherwise valid original queued reply: cold publication retains
  only the SAME donor's live `AwaitingReceiver` custody for an already runnable,
  unarmed/unblocked receiver. It grants no generic wake, early worker funding,
  FIFO or execution right; only actual later receive binds the worker. A foreign
  typed arm or absent/wrong reply reservation still refuses the handoff.
  `ReceiverMismatch` returns before any scheduler effect, so while the exact
  receiver still holds its receive phase on that endpoint the cold call retries
  the same handoff a bounded number of times (`RECEIVER_REFUSAL_RETRIES`)
  before failing with ENOSPC.
  A refused cold call wake must not strand the receiver it popped: after the
  caller cancels its request, a receiver that still holds its typed receive
  phase for that endpoint is republished at the waiter front with capacity 0
  (as the fast-path rollback does), so the next call can wake it. Without this
  an armed receiver commits its block outside the waiter list and every later
  call to the service times out (8-vCPU boot stall, 2026-10-20).
  A cold producer's own funding bind is not evidence of request delivery:
  frame/queue admission can still refuse after that bind. The late
  already-dispatched exception is allowed only when this producer did not bind
  funding. Its own failed wake still returns failure. The adapter may retain
  the SAME reply only if it independently completed, or earlier admission
  succeeded and runtime confirms actual nonempty-request consumption plus the
  exact live message/reply/immediate-caller custody under Message -> Reply
  leaves. A queued, uncustodied, mismatched, consumed or recycled edge cannot
  certify receipt. This observation grants no wake or replacement budget;
  otherwise the sender cancels the exact request/custody rather than sleeping
  on an unserved request.
- A single-endpoint byte-only policy service may complete its current reply and
  enter the next sender-authenticated receive through
  `SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER`. The syscall accepts only the exact
  versioned `IpcReplyRecvWithSenderArgs` shape. Endpoint ownership, response
  input, request capacity, and every next-request output range are validated
  before the old one-shot reply is consumed. A normal negative errno is
  therefore pre-commit. A receive error after reply completion is encoded
  below Linux's `-4095` errno range with
  `IPC_REPLY_RECV_COMMITTED_ERROR_BASE`. The admitted raw partitions are
  non-negative success, `-1..=-4095` pre-commit errno, and
  `-4097..=-8191` post-commit errno; `-4096`, `<= -8192`, and `i64::MIN` are
  fail-closed ABI violations rather than retry states. Ring3 may retry the old
  cap only after a decoded pre-commit errno and must never retry it after a
  tagged post-commit result. A non-negative result both proves reply completion and
  reports the next request length. The receive half reuses the endpoint
  check-arm-recheck waiter protocol and retains the exact awakened caller's
  bounded synchronous scheduler custody. Handle-bearing reply/receive remains
  on the explicit separate syscalls; the fused byte path may not drop, infer,
  or partially install handles. The executable transaction is
  `formal/ipc-reply-recv-transaction/IpcReplyRecvTransaction.tla`.
- root-supervisor services that authorize subjects may use
  `SYS_RUSTOS_IPC_{TRY_RECV,RECV}_WITH_SENDER` to receive the caller PID/TID
  stamped by the kernel. Payload subject fields are not trusted unless they
  match this sender identity. Use the blocking variant only when the supervisor
  has no independent event source that must be polled before the next IPC.
- Sender binding is mandatory for every published policy-service ingress, not
  only rootd. Direct requests require a nonzero exact PID/TID match.
  Service-to-service delegation is explicit: the receiver rechecks
  `SYS_RUSTOS_IPC_VALIDATE_SERVICE_OWNER` for the kernel-stamped sender PID on
  every delegated request and never caches the result across exit, revoke, or
  endpoint republish. Sender identity, lookup admission, object rights, object
  generation, and exact response binding are cumulative checks. The complete
  ingress registry is `formal/trust-boundaries.tsv`.
- IPC stability changes are substrate only. Service restart, admission, routing,
  and policy decisions stay in `rootd`/`syscalld`/`vfsd`/`loaderd`/other owning
  services, not in new ring0 policy tables.

## IPC Performance Acceptance

- Kernel entry to the first completed private-compositor frame has a 3-second
  optimization target and a 5-second hard acceptance limit. The KVM harness
  starts this clock only after both guests exist and fails independently of the
  broader 30-second readiness budget when
  `uiserver: gpu-compositor active contract=3` is late. The required first
  retained-scene gradient is published by a bounded `uiserver` bootstrap
  worker before that worker self-demotes for the optional full background;
  the GPU marker may not be synthesized from a CPU-only frame. Storage-only
  validation is excluded because it intentionally launches no UI.
- A service dependency is checked once per publication epoch. Supervisors use
  deferred-start plus exact-PID endpoint wait when they own the child. A child
  activated behind an already-proven dependency performs at most one admitted
  lookup and lets its bounded supervisor retry own transient failure; it may
  not wrap a synchronous lookup in a yield/sleep retry loop.
- A frame/present turn permits zero synchronous policy-service IPC. Display
  submit is a direct capability-checked broker operation; provider policy,
  catalog reads, console routing, filesystem access, and logging run outside
  that turn. The CPU-side frame target is 8 ms and the completed frame limit is
  16,667 us. Input arrival to visible cursor progress is capped at 50 ms.
- Readiness check-arm-recheck may perform one query per distinct provider in
  each scan. Duplicate FDs sharing one provider observation are deduplicated,
  and every query uses the shorter of the remaining application deadline and
  16 ms. A timeout cannot restart the whole scan or hide readiness already
  found elsewhere.
- Deferred VFS checkpoint/replay maintenance runs only in the nucleus
  housekeeping task and consumes at most one bounded 100 ms control
  transaction per yielded turn. It cannot drain a backlog in an eight-request
  burst or synchronously chain a second checkpoint ACK; successful replay
  enqueues that idempotent ACK for a later turn. No recovery wait is charged
  to foreground I/O, and the deadline must exceed a normal SMP IPC round trip
  so cancellation cannot amplify retries into revoked-reply queue pressure.
- Deferred netd reference reconciliation follows the same one-attempt,
  housekeeping-owned 100 ms control bound. A foreground netd DUP/final CLOSE
  owns one complete 100 ms interactive-control deadline; only early transport
  failures may retry within its remaining time, never a real timeout. It
  defers the acknowledgement; exact operation replay and acknowledgement are
  separate housekeeping turns. The 16 ms rail is for non-consuming readiness
  queries, not reference mutations. Eight-entry operation-plus-ack bursts or
  retries that restart the caller's deadline are forbidden.
- Performance diagnostics are bounded evidence: early-call samples and
  threshold crossings only. They may not print per request, hold a policy
  lock, allocate an unbounded record, or become a prerequisite for forward
  progress.
- A hot single-endpoint service loop must use the fused reply-receive primitive
  only when it has no independent event source that must run between phases.
  This removes the reply return-to-ring3 plus the following receive trap while
  preserving one exact scheduler handoff. `inputd` is the first admitted user:
  its DVM ingestion remains an independent worker, every dequeued malformed
  request receives terminal `EINVAL`, a pre-commit failure makes one immediate
  standalone attempt with the proven-live reply cap, and a tagged post-commit
  receive failure falls back to a fresh receive without replaying the completed
  reply. Any value outside the three exact result partitions terminates the
  service as an ABI contract violation instead of guessing whether to retry.

