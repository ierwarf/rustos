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
| post-init-leases/PostInitLeases | rootd post-init readiness and restart policy | only the designated supervisor may report; exact PID idempotency; no capability before report; restart budget never underflows |
| dvm-control-relay/DvmControlRelay | L0 hostd, Linux DVM agent, RDI2 input receiver | launch-bound vsock identity and HELLO/WELCOME gate; serial allowlisted probes; stale/mismatched replies fail closed; a completed probe gates a fresh relay epoch; input is strictly sequenced and clears on disconnect |
| dvm-display-seqlock/DvmDisplaySeqlock | DVM display provider and GUI backend | begin/finish parity follows the backend lock; a replaced DVM header is always retired at an even generation; no frame outlives its provider |
| ipc-reply-deadline/IpcReplyDeadline | kernel IPC runtime and compat deadline wait | exact caller/reply ownership; one-shot reply completion; owner exit and deadline clear the waiter; every blocked control cycle carries a finite break; stale or late replies cannot revive authority |
| scheduler-wakeup/SchedulerWakeup | kernel scheduler, current-task block API, timer IRQ | arm/wake/commit uses a fresh epoch; a wake before commit cannot become a block; blocked tasks own one unexpired timer; timer expiry precedes subsequent dispatch; retired tasks retain no scheduler or timer authority |
| ipc-handle-transfer/IpcHandleTransfer | process handle substrate, IPC runtime, compat IPC syscalls | a transferred descriptor is either installed or dropped exactly once; queue cancellation, peer-close, invalid receiver output, and caller exit leave no registry entry; batch transfer is all-or-nothing |
| ipc-endpoint-ownership/IpcEndpointOwnership | kernel IPC runtime, compat IPC syscalls, process handle table | an owner-bound endpoint/reply cannot be received, replied to, or handle-drained by a foreign task; sparse descriptor duplication never grows beyond the process ceiling |
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
general hypervisor RPC channel: Linux DVM → L0 over host-bound KVM-vsock, then
L0 → RustOS over fixed RDI2 input frames. It keeps the existing ownership
boundary: L0 validates DVM identity and relay syntax; the kernel validates a
bounded receiver; `inputd` retains input policy. It does not grant a DVM a
RustOS management, filesystem, network-policy, or arbitrary IPC endpoint.

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
