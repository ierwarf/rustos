# Job Model and Hierarchical Resource Accounting

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

**Status (2026-10-31 source snapshot):** Phase 1 jobs, phase 2a's chunked
process directory, and phase 2b's process/thread limits and resident user-page
charging are implemented. Application-job placement and a JobPolicy v1/256-bit
ceiling implementation also exist; their opt-in runtime evidence does **not**
complete policy migration or commercial stability acceptance. Kernel-memory
charging (`kmem.max_bytes`), `memory.high/min`, client-funded sessions, and the
later component/personality redesign remain open. Read the **as-built** sections
below for implementation details; dated tests are not current acceptance.
The original watermark/OOM baseline is in the
[address-space contract](../ai/address-space-lifecycle-contract.md#4-memory-pressure-watermark-and-oom-policy).
The parent decision record is [Ring3 Process Model](ring3-process-model.md).

Every claim below is tagged **(verified)** when it was read from this tree,
**(proposed)** when it is a design decision, or **(recalled)** when it comes from
background knowledge of another system that was not re-read for this document.

### 1. Why jobs — current baseline

The original fixed-process-table audit predates phases 2a/2b. Current limits
and owners must not be confused with that historical design input:

| Resource | Current shape | Source |
| --- | --- | --- |
| Process slots | Default admission 32; chunked storage up to 4,096 slots (internal ceiling), with a runtime/root-job policy limit | [process_table.rs](../../kernel/ps/src/multitask/process_table.rs), [slot_dir.rs](../../kernel/ps/src/multitask/slot_dir.rs) |
| Threads | Job-level counted limit; per-process structural ceiling still 32 | [process_table.rs](../../kernel/ps/src/multitask/process_table.rs) |
| Jobs | Generation-bound tree, 64 internal slots | [jobs.rs](../../kernel/ps/src/multitask/process_table/jobs.rs) |
| Resident user memory | Per-job hierarchical page charging and admission | [Phase 2b-ii](#phase-2b-ii-memory-limit-as-built) |
| Service identity | Static numeric IDs and legacy capability bits remain | [Ring3 process model](ring3-process-model.md) |

Job lifetime and accounting protect privileged service resources from
application workloads. The remaining work is kernel-memory charging,
`memory.high/min`, capability cutover, and client-funded service usage.
Proposed sections below are target designs, not implementation claims.

### 2. Invariants

These are fixed up front so later phases build on the final shape.

1. **One job per process, for life.** A process is created into a job and never
   moves. Exec and fork keep it. This makes every charge attributable to exactly
   one ancestor chain.
2. **A tree.** Every job except the root has one parent. The kernel creates the
   root job at boot and gives its only handle to `rootd`.
3. **Limits are policy, not constants.** No ABI-visible count is a `const`. The
   root job's limits are the system's ceilings and come from boot configuration.
4. **Top-down.** A child's effective limit is `min(own, parent effective)`, and
   its effective policy is `parent effective ∩ own`. A child can never be more
   permissive than its parent.
5. **Authority flows down through handles.** A job handle confers rights over
   that job's subtree and never over its parent or siblings.
6. **The kernel knows jobs, not components.** Names, realms and capability
   routing belong to the component manager in `rootd`. The kernel cannot learn
   what a "service" is.
7. **Lock-free on the fault path.** Charging must work with interrupts off, with
   no allocation and no sleeping lock, because the anonymous-fault gate runs that
   way **(verified**, see `serve_anonymous_first_touch`**)**.
8. **Group operations are resumable.** Killing a job of any size is a bounded
   step repeated by a kernel task, never one long IRQ-off section.

### 3. Objects

**Job** *(proposed)*

```text
Job {
  id            (slot: u32, generation: u32)     // generational, like ProcessHandle
  parent        JobRef                           // none only for the root
  depth         u8                               // bounded, default max 8
  state         Live | Killing | Dead
  limits        JobLimits                        // own, as configured
  effective     JobLimits                        // min(own, parent.effective), cached
  policy        JobPolicy                        // own, as configured
  effective_policy  JobPolicy                    // parent ∩ own, cached with an epoch
  usage         JobUsage                         // atomics, see section 6
  events        JobEvents                        // monotonic counters, see section 8
  live_children, live_processes
  critical      Option<ProcessRef>
}
```

The job table is a generational slab like the process table
**(verified** pattern: `ProcessHandle` carries a generation, and a recycled slot
cannot alias a stale handle**)**. Its capacity is a limit of the root job
(`jobs.max`), not a constant.

**Process → job.** `ProcessObject` gains a `job: JobRef`, set when the process
is reserved (`reserve_spawn`) from a job handle that holds `CREATE_PROCESS`.
The frame-descriptor ledger's root record also carries the `JobId` so a data-leaf
claim can find its charge target without taking the process table lock
**(proposed)**. Today the root record keeps a per-root data-leaf count that the
memory census already reads **(verified**, `data_leaf_count`**)**; the job simply
makes that count hierarchical.

**Moniker.** The kernel stores no path. `rootd` keeps `moniker → JobId`.

### 4. Handles and rights

`HandleRights` is already a typed enum with typed attenuation
**(verified**, [handle.rs:189](../../kernel/object/src/handle.rs) and `can_attenuate_to`**)**,
so jobs add one variant:

```text
HandleRights::Job(JobHandleRights)

JobHandleRights = INSPECT        read limits, effective limits, usage, events
                | WAIT           observe job signals
                | CREATE_JOB     create a child job
                | CREATE_PROCESS create a process in this job
                | SET_LIMITS     change limits of this job or a descendant
                | SET_POLICY     tighten policy of this job or a descendant
                | KILL           kill this job's subtree
                | SET_CRITICAL   mark a member process critical
                | DUPLICATE | TRANSFER
```

Rights only attenuate on duplicate. A process holds no handle to its own job
unless it was given one, so a confined process cannot raise its own limits.
Setting a child limit larger than the parent's effective limit is accepted and
clamped, and the clamp is reported in `GET_INFO`. Refusing it instead would make
the order of operations matter when a parent is shrunk.

### 5. Limits

Every limit is `u64` with `u64::MAX` meaning "inherit". Charging is to the job
of the creating process, and propagates to every ancestor.

| Limit | Charged for | Enforced at | On exceed | Replaces |
| --- | --- | --- | --- | --- |
| `memory.max_pages` | data leaves and page-table frames of member processes | frame claim in the descriptor ledger | refuse the allocation; same `Refused` / `ENOMEM` path as the watermark; count `memory_max` | no equivalent |
| `memory.high_pages` | same | same | event only in v1 (no throttling) | no equivalent |
| `memory.min_pages` | a guarantee, not a charge | admission when set: `Σ children.min ≤ parent.effective.min` | refuse `SET_LIMITS` | the watchdog's service-endpoint heuristic |
| `kmem.max_bytes` | kernel objects made for the job (process objects, VMA tables, handle tables) | the allocation site | `ENOMEM` | unbounded ring0 `Vec`s |
| `jobs.max`, `processes.max`, `threads.max` | live objects | create | `EAGAIN` | 32, 32 |
| `handles.max`, `endpoints.max`, `ipc_objects.max` | live objects | create | `EMFILE` / `ENOSPC` | 32/8, 16, 1 024 |
| `sched.domain` | a reference to an existing scheduling domain | admission | inherit the parent's | `MAX_SCHEDULING_DOMAINS` call sites |

CPU stays in the existing scheduling-domain admission, which already enforces a
utilization quota **(verified**, `domain_admission.rs`**)**. A job names a domain;
it does not reimplement one.

### 6. Charging mechanics

The data-leaf claim runs once per page fault, so a shared atomic per job would
bounce one cache line across every CPU. The design follows the batching that
Linux's memory controller uses *(recalled, not re-read)*:

- Each CPU keeps a small **stock** `(job, pages)` for the job it last charged.
- A charge first takes from the local stock, which is a CPU-local, interrupt-safe
  operation with no atomics.
- When the stock is empty, the CPU takes a batch (default 64 pages) from the job
  with a CAS that walks the ancestor chain, which is at most `depth` steps and
  needs no allocation or lock.
- A refund goes back to the stock and drains to the job when the stock exceeds
  two batches.

Consequences, stated rather than hidden:

- The limit is **fuzzy by at most `batch × cpus`** while far from the limit.
- Within `batch × cpus` of the limit the charge switches to **precise mode**: the
  stock is bypassed and each page is a CAS. Overshoot there is zero.
- A census, a kill, and `GET_INFO` drain stocks first, so reported usage is exact
  at the moment it is read.
- A process exit refunds through the same path, after the retirement walk that
  already runs without allocating.

### 7. Policy

`JobPolicy` is qualitative: what a member process may ask the kernel to do. It
is a fixed-width bitmap sized to the number of broker operations (256 bits in
v1), not one bit per service.

- **Effective policy** is `parent effective ∩ own`, evaluated live, so tightening
  a policy takes effect on the next check.
- A check is one load of the process's cached job pointer and a bit test, so the
  syscall path pays no lookup.
- `IPC_SERVICE_CAP_*` bits become *roles* in this bitmap. The check
  `current_process_has_service_capability(CAP)` becomes
  `job.effective_policy.allows(Broker::X)`. The service registry keeps mapping
  names to endpoints and stops being an authority source.
- During migration both checks run and must agree; then the old bits are removed.

### 8. Lifecycle, events, and criticality

States: `Live → Killing → Dead`.

- **Kill** is idempotent. It marks the job and every descendant `Killing`, which
  refuses new processes, new jobs and new charges, then terminates members with
  the existing group exit and the existing finalizer
  `finalize_terminated_process` **(verified**, shared by `rootd`'s terminate call
  and the watchdog**)**. A kernel task repeats a bounded step until the subtree is
  empty, so a large job never holds interrupts off.
- A job becomes `Dead` when it has no live process, no live child, and no
  outstanding kill step. Its slot is recycled only after the generation changes.
- **Critical.** `SET_CRITICAL` marks one member. If it terminates, the job
  becomes `Killing` and its parent observes `CRITICAL_EXIT`. Core services use
  this instead of `fail_closed` spin loops. Zircon exposes the same idea as
  `zx_job_set_critical` *(verified that the call exists; its exact semantics were
  not read)*.
- **Events** are monotonic counters plus waitable signals, per job and
  hierarchical for aggregates:
  `PROCESS_EXIT`, `JOB_EMPTY`, `CRITICAL_EXIT`, `KILLED`, and for each limit
  `hit[dim]`, plus `denials` and `oom_kills`. This mirrors Linux's
  `memory.events` fields `low`, `high`, `max`, `oom`, `oom_kill` **(verified from
  the cgroup v2 documentation)**.

### 9. The OOM policy at job granularity

Nothing built so far is thrown away; each piece changes unit.

| Today (per process) | With jobs |
| --- | --- |
| census lists the largest processes | census lists jobs by hierarchical `memory` usage |
| `rootd` governor picks the largest process outside every lease | picks the leaf job with the largest usage, skipping jobs that are `critical` or whose usage is within their `memory.min` |
| kill one pid | `KILL` on the job; an application job has `oom_group` semantics, so its whole subtree dies together |
| watchdog ends the last process refused a frame, unless it owns a service endpoint | ends the **job** of the last refused process unless the job is `critical` or covered by `memory.min`; the endpoint heuristic goes away |
| global watermark only | global watermark **and** `memory.max`; a job hitting `max` raises its own event to the supervisor of that job first |

Linux's `memory.oom.group` and `cgroup.kill` are the precedent for killing a
group atomically **(verified)**. Android moved its killer to a userspace daemon
that reads kernel pressure signals because the older vmpressure signal produced
false wakeups **(verified from the lmkd documentation)**, which is the same split
as `rootd` policy over ring0 measurement here.

### 10. Process-table scalability — phase 2a implemented

The former `MAX_PROCESS_OBJECTS`-sized arrays are now chunked, non-moving
directories; see [Phase 2a as built](#phase-2a-as-built). The admitted limit
defaults to 32, but the storage ceiling is 4,096. Per-process VMA capacity
remains bounded, with its tables and writer locks in chunked directories.

Do not treat the earlier two-level directory proposal as unimplemented.
Remaining work concerns accounting, capacity qualification and resource policy,
not another process-table rewrite.

### 11. ABI

One multiplexed syscall with a versioned argument struct, following the pattern
of the other broker calls *(proposed)*. The next free number after
`SYS_RUSTOS_ROOTD_MEMORY_BROKER` (`0x5255_0056`) is `0x5255_0057`.

```text
SYS_RUSTOS_JOB(args: *mut JobArgs)
  JobArgs { abi_version: u16, op: u16, flags: u32, job_handle: u64, ... }
  ops: CREATE_CHILD, GET_INFO, SET_LIMITS, SET_POLICY, KILL,
       SET_CRITICAL, WAIT_EVENTS, DUPLICATE_HANDLE
Process creation: the prepare step of the process broker takes a job handle.
  A legacy caller that passes none creates into its own job.
```

When the IDL from the [Ring3 Process Model](ring3-process-model.md) exists, this
struct is its first generated type; until then it is a `#[repr(C)]` struct with
`abi_version`, like the existing broker arguments.

### 12. Migration

Each phase ends in the final shape of what it touches.

| Phase | Change | Done when |
| --- | --- | --- |
| 1 | `Job`, the root job, process↔job binding, `GET_INFO`, `KILL`. No limits. All existing processes live in the root job. | census and kill work by job; existing KVM gates unchanged |
| 2a | Process-table directory (section 10) | 32 is a root-job policy value; the pager VMA and identity models re-verified |
| 2b | Counted `processes.max`/`threads.max` and resident `memory.pages` implemented; `kmem.max_bytes` and `memory.high/min` open | verify native and opt-in runtime refusal, not only the ABI |
| 3 | JobPolicy v1 ceiling exists (3a); legacy `IPC_SERVICE_CAP_*` authorization remains | prove exact revoke/fork/cutover behavior before deleting bits |
| 4 | `rootd` becomes the component manager: manifests, realm ↔ job, capability routing, supervision trees | `service_manifest.rs`'s static arrays are gone |
| 5 | Session jobs funded by clients (see below) | a flooding client exhausts only its own quota on `vfsd`/`netd` |

**Session quotas (phase 5).** Genode makes a server's allocation on behalf of a
client come out of a budget the client donates **(verified)**, so a misbehaving
client exhausts only what it paid. The job-model equivalent needs a **charge
context** separate from the allocating process: a server allocates "to" a
session job that lives under the client's subtree. Linux has a comparable remote
charge for kernel allocations *(recalled)*. This is why invariant 1 is "one job
per process" and not "one job per allocation": the owner stays simple and the
exception is an explicit, scoped charge target.

### Phase 1 as built

Implemented in `kernel/ps/src/multitask/process_table/jobs.rs`, the syscall edge
`kernel/compat/src/user/syscall/linux/job_ops.rs`
(`SYS_RUSTOS_JOB`, `0x5255_0057`, ABI v2), and the `rootd` client
`services/rootd/src/jobs.rs`. Model: `job-lifecycle`.

What exists: a root job at boot; `CREATE_CHILD`, `GET_INFO`, `KILL` and
`PROCESS_JOB`; a `JobId` of `(slot, generation)` that a recycled slot cannot
alias; every process created into its parent's job or, through the kernel entry
`reserve_process_spawn_in`, an explicit live job; a `Killing` job refusing new
members and children across its whole subtree; the memory census carrying each
process's job; and a kill that ends members with the same single-finalizer
termination the watchdog uses, at most 128 per call, reporting `remaining`.

Decisions and departures from the sections above, stated so they are not read as
the final form:

- **Authority is now a typed Job descriptor, not root-supervisor capability.**
  The executive explicitly grants rootd fd 3 (`JOB_BOOTSTRAP_ROOT_FD`) before
  its first publication; ordinary/admin/path-matching images get no grant.
  Every job syscall resolves `job_handle`, its Job type, operation-specific
  rights and live generation. `PROCESS_JOB` returns identity only within the
  inspected subtree (out-of-scope/absent PIDs both give `ESRCH`). ABI v2 returns
  separate `job`/`parent_job` identities and `result_handle` authority. Duplication
  requires `DUPLICATE` and subset rights; a raw Job object defaults to empty
  rights, and transfer/fork preserve exact identity and rights. Fork inherits
  a table rather than making an explicit duplicate request. New descriptors
  remain reserved until copyout succeeds; failure cancels the exact reservation
  and kills an unpublished empty child. Source/native tests and the bounded
  `job-handle-publication` model witness this mechanism. One 1-vCPU KVM attempt
  reached `rootd: job tree ok` and `ipcbench: end`, but ordinary smoke acceptance
  failed on a uiserver idle watchdog / missing wayclick frame. Neither diagnostic
  TLB/IRQ milestone appeared; this is not a matched-cohort regression claim.
  A subsequently fixed overlapping-fd-reservation defect has native red/green
  coverage and passed independent review. Two post-fix 1-vCPU attempts also
  passed the Job self-check but missed full 20s UI readiness (one also missed
  ipcbench end); no TLB/IRQ diagnostic milestone appeared. A later quiet
  1-vCPU attempt again missed wayclick readiness. The same authority/fix
  snapshot subsequently passed full PR (45 artifacts) and one sealed 16-vCPU
  smoke including the Job self-check and ipcbench end. This is functional SMP
  evidence, not a failure-rate comparison or an explanation of the UP failures.
- **Membership is derived, not counted.** Live-process counts are computed by
  scanning the 32 process slots under the process-table lock, so no counter can
  disagree with the processes. This is a phase-1 convenience; the directory in
  phase 2a replaces the scan with an indexed structure.
- **The job slab holds 64 jobs and depth 8.** These are internal bounds, not ABI
  values. They become `jobs.max` and a depth limit of the root job's policy in
  phase 2a.
- **A job slot is recycled only when no process object, exiting or not, names
  it.** An exiting process that has not been reaped still carries its job index,
  so releasing earlier would let a new job inherit its members. A killed job with
  no member and no child is released inside the kill call; otherwise on the next
  job operation after the last member is reaped.
- **A caller inside the target subtree is refused (`EPERM`).** A supervisor
  cannot kill itself, and the root job cannot be killed at all.
- **Broker placement is being connected; rootd placement remains open.** The
  process broker's ABI v5 prepare now accepts an explicitly flagged, loader-local
  Job descriptor with `CREATE_PROCESS`. Loaderd's ordinary and fused receives
  retain kernel-stamped sender identity alongside descriptor transfer, and close
  received authority after prepare has captured its generation. Commit reserves
  in that job, or the authenticated requester's job when absent (not loaderd's).
  Exec rejects placement. The broker/loader seam passed native checks, ten
  implementation mutants, five spec mutants, its 97-state bounded model, the
  full now lane and signed build. Rootd now retains child-job authority for
  loader-launched leases (initial initd and loader-backed restarts), lends only
  `CREATE_PROCESS|TRANSFER`, verifies the returned process's job, and retains
  supervisor custody until a failed/lost attempt or exiting lease is empty.
  The newer rootd slice passed 49 host tests, eight implementation mutants,
  signed build and the expanded 277-state model with seven spec mutants. Its
  witness-routing host-test omission was fixed; refreshed now checks are pending.
  One unsealed 1-vCPU smoke reached `rootd: loader job placement ok`, but failed
  full 20s ipcbench/compositor/wayclick readiness without either TLB/IRQ diagnostic
  milestone. This is limited placement evidence, not full smoke acceptance or a
  matched-cohort regression claim. Boot-direct service placement and job-level
  OOM policy remain open; the earlier `rootd: job tree ok` alone is not evidence
  of service placement. The subsequent boot-direct source slice now uses a
  versioned 40-byte `RustosBootstrapSpawnArgs` at `SYS_RUSTOS_SPAWN_EXEC`, with a
  required typed CREATE_PROCESS Job descriptor, forwarded through ConsoleProgramSpec
  to reservation. Rootd allocates, retains and verifies a child job for these
  services too. Native compat 226, feature-unified ABI 69 and rootd host-test 51
  passed. Its full now lane (768 selected native tests), signed build, ten
  implementation mutants and additional rights spec mutant also passed. A new
  unsealed 1-vCPU smoke reached five direct-placement markers and the loader
  placement marker, but missed full 20s ipcbench/wayclick readiness without either
  TLB/IRQ diagnostic milestone. These limited functional markers do not establish
  full smoke acceptance or a regression rate. Work stopped at the operator's
  commit checkpoint.
- **`rootd`'s memory governor now prefers a whole job (2026-10-29).** It groups
  the census by job, sums the eligible members' resident pages, and probes jobs
  largest first. A job is killable only if no protected pid (rootd, every core
  and post-init lease, every reporter) lies anywhere in its subtree. The kernel
  answers that per pid through `JOB_OP_OPEN_SUBTREE` (a rights-attenuated handle
  to a job named by id, granted only inside the authorizing handle's subtree)
  followed by `PROCESS_JOB`, which is `ESRCH` for a pid outside the subtree. So
  ancestors, descendants and siblings need no tree walk in `rootd`, and any
  other answer, including an error, refuses. With no killable job the verdict
  is the single largest eligible process, exactly as before. A job kill also
  counts as a rootd termination for the ring0 watchdog, which therefore defers.
  Limits: the census lists the 32 largest processes, so a job's sum is a lower
  bound; the ring0 watchdog still ends a single process, not a job, because the
  lease set that protects services lives in `rootd`; and no unprotected
  application job exists at boot yet (applications inherit `initd`'s protected
  job), so the job path is covered by unit tests, source guards and mutants but
  has not run in a live OOM episode.

Initial verification: the `job-lifecycle` TLA+ model (two spec mutants killed),
seven kernel-ps unit tests, four compat tests, three implementation mutants
killed, and a 1-vCPU KVM boot that reached the self-check milestone.

Follow-up verification: the model now separates explicit-job reservation from
publication and checks the original generation again after a possible kill and
slot reuse (5,994 distinct states). Nine native job tests include publication
refusal after reservation and kill/reuse, exact cancellation after refusal, and
actual last-thread detach/reap followed by job-slot reuse. Two new implementation
mutants and the new stale-reservation spec mutant were killed; both original
spec mutants were re-killed. Two registered Loom and two Shuttle witnesses cover
reservation versus kill/publication and exiting-object reap versus reuse. They
are bounded projections using one model mutex, not executions of the production
spinlock, teardown, or the whole job slab. The whole Loom and Shuttle lanes
passed. The reservation/concurrency slice alone changed no job authority or spawn ABI.
The subsequent typed-handle/ABI-v2 slice described above is still being validated;
none of this claims that service placement, job-level OOM policy or all remaining
phase-1 departures are closed.

### Phase 2a as built

Implemented in `kernel/ps/src/multitask/slot_dir.rs`, `process_table.rs`,
`process_table/spawn.rs` and `kernel/ps/src/user/handles.rs`. Model:
`slot-directory-growth`.

What changed: the 32-slot process table and the six lock-free arrays sized by
it (identity x3, pin cells, rewrite epochs, placement cells, VMA tables and
writer locks) are chunked directories. A chunk is 32 slots, is allocated in
normal context, published once, and **never freed or moved**, so a reader with
interrupts off still dereferences stable storage. `grow_slots` publishes every
side array's chunk first and installs the table chunk last, without allocating
under the table lock; `reserve_spawn_in` grows on demand while a free slot
below the limit is missing. The reaper now works in batches of 32 under one lock
hold instead of keeping two process-count-sized arrays on the stack. The console
open-description registry, a second hidden cap of 256 entries that panicked when
full, grows the same way (128-entry chunks).

Decisions and limits, so they are not read as the final form:

- **The limit is a runtime variable, not yet job policy.** `process_limit()`
  defaults to 32 (`DEFAULT_PROCESS_LIMIT`), `set_process_limit` clamps to the
  directory ceiling of 4096 slots, and lowering it never evicts a live process.
  Nothing sets it yet: wiring it to the root job's `processes.max` needs
  `SET_LIMITS`, which is phase 2b. Boot behaviour is therefore unchanged.
- **Other constants remain.** Threads per process (32, which also sizes each
  placement cell), the job slab (64 jobs, depth 8), service endpoints (16), and
  the other fixed tables listed in section 1 are untouched.
- **Chunks are leaked on purpose.** A process slot index is stable for the life
  of the system; memory is only ever added.
- **The ceiling is an implementation bound**, 128 chunks of 32 slots, kept so
  the directory itself is a fixed array that needs no allocation to read.

Verification: 1022 kernel-ps unit tests (new: four directory tests, five growth,
limit and batched-reap tests), `slot-directory-growth` TLA+ with two spec
mutants killed, four implementation mutants killed, and a Loom and Shuttle
witness for chunk publication and for side-storage-before-slot (the Loom
ordering witness was shown to fail when its release/acquire pairing is
removed). Those witnesses are protocol projections; they do not execute the
production allocator or table lock. A boot with more than 32 processes has not
been run; chunk 0 is grown at first process creation in every boot, chunks past
it only in unit tests.

Live evidence on the sealed tree (16 vCPU, `--expect "ipcbench: end"`,
`--timeout 20`, no matched control arm): the OOM hog e2e passed, and 47 of 52
boots passed (5 failures: two missing the gpu-scene compiler, one compositor and
wayclick, two `ipcbench: end` not reached in 20 s; no panic and neither
diagnostic milestone). The earlier unmatched baseline was 48/50; 5/52 against
2/50 failures is not distinguishable at these sample sizes (Fisher p ~0.4), so
this is neither a regression finding nor a clean bill. `--proc-stress` did not
print any `procstress:` line in two attempts (16 vCPU and 1 vCPU); whether that
mode ever worked end to end was not established, so it gives no evidence either
way.

### Phase 2b (counted limits) as built

Implemented in `process_table/jobs.rs`, `process_table/spawn.rs`,
`process_table.rs` (`attach_task`), `kernel/compat/src/user/syscall/linux/job_ops.rs` and
`services/rootd/src/jobs.rs`; ABI `SYS_RUSTOS_JOB` v3, op `SET_LIMITS` (7).
Model: `job-limits`.

What exists: a job carries an own `processes` and `threads` limit (unset =
inherit); the effective limit is the tightest on the chain to the root.
Admission of a process (at reservation for an explicit job, authoritatively at
publication) and of a thread (`attach_task`) asks every job on the chain whether
its subtree count plus one fits that job. Counts are the unreaped process
objects and their threads, derived from the objects under the process-table
lock, so no counter can drift. A larger child limit is accepted and clamped in
the effective value; lowering a limit evicts nobody and only stops admission.
`GET_INFO` reports own limits, effective limits and usage, and `SET_LIMITS`
returns the limits now in force. The root job's process limit **is**
`process_limit()`, so the 2a runtime variable is now the root job's policy
value, reachable through a handle that holds `SET_LIMITS`.

**Not done, and deliberately not faked:**

- **`memory.max`, `memory.high`, `memory.min` and `kmem.max_bytes`.** Charging
  memory exactly needs a hook at each site in the frame-descriptor ledger that
  changes a root's data-leaf count (exclusive publish, shared alias, release,
  copy-on-write break, root unregister), carrying a charge group from the root
  record to the job. That is a change inside a critical, formally modelled path
  of `kernel/mm`, so it is its own slice. `SET_LIMITS` rejects the memory mask
  bit instead of storing a limit nothing enforces.
- **Per-CPU stock charging.** Counts here are exact and derived under the
  table lock, which is right for process and thread admission (rare events). The
  stock-and-batch design of section 6 is for the page-fault path and arrives
  with memory.
- **Policy.** `rootd` sets no limit at boot; the defaults (root processes 32,
  everything else unset) keep boot behaviour unchanged. The boot self-check
  exercises set, read-back, clamp reporting and the missing-right refusal.
- **`handles.max`, `endpoints.max`, `ipc_objects.max`** and the 32-threads-per-
  process structural cap are untouched.
- A scan of the process slots per admission is O(slots x depth); fine at tens
  of processes, to be indexed when the table is large.

Verification: 1031 kernel-ps tests (eight new job-limit tests plus a fork
inheritance test), compat job_ops 12, rootd 59; TLA+ `job-limits` (4096 states)
with two spec mutants killed; five implementation mutants killed (one first
survived because an explicit-job pre-check hid the publication check; the fork
inheritance test now kills it). The 2a "reserve ignores the limit" mutant was
removed: once admission counts processes it is equivalent. No concurrency
witness was added because every check runs under the existing table lock.

### Phase 2b-ii (memory limit) as built

Implemented in `kernel/mm/src/memory/charge.rs`, the frame-descriptor ledger
(`phys/frame_descriptor_ledger.rs`), the admission calls in
`address_space/pager_fault_mapping.rs` and `address_space.rs`, the binding in
`kernel/ps` (`process_table/spawn.rs` at publication, `process_table.rs` at exec
commit), job ABI v4 (`JOB_LIMIT_MEMORY_PAGES`) and `rootd`. Model:
`memory-charge-conservation`.

How it works: each job owns a **charge group** of the same index (64, asserted
equal to the job slab). A process's address-space root is bound to its job's
group once, in the root's otherwise unused `next_or_virtual_address` word. The
ledger charges the group exactly where it raises the root's data-leaf count (the
exclusive, shared-alias and private-file publish sites) and uncharges where it
lowers it (`decrement_root_data_count_with`), and the charge propagates to every
ancestor, so a group's usage equals the pages held at or below it by
construction. Binding a root that already holds pages (a new image, a forked
child, an exec's new image) charges what it holds, forcibly. `memory_pages` is the
group's own limit; the effective limit is the tightest on the chain.

**Enforcement.** Before a root claims a page the caller asks `root_admits`, and a
refusal is `AddressSpaceError::OutOfFrames`, the same error a watermark denial
returns: the first-touch gate returns its unpublished frame and reports a
refused fault, and fault-around stops at the limit. The faulting thread then
takes an unhandled page fault; there is no policy kill from the limit.
Admission is advisory (checked before the claim, charged at publication), so a
limit can be exceeded by pages in flight on other CPUs and by the forced charge
at binding; usage never drifts from the ledger counts. The model carries that
as an explicit slack that grows only by concurrent admissions and binding.

**Not done:**

- **Page-table frames and kernel objects are not charged**, so `memory.max`
  bounds resident user data pages only. `kmem.max_bytes` is not implemented.
- **`memory.high` and `memory.min`**, and the job-level OOM policy that would use
  `memory.min` as protection, are not implemented.
- **Application jobs do not exist yet**, so no application has run into a job
  memory limit. The live evidence instead comes from `rootd` boot self-checks.
  `rootd: job memory charge ok` runs on every boot (touching 32 fresh pages
  raised the root job's usage by at least half and unmapping returned it).
  `rootd: job memory limit ok peak/limit:` is a **test mode**, built with
  `RUSTOS_ROOTD_LIMIT_PROBE=1 cargo xtask build`, not a default: run at every
  boot it perturbed SMP start-up (a CPU's first-reschedule marker went missing
  in 4 of 60 boots, against none in about 130 boots without it). When enabled,
  `rootd` starts an unprivileged copy
  of its own image into a fresh job limited to 12 288 pages (48 MiB), the copy
  maps 128 MiB and touches pages until it is refused, and the parent checks
  that the copy died, that the job's peak charge stayed within 256 pages above
  and 512 below the limit, and that the job drained to zero and its slot was
  released). The kernel admits a copy of `services/rootd/rootd.elf` for direct
  spawn only without logical admin. A first attempt with a 2 048-page limit
  failed the probe with peak 6 436 because the child image itself is charged at
  publication and is not refused (its 8 MiB static heap alone is 2 048 pages);
  that is the documented forced binding charge, and the limit was raised above
  it.
- Shared copy-on-write pages are charged to every mapping root (per mapping, not
  per frame), as the model states.
- A job slot is not reused while pages are still charged to it (a retiring
  address space can outlive its process object), so a dead job can linger until
  its last page is freed.

Verification: kernel-mm 103 tests (six charge, seven ledger), kernel-ps job
tests including limit hierarchy, recycled groups and the release gate; the TLA+
`memory-charge-conservation` model (85 231 states) with two spec mutants killed;
ten implementation mutants killed.

### Application placement (step 0a as built; limit proof still open)

`services/rootd/src/loader_realm.rs` retains a separate root-child application
realm for each loader incarnation. Provisioning is a new exact 16-byte v1
request in `libs/rustos-user-abi/src/syscall/loader_job_realm.rs`, accompanied by
one typed, attenuated descriptor. It uses cooperative rootd supervision so a
queued spawn's first VFSD lookup can complete. An independently duplicated
descriptor precedes worker startup; incarnation/lease revalidation follows its
join. `services/rootd/src/job_lease.rs` drains this realm before retiring the
loader's service job, including incomplete bounded retirement.

`services/loaderd/src/application_launch.rs` places every live runtimed/SESSIOND
spawn in a child of that realm, not in initd's protected service job. The
loader retains supervisor custody in
`services/loaderd/src/application_jobs.rs`, sets all three limits before
prepare (8 processes, 64 threads, 524288 resident user-data pages), verifies
membership before a successful reply, and retires the whole job on an error
or failed terminal reply. Runtime-supplied placement handles are refused.
Normal collection happens on the next loader request, not a new periodic boot
probe; a reaped leader retires its remaining descendants. Rootd retains the
ancestor if the loader dies. Runtimed remains the process requester and still
records a suspended child before activation. Runtime-launched services receive
the same isolation; rootd's existing lease protection excludes them from OOM
victims. No Job descriptor is given to an app.

The 2 GiB default ceiling is finite but allows physical pressure in the current
2 GiB KVM guest. The opt-in `RUSTOS_LOADERD_LIMIT_PROBE=1` build selects 128 MiB
only for `/apps/oomhog/oomhog.elf` for the separate limit-refusal proof; it does
not lower UI/service quotas. Neither setting claims a strict bound
against forced image-binding charges or concurrent admissions. The model
`application-job-custody` projects provisioning, all-limits-before-publication,
exact placement, retained retirement and ancestor recovery; Loom/Shuttle are
bounded protocol projections. Native tests and focused mutants accompany it.
Verification of step 0a: native kernel-ps 1035, kernel-compat 233, kernel-mm
103, kernel-object 7, rustos-user-abi 70, rootd host-test 63, loaderd 25, and OOM
validator 14 passed. The now lane passed 895 native selections / 932 witness
associations. Nineteen focused implementation mutants (two are re-anchored
existing rootd mutants) and five spec mutants were killed. The bounded model
checked 1780 states; full PR sealed 45 artifacts. A sealed 16-vCPU ordinary
smoke passed. A separate real hog boot passed exact job-level OOM acceptance:
pid 75, job 42949672961, resident 372335 pages, free pages 3947 → 208939, with
actual DVM input relay and no reported panic/stall/relay failure. This proves
at least half-page recovery, not an observed full drain.

The requested sealed 16-vCPU 40-boot cohort was 38/40 (command exit 1): run 2
missed the wayclick first-frame marker, run 6 missed `ipcbench: end`. There was
no matched same-session control, so no regression-rate claim or clean bill is
made. The runner did not archive both failed debug logs; the retained run-6
failure summary has no TLB/IRQ diagnostic milestone in its causal tail, which
is not evidence about the unavailable run-2 full log. Convenience evidence:
`/tmp/ring3-app-kvm-oom-debugcon.log`, `/tmp/ring3-app-kvm-40.log`,
`/tmp/ring3-app-kvm-40-failure.json`; the exact seal was copied to
`/tmp/ring3-app-0a-pr-seal.json` (tree
`4ad95cd5189ba9e9ee369e51b9d4a6eca20f46fcf90ddbc8771d36c112c34179`).
Live limited-hog refusal remains open; this is not phase-0 closure. Older
as-built statements that application jobs do not exist describe earlier
snapshots. Subsequent edits invalidate that exact-tree seal.

### Application limit proof (step 0b functional evidence; stability follow-up)

The opt-in `RUSTOS_ROOTD_APP_LIMIT_PROBE=1` rootd build adds a state machine in
`services/rootd/src/application_limit_probe.rs` to the existing census loop,
not a new thread/timer or default boot check. Combined with
`RUSTOS_LOADERD_LIMIT_PROBE=1`, only the hog receives the shared 32768-page
quota defined in `libs/rustos-user-abi/src/syscall/loader_job_realm.rs`.
The existing job ABI v4 already reports thread and unreaped-process-object
usage separately from live membership; `services/rootd/src/jobs.rs` preserves
all three in its client snapshot.

The observer requires an exact quota and JobId, one live process and one
process object/no children, only one remaining thread, resident charge from 512 pages below to 256 above the
limit (advisory-admission/binding slack), and free pages above twice the kernel
reserve. It holds inspect/kill authority, rechecks protected-subtree exclusion
before each cleanup step, retains custody on partial/denied cleanup, and
reports drain only after live membership, unreaped objects, threads and page
charges are zero. The zero-live count alone is insufficient: the real wire →
client → probe regression failed with premature close/drain before preserving
`usage_processes`, and passed with the separate object counter. The model and
Loom/Shuttle projections now retain the post-kill zero-live/nonzero-object
interval. A stale generational job also means drained under the kernel's
release gate.

`tools/xtask/src/kvm/application_limit.rs` independently requires the hog's
exact loader placement and accounting/drain identities plus four distinct
complete checksum-framed vector-14 worker events. The existing native x86
wait-status test in
`kernel/compat/src/user/syscall/linux/process_termination_tests.rs` maps that
class to SIGSEGV. Physical OOM kills, worker-creation/mmap failures, malformed
frames, repeats, wrong identity/limit and incomplete drain refuse acceptance.
The host mode is `--oom-hog-job-limit`, mutually exclusive with watchdog proof.

These are distinct claims: the boot observes worker fault class, near-limit
accounting, last-thread state, physical headroom and drain; attribution to
quota refusal also uses the signed hog source's valid anonymous first touches
in `apps/oomhog/oomhog.c` and the existing native/MM admission witnesses. There
is no new per-fault kernel cause counter, no whole-C/MM/x86 correctness proof,
and no claim that the final process wait status is SIGSEGV: rootd deliberately
cleans up the surviving main thread. The model `application-limit-refusal`
uses two workers/three pages, with bounded Loom/Shuttle retirement projections,
not executions of production atomics or teardown. Following the P1 fix, the
model checked 431 states; all 70 rootd host-tests and 17 implementation/seven
spec mutants passed, with the full required native batch and focused
Loom/Shuttle witnesses. The initial refreshed now lane passed 916 selected
native tests / 958 associations. Full PR then exposed 23 existing test-order-
dependent mutation baselines, fixed by explicit test-only side-storage growth
in `kernel/ps/src/multitask/pager_vma/tests.rs`,
`kernel/ps/src/multitask/process_placement/tests.rs`, and
`kernel/ps/src/multitask/current_identity/execution/tests.rs`. Their standalone
red cases now pass green, and all 23 mutants were killed without weakening
production or runner guards. Post-fixture now passed 951 selected tests / 993
associations; required native, opt-in signed build, check and contracts passed.
Live opt-in 60-boot evidence and PR resealing remain pending.

The first sealed opt-in 16-vCPU attempt was rejected by the strict host parser:
rootd observed job 42949672961 / pid 75 at 32769 charged pages against 32768,
one thread, free/reserve 342128/3947, and reported drain, but raw `!EX:0e`
breadcrumbs spliced structured fault-frame prefixes. This is not a passed quota
proof. The parser is not relaxed. `kernel/hal/src/arch/idt/handlers.rs` now
routes ordinary user breadcrumbs through the existing bounded, allocation-free
serialized sink in `kernel/nucleus-core/src/debug/mod.rs`. Supervisor, NMI,
double-fault and refused-retirement emergency paths stay lock-free/raw. The
`exception-diagnostic-custody` model and native sink-selection/boundary witnesses
cover this seam; their concurrency projection is not port-I/O or panic proof.
The exact pre-fix logs are `/tmp/ring3-quota-kvm-first.log` and
`/tmp/ring3-quota-kvm-first-debugcon.log`; its full-tree supplemental seal was
`08c41c639319c8df72339c8893a88bbc9b8de004c35375bb43ad600ab401406b`.
The correction passed full PR (45 artifacts) and a zero-exclusion supplemental
seal over 1772 source files, hash
`61940ff3ed7e1f09f642db8c1659733c9863254e1251a0f4b333a53bfb346b21`.
The strict single 16-vCPU proof then passed: pid 73, job 42949672961, charged
32768 pages, free 342110 pages, four checksum-verified vector-14 workers,
exact object/page drain and real DVM input relay.

The subsequent opt-in cohort was 59/60 (aggregate exit 1). Every attempt and
pre/post full-tree check was preserved under `/tmp/ring3-quota-60-framing`.
Run 1 missed `ipcbench: end` within 30 seconds; its accounting/drain markers
were present but the overall attempt is not counted as a pass. Its full log
contains no TLB/IRQ diagnostic or panic marker. Accepted charge snapshots
ranged 32768–32770 pages. All 60 raw logs contain first-reschedule-IPI markers
for CPU indexes 0–15; this presence check is distinct from complete-frame
validation and is not a matched-control regression claim. No same-session
control cohort or performance measurement was run. The test modes remain
off by default; restore a normal signed image, reseal and execute the required
40-boot default smoke before advancing.

Fresh resume evidence (2026-10-30): the previous temporary logs were unavailable,
so the current source was verified again rather than treating this chronology
as proof. The now lane passed 955 native tests / 998 associations; the required
native batch passed (kernel-ps 1035, kernel-compat 233, kernel-mm 103,
kernel-object 7, rustos-user-abi 70), as did rootd 70 / loaderd 25 host tests,
normal signed build, check, contract generation/check and exact-tree PR seal
reuse (45 artifacts). An additional zero-exclusion seal covers all 1772 files,
tree `55ee10fed64c080a622dc75d748904e59b44eedce06d2a887a747300462b7b7c`.

The exact requested default 16-vCPU / 20-second / 40-boot command yielded 38/40
(exit 1). Runs 1 and 12 missed ipcbench/compositor/wayclick readiness; run 1
also logged the uiserver main-present watchdog. Both full failed logs are
retained; neither contains either TLB/IRQ diagnostic milestone or a kernel
panic marker. All 40 logs contain first-reschedule markers for CPU0–15, and
none contains an enabled limit-probe result. This is not a matched-control
regression finding or a claim that all boots passed.

The source-unchanged opt-in signed image then passed one strict quota proof
and a fresh 60-boot cohort yielded 57/60 (exit 1). Runs 9 and 28 lacked quota
observation/drain; run 59 had those markers but missed DVM GPU readiness.
These are failed attempts, not accepted quota proofs. All 60 raw logs contain
CPU0–15 first-reschedule markers; no failed log contains a TLB/IRQ diagnostic
milestone or kernel panic marker. There were 58 accounting observations in the
range 32768–32770 pages. The strict host acceptance passed in each of the 57
successful attempts. Source/PR supplemental checks ran before and after every
attempt. These marker checks do not prove arbitrary scheduling progress or a
matched-control failure rate.

All three probe build options were then unset again. Normal signed build,
check and contract check passed, followed by a successful live job-level OOM
proof with real input relay: pid 73, job 42949672961, resident 372271 pages,
free pages 3947 → 374914. This observes substantial recovery, not a full job
charge-drain assertion. Current evidence, command lines and raw per-attempt
logs are retained under `build/ring3-evidence/resume-20261030`; the original
supplemental seal is retained there too. The user requested execution stability
before further implementation, so HEAD control preparation and a same-session
interleaved comparison are next. Step 3 has not been implemented; no dedicated
benchmark or commit has run. These documentation edits invalidate the earlier
supplemental source seal and require resealing before another SMP boot.

The subsequent same-session interleaved comparison completed 40 pairs:
current tree 38/40, clean HEAD 37/40, two-sided Fisher p=1.0. Source supplements
were checked before/after every attempt and raw logs are retained under
`build/ring3-evidence/resume-20261030/ab40`. This does not establish equivalence
or complete stability. Neither arm logged TLB/IRQ diagnostics or a kernel panic;
all 80 logs contain CPU0–15 first-reschedule markers. Both arms exhibited a
host runtime-trace rejection when the first snapshot marker preceded storage
but a later verified snapshot could satisfy the dependency. The serial host
selector correction in `tools/xtask/src/formal_contracts/runtime_trace.rs`
leaves all scenario prerequisites and strict frame/identity checks intact.
Its six focused native tests, 21204-state model, five spec/five source mutant
kills, independent review, now959/1002, required stable batch and full PR passed.
A fresh sealed default16-vCPU/20-second/40-boot cohort then passed40/40, exit0,
with complete-tree checks before the cohort and after each completed attempt
(1775 files, tree
`8490b7f51593680b80118444cd1cab6507026720403fe45b7521f96d8714f2f5`).
All40 raw logs have CPU0–15 first-reschedule markers and no TLB/IRQ diagnostic,
kernel panic, uiserver watchdog or enabled limit-probe result. Evidence is under
`build/ring3-evidence/resume-20261030/causal40`. This closes that execution-tooling
slice, not arbitrary boot liveness or a measured performance/failure-rate
improvement. Storage-before-input with no later storage marker and ipcbench
timeouts remain failed-attempt categories, not outcomes this correction repairs.
Step3 is next and remains unimplemented. Result-document edits invalidate this
exact-tree seal; final resealing precedes further work.

### Job qualitative ceilings (step 3a; validation in progress)

`libs/rustos-user-abi/src/syscall/job_policy.rs` defines the separate 96-byte
JobPolicy v1 ABI at `SYS_RUSTOS_JOB_POLICY` (`0x5255_005a`), leaving Job ABI v4
unchanged. `kernel/compat/src/user/syscall/linux/job_policy_ops.rs` requires
an exact typed INSPECT descriptor for GET and SET_POLICY for SET; SET_LIMITS
cannot authorize a qualitative change. Closed wire fields are checked before
mutation. A later copyout fault reports an error without undoing scalar SET.

`kernel/ps/src/multitask/process_table/jobs/policy.rs` owns four-word ceilings
under PROCESS_TABLE. ALL is the default own mask; every live ancestor clamps
the effective mask. A completed SET refreshes all existing descendants; racing
single-bit checks see old or new data, not an atomic subtree/256-bit snapshot.
Kill invalidates the cache; reuse cannot alias its generation. The cache's
release/acquire replacement-payload bridge prevents an old incarnation from
borrowing a new tenant's operation word. Immutable process membership is
published in `kernel/ps/src/multitask/process_table/identity.rs` and revalidated
around the bounded Current adapter in `kernel/ps/src/multitask/current/job_policy.rs`.
Missing/hidden publication refuses; no table lock, state borrow or PID lookup
is introduced in this read path.

`services/rootd/src/job_policy.rs` validates typed ABI replies. Its opt-in
controller in `services/rootd/src/job_policy_probe.rs` requires the exact
`RUSTOS_ROOTD_POLICY_PROBE=1` build setting, restores the root mask, checks
all words/hierarchy, attenuated rights, stale generations and child retirement.
Failures retain cleanup authority. The test-build-only handler witnesses the
actual installed Current cache on root GET; default builds do not run it.
`tools/xtask/src/kvm/job_policy.rs` independently requires complete verified
kernel frames from the exact rootd/bootstrap subject, distinct denial and a
later allowed observation (initial allowance is not restoration), and a
successful controller with no cleanup/restore failure. `--job-policy-probe`
selects host acceptance and cannot combine with memory/process stress proofs.

Models `job-policy-lifecycle` and `job-policy-publication`, native tests,
implementation/specification mutants and source-anchored Loom/Shuttle/herd
witnesses cover these bounded mechanisms. They are not executions/proofs of
production spinlocks, Current ownership, all MM teardown, arbitrary scheduling
progress or atomic whole-mask updates. Live boot validation and exact-tree
sealing for this slice are not yet complete.

The first opt-in attempt missed the controller because a subsequent KVM dry-run
used its default build option and restaged a normal image. That is failed test
configuration, not a quota/policy proof. Rebuilding opt-in and checking the signed
rootd artifact restored the controller. The second attempt passed the controller
but strict host acceptance refused absent Current-cache frames. The debug sink
classified the new exact event as best-effort; the log's dropped counter rose
during the probe. A native regression reproduces BestEffort instead of Required.
`kernel/nucleus-core/src/debug/milestone_class.rs` now puts only the exact event
in existing bounded retry/deferred custody; near-prefix events stay best-effort.
Native selector tests and targeted mutants accompany this correction. This does
not prove queue capacity or arbitrary output progress, and no parser acceptance
condition is relaxed. Subsequent source changes invalidate both earlier seals;
new closure gates and sealing precede any further SMP boot. Future dry-runs use
`--no-build --no-auto-verify` so they cannot replace the selected test image.
The correction's regression passed green and both exact-class/near-prefix
implementation mutants were killed fresh. Refreshed now1024 tests/1071
associations, required native/services, nucleus-core69, xtask237, preflight20/20,
check, contracts generation/check, non-mutating dry-run and opt-in signed build
passed. New full PR/whole-source sealing and live rerun remain pending.

**Not step 3 completion:** no production broker consumes this ceiling yet,
no independently authorized positive subject grant replaces legacy roles,
and no overlap release has occurred. `IPC_SERVICE_CAP_*` stays intact until
independent old/new decisions agree through the required release interval;
local tests cannot establish that release history. A bitmap alone cannot encode
exact live endpoint-owner, fork noninheritance and revocation authority.

### 13. Alternatives considered

- **Per-process limits only (rlimits).** Does not aggregate a multi-process
  service, so a service cannot be bounded or killed as one thing.
- **cgroup-style multi-controller groups with processes in several groups.**
  Charges become ambiguous. One membership keeps accounting simple.
- **Quota *transfer* as the only mechanism (Genode).** It gives hard guarantees
  without overcommit but forces static budgeting of every component. The design
  keeps `max` caps for ordinary jobs and `min` guarantees for the few that need
  them, and reserves transfer semantics for session quotas.
- **Building on today's service-endpoint registry.** The registry is a fixed
  16-slot name table and cannot also be the authority model.

### 14. Open questions

- Whether threads need a separate accounting level, or `threads.max` per job is
  enough.
- Whether the Linux personality must present a cgroup-like filesystem; Linux
  applications and container tools expect one.
- How page cache and swap are charged once `page_cache.rs` is live.
- JobPolicy has a 256-bit bitmap; assignment of operation authority and the
  legacy service-capability cutover remain open.
- Zircon's concrete policy kinds and inheritance rules were **not** read for
  this document (the fetched page described only the job syscalls); read them
  before fixing the policy model.

### References

- [Linux cgroup v2: memory.max/high/min/low, memory.oom.group, cgroup.kill, memory.events, pids.max, the no-internal-process and top-down rules, PSI](https://docs.kernel.org/admin-guide/cgroup-v2.html)
- [Zircon job object](https://fuchsia.dev/docs/zircon/objects/job.md) and [kernel concepts](https://fuchsia.googlesource.com/fuchsia/+show/0054a8a1162c2ea857fb02553835b804ead7b124/docs/concepts/kernel/concepts.md)
- [Genode: resource trading](https://genode.org/documentation/genode-foundations/19.05/architecture/Resource_trading.html)
- [Android lmkd](https://source.android.com/docs/core/perf/lmkd)
- [seL4 capDL](https://docs.sel4.systems/projects/capdl/), [seL4 Microkit verification](https://trustworthy.systems/projects/microkit/verification)

<a id="korean"></a>

## 한국어 요약

**상태(2026-10-31 소스 기준):** Job 생성·수명주기(1단계), 청크형 프로세스
디렉터리(2a), 계층형 프로세스·스레드 한도 및 상주 사용자 페이지 과금(2b)이
구현되었습니다. 기본 프로세스 한도 32개는 고정 테이블 크기가 아니며,
디렉터리 내부 상한은 4,096개입니다. 프로세스당 스레드 상한 32개와
Job 슬롯 64개는 별도의 내부 제한입니다.

애플리케이션을 별도 Job에 배치하는 경로와 256비트 JobPolicy v1도
존재하지만, opt-in 검증을 전체 안정성이나 레거시 서비스 권한 비트의
대체 완료로 간주하지 않습니다. `kmem.max_bytes`, `memory.high/min`,
클라이언트 부담 세션 쿼터, 컴포넌트 매니저 통합은 후속 작업입니다.

상세한 소스 연결과 날짜별 검증 증거는 위 **Phase 1 / 2a / 2b /
Application placement / Job qualitative ceilings as built** 항목을
참고하세요. 과거 설계 제안과 실패 기록은 현재 구현을 설명하는
근거로 재사용하지 마세요.
