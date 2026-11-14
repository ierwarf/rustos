# Core ABI and service registry

[Contract index](../contracts-abi.md).

## Kernel/Userspace ABI Surface

Linux `pipe`/`pipe2`/`eventfd`/`eventfd2` are kernel-local descriptor kinds
(`KernelHandle::{Pipe, EventFd}`, waitset provider `WAITSET_PROVIDER_KERNEL`);
the contract is [Kernel-local pipe and eventfd](local-fd.md).

`SYS_DEBUG_PRINT` is a bounded-admission byte stream: success counts only the
accepted input prefix; zero admission returns `EAGAIN`. A later copy fault
returns an already accepted prefix rather than inviting duplicate replay.
Callers retry only the unaccepted suffix, with bounded yielding outside timed
measurement windows. Benchmark output exhaustion fails collection; runtime
best-effort diagnostics count abandoned suffix bytes, and observability falls
back to stderr for the remaining suffix. Retriable refusal is not counted as a
dropped record until its owner abandons it. Admission is not durable delivery.

- Shared ABI crate: `libs/rustos-user-abi`.
- Kernel re-export: `kernel/ps/src/user/{abi,handles,sysops}.rs`. `kernel/compat` re-exports through `kernel_ps::api`; no shadow ABI/handle/user-memory sysop files.
- Device/console/UI `repr(C)` structs and ioctl numbers must live in `rustos-user-abi`. Services (`uiserver`, `runtimed`) consume that crate — never duplicate request structs or ioctl encoding.
- Every native `SYS_RUSTOS_*` number is one literal in
  `libs/rustos-user-abi/src/syscall.rs` or its direct syscall modules, remains
  inside the `0x5255_XXXX` namespace, and is globally unique. Adding a syscall
  requires `formal/check-native-syscall-numbers.py` to pass; a duplicate would
  make top-level dispatch select an unrelated authority path and is therefore
  a critical ABI failure, not a compatibility alias.
- Evacuation policy, ring0/ring3 boundary, service ownership: live source
  `RING3-MIGRATION-REFERENCE` / `RING3-MIGRATION-COMMENTED-OUT` markers, exact
  broker call paths, and owning service contracts.
- `RING3-MIGRATION-REFERENCE` / `RING3-MIGRATION-COMMENTED-OUT` blocks are references for migration, not dormant code to revive. Do not fix breakage by uncommenting them unless the exact lines are the remaining ring0 substrate.
- For each slice, move policy/state/lifecycle behavior into the owning service, leave only narrow ring0 fd-table/user-copy/page-table/privileged-device substrate, then delete or bypass the reference block.

## Native ABI revision stamp

- Source: `libs/rustos-user-abi/src/abi_revision.rs`
  (`NATIVE_ABI_REVISION_CURRENT = 1`, `NATIVE_ABI_SUPPORTED_REVISIONS`, parser
  `parse_native_abi_note`). Rules for when to bump: `docs/architecture/evolution-rules.md` section 7.
- Format: ELF note in section `.note.rustos.abi` (4-aligned, becomes a
  `PT_NOTE`): `namesz=7`, `descsz=4`, `type=1`, name `"RustOS\0"` (padded to
  4), desc one little-endian `u32` revision; 24 bytes. Emitted by
  `rustos_user_abi::native_abi_note!` (also expanded by `rustos_svc_runtime::entry!`); a second expansion in one
  image is a link error (hidden symbol `__rustos_native_abi_note`).
- Scope: every packaged executable whose crate depends directly on `rustos-user-abi` or `rustos-svc-runtime` (static-PIE and dynamic Rust-std alike; one expansion per executable root). C demos, apps/shell, sessiond and prebuilt apps are unstamped. xtask `native_abi.rs` derives the set from `Cargo.toml` dependencies; exceptions live in its `NOT_STAMPED` list.
- `loaderd` (`admit_native_abi_stamp`, `libs/rustos-image-admission`) reads at
  most `NATIVE_ABI_MAX_NOTE_SEGMENTS` (8) `PT_NOTE` segments, the first
  `NATIVE_ABI_NOTE_READ_MAX` (4096) bytes of each, for the main executable of
  a spawn. Absent: admitted unchanged. Supported revision: admitted.
  Unsupported revision, malformed RustOS note, or more than one: spawn fails
  with `ENOEXEC` and `loaderd` prints one diagnostic line. Foreign notes are
  skipped; a foreign note with broken framing ends the walk as absent.
- The stamp is a self-declared hint, not authority: never mapped to a
  personality, not consulted by `personality_gate` or `rootd`, grants nothing.
- Kernel-bootstrap-spawned services bypass `loaderd`; the kernel does not read
  notes. `cargo xtask build`/`stage` fails if any staged native ELF lacks the
  current revision; `cargo xtask check` checks the sources and fresh artifacts.

## Boot Initial Task

`rootd` (`services/rootd/rootd.elf`) is the first user process:

- Must avoid Linux libc/std dynamic runtime deps.
- Spawns `syscalld`, `vfsd`, `loaderd`, `procd`, then hands off to `services/initd/initd.elf`.
- Kernel boot code must not grow generic POSIX compat exceptions for `initd`; early bootstrap surface stays narrow, explicit, tied to `rootd` bringing up foundational policy services.
- Stays resident as `IPC_SERVICE_ROOTD`; serves only the versioned
  `COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR` contract and tracks
  `CoreServiceLeaseWire` via `SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER`.
- Bootstrap policy services must not perform a rootd-authorized service lookup
  while rebasing private maintenance state. Rootd, procd, and syscalld therefore
  own independent bounded lifecycle fan-out queues. Procd consumes exit evidence
  only for signal policy; syscalld consumes the same evidence directly before
  interpreting its next request and owns credential/MM-policy cleanup. Neither
  service calls the other or rootd during the drain, so queued recovery evidence
  cannot close a `rootd -> loaderd -> procd -> rootd` authorization cycle.
- Each fan-out queue is fixed-capacity and allocation-free under its tracked
  spin lock. Drain output reserves caller capacity before locking, copies only
  the claimed prefix, and removes that prefix only after successful copyout;
  producer appends during copyout therefore survive the transaction.
- Every lifecycle drain requires the exact current ABI version and zero reserved
  fields. Rootd overflow is sticky and terminal. Procd/syscalld overflow rebases
  only the affected private queue after its owner clears all cached per-process
  authority.
- Procd additionally receives `LIFECYCLE_EVENT_THREAD_EXIT` (`pid`, `tid`),
  queued by `syscall_process_exit` when a non-last thread exits while its process
  lives on. `LifecycleEventWire` is 32 bytes (`event`, `exit_status`, `pid`,
  `parent_pid`, `tid`); drain ABI version 2. Thread exits use a separate
  256-entry queue (`PROCD_THREAD_EXIT_EVENTS`, never locked together with the
  process queue) appended behind the process events in the same drain, so thread
  churn cannot overflow the process queue and trigger procd's forget-everything
  reset. If that queue is full the producer sets a sticky loss marker that the
  drain reports as `LIFECYCLE_EVENT_THREAD_EXIT_LOST` after the queued exits
  (restored if copyout fails). procd then drops thread entries with no
  sigaltstack (the mask is re-seeded from the kernel on the next request) and
  keeps sigaltstack entries until process exit: a bounded residue, not a loss of
  live state. Thread exit is emitted only to procd.
- The lifecycle transaction id is a global non-wrapping `u64` allocator
  (`process_table.rs`); the trace marker records its low 32 bits. Exhaustion
  (practically unreachable) fails every later spawn/exec/exit closed instead of
  wrapping onto a live id.
- User copy-out of a struct goes through `usermem::write_current_user_struct` /
  `write_current_user_slice`, which require `rustos_user_abi::wire::UserWire`.
  Implement it only with `user_wire!(Type { field: Ty, .. })`: the macro lists
  every field (a missing or mistyped field is a compile error), requires each
  field to be `UserWire` itself, and asserts the field sizes sum to the type's
  size. A struct with padding therefore cannot be copied out; the padding must
  be an explicit zeroed field. Do not hand-cast a struct to bytes for copy-out.
- Vfsd is a fourth consumer (`LifecycleConsumer::VfsPolicy`, selected by the
  `VfsPolicy` capability, after the rootd/syscalld/procd capabilities). It
  receives every exit plus a `LIFECYCLE_EVENT_FORK` (`pid` = child,
  `parent_pid` = forking process) that `proc_fork_broker` queues while the child
  is still suspended. Vfsd drains before it interprets each request, so the
  FIFO order exit-before-pid-reuse / fork-before-child-runs makes the per-pid
  cwd generation-safe: exit prunes the entry, fork copies the parent's cwd (or
  clears a stale entry under a reused pid). A fork whose activation fails queues
  an exit-only event for the abandoned child. Overflow (`EOVERFLOW`) loses
  identity evidence, so vfsd forgets every cwd (default `/`) and resumes from the
  rebased empty queue; any other drain failure is logged and leaves the table
  unchanged.
- `/run` is vfsd's volatile in-memory directory namespace (`vfsd::run_dirs`):
  `mkdir` of `/run/user` and `/run/user/<euid>` creates a real bounded entry that
  `stat`/`getdents` observe (`EEXIST` for existing, `ENOENT` for a missing
  parent); every other path stays `EROFS`. Removal of runtime directories is not
  implemented.

## IPC Service Registry

All raw and handle-carrying `SYS_RUSTOS_IPC_CALL*` waits use the same finite
30-second service ceiling. Timeout cancels the exact reply identity and drops
any transferred-handle authority; a late reply is invalid. Shorter provider or
application deadlines remain authoritative and may not be widened to this
ceiling. Server receive loops may wait for new work indefinitely because they
hold no caller reply, but every accepted synchronous request must reach reply,
timeout/cancel, peer-close, or owner-exit cleanup.
Latency-sensitive service-to-service control uses
`SYS_RUSTOS_IPC_CALL_BOUNDED`; the caller supplies a nonzero deadline no wider
than the 30-second ceiling. Inputd's authenticated session notification to
netd is capped by the 16 ms readiness rail, and timeout revokes the exact reply
rather than leaving a late transition able to reactivate a retired generation.

Kernel-owned cross-service calls use the typed limits in
`rustos_user_abi::performance`, never an unlabelled copy of the 30-second
ceiling: non-consuming readiness queries are capped at 16 ms, interactive
policy-only calls at 100 ms, state-changing mutations (a timeout leaves their outcome unknown) at a 5 second lease sized to outlast one executable snapshot, boot/control transactions at 5 seconds, and bulk
data at 30 seconds. A call may use a shorter application deadline but cannot
widen its class. Bulk data is reserved for operations whose external device or
application semantics can legitimately wait; endpoint lookup, authorization,
ioctl routing, readiness, close/dup/exec maintenance, and frame submission are
not bulk data. The class is explicit at every kernel call site, and
`formal/run-source-conformance.sh` rejects a broken limit ordering.

Linux futex WAIT/WAKE/REQUEUE operations are scheduler substrate, including
their fixed opcode and flag envelope validation. They must validate locally,
snapshot task/MM identity from scheduler-local state, and avoid allocation,
the process-state lock, and synchronous policy-service calls before waiter and
deadline registration. This keeps ordinary userspace mutexes independent of
unrelated same-process state mutations, syscalld scheduling, restart, and IPC
timeout and closes the pre-registration unpark race. Syscalld may own
process-level ABI admission or versioned policy leases, but operation 66 is
retired and no per-futex policy RPC exists.

Linux `clock_gettime`, `nanosleep`, and `clock_nanosleep` use the same local
timer-substrate rule. Their supported clock IDs, flags, and timespec envelope
are fixed ABI validation, not mutable per-call policy. Operations 51 through
53 are retired; the hot path must not acquire process state or call syscalld
before arming a finite deadline. A future mutable time policy must be admitted
once through an immutable scheduler-local lease, never through each clock read
or sleep.

Ring3 authority deadlines use the same kernel-wide `CLOCK_MONOTONIC`
nanosecond domain. `try_monotonic_nanos` distinguishes syscall failure,
malformed timespec and arithmetic overflow from a valid zero timestamp.
The infallible service-runtime wrapper emits a fatal clock diagnostic and
exits with status 125 on error; it never fabricates zero or saturates invalid
time into an unlimited lease. Inputd uses that same checked reader. A caller
that can safely abort one transaction may use the fallible API instead of
terminating its service. Monotonic instants are not portable across the
RustOS/DVM/L0 clock domains.

Endpoints registered via `SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT`, looked up by stable `IPC_SERVICE_*` id. Registering endpoint `0` revokes; later lookups fail closed.
Before final endpoint cleanup, a terminating process is marked exiting. Endpoint
publication, explicit revoke, and exit cleanup share one registry mutation
critical section; registration rechecks that marker inside it and fails with
`ESRCH`. This prevents concurrent registrars or an exiting process from
publishing a stale endpoint after the cleanup scan. Endpoint lookup and
capability checks also fail closed when the recorded owner is already marked
exiting, including the interval before the cleanup stores become visible.
The kernel compat service table records low-volume structured milestones for
endpoint lifecycle transitions: `ipc-service-register`,
`ipc-service-register-denied`, `ipc-service-register-busy`,
`ipc-service-revoke`, `ipc-service-revoke-denied`, and
`ipc-service-exit-revoke`. These are diagnostic state only; rootd remains the
service admission and capability policy owner. Endpoint revoke is owner-only
unless the caller holds `ROOT_SUPERVISOR`.
`SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT` is service/supervisor discovery: root-supervisor callers may use the kernel table directly; other callers are admitted through rootd `COMMERCIAL_MAX_ROOTD_OP_SERVICE_LOOKUP`. Core services and post-init services are admitted only by matching a running rootd lease. Generic apps use Linux/Win32 ABI routes and kernel compat helpers, not raw policy-service endpoint lookup.
Service capability assignment is rootd policy, delegated to the kernel ahead of time: after self-registration rootd installs the typed routing policy with `SYS_RUSTOS_SERVICE_POLICY` (`INSTALL`: directed lookup routes plus each service's `ServiceCapability` list as discriminants; no mask) and binds each live process to its authenticated service identity (`BIND_PRINCIPAL`) when its lease becomes live, before the child is activated. The kernel then decides every service lookup and every endpoint registration from that table with no rootd round trip; a caller with no binding is refused. `COMMERCIAL_MAX_ROOTD_OP_SERVICE_CAPABILITY` / `_SERVICE_LOOKUP` (6, 7) are retired and never reused. Do not reintroduce a full `service_id -> capability` table in ring0 policy: the table is installed data, authored by rootd.
Rootd self-registration is a boot-only trust transition, not a permanent
fallback. The first successful rootd publication seals its non-reusable process
identity for the boot; revoke or exit never allows a foreign process to claim
`IPC_SERVICE_ROOTD`. A rootd failure is therefore fail-stop until reboot unless
a future explicit, authenticated replacement protocol is added.
Every service publication first proves that the endpoint is owned by the
publishing process. Non-root registration also binds rootd's authorization to
the exact rootd endpoint epoch and rechecks that epoch under the registry
mutation lock at commit; restart/revoke/exit between check and publish returns
`EAGAIN` and cannot publish stale authority.
Numeric endpoint IDs are not ambient call authority. A successful service
lookup records a bounded process-local grant for that exact service publication
epoch; ordinary IPC calls and calls carrying handles must present the endpoint
under that grant. Revoke/republication invalidates it by epoch, and process exit
removes it. Endpoints outside the live service registry are callable only by
their owner process. Raw endpoint values are not inherited/transferable
capabilities; future cross-process endpoint transfer must use an explicit
typed-handle grant and lifecycle contract.
An exact current-epoch grant also satisfies repeated lookup without another
rootd request. The lookup still rechecks the live endpoint, owner-exit marker,
and publication epoch under the registry lock; a miss, revoke, or republish
returns to rootd admission. Caching an endpoint without those checks is
forbidden, but re-authorizing an unchanged grant is a performance-contract
violation because it converts a local table read into an avoidable synchronous
boot/runtime dependency.
Successful call authorization also returns the matched service identity as
part of that same publication snapshot. Reply-deadline classification must use
this captured identity; it must not translate the numeric endpoint through a
second service-publication lookup. Private owner-local endpoints carry no
service identity and use the generic finite deadline. This keeps authority and
deadline policy bound to one publication epoch while avoiding a redundant
registry read on every synchronous IPC call.
Runtimed's Unix control socket is also an authority boundary, not a trusted
local channel. It reads `SO_PEERCRED` and ignores caller-supplied identity.
Snapshot, launch, and terminate require either the current live uiserver
service owner or a running process whose immutable launch record carries the
signed `logical_admin` bit. `UI ready` is uiserver-only. Socket path permissions
are defense in depth and never replace this per-request authorization.
Post-init lease reporting is ring3-owned: `initd` reports successfully spawned
policy services to rootd with `COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL`
(`arg0=IPC_SERVICE_*`, `arg1=pid`, `path=exec`). rootd uses that lease registry
when installing the kernel service policy.
Services launched by another supervisor must be reported by that supervisor
before the child depends on service endpoint registration; `runtimed` reports
the `uiserver` lease. There is no post-init capability or lookup allowlist
fallback. `READINESS_SIGNAL` is a supervisor-spawn lease admission signal, not
proof that the child has registered its own policy endpoint; endpoint readiness
is still determined by `SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT` /
`SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT`. Supervised launches use the
deferred-start transaction: loaderd creates the child suspended, the supervisor
admits the exact PID lease in rootd, loaderd activates the child, and the
supervisor waits with `SYS_RUSTOS_IPC_WAIT_SERVICE_ENDPOINT`. The wait is
race-free, deadline bounded, keyed by service id plus expected PID, and wakes on
registration or child exit. Polling endpoint lookup is not a readiness model.
rootd must authorize the reporter
for each post-init lease: `initd` reports netd/devmgrd/inputd/storaged
and runtimed-as-sessiond, while running sessiond/runtimed reports uiserver.
Reports for an already-running lease are idempotent only for the same PID and
must reject attempts to overwrite the lease with a different PID. rootd also
records the kernel-stamped reporter PID. A newly restarted `initd` reconciles
its five service leases through the generated-IDL
`COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_SNAPSHOT_IDL` (supervisor op 15,
schema 517): it adopts only an exact-PID endpoint, keeps an
endpoint-pending admitted lease in a bounded 30-second recovery window,
then requests `COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_RECLAIM_IDL`
(supervisor op 16, schema 518) for the exact stale PID. The former scalar
query/reclaim operations 8 and 9 are retired with no fallback. Both replies
bind the service and request challenge; destructive reclaim also binds the
exact PID and prepares its IDL acknowledgement before endpoint revocation.
Only the current initd may query/reclaim its own service classes; sessiond's
uiserver child is reclaimed first with its parent. Reclaim uses the
root-supervisor-only `SYS_RUSTOS_ROOTD_TERMINATE_BROKER`, which retires all
threads, marks the process exiting, revokes its endpoints, clears loader
broker state, records one fixed SIGKILL lifecycle exit, and signals its parent.
It is teardown substrate, not a generic kill syscall or a kernel restart
policy table.
`SYS_RUSTOS_ROOTD_MEMORY_BROKER` (ABI v1, also root-supervisor-only) is its
read-only counterpart for memory pressure: one in-place struct returns usable,
free and kernel-reserve pages, the monotonic watermark-denial count, and up to
32 live processes ordered by resident data-leaf count. Ring0 neither retains
pressure state nor chooses a victim; rootd's `memory_governor` does, then
terminates through the call above. Each census entry also carries the process's
`job`.
`SYS_RUSTOS_JOB` (`0x5255_0057`, ABI v4) operates the kernel job tree
through one in-place `RustosJobArgs` and an exact typed Job descriptor with
operation-specific rights: `GET_INFO`,
`CREATE_CHILD`, `KILL` (bounded: at most 128 members ended per call, the rest
reported in `remaining`) and `PROCESS_JOB`. Input fields only on entry; every
output field must arrive zero. A job id is `(slot << 32) | generation` and the
root job is `JOB_ID_ROOT` (1). A caller inside the target subtree and the root
job are refused with `EPERM`. `OPEN_SUBTREE` (op 6) takes the target id in
`job` and a rights mask, and returns a handle attenuated to that mask only when
the target lies within the authorizing handle's subtree; anything else is
`ESRCH`. It needs the `DUPLICATE` right and is how `rootd` reaches a job named
by a census entry. ABI v3 adds `SET_LIMITS` (op 7, `SET_LIMITS` right): input
`limit_mask` (`JOB_LIMIT_PROCESSES | JOB_LIMIT_THREADS`) with
`limit_processes` / `limit_threads` (`JOB_LIMIT_UNSET` inherits); the reply
carries the limits now in force. `GET_INFO` additionally returns own and
effective limits and subtree usage. Unknown mask bits are `EINVAL`; the
limit fields must be zero for every other op. ABI v4 adds
`JOB_LIMIT_MEMORY_PAGES` (`limit_memory_pages`, `effective_memory_pages`,
`usage_memory_pages`): a limit on resident user data pages of the job and its
descendants, enforced by refusing the page claim (`OutOfFrames`); it does not
count page tables or kernel objects. Design and limits:
[Job Model](../../architecture/job-model.md#phase-1-as-built).

`SYS_RUSTOS_JOB_POLICY` (`0x5255_005a`) has an independent 96-byte v1
`RustosJobPolicyArgs` wire in `libs/rustos-user-abi/src/syscall/job_policy.rs`.
GET requires `INSPECT`, SET requires `SET_POLICY` (not `SET_LIMITS`) on the typed
Job descriptor. Flags, reserved, output job identity and effective mask must be
zero on entry; GET also requires an empty own mask. Own/effective masks each
have four 64-bit words; effective is the intersection of every live ancestor.
New jobs default to ALL, so existing behavior is unchanged. A ceiling never
grants a broker role; broker roles are the typed `ServiceCapability` values the kernel holds (see `SYS_RUSTOS_SERVICE_POLICY`).
SET refreshes every descendant before returning, but racing single-operation
readers may see the old or new word, not an atomic whole-subtree snapshot.
A late user-copy failure does not roll back the completed scalar mutation.
The handler in `kernel/compat/src/user/syscall/linux/job_policy_ops.rs` resolves
generations again under the lifecycle lock. `kernel/ps/src/multitask/current/job_policy.rs`
uses exact published membership, generation-bound cached ceilings and final
membership revalidation without a PID scan, state borrow or table lock.
Missing publication fails closed. The opt-in `RUSTOS_ROOTD_POLICY_PROBE=1` build
exercises denial/restoration and typed hierarchy through
`services/rootd/src/job_policy_probe.rs`; it is not a default boot check or a
completed broker-policy migration.

Rootd receives supervisor requests through endpoint-owner-only sender-stamped
receive syscalls; both pre-init and post-init supervisor turns must drain
lifecycle/restart state before using `SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER`, so
a failed child cannot hide behind an indefinite rootd IPC wait. `READINESS_SIGNAL` must reject payload `subject_pid/tid`
values that do not match the kernel-stamped sender PID/TID. The sender-stamped
receive path first re-authorizes the exact endpoint owner and is not a generic
app IPC ABI; kernel-stamped sender identity does not grant policy capability.
Rootd's supervisor loop must never wait without receiving, because service
endpoint registration synchronously depends on rootd capability replies and
every core service's heartbeat is a 250 ms bounded call to rootd. Between
nonblocking drain turns it either yields (the drain hit its budget) or blocks in
one bounded receive (`SYS_RUSTOS_IPC_RECV_WITH_SENDER_BOUNDED`,
`ROOTD_SUPERVISOR_IDLE_POLL_MS`) that the next control message ends at once.
This holds for the pre-init core-readiness loop as well as post-init: readiness
is re-checked after every such boundary, and the readiness budget is charged by
monotonic time (`supervise_core_readiness`), never by sleeping out a window.
No service may hold a local policy/state lock across service discovery or a
synchronous cross-service call. Early-boot maintenance paths additionally
skip discovery when their drained work set is empty. A new bootstrap
dependency must either be covered by rootd's declared readiness order or use a
bounded, explicit ready handshake; startup scheduling luck is not readiness.
Rootd must also remain the sole consumer of its supervisor endpoint while a
loaderd launch is in flight. Because loaderd executable open can synchronously
enter `vfsd`, and vfsd commits open-description state through rootd's
checkpoint protocol, rootd may not block its supervisor thread in
`SYS_RUSTOS_IPC_CALL` to loaderd without another receiver. It delegates the
sole supervisor endpoint turn to exactly one same-process, fixed-stack worker
while the original rootd thread performs the loader call. The worker blocks in
endpoint receive, drains lifecycle evidence, and services nested loaderd/vfsd
requests without scheduler polling. After the loader call publishes the exact
PID or errno, the original thread issues the sender-stamped
`COMMERCIAL_MAX_ROOTD_OP_LOADER_WORKER_COMPLETE` wake on the same endpoint;
only that same-process request may advance `RESULT_READY` to `COMPLETE`, and
the original thread does not touch the borrowed supervisor state until the
worker publishes `EXITED`. The worker receives no new capability and concurrent
jobs fail closed; direct spawn and deferred checkpointing are not permitted
substitutes.

| ID | Service | Capability (`ServiceCapability`) | Owns |
|----|---------|----------------------------------|------|
| 1 | `syscalld` | `LINUX_SYSCALL_POLICY` | Linux cold validation, credentials, rlimits, clock/random/MM policy, Win32 syscalls |
| 2 | `vfsd` | `VFS_POLICY` | File namespace, cwd, dir/file cursors, mount/umount, metadata |
| 3 | `netd` | `NET_POLICY` | Socket namespace + all socket syscalls (AF_UNIX too) |
| 4 | `devmgrd` | `DEVICE_POLICY` | `/dev` registry, device open, ioctl auth |
| 6 | `loaderd` | `PROCESS_LOADER` | ELF/PE image policy, mapping, launch |
| 7 | `storaged` | `STORAGE_POLICY` | Block inventory after registration |
| 8 | `inputd` | `INPUT_POLICY` | Input ingest, read payload |
| 9 | `procd` | `PROCESS_POLICY` | exec/fork/wait/signal |
| 10 | `rootd` | `ROOT_SUPERVISOR` | Core-service leases, restart budgets |
| 11 | `sessiond` (reserved) | `SESSION_POLICY` | Console/TTY/session |
| 12 | `pagerd` | `PAGER_POLICY` | VM object, fault, page-cache, COW, reclaim/writeback policy |
Service IDs 5 and 13, their former capability bits, and raw syscall numbers
`0x52550020..=0x52550022` plus `0x52550037` are permanently retired. They are
not extension points and must continue to return `ENOSYS`.

### Linux `prctl` (syscall 157)

Implemented in `kernel/compat/src/user/syscall/linux/prctl_ops.rs` (option table is pure and
unit-tested). Supported: `PR_SET_NAME`/`PR_GET_NAME` (per-thread 15+NUL bytes in
`LinuxThreadState::comm`; copied to clone/fork children, reset by exec; unset
reads as the exec-path basename), `PR_SET_NO_NEW_PRIVS`/`PR_GET_NO_NEW_PRIVS`
(`ProcessSecurityContext::no_new_privs`: one-way, fork-inherited,
exec-preserved; arg2 must be 1 to set, stray args are `EINVAL`), and
`PR_SET_DUMPABLE`/`PR_GET_DUMPABLE` (0/1 only; fork-inherited, exec-reset to 1;
nothing consumes it yet). Everything else returns `EINVAL`, never success:
`PR_SET/GET_PDEATHSIG` (no parent-exit signal hook), `PR_SET/GET_SECCOMP` (no
filter engine: run Chrome with `--no-sandbox`), `PR_SET_VMA`,
`PR_SET/GET_TIMERSLACK`, `PR_CAPBSET_READ` (no capability bounding set) and
unknown options. User pointers go through `usermem`; the name read never
crosses a page boundary beyond the string's NUL.

Broker authorization checks the caller's registered service capability — **not** its executable path.
`SYS_RUSTOS_ENTROPY_BROKER` is limited to the Linux-syscall, network-policy,
and process-loader capabilities and returns at most one inline-IPC payload of bytes from the
boot-seeded ChaCha20 substrate. Initialization is one-shot and rejects an
absent/all-zero seed. A private master stream derives every child seed from
master output; consumers never clone the master or form related keys by
combining the boot seed with public PID/TID/counter state. Syscalld still owns
Linux `getrandom` flag, length, and error policy; netd still owns token
collision/admission policy; loaderd owns image and initial-stack ASLR policy. PID/TID/counter-derived pseudo-random output is
forbidden for credentials, object capabilities, ASLR material, and Linux
`getrandom`.
`pagerd` is a standalone rootd-managed service and is the only process that may
register `IPC_SERVICE_PAGERD` or receive `PAGER_POLICY`. The former syscalld
delegation is retired; it is not a fallback. `initd` must not be spawned until
the exact pagerd endpoint and lease are registered, otherwise dynamic-loader
fault/backing policy can deadlock behind rootd's own initd spawn IPC.

Pager fault wire ABI v2 adds `backing_service` to
`PagerObjectIdentityWire`. Anonymous objects require that field to be zero;
file-private, file-shared, image-section, and memfd objects require a nonzero
owning service identity. A page-cache key is the exact tuple of backing service,
object slot/generation, backing generation, and page offset. Paths, frame
numbers, or an object slot without generations are never cache authority.
The bounded policy implementation is `services/pagerd/src/page_cache.rs`.
For the anonymous Phase-7 path, `PagerFaultDispatchWire` carries the canonical
fault request plus an opaque, one-shot pre-zeroed frame capability and its
maximum W^X-safe rights. `pagerd` must return that exact capability only in a
`MAP_ZEROED` reply and may only attenuate those rights; the former generic
commercial-request `arg0` grant is not an authority channel.
Current status: single-load admission, clean/private-COW, dirty/writeback,
provider-generation sealing, mapped/dirty frame quarantine, exact-token dirty
reauthorization, and TLB-acknowledged reclaim exist with model and mutation
evidence; vfsd/storaged I/O, kernel fault dispatch, runtime pressure/restart,
and Linux/Windows differential cutover are still required before Phase 8 is
complete.

### Library object control

`SYS_RUSTOS_LIBRARY_OBJECT` (`0x5255_0064`, ABI v1) takes `RustosLibraryObjectArgs`
with ops CREATE(1)/SUPPLY_PAGE(2)/SEAL(3)/RETIRE(4)/PRESSURE(5; returns
`LibraryPressureWire` free/min-free/denials/reserved pages). The caller must hold
PagerPolicy and the live pager lease. CREATE takes a `LibraryIdentityWire`
(immutable `VfsFileIdentityWire` plus a 32-byte content hash, at most 1024
pages); SUPPLY_PAGE copies the exact remaining bytes (kernel zero-fills the
tail) into frame-source pages; an object is invisible to guests until SEAL.
Limits: 128 objects, 12288 reserved pages. Related wire: vfsd ops
`VFS_IPC_OP_FILE_IDENTITY`=26, `VFS_IPC_OP_LIBRARY_READ`=27 (pager only, chunks
up to `VFS_LIBRARY_READ_MAX_BYTES`), and `MM_BROKER_OP_BIND_LIBRARY`=11.
`SYS_RUSTOS_LIBRARY_MAP_PROBE` (`0x5255_0065`) is a diagnostic for the opt-in
probe only (frames/stats; caller exec path must be `libmapprobe.elf`).
Design and status: `docs/ai/pager/library-cache.md`.

### Immutable frame-source control

`SYS_RUSTOS_FRAME_SOURCE` (`0x5255_0063`, ABI v1; formerly collided with the app-buffer broker at `0x5255_0061`) takes the exact 88-byte
`RustosFrameSourceArgs`. `SUPPLY` copies at most one page, zeroes its tail,
charges the pager's actual resident group, and returns a reusable source token.
`RETIRE` closes that token; `OWNER_EPOCH` returns the currently published service
lease epoch. Owner IDs and charge groups are never caller-controlled fields.
The caller must both hold PagerPolicy and own the live pager service lease.
Source publication is serialized with lease withdrawal; user copies and cold
reclamation never run under the registry lock. Source ownership binds logical
service ID plus publication epoch, not endpoint-object identity: unregistering
and registering the same endpoint cannot revive previous source tokens.

Keys bind backing namespace, object identity/generation, content generation and
page offset. Source rights require READ and permit optional EXECUTE, never
WRITE; private writable views obtain their own COW frame. All reserved fields
are checked before admission. A source pin protects installation; installed leaves acquire independent MM
custody. Withdrawal/exit and last-pin Drop only queue reclamation. The normal
pager housekeeping hook drains a bounded64 sources outside locks, after which
unused physical frames and their source charges may return. Existing mapped
leaves survive source retirement and still obey their TLB-safe final release.
Private file/section dispatch carries no zero-frame reserve. Its fixed reply
adoption pins the exact source through current MM/VMA validation and independent
COW leaf publication; source pages are never anonymously populated as a run.
This is not yet live file paging: VFS identity/VMA admission, pagerd I/O/cache
reuse and boot prewarming still need connection.

### syscalld offload envelopes

The kernel offloads Linux policy syscalls to `syscalld` over one endpoint and
tells the envelopes apart by **frame size**; there is no tag field to forge.

| Frame | Used for | IPC lane |
| --- | --- | --- |
| `LinuxSyscallOffloadFastRequest` (40 B) | `getuid`, `getgid`, `geteuid`, `getegid` | fast lane |
| `LinuxSyscallOffloadFastMemRequest` (72 B) | `mmap`, `munmap`, `mprotect`, `brk` | fast lane |
| `LinuxSyscallOffloadRequest` (full, path-bearing) | everything else | blocking transaction |

The compact address-space frame carries a kernel-issued `ticket` (expanded into
`arg3` of the full request that `syscalld` handlers see). `syscalld` must echo it
in `RustosMmBrokerArgs::ticket` (broker ABI **v2**, 88 B) on every MM broker call
it makes while serving the request. The kernel opens a request lease, capturing
the caller's process and MM generations, before the IPC call and closes it when
the call returns for any reason; a broker operation is refused (`ESRCH` for an
absent/retired ticket, `EPERM` for another process's ticket) unless it names a
live lease for `target_pid`, and then acts only on the captured generations. The
only exception is `MM_BROKER_OP_SET_ANON_POLICY`, authorized by owning the pager
endpoint. See `address-space-lifecycle-contract.md` (request lease).

Both compact requests answer with `LinuxSyscallOffloadFastResponse` (48 B, 32 B
payload). The IPC fast lane admits only frames up to `IPC_FAST_INLINE_BYTES`
(256 B); the full request is larger, so every operation sent in it pays the
heap-buffered blocking transaction. Measured at 8 vCPU, a trivial handler
(`umask`) over the full frame cost about 129k cycles against about 25k for
`getuid` over a compact one, which is why the address-space operations, the
hottest offloads, have their own compact frame (`mmap` 114k -> 31k, `munmap`
117k -> 41k, `mprotect` 124k -> 41k cycles at the median).

A compact request must carry the caller's own `pid`/`tid`
(`identity_is_exact_sender`); `syscalld` refuses any other identity, so a
service cannot name another process's mappings. Operations that carry a path or a
payload larger than 32 B stay on the full request.

### Client-Job accounting schema 519

Production sessiond/rootd control of client session accounting uses a
canonical generated-IDL frame in the existing supervisor IPC envelope:
wire schema 519 carries token, exact client PID, operation, Job output and
request/ack phase. The historical scalar arg0/arg1 request form for these
operations is retired. Outer supervisor operations 17/18 remain as selectors
rather than untyped payload formats.


### Job and supervisor management ABIs

- SYS_RUSTOS_JOB_KMEM (0x5255_005e): versioned INSPECT and SET_LIMIT for the
  exact boxed ProcessObject allocation byte dimension, plus INSPECT_CLASS and
  SET_CLASS_LIMIT for six separately accounted classes: handle-table slots,
  endpoint queues, page-table frames, user-thread kernel stacks, shared-mapping
  records, and queued IPC messages. This is **not** a total kmem allocator
  ledger. Loaderd obtains actual kernel-reported object size before setting
  application Job limits; class-specific limits and evidence remain distinct.
- SYS_RUSTOS_JOB_SESSION (0x5255_005b), version 2: provider-PID and exact token
  support; kernel retains token/release watermark and rootd process-crash drain.
- SYS_RUSTOS_JOB_MEMORY_QOS (0x5255_005c): nonblocking memory.high threshold
  event and conservative memory.min policy controls.
- SYS_RUSTOS_CHECKPOINT_JOURNAL (0x5255_005d): rootd-only memory-resident
  service checkpoint operation journal, survives process death but not reboot.
- Generated IDL schemas 514/515/517/518/519/520: authenticated heartbeat, update
  approval, post-init lease snapshot/reclaim, client-billed session control and
  the provider session RPC (ops 25-28); other rootd envelopes are still C-shaped.

Full status and Codex fault-injection handoff:
[Ring3 eight-stage status](../../validation/ring3-8stage-status.md).
- SYS_RUSTOS_SERVICE_POLICY (0x5255_005f): rootd-only (`RootSupervisor`) `INSTALL` of the typed routing policy (generation must increase; validated whole before it replaces the old table) and `BIND_PRINCIPAL` of a live pid to a service identity under the installed generation. Typed wire only (`ServiceRouteWire`, `ServiceCapabilityListWire`).
