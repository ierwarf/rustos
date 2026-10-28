# Ring3 Process Model

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

**Status: decision record and roadmap. Step 1 (job object, root job, kill by
job) is implemented as described in [Job Model](job-model.md#phase-1-as-built);
steps 2 to 8 are not.** It states
what in the current ring3 structure must be replaced, the target shape, and the
invariants that keep later work from being redone. The first and largest
component, jobs and quotas, is specified in [Job Model](job-model.md).

Tags: **(verified)** read from this tree, **(proposed)** a design decision,
**(recalled)** background knowledge not re-read for this document.

### 1. Verdict

The split of policy into named services (`rootd`, `vfsd`, `netd`, ...) is
sound and stays. What does not scale is the layer beneath it: the process and
service model has hard-wired numbers and enumerations. It fits a fixed system of
about fifteen services; third-party services, many applications, per-user
isolation, or any resource accounting would rewrite it. So the work is three
full redesigns and five partial ones.

### 2. Evidence

All **(verified)** against the tree.

| Fact | Where |
| --- | --- |
| 32 processes system-wide, 32 threads per process | `MAX_PROCESS_OBJECTS`, `MAX_THREADS_PER_PROCESS`, [process_table.rs:27](../../kernel/ps/src/multitask/process_table.rs) |
| 16 registered service endpoints | [ipc_ops.rs:80](../../kernel/compat/src/user/syscall/linux/ipc_ops.rs) |
| Service identity is an integer 1..15 | `IPC_SERVICE_*`, [syscall.rs:332](../../libs/rustos-user-abi/src/syscall.rs) |
| Service authority is one bit per service, 64 at most | `IPC_SERVICE_CAP_*`, same file |
| Bootstrap manifest is a compile-time array with a `u16` dependency mask and a hard-coded index | [service_manifest.rs](../../services/rootd/src/service_manifest.rs) |
| Restart policy is a fixed budget of 3 with a 250 ms backoff | [rootd main.rs](../../services/rootd/src/main.rs) |
| No per-service memory, handle, endpoint or process quota; CPU has a domain-admission quota | grep of `kernel/ps`, `kernel/compat`, `services` |
| Security context is uid/gid plus a `logical_admin` flag | `ProcessSecurityContext`, [process_state.rs:85](../../kernel/ps/src/user/process_state.rs) |
| 42 `RING3-MIGRATION-REFERENCE` exception blocks in 39 kernel files (Linux and Windows compatibility, tty, console host, VFS metadata, DVM transports) | `kernel/compat`, `kernel/io-manager` |
| Seven services are `std`, six are `no_std`; raw `syscall0/1/2` wrappers are re-implemented per service (`rootd` 6, `devmgrd` 6, `netd` 5, ...) | `services/*/src` |
| IPC wire formats are hand-written `#[repr(C)]` structs, about 10 000 lines, versioned per struct | `libs/rustos-user-abi` |
| Lifecycle is split across `rootd` (core and post-init leases), `initd`, `runtimed`, and `sessiond`, which reconcile leases among themselves | [ring3-service-maturity.md](../ai/ring3-service-maturity.md), `services/*` |

Not examined: the internals of `uiserver` (about 23 000 lines) and `runtimed`
(about 7 000), and any cost estimate.

### 3. Redesigns

**Full**

| ID | Area | Why it cannot be patched | Target |
| --- | --- | --- | --- |
| A | Process and service identity, namespace | The numbers are in the ABI and in static lock-free arrays | Job tree, generational identity, capability routing instead of integer service IDs |
| B | Lifecycle and supervision | Four supervisors share leases; the manifest is static and positional | One declarative component manifest, one component tree, one supervision tree with restart intensity and escalation |
| C | Resource accounting and quotas | Nothing aggregates across processes | Hierarchical budgets in the job tree; session quotas funded by clients |

**Partial**

| ID | Area | Direction |
| --- | --- | --- |
| D | Policy still in ring0 | Finish the migration. The kernel keeps syscall entry and mechanism; Linux and Windows semantics move to personality servers |
| E | Service runtime | One `svc-runtime`: server loop, typed dispatch, deadlines, per-session heaps; one syscall layer; retire the `std`/`no_std` split for services |
| F | IPC wire format | An IDL with generated bindings and written compatibility rules, replacing hand-written structs |
| G | Security model | A sandbox derived from the manifest and enforced by job policy, replacing uid/gid plus `logical_admin` |
| H | Operability | Structured logs, crash dumps, health and watchdog, A/B update with rollback. These are commercial requirements, and none was found in this survey |

### 4. Target shape

```text
kernel      mechanism only
  Job tree · Process · Thread · Channel/Endpoint · Handle(rights) · VMO
  syscall entry · scheduling · frame/TLB · measurement · group kill

rootd       component manager (holds the root job)
  manifest → component tree (realms)
  realm ↔ job            (resource and authority)
  capability routing     (naming and access)
  supervision tree       (restart intensity, escalation, criticality)
  memory governor        (chooses a job to kill under pressure)

services    components in realms: vfsd · netd · storaged · inputd · devmgrd · uiserver ...
personality servers       Linux · Windows (take over D)
applications              components in a per-session realm
```

Two trees, deliberately separate. The **job tree** is the kernel's: resources,
authority, group lifetime. The **component tree** is `rootd`'s: names, routing,
supervision. A realm maps to a job, but the kernel never learns what a component
is. Fuchsia draws the same line: jobs carry resources and policy, and a
component manager brokers capabilities between components **(verified from the
Fuchsia documentation: capabilities are routed by explicit offer/expose along the
topology, so a component cannot reach what no ancestor passed down)**.

### 5. Invariants

Fixed now, so that no phase has to be reopened.

1. **Identity** is `(job, generation, moniker)`. The kernel stores the first two;
   `rootd` stores the moniker.
2. **No ABI-visible `const` count.** Every limit is a job policy value, with the
   root job's values coming from boot configuration.
3. **Service names are routed capabilities**, not integers or bits.
4. **Wire formats are generated from an IDL**, with compatibility rules.
5. **One supervisor.** `initd`, `runtimed` and `sessiond` become components in the
   tree.
6. **Mechanism in the kernel, policy in services** (already the rule in the
   repository's `AGENTS.md`).
7. **Core services are described at build time and signed**, applications at
   run time. seL4 Microkit does the same: its monitor only iterates a system
   description compiled in advance **(verified from the Microkit material)**.

### 6. Order

Ordered so that each step's output is the final form of what it touches. Phase
numbers match [Job Model §12](job-model.md#12-migration).

| Step | Work | Area |
| --- | --- | --- |
| 1 | Job object and root job, process↔job binding, kill by job | A, C |
| 2 | Process-table directory; charging; `memory.max`, `processes.max`, `threads.max` | A, C |
| 3 | Job policy replaces the `IPC_SERVICE_CAP_*` bits | A, G |
| 4 | IDL and the unified `svc-runtime`, service by service | E, F |
| 5 | Component manager: manifests, realms, capability routing, supervision trees | B |
| 6 | Session jobs funded by clients | C |
| 7 | Personality servers; shrink the 42 kernel exception blocks | D |
| 8 | Operability: logs, dumps, health, update | H |

Steps 4 and 5 can run in parallel with 2 and 3 once step 1 has fixed the job
interface; nothing before step 1 should start, because every other step names a
job.

### 7. What the OOM work already built is reused

| Existing | Becomes |
| --- | --- |
| Per-process resident count from the frame-descriptor ledger | Per-job hierarchical usage |
| `rootd` memory governor | The root job's memory governor, selecting jobs |
| Ring0 watchdog | Kills the job of the refused process unless critical or covered by `memory.min` |
| `SYS_RUSTOS_ROOTD_MEMORY_BROKER` census | Job census in the job ABI |
| Watermark / user-class allocation | Unchanged; the physical reserve stays global |

### 8. Non-goals

- A rewrite of the scheduler, IPC fast paths or memory manager. They are
  referenced, not replaced.
- Dynamic loading of services into the kernel. Policy stays in ring3.
- Source compatibility with Fuchsia, seL4 or Genode. They are design references.

### References

- [Fuchsia components v2 introduction](https://fuchsia.googlesource.com/fuchsia/+show/HEAD/docs/concepts/components/v2/introduction.md), [realms](https://fuchsia.googlesource.com/fuchsia/+/cfef5042b7b9976bc7e0ed8c11f005ee41d1b7b9/docs/concepts/components/v2/realms.md)
- [Zircon job object](https://fuchsia.dev/docs/zircon/objects/job.md), [kernel concepts](https://fuchsia.googlesource.com/fuchsia/+show/0054a8a1162c2ea857fb02553835b804ead7b124/docs/concepts/kernel/concepts.md)
- [Genode resource trading](https://genode.org/documentation/genode-foundations/19.05/architecture/Resource_trading.html)
- [seL4 capDL](https://docs.sel4.systems/projects/capdl/), [Microkit verification](https://trustworthy.systems/projects/microkit/verification)
- [Linux cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html), [Android lmkd](https://source.android.com/docs/core/perf/lmkd)

<a id="korean"></a>

## 한국어 요약

**상태: 결정 기록과 로드맵입니다. 1단계(job 객체, 루트 job, job 단위 kill)만
구현되었고 2~8단계는 아직입니다.**

- **판정**: 정책을 이름 있는 서비스로 나눈 구조는 유지합니다. 그 아래 프로세스·서비스
  모델이 서비스 약 15개짜리 고정 시스템에 맞춰져 있어, 서드파티 서비스나 다수
  앱, 사용자별 격리, 자원 회계가 들어오면 다시 써야 합니다.
- **전면 재설계 3건**: (A) 프로세스·서비스 신원과 이름공간 → Job 트리와 capability
  라우팅, (B) 수명주기·감독 → 선언형 컴포넌트 매니페스트와 단일 감독 트리,
  (C) 자원 회계 → job 트리의 계층형 예산과 클라이언트가 내는 세션 쿼터.
- **부분 재설계 5건**: (D) ring0에 남은 정책 42블록, (E) 서비스 런타임 통일,
  (F) IPC wire 형식의 IDL화, (G) 매니페스트 기반 보안 모델, (H) 운영성(로그, 덤프,
  헬스, 롤백 가능한 업데이트).
- **구조**: 커널은 job 트리(자원·권한·그룹 수명)만 알고, `rootd`가 컴포넌트
  트리(이름·라우팅·감독)를 갖는 컴포넌트 매니저가 됩니다. realm은 job에
  대응하지만 커널은 컴포넌트를 모릅니다.
- **불변식**: 신원은 `(job, generation, moniker)`, ABI에 보이는 `const` 개수 없음,
  서비스 이름은 라우팅된 capability, wire 형식은 IDL 생성, 감독자는 하나, core는
  빌드 시 서명·앱은 런타임.
- **순서**: Job 객체 → 테이블 디렉터리·충전·한도 → 정책이 권한 비트 대체 → IDL과
  `svc-runtime` → 컴포넌트 매니저 → 세션 쿼터 → personality 서버 → 운영성.
  1단계(Job 객체)가 인터페이스를 확정하기 전에는 다른 작업을 시작하지 않습니다.
- **재사용**: 이번에 만든 OOM 작업(census, 거버너, watchdog)은 버려지지 않고 job
  단위로 확장됩니다.
- **미검증**: `uiserver`/`runtimed` 내부와 공수 추정은 평가하지 않았습니다.
