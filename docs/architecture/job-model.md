# Job Model and Hierarchical Resource Accounting

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

**Status: design. Nothing in this document is implemented.** The one thing that
exists today is its per-process ancestor: the memory census, the watermark, and
the `rootd` / ring0-watchdog OOM policy described in
[the address-space contract](../ai/address-space-lifecycle-contract.md#4-memory-pressure-watermark-and-oom-policy).
The parent decision record is [Ring3 Process Model](ring3-process-model.md).

Every claim below is tagged **(verified)** when it was read from this tree,
**(proposed)** when it is a design decision, or **(recalled)** when it comes from
background knowledge of another system that was not re-read for this document.

### 1. Why jobs

The kernel's process and service limits are compile-time constants, baked into
lock-free static arrays **(verified)**:

| Limit | Value | Where |
| --- | --- | --- |
| processes, system-wide | 32 | `MAX_PROCESS_OBJECTS`, [process_table.rs:27](../../kernel/ps/src/multitask/process_table.rs) |
| threads per process | 32 | `MAX_THREADS_PER_PROCESS`, same file, line 28 |
| endpoints per process / per task | 32 / 8 | [ipc/mod.rs:89](../../kernel/ipc-runtime/src/ipc/mod.rs) |
| registered service endpoints | 16 | `MAX_SERVICE_ENDPOINTS`, [ipc_ops.rs:80](../../kernel/compat/src/user/syscall/linux/ipc_ops.rs) |
| descriptors per process | 65 535 | `MAX_DYNAMIC_FD`, [handles.rs:41](../../kernel/ps/src/user/handles.rs) |
| scheduling domains | `MAX_SCHEDULING_DOMAINS` | [scheduling_runtime.rs:227](../../kernel/ps/src/multitask/scheduler/scheduling_runtime.rs) |
| service identities | 1..15, integers | `IPC_SERVICE_*`, [syscall.rs:332](../../libs/rustos-user-abi/src/syscall.rs) |
| service authority | one bit per service, 64 max | `IPC_SERVICE_CAP_*`, same file, line 351 |

`MAX_PROCESS_OBJECTS` alone sizes at least eight static arrays: `PROCESS_IDENTITY`,
`PROCESS_ID`, `PROCESS_STATE_PTR` ([identity.rs:131](../../kernel/ps/src/multitask/process_table/identity.rs)),
`PINS` ([pin_owner.rs:105](../../kernel/ps/src/multitask/process_table/pin_owner.rs)),
`CELLS` ([process_placement.rs:176](../../kernel/ps/src/multitask/process_placement.rs)),
`REWRITE_EPOCHS` ([rewrite_epoch.rs:10](../../kernel/ps/src/multitask/pager_vma/rewrite_epoch.rs)),
`PAGER_VMA_WRITERS`, and `PAGER_VMAS` (`MAX_PROCESS_OBJECTS * MAX_PAGER_VMAS_PER_PROCESS`,
[pager_vma.rs:450](../../kernel/ps/src/multitask/pager_vma.rs)).

Nothing aggregates resources across processes: there is no per-service memory,
handle, endpoint or process quota, and the security context is a Unix
uid/gid plus a `logical_admin` flag **(verified)**. A service that leaks, or a
client that floods a shared server, is bounded only by the machine.

A **job** is the single object that carries these limits, the authority that
goes with them, and the group lifetime. It replaces the constants above with
policy values, and it is what the OOM policy should select and kill.

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

### 10. Prerequisite: the process table must scale

Lifting `MAX_PROCESS_OBJECTS` is the largest piece of work and is not specific
to jobs. The arrays in section 1 are static because the fault path reads them
lock-free with interrupts off.

Design **(proposed)**:

- Replace each fixed array with a **two-level directory**: a fixed array of
  `AtomicPtr<Chunk>`, each chunk a block of cells, published with a release CAS
  and never freed while any generation in it is live. Lookup stays two dependent
  loads with no lock; chunk allocation happens in normal context at process
  creation, charged to the creating job's `kmem`.
- `PAGER_VMAS` stops being `MAX_PROCESS_OBJECTS × MAX_PAGER_VMAS_PER_PROCESS`
  statically. Each process owns a heap table published through its cell with the
  process generation, charged to its job.
- The directory's size is set by the root job's `processes.max`, read at boot.

Cost, stated plainly: these structures are covered by existing formal models and
concurrency witnesses (for example `process-address-space-lifetime`), so each
change needs the model and the Loom/Shuttle witnesses extended in the same
change. That verification is most of the effort.

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
| 2b | Charging, `memory.max`, `processes.max`, `threads.max`, `kmem.max_bytes` | a job over its limit refuses, others unaffected, on the existing `oomhog` proof |
| 3 | Policy replaces `IPC_SERVICE_CAP_*` | both checks agree for one release, then the bits are deleted |
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
- The exact policy bitmap width and which operations get their own bit.
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

**상태: 설계 문서이며 구현된 것은 없습니다.** 이번 세션에서 만든 OOM 작업
(프로세스별 census, 워터마크, `rootd` 거버너, ring0 watchdog)이 이 설계의
프로세스 단위 선행 형태입니다.

- **문제**: 프로세스 32개, 스레드 32개, 서비스 endpoint 16개, 서비스 ID 정수,
  서비스당 권한 비트 하나(최대 64)가 컴파일 타임 상수이며, 프로세스 상한 하나가
  정적 lock-free 배열 8개의 크기를 정합니다. 서비스·클라이언트 단위 메모리/핸들
  쿼터는 없습니다.
- **해법**: Job 트리. 프로세스는 평생 정확히 하나의 job에 속하고, 한도(메모리,
  프로세스, 스레드, 핸들, 커널 메모리)와 정책(허용 연산 비트맵)은 위에서 아래로
  좁아지기만 하며, 권한은 `HandleRights::Job` 핸들로 하위 트리에만 전달됩니다.
- **핵심 불변식**: 한 프로세스 = 한 job, 한도는 상수가 아니라 정책, 커널은 job만
  알고 컴포넌트는 모름, fault 경로에서는 lock-free 충전, 그룹 kill은 재개 가능한
  단계.
- **충전 방식**: CPU별 stock(배치 64페이지)으로 캐시 라인 경합을 피하고, 한도
  근처에서는 정밀 모드로 전환합니다. 한도는 멀리 있을 때 최대 `배치 × CPU 수`만큼
  흐릿합니다.
- **OOM**: 거버너는 프로세스 대신 job을 고르고, `KILL`은 하위 트리를 함께
  종료하며, watchdog의 서비스 endpoint 휴리스틱은 `critical`/`memory.min`으로
  대체됩니다.
- **선행 과제**: 프로세스 테이블을 2단계 디렉터리로 바꾸는 일이 가장 크고,
  관련 formal 모델과 Loom/Shuttle witness를 같은 변경에서 확장해야 합니다.
- **이행**: 1단계 Job 객체(한도 없음) → 2a 테이블 디렉터리 → 2b 충전과 한도 →
  3 정책이 `IPC_SERVICE_CAP_*` 대체 → 4 컴포넌트 매니저 → 5 세션 쿼터.
- **미확인**: Zircon 정책 종류와 상속 규칙 원문은 읽지 못했고, Linux 메모리
  컨트롤러의 배치 충전과 원격 충전은 기억에 근거합니다.
