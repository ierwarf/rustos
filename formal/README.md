# RustOS formal models

This directory contains small, executable TLA+ models for RustOS contracts
whose correctness depends on interleavings. They are design and regression
checks; they do not generate kernel code or replace Rust tests, fuzzing, ABI
checks, or KVM smoke tests.

The modeled Rust contracts and their remaining abstraction limits are recorded
in [CONFORMANCE.md](CONFORMANCE.md). Update that audit whenever a mapped
source transition or cleanup owner changes.

[REVIEW.md](REVIEW.md) records the model-quality review: it distinguishes an
exhaustive TLC result from the separate checks for clock horizon, freshness,
terminal cleanup, and named liveness assumptions.

## Registered gates and evidence

`models.tsv` is the sole executable-model registry. `selftest.sh` rejects an
unregistered primary TLA source or TLA/CFG pair, orphaned config, missing invariant/property,
unexplained terminal deadlock policy, missing conformance mapping, or a pilot
flag without its corresponding executable artifact. A `temporal` row must use
`SPECIFICATION Spec`; configuring `INIT`/`NEXT` directly would silently bypass
the fairness assumptions written into `Spec`. Do not add a model only to
`run-all-tlc.sh`; that list is generated from the registry.
`system-flows.tsv` is the cross-model lifecycle registry. Its checker binds
stable requirement/hazard IDs to owner transitions, finite waits, one formal
model, one source anchor, and one exact source witness before TLC runs.
`product-scenarios.tsv` composes those local transitions into exact KVM
topologies with explicit prerequisites and absolute deadlines. Sequence is a
stable topological identifier, not a claim that independent branches finish in
that wall-clock order. The `product-boot/ProductBoot` model runs display and
storage admission in parallel after input policy, joins both before the first
presented frame, and requires an exact executable image to be sealed and
activated. The storage-only branch terminates only after its generation-bound
data plane.
`fault-scenarios.tsv` is the closed fault-point/evidence registry; its checker
rejects phantom and duplicate rules and requires every critical/high point to
resolve to exactly one executable source witness. The durability fault also
requires the bounded storage-DVM negative KVM gate.
`abi-divergences.tsv`, `recovery-scenarios.tsv`,
`spec-mutations.toml`, `implementation-mutations.tsv`,
`sanitizer-targets.tsv`, and `concurrency-triangle.toml` close six
independent source-evidence gaps: native dual-ABI drift, bounded
restart/crash-consistency outcomes, test sensitivity to real implementation
regressions, TLA+ property/transition mutation survivors, and instrumented
host-testable critical/high boundaries. Their runners reject stale exceptions,
missing transition classes, zero-test filters, parser-only or compile-only
mutant failures, and unbounded execution. See
SPEC_MUTATION_CONTRACT.md for the one-mutant, named-invariant,
counterexample-trace protocol.
`CONCURRENCY_TRIANGLE_CONTRACT.md` defines the complementary bounded Loom,
Shuttle PCT, and x86_64 herd7 lanes. It requires a source/model/flow binding
and rejects both TLA+ and litmus survivors, so a green checker cannot be
credited for an empty property or a non-sensitive order assertion.
`proof-index.toml` is a closed proof-retrieval graph for the selected Kani and
Verus kernels. It binds an executable source symbol to a registered TLA+ model,
the exact harness or lemma, its companion test, and any dependency edge. The
index checker rejects a Kani proof without its own `kani::cover!`, unregistered
Verus proof files, stale source/model links, dependency cycles, and Verus
`admit`/`assume`/axiom/external shortcuts. `run-proof-index.sh` records hashes
of the index and all indexed inputs, which the PR seal consumes. It is not an
LLM, proof generator, or claim that the source has been fully verified; see
`PROOF_INDEX_CONTRACT.md` for the precise boundaries.
The PR TLC profile is deliberately a 120-second, risk-weighted pre-QEMU set:
it retains the exact finite configurations of 21 critical ownership, CPU,
TLB, wake, ABI, and product-boot models rather than quietly reducing their
state depth. The complete registered inventory is the nightly qualification
lane. Any changed model is still run directly by `dev-plan` before either
profile; a PR pass is not a claim that unrelated nightly models were explored.
The `tlc_max_wall_seconds` profile budget fails closed rather than accepting a
partially explored model.
An exact PR pass may be reused for at most 24 hours. `tlc_cache.py` rejects it
unless the TLA module, CFG, pinned TLC version and digest, deadlock policy,
worker policy, fingerprint, seed, positive exploration metrics, and model name
still match. A source or policy change therefore reruns only the affected
model. The SMP iteration profile applies the same rule to its smaller model
set; nightly qualification never reuses TLC evidence. This is proof-evidence
reuse, not TLC state recovery, and no artifact is touched to fabricate
freshness. TLC's `-depth` option controls random simulation rather than the
depth of exhaustive model checking, so the PR lane does not use it to truncate
the state graph; see the official [TLC tool options](https://github.com/tlaplus/tlaplus/blob/master/general/docs/current-tools.md).
`verify-all.sh` emits a profile verification-run seal only after every selected
gate succeeds. That seal hashes the complete source tree and every normalized
gate/TLC artifact; commercial evidence rejects a stale, partial, or mixed-source
run. KVM product traces separately bind the exact source tree, RustOS boot
image, and verified DVM manifest used for the observed run. An optional stale
KVM trace is classified as stale and excluded from the formal seal; a required
KVM trace still fails until a fresh run replaces it.
`proof-assumptions.tsv` explicitly lists the assembly, hardware, boot, DMA,
toolchain, external-kernel, observability, hypervisor, physical-hardware, and
side-channel assumptions below those proofs. `verified-configurations.tsv`
limits each evidence claim to one exact platform/topology. The proof-boundary
checker rejects missing assumption classes, unknown references, unsealed
profiles, and any attempt to inherit QEMU evidence into physical hardware.
`check-performance-contracts.sh` is the source-drift gate for the shared boot,
frame, and typed IPC limits. During SMP qualification it also rejects restoring
an independent guest boot deadline before the runtime failure path is stable.
It rejects unclassified compat service calls,
service-registration retry amplification, a stable endpoint lookup that takes
the global writer lock, synchronous policy IPC in frame/present code, unbounded
foreground VFS recovery, and unbounded outer KVM execution.
`check-rust-source-contracts.py` binds every critical/high Rust source in
`contracts.toml` to a leading owner/boundary/lifecycle/concurrency/failure
contract. It prevents undocumented unsafe/ordering debt and files over 1300
lines from growing through explicit debt registries, and rejects unresolved
production markers or unexplained `dead_code` allowances.
`check-smp-source-assumptions.py` separately rejects mutable kernel statics,
raw APIC-ID indexing, CPU-zero identity fallback, undocumented unsafe
`Send`/`Sync`, stale BSP-only runtime contracts, and scalar regressions of the
registry-declared per-CPU authority set. Its normalized result is required by
the PR, nightly, and bounded SMP-iteration evidence profiles.

## Run the PR suite

Java 11 or later plus curl and sha256sum are required. The runner fetches the
TLC jar named in [tla2tools.lock](tla2tools.lock), verifies its SHA-256, and
stores it outside the worktree. TLC state files also stay in a temporary
directory.

    bash formal/run-all-tlc.sh --profile pr

The full formal gate also runs the Rust implementation proofs:

    bash formal/setup-kani.sh   # once per pinned Kani version
    bash formal/setup-verus.sh  # once per pinned Verus release
    bash formal/setup-herdtools.sh # once, with documented OCaml prerequisites
    bash formal/verify-all.sh --profile pr

For an iterative multi-vCPU debugging boot, use the bounded exact-tree SMP
profile instead:

    bash formal/verify-smp-iteration.sh
    cargo xtask kvm-smoke --timeout 30 --rustos-vcpus 2 --smp-iteration

This profile is not release evidence. It covers source conformance and the
registry-declared high-risk SMP model subset, caps each model at 30 seconds,
and is mechanically rejected by FPS, recovery, and physical-GPU gates.

The scheduled/manual nightly tier changes TLC fingerprint and seed, retains
single-worker reproducibility, adds fixed-seed long-trace simulation only for
registry-selected models, and runs Miri, Apalache, TLAPS, and bounded
Rust/C libFuzzer campaigns:

    bash formal/verify-all.sh --profile nightly

Set up its pinned optional tools with `setup-miri.sh`, `setup-fuzz.sh`,
`setup-apalache.sh`, and `setup-tlaps.sh`. Tool archives are hash checked and
installed below user caches; no setup script changes host packages.

`PROOF-INFRA.md` records the evidence boundary and the rule for accepting a
counterexample as an implementation bug. Do not treat a solver limitation or
an unmapped model trace as a source defect.

Run an individual model with:

    bash formal/run-tlc.sh endpoint-registry/EndpointRegistry

The runner uses TLC's automatic local worker count and fixed PR fingerprint and
seed. Worker scheduling can change exploration order; use `TLC_WORKERS=1` for
a serial reproduction. Deadlock handling is per-model in `models.tsv`:
`check` requires a deadlock-free state graph, while `intentional-terminal`
must name why a finite protocol is allowed to stop and passes TLC's `-deadlock`
flag (which disables deadlock reporting). TLC expression coverage is retained;
in TLC 1.7.4 a `0:N` action was evaluated but produced no new state, so only an
evaluation count of zero is a coverage failure. Logs, normalized summaries,
and counterexamples are retained under `build/formal/`.

## Models and required properties

| Model | Concrete owner | Required safety properties |
| --- | --- | --- |
| authority-identity-lifecycle/AuthorityIdentityLifecycle | task IDs, process generations, local open-description tokens, prepare handles, exec tickets | allocation is nonwrapping; revoke permanently separates stale and live identities; exhaustion rejects allocation or retires the slot instead of aliasing a stale authority |
| root-authority-publication/RootAuthorityPublication | rootd bootstrap owner, service endpoint registry, process-owned IPC endpoints | the first successful rootd owner is sealed for the boot; a foreign process cannot reclaim the root namespace after revoke/exit; every non-root publication owns its endpoint and commits only under the exact rootd epoch that authorized it |
| service-call-authority/ServiceCallAuthority | service lookup, raw IPC call syscalls, process-owned endpoints | numeric endpoint IDs are routing identifiers rather than ambient authority; lookup grants one exact process and publication epoch; revoke/republication invalidates stale grants; process exit clears grants; unpublished generic endpoints remain owner-only |
| runtime-control-authority/RuntimeControlAuthority | runtimed Unix control socket, netd SO_PEERCRED, signed launch registry | request bytes never assert identity; the kernel-stamped peer PID must be the current uiserver endpoint owner or a live logical-admin launch; UI readiness is uiserver-only; service revoke and process exit withdraw authority before dispatch |
| deferred-process-activation/DeferredProcessActivation | loaderd deferred spawn and kernel process broker | a suspended target is bound to the exact kernel-stamped requester; loader restart preserves the binding; activation consumes it once; foreign use, replay, and requester-exit orphans fail closed |
| atomic-process-activation-batch/AtomicProcessActivationBatch | initd cohort policy, loaderd sender binding, kernel process broker, and kernel-ps scheduler | one 1..=8 unique cohort is completely shape/capability/context preflighted before publication; rejection changes no member; success consumes every one-shot capability while the complete cohort remains suspended, then publishes every runnable sibling in one registry-to-scheduler critical section; FIFO first turns drain before the loader reply resumes; requester exit revokes the still-suspended cohort |
| cpu-affinity-observation/CpuAffinityObservation | HAL CPU lifecycle, kernel-compat Linux/Windows syscall boundaries, syscalld affinity policy, and shared ABI | only a versioned kernel-stamped nonempty bounded Online bitmap whose popcount matches and, for Linux, an exact same-process effective task mask may become topology ABI; stale, forged, empty, oversized, foreign-owner, or reserved-bearing observations publish nothing |
| task-affinity-lifecycle/TaskAffinityLifecycle | kernel-ps scheduler affinity owner, Linux/Windows compat adapters, syscalld policy, and winsys exports | Linux thread and Windows process/thread masks remain nonempty Online subsets; process mutation is atomic across live threads; excluded running CPUs must migrate before user dispatch; previous-mask, fork inheritance, exec preservation, pseudo-handle admission, and current-processor observation remain exact |
| loader-request-authority/LoaderRequestAuthority | initd identity publication, loaderd ingress, process commit and exec-target brokers | privileged spawn is rootd/initd/sessiond-only and exec replacement is procd-only; both ingress and terminal ring0 commit require the current kernel-owned service identity, so guessed PIDs and service restart/revoke cannot retain authority |
| boot-storage-handoff/BootStorageHandoff | hostd storage admission/supervision plus durable VFIO lease | whole-device and every partition must be idle before bounded host flush; host-driver and VFIO authority are exclusive; VFIO assignment requires a durable binding to the exact signed epoch identity; DVM launch requires a durable exact runtime record and live generation-bound aperture; readiness binds that exact generation and epoch identity; active recovery observes the exact QEMU process exit before aperture revoke; repeated revoke preserves the signed immutable read-only flag while clearing all live state; the host driver is restored only after revoke and failure retains quarantine |
| commercial-service-envelope/CommercialServiceEnvelope | shared commercial ABI, service handlers, and exact-response clients | malformed requests receive an explicit error instead of dispatch or abandonment; only an exact request and fully bound response may become authority; foreign, truncated, reserved, and oversized replies fail; timeout and peer-close are explicit terminals |
| zero-trust-service-flow/ZeroTrustServiceFlow | kernel-stamped IPC sender, every published service ingress, object owner, exact-response caller | every hop independently validates shape and authority; direct subjects bind to the exact sender; delegation requires a live service owner on every request; stale capability/generation cannot mutate; only an exact bound response succeeds |
| zero-trust-subsystems.tsv | boot/image parsers, user memory, service authority, every DVM shared-memory consumer, DVM vsock control, host QMP, every local control socket, network frames | every inventoried ingress has explicit shape, authority, lifecycle/revoke, registered model, and executable source evidence; new `dvm_*.rs` consumers, host-side vsock/QMP readers, or service socket listeners fail selftest until added |
| entropy-broker-boundary/EntropyBrokerBoundary | boot entropy admission, boot-random master stream, compat broker, syscalld/netd policy | absent/zero entropy cannot initialize; only authorized policy services receive bounded copies; child streams derive from private master output and never public PID/TID/counter state |
| early-system-admission/EarlySystemAdmission | signed Multiboot2 module, boot-protocol fixed table, io-manager bootstrap reader, xtask staging | exactly one well-formed module declares the complete bounded bootstrap set; only declared digest-valid payloads load; missing, duplicate, malformed, undeclared, or corrupt content fails closed; DVM storage publication waits for the complete bootstrap set and never requires a native storage probe |
| dvm-volume-io/DvmVolumeIo | vfsd/storage-fat provider admission, DVM volume requests, and io-manager transport dispatch | foreign, unsupported, zero, unknown-flag, or byte-length-overflowing provider geometry never mounts; malformed FAT images fail before volume publication; the fixed rings and slots fit one 8-MiB power-of-two PCI BAR with an inaccessible reserved tail; empty, unaligned, overflowing, and out-of-range requests never dispatch; the exact 64-KiB storaged bulk-read reply is admitted only when all request/range/generation/length bindings match and it reuses read authority; configured read, mutation, and flush failures publish no request authority; chunk accounting is exact; timeout and device revocation remain distinguishable from transport failure |
| dvm-read-cache/DvmReadCache | storaged bounded DVM read-ahead cache | only an exact live generation and covered range may hit; misses fill at most eight non-overlapping 64-KiB windows; another generation atomically replaces the cache epoch; write and restart clear every window before completion |
| remote-file-mapping/RemoteFileMapping | loaderd prepared mappings, kernel-compat file copy, vfsd early-system/DVM ownership, VFS IPC v4 | source ownership is selected before applying its transfer bound; early-system reads remain 4-KiB broker chunks, DVM-volume replies remain within the exact maximum inline response, immutable-owner loss cannot fall through, and only an exact byte count may commit |
| syscall-simd-lifecycle/SyscallSimdLifecycle | syscall entry/exit and kernel-ps scheduler continuation state | the entering task owns a distinct user SIMD/FPU snapshot and live syscall frame; blocking publishes one scheduler continuation without releasing the syscall frame; resume consumes that continuation; nested capture and foreign-task restore are rejected; canonical-address/RFLAGS validation occurs after the last possible resume; return restores the exact entering image |
| syscall-scheduler-continuation/SyscallSchedulerContinuation | composed syscall and scheduler frame lifecycle | a running/armed syscall owns a consumed scheduler frame; only atomic block publication creates a resumable frame; raced wake changes only its token; blocked wake retains the published frame until exact-owner dispatch consumes it; the syscall frame remains live through the composition and SYSRET follows post-resume validation |
| pci-bar-discovery/PciBarDiscovery | kernel-hal standard PCI BAR discovery and resource publication | command decoding is disabled during sizing; each BAR dword is restored before its 64-bit partner is probed; decoding occurs only from the restored pair; the least significant implemented mask bit defines size; every terminal restores BARs and command state |
| runtime-control-rpc/RuntimeControlRpc | `libs/runtime-control` request/reply client | only an exact successful opcode is admitted; snapshot payload count is bounded; non-snapshot success is payload-free; malformed statuses fail closed |
| dual-abi-image-admission/DualAbiImageAdmission | loaderd plus `rustos-image-admission` | ELF64 and PE64 plans share one bounded, non-overlapping W^X gate; a main entry must belong to executable memory; only an entryless PE DLL may use entry zero; rejected plans never map |
| dual-abi-byte-parser/DualAbiByteParser | loaderd plus `rustos-image-admission` | a bounded ELF64/PE64 header, table, relocation and import parse must settle before mapping; rejected or subsequently mutated snapshots never map |
| page-table-lifecycle/PageTableLifecycle | compat MM broker and `kernel-mm` process address spaces | broker ranges are canonical, non-wrapping, and page-rounded before mutation; only live user frames map into user pages; every map/protect/unmap preserves W^X and removes unmapped access authority |
| page-table-map-transaction/PageTableMapTransaction | `kernel-mm` intermediate table publication and rollback | failed mapping reverses the exact commit log, restores prior topology, performs one shootdown, and only then frees transaction-owned frames |
| physical-frame-lifecycle/PhysicalFrameLifecycle | `kernel-mm` boot physical-frame allocator | only aligned firmware-usable frames below the direct-map ceiling enter the free set; kernel/module reservations are monotonic before allocation; allocation and exact release preserve single ownership; invalid/double release and exhaustion fail without minting capacity |
| service-heap-lifecycle/ServiceHeapLifecycle | `rustos-svc-runtime` allocator, syscalld VM policy, xtask KVM health oracle | dropped service allocations return exact spans to an address-ordered coalescing free set; growth occurs only after no reusable span fits; Linux mapping hints wrap to released gaps; allocation and fatal core-service failures are explicit failed runtime evidence |
| process-address-space-lifetime/ProcessAddressSpaceLifetime | `kernel-ps` process table and `UserProcessState` | every state/address-space access holds one retained process reference and the per-process mutex; exit freezes the address-space epoch, stale exec cannot clear it, a prepared thread attachment cannot publish after exit and must release its unpublished stack, and reclamation waits for all authority to disappear |
| futex-waiter-lifecycle/FutexWaiterLifecycle | Linux futex scheduler substrate | a task owns at most one bounded waiter and original identity; requeue changes only its active key; keyed wake, key-independent timeout/spurious wake, and every retirement leave one explicit terminal outcome; the exact 24-byte robust-list ABI is snapshotted per thread, traversed at most 2048 entries, includes the pending operation, marks only words still owned by the retiring task as owner-died, and wakes an existing waiter before slot reuse |
| kernel-resource-accounting/KernelResourceAccounting | IPC endpoint/shared-region allocation and process task admission | process and task owners reserve endpoint quota before queue allocation; process-owned regions have object and byte ceilings plus a global byte ceiling; dropped backing remains charged through deferred physical reclaim; one process cannot consume the global scheduler task table |
| process-signal-delivery/ProcessSignalDelivery | procd policy, HAL fault handoff, scheduler job-control gate, child wait state, and ring0 signal substrate | ring0 consumes only a still-pending unmasked signal; SIGKILL can only terminate and SIGSTOP can only stop; SIGCONT resumes the complete process before disposition; `WUNTRACED`/`WCONTINUED` consume exact child state; invalid targets and stale policy replies cannot redirect execution; recoverable faults retain authority while fatal final-thread faults revoke it |
| sigchld-notification/SigchldNotification | procd `SA_NOCLDSTOP` policy and ring0 process-directed SIGCHLD cause substrate | exit, stop, and continue causes coalesce in a bounded snapshot; stop/continue-only causes may be suppressed, exit never is, and ring0 removes only the causes still present after policy selection so concurrent causes remain pending |
| netd-deferred-reply/NetdDeferredReply | netd AF_UNIX deferred poll queue | the global reservation includes mutex-queued and worker-detached batches; admission stays bounded and each accepted request makes exactly one terminal reply attempt, including queue poison failure |
| memfd-seal-lifecycle/MemfdSealLifecycle | `kernel-ps` memfd object | atomic seal installation respects `F_SEAL_SEAL`; write sealing requires zero writable mappings; both truncate and EOF-extending write respect grow/shrink seals; mapping counters remain bounded |
| msi-vector-lifecycle/MsiVectorLifecycle | kernel HAL MSI allocator | allocation creates an unpublished exact lease; only that lease may bind one handler, failed MSI-X setup clears the exact handler before returning the slot, and only a fully programmed APIC-ready lease commits a permanent route |
| acpi-table-admission/AcpiTableAdmission | kernel HAL ACPI/MCFG/HPET parser | RSDP and SDTs have strict size/checksum/signature/entry-width bounds; MCFG regions publish atomically only when every ECAM range is aligned, mapped, bounded and non-overlapping; invalid firmware publishes no partial ECAM or HPET authority |
| cpu-topology-admission/CpuTopologyAdmission | kernel HAL MADT admission | a complete bounded unique fixed-CPU set containing the executing BSP is assigned dense logical indexes, normalized with the BSP at logical zero, and published atomically; malformed or unsupported topology publishes no CPU authority |
| cpu-online-lifecycle/CpuOnlineLifecycle | fixed low-memory AP trampoline, kernel HAL logical CPU/xAPIC/GDT/TSS state, kernel-ps syscall CPU-local state, and nucleus lockdep identity | the BSP serializes an RX trampoline and generation mailbox through bounded INIT-SIPI-SIPI and OnlineParked acknowledgement, then retires startup pages R/NX; every CPU has disjoint bootstrap/GDT/TSS/RSP0/IST and GS/syscall storage, while dense APIC identity selects distinct lockdep slots; failed CPUs own no dispatch authority |
| smp-reschedule-ipi/SmpRescheduleIpi | kernel-ps CPU-local task ownership and reschedule request state, kernel HAL fixed IPI, and low-level interrupt dispatch | a 0→1 durable per-target request emits one exact fixed IPI; repeated requests coalesce, raw-lock interruption acknowledges without clearing work, and only a same-CPU safe point consumes and dispatches it |
| scheduler-cpu-ownership/SchedulerCpuOwnership | nucleus lockdep CPU/APIC guard ownership plus kernel-ps CPU-local current-task and IRQ dispatch gates | pending acquisition units convert to held guards or cancellation in local-IRQ atomic transitions; total depth equals pending plus held units, and a published guard pins the exact task and CPU until same-CPU release, while accounting disagreement, guarded dispatch, migration, cross-CPU release, and underflow fail closed |
| bootstrap-activation-handoff/BootstrapActivationHandoff | kernel-ps supervisor-committed child first-turn handoff | every exact activation occupies one deduplicated entry in a fixed `MAX_TASK` FIFO; later activations and IPC replies cannot overwrite it, stale children are removed without reordering survivors, strict-System recovery or a fair-share turn preserves the queue, and the oldest live child receives the next eligible first turn |
| tlb-shootdown-lifecycle/TlbShootdownLifecycle | kernel HAL CR3/active-root registry and generation mailbox plus kernel-mm page-table mutation guards | activation and mutation serialize, only eligible CPUs running the affected root become targets, every target flushes before publishing its exact generation acknowledgement, and frame reclaim remains forbidden until all acknowledgements arrive |
| cross-cpu-task-retirement/CrossCpuTaskRetirement | kernel-ps exec/exit task barrier, per-CPU running ownership, and kernel HAL address-space reclaim admission | exec seals thread attachment, makes the exact target undispatchable without detaching it, retires siblings only after they leave remote CPUs, advances one process generation, and permits reclaim only after no CPU owns the old execution and every shootdown target acknowledges |
| robust-futex-owner-death/RobustFutexOwnerDeath | kernel-mm atomic user-u32 access and kernel-compat Linux robust-list/clear_child_tid cleanup | aligned writable admission precedes an acquire load; OWNER_DIED is published with bounded AcqRel compare-exchange retries while preserving WAITERS, clear_child_tid uses a release zero store, and wake happens only after atomic publication |
| per-cpu-clockevent-lifecycle/PerCpuClockeventLifecycle | kernel HAL invariant-TSC local APIC deadline programming and kernel-ps timer dispatch | every AP programs and arms its private fixed vector before Online, re-arms a strictly future deadline at interrupt entry, performs one CPU-local scheduler turn, and sends local APIC EOI without touching the BSP PIT |
| persistent-mutation-admission/PersistentMutationAdmission | vfsd persistent-volume dispatch | the current writable-feature constant is false, so journal/recovery placeholders cannot authorize persistent mutation; volatile `/run` policy never advances persistent state |
| dma-iommu-isolation/DmaIommuIsolation | L0 hostd plus IOMMUFD/VFIO DVM assignment | device ownership is exact, mappings remain in the assigned DVM aperture, revocation removes mappings, and the finite map set stays bounded; ring0 owns no physical-storage DMA domain |
| filesystem-content-integrity/FilesystemContentIntegrity | signed early-system table plus bounded kernel bootstrap reader | only an exact allowlisted payload matching its digest verifies; corrupted content fails closed and missing bootstrap state terminates the read |
| network-payload-session/NetworkPayloadSession | DVM Ethernet transport plus netd | only bounded ARP/IPv4 payloads from an active authenticated epoch are delivered; malformed frames are dropped while advancing the sole consumer cursor |
| scheduler-cpu-distribution/SchedulerCpuDistribution | `kernel-ps` scheduler | each CPU owns its System/User burst, virtual timeline, and bounded handoff FIFO; two local System turns reserve that CPU's next ordinary User turn, ordinary User selection admits only a least-runtime fair peer, idle stealing and staggered one-task active balancing transfer through single-owner mailbox custody, process-owned server boost targets the worker's actual CPU, repeated rehome generations coalesce by slot, and only System recovery has an absolute ready-age rail because User wall-clock guarantees require explicit bandwidth admission |
| scheduler-thread-demotion/SchedulerThreadDemotion | `kernel-ps` scheduler, uiserver helper threads, loaderd, and vfsd | self-demotion cannot discard a live reply-scoped donation; completion-bound bootstrap servers cannot demote before their exact terminal UI reply; untrusted or blocking UI helpers lose inherited System class before entering their loops, while input/present authority remains explicit |
| synchronous-ipc-handoff/SynchronousIpcHandoff | kernel IPC call/reply transfer and `kernel-ps` scheduler handoff FIFO | every call enqueue retains its exact required receiver and every successful reply retains its exact caller without overwrite; dispatch is FIFO before unrelated overdue work, stale retired peers alone are removed, and a bounded synchronous burst forces one ordinary fairness turn without consuming the queue |
| ipc-reply-recv-transaction/IpcReplyRecvTransaction | kernel compat reply-receive ABI, service runtime decoder, inputd fused loop, and loaderd selective fused loop | every caller-controlled range is checked before reply consumption; successful commit wakes the exact caller before check-arm-recheck receive; pre-commit and tagged post-commit errors are disjoint; blocked receive retains exact waiter custody; malformed dequeued input or loader requests receive a terminal error; loaderd never blocks before reply-dependent descriptor cleanup or class demotion |
| rootd-bootstrap/RootdBootstrap | rootd, loaderd, IPC endpoint wait | core dependency gate before initd; exact PID lease; endpoint/capability lifecycle; a five-second endpoint deadline retires an unready child before bounded restart; single initd launch |
| service-bootstrap-lifecycle/ServiceBootstrapLifecycle | rootd raw entry and helper handoff, kernel process retirement, initd dependency lookup | raw process entry aligns the stack before ordinary Rust; a non-final worker exit preserves process-owned authority; initd authorization is derived from the bootstrap manifest; only an unpublished endpoint is retryable, while undeclared or malformed lookups terminate |
| post-init-bootstrap-barrier/PostInitBootstrapBarrier | initd independent foundation activation and consumer dependency barrier | netd/devmgrd/inputd may initialize while later loader work proceeds, but spawned children remain absent from dependency authority until exact PID-bound endpoint admission; runtimed/storaged start only after the complete live barrier |
| endpoint-registry/EndpointRegistry | kernel compat IPC registry, rootd capability decision | publication is capability-complete; revoke/exit leave no authority; exact-PID wait cannot succeed on stale or foreign state |
| endpoint-receiver-wakeup/EndpointReceiverWakeup | kernel IPC endpoint slot, compat blocking receive, scheduler wake token | pending-message observation and waiter publication are one endpoint-slot decision; a pending fast path publishes no waiter, a blocked receiver owns one exact waiter, and a producer atomically consumes that authority when waking it |
| endpoint-publication/EndpointPublication | kernel compat IPC registry, process-table exit marker | writers and reader snapshots share one registry critical section; an exit marker aborts in-flight publication; lookup/capability authority needs one exact running-owner generation; cleanup leaves no terminal authority |
| exception-retirement-lifecycle/ExceptionRetirementLifecycle | HAL exception entry, user-fault policy, scheduler retirement | every general exception aligns the first ordinary Rust call; recovery preserves live authority; non-final retirement drops only task-local waits; final retirement also revokes process endpoint authority |
| deferred-start/DeferredStart | loaderd, rootd, initd, runtimed | suspended child is inert; only its designated supervisor admits it; activation is single-use; endpoint follows activation; activation/timeout failure retires the exact child or stops the unhealthy supervisor before another launch |
| post-init-leases/PostInitLeases | rootd post-init readiness and restart policy | only initd or the live sessiond/runtimed service may report its child; the report must prove ring0's exact unconsumed deferred-spawn binding and the declared executable path; capability and lookup authority require a complete live reporter chain; reporter exit revokes the bounded descendant closure in the same rootd turn; a foreign live-lease rebind preserves PID/reporter/capability; exact PID idempotency; no capability before report; restart budget never underflows |
| rootd-restart-backoff/RootdRestartBackoff | rootd core-service recovery, lifecycle fan-out, and timer substrate | exit revokes old authority; external time cannot be starved by unrelated lifecycle work and every pending restart eventually settles or fails the supervisor closed; activation failure retires the exact suspended child before another retry; rootd, procd, and syscalld drain independent bounded queues without cross-service lookup; root evidence overflow is terminal while each policy overflow clears only its owning cache before rebase; retry budget is finite and monotonic |
| post-init-supervisor-recovery/PostInitSupervisorRecovery | rootd/initd post-init recovery and dependent UI revocation | normal initd exit revokes the complete reporter closure before replacement; a defensive recovery cut adopts only an exact ready endpoint; external monotonic time cannot be starved by repeated sibling recovery, every imported recovery eventually adopts or reclaims, and reclaim clears all process/endpoint authority while cascading from sessiond to uiserver |
| dvm-control-relay/DvmControlRelay | L0 hostd, Linux DVM agent, RDI3 input receiver | launch-bound CID and exact HELLO issue a fresh challenge; only its HMAC proof permits WELCOME; allowlisted probes, stale/mismatched replies, and replay fail closed; a completed probe gates a fresh relay epoch |
| dvm-control-endpoint/DvmControlEndpoint | L0 hostd/xtask, Linux DVM agent | only the root-only launch-secret holder derives the per-launch vsock listener port; a same-CID untrusted process cannot reserve setup; a reached endpoint still requires the separate HMAC proof |
| dvm-agent-readiness/DvmAgentReadiness | Linux DVM control agent and init owner | local health requires an initialized live serving process holding the atomically installed exact ready inode; stale, malformed, symlinked, partial, announced-only, or post-exit state fails closed, and one fixed candidate bounds crash residue |
| dvm-network-ring/DvmNetworkRing | DVM network ivshmem mapper, Linux Ethernet relay, netd substrate | only a host-validated fixed header installs the aperture; DVM counters are bounded before either kernel cursor advances; malformed/forged RX work is rejected without delivery; DVM header mutation cannot alter installed bounds |
| dvm-network-control/DvmNetworkControl | L0 RDI1 lifecycle, fixed input-ring receiver, DVM network gate | aperture mapping alone has no authority; only a fresh authenticated control epoch permits RustOS network access; exact end revokes it; stale end, epoch reuse, and DVM data writes cannot alter a replacement lease |
| dvm-input-revocation/DvmInputRevocation | RDI3 receiver, ring0 ingress queue, inputd, keyboard-core | every one-shot epoch start/end is a priority reset barrier; queued keys are bound to the current epoch; no key is delivered before its epoch reset; inputd releases all retired provider key state |
| dvm-input-ring/DvmInputRing | L0 producer, fixed ivshmem ring, RustOS MSI-X leaf, inputd broker | only L0 advances producer and only the broker advances consumer; 2,048 fixed 32-byte slots retain cleanup reserve; continuous production requires both an armed vector and a successful policy-backed client poll; DVM tamper attempts cannot mutate the ring; IRQ only wakes; malformed/stale records cannot reach inputd; revoke clears transport/consumer readiness and decoder authority; a boot-wide bounded recovery budget reuses one pinned vector; fairness drains or revokes finite committed work |
| dvm-transport-lifecycle/DvmTransportLifecycle | display/input shared transport lifecycle | exact-epoch claims admit only Active state; drain closes new admission and reset/revoke waits for zero in-flight claims |
| dvm-block-transport/DvmBlockTransport | `driver-domain-protocol` ABI-v2 block transport, kernel signed-epoch admission/rebind, Linux storage-DVM relay, and storaged frontend | fixed request/completion queues remain bounded; records are launch-generation bound and contain no addresses; completions bind exact request identity; reads cannot invent durability; successful FUA and FLUSH completions report the exact stable operation; restart revokes queued authority and only a newer L0-signed epoch can be readmitted |
| dvm-block-startup/DvmBlockStartup | signed early-system key, conditional RustOS-ready publication, `kernel/io-manager` readiness predicate, compat block waiter, storage-DVM publication, and storaged first generation query | a peer race between verification and ready publication fails without mutating readiness; initial provider-not-ready remains sleepable; readiness publication before check, during registration, or after sleep cannot be lost; only an observed exact signed generation permits volume use; timeout and revoke are explicit terminal outcomes and never select bootstrap storage |
| trusted-ui-boundary/TrustedUiBoundary | DVM display/input provenance, GUI backend, uiserver trusted-UI status | a privileged prompt requires independently attested scanout and human input; DVM compromise, provider loss, or independent-attestation revocation cancels it; a DVM transport may never self-attest |
| input-readiness/InputReadiness | ring0 ingress queue, finite poll substrate, inputd worker, uiserver reader | the MSI-X worker, bounded STATS readiness recheck, and readiness-gated read are explicit transfer races; every record has exactly one ring0, policy, or delivered owner and service policy drains under consumer fairness |
| userspace-wait-set/UserspaceWaitSet | vfsd epoll registry, syscall-scoped poll sets, netd/inputd/sessiond readiness providers, compat wait-token and deadline substrate | each poll/epoll wait registers exact observed generations, rechecks providers while runnable, arms the scheduler, and verifies waiter presence before commit; readiness is re-queried before return and each service query is bounded by the application deadline; registration identity excludes provider epoch so MOD can rebind after downstream-provider restart without duplicate/undeletable interests; timeout, unmasked-signal interruption, revoke, dup/fork/close/exec, and last-reference retirement settle without a lost wake; vfsd restart composition is covered by `service-mutation-recovery` and still requires runtime crash injection |
| service-mutation-recovery/ServiceMutationRecovery | rootd-retained vfsd checkpoint, netd replay ledger, compat reconciliation queues | unique operation IDs advance one revision at most once; committed state survives service crash; replacement endpoints publish only after local state equals the retained checkpoint; an unresolved commit remains explicit until reply/reconciliation |
| vfs-open-description-recovery/VfsOpenDescriptionRecovery | rootd-retained vfsd open descriptions and compat user-copy settlement | an open becomes live only after every path chunk is durable and an uncertain open is cancelled by its proposed capability; sequential read/getdents cursor advances remain prepared until kernel copyout commits or cancels them; close tombstones survive response loss and may be compacted only after an explicit visibility ACK |
| input-ingestion-worker/InputIngestionWorker | ring0 DVM input wake leaf and inputd ingestion worker | only the capability-gated worker may claim policy-consumer readiness or advance committed records; wake/arm races are cursor-rechecked, every turn is bounded, and finite work eventually drains without granting drain authority to client poll |
| ui-frame-budget/UiFrameBudget | uiserver input loop, console-command worker, frame/present loop | console-policy IPC has bounded FIFO admission and one delivery owner; overload is recorded; an in-flight policy call cannot make local redraw debt wait; active-input feedback is eventually presented |
| ui-main-loop-wakeup/UiMainLoopWakeup | uiserver service-owned readiness generation, thread parker, and scheduler deadline wake | check-generation-recheck closes notification coalescing races; block commit and software reschedule are atomic; either an input/Wayland generation or the bounded deadline returns control to the UI loop |
| wayland-accept-isolation/WaylandAcceptIsolation | uiserver readiness-gated bounded accept worker and netd-backed socket path | accept begins only after a listener readiness publication; a stalled cross-service accept never owns the UI tick, accepted streams cross one bounded completion queue, overload closes the new client, and queued clients eventually settle under the declared worker/UI fairness |
| wayland-frame-pacing/WaylandFramePacing | uiserver main present loop and Wayland frame callbacks | input and Wayland damage are consumed by one coalesced presentation; a callback requires one previous-presentation or callback-only cadence permit; permits do not accumulate; pending damage and callbacks eventually progress under named fairness assumptions |
| ui-input-motion/UiInputMotion | DVM KVM input selftest and uiserver present loop | the test pointer reverses both axes before permanent edge clamping; every initial cursor position yields bounded visible work, and every accepted visible-motion epoch is eventually presented |
| dvm-input-selftest/DvmInputSelftest | DVM KVM selftest evdev selection, bounded guest scheduler admission, and host input relay | the synthetic composite device cannot emit without absolute pointer capability plus a verified SCHED_RR/RLIMIT_RTTIME guard; limit installation and scheduler admission are separate, rollback/restore must settle, and uncertain restore terminates all process authority; exactly one non-printable keyboard probe precedes pointer-only cycles; every generated position consumes one non-accumulating monotonic cadence permit and produces one pointer position |
| dvm-absolute-pointer/DvmAbsolutePointer | DVM evdev agent, L0 RDI3 relay, ring0 decoder, inputd, uiserver | partial axis reports never publish; duplicate complete positions are idempotent; every position has exactly one pipeline owner; absolute coordinates stay bounded and cannot cause phantom UI motion; finite accepted work drains under fairness |
| devmgrd-sessiond-isolation/DevmgrdSessiondIsolation | devmgrd receive loop and bounded sessiond ioctl workers | sessiond ioctls have bounded FIFO admission or EAGAIN; worker stalls never own devmgrd's receive loop; unrelated device traffic remains replyable |
| vfio-release-authorization/VfioReleaseAuthorization | hostd release gate, durable VFIO lease | no topology preflight can bind a device; a pinned-key signature binds the exact group, CID, DVM artifact, and device policy; every bind mutation is within the signed validity window; restore leaves no authority |
| dvm-commercial-lifecycle/DvmCommercialLifecycle | hostd physical display-DVM supervisor and VFIO recovery | VFIO binding requires a completed reversible runtime preflight including IOMMUFD, at least 4 GiB soft memlock, live proof that no L0 boot/connected display is assigned, reset impact contained by the admitted lease, DMA-safe vfio-pci idle-power configuration, and a validated host VFCT image relocated with a valid checksum to the fixed guest BDF without changing its VBIOS payload; mutable launch inputs cannot authorize a child; a child exists only behind a durable exact lease, complete VFIO group, pre-launch reset, non-identity IOMMUFD, a fixed 2 GiB guest-memory profile, private ACPI VFCT supply, and runtime identity; readiness needs authenticated control; normal stop requires QMP capability negotiation, ACPI powerdown, and observed QEMU exit because command acceptance alone is not evidence; recovery signaling requires an exact pidfd and rejects numeric PID reuse; forced TERM/KILL is never accepted as a successful run; restoration follows observed child exit and post-stop reset; reset failure retains quarantine; physical network and block stay disabled |
| dvm-release-bundle/DvmReleaseBundle | DVM artifact writer, safe staging, xtask and hostd admission | exactly one strict 25-key manifest, one strict six-key control contract, and the other six co-located payloads must verify before atomic publication; unsafe/mutable paths, missing or corrupted companions, replacement, and post-publication mutation cannot grant launch authority; hostd independently reverifies and snapshots the published bundle |
| dvm-display-driver-supply/DvmDisplayDriverSupply | pinned Linux DVM physical-display package and boot admission | KMS requires the exact open-module/GSP release selected only by a kernel PCI modalias plus a bound kernel-enforced module-signing certificate; UVM/compute authority is absent; relay readiness follows complete KMS initialization; firmware distribution requires an independent authorization and cannot mix releases |
| dvm-amdgpu-supply/DvmAmdgpuSupply | AMD `1002:1900` DVM VBIOS, module, firmware, and KMS admission | KMS and relay readiness require an exact host PCI identity, an exact-or-absent subsystem pair, a checksummed 0x55aa/ATOM VFCT image relocated to fixed guest BDF with unchanged payload, its owner-private full-table QEMU ACPI snapshot, the signed upstream amdgpu module, its bound certificate, kernel PCI modalias selection, and all thirteen DCN 3.1.4, GC 11.0.1, PSP 13.0.4, SDMA 6.0.1, and VCN 4.0.2 firmware payloads; incomplete or revoked supply remains offline |
| dvm-amdgpu-evidence/DvmAmdgpuEvidence | hostd schema-3 AMD policy and authenticated physical page-flip evidence | admission requires exact host and DVM `amdgpu` `1002:1900` identity plus consecutive fresh zero-copy samples meeting signed frame-rate and latency bounds; wrong identity, a failing sample, or stale sequence revokes readiness |
| driver-domain-fleet/DriverDomainFleet | hostd fleet policy and signed release gate | a fleet member is exactly encoded; CIDs, IOMMU groups, and representative PCI functions are globally disjoint; policy is immutable after sealing; only a signed release bound to that fleet can activate a member |
| ivshmem-pairing/IvshmemPairing | launch-private ivshmem broker and KVM launcher | the RustOS QEMU connection is observed as peer 0 before GUI-DVM launch; peer 1 cannot exist without peer 0; disconnect fails the complete pair closed and no reconnect can reuse the assignment |
| gui-dvm-surface/GuiDvmSurface | RustOS compositor to the sole supported GUI-DVM transport | `RSGUI002` has exactly three host-provisioned slots, exact even PRESENT/RELEASE generations, one outstanding DVM control record, module-latched pre-boot invitations, offline confirmation clearing, saturated-pool re-invitation, and stale-slot reclamation. It asserts bounded backpressure without fabricating capacity. Multi-domain focus is unavailable and rejected by the source rather than modeled as authority. |
| gui-dvm-pixel-authority/GuiDvmPixelAuthority | RustOS surface producer, uiserver, and GUI-DVM consumer | damage-only publication may reuse pixels only from the exact predecessor or a complete contiguous history bounded by the fixed slot count; every slot has one writer/reader owner and monotonic generation, and revoke removes stale pixel authority before a later epoch |
| dvm-atomic-scanout/DvmAtomicScanout | explicit physical-AMD DMA-BUF/GPU/KMS relay mode | source/model matched, hardware gate failed: the complete 128 MiB pixel backing must first be DMA-pinnable and mapped into the VFIO IOAS, then only a coherent DMA attachment may import all three read-only sources; the kernel names the exact oldest live generation in a non-replayable acquire `sync_file`, and EGL server-waits it before composition into a separate three-buffer GBM output pool; GPU and page-flip fences precede source/output reuse; device-write DMA authority to sources is absent; evidence requires the complete chain; offline revokes both pools. Physical import, scanout, and sustained-rate evidence remain required. |
| dvm-gpu-compositor/DvmGpuCompositor | uiserver private scene compiler and Linux DVM fixed GLES executor | a bounded OS-owned context admits only clear, solid-quad, and textured-quad commands with host-bound read-only source tokens; only a measured prime record for the current host-selected epoch enables the asynchronous three-entry queue; acquire, completion, release, and presentation are monotonic fence states; raw commands, application shaders, CPU fallback success, and device writes to RustOS sources are impossible; a 16.667 ms target miss retains the prior front and live epoch, while the separate 50 ms hard timeout or revoke invalidates the full epoch and stale completions cannot revive it |
| dvm-gpu-proof-scheduler/DvmGpuProofScheduler | private AMD/virtio GPU proof process | only the finite post-prime measurement may use bounded SCHED_RR priority 8; limit installation, admission, and exact restore readback are distinct states; it remains below display/input relays; success and ordinary failure restore normal policy before evidence, while hard-limit or uncertain-restore termination publishes no evidence; the health loop has no realtime authority |
| dvm-display-scheduler/DvmDisplayScheduler | authenticated Linux DVM GPU/KMS relay scheduling | only a confirmed host invitation may first install the exact RT bound and then admit the current relay thread to SCHED_RR; display priority remains below input, partial admission cannot run the relay, continuous realtime CPU is capped, and retry is permitted only after exact policy/limit restore readback; hard-limit or restore failure terminates all process authority |
| dvm-display-readiness/DvmDisplayReadiness | Linux DVM GPU/KMS relay and agent health reader | one process singleton owns publication; only a complete locked candidate is atomically installed as ready; ordinary failure withdraws health before scheduler restoration; crash/hard-limit release all readiness authority; one fixed candidate bounds residue |
| dvm-gpu-admission/DvmGpuAdmission | uiserver provider admission, off-UI-thread GPU atlas initialization, and frame cadence | a mandatory DVM topology never reports software fallback as GPU success; CPU presentation remains live while the bounded worker initializes; only a current measured full-atlas/textured-draw prime, exact valid provider stride/mapping, retained scene, and completed first GPU frame promote the consumer; clear-only priming fails closed, each steady frame consumes one non-accumulating timer permit, initialization/first-frame timeout settles, and revoke requires a fresh epoch prime |
| dvm-gpu-atlas-transport/DvmGpuAtlasTransport | uiserver atlas owner, fixed RustOS transport, and display-DVM executor | a registered backend class selects exactly one compatible source mode; prime-completion v2 authenticates that mode and every submit must match it; exactly three imported source slots retain one mapping generation for the provider epoch while frame sequence/content epoch advance; the first update defines the full atlas, later bounded non-overlapping damage or command-only updates execute strictly in order; QEMU staged upload and physical read-only DMA-BUF modes cannot exchange evidence; source reuse requires the GPU fence, old-front reuse requires the later present fence, and revoke/reset removes every outstanding authority |
| gui-dvm-install/GuiDvmInstall | GUI-DVM ivshmem installer in the I/O manager | one serialized installer owns both BAR mappings, two permanent MSI-X vectors, and provider registration. Every malformed, absent, or failed installation releases mappings before terminal rejection; a concurrent caller cannot allocate a second installation; a revoked transport never reopens or falls back. |
| ipc-reply-deadline/IpcReplyDeadline | kernel IPC runtime and compat deadline wait | exact caller/reply ownership; one-shot reply completion; owner exit and deadline clear the waiter; every blocked control cycle carries a finite break; stale or late replies cannot revive authority |
| scheduler-wakeup/SchedulerWakeup | kernel scheduler, current-task block API, timer IRQ | arm/wake/commit uses a fresh epoch; a wake before commit is a token transition and never validates the running task's consumed frame; only non-current ready or blocked tasks own a published resumable frame; blocked tasks own one unexpired timer; timer expiry precedes subsequent dispatch; retired tasks retain no scheduler or timer authority |
| smp-release-admission/SmpReleaseAdmission | KVM RustOS topology admission | one vCPU may launch without claiming SMP; a multi-vCPU launch requires every named high-risk prerequisite plus a fresh versioned PR seal bound to the exact source tree, and release acceptance additionally requires online/idle/user/timer/IPI evidence from every requested CPU |
| clocksource-deadline/ClocksourceDeadline | invariant-TSC/HPET clocksource, PIT clockevent, scheduler sleep identity | elapsed time never derives from delivered RTC-edge count; a delayed event catches every absolute deadline crossed by a clocksource jump; only a calibrated source is admitted; sleep identity is the exact scheduler task id even while syscall code holds the process-table lock |
| scheduler-admission/SchedulerAdmission | runtimed launch-catalog admission | a launch record is not a realtime capability: all non-UI requests are clamped below System admission even when registry input is hostile; only the exact trusted UI executable receives its pinned System weight; pending admission eventually settles |
| ipc-priority-inheritance/IpcPriorityInheritance | scheduler effective classes and compat synchronous IPC | a live reply capability owns the only priority donation; System class propagates through nested calls; completion, cancellation, and task exit revoke it; System work wins until its bounded burst is exhausted, then one ready User turn is mandatory |
| ipc-priority-queue/IpcPriorityQueue | scheduler-derived service endpoint delivery | System calls bypass an ordinary backlog in lane-local FIFO order, ring3 cannot choose the lane, combined admission remains bounded, and two consecutive System deliveries reserve the next queued ordinary head |
| ipc-handle-transfer/IpcHandleTransfer | process handle substrate, IPC runtime, compat IPC syscalls | a transferred descriptor is either installed or dropped exactly once; every exported service-backed description owns a matching service reference which installation adopts or bounded deferred cleanup releases; queue cancellation, peer-close, invalid receiver output, caller exit, and owner exit after dequeue leave no registry entry; batch transfer is all-or-nothing |
| ipc-transfer-authority/IpcTransferAuthority | process handle registry, AF_UNIX channel, netd service epoch, receiver FD table | transfer batches bind exact receiver/service/channel/stream identity, remain invisible through copyout, and settle at one install or release terminal |
| ipc-endpoint-ownership/IpcEndpointOwnership | kernel IPC runtime, compat IPC syscalls, process handle table | a process-owned endpoint/reply may be served by its worker threads but cannot be received, replied to, or handle-drained by a foreign process; transferred handles install before a reply becomes terminal; process exit kills the endpoint, revokes queued/received and installed process-local transfer authority, and cannot be followed by enqueue revival through the dead numeric endpoint; every descriptor installation stays within the process ceiling and a full-table rejection is non-destructive |
| proc-broker-session/ProcBrokerSession | process broker, loaderd, Linux process teardown | exact loader ownership and inherited console-session binding; mapping/runtime state only in a live prepare session; commit attempt is terminal; deferred children stay inert until activation; owner exit aborts every uncommitted or in-flight prepare before publication |
| exec-ticket/ExecTicket | procd, loaderd, process broker, Linux thread/process teardown | exact live PID/TID ticket binding; mismatched cancel/exec cannot consume a ticket; one-shot execution and pre-image register handoff; target-thread exit and exec sibling retirement retain no ticket or transition authority |

The rootd-bootstrap model covers the supervisor transaction for core services
and initd:

1. A core service is created suspended.
2. Rootd admits the exact PID lease.
3. The child is activated.
4. Successful registration publishes the exact-PID endpoint and capability
   together, then completes the endpoint wait.
5. Revocation or exit clears both endpoint and capability bindings.

The atomic registration step is an externally visible contract, not an
assumption about one CPU instruction: kernel compat publishes the
rootd-authorized capability and owner before the endpoint, and effective
capability checks require that endpoint to remain published. Clearing the
endpoint therefore fails both endpoint wait and broker authorization closed.

The checked configuration uses two representative core services, four PIDs,
at most one restart in an execution, and a short timeout. This keeps the PR
model check exhaustive; the TLA+ actions quantify over services rather than
encode a special case for either named service.

The authoritative design contract is
[docs/ai/contracts-abi.md](../docs/ai/contracts-abi.md). The model preserves
the existing boundary: service admission and restart policy are owned by rootd;
the kernel supplies only the narrow endpoint and lease substrate.

`dvm-control-relay` models the narrow host-mediated DVM control path, not a
general hypervisor RPC channel: Linux DVM → L0 over launch-bound KVM-vsock,
then L0 → RustOS over fixed RDI3 input frames. A CID and exact HELLO issue a
single fresh challenge; the model admits control authority only after the
matching HMAC proof. It keeps the existing ownership boundary: L0 validates
DVM identity and relay syntax; the kernel validates a bounded receiver;
`inputd` retains input policy. It does not grant a DVM a RustOS management,
filesystem, network-policy, or arbitrary IPC endpoint.

`dvm-control-endpoint` models the availability boundary before that handshake.
The first four secret bytes select a private per-launch KVM-vsock port, derived
identically by L0 and the root-only DVM agent. A same-CID process that lacks the
fw_cfg secret cannot occupy the listener's setup slot, while a connection that
does reach the endpoint still has no control authority before the fresh HMAC
proof modeled separately above.

`dvm-agent-readiness` models the independent DVM-local process-liveness claim.
The exact payload is written and locked on a candidate inode before one atomic
rename publishes it; the serving process retains both singleton and ready-inode
locks. A stale file after any exit therefore cannot satisfy health, and the
diagnostic one-shot announcement has no readiness authority.

`dvm-network-ring` covers the independent bounded Ethernet data plane. Its
fixed header is host-created and copied at RustOS install time. The model makes
the DVM an adversarial writer of shared payloads and counters, while preserving
the implementation boundary: no guest pointer or descriptor reaches RustOS,
and an invalid producer, consumer, or packet length cannot advance a
kernel-owned cursor or reach network policy.

`dvm-network-control` covers the separate authority condition above that data
plane. An RDI1 session start is emitted only after L0 authenticates the DVM
agent; its exact session end revokes network use even though the ivshmem
aperture remains mapped. The model allows DVM data-plane writes after revocation
and proves that they cannot restore access or let a stale cleanup disable a
replacement lease. It does not make the input ring an Ethernet transport.

`dvm-input-revocation` covers the lifecycle boundary underneath that relay.
Its reset record replaces all queued frames from a retired DVM epoch; the new
epoch cannot deliver a key until inputd consumes that reset and clears
provider-owned keyboard/pointer state. It intentionally abstracts key layout,
frame checksum arithmetic, and mouse motion.

`trusted-ui-boundary` is a fail-closed release boundary. Fixed apertures and
launch-bound authentication bound memory and identity, but neither proves what
a human sees or intends. A privileged prompt is admissible only with
independent scanout and input attestation; DVM compromise, DVM provenance, or
either channel's loss cannot retain prompt authority. Until that evidence
exists, the uiserver endpoint reports the two attestation blockers for every
provider, with `DVM_SCANOUT` as additional provenance.

`input-readiness` covers the availability boundary between the same ring0
transport and its user-space policy owner. The model includes the dedicated
MSI-X-woken inputd worker, which may move ingress into service policy without
an application read. Finite poll and the latency-sensitive uiserver reader
invoke bounded `INPUTD_IPC_OP_STATS` readiness rechecks before an authorized
nonblocking read on a cumulative cadence. Both operations refresh ingress
under the inputd queue lock, and every transfer retains exact ownership and provenance.
The generic wait-set separately models the service-owned readiness generation,
exact provider epoch, arm/recheck race, and descriptor lifetime. Reader identity
and key translation remain covered by the input ABI, revocation model, and KVM
input-stream gate.

`userspace-wait-set` is the cross-provider availability and lifetime boundary.
vfsd owns persistent epoll membership, compat builds bounded transient poll
sets, and netd/inputd/sessiond own readiness truth and generations. Registration
identity combines the target fd with its stable provider object, matching the
Linux fd/open-description pair; its observed endpoint epoch is mutable state
that only explicit MOD may replace after restart. DEL uses the stable identity
and remains available without a live provider epoch. Ring0 stores only
bounded task wait tokens, validates the exact live
service endpoint epoch, supplies deadline wakeup, and re-queries provider state
before returning an event. Waiter capacity is derived from the scheduler task
ceiling times the provider ceiling, covering every task/provider pair. A
provider restart or last reference close wakes and
revokes a pending wait; the affected interest reports per-fd `ERR|HUP` while
unrelated readiness remains visible, instead of allowing numeric-token reuse or
aborting the aggregate wait. ppoll/epoll_pwait temporarily replace the calling thread's signal
mask around that wait and treat unmasked delivery as interruption. The finite
model includes the check-to-arm race, signal/timeout race,
provider restart/recovery, dup/fork/close/exec reference changes, terminal
console-session close without stale-operation resurrection, console output/HUP
readiness, and nonblocking empty-read termination.
Concrete dup/fcntl uses token snapshot, provider-ref acquisition, locked token
recheck, and exact replaced-handle return as one two-phase linearization.
Fork acquires provider refs from the same frozen child fd-table clone it will
publish, while exec cleanup consumes only the exact handles returned by its
atomic CLOEXEC commit. EPOLL_CTL_ADD/MOD pins the target description through
the vfsd mutation and applies ordinary last-close purge when that guard is the
final reference.
The concrete check-to-arm refinement keeps service IPC outside the scheduler
arm: it registers observations, rechecks providers, arms, then tests that the
same waiter records remain. Each provider query is capped by both 16 ms and the
remaining syscall deadline; `ipc-reply-deadline` supplies the compositional
bounded-call guarantee below this model. An internal query timeout cannot hide
readiness already found in the scan and otherwise retries under the original
application deadline rather than escaping as an early `ETIMEDOUT`.
`service-mutation-recovery` composes with this model for provider mutations.
Kernel-minted operation IDs, netd's ACK-retained replay ledger, compat's bounded
reconciliation queue, and rootd's authenticated vfsd checkpoint close the
former uncertain-outcome mechanism at source/model level. Vfsd replays epoll
state before endpoint publication, and object admission combines boot-entropy
capabilities, kernel-stamped senders, and rootd dependency edges. Runtime
service-crash injection remains an acceptance gate; source/model success is not
reported as that missing runtime evidence.

`ui-frame-budget` begins after that input ownership transfer. Keyboard routing
and console focus updates cross the `uiserver -> devmgrd -> runtimed` policy
boundary; the model treats their reply as independently slow or permanently
stalled. The UI takes a bounded FIFO admission decision, records a queue
rejection rather than waiting, and creates local redraw debt in the same
transition. TLC checks that only the worker owns synchronous delivery, terminal
accounting is exact, queue order/capacity hold, and redraw debt is eventually
presented under UI scheduling fairness. It deliberately does not promise
eventual policy delivery when the downstream owner is unavailable.

`ui-input-motion` covers the KVM-only DVM input selftest used as the concrete
FPS workload. Its relative pointer motion reverses on independent short x/y
phases, so a pre-existing cursor at any edge cannot turn a sustained input
stream into a visually idle sample. TLC explores every finite initial cursor
position, bounds consecutive clamped cycles, requires visible work in a
sample, and requires the final visual state to be presented. It is a workload
validity proof; `ui-frame-budget` remains the proof that stalled policy IPC
cannot block presentation.

`devmgrd-sessiond-isolation` covers the next broker boundary. Console routing
and focus ioctls may need a synchronous sessiond request, but devmgrd's receive
loop now admits them to a bounded worker pool instead of performing that call
inline. TLC checks admission, FIFO worker assignment, exact reply or EAGAIN
accounting, and the key isolation property: a pending unrelated device request
remains replyable while every sessiond worker is stalled.

`rootd-restart-backoff` covers the core-service recovery boundary. An exit
first revokes the old lease's authority and enters `RESTART_PENDING`; rootd's
bounded timer wait must expire before a replacement can consume one retry
budget unit. Ring0 supplies only a rootd-capability-gated timer substrate, so
backoff choice, terminal failure, and fresh authority publication remain
rootd policy.

`vfio-release-authorization` models the separate irreversible hardware handoff.
A launch-plan topology check does not grant bind authority. Only an
authorization whose signature has been verified against a pinned keyring and
whose exact artifact/policy digests match may create a durable VFIO record or
bind the complete IOMMU group. Restoring a record is intentionally allowed
after expiry so a failed release cannot strand host hardware.

`dvm-commercial-lifecycle` begins after that authorization. It models the
runtime manager's preflight-bind-reset-launch-authenticate-stop-reset-restore
order, including rejection of any reset fallback whose affected functions
escape the admitted IOMMU group and rejection of a VFIO idle-D3 bind that can
restore bus mastering before the non-identity mapping exists,
trusted immutable launch inputs, the exact PID/start-time recovery identity,
pidfd-bound signaling after a second identity check, numeric-PID reuse rejection,
nonzero/signaled child-exit rejection, and fail-closed VFIO quarantine. The
model deliberately fixes physical network and block assignment to disabled;
those excluded topologies gain no authority from this display-DVM proof.

`dvm-release-bundle` covers the artifact boundary immediately before that
lifecycle. Its staging linearization point is the atomic rename of a complete,
fsync'd, twice-verified temporary directory to a fresh trusted destination.
Launch authority has a separate linearization point: hostd's independent
verification of that published directory. The finite state space enumerates
all subsets of the eight required files plus valid and corrupted copies, and
checks that replacement or mutation never turns an incomplete bundle into
launch authority.

`driver-domain-fleet` models the cross-domain policy that surrounds that
handoff. A representative PCI BDF stands for every BDF in a real complete
IOMMU group; the source parser checks the full list. The model prevents CID,
group, or representative-function reuse, freezes policy before release
verification, and requires the signed release's fleet hash before activation.

`ipc-reply-deadline` is deliberately about the kernel-owned control-call path,
not arbitrary application-level wait graphs. Two policy services may
legitimately call one another, so the model permits that cycle and checks that
the concrete deadline, cancellation, and peer-close rules eliminate any
permanent blocked control wait. The service registry publishes the endpoint
last, clears it first, and accepts a steady-state lookup only when endpoint and
epoch remain unchanged across the owner read; that path does not acquire the
writer mutation lock. `scheduler-wakeup` then checks the lower-level
arm–timer–recheck–commit race: an early wake invalidates the same arm epoch,
and the timer clockevent wakes due tasks before a later dispatch can select
work. `clocksource-deadline` separates that clockevent from elapsed time: TSC
or HPET time may jump across coalesced virtual interrupts, and the next PIT
event must catch every crossed absolute deadline without consulting a
process-table lock for sleeper identity.

`ipc-priority-inheritance` covers the bounded critical-class boundary above that wake
protocol. A reply capability installs its bounded donation before a receiver
is woken, so a System caller can promote a User broker and its nested User
policy dependency. TLC checks that the elevation is transitive but cannot
survive reply completion, cancellation, or either task's exit.

`ipc-handle-transfer` covers the cross-crate ownership boundary that ordinary
endpoint models intentionally abstract away: IPC runtime queues opaque
descriptors, while `kernel-ps` owns the duplicated handle entries. Every path
that detaches a message must therefore return the descriptors for exactly-once
drop or installation. `proc-broker-session` covers the analogous loaderd
transaction; invalid commit attempts and loader exit are terminal cleanup
outcomes, not a way to retain a privileged prepare handle.

`exec-ticket` covers the separate `execve` transaction: procd authorizes one
exact running PID/TID pair, loaderd may consume that ticket only with the same
pair, and the broker must publish the target's register handoff before the
scheduler can observe its new image. A mismatched request is non-destructive;
normal or signal-driven target exit, a non-final target-thread exit, and Linux
exec sibling retirement remove any pending ticket and handoff.

When changing a mapped protocol, update the model in the same change or state
why the abstraction remains valid. A passing TLC model proves only the finite
state spaces in the corresponding cfg files. It does not prove Rust code
equivalence, ELF or PE loader memory safety, full CPU-time fairness, device-DMA
safety, or filesystem data integrity. Add a focused Rust test or KVM
expectation for every real-code path whose contract changes.

Equivalent concrete operations may be quotiented only when they have identical
state, authority, timeout, and terminal effects. The model must name that
equivalence class and the exact concrete vocabulary/bounds must remain covered
by source conformance. `DvmGpuCompositor` therefore explores one abstract
fixed-command class while retaining three in-flight values and three outputs;
it does not multiply the state graph by three command labels that no invariant
can distinguish.

The pre-QEMU transaction set additionally includes
`user-stack-growth/UserStackGrowth` for recoverable page faults,
`exec-address-space-transaction/ExecAddressSpaceTransaction` for active-root
ownership, `gpu-submit-transaction/GpuSubmitTransaction` for rejected-submit
rollback, and
`acceptance-profile-publication/AcceptanceProfilePublication` for bounded late
observer activation. `robust-futex-owner-death/RobustFutexOwnerDeath` includes
canonical shared/private cleanup-key selection. These focused models stay
small enough for the two-minute PR TLC budget and do not trigger the unchanged
large compositor model.
