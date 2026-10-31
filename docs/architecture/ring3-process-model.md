# Ring3 Process Model

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

**Status (2026-10-31 source snapshot):** This is a target architecture and
migration plan, not an all-unimplemented feature list. Job lifecycle (step 1),
the chunked process directory (2a), counted process/thread limits, resident
user-page accounting (2b) and application Job placement have landed. JobPolicy
v1's 256-bit ceiling is implemented as a **partial step 3a**; the complete
legacy-capability cutover has not. Kernel-memory accounting,
`memory.high/min`, unified service/runtime/IDL, component supervision,
client-funded sessions, personality servers and operability work remain open.\nA no_std component-policy model, generated staging IDL, validated rootd role,
route and supervisor-parent manifests, bounded rootd checkpoints and runtimed
console sessions are implemented, but **do not yet constitute a completed production cutover**.
See [Job Model as-built sections](job-model.md#phase-1-as-built) for exact status.
Dated tests there are not automatically current release evidence.

Tags: **(verified)** refers to the snapshot identified above,
**(proposed)** to a target and **(historical)** to the pre-migration audit.

### 1. Verdict

The division into named userspace policy services remains sound. The redesign
targets the **identity, resource and supervision model beneath those services**,
not the scheduler, IPC fast paths or basic memory manager. Process-table
scalability and initial job quotas are already implemented; fixed service
identity/authority and fragmented lifecycle supervision are not yet retired; production post-init
reporter admission now uses a manifest-declared exact parent edge.

### 2. Current source evidence

| Area | Snapshot observation | Owner |
| --- | --- | --- |
| Process directory | Default admission 32, non-moving chunked slots up to internal ceiling 4,096; no longer a 32-slot static array | [process table](../../kernel/ps/src/multitask/process_table.rs), [slot directory](../../kernel/ps/src/multitask/slot_dir.rs) |
| Thread limits | Job-level counted quota; per-process structural maximum 32 | [process table](../../kernel/ps/src/multitask/process_table.rs) |
| Job and resident memory | Generation-bound Job tree, hierarchical counted limits and resident-page charge implemented; kernel-memory and high/min policies open | [Job model](job-model.md#phase-2b-ii-memory-limit-as-built) |
| Application isolation | Loader application-job placement implemented; opt-in limit evidence is not general stability acceptance | [Job model](job-model.md#application-placement-step-0a-as-built-limit-proof-still-open) |
| JobPolicy | 256-bit v1 ceiling and GET/SET operations exist, but service-capability cutover is incomplete | [job policy ABI](../../libs/rustos-user-abi/src/syscall/job_policy.rs) |
| Service discovery | Immutable rootd service-role and routing manifests now validate legacy-equivalent positive policy; live registration still uses fixed numeric IDs | [service ABI](../../libs/rustos-user-abi/src/syscall.rs), [rootd manifest](../../services/rootd/src/service_manifest.rs) |
| Supervision | `rootd`, `initd`, `runtimed` and `sessiond` retain distinct lifecycle responsibilities | [Services](services.md) |
| IPC wire/runtime | Existing production wire structs and heterogeneous runtimes; generated IDL now serves live initd/rootd lease query/reclaim, while other schemas remain staging-only. The IDL does not grant authority. | [ABI contract](../ai/contracts-abi.md) |

The **historical** audit found 32 static process slots and no per-job quotas.
Do not repeat those findings as current implementation facts. The original
`uiserver`/`runtimed` estimate was not an exhaustive source audit.

### 3. Remaining redesign work

| Area | Implemented foundation | Remaining target |
| --- | --- | --- |
| A. Process/service identity | Chunked process slots; generational process and Job identities | Routed named capabilities instead of numeric service identities |
| B. Lifecycle/supervision | Individual service restart and lease contracts | Declarative component tree and one coherent supervision owner |
| C. Resource accounting | Counted process/thread and resident user-page limits | Kernel-memory, high/min, client-funded session budgets |
| D. Personality policy | Existing user-service brokers | Remove remaining ring0 personality exceptions without weakening mechanisms |
| E–F. Runtime/IDL | Existing `rustos-svc-runtime` and versioned `repr(C)` ABI | Shared service runtime, generated bindings and compatibility rules |
| G. Security model | Typed Job handles and JobPolicy v1 ceiling | Full manifest-derived sandbox and legacy-capability cutover |
| H. Operability | Existing logging, probes and fault models | Crash/health, update/rollback and sustained recovery evidence |

These are **targets**, not claims that each row is absent or that
historical audit counts still match the tree.

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

**Proposed end-state invariants** (not a list of already-shipped properties):

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

### 6. Migration order and snapshot status

| Step | Target | 2026-10-31 status |
| --- | --- | --- |
| 1 | Root Job, membership and group kill | Implemented |
| 2 | Chunked process directory; job process/thread and resident memory quotas | Partly implemented: 2a/2b landed; `kmem.max_bytes`, `memory.high/min` open |
| 3 | Replace legacy service authority with Job policy | JobPolicy 3a exists; cutover unproven |
| 4 | Generated IDL and unified service runtime | Production post-init lease query/reclaim migrated to generated IDL (opcodes 517/518), scalar operations 8/9 retired; other services and shared runtime migration remain open |
| 5 | Declarative components, realms, capability routing and one supervisor | Component graph and immutable rootd route/role manifests implemented; unified live supervisor and kernel capability transfer open |
| 6 | Client-funded session quotas | Real kernel-backed client Job billing now wired for runtimed console creation and teardown, including hierarchical session ceilings and generation-bound Job refunds; generic provider sessions and durable crash recovery remain open |
| 7 | Linux/Windows personality servers and ring0 policy reduction | Existing service brokers preserved; remaining slowpath-only migration unproven; ring0 hot paths deliberately untouched |
| 8 | Operational logs, crash reporting, health and rollback | Health/update policy models, rootd checkpoint and transitive service revocation implemented; durable recovery and verified rollback open |

The staging work for steps 4–8 exists in source, but each production
authority/lifecycle transition still needs source and KVM evidence before old
routes are deleted. See [the Ring3 cutover ledger](ring3-component-model.md)
for implemented-vs-model-only boundaries. See the separate [TLB design](tlb-shootdown-design.md):
its redesign is outside this service-model roadmap.

### 7. What the OOM work already built is reused

| Existing | Becomes |
| --- | --- |
| Per-process resident count from the frame-descriptor ledger | Per-job hierarchical resident usage (implemented for user data pages) |
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

**상태(2026-10-31 소스 기준):** Job 생성·수명주기(1단계), 프로세스
디렉터리 청크화(2a), 프로세스·스레드 한도 및 상주 사용자 페이지 과금(2b),
애플리케이션 Job 배치가 구현되었습니다. JobPolicy v1의 256비트
한도도 구현되었지만 기존 서비스 권한 비트 대체는 끝나지 않았습니다.
Ring3 컴포넌트 정책 모델, 스테이징 IDL 생성, rootd의 역할/라우팅
매니페스트, 체크포인트와 runtimed 콘솔 세션 상한은 구현되었습니다.
다만 실제 서비스·커널 권한의 전체 전환을 완료했다는 뜻은 아닙니다.

다음 구조적 작업은 `kmem.max_bytes`, `memory.high/min`, 서비스
커널에서 강제되는 capability 라우팅, 단일 컴포넌트 감독, 운영 ABI/IDL 전환,
클라이언트 부담 세션 쿼터, personality 서버와 운영성 확장입니다.
과거 감사의 '프로세스 32개 고정 배열·Job 과금 없음'은 현재 사실이
아닙니다. 기본 한도 32개와 내부 청크 디렉터리 상한 4,096개를
구분해야 합니다.

위 8단계 표는 **목표 아키텍처와 현재 상태를 분리**합니다.
구체적 구현·검증은 [Job Model](job-model.md)의 as-built 섹션과
실제 소스를 기준으로 판단하세요.
