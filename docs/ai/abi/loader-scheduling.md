# Loader and scheduling-context contracts

[Contract index](../contracts-abi.md).

## Process Loader Surface (`loaderd` + `procd`)

- Runtime launches route through `loaderd` (`IPC_SERVICE_LOADERD`), not direct `SYS_RUSTOS_SPAWN_EXEC`.
- `SYS_RUSTOS_PROC_*_BROKER` calls fail with `EACCES` unless caller owns `PROCESS_LOADER`.
- `SYS_RUSTOS_PROC_PREPARE_BROKER` consumes loaderd's exact
  `PROCESS_LOADER` identity without a reverse procd IPC. Loaderd already owns
  ELF/PE image admission; the broker owns only the bounded prepare-handle
  mechanism. The retired procd process-prepare query was a no-op format echo
  and could deadlock as `procd -> loaderd -> procd` when exec raced spawn.
- Loader request ABI v3 requires `requester_pid` to equal the kernel-stamped
  IPC sender. Process broker ABI v3 carries that identity into deferred
  commit: ring0 binds the suspended target PID to the exact requester in a
  bounded registry. ACTIVATE consumes that pair once; foreign callers and
  replays fail, loaderd restart cannot transfer or erase the authority, and
  requester exit revokes the entry and retires the still-suspended target.
- `LoaderActivateBatchRequest` v1 / `LOADER_OP_ACTIVATE_BATCH` carries one
  requester and 1..=8 unique nonzero target PIDs; the unused fixed-array tail
  and every reserved field are zero. Loaderd rebinds the requester to the
  kernel-stamped sender before invoking
  `SYS_RUSTOS_PROC_ACTIVATE_BATCH_BROKER`. Ring0 captures the original Current,
  exact suspended binding Cohort and frame readers with no raw registry guard.
  After full capability/frame preflight, one bounded registry callback validates
  the whole batch and moves its original non-Copy grants out exactly once.
  The registry leaf ends before any runnable effect; those SAME grants and
  original Current/Cohort/frame loans survive the final publication ACK. A
  refusal consumes nothing; requester cleanup wins only before grant transfer.
  No identity/resource/policy leaf spans that callback.
  Task effects precede one group admission Release. A concurrent terminal stop
  wins without revival, including hidden metadata. For a multi-target batch
  admitted into available atomic-cohort priority custody, kernel-ps gives those
  surviving members FIFO first turns before the synchronous loader reply resumes;
  competing priority saturation degrades to ordinary spawn custody, not panic.
  Later single spawns cannot extend that prefix, so reply delay remains ABI
  bounded. Any validation failure before authority consumption changes no member.
  Lost exact binding/suspension or impossible post-effect mismatch is ring0
  corruption; desired terminal stop is not corruption. Concurrent task-affinity
  replacement shares the Identity51 cohort leaf and refuses before masks/epoch,
  returning the existing MissingTask error; there is no new wire/errno enum.
- `SYS_RUSTOS_SPAWN_EXEC` restricted to `rootd` spawning the fixed bootstrap allowlist (`syscalld`, `vfsd`, `loaderd`, `procd`, `initd`). Fails closed for `initd`, generic apps, broad service restarts. rootd may use direct spawn only during fixed bootstrap + `loaderd` recovery; post-bootstrap restarts of other leases must call loaderd.
- Linux `execve` → `procd` (target auth) → `loaderd` (image materialization)
  → `vfsd` (namespace resolution and immutable executable snapshot). Kernel
  process state carries no cwd copy. Procd forwards the raw path plus
  `AT_FDCWD`; loaderd's VFS executable-snapshot ABI v2 names the target PID,
  and vfsd resolves the path against that PID's authoritative cwd before it
  admits bulk I/O. Vfsd returns the canonical absolute path and the terminally
  sealed executable handle in one reply. Loaderd uses that returned path for
  process identity and never combines a cwd string itself. Trusted bootstrap
  and ordinary spawn requests have no live target PID; they use the explicit
  v2 stage-root basis and vfsd resolves their relative package path from `/`.
  A live-target exec cannot select that basis and must use `AT_FDCWD`. If loader
  materialization fails, procd must cancel the exec ticket via
  `SYS_RUSTOS_PROC_CANCEL_EXEC_BROKER` before replying.
- An exec ticket binds one live, non-exiting Linux `(target_pid, target_tid)` pair. Cancel and exec-target validate that exact stored pair before consuming the ticket; a mismatched request must leave it live. The successful exec-target path publishes its register handoff before replacing the target image. Normal/signal process exit, a non-final target-thread exit, and sibling retirement caused by Linux exec remove stale ticket or handoff state.
  Exec detaches retired siblings from the process thread count immediately but
  keeps each scheduler slot quarantined with its exact runtime-cleanup stamp;
  it must never clear and reuse a sibling slot while futex, wait-set, or broker
  records can still name that thread.
- `loaderd` must attempt `ABORT_BROKER` after every rejected commit/exec-target call. Commit is normally terminal, but this closes early rejection paths (such as an already-pending target handoff) before they can retain a bounded prepare slot.
- **Do not move** executable-format, import/export, or DLL namespace policy back into the kernel.

### Process Broker Session

Start: `SYS_RUSTOS_PROC_PREPARE_BROKER` with `PROC_BROKER_ABI_VERSION` + explicit format (`PROC_BROKER_FORMAT_ELF64` or `PROC_BROKER_FORMAT_PE64`). Returned `prepare_handle` is owned by the loader process; supply to `SYS_RUSTOS_PROC_COMMIT_BROKER` or `_ABORT_BROKER`.

Prepare state is owner-bound and bounded. `COMMIT_BROKER` consumes the prepare
handle before its later launch validation, so both successful and rejected
commit attempts are terminal for that handle. Normal loader process exit and
signal-driven process exit must remove every still-uncommitted prepare state;
its pinned mapping metadata must not survive a crashed loader.

Mapping ops use `PROC_BROKER_MAP_{READ,WRITE,EXEC,PRIVATE}` flags and record non-overlapping page-aligned mappings:

- `SYS_RUSTOS_PROC_MAP_ZEROED_BROKER`
- `SYS_RUSTOS_PROC_MAP_DATA_BROKER`
- `SYS_RUSTOS_PROC_MAP_FILE_BROKER` + batch variant `_BATCH_BROKER` — **fd/cap-backed only**. Kernel resolves fd to pinned `KernelHandle` at registration; no path re-open at commit. Backing must be file-kind (`VfsFile`, `RemoteVfs(File)`, `Memfd`); directory/device/socket fd rejected with `EINVAL`/`EACCES`.

Before any mapping broker call, parsed ELF64 and PE64 regions cross the shared
`rustos-image-admission` gate. Every region must remain inside the single
process window, have a non-overflowing nonzero extent, not overlap another
region, and satisfy W^X. A main-image entry must fall inside an executable
region; only a PE DLL may explicitly use entry zero. The same crate parses the
bounded ELF64/PE64 layout bytes and validates PE64 base relocation and import
tables against an isolated image snapshot. Loaderd retains format policy and
exact file reads, while a short read in the process broker aborts rather than
committing a zero-filled file tail. The TLA+ models, source-level tests, and
`fuzz-host --target image-admission` are complementary gates.

**ELF:** loaderd emits `PT_LOAD` mappings for main image + `PT_INTERP` via `MAP_FILE_BROKER`. Static-PIE biases: main = `PROC_BROKER_USER_SPACE_BASE + 0x0040_0000`, interpreter = `+ 0x0200_0000`. Must use `SYS_RUSTOS_PROC_SET_LINUX_RUNTIME_BROKER` for minimal launch metadata (entry, phdr, phnum, phent, brk_start, interpreter_base) — **not** raw blob streaming via `SET_IMAGE_BLOB_BROKER`. Kernel derives launch state from this metadata and the pre-built address space.

The main ELF's admitted header, complete bounded program-header table, and
derived load bias form one immutable loaderd admission snapshot for that open
description. Segment mapping reuses it; it must not repeat the header/table
`pread` and validation cycle after prepare. Interpreter admission remains
independent because it owns a different open description and load bias.

**PE64:** PE validation, section materialization, base relocation, import/export resolution, staged system-DLL registry lookup, PEB/TEB/runtime blob construction all happen in loaderd before commit. The bounded section-header table is fetched with one `pread64`, not one policy/VFS roundtrip per section. PE64 commit includes `SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER` after all `MAP_DATA_BROKER` ops and before `COMMIT_BROKER`. Kernel validates metadata + spawns the materialized address space but **must not** reintroduce PE import/export/system-DLL policy.

A prepared remote file mapping may use a larger internal copy buffer, but each
fallback read from vfsd is capped to the versioned VFS IPC payload.
Bootstrap-backed reads may fill the larger buffer directly. VFS IPC version 5
uses the maximum 64-KiB inline message with a fixed 40-byte response header and
the remaining 65,496 bytes as payload, so one kernel copy window requires at
most two bounded reads. The response layout and exact maximum size are source
assertions; a larger request is chunked instead of becoming an
executable-specific `EOVERFLOW`.

Commit (`COMMIT_BROKER`) builds the child address space from recorded mappings.
When a broker request supplies `console_session = 0`, the kernel inherits the
exact live caller (or target thread) session. Missing process/thread state is
`EINVAL`; it must never manufacture the privileged system session.
One loader request is one synchronous reply-capability transaction. Image
reads and nested policy IPC remain ordinary preemption/block points, but
loaderd must not insert voluntary scheduler yields between mapping,
runtime-metadata publication, and commit while retaining that reply. Such
yields neither release ownership nor improve correctness and can expand a
bounded launch by whole scheduler fairness windows under concurrent System
services. The idle/failed receive loop may still yield because it owns no
caller transaction.
By default, commit-broker spawns do not request immediate deferred reschedule so
`loaderd` can reply to its caller before the spawned child runs startup policy.
Supervisors (`rootd` for initd, `initd` for post-init services, and `runtimed`
for every catalog launch) set `LOADER_SPAWN_FLAG_DEFER_START`. `runtimed`
records ordinary app ownership before `LOADER_OP_ACTIVATE`; endpoint-owning
services also complete lease admission before activation and are then confirmed
through the exact-PID endpoint wait syscall. Activation is single-use, fails
closed for an unknown, exited, already-running, or non-suspended PID, and
publishes one spawn-specific scheduler handoff only after this supervisor
commit point. Each asynchronous runtimed launch has its own record-ACK channel;
late acknowledgement of a timed-out child cannot authorize the next launch.
Every terminal result retains the created PID so exact launch bookkeeping can
be retired without deleting a record owned by another launch.

Rootd supervises replacement core-service endpoint readiness after bootstrap
as well as during it, at the same 250 ms monotonic cadence. Control traffic
cannot consume extra readiness polls. Initd retains its 32-failure budget for
each executable across successful replacements; creation or endpoint publication
alone does not establish sustained health or reset the supervisor-lifetime budget.

Initd batches only siblings whose declared dependency/barrier state is already
satisfied. Pending independent children remain suspended while their exact
rootd leases are reported; before a consumer waits on those children, initd
atomically activates the pending cohort. Netd/devmgrd/inputd therefore publish
as one cohort before their endpoint barrier, and runtimed/storaged may publish
together only after that barrier. Batch membership remains initd policy;
ring0 accepts only the fixed atomic mechanism. The scheduler enqueues every
member in the existing allocation-free spawn FIFO before loaderd replies, so a
later child launch cannot overwrite or jump ahead of an earlier sibling.

`LOADER_SPAWN_FLAG_IMMEDIATE_HANDOFF` remains an ABI option for a child with a
pre-admitted ownership contract; it is not valid for normal supervised service
bootstrap.
Activation handoff uses a spawn-specific scheduler hint that is consumed before
both bounded reply wakeups and generic IPC reply hints, so unrelated traffic
cannot stretch a committed bootstrap. The spawn broker must set this hint
without requesting or taking a reschedule before returning to `loaderd`. Spawn
handoff is an explicit bootstrap transfer and may be consumed by the next
scheduler dispatch, including deferred syscall-exit dispatch; it may bypass the
generic IPC strict-class guard, otherwise a ready System-class task can
indefinitely defer a child-first handoff. A service that lacks a pre-admitted
lease must not use this direct bootstrap handoff. IPC enqueue/reply
completion wakes the receiver/caller and may set a direct pick hint for any
currently ready and
schedulable target; already-runnable service endpoints still need a handoff when
the caller queued work before the service re-entered receive. Synchronous IPC
enqueue only sets the receiver handoff hint; the caller must arm and re-poll its
reply wait before yielding so a fast service reply cannot race a not-yet-armed
waiter. Generic IPC hints are caller-local and the newest eligible receiver
replaces older generic hints, even across scheduling classes; stale high-class
service hints must not block the service that the current caller is waiting on.
A blocked caller's exact receiver handoff remains first because it is the
causal path needed to unblock that caller. Outside that blocked-call case, a
System continuation whose ready age reached the 2 ms recovery bound is
selected before an unrelated generic IPC hint. The hint remains pending for
the following dispatch. This ordering is required because loaderd can be
preempted inside a bounded commit substrate operation while carrying a
reply-scoped System donation; an unbounded stream of UI/network hints must not
strand that continuation inside the kernel.
A generic receiver hint is consumed when the server blocks or the scheduler
next selects work; it must not switch away from a live syscall continuation.

ELF file mappings cross the VFS/loader/process boundary only as one immutable
executable snapshot. Vfsd reads the admitted file once into a bounded private
memfd and applies
`F_SEAL_WRITE|F_SEAL_GROW|F_SEAL_SHRINK|F_SEAL_SEAL` before publishing it.
Relative executable paths are normalized by the vfsd endpoint receive owner
while it exclusively owns PID cwd state; the bulk snapshot worker receives
only the canonical absolute path. The v2 response binds that exact path to the
handle, byte count, and mount generation returned to loaderd. Every open owns
an independent descriptor through terminal reply completion, including failed
replies. A cache hit duplicates under storage ownership and captures that
entry's mount generation; later eviction, remount, or descriptor-number reuse
cannot retarget the pending reply. The current request's byte limit also applies
to hits. Concurrent same-path commits reuse the winning cache entry rather than
leaking its descriptor or double-counting cache bytes. A relative
snapshot with no target PID or a directory basis other than `AT_FDCWD` fails
closed until the ABI carries an authenticated remote directory handle.
Snapshot job ownership remains the bounded IDLE→WRITING→READY→BUSY admission.
After READY, the receive owner advances a process-private futex epoch and wakes
both workers. A worker observes the epoch **before** scanning READY slots and
compare-and-parks only after an empty scan, closing the scan/sleep wake race.
The notification grants no slot or reply authority. Timed waits retain the
previous 1 ms recovery timeout for a refused wake or a wrapped epoch; unsupported
waiter admission falls back to the bounded sleep, never a busy loop.
The kernel-stamped `product-executable-snapshot-sealed` milestone is reserved
for a DVM-volume read. Its checksummed `evidence_v=1` payload contains the
fixed Vfsd service identity, kernel-observed live Vfsd endpoint generation,
storaged block-provider generation, VFS mount generation, nonzero Vfsd
request identity, and SHA-256 of the terminally sealed bytes. Vfsd cannot
supply the service identity or endpoint generation, and a failed kernel stamp
fails the DVM snapshot open closed. A snapshot sourced from the signed early-system image emits only
`product-bootstrap-executable-snapshot-sealed` and cannot satisfy a product
storage-execution scenario.
Loaderd validates every `PT_LOAD` range against that same sealed file and
passes the snapshot plus original file offsets to the process broker; it does
not reopen the pathname or duplicate segment bytes. The snapshot is bounded to
128 MiB, and the process broker rejects live VFS handles and partially sealed
memfds. Commit may allocate and copy page-table backing, but it must not issue
nested vfsd/storage IPC or re-read a mutable path after validation. This closes
the validation/commit content race, removes redundant storage reads, and keeps
service-owned storage policy out of ring0.

Every syscall captures the entering user's complete SIMD/FPU image in a
syscall-scoped snapshot distinct from the scheduler's per-task SIMD slot. If a
blocking syscall yields, the task slot is allowed to hold the suspended kernel
continuation's SIMD scratch state; syscall return must restore only the
entering-user snapshot. Reusing the scheduler slot for both lifetimes violates
the syscall register-preservation ABI and can corrupt userspace request
structures assembled with XMM/YMM registers after the wait.

The separate local-socket completion path accepts a netd latency flag only
from the currently registered netd endpoint, only for the exact versioned
response length, and only for successful local data/event transfer or the one
armed nonblocking-receive drain. It queues only ready User tasks in a bounded,
deduplicated FIFO and requests a user-return reschedule edge. Unknown flags,
foreign endpoints, System targets, stale tasks, and malformed replies cannot
create scheduler authority. The 100 us PIT edge is an upper bound if the
normal user-return scheduler check is missed; no handoff preempts an arbitrary
kernel frame.

## Scheduling-context accounting snapshot

Rootd owns scheduling policy in its immutable bootstrap/post-init manifests.
Each launch grant is sealed to the live rootd endpoint epoch, exact requester
identity, executable path, and complete policy, then consumed once. The direct
pre-loaderd spawn path obeys the same rule; rootd itself is the sole explicit
boot exception and receives a fixed 4 ms/10 ms context before publication.
Production user-slot publication fails closed without an admitted context.
Deadline-critical domains additionally pass a conservative per-CPU utilization
test capped at 90%; Linux or Windows priority requests cannot widen this
temporal authority.

A wake candidate is not the final request consumer. If another same-process
worker actually dequeues a queued request, its receive transaction moves the
SAME retained reply funding and original trusted priority to that worker,
withdraws the old candidate's charge/boost, and leaves late sender hints
idempotent. It does not sample a replacement native context or widen policy.
The native witness uses actual runtime enqueue/dequeue plus original physical
participant/ledger loans; `IpcPriorityInheritance.RebindWorker` supplies the
corresponding abstract transition. This follows the caller-budget lifetime
principle of [seL4 MCS passive servers](https://docs.sel4.systems/Tutorials/mcs.html#passive-servers),
not seL4's specific scheduler policy or ABI.

A terminal synchronous reply may outlive its original scheduling-context
owner, including a nested passive-server reply after the root caller retires.
Slow and inline completion therefore accept either that exact original context
or an already-retired owner, just as cancellation does. A live owner's foreign
context still fails closed. Settlement never substitutes another context or
grants execution: the subsequent wake must match the immediate caller's exact
original typed reply wait, and an absent/retired caller is not revived.

`SYS_RUSTOS_SCHEDULING_CONTEXT_SNAPSHOT` is a read-only, versioned diagnostic
ABI (version 2). It copies the calling thread's executing identity and the exact effective
scheduling-context owner, object identity, admitted budget/period, bounded
refill state, exhaustion counters, and shared-domain ledger. During a nested
passive-server call, the executing task remains the server but the reported
context owner and both ledgers are the root caller's. A domain's
`budget/period` is a per-CPU share, matching deadline admission's per-CPU
utilization ceiling: the domain keeps one budget bank per logical CPU, and an
interval is admitted and charged against the bank of the CPU that runs it. The
snapshot reports the executing CPU's domain bank. A refill is dated one period
after the consumption it returns began, and contiguous consumption merges into
one refill that keeps the run's start; consumption after a gap merges at the
later point. Ordinary applications are granted budget equal to period, so a
continuous run is refilled exactly when it would exhaust and never idles a CPU
that has nothing else to run (System services still win by class). The syscall returns
`EAGAIN` when no admitted effective context exists and never grants mutation,
donation, endpoint, or scheduling authority. Context and domain runtime
milestones use distinct counter banks so domain billing is not double-counted
as client consumption. On exhaustion the same snapshot exposes the cumulative
consumed time, original budget/period, timeout endpoint token, and exact latest
reply identity. A missing or unresolved endpoint selects one bounded throttle
until the already-scheduled refill; the kernel records that terminal action
once and never retries handler delivery in the accounting path.

`ipcbench` carries two KVM-only public-ABI acceptance probes for this contract.
`scheduling_budget_exhaust_refill` runs one continuously runnable User context
across four admitted periods and emits a result only when the same object and
domain show consumption, exhaustion, refill, and the bounded missing-handler
timeout action. `ipc_nested_passive_server` sends one caller through two
receive-blocked server threads and emits a result only when both executing
server identities observe the root caller's exact context object/domain and
the caller regains that identity after both replies. An isolated `xtask bench`
run fails if its exact primary result is missing, duplicated, or skipped.
System-wide IPC phase totals are displayed but are not divided into these
semantic probes; their acceptance is the kernel-stamped snapshot invariant.

