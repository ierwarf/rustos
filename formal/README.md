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

The CI job uses one TLC worker and a fixed seed. This keeps each result
reproducible and avoids accepting a liveness result from a multi-worker
execution. The bounded models intentionally reach a finite cutoff, so the
runner disables TLC's deadlock report; configured invariants remain mandatory.

## Models and required properties

| Model | Concrete owner | Required safety properties |
| --- | --- | --- |
| boot-volume-admission/BootVolumeAdmission | ring0 boot-volume block substrate, Multiboot2 extent manifest | supplied identity selects only its exact target; a mismatch never degrades into discovery; identity-free Multiboot2 admission requires an extent manifest and exactly one FAT candidate |
| runtime-control-rpc/RuntimeControlRpc | `libs/runtime-control` request/reply client | only an exact successful opcode is admitted; snapshot payload count is bounded; non-snapshot success is payload-free; malformed statuses fail closed |
| rootd-bootstrap/RootdBootstrap | rootd, loaderd, IPC endpoint wait | core dependency gate before initd; exact PID lease; endpoint/capability lifecycle; bounded waits; single initd launch |
| endpoint-registry/EndpointRegistry | kernel compat IPC registry, rootd capability decision | publication is capability-complete; revoke/exit leave no authority; exact-PID wait cannot succeed on stale or foreign state |
| endpoint-publication/EndpointPublication | kernel compat IPC registry, process-table exit marker | registry writers are serialized; an exit marker aborts in-flight publication; lookup/capability authority needs an exact running owner; cleanup leaves no terminal authority |
| deferred-start/DeferredStart | loaderd, rootd, initd, runtimed | suspended child is inert; only its designated supervisor admits it; activation is single-use; endpoint follows activation |
| post-init-leases/PostInitLeases | rootd post-init readiness and restart policy | only the designated supervisor may report; a foreign live-lease rebind preserves PID/reporter/capability; exact PID idempotency; no capability before report; restart budget never underflows |
| rootd-restart-backoff/RootdRestartBackoff | rootd core-service recovery and timer substrate | exit revokes old authority; restart is delayed before every replacement; only a successful post-delay retry publishes fresh authority; retry budget is finite and monotonic |
| post-init-supervisor-recovery/PostInitSupervisorRecovery | rootd/initd post-init recovery and dependent UI revocation | a new initd adopts only an exact ready endpoint; an endpoint-less old lease blocks duplicate launch only for a bounded window; reclaim clears all process/endpoint authority and cascades from sessiond to uiserver |
| dvm-control-relay/DvmControlRelay | L0 hostd, Linux DVM agent, RDI3 input receiver | launch-bound CID and exact HELLO issue a fresh challenge; only its HMAC proof permits WELCOME; allowlisted probes, stale/mismatched replies, and replay fail closed; a completed probe gates a fresh relay epoch |
| dvm-control-endpoint/DvmControlEndpoint | L0 hostd/xtask, Linux DVM agent | only the root-only launch-secret holder derives the per-launch vsock listener port; a same-CID untrusted process cannot reserve setup; a reached endpoint still requires the separate HMAC proof |
| dvm-network-ring/DvmNetworkRing | DVM network ivshmem mapper, Linux Ethernet relay, netd substrate | only a host-validated fixed header installs the aperture; DVM counters are bounded before either kernel cursor advances; malformed/forged RX work is rejected without delivery; DVM header mutation cannot alter installed bounds |
| dvm-network-control/DvmNetworkControl | L0 RDI1 lifecycle, fixed input-ring receiver, DVM network gate | aperture mapping alone has no authority; only a fresh authenticated control epoch permits RustOS network access; exact end revokes it; stale end, epoch reuse, and DVM data writes cannot alter a replacement lease |
| dvm-input-revocation/DvmInputRevocation | RDI3 receiver, ring0 ingress queue, inputd, keyboard-core | every one-shot epoch start/end is a priority reset barrier; queued keys are bound to the current epoch; no key is delivered before its epoch reset; inputd releases all retired provider key state |
| dvm-input-ring/DvmInputRing | L0 producer, fixed ivshmem ring, RustOS MSI-X leaf, inputd broker | only L0 advances producer and only the broker advances consumer; 2,048 fixed 32-byte slots retain cleanup reserve; continuous production requires both an armed vector and a successful policy-backed client poll; DVM tamper attempts cannot mutate the ring; IRQ only wakes; malformed/stale records cannot reach inputd; revoke clears transport/consumer readiness and decoder authority; a boot-wide bounded recovery budget reuses one pinned vector; fairness drains or revokes finite committed work |
| trusted-ui-boundary/TrustedUiBoundary | DVM display/input provenance, GUI backend, uiserver trusted-UI status | a privileged prompt requires independently attested scanout and human input; DVM compromise, provider loss, or independent-attestation revocation cancels it; a DVM transport may never self-attest |
| input-readiness/InputReadiness | ring0 ingress queue, poll/epoll substrate, inputd | arming/recheck cannot hide ingress; the poll STATS recheck or an authorized read transfers an ingress record to inputd; every record has exactly one ring0, policy, or delivered owner |
| ui-frame-budget/UiFrameBudget | uiserver input loop, console-command worker, frame/present loop | console-policy IPC has bounded FIFO admission and one delivery owner; overload is recorded; an in-flight policy call cannot make local redraw debt wait; active-input feedback is eventually presented |
| ui-input-motion/UiInputMotion | DVM KVM input selftest and uiserver present loop | the test pointer reverses both axes before permanent edge clamping; every initial cursor position yields bounded visible work, and every accepted visible-motion epoch is eventually presented |
| dvm-input-selftest/DvmInputSelftest | DVM KVM selftest evdev selection and host input relay | the synthetic composite device cannot stream without absolute pointer capability; exactly one non-printable keyboard probe precedes pointer-only cycles; every generated position produces one pointer position and proves both routes |
| dvm-absolute-pointer/DvmAbsolutePointer | DVM evdev agent, L0 RDI3 relay, ring0 decoder, inputd, uiserver | partial axis reports never publish; duplicate complete positions are idempotent; every position has exactly one pipeline owner; absolute coordinates stay bounded and cannot cause phantom UI motion; finite accepted work drains under fairness |
| devmgrd-sessiond-isolation/DevmgrdSessiondIsolation | devmgrd receive loop and bounded sessiond ioctl workers | sessiond ioctls have bounded FIFO admission or EAGAIN; worker stalls never own devmgrd's receive loop; unrelated device traffic remains replyable |
| vfio-release-authorization/VfioReleaseAuthorization | hostd release gate, durable VFIO lease | no topology preflight can bind a device; a pinned-key signature binds the exact group, CID, DVM artifact, and device policy; every bind mutation is within the signed validity window; restore leaves no authority |
| driver-domain-fleet/DriverDomainFleet | hostd fleet policy and signed release gate | a fleet member is exactly encoded; CIDs, IOMMU groups, and representative PCI functions are globally disjoint; policy is immutable after sealing; only a signed release bound to that fleet can activate a member |
| ivshmem-pairing/IvshmemPairing | launch-private ivshmem broker and KVM launcher | the RustOS QEMU connection is observed as peer 0 before GUI-DVM launch; peer 1 cannot exist without peer 0; disconnect fails the complete pair closed and no reconnect can reuse the assignment |
| gui-dvm-surface/GuiDvmSurface | RustOS compositor to the sole supported GUI-DVM transport | `RSGUI002` has exactly three host-provisioned slots, exact even PRESENT/RELEASE generations, one outstanding DVM control record, module-latched pre-boot invitations, offline confirmation clearing, saturated-pool re-invitation, and stale-slot reclamation. It asserts bounded backpressure without fabricating capacity. Multi-domain focus is unavailable and rejected by the source rather than modeled as authority. |
| dvm-atomic-scanout/DvmAtomicScanout | Linux DVM DRM/KMS relay | an immutable source slot remains owned until its nonblocking atomic page flip completes and the old front buffer is synchronized as a shadow of that exact generation; release cannot precede the completion fence, an in-flight generation is strictly newer than the displayed generation, and the fixed local scanout set has exactly three buffers |
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
transport and its user-space policy owner. Ring0 poll wake is derived from the
bounded ingress queue, so an eager periodic drain would leave a reader asleep
after removing the only observable record. Instead, the poll recheck invokes
`INPUTD_IPC_OP_STATS`, and inputd transfers ingress before reporting its
service-owned queue. That non-consuming probe has a finite reply deadline: a
retry sees either unchanged ingress or the policy record transferred just
before cancellation. An authorized read retains the same transfer as a direct
read operation. The model constrains every ingress record to a DVM-labelled
Linux-key or pointer-packet kind; reader identity and key translation remain
covered by the ABI, revocation model, and KVM input-stream gate.

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
