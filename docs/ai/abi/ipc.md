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
- Supervisor services draining independent brokers must use `SYS_RUSTOS_IPC_TRY_RECV`, not blocking `SYS_RUSTOS_IPC_RECV`. Their *idle* wait is `SYS_RUSTOS_IPC_RECV_WITH_SENDER_BOUNDED`, not a timer; see the IPC Wait Discipline section.
- FD-table transfer goes through `kernel_ps::api::TransferredHandleEntry` + `HandleTable::{duplicate_for_transfer, install_transferred}`. Source class + rights must permit descriptor transfer; directory FDs are file capabilities and transferable for VFS migration.
- Userspace handle-aware IPC: `SYS_RUSTOS_IPC_{CALL,RECV,REPLY}_WITH_HANDLES` with `Ipc*WithHandlesArgs`. Send handles = Linux fd arrays; received handles install into receiver fd table and return as `i32` fd arrays + `u16` count. `recv_fd_count_ptr` mandatory even when no handles returned. Counts bounded by `IPC_MAX_TRANSFER_HANDLES`.

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
  housekeeping-owned 100 ms control bound. A foreground dup/close divides one
  16 ms total operation deadline across its retries and always defers the
  acknowledgement; operation replay and acknowledgement are separate
  housekeeping turns.
  Eight-entry operation-plus-ack bursts and three independent 16 ms retries
  are forbidden because their cumulative wait is not bounded by the caller's
  advertised deadline.
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

