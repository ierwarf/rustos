# RustOS formal models

This directory contains small, executable TLA+ models for RustOS contracts
whose correctness depends on interleavings. They are design and regression
checks; they do not generate kernel code or replace Rust tests, fuzzing, ABI
checks, or KVM smoke tests.

The modeled Rust contracts and their remaining abstraction limits are recorded
in [CONFORMANCE.md](CONFORMANCE.md). Update that audit whenever a mapped
source transition or cleanup owner changes.

## Run the PR suite

Java 11 or later plus curl and sha256sum are required. The runner fetches the
TLC jar named in [tla2tools.lock](tla2tools.lock), verifies its SHA-256, and
stores it outside the worktree. TLC state files also stay in a temporary
directory.

    bash formal/run-all-tlc.sh

Run an individual model with:

    bash formal/run-tlc.sh endpoint-registry/EndpointRegistry

The CI job uses one TLC worker and a fixed seed. This keeps each result
reproducible and avoids accepting a liveness result from a multi-worker
execution. The bounded models intentionally reach a finite cutoff, so the
runner disables TLC's deadlock report; configured invariants remain mandatory.

## Models and required properties

| Model | Concrete owner | Required safety properties |
| --- | --- | --- |
| rootd-bootstrap/RootdBootstrap | rootd, loaderd, IPC endpoint wait | core dependency gate before initd; exact PID lease; endpoint/capability lifecycle; bounded waits; single initd launch |
| endpoint-registry/EndpointRegistry | kernel compat IPC registry, rootd capability decision | publication is capability-complete; revoke/exit leave no authority; exact-PID wait cannot succeed on stale or foreign state |
| endpoint-publication/EndpointPublication | kernel compat IPC registry, process-table exit marker | registry writers are serialized; an exit marker aborts in-flight publication; lookup/capability authority needs an exact running owner; cleanup leaves no terminal authority |
| deferred-start/DeferredStart | loaderd, rootd, initd, runtimed | suspended child is inert; only its designated supervisor admits it; activation is single-use; endpoint follows activation |
| post-init-leases/PostInitLeases | rootd post-init readiness and restart policy | only the designated supervisor may report; a foreign live-lease rebind preserves PID/reporter/capability; exact PID idempotency; no capability before report; restart budget never underflows |
| rootd-restart-backoff/RootdRestartBackoff | rootd core-service recovery and timer substrate | exit revokes old authority; restart is delayed before every replacement; only a successful post-delay retry publishes fresh authority; retry budget is finite and monotonic |
| post-init-supervisor-recovery/PostInitSupervisorRecovery | rootd/initd post-init recovery and dependent UI revocation | a new initd adopts only an exact ready endpoint; an endpoint-less old lease blocks duplicate launch only for a bounded window; reclaim clears all process/endpoint authority and cascades from sessiond to uiserver |
| dvm-control-relay/DvmControlRelay | L0 hostd, Linux DVM agent, RDI2 input receiver | launch-bound CID and exact HELLO issue a fresh challenge; only its HMAC proof permits WELCOME; serial allowlisted probes; stale/mismatched replies fail closed; a completed probe gates a fresh relay epoch; input is strictly sequenced and clears on disconnect |
| dvm-control-endpoint/DvmControlEndpoint | L0 hostd/xtask, Linux DVM agent | only the root-only launch-secret holder derives the per-launch vsock listener port; a same-CID untrusted process cannot reserve setup; a reached endpoint still requires the separate HMAC proof |
| dvm-network-ring/DvmNetworkRing | DVM network ivshmem mapper, Linux Ethernet relay, netd substrate | only a host-validated fixed header installs the aperture; DVM counters are bounded before either kernel cursor advances; malformed/forged RX work is rejected without delivery; DVM header mutation cannot alter installed bounds |
| dvm-network-control/DvmNetworkControl | L0 RDI1 lifecycle, COM2 receiver, DVM network gate | aperture mapping alone has no authority; only an authenticated control epoch permits RustOS network access; exact end revokes it; stale end and DVM data writes cannot alter a replacement lease |
| dvm-input-revocation/DvmInputRevocation | RDI2 receiver, ring0 ingress queue, inputd, keyboard-core | every epoch start/end is a priority reset barrier; queued keys are bound to the current epoch; no key is delivered before its epoch reset; inputd releases all retired provider key state |
| trusted-ui-boundary/TrustedUiBoundary | DVM display/input provenance, GUI backend, uiserver trusted-UI status | a privileged prompt requires independently attested scanout and human input; DVM compromise or loss revokes it; a DVM transport may never self-attest |
| input-readiness/InputReadiness | ring0 ingress queue, poll/epoll substrate, inputd | arming/recheck cannot hide ingress; only a poll-woken client read transfers an ingress record to inputd; every record has exactly one ring0-or-policy owner |
| vfio-release-authorization/VfioReleaseAuthorization | hostd release gate, durable VFIO lease | no topology preflight can bind a device; a pinned-key signature binds the exact group, CID, DVM artifact, and device policy; every bind mutation is within the signed validity window; restore leaves no authority |
| driver-domain-fleet/DriverDomainFleet | hostd fleet policy and signed release gate | a fleet member is exactly encoded; CIDs, IOMMU groups, and representative PCI functions are globally disjoint; policy is immutable after sealing; only a signed release bound to that fleet can activate a member |
| dvm-display-seqlock/DvmDisplaySeqlock | DVM display provider and GUI backend | begin/finish parity follows the backend lock; a replaced DVM header is always retired at an even generation; no frame outlives its provider |
| ipc-reply-deadline/IpcReplyDeadline | kernel IPC runtime and compat deadline wait | exact caller/reply ownership; one-shot reply completion; owner exit and deadline clear the waiter; every blocked control cycle carries a finite break; stale or late replies cannot revive authority |
| scheduler-wakeup/SchedulerWakeup | kernel scheduler, current-task block API, timer IRQ | arm/wake/commit uses a fresh epoch; a wake before commit cannot become a block; blocked tasks own one unexpired timer; timer expiry precedes subsequent dispatch; retired tasks retain no scheduler or timer authority |
| ipc-handle-transfer/IpcHandleTransfer | process handle substrate, IPC runtime, compat IPC syscalls | a transferred descriptor is either installed or dropped exactly once; queue cancellation, peer-close, invalid receiver output, and caller exit leave no registry entry; batch transfer is all-or-nothing |
| ipc-endpoint-ownership/IpcEndpointOwnership | kernel IPC runtime, compat IPC syscalls, process handle table | a process-owned endpoint/reply may be served by its worker threads but cannot be received, replied to, or handle-drained by a foreign process; process exit revokes queued/received authority; sparse descriptor duplication never grows beyond the process ceiling |
| proc-broker-session/ProcBrokerSession | process broker, loaderd, Linux process teardown | exact loader ownership; mapping/runtime state only in a live prepare session; commit attempt is terminal; deferred children stay inert until activation; owner exit aborts every uncommitted prepare |
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
then L0 → RustOS over fixed RDI2 input frames. A CID and exact HELLO issue a
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
replacement lease. It does not make COM2 an Ethernet transport.

`dvm-input-revocation` covers the lifecycle boundary underneath that relay.
Its reset record replaces all queued frames from a retired DVM epoch; the new
epoch cannot deliver a key until inputd consumes that reset and clears
provider-owned keyboard/pointer state. It intentionally abstracts key layout,
frame checksum arithmetic, and mouse motion.

`trusted-ui-boundary` covers a deliberately fail-closed product boundary. The
current DVM display relay drives physical KMS and the current input relay
reports device events; fixed apertures and launch-bound authentication bound
memory and identity, but neither proves what a human sees or intends. The
model therefore grants a privileged prompt only to a future path that
independently attests both scanout and input. DVM compromise, DVM provenance,
or either channel's loss cannot retain prompt authority. The corresponding
uiserver endpoint currently reports the two missing-attestation blockers for
every provider, with `DVM_SCANOUT` as additional provenance.

`input-readiness` covers the availability boundary between the same ring0
transport and its user-space policy owner. Ring0 poll readiness is derived
from the bounded ingress queue, so `inputd` must transfer records only while
serving the poll-woken reader; an eager periodic drain would leave a reader
asleep after removing the only observable readiness record. It intentionally
abstracts reader identity and key translation, which remain covered by the
ABI, revocation model, and KVM input-stream gate.

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
and the timer IRQ wakes due tasks before a later dispatch can select work.

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
equivalence, ELF or PE loader memory safety, scheduler fairness, device-DMA
safety, or filesystem data integrity. Add a focused Rust test or KVM
expectation for every real-code path whose contract changes.
