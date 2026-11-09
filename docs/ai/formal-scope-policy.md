# Formal scope policy

Which tool covers which kind of property, how mutants are graded, how big a
concurrent model must be, how a model graduates to executable conformance, and
which models are still the right tool. Owner: `formal/`. Checked by
`formal/check-validators.py` (validators) and `cargo xtask formal-contracts
check` (models, flows, witnesses). Prior art is listed at the end.

## 1. Decision rule

Ask in this order and stop at the first yes.

1. Is the property about **one actor** on **one input** (a parser, a validator,
   a size computation, an admission predicate, a request/response shape check)?
   It is a **validator**, not a model. Specify it with an executable check in
   the owning crate: Kani for unbounded integer/array reasoning, an exhaustive
   small-domain test against an independent oracle when the domain is
   enumerable (this repository's default, because it needs no extra tool and
   runs in the PR gate), `proptest` when the domain is not enumerable. Register
   it in `formal/validators.tsv`. Do not write a TLA+ model: a three-state
   "valid, invalid, published" machine proves nothing the code does not.
2. Is the property a **lock-free kernel** (CAS words, memory ordering, ABA on a
   packed word)? Use Loom or Shuttle on the real data structure
   (`formal/loom-proof-kernel`, `formal/shuttle-proof-kernel`) with a litmus
   test for the architecture claim. A TLA+ model of a CAS protocol is allowed
   only as the *protocol* companion (slot growth, vector lease) and must say
   which Loom witness covers the memory-order half.
3. Is the property the **ordering of a pipeline** (build -> sign -> stage ->
   boot, driver supply, release bundle, readiness markers)? Use an integration
   test that exercises the pipeline (`cargo xtask kvm-smoke` marker, a stage
   test, a build-plan check). A model of a pipeline restates its script.
4. Is it **several actors interleaving** over shared state, with loss,
   restart, stale observation or revoke (scheduler, IPC handoff, TLB, endpoint
   registry, supervisor recovery, DVM lease/ring protocols)? It is a **TLA+
   model**: `formal/models.tsv`, minimum size in section 4, with a source
   witness (section 5).
5. Otherwise it is not a formal property yet; write the test.

A model must also be **replayable** when it claims a *runtime* contract: if its
invariants name events that the kernel or a service can log, add the event
names to the runtime trace (`formal/runtime-trace*`, `runtime_trace.rs`) so a
real boot can be checked against the model. If it names no observable event,
it cannot be replayed, and the model is only a design aid.

Retire a model (section 7) only when the replacement is demonstrably equal or
stronger: it rejects the mutants the model's invariants were written to catch,
both directions (accepts what the oracle accepts, rejects what it rejects), and
the atomicity half ("a rejection publishes nothing") is asserted explicitly.
If it is not, keep the model and write down why.

## 2. Mutation severity rubric

`formal/implementation-mutations.tsv` grades each source mutant. The grade is
the *consequence if a real regression of the same shape shipped and no test
noticed*. `formal/classify-mutation-severity.py` applies this rubric mechanically
and prints a proposal (it never edits the TSV); a human accepts or overrides.

| Grade | Consequence | Typical examples |
| --- | --- | --- |
| critical | Input an unprivileged principal controls (a Ring3 process, a file or network byte, a device) causes memory unsafety, privilege or authority escalation, isolation breakage, data loss or corruption, or a kernel panic / unrecoverable hang | W^X region admitted by the ELF/PE gate; a stale capability generation accepted; a user-copy bound dropped; a forged reply accepted; double free of a frame |
| high | Same failure class but only from a trusted-but-fallible source (firmware, signed boot image, authenticated service), or a kernel-internal invariant whose violation stalls or mis-schedules the system with no direct unprivileged trigger | overlapping ECAM region admitted from firmware; MADT BSP not normalized; scheduler wake never notified; a restart budget ignored |
| medium | Wrong accounting, ordering, budgets or policy with bounded blast radius; the system degrades but stays safe and recoverable | vruntime charge off by one tick; a per-job counter not decremented on one path; wrong restart backoff constant |
| low | Diagnostics, tracing, readiness evidence, benchmarks and harness behaviour: wrong output, no safety consequence | milestone label, warm-up marker, help text, KVM transcript summary |

Rules of thumb: grade by the **worst reachable consequence**, not by the line
changed; a one-token change in an authority check is critical even though it is
one token. Downgrade only when the consequence is demonstrably contained (the
check is duplicated downstream, or the value never reaches an authority). A
mutant that cannot change observable behaviour is *equivalent* and is removed,
never graded.

### Gate by severity

`formal/run-implementation-mutations.py` accepts `critical|high|medium|low` and
runs under `--profile pr|nightly` (`formal/verify-all.sh` passes its own
profile; a direct run defaults to `nightly`).

| Severity | Role | `pr` profile | `nightly` profile |
| --- | --- | --- | --- |
| critical, high | blocking | every row executed; 100% must be killed by the exact witness, any survivor fails | same |
| medium, low | advisory | executed only for packages with a changed file (working tree and untracked files against `HEAD`, or `RUSTOS_MUTATION_CHANGED_BASE`); a survivor is printed as `WARNING implementation mutation survivor (...)` and recorded in the summary (`advisory.survivors`), the gate still passes; rows not executed are counted and named in the summary (`advisory.not_executed`) | every row executed, cache off; any survivor fails the profile and is listed |

A witness that cannot be established (baseline failure, missing or ambiguous
test) fails in every profile and for every severity: that is a registry defect,
not a survivor. A `--only` run executes exactly the rows asked for. Path
selection is a deliberate approximation (a change in a dependency does not
select its dependents' advisory rows); the nightly profile is the audit that
bounds it. The summary reports `kill_ratio` over executed rows and
`blocking_kill_ratio` (must be 1.0).

`formal/classify-mutation-severity.py` proposes grades by the rubric above
(surface tier from the source path, safety tier from the mutation id; the
`reason` column names the rule). `--apply [--proposal FILE] [--tsv PATH]`
rewrites only the severity column of the registry in place, preserving every
other byte and the row order, after checking every row's current grade against
the proposal; it is covered by `formal/test-classify-mutation-severity.py` and
`formal/test-implementation-mutation-runner.py` (both in `formal/selftest.sh`).

## 3. Where each kind of check runs (summary)

| Property shape | Tool | Gate | Replayable? |
| --- | --- | --- | --- |
| sequential validator / parser / size math | exhaustive small-domain test or Kani, mutants in `implementation-mutations.tsv` | PR (source conformance + mutation lane; critical/high 100%, medium/low per the severity gate) | n/a, deterministic |
| lock-free kernel (CAS, ordering) | Loom/Shuttle on the real code, litmus for the architecture | PR / nightly | schedule seeds |
| multi-actor protocol | TLA+/TLC at the section 4 minimum, source witness | PR (critical), nightly | runtime trace where the events are logged |
| pipeline ordering | integration test / `kvm-smoke` marker | PR / release | the log itself |
| timing / readiness claim | `kvm-smoke` under the timing profile (section 8) | release | build/kvm archive |

## 4. Minimum configuration size for concurrent models

A concurrent model that cannot express the interference it exists to exclude
gives false confidence. Minimums, reviewed in the model header:

* **At least three actors** (e.g. two CPUs plus an exiting owner; producer,
  consumer and revoker) **plus one adversarial delayed actor**: a stale
  observer, a late acknowledgement, a replayed request or a recycled slot. Two
  actors cannot exhibit third-party interference (wake stealing, ABA through
  slot reuse, a stale ticket redeemed after reuse).
* **At least two generations of every reusable identity** (slot, task id,
  endpoint, handle), so reuse is reachable, and **capacity one below the
  number of actors** where exhaustion is a property.
* Every outcome the real protocol has: success, reject, timeout, revoke and
  exit; a model without its failure arms checks only the happy path.
* A model that is intentionally smaller (state explosion) records the missing
  dimension in its header and names the companion that covers it
  (`synchronous-ipc-handoff-concurrency` is the pattern).
* If the model fits in under a few hundred states it is almost certainly a
  validator: apply section 1 first.

Mutation adequacy for a model is `formal/spec-mutations.toml`: a model whose
invariants survive every plausible mutation of its own spec is vacuous.

## 5. Conformance ladder

A model or validator climbs one rung at a time; each rung is a stronger claim
and is named in `formal/CONFORMANCE.md`.

1. **Source witness**: a named test (`model|package|test` in
   `formal/run-source-conformance.sh`) proves the Rust anchor exists and the
   decision it models is exercised. Weakest rung; it alone cannot reject a
   changed implementation.
2. **Implementation mutants**: the witness *fails* when the guarded line is
   mutated (`formal/implementation-mutations.tsv`); this is what turns a
   witness into evidence. Validators need at least five killed mutants
   (`check-validators.py`).
3. **Generated tests**: the model's state graph (TLC dump, counterexample
   generator, or the oracle in the validator) produces the test inputs, so the
   executable check covers every modelled transition rather than the cases an
   engineer thought of. The validators in section 7 are at this rung for their
   enumerable domains.
4. **Trace**: a real execution (KVM boot, Loom schedule) is replayed against
   the model (`runtime_trace.rs`). Required for any model whose claim is "the
   system behaves like this at run time".

## 6. Model triage

Reading of every registered model header against the section 1 rule
(153 model directories; `userspace-wait-set` holds two modules). Verdict
`keep` means the property is a real multi-actor interleaving. Convert/merge
rows say whether an equal-or-stronger replacement already exists. Six models
are already retired (section 7) and are not listed. Models whose header says
"undocumented" have no descriptive comment; the verdict is from their variables
and owner and should be rechecked when the header is written.

Totals: keep 117, convert-to-Kani/proptest 10, convert-to-integration-test 18, merge 8.

| Model | Verdict | Justification | Replacement exists? |
| --- | --- | --- | --- |
| acceptance-profile-publication | keep | the registry appears after uiserver start; one watcher races its publication | - |
| affinity-claim-admission | keep | undocumented header; affinity claim versus migration | - |
| application-job-custody | keep | rootd, loaderd and a replacement loader race over lost provisioning replies | - |
| application-limit-refusal | keep | two hog workers race to charge pages near the job quota; exact accounting under concurrency | - |
| atomic-process-activation-batch | keep | initd, loaderd and exiting children race one all-or-nothing cohort | - |
| authority-identity-lifecycle | keep | concurrent minting and revocation of identities; the nonwrap arithmetic alone could move to Kani | - |
| boot-storage-handoff | keep | host-native driver, DVM and controller reset hand over one device with ordered recovery | - |
| bootstrap-activation-handoff | keep | supervisor-committed children and unrelated IPC race the first-turn FIFO | - |
| capability-derivation-lifecycle | keep | derive and revoke race across object, lease and derivation epochs | - |
| child-wait-publication | keep | old slot withdrawn, new task claims it, parent publishes: three actors on one slot | - |
| clocksource-deadline | keep | timer interrupt, sleeper and clock source interleave; the arithmetic part could move to Kani | - |
| cold-exec-intent | keep | undocumented header; exec intent claim versus exit | - |
| commercial-service-envelope | convert-to-Kani/proptest | single request, dispatch, reply validator chain with one actor; no interleaving | No; needs a table-driven exhaustive test over the shared request/response validators |
| cow-frame-lifecycle | keep | undocumented header; COW refcount raced by fork, fault and exit, document actors | - |
| cpu-affinity-observation | keep | topology snapshot observed while CPUs change state | - |
| cpu-dispatch-preemption | keep | undocumented header; dispatch preemption window across IRQ and scheduler | - |
| cpu-online-lifecycle | keep | per-CPU lifecycle raced against other CPUs and the scheduler | - |
| cross-cpu-task-retirement | keep | exec/exit barrier between several CPUs, scheduler ownership and page-table reclaim | - |
| deferred-process-activation | keep | requester exit, loaderd restart and activation replay race the one-shot authority | - |
| deferred-start | keep | loaderd, supervisor and child race the one-shot activation | - |
| devmgrd-sessiond-isolation | keep | a synchronous console ioctl into sessiond must not block unrelated device work (starvation) | - |
| dma-iommu-isolation | keep | domain activation versus device DMA; no identity mapping may be live | - |
| driver-domain-fleet | keep | several domains compete for exclusive hardware; fleet exclusivity | - |
| dvm-absolute-pointer | keep | host broker, Linux DVM and RustOS ring/lease protocol with independent actors and loss/restart | - |
| dvm-agent-readiness | merge | process-owned readiness identical in shape to dvm-display-readiness; one model with two instances | Partial: KVM smoke asserts the readiness markers |
| dvm-amdgpu-evidence | convert-to-integration-test | signed physical-display evidence admission is a sequential policy check in host tooling | Partial: tools/hostd evidence tests; the physical-GPU lab proof is not CI |
| dvm-amdgpu-supply | convert-to-integration-test | build/supply-chain pin contract for one PCI target; no runtime interleaving | Partial: supply lock checks in the driver-domains build plan |
| dvm-atomic-scanout | keep | KMS atomic commit ownership between compositor and display driver | - |
| dvm-block-startup | keep | asynchronous DVM startup raced with the first volume query and the kernel check-arm-recheck sleep | - |
| dvm-block-transport | keep | fixed block queue producers and consumers plus the flush durability boundary | - |
| dvm-commercial-lifecycle | keep | host lease, DVM restart and RustOS epochs interleave | - |
| dvm-control-endpoint | convert-to-integration-test | launch-private endpoint derivation and bind is a single-actor sequence in L0 | Partial: libs/driver-domain-host tests |
| dvm-control-relay | keep | L0 listener, Linux agent and RustOS relay interleave | - |
| dvm-display-driver-supply | convert-to-integration-test | pinned driver supply admission is a build-time pipeline | Partial: sources.lock and release-bundle checks |
| dvm-display-readiness | merge | see dvm-agent-readiness; merge the two readiness models | Partial: KVM smoke asserts the readiness markers |
| dvm-display-scheduler | merge | bounded scheduler admission, same shape as dvm-gpu-proof-scheduler; merge | Partial: KVM smoke |
| dvm-gpu-admission | keep | uiserver worker, display DVM and legacy presenter race a promotion | - |
| dvm-gpu-atlas-transport | keep | atlas slot ownership between RustOS, QEMU and DVM | - |
| dvm-gpu-compositor | keep | private submit owner and DVM execution race over batches | - |
| dvm-gpu-proof-scheduler | merge | see dvm-display-scheduler | Partial: KVM smoke |
| dvm-input-revocation | keep | transport-generation reset races the input ring consumer and a replacement session | - |
| dvm-input-ring | keep | cache-line cursor SPSC ring across guest and host (also needs Loom-style memory-order review) | - |
| dvm-input-selftest | convert-to-integration-test | KVM-only self-test device sequence inside one agent | Yes in effect: --exercise-input markers are the executable check |
| dvm-network-control | keep | control lease gating versus ring traffic | - |
| dvm-network-ring | keep | two-ring ivshmem substrate with producer and consumer | - |
| dvm-read-cache | keep | storaged cache hit/fill raced by a generation change from the transport | - |
| dvm-release-bundle | convert-to-integration-test | release-bundle admission is a build and sign pipeline | Partial: write-manifest and the loaderd release-stage probe |
| dvm-transport-lifecycle | keep | undocumented header; transport generation lifecycle shared by several consumers, document or merge into dvm-input-revocation | - |
| dvm-volume-io | convert-to-Kani/proptest | geometry, capacity and feature validation of one response against its generation; a pure validator | No; Kani or exhaustive test on the response admission function |
| early-system-admission | convert-to-integration-test | module count, allowlist and signature pipeline across xtask stage, multiboot and boot-protocol; the table and digest half is already validator:early-system-content | Partial: validator:early-system-content covers table and digest; module uniqueness and allowlist need a stage integration test |
| endpoint-publication | keep | publish versus lookup, wait and exit protocol below the registry | - |
| endpoint-receiver-wakeup | keep | classic lost wakeup: arm, recheck, block | - |
| endpoint-registry | keep | observable registry wait contract under concurrent publication | - |
| entropy-broker-boundary | convert-to-integration-test | sequential composition boot seed, CSPRNG, copyout, policy; one actor | Partial: entropy unit tests; add a boot-seed absent/present integration case |
| exception-diagnostic-custody | keep | user exception breadcrumb and fatal frames share one serialized sink | - |
| exception-retirement-lifecycle | convert-to-integration-test | x86 exception entry, diagnostics, resume or retire is one thread's sequence | Partial: exception unit tests and KVM fault probes |
| exec-address-space-transaction | keep | exec reservation versus concurrent exit and the ProcessState lock | - |
| exec-ticket | keep | procd, loaderd and Linux threads race exec tickets | - |
| frame-reader-claim | keep | undocumented header; reader claim of frames raced by writers | - |
| futex-waiter-lifecycle | keep | waiter insertion, wake removal and requeue race | - |
| gpu-submit-transaction | convert-to-Kani/proptest | commit-after-accept and rollback of the compiler timeline is a sequential transaction | No; proptest over accept/reject sequences in the compiler |
| gui-dvm-install | convert-to-integration-test | install and failure cleanup is a sequential try_install with rollback | Partial: dvm_display install tests and the gui-dvm KVM smoke |
| gui-dvm-pixel-authority | keep | RustOS, DVM and QEMU share a pixel/control pool; ownership transfer is the property | - |
| gui-dvm-surface | keep | RustOS, DVM and QEMU share a pixel/control pool; ownership transfer is the property | - |
| input-ingestion-worker | keep | the ring consumer must not depend on app poll cadence: producer, worker and reader | - |
| input-readiness | keep | ring0 ingress, waiter wake and readiness handoff | - |
| ipc-endpoint-ownership | keep | kernel IPC endpoint, caller, receiver and teardown interleavings | - |
| ipc-fast-handoff | merge | undocumented header; same CPU-handoff custody as synchronous-ipc-handoff, merge or document the delta | - |
| ipc-handle-transfer | keep | kernel IPC endpoint, caller, receiver and teardown interleavings | - |
| ipc-priority-inheritance | keep | kernel IPC endpoint, caller, receiver and teardown interleavings | - |
| ipc-priority-queue | keep | kernel IPC endpoint, caller, receiver and teardown interleavings | - |
| ipc-reply-deadline | keep | kernel IPC endpoint, caller, receiver and teardown interleavings | - |
| ipc-reply-recv-transaction | keep | kernel IPC endpoint, caller, receiver and teardown interleavings | - |
| ipc-transfer-authority | keep | kernel IPC endpoint, caller, receiver and teardown interleavings | - |
| ivshmem-pairing | convert-to-integration-test | peer-id allocation order versus connection order is one sequential host handshake | Partial: ivshmem server tests in driver-domain-host |
| job-handle-publication | keep | job tree membership, limits and policy mutated by spawn, exit and policy writers concurrently | - |
| job-lifecycle | keep | job tree membership, limits and policy mutated by spawn, exit and policy writers concurrently | - |
| job-limits | keep | job tree membership, limits and policy mutated by spawn, exit and policy writers concurrently | - |
| job-policy-lifecycle | keep | job tree membership, limits and policy mutated by spawn, exit and policy writers concurrently | - |
| job-policy-publication | keep | job tree membership, limits and policy mutated by spawn, exit and policy writers concurrently | - |
| job-spawn-placement | keep | job tree membership, limits and policy mutated by spawn, exit and policy writers concurrently | - |
| kernel-resource-accounting | keep | per-object and per-process quotas charged by concurrent threads | - |
| lifecycle-execution-claim | keep | current cold effects versus parked banks on one CPU and a stopper | - |
| loader-request-authority | keep | service revoke during image loading versus the terminal ring0 commit | - |
| memfd-seal-lifecycle | keep | seal installation versus mapping admission and resize under one mutex | - |
| memory-charge-conservation | keep | charge and uncharge on several CPUs against the descriptor ledger | - |
| mm-activation-close | keep | undocumented header; address-space activation close raced by exit | - |
| msi-vector-lifecycle | keep | lock-free free-slot lease CAS and handler CAS; Loom is the stronger tool and should replace the plain TLA+ model long term | Loom witnesses exist (formal/loom-proof-kernel); keep until they cover revoke-commit |
| netd-deferred-reply | keep | deferred poll reply raced by readiness and caller timeout | - |
| network-payload-session | convert-to-Kani/proptest | bounded authenticated Ethernet payload admit or drop is a pure validator; a fuzz target exists | Partial: libFuzzer target (bug finding, not exhaustive); convert next with an exhaustive frame-shape test |
| oom-termination-owner | keep | rootd and the watchdog race to end one process | - |
| page-cache-lifecycle | keep | several CPUs and the pager share page-table or page-cache state | - |
| page-table-lifecycle | keep | several CPUs and the pager share page-table or page-cache state | - |
| page-table-map-transaction | keep | several CPUs and the pager share page-table or page-cache state | - |
| pager-fault-slot-lifecycle | keep | pager, faulting task and exit race over fixed fault/VMA/frame-grant slots | - |
| pager-frame-grant-lifecycle | keep | pager, faulting task and exit race over fixed fault/VMA/frame-grant slots | - |
| pager-region-agreement | keep | pager, faulting task and exit race over fixed fault/VMA/frame-grant slots | - |
| pager-vma-publication | keep | pager, faulting task and exit race over fixed fault/VMA/frame-grant slots | - |
| per-cpu-clockevent-lifecycle | keep | AP bring-up versus timer interrupt ordering across CPUs | - |
| persistent-mutation-admission | convert-to-Kani/proptest | vfsd mutation admission is decided before any storage effect; sequential policy | No; exhaustive admission-table test in vfsd |
| physical-frame-lifecycle | keep | per-CPU frame caches and the central bitmap race (lock-order property) | - |
| post-init-bootstrap-barrier | keep | initd, rootd and replacement supervisors recover shared lease state | - |
| post-init-leases | keep | initd, rootd and replacement supervisors recover shared lease state | - |
| post-init-supervisor-recovery | keep | initd, rootd and replacement supervisors recover shared lease state | - |
| proc-broker-session | keep | loader-owned session raced by process exit and broker commands | - |
| process-address-space-lifetime | keep | ProcessRef holders race process teardown | - |
| process-lifecycle-transaction | keep | spawn, exec, exit and reap namespace under concurrent actors | - |
| process-signal-delivery | keep | pending-bit removal versus frame revalidation | - |
| product-boot | convert-to-integration-test | composition of boot topologies exercised by xtask; the executable form is kvm-smoke itself | Yes in effect: kvm-smoke topologies |
| remote-file-mapping | convert-to-integration-test | loaderd, kernel, vfsd and early-system pipeline ordering for one request at a time | Partial: loaderd/compat tests cover copy admission; Linux/PE launch smoke covers the pipeline |
| robust-futex-owner-death | keep | retiring thread, waiters and clear_child_tid race on owner-died publication | - |
| root-authority-publication | keep | first publisher versus concurrent claimants of the service namespace root | - |
| rootd-bootstrap | merge | supervised launch transaction; overlaps service-bootstrap-lifecycle, merge | - |
| rootd-restart-backoff | keep | restart budget and authority across supervisor crashes | - |
| runtime-control-authority | convert-to-Kani/proptest | per-connection peer authorization decision; sequential | No; table test over peer credentials in the runtimed socket |
| runtime-control-rpc | convert-to-Kani/proptest | response admission: a reply must match the exact RPC; a single-frame validator | No; exhaustive test on libs/runtime-control response admission |
| runtime-trace-causal-selection | convert-to-Kani/proptest | selection among immutable matching log candidates is a deterministic function | No; proptest on the trace selector in xtask |
| scheduler-active-balance | keep | SMP scheduler state shared by several CPUs and IRQ entry; the interleaving is the property (Loom/litmus companions exist) | - |
| scheduler-admission | convert-to-Kani/proptest | runtime launch weight admission is a pure function of catalog record and policy | No; exhaustive test over the weight/class table in runtimed spawn |
| scheduler-cpu-distribution | keep | SMP scheduler state shared by several CPUs and IRQ entry; the interleaving is the property (Loom/litmus companions exist) | - |
| scheduler-cpu-ownership | keep | SMP scheduler state shared by several CPUs and IRQ entry; the interleaving is the property (Loom/litmus companions exist) | - |
| scheduler-local-authority | keep | SMP scheduler state shared by several CPUs and IRQ entry; the interleaving is the property (Loom/litmus companions exist) | - |
| scheduler-terminal-claim | keep | stop request against dispatch claim before Current publication | - |
| scheduler-thread-demotion | keep | SMP scheduler state shared by several CPUs and IRQ entry; the interleaving is the property (Loom/litmus companions exist) | - |
| scheduler-wakeup | keep | SMP scheduler state shared by several CPUs and IRQ entry; the interleaving is the property (Loom/litmus companions exist) | - |
| scheduling-context-budget | keep | undocumented header; budget donation across CPUs, document the actors | - |
| service-bootstrap-lifecycle | merge | see rootd-bootstrap | - |
| service-call-authority | keep | endpoint id reuse and delegated authority race across service restarts | - |
| service-heap-lifecycle | keep | allocator regions and concurrent threads in a service generation | - |
| service-mutation-recovery | keep | rootd-retained mutation versus replacement of vfsd or netd | - |
| sigchld-notification | keep | coalesced cause snapshot raced by new child events | - |
| slot-directory-growth | keep | lock-free AtomicDir growth; Loom is the stronger tool for the memory-order part | Loom witness exists; keep the model for the growth protocol |
| smp-release-admission | convert-to-integration-test | launcher gate over fresh seals is a sequential evidence check | Partial: xtask ensure_smp_launch_evidence tests |
| smp-reschedule-ipi | keep | one packed word per CPU raced by sender, claimer and a late sender; CAS ordering is the property | - |
| smp-ring3-qualification | convert-to-integration-test | KVM-private provenance and evidence chain checked after the run | Partial: xtask qualification tests |
| synchronous-ipc-handoff | keep | reply-handoff window between two tasks on two CPUs | - |
| synchronous-ipc-handoff-concurrency | keep | reply-handoff window between two tasks on two CPUs | - |
| syscall-scheduler-continuation | keep | a wake racing a syscall frame between check, arm and commit | - |
| syscall-simd-lifecycle | keep | syscall, IRQ preemption and register IPC race over the XMM image custody | - |
| task-affinity-lifecycle | keep | effective mask changed by sched_setaffinity while the task migrates | - |
| task-retirement-cleanup-owner | keep | directory producer, cleanup consumers and a recycled slot race (Loom companion) | - |
| tlb-shootdown-lifecycle | keep | initiator, remote CPUs and a late acknowledger race over a TLB epoch | - |
| trusted-ui-boundary | keep | DVM scanout, uiserver and the privileged prompt race for pixel authority | - |
| ui-frame-budget | convert-to-integration-test | keyboard-to-frame budget is a measured latency, not an interleaving; covered by the UI FPS gate | Yes in effect: --min-ui-fps windows measure it |
| ui-input-motion | convert-to-integration-test | KVM-only pointer trajectory generator in the agent | Yes in effect: FPS gate input windows |
| ui-main-loop-wakeup | keep | idle-wait lost wakeup between input publication and the main loop | - |
| user-stack-growth | convert-to-integration-test | eager stack mapping with a guard page is a single-task sequence | Partial: mm unit tests and the Linux stack smoke |
| userspace-wait-set | merge | two modules (generic wait set and vfsd epoll registry) share one directory; merge the shared registry part | - |
| vfio-release-authorization | convert-to-Kani/proptest | release parser plus durable lease check before activation; sequential | No; Kani or exhaustive test on the release parser in libs/driver-domain-host |
| vfs-open-description-recovery | keep | OPEN published after durable chunks; rootd-retained recovery across restarts | - |
| wayland-accept-isolation | keep | a slow or hostile client must not stall other clients' accept | - |
| wayland-frame-pacing | keep | client frame callbacks versus presentation | - |
| zero-trust-service-flow | keep | two services and a delegated policy service; end-to-end authority across restart | - |

Next conversion candidates in order of value: `network-payload-session`,
`persistent-mutation-admission`, `runtime-control-rpc`,
`runtime-control-authority`, `scheduler-admission`, `vfio-release-authorization`
(all pure validators, no replacement yet). Models owned by files that other work
is changing (runtimed, scheduler) were triaged but not converted here.

## 7. Retired models and validators

Registry: `formal/validators.tsv`. A flow's `model` column, a zero-trust row, a
proof-index `formal_model` and a source-conformance catalogue line may name a
`validator:<id>` instead of a TLA+ model; `tools/xtask/src/formal_contracts`,
`check-system-flows.sh`, `check-zero-trust-subsystems.sh`, `check-proof-index.py`
and `run-source-conformance.sh` accept both, and
`check-validators.py` enforces the validator-specific rules (witnesses exist,
flows cite it, retired models are gone, at least five mutants killed).

| Validator | Retired model(s) | Replacement checks | Mutants killed (private run) |
| --- | --- | --- | --- |
| `validator:image-admission` | dual-abi-image-admission, dual-abi-byte-parser | `libs/rustos-image-admission/src/exhaustive_tests.rs`: plan admission vs predicate for 0..2 regions, ELF exec images vs byte predicate for two load segments, every ELF table truncation, PE layout vs predicate for two sections, every PE truncation/extension, unaligned relocation page; plus the existing `tests::` and Kani harnesses | 27 |
| `validator:pci-bar-sizing` | pci-bar-discovery | `BarConfig` seam in `kernel/hal/src/arch/pci.rs`; `pci/bar_sizing_tests.rs` simulates every device shape (unimplemented, I/O, 32-bit, 64-bit memory BAR) and asserts the probe protocol at access time | 8 |
| `validator:acpi-table-admission` | acpi-table-admission | `kernel/hal/src/arch/acpi/admission_tests.rs`: RSDT/XSDT signature, width, alignment, checksum; MCFG predicate plus atomicity and capacity; HPET GAS shape | 16 |
| `validator:madt-topology-admission` | cpu-topology-admission | same file: MADT topology predicate, atomicity, BSP normalization, over-capacity order | 15 |
| `validator:early-system-content` | filesystem-content-integrity | `kernel/io-manager/src/storage/boot_volume.rs` tests: every single-byte corruption and every truncation of a real image, non-canonical tables with valid digests, namespace aliases, cache bound to the image address | 7 |

Equivalence evidence for each: the replacement compares the production function
with an independent oracle (plain `i128` arithmetic or a stated predicate) over
an enumerable domain *in both directions* and asserts atomicity (a rejected
table leaves the published set untouched). The implementation mutants listed
in `new-implementation-mutation-rows` were each applied in an isolated copy of
the tree and observed to be killed by the designated witness. No
`spec-mutations.toml` entries existed for these six models, so the oracle was
their TLA+ invariants and the guards in the code; equivalent mutants (a guard
duplicated downstream) were discarded rather than graded.

What the TLA+ models could not say and the validators now do: the real byte
layouts, field widths and overflow behaviour (the models used two booleans),
and the exact probe sequence of PCI BAR sizing (the model had three states).
What was deliberately **not** carried over: ELF/PE post-parse snapshot
stability (a loaderd file-copy property, covered by its integration path, not
an admission function) and the `early-system-admission` boot-module count
(a Multiboot/stage property, kept as a model until a stage integration test
exists).

## 8. Timing profile for readiness claims

An idle-host boot proves readiness only on an idle host. For a readiness or
latency claim, also run the same topology under pressure:

    cargo xtask kvm-smoke --host-cpu-pressure 50 ...
    cargo xtask kvm-smoke --host-cpu-pressure 100 ...

`--host-cpu-pressure <1..=100>` keeps that percent of the host cores the run may
use busy (rounded up to whole cores, one pinned busy worker per core,
distributed across the allowed core list) for the whole run, including every
`--repeat` boot, and prints `timing profile host-cpu-pressure=...`. It adds host
load only; the guest, QEMU command line and recorded evidence are unchanged, so
quote the profile with the result. It is refused with `--smp-iteration` because
sealed SMP evidence does not record the profile, and is ignored by `--dry-run`.
Design: `tools/xtask/src/kvm/host_pressure.rs` (worker-count and core-spread
are pure functions with unit tests; affinity comes from `sched_getaffinity` of
the xtask process, which QEMU inherits).

## Appendix A. Pin fragility inventory (read-only audit)

Fragility classes: **A** pure text token (substring or regex presence), **B**
ordered tokens (positional substring match; whitespace normalised), **C**
semantic (compiled or typed). None of the three pin files contains class C.

| File | Size | Mechanism | Class | Cheapest semantic replacement |
| --- | --- | --- | --- | --- |
| `formal/check-dispatch-source-contracts.py` | 1386 lines | 191 `ordered()` calls over 200 extracted function bodies in 36 files; 72 `SystemExit` checks; 20 regex absence checks for deleted symbols | B (ordering), A (absence) | Ordering: a unit test with a recording seam (a trait or event log, as `BarConfig` does for PCI) asserting the effect order on the real function; absence of deleted authority: a `compile_fail`/`trybuild` test that names the symbol, or delete the module so the compiler enforces it |
| `formal/smp-source-contracts.toml` | 3663 lines | 357 `required_sequences` (4429 tokens, median 11 per entry, 137 files), 104 `forbidden_patterns`, 12 `per_cpu_statics`, 5 `audited_fail_stops`, 2 `admit_commit_pairs` | B (sequences), A (forbidden, fail-stop) | 707 of 4429 tokens are `fn` signature existence: replace with a compiled reference from a test (`use`/call) that fails to build when the item moves. `per_cpu_statics` capacity: `const _: () = assert!(...)` on the real array. Sequences: the same recording seam or a Loom schedule over the real code |
| `formal/check-performance-contracts.sh` | 990 lines | 140 `rg -Fq` literal greps, 16 regex `rg`, 5 `sed -n` range extractions; 24 are `NAME: type = value` constant pins on `libs/rustos-user-abi/src/performance.rs` | A, B for the `sed` ranges | Constant pins: one unit test (or `const _: () = assert!`) in `rustos-user-abi` importing the constants and asserting the value and the relations (`READ_AHEAD_IN_FLIGHT_LIMIT < READ_CACHE_WINDOW_LIMIT` is already a const assertion). Lock-acquisition and IPC-count budgets: counted lock-acquisition tests in the owning crate |

Headline: about 730 class-A pins (707 `fn` existence tokens plus 24 constant pins)
have a compiler-enforced or test-enforced replacement of equal strength; the ordering pins (about 550
`ordered()` plus `required_sequences` entries) are the fragile ones: a
refactor that preserves behaviour and reorders or renames a token fails them,
and a behaviour change that preserves the tokens passes them. Replace by
rewriting each function behind a typed seam rather than by loosening the
tokens. This audit did not edit the three files.

## Prior art

Verification status of each source is stated, because two of the seven pages
could not be fetched. "Read" means the full text of that source was read while
writing this section (2026-11-09). The claims below are limited to what was read.

* Newcombe, Rath, Zhang, Munteanu, Brooker, Deardeuff, "How Amazon Web
  Services Uses Formal Methods", CACM 58(4), 2015. **The CACM page returned
  HTTP 403 and was not read.** The earlier version by the same authors, "Use of
  Formal Methods at Amazon Web Services" (AWS, 29 September 2014, PDF hosted at
  lamport.azurewebsites.net/tla/formal-methods-amazon.pdf), was read. It reports
  that since 2011 AWS engineers used formal specification and model checking
  (TLA+/PlusCal) on S3, DynamoDB, EBS and others; that subtle bugs were errors
  in the *design* (the code faithfully implemented a design that mishandled a
  rare scenario); that testing the code is inadequate for such errors because
  the reachable state count is astronomical; and that the specifications found
  bugs. Used here for: spend modelling on concurrent-protocol design, not on
  sequential code that tests can pin down. Not claimed: anything about
  spec-to-code conformance.
* Brooker and Desai, "Systems Correctness Practices at Amazon Web Services".
  **The ACM Queue page (id 3712057) and the CACM practice page returned HTTP
  403 and were not read.** Only a search-result summary was available:
  Communications of the ACM 68(6), June 2025 (Crossref DOI 10.1145/3729175;
  the Queue id is 3712057, not a DOI), by Marc Brooker and Ankush Desai,
  described as a survey of formal and semi-formal techniques used at AWS
  including P, runtime log checking against P specifications (PObserve),
  fuzzing and fault injection. This policy takes nothing from it beyond the
  idea that several techniques coexist; the earlier wording about a technique
  ladder and model size was removed because it could not be checked. Re-read
  the article before citing it for more.
* Davis, Hirschhorn, Schvimer, "eXtreme Modelling in Practice", PVLDB 13(9),
  1346-1358, 2020 (arXiv 2006.00915). Read. MongoDB studied two conformance
  techniques for TLA+ specs: model-based *trace checking* of the Server's
  replication protocol, found impractical (system size, internal concurrency
  and the abstraction gap to a highly abstract spec); and model-based *test-case
  generation* for Realm Sync's operational transformation, found highly
  successful. Consequence here: generated tests (ladder rung 3) are the
  dependable rung; trace replay (rung 4) is only demanded where the model's
  events are logged one for one by a small component, not for a whole
  concurrent kernel.
* Jackson and Wing, "Lightweight Formal Methods" (1996; the fetched page does
  not state the venue). Read. Claims used: formality itself is not the source of
  the benefit, mechanical analysis is; model only the details that justify the
  cost, starting from a purpose and the risks addressed; accept incomplete
  analysis to find errors reliably; model checking finds bugs more reliably than
  conventional testing. These support partial models and registered
  gaps, not the validator/model split, which is this repository's own rule.
* Freund and Qadeer, "Checking Concise Specifications for Multithreaded
  Software", Journal of Object Technology (the Microsoft Research PDF named
  "jot.calvin.pdf"; volume and year were not legible in the extracted text).
  Read (abstract and introduction). It presents Calvin-R, a static checker for
  multithreaded Java: thread-modular analysis through an access predicate on
  each shared variable, and procedure-modular analysis through a concise
  specification related to the implementation by an abstraction relation that
  combines simulation and reduction. Relevance here is loose: it is the closest
  published form of "relate a concise specification to the code with a
  checkable relation", which the source-witness and mutant ladder approximates
  by testing rather than proof. No claim in this policy depends on its
  results.
* Hillel Wayne, "Augmenting Agile with Formal Methods", 30 July 2018. Read.
  Argues formal methods complement testing: concurrency and ordering behaviour
  (his example: a wait/notify deadlock in a bounded buffer found by TLC in about
  thirty minutes) is where hand-written tests fail; model synchronization
  first, keep parameters small because model checking cost grows quickly; and
  the spec is not linked to the code, so the two can drift apart. That drift is
  the problem the conformance ladder and mutation lane address.
* Nelson, Sigurbjarnarson, Zhang, Johnson, Bornholt, Torlak, Wang, "Hyperkernel:
  Push-Button Verification of an OS Kernel", SOSP 2017. Read. The kernel
  interface is made finite (no unbounded loops or recursion) so that Z3 can
  verify the implementation at the LLVM IR level against a state-machine
  specification (50 system calls and trap handlers); the paper states that
  verifying multiprocessor support is beyond its scope. Used here only for:
  a finite, loop-free interface makes sequential kernel code amenable to
  automated checking, which supports checking sequential validators with
  executable exhaustive tests instead of modelling them. Hyperkernel itself is
  SMT-based and uniprocessor; it is not evidence for the multi-actor models.
