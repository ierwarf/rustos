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

## Run the PR suite

Java 11 or later plus curl and sha256sum are required. The runner fetches the
TLC jar named in [tla2tools.lock](tla2tools.lock), verifies its SHA-256, and
stores it outside the worktree. TLC state files also stay in a temporary
directory.

    bash formal/run-all-tlc.sh --profile pr

The full formal gate also runs the Rust implementation proofs:

    bash formal/setup-kani.sh   # once per pinned Kani version
    bash formal/setup-verus.sh  # once per pinned Verus release
    bash formal/verify-all.sh --profile pr

The scheduled/manual nightly tier changes TLC fingerprint and seed, retains
single-worker reproducibility, adds fixed-seed long-trace simulation only for
registry-selected models, and runs Miri, Loom, Apalache, TLAPS, and bounded
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
| loader-request-authority/LoaderRequestAuthority | initd identity publication, loaderd ingress, process commit and exec-target brokers | privileged spawn is rootd/initd/sessiond-only and exec replacement is procd-only; both ingress and terminal ring0 commit require the current kernel-owned service identity, so guessed PIDs and service restart/revoke cannot retain authority |
| boot-storage-handoff/BootStorageHandoff | hostd storage admission/supervision plus durable VFIO lease | whole-device and every partition must be idle before bounded host flush; host-driver and VFIO authority are exclusive; VFIO assignment requires a durable binding to the exact signed epoch identity; DVM launch requires a durable exact runtime record and live generation-bound aperture; readiness binds that exact generation and epoch identity; active recovery observes the exact QEMU process exit before aperture revoke; repeated revoke preserves the signed immutable read-only flag while clearing all live state; the host driver is restored only after revoke and failure retains quarantine |
| commercial-service-envelope/CommercialServiceEnvelope | shared commercial ABI, service handlers, and exact-response clients | malformed requests receive an explicit error instead of dispatch or abandonment; only an exact request and fully bound response may become authority; foreign, truncated, reserved, and oversized replies fail; timeout and peer-close are explicit terminals |
| zero-trust-service-flow/ZeroTrustServiceFlow | kernel-stamped IPC sender, every published service ingress, object owner, exact-response caller | every hop independently validates shape and authority; direct subjects bind to the exact sender; delegation requires a live service owner on every request; stale capability/generation cannot mutate; only an exact bound response succeeds |
| zero-trust-subsystems.tsv | boot/image parsers, user memory, service authority, every DVM shared-memory consumer, DVM vsock control, host QMP, every local control socket, network frames | every inventoried ingress has explicit shape, authority, lifecycle/revoke, registered model, and executable source evidence; new `dvm_*.rs` consumers, host-side vsock/QMP readers, or service socket listeners fail selftest until added |
| entropy-broker-boundary/EntropyBrokerBoundary | boot entropy admission, boot-random master stream, compat broker, syscalld/netd policy | absent/zero entropy cannot initialize; only authorized policy services receive bounded copies; child streams derive from private master output and never public PID/TID/counter state |
| early-system-admission/EarlySystemAdmission | signed Multiboot2 module, boot-protocol fixed table, io-manager bootstrap reader, xtask staging | exactly one well-formed module declares the complete bounded bootstrap set; only declared digest-valid payloads load; missing, duplicate, malformed, undeclared, or corrupt content fails closed; DVM storage publication waits for the complete bootstrap set and never requires a native storage probe |
| dvm-volume-io/DvmVolumeIo | vfsd/storaged DVM volume requests and io-manager transport dispatch | the fixed rings and slots fit one 8-MiB power-of-two PCI BAR with an inaccessible reserved tail; empty, unaligned, overflowing, and out-of-range requests never dispatch; the exact 64-KiB storaged bulk-read reply is admitted only when all request/range/generation/length bindings match and it reuses read authority; chunk accounting is exact; timeout and device revocation remain distinguishable from transport failure |
| dvm-read-cache/DvmReadCache | storaged bounded DVM read-ahead cache | only an exact live generation and covered range may hit; misses fill at most eight non-overlapping 64-KiB windows; another generation atomically replaces the cache epoch; write and restart clear every window before completion |
| remote-file-mapping/RemoteFileMapping | loaderd prepared mappings, kernel-compat file copy, vfsd early-system/DVM ownership, VFS IPC v4 | source ownership is selected before applying its transfer bound; early-system reads remain 4-KiB broker chunks, DVM-volume replies remain within the exact maximum inline response, immutable-owner loss cannot fall through, and only an exact byte count may commit |
| syscall-simd-lifecycle/SyscallSimdLifecycle | syscall entry/exit and kernel-ps scheduler continuation state | the entering task owns a distinct user SIMD/FPU snapshot; blocking and preemption may replace only the scheduler continuation image; nested capture and foreign-task restore are rejected; return restores the exact entering image |
| pci-bar-discovery/PciBarDiscovery | kernel-hal standard PCI BAR discovery and resource publication | command decoding is disabled during sizing; each BAR dword is restored before its 64-bit partner is probed; decoding occurs only from the restored pair; the least significant implemented mask bit defines size; every terminal restores BARs and command state |
| runtime-control-rpc/RuntimeControlRpc | `libs/runtime-control` request/reply client | only an exact successful opcode is admitted; snapshot payload count is bounded; non-snapshot success is payload-free; malformed statuses fail closed |
| dual-abi-image-admission/DualAbiImageAdmission | loaderd plus `rustos-image-admission` | ELF64 and PE64 plans share one bounded, non-overlapping W^X gate; a main entry must belong to executable memory; only an entryless PE DLL may use entry zero; rejected plans never map |
| dual-abi-byte-parser/DualAbiByteParser | loaderd plus `rustos-image-admission` | a bounded ELF64/PE64 header, table, relocation and import parse must settle before mapping; rejected or subsequently mutated snapshots never map |
| page-table-lifecycle/PageTableLifecycle | compat MM broker and `kernel-mm` process address spaces | broker ranges are canonical, non-wrapping, and page-rounded before mutation; only live user frames map into user pages; every map/protect/unmap preserves W^X and removes unmapped access authority |
| process-address-space-lifetime/ProcessAddressSpaceLifetime | `kernel-ps` process table and `UserProcessState` | every state/address-space access holds one retained process reference and the per-process mutex; exit freezes the address-space epoch, stale exec cannot clear it, a prepared thread attachment cannot publish after exit and must release its unpublished stack, and reclamation waits for all authority to disappear |
| futex-waiter-lifecycle/FutexWaiterLifecycle | Linux futex scheduler substrate | a task owns at most one bounded waiter and original identity; requeue changes only its active key; keyed wake, key-independent timeout/spurious wake, and ABI-aware current-thread exit leave one explicit terminal outcome and no futex-table authority; forced foreign-thread cleanup remains a failed source gate |
| process-signal-delivery/ProcessSignalDelivery | procd policy, HAL fault handoff, and ring0 signal substrate | ring0 consumes only a still-pending unmasked signal; SIGKILL can only terminate and SIGSTOP can only enter a distinct stopped state; neither may be masked, ignored, or handled; invalid user targets and stale policy replies cannot redirect execution; a recoverable user fault retains process and task-IPC authority while a fatal final-thread fault publishes lifecycle evidence and revokes both; source stop/resume conformance remains failed |
| netd-deferred-reply/NetdDeferredReply | netd AF_UNIX deferred poll queue | the global reservation includes mutex-queued and worker-detached batches; admission stays bounded and each accepted request makes exactly one terminal reply attempt, including queue poison failure |
| memfd-seal-lifecycle/MemfdSealLifecycle | `kernel-ps` memfd object | atomic seal installation respects `F_SEAL_SEAL`; write sealing requires zero writable mappings; both truncate and EOF-extending write respect grow/shrink seals; mapping counters remain bounded |
| msi-vector-lifecycle/MsiVectorLifecycle | kernel HAL MSI allocator | allocation creates an unpublished exact lease; only that lease may bind one handler, failed MSI-X setup clears the exact handler before returning the slot, and only a fully programmed APIC-ready lease commits a permanent route |
| acpi-table-admission/AcpiTableAdmission | kernel HAL ACPI/MCFG/HPET parser | RSDP and SDTs have strict size/checksum/signature/entry-width bounds; MCFG regions publish atomically only when every ECAM range is aligned, mapped, bounded and non-overlapping; invalid firmware publishes no partial ECAM or HPET authority |
| persistent-mutation-admission/PersistentMutationAdmission | vfsd persistent-volume dispatch | the current writable-feature constant is false, so journal/recovery placeholders cannot authorize persistent mutation; volatile `/run` policy never advances persistent state |
| dma-iommu-isolation/DmaIommuIsolation | L0 hostd plus IOMMUFD/VFIO DVM assignment | device ownership is exact, mappings remain in the assigned DVM aperture, revocation removes mappings, and the finite map set stays bounded; ring0 owns no physical-storage DMA domain |
| filesystem-content-integrity/FilesystemContentIntegrity | signed early-system table plus bounded kernel bootstrap reader | only an exact allowlisted payload matching its digest verifies; corrupted content fails closed and missing bootstrap state terminates the read |
| network-payload-session/NetworkPayloadSession | DVM Ethernet transport plus netd | only bounded ARP/IPv4 payloads from an active authenticated epoch are delivered; malformed frames are dropped while advancing the sole consumer cursor |
| scheduler-cpu-distribution/SchedulerCpuDistribution | `kernel-ps` scheduler | every continuously runnable User task receives a turn after the two-dispatch System bound or its per-task ready-age limit; a bounded, deduplicated User latency FIFO drops stale owners and cannot exceed its eight-pick burst; per-task CPU accounting remains bounded in the checked horizon |
| scheduler-thread-demotion/SchedulerThreadDemotion | `kernel-ps` scheduler and uiserver helper threads | self-demotion cannot discard a live reply-scoped donation; untrusted or blocking UI helpers lose inherited System class before entering their loops, while input/present authority remains explicit |
| rootd-bootstrap/RootdBootstrap | rootd, loaderd, IPC endpoint wait | core dependency gate before initd; exact PID lease; endpoint/capability lifecycle; a five-second endpoint deadline retires an unready child before bounded restart; single initd launch |
| service-bootstrap-lifecycle/ServiceBootstrapLifecycle | rootd raw entry and helper handoff, kernel process retirement, initd dependency lookup | raw process entry aligns the stack before ordinary Rust; a non-final worker exit preserves process-owned authority; initd authorization is derived from the bootstrap manifest; only an unpublished endpoint is retryable, while undeclared or malformed lookups terminate |
| endpoint-registry/EndpointRegistry | kernel compat IPC registry, rootd capability decision | publication is capability-complete; revoke/exit leave no authority; exact-PID wait cannot succeed on stale or foreign state |
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
| dvm-block-transport/DvmBlockTransport | `driver-domain-protocol` ABI-v2 block transport, kernel signed-epoch admission/rebind, Linux storage-DVM relay, and storaged frontend | fixed request/completion queues remain bounded; records are launch-generation bound and contain no addresses; completions bind exact request identity; reads cannot invent durability; successful FUA and FLUSH completions report the exact stable operation; restart revokes queued authority and only a newer L0-signed epoch can be readmitted |
| dvm-block-startup/DvmBlockStartup | signed early-system key, conditional RustOS-ready publication, `kernel/io-manager` readiness predicate, compat block waiter, storage-DVM publication, and storaged first generation query | a peer race between verification and ready publication fails without mutating readiness; initial provider-not-ready remains sleepable; readiness publication before check, during registration, or after sleep cannot be lost; only an observed exact signed generation permits volume use; timeout and revoke are explicit terminal outcomes and never select bootstrap storage |
| trusted-ui-boundary/TrustedUiBoundary | DVM display/input provenance, GUI backend, uiserver trusted-UI status | a privileged prompt requires independently attested scanout and human input; DVM compromise, provider loss, or independent-attestation revocation cancels it; a DVM transport may never self-attest |
| input-readiness/InputReadiness | ring0 ingress queue, finite poll substrate, inputd worker, uiserver reader | the MSI-X worker, bounded STATS readiness recheck, and readiness-gated read are explicit transfer races; every record has exactly one ring0, policy, or delivered owner and service policy drains under consumer fairness |
| userspace-wait-set/UserspaceWaitSet | vfsd epoll registry, syscall-scoped poll sets, netd/inputd/sessiond readiness providers, compat wait-token and deadline substrate | each poll/epoll wait registers exact observed generations, rechecks providers while runnable, arms the scheduler, and verifies waiter presence before commit; readiness is re-queried before return and each service query is bounded by the application deadline; registration identity excludes provider epoch so MOD can rebind after downstream-provider restart without duplicate/undeletable interests; timeout, unmasked-signal interruption, revoke, dup/fork/close/exec, and last-reference retirement settle without a lost wake; vfsd restart composition is covered by `service-mutation-recovery` and still requires runtime crash injection |
| service-mutation-recovery/ServiceMutationRecovery | rootd-retained vfsd checkpoint, netd replay ledger, compat reconciliation queues | unique operation IDs advance one revision at most once; committed state survives service crash; replacement endpoints publish only after local state equals the retained checkpoint; an unresolved commit remains explicit until reply/reconciliation |
| vfs-open-description-recovery/VfsOpenDescriptionRecovery | rootd-retained vfsd open descriptions and compat user-copy settlement | an open becomes live only after every path chunk is durable and an uncertain open is cancelled by its proposed capability; sequential read/getdents cursor advances remain prepared until kernel copyout commits or cancels them; close tombstones survive response loss and may be compacted only after an explicit visibility ACK |
| input-ingestion-worker/InputIngestionWorker | ring0 DVM input wake leaf and inputd ingestion worker | only the capability-gated worker may claim policy-consumer readiness or advance committed records; wake/arm races are cursor-rechecked, every turn is bounded, and finite work eventually drains without granting drain authority to client poll |
| ui-frame-budget/UiFrameBudget | uiserver input loop, console-command worker, frame/present loop | console-policy IPC has bounded FIFO admission and one delivery owner; overload is recorded; an in-flight policy call cannot make local redraw debt wait; active-input feedback is eventually presented |
| wayland-accept-isolation/WaylandAcceptIsolation | uiserver bounded accept worker and netd-backed socket path | a stalled cross-service accept never owns the UI tick; accepted streams cross one bounded completion queue, overload closes the new client, and queued clients eventually settle under the declared worker/UI fairness |
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
| gui-dvm-pixel-authority/GuiDvmPixelAuthority | RustOS surface producer, uiserver, and GUI-DVM consumer | damage-only publication may reuse pixels only from the exact preceding snapshot; every slot has one writer/reader owner and monotonic generation, and revoke removes stale pixel authority before a later epoch |
| dvm-atomic-scanout/DvmAtomicScanout | explicit physical-AMD DMA-BUF/GPU/KMS relay mode | source/model matched, hardware gate failed: the complete 128 MiB pixel backing must first be DMA-pinnable and mapped into the VFIO IOAS, then only a coherent DMA attachment may import all three read-only sources; the kernel names the exact oldest live generation in a non-replayable acquire `sync_file`, and EGL server-waits it before composition into a separate three-buffer GBM output pool; GPU and page-flip fences precede source/output reuse; device-write DMA authority to sources is absent; evidence requires the complete chain; offline revokes both pools. Physical import, scanout, and sustained-rate evidence remain required. |
| dvm-gpu-compositor/DvmGpuCompositor | uiserver private scene compiler and Linux DVM fixed GLES executor | a bounded OS-owned context admits only clear, solid-quad, and textured-quad commands with host-bound read-only source tokens; only a measured prime record for the current host-selected epoch enables the asynchronous three-entry queue; acquire, completion, release, and presentation are monotonic fence states; raw commands, application shaders, CPU fallback success, and device writes to RustOS sources are impossible; a 16.667 ms target miss retains the prior front and live epoch, while the separate 50 ms hard timeout or revoke invalidates the full epoch and stale completions cannot revive it |
| dvm-gpu-proof-scheduler/DvmGpuProofScheduler | private AMD/virtio GPU proof process | only the finite post-prime measurement may use bounded SCHED_RR priority 8; limit installation, admission, and exact restore readback are distinct states; it remains below display/input relays; success and ordinary failure restore normal policy before evidence, while hard-limit or uncertain-restore termination publishes no evidence; the health loop has no realtime authority |
| dvm-display-scheduler/DvmDisplayScheduler | authenticated Linux DVM GPU/KMS relay scheduling | only a confirmed host invitation may first install the exact RT bound and then admit the current relay thread to SCHED_RR; display priority remains below input, partial admission cannot run the relay, continuous realtime CPU is capped, and retry is permitted only after exact policy/limit restore readback; hard-limit or restore failure terminates all process authority |
| dvm-display-readiness/DvmDisplayReadiness | Linux DVM GPU/KMS relay and agent health reader | one process singleton owns publication; only a complete locked candidate is atomically installed as ready; ordinary failure withdraws health before scheduler restoration; crash/hard-limit release all readiness authority; one fixed candidate bounds residue |
| dvm-gpu-admission/DvmGpuAdmission | uiserver provider admission, off-UI-thread GPU atlas initialization, and frame cadence | a mandatory DVM topology never reports software fallback as GPU success; CPU presentation remains live while the bounded worker initializes; only a current measured full-atlas/textured-draw prime, exact valid provider stride/mapping, retained scene, and completed first GPU frame promote the consumer; clear-only priming fails closed, each steady frame consumes one non-accumulating timer permit, initialization/first-frame timeout settles, and revoke requires a fresh epoch prime |
| dvm-gpu-atlas-transport/DvmGpuAtlasTransport | uiserver atlas owner, fixed RustOS transport, and display-DVM executor | a registered backend class selects exactly one compatible source mode; prime-completion v2 authenticates that mode and every submit must match it; exactly three imported source slots retain one mapping generation for the provider epoch while frame sequence/content epoch advance; the first update defines the full atlas, later bounded non-overlapping damage or command-only updates execute strictly in order; QEMU staged upload and physical read-only DMA-BUF modes cannot exchange evidence; source reuse requires the GPU fence, old-front reuse requires the later present fence, and revoke/reset removes every outstanding authority |
| gui-dvm-install/GuiDvmInstall | GUI-DVM ivshmem installer in the I/O manager | one serialized installer owns both BAR mappings, two permanent MSI-X vectors, and provider registration. Every malformed, absent, or failed installation releases mappings before terminal rejection; a concurrent caller cannot allocate a second installation; a revoked transport never reopens or falls back. |
| ipc-reply-deadline/IpcReplyDeadline | kernel IPC runtime and compat deadline wait | exact caller/reply ownership; one-shot reply completion; owner exit and deadline clear the waiter; every blocked control cycle carries a finite break; stale or late replies cannot revive authority |
| scheduler-wakeup/SchedulerWakeup | kernel scheduler, current-task block API, timer IRQ | arm/wake/commit uses a fresh epoch; a wake before commit cannot become a block; blocked tasks own one unexpired timer; timer expiry precedes subsequent dispatch; retired tasks retain no scheduler or timer authority |
| clocksource-deadline/ClocksourceDeadline | invariant-TSC/HPET clocksource, PIT clockevent, scheduler sleep identity | elapsed time never derives from delivered RTC-edge count; a delayed event catches every absolute deadline crossed by a clocksource jump; only a calibrated source is admitted; sleep identity is the exact scheduler task id even while syscall code holds the process-table lock |
| scheduler-admission/SchedulerAdmission | runtimed launch-catalog admission | a launch record is not a realtime capability: all non-UI requests are clamped below System admission even when registry input is hostile; only the exact trusted UI executable receives its pinned System weight; pending admission eventually settles |
| ipc-priority-inheritance/IpcPriorityInheritance | scheduler effective classes and compat synchronous IPC | a live reply capability owns the only priority donation; System class propagates through nested calls; completion, cancellation, and task exit revoke it; System work wins until its bounded burst is exhausted, then one ready User turn is mandatory |
| ipc-handle-transfer/IpcHandleTransfer | process handle substrate, IPC runtime, compat IPC syscalls | a transferred descriptor is either installed or dropped exactly once; every exported service-backed description owns a matching service reference which installation adopts or bounded deferred cleanup releases; queue cancellation, peer-close, invalid receiver output, caller exit, and owner exit after dequeue leave no registry entry; batch transfer is all-or-nothing |
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
permanent blocked control wait. `scheduler-wakeup` then checks the lower-level
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
