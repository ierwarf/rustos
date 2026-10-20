# Input, console and device contracts

[Contract index](../contracts-abi.md).

## Input Surface (`inputd`)

- Linux input reads call `InputdIpcRequest` with `INPUTD_IPC_OP_READ`.
- Compat input-device reads call `INPUTD_IPC_OP_AUTHORIZE_READ` before
  `INPUTD_IPC_OP_READ`; inputd consumes the authorization by `(pid, tid, fd,
  access)` so raw reads are not accepted as standalone event-drain authority.
- inputd owns a single System-class `inputd-dvm-ingress` worker. It waits in
  `SYS_RUSTOS_INPUT_WAIT_BROKER`, which arms before sleeping and rechecks raw
  producer/consumer cursors after registration; the MSI-X leaf only wakes that
  worker. Its dedicated kernel wake slot is separate from the finite generic
  application poll-waiter set, and a dead predecessor is reclaimed before an
  inputd restart may arm it. The worker alone calls gated
  `SYS_RUSTOS_INPUT_INGEST_BROKER`, drains at most
  `INPUTD_INGEST_MAX_EVENTS` generation-stamped raw records per turn, validates
  RDI1 framing/CRC/epoch/sequence in inputd, then yields before a recovery
  batch. Thus a stalled or absent application reader cannot fill the fixed DVM
  ring, while no periodic polling loop is introduced. inputd registers its
  exact task, publishes a monotonic consumer wake generation, and rechecks the
  producer cursor before sleeping. L0 samples that generation only after its
  release-ordered producer commit and rings MSI-X once per new generation.
  This closes both sides of the check-arm-recheck race while batching records
  that arrive while the worker is already runnable. The same armed task has an
  independent 100 ms kernel timer: a lost or indefinitely coalesced MSI-X edge
  therefore causes an authoritative cursor recheck instead of filling the
  ring. L0 also issues a bounded recovery kick after two additional committed
  records remain outstanding under the same wake generation. Normal draining
  still batches by generation; recovery therefore retries a lost edge within
  30 ms at the 66.7 Hz acceptance source without an interrupt per record.
  Neither the timer nor the recovery kick has record/policy authority.
- The RDI decoder is owned exclusively by the ingestion worker, not by the
  shared policy queue. Session start/end may require a bounded netd authority
  transition, but inputd releases the queue lock before service discovery or
  synchronous IPC. Events from a newly granted epoch are not published until
  that transition succeeds. A failed transition resets the decoder and input
  provider state while the worker continues draining the fixed ring, so a
  bounded authority failure cannot become an unbounded producer-credit stall.
- Inputd endpoint-owner exit withdraws the ring's separate policy-consumer
  lease in the same kernel cleanup turn. RustOS clears policy readiness before
  retiring old-owner records and publishes a reset barrier. A replacement
  inputd discards any commit that raced withdrawal, observes the reset, and
  only then rearms policy readiness; neither a dead process nor its records can
  remain the semantic consumer of a live transport.
- Inputd ABI v5 retires caller-driven `INPUTD_IPC_OP_DRAIN_INGEST` and the
  commercial INPUT_INGEST operation. `INPUTD_IPC_OP_STATS` (the kernel's poll
  recheck) and an authorized `INPUTD_IPC_OP_READ` observe only the
  service-owned policy queue and never advance the ring. The ingestion worker
  performs the broker copy and RDI decoding without holding the policy-queue
  mutex, then takes the mutex once per ordinary batch for bounded
  coalescing/publication. This prevents a hot reader
  from starving transport progress. The worker declares an ingestion handoff
  before taking the queue mutex; request threads that race it release rather
  than barge and retry only after the worker owns one bounded critical section.
  The worker uses nonblocking try-lock plus scheduler yield, so it never parks
  behind a missed service-local futex wake while the transport ring fills.
  This closes the service-local priority inversion and makes the
  single-consumer claim true in implementation rather than merely by
  convention. Inputd returns a bounded
  `InputdReadResponse` capped at 32 KiB, and uses fixed-size ingress scratch.
  The non-consuming `STATS` probe has a 16 ms IPC reply deadline, so a wedged
  inputd cannot freeze the UI poll loop behind the generic service timeout; a
  retry sees either unchanged ingress or the already-transferred policy record.
  The endpoint server still blocks in `SYS_RUSTOS_IPC_RECV` while idle.
- RustOS-native input polls and uiserver use readiness-then-read safely:
  the worker publishes a policy readiness generation after the empty-to-ready
  transition, and generic poll/epoll rechecks service policy after every
  generation wake before returning an event. The latency-sensitive uiserver
  reader registers every input open description in one epoll instance, blocks
  without a probe cadence, and enters the stateful authorized `READ` only after
  positive readiness. An endpoint revoke wakes the same wait as ERR/HUP and is
  fail-closed. The reader is demoted before its first wait, so empty input
  cannot create System-class polling traffic or canceled reply capabilities.
  Inputd's monotonic service-owned readiness generation therefore supplies the
  only empty-to-ready wake authority without moving policy or transport-
  consumer ownership out of inputd.
- Inputd is one provider of the common cross-service wait-set ABI. vfsd owns a
  bounded interest registry keyed by the target fd plus stable provider object
  identity, matching Linux's fd/open-description pair; the observed service
  epoch is mutable registration state, so an explicit MOD can rebind an
  existing interest after provider restart without creating an undeletable
  duplicate. Netd and inputd own readiness truth and monotonic generations. Compat
  performs check, bounded waiter registration, provider recheck while runnable,
  scheduler arm, exact waiter-presence recheck, commit, and a final
  authoritative provider recheck. It supplies finite/infinite
  timeout and signal-cancellation substrate but never inspects a
  service-private queue. Empty transient poll sets still use the same scheduler
  arm plus deadline/signal wake rather than spinning.
  A 16 ms internal provider-query timeout never becomes the Linux-visible
  application timeout: readiness already found elsewhere in the same scan is
  returned immediately, otherwise the scan retries until the caller's original
  deadline or an unmasked signal settles it.
- Wait interests follow open-description lifetime rather than reusable numeric
  descriptors. Socket and input service references are acquired by
  dup/fork/transfer and released by close/CLOEXEC/process exit. Epoll and remote
  VFS descriptor references are kernel-local: transient syscall snapshots do
  not count, zero cannot be resurrected, and only the initial create/open plus
  idempotent final retire/close crosses into vfsd. This removes a synchronous
  policy-service dependency from descriptor duplication while preserving the
  Linux rule that dup/fork share one open file description.
  Dup/fcntl snapshots one source handle token, acquires its provider reference,
  then revalidates that token under the fd-table lock before commit. Exact-fd
  replacement returns the handle actually retired at that linearization point,
  so concurrent close/reuse cannot redirect either acquisition or cleanup.
  IPC descriptor export likewise acquires the service-backed open-description
  reference before publication; successful installation adopts it, while
  cancellation or rejection moves it to the process substrate's bounded
  deferred-drop queue. That queue shares the 1,024-entry admission ceiling
  with live transfers, reserves that complete capacity before its spin lock is
  published, and preallocates drain output before locking. Task exit therefore
  cannot enter the allocator while holding the global deferred-drop lock.
  Housekeeping releases at most one entry per turn through bounded provider
  cleanup. Executive retirement likewise completes at most four exact cleanup
  records per turn. Individually bounded provider calls therefore cannot
  concatenate into an unbounded scheduler stall, while unacknowledged cleanup
  remains scheduler-owned and unreapable.
  Every remaining provider-side reference mutation carries a kernel-minted
  128-bit operation ID. Netd reserves a bounded replay slot before applying a
  mutation, replays the exact result, rejects operation-ID aliases, and retains
  completion until the kernel ACK. Compat retries the same operation ID and
  keeps unresolved mutation/ACK work in a bounded fail-closed reconciliation
  queue. Vfsd epoll mutations are committed to rootd's authenticated service
  checkpoint before local publication and exact retries cannot advance a
  revision twice. Queue exhaustion is `ENOSPC`, never fabricated success or
  silent eviction.
  A provider restart advances endpoint authority, wakes matching waiters, and
  reports the affected interest as `ERR|HUP` instead of failing the aggregate
  wait or accepting a reused token. The stale epoch remains revoked until an
  explicit MOD rebinds it or DEL removes it; DEL validates only the stable
  fd/open-description key and remains available while the provider is down.
  Unsupported
  edge-trigger and one-shot flags are rejected until their service-owned
  rearm contracts exist; they are never silently approximated.
  On vfsd restart, rootd returns the versioned opaque vfsd checkpoint only to
  the current authenticated vfsd lease. The replacement reconstructs epoll
  refcounts and every provider/object/epoch/events/data interest before it
  creates or publishes its endpoint; malformed, duplicate, stale, or partial
  replay fails closed. Ring0 does not mirror service policy as a fallback.
  Crash-injection runtime evidence is still required before release acceptance.
  Ordinary remote VFS open descriptions now use the same restart boundary.
  Vfsd durably stages the parent and every normalized-path chunk before the
  live record binds kind, cursor, length/content identity, and mutable status
  flags to the kernel-minted capability. If the OPEN response remains
  uncertain, compat closes the exact proposed capability; vfsd can tombstone
  either a staging parent or the completed handle, so neither becomes an
  ownerless fallback object. Sequential read/getdents leaves a
  prepared cursor result until compat commits after successful user copyout or
  cancels on failure. Kernel fd lookup and temporary reference acquisition are
  one atomic close-exclusion step; dup/fork/transfer references remain
  kernel-owned. Final close preserves its exact tombstone across reply loss and
  service restart, and only a separate kernel visibility ACK authorizes exact-
  proof rootd compaction. A stale proof cannot erase a reused key. Source,
  focused tests, and `vfs-open-description-recovery` cover these invariants;
  current-image crash injection and long-churn runtime evidence remain release
  acceptance gates rather than being replaced by a path-reopen fallback.
- Standard input, output, and error descriptors are ordinary Arc-backed console
  open descriptions rather than numeric fd exceptions. Sessiond-backed output
  and error report `POLLOUT` only while their exact session remains live and
  report `POLLHUP` after revoke. An empty `O_NONBLOCK` console read returns
  `EAGAIN` from the first bounded sessiond reply and never re-enters the blocking
  retry loop. Session close is terminal for that session generation: stale
  read, write, TTY, and input-injection requests return `ENODEV` and cannot
  recreate service state or turn a HUP token live again.
  Console liveness counts only descriptor-table ownership: temporary syscall
  snapshots do not postpone the final-close purge or suppress its waiter wake.
- The current signed-image parallel KVM witness passed the five-second runtime
  terminal plus three consecutive WayClick callback, uiserver
  input/presentation, and DVM atomic-page-flip windows above 55 FPS. The
  accepted windows had no input drop, slow, error, or backlog. This is
  current-source virtio/DVM acceptance; physical-GPU admission remains a
  separate hardware evidence boundary documented in `physical-gpu-status.md`.
- KVM latency evidence records input arrival at uiserver queue consumption,
  before a GPU backpressure retry may leave the turn. The previous end-of-turn
  sample counted consumed input and cursor motion but skipped their timestamp
  on that retry, fabricating 100+ ms gaps. One-second profile rollover preserves
  the prior arrival timestamp so a real cross-window stall still fails closed.
- inputd must coalesce lossy DVM pointer motion to the latest delta while
  preserving keyboard and pointer-button edges. Linux key translation and
  modifier/text state remain inputd policy; RustOS receives only bounded,
  authenticated relay records.
- The authenticated DVM `SESSION_START` is a one-shot epoch authority record.
  If netd is not yet published or its bounded grant/revoke call fails, inputd
  retains the already-decoded fixed-size batch, decoder epoch, events, and
  exact unacknowledged transition suffix. It retries outside the policy-queue
  lock for at most five seconds and publishes no event first. A successful ACK
  clears only that transition; timeout exits fail-closed so endpoint-owner
  cleanup withdraws producer admission. Resetting the decoder and draining
  later records after a transient failure is forbidden because the host does
  not repeat the session authority marker.
- Input IPC ABI version 2 extends `InputIngressWire` with a distinct,
  report-atomic DVM absolute-pointer position. It is never reinterpreted as a
  relative delta: complete `SYN_REPORT` positions are bounded to the declared
  surface and identical positions are idempotent. The relative-pointer kind
  remains separate for truly relative devices. Native PS/2 and raw HID-report
  ingress are not accepted ABI variants; an unknown kind, access mode, ABI
  version, or provenance flag is discarded rather than translated into a
  fabricated event.
- Ring0 performs only current-process user-copy of service-returned bytes.
- `INPUTD_IPC_OP_AUTHORIZE_READ` + `SYS_RUSTOS_INPUT_STATS_BROKER` remain compat/observability surfaces while remaining event queue is evacuated.

## Console / TTY Surface (`runtimed` session policy)

- `runtimed::session::SessionRuntime` owns Linux-style line discipline for
  console-hosted programs: canonical edit buffer, echo, cursor edit keys,
  noncanonical byte translation, and termios state.
- `uiserver` forwards focused keyboard `InputEvent`s to
  `CONSOLE_IOCTL_SEND_INPUT_EVENT`; it must not implement shell line editing.
- `runtimed` interprets key meanings through `keyboard-core::KeyCode`, not
  duplicated numeric constants. `InputEvent.text` is the source of printable
  bytes; Enter/backspace/arrows use `KeyCode`.
- Kernel TTY substrate remains a bootstrap/user-copy fallback only. Do not add
  new canonical editing or focus policy to ring0.
- The bootstrap TTY substrate uses the ABI's fixed
  `MAX_CONSOLE_SESSIONS` table. A non-system handle whose slot is outside that
  table is rejected without allocation, waiter publication, or fallback to the
  system console; raw handle bits may never resize ring0 state.

## Device Surface (`devmgrd`)

- `/dev` registry exposed to vfsd via `DevmgrdIpcRequest`/`Response`: `DEVMGRD_IPC_OP_LOOKUP`, `DEVMGRD_IPC_OP_READDIR`.
- vfsd may mirror only the explicit pre-devmgrd bootstrap nodes: `console0`, `display0`, `input0`, `input/event0`, `dri/card0`. **Do not reintroduce wildcard `/dev/*` success path.**
- After devmgrd registration, `/dev/...` file opens route through `devmgrd` `DEVMGRD_IPC_OP_OPEN` → `SYS_RUSTOS_DEVICE_OPEN_BROKER` → transferred device fd in reply. `/dev` directories stay vfsd-owned; devmgrd decides which device file paths exist.
- Device-open uses `DevmgrdDeviceOpenRequest`/`Response` with typed `DeviceId + access + rights`. Broker must install fd with the **exact reduced `DeviceHandleRights` chosen by devmgrd**, not default native-device rights. Broker must not infer policy from paths.
- `ioctl` route ownership lives in devmgrd: kernel compat asks `DEVMGRD_IPC_OP_IOCTL_ROUTE` and follows `DEVMGRD_IOCTL_ROUTE_DEVMGRD`, `DEVMGRD_IOCTL_ROUTE_SESSIOND_TTY`, `DEVMGRD_IOCTL_ROUTE_SESSIOND_COMMIT`, or `DEVMGRD_IOCTL_ROUTE_DIRECT`. Do not add new ioctl policy match tables to ring0.
- Policy-sensitive `ioctl` routes through devmgrd → `SYS_RUSTOS_DEVICE_IOCTL_BROKER` (gated by `DEVICE_POLICY`). Direct ioctl fallback allowed only pre-devmgrd. Hot data-path ioctls (display present) may stay direct broker calls to avoid per-frame policy IPC.

