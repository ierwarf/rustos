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

## Run the PR suite

Java 11 or later plus curl and sha256sum are required. The runner fetches the
TLC jar named in [tla2tools.lock](tla2tools.lock), verifies its SHA-256, and
stores it outside the worktree. TLC state files also stay in a temporary
directory.

    bash formal/run-all-tlc.sh

The full formal gate also runs the Rust implementation proofs:

    bash formal/setup-kani.sh   # once per pinned Kani version
    bash formal/setup-verus.sh  # once per pinned Verus release
    bash formal/verify-all.sh

`PROOF-INFRA.md` records the evidence boundary and the rule for accepting a
counterexample as an implementation bug. Do not treat a solver limitation or
an unmapped model trace as a source defect.

Run an individual model with:

    bash formal/run-tlc.sh endpoint-registry/EndpointRegistry

The runner uses TLC's automatic local worker count and a fixed fingerprint seed.
This preserves exhaustive invariant checking while avoiding a repository-wide
single-core bottleneck. Worker scheduling can change exploration order; use
`TLC_WORKERS=1` when a serial reproduction is needed, or set any positive
integer for a bounded worker count. The bounded models intentionally reach a
finite cutoff, so the runner disables TLC's deadlock report; configured
invariants remain mandatory.

## Models and required properties

| Model | Concrete owner | Required safety properties |
| --- | --- | --- |
| boot-volume-admission/BootVolumeAdmission | ring0 boot-volume block substrate, Multiboot2 extent manifest | supplied identity selects only its exact target; a mismatch never degrades into discovery; identity-free Multiboot2 admission requires an extent manifest and exactly one FAT candidate |
| runtime-control-rpc/RuntimeControlRpc | `libs/runtime-control` request/reply client | only an exact successful opcode is admitted; snapshot payload count is bounded; non-snapshot success is payload-free; malformed statuses fail closed |
| dual-abi-image-admission/DualAbiImageAdmission | loaderd plus `rustos-image-admission` | ELF64 and PE64 plans share one bounded, non-overlapping W^X gate; a main entry must belong to executable memory; only an entryless PE DLL may use entry zero; rejected plans never map |
| dual-abi-byte-parser/DualAbiByteParser | loaderd plus `rustos-image-admission` | a bounded ELF64/PE64 header, table, relocation and import parse must settle before mapping; rejected or subsequently mutated snapshots never map |
| page-table-lifecycle/PageTableLifecycle | `kernel-mm` process address spaces | only live user frames map into user pages; every map/protect/unmap preserves W^X and removes unmapped access authority |
| dma-iommu-isolation/DmaIommuIsolation | L0 hostd plus kernel I/O substrate | device ownership is exact, mappings remain in the assigned domain aperture, revocation removes mappings, and the finite map set stays bounded |
| filesystem-content-integrity/FilesystemContentIntegrity | signed boot extent manifest plus kernel boot-volume reader | only bytes matching the manifest digest verify; corrupted content fails closed and an unavailable medium terminates the read |
| network-payload-session/NetworkPayloadSession | DVM Ethernet transport plus netd | only bounded ARP/IPv4 payloads from an active authenticated epoch are delivered; malformed frames are dropped while advancing the sole consumer cursor |
| scheduler-cpu-distribution/SchedulerCpuDistribution | `kernel-ps` scheduler | every continuously runnable User task receives a turn after the two-dispatch System bound or its per-task ready-age limit; a bounded, deduplicated User latency FIFO drops stale owners and cannot exceed its eight-pick burst; per-task CPU accounting remains bounded in the checked horizon |
| rootd-bootstrap/RootdBootstrap | rootd, loaderd, IPC endpoint wait | core dependency gate before initd; exact PID lease; endpoint/capability lifecycle; bounded waits; single initd launch |
| endpoint-registry/EndpointRegistry | kernel compat IPC registry, rootd capability decision | publication is capability-complete; revoke/exit leave no authority; exact-PID wait cannot succeed on stale or foreign state |
| endpoint-publication/EndpointPublication | kernel compat IPC registry, process-table exit marker | registry writers are serialized; an exit marker aborts in-flight publication; lookup/capability authority needs an exact running owner; cleanup leaves no terminal authority |
| deferred-start/DeferredStart | loaderd, rootd, initd, runtimed | suspended child is inert; only its designated supervisor admits it; activation is single-use; endpoint follows activation |
| post-init-leases/PostInitLeases | rootd post-init readiness and restart policy | only the designated supervisor may report; a foreign live-lease rebind preserves PID/reporter/capability; exact PID idempotency; no capability before report; restart budget never underflows |
| rootd-restart-backoff/RootdRestartBackoff | rootd core-service recovery and timer substrate | exit revokes old authority; restart is delayed before every replacement; only a successful post-delay retry publishes fresh authority; retry budget is finite and monotonic |
| post-init-supervisor-recovery/PostInitSupervisorRecovery | rootd/initd post-init recovery and dependent UI revocation | a new initd adopts only an exact ready endpoint; an endpoint-less old lease blocks duplicate launch only for a bounded window; reclaim clears all process/endpoint authority and cascades from sessiond to uiserver |
| dvm-control-relay/DvmControlRelay | L0 hostd, Linux DVM agent, RDI3 input receiver | launch-bound CID and exact HELLO issue a fresh challenge; only its HMAC proof permits WELCOME; allowlisted probes, stale/mismatched replies, and replay fail closed; a completed probe gates a fresh relay epoch |
| dvm-control-endpoint/DvmControlEndpoint | L0 hostd/xtask, Linux DVM agent | only the root-only launch-secret holder derives the per-launch vsock listener port; a same-CID untrusted process cannot reserve setup; a reached endpoint still requires the separate HMAC proof |
| dvm-agent-readiness/DvmAgentReadiness | Linux DVM control agent and init owner | local health requires an initialized live serving process holding the atomically installed exact ready inode; stale, malformed, symlinked, partial, announced-only, or post-exit state fails closed, and one fixed candidate bounds crash residue |
| dvm-network-ring/DvmNetworkRing | DVM network ivshmem mapper, Linux Ethernet relay, netd substrate | only a host-validated fixed header installs the aperture; DVM counters are bounded before either kernel cursor advances; malformed/forged RX work is rejected without delivery; DVM header mutation cannot alter installed bounds |
| dvm-network-control/DvmNetworkControl | L0 RDI1 lifecycle, fixed input-ring receiver, DVM network gate | aperture mapping alone has no authority; only a fresh authenticated control epoch permits RustOS network access; exact end revokes it; stale end, epoch reuse, and DVM data writes cannot alter a replacement lease |
| dvm-input-revocation/DvmInputRevocation | RDI3 receiver, ring0 ingress queue, inputd, keyboard-core | every one-shot epoch start/end is a priority reset barrier; queued keys are bound to the current epoch; no key is delivered before its epoch reset; inputd releases all retired provider key state |
| dvm-input-ring/DvmInputRing | L0 producer, fixed ivshmem ring, RustOS MSI-X leaf, inputd broker | only L0 advances producer and only the broker advances consumer; 2,048 fixed 32-byte slots retain cleanup reserve; continuous production requires both an armed vector and a successful policy-backed client poll; DVM tamper attempts cannot mutate the ring; IRQ only wakes; malformed/stale records cannot reach inputd; revoke clears transport/consumer readiness and decoder authority; a boot-wide bounded recovery budget reuses one pinned vector; fairness drains or revokes finite committed work |
| trusted-ui-boundary/TrustedUiBoundary | DVM display/input provenance, GUI backend, uiserver trusted-UI status | a privileged prompt requires independently attested scanout and human input; DVM compromise, provider loss, or independent-attestation revocation cancels it; a DVM transport may never self-attest |
| input-readiness/InputReadiness | ring0 ingress queue, finite poll substrate, inputd worker, uiserver reader | the MSI-X worker, bounded STATS readiness recheck, and readiness-gated read are explicit transfer races; every record has exactly one ring0, policy, or delivered owner and service policy drains under consumer fairness |
| ui-frame-budget/UiFrameBudget | uiserver input loop, console-command worker, frame/present loop | console-policy IPC has bounded FIFO admission and one delivery owner; overload is recorded; an in-flight policy call cannot make local redraw debt wait; active-input feedback is eventually presented |
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
| ipc-handle-transfer/IpcHandleTransfer | process handle substrate, IPC runtime, compat IPC syscalls | a transferred descriptor is either installed or dropped exactly once; queue cancellation, peer-close, invalid receiver output, caller exit, and owner exit after dequeue leave no registry entry; batch transfer is all-or-nothing |
| ipc-endpoint-ownership/IpcEndpointOwnership | kernel IPC runtime, compat IPC syscalls, process handle table | a process-owned endpoint/reply may be served by its worker threads but cannot be received, replied to, or handle-drained by a foreign process; transferred handles install before a reply becomes terminal; process exit revokes queued/received authority; sparse descriptor duplication never grows beyond the process ceiling |
| proc-broker-session/ProcBrokerSession | process broker, loaderd, Linux process teardown | exact loader ownership and inherited console-session binding; mapping/runtime state only in a live prepare session; commit attempt is terminal; deferred children stay inert until activation; owner exit aborts every uncommitted prepare |
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
The general indefinite-poll service readiness object remains a failed next-ABI
gate rather than being inferred from a ring0 wake. Reader identity and key
translation remain covered by the ABI, revocation model, and KVM input-stream
gate.

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
