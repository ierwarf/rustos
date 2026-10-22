# Core ABI and service registry

[Contract index](../contracts-abi.md).

## Kernel/Userspace ABI Surface

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
policy-only calls at 100 ms, boot/control transactions at 5 seconds, and bulk
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
Service capability assignment is rootd policy: after rootd self-registration, kernel compat asks `COMMERCIAL_MAX_ROOTD_OP_SERVICE_CAPABILITY` with the registering subject PID/TID and records the returned `IPC_SERVICE_CAP_*` mask only if rootd confirms the PID matches the running lease. This includes endpoint registration through the Linux syscall ABI. Do not reintroduce a full `service_id -> capability` table in ring0.
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
when answering kernel compat `SERVICE_CAPABILITY` / `SERVICE_LOOKUP` requests.
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
its five service leases through `COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_QUERY`:
it adopts only an exact-PID endpoint, keeps an endpoint-pending admitted lease in a
bounded 30-second recovery window, then requests
`COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_RECLAIM` for the exact stale PID.
Only the current initd may query/reclaim its own service classes; sessiond's
uiserver child is reclaimed first with its parent. Reclaim uses the
root-supervisor-only `SYS_RUSTOS_ROOTD_TERMINATE_BROKER`, which retires all
threads, marks the process exiting, revokes its endpoints, clears loader
broker state, records one fixed SIGKILL lifecycle exit, and signals its parent.
It is teardown substrate, not a generic kill syscall or a kernel restart
policy table.
Rootd receives supervisor requests through endpoint-owner-only sender-stamped
receive syscalls; both pre-init and post-init supervisor turns must drain
lifecycle/restart state before using `SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER`, so
a failed child cannot hide behind an indefinite rootd IPC wait. `SERVICE_CAPABILITY`,
`SERVICE_LOOKUP`, and `READINESS_SIGNAL` must reject payload `subject_pid/tid`
values that do not match the kernel-stamped sender PID/TID. The sender-stamped
receive path first re-authorizes the exact endpoint owner and is not a generic
app IPC ABI; kernel-stamped sender identity does not grant policy capability.
Rootd's supervisor loop must yield
between nonblocking receive turns rather than sleep-polling, because service
endpoint registration synchronously depends on rootd capability replies.
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

| ID | Service | Capability (`IPC_SERVICE_CAP_*`) | Owns |
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

Broker authorization checks the caller's registered service capability — **not** its executable path.
`SYS_RUSTOS_ENTROPY_BROKER` is limited to the Linux-syscall and network-policy
capabilities and returns at most one inline-IPC payload of bytes from the
boot-seeded ChaCha20 substrate. Initialization is one-shot and rejects an
absent/all-zero seed. A private master stream derives every child seed from
master output; consumers never clone the master or form related keys by
combining the boot seed with public PID/TID/counter state. Syscalld still owns
Linux `getrandom` flag, length, and error policy; netd still owns token
collision/admission policy. PID/TID/counter-derived pseudo-random output is
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
