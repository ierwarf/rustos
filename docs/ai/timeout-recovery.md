# Timeout, cancellation and recovery ownership

## One operation, one admitted deadline

The initiating Ring3 policy owner fixes a monotonic operation end once.
Queueing, partial transfer, nested IPC, retries and cleanup consume that end;
child limits can shorten it, never renew it. Zero cannot become an unlimited
child timeout. A failed clock read is an error, not timestamp zero. The shared
service runtime offers a fallible checked reader and a fail-stop wrapper.

All RustOS processes share the kernel's `CLOCK_MONOTONIC` nanosecond domain.
`std::Instant` has no serializable cross-process epoch. RustOS, Linux DVM and
L0 do not share a monotonic epoch; a boundary uses its own local remaining-time
contract while the initiating owner retains final admission authority.

A deadline bounds authority to proceed and wait. It cannot make an unscheduled
thread return at an exact wall-clock instant, interrupt an uninterruptible
Linux device operation, or establish DMA quiescence. CPU scheduling budgets,
performance objectives, startup ceilings and provider watchdogs are separate
contracts, not interchangeable timeout constants.

## Completion and custody

A timeout is not proof that a mutation did not occur. Runtime-control v3 binds
mutations to an authenticated peer, immutable transaction identity and full
request fingerprint. Clients retain that identity and query after an ambiguous
reply. The bounded replay horizon derives from identity creation, so expired
unknown metadata can be reclaimed without readmitting the old operation.
Worker/resource custody is independent of replay metadata.

A consumed, timely IPC reply may already transfer a child PID or descriptor.
The caller must preserve that custody even if it is next scheduled after its
local deadline; it must not discard a committed child and report a fictitious
pre-commit failure. Loader cancellation uses only the work lease remainder;
expired work is retired independently by its server.

Netd's rootd-authorized registration serializes predecessor lease retirement
with endpoint publication. Packet brokers revalidate exact owner/publication
epoch under that same transport lock, preventing a paused predecessor syscall
from restoring or revoking successor authority. Ring0 supplies the narrow
lifetime mechanism; input/session/network retry policy stays in Ring3.

## Startup and device recovery

- [Startup contract](startup-timeout-contract.md): pending dependencies,
  admission, host framing and interactive startup consume bounded parent
  budgets. Healthy established input/UI sessions may remain idle indefinitely.
  Diagnostic startup ceilings do not widen the product timing SLO.
- [Runtime RPC contract](runtime-rpc-deadlines.md): worker commit, outcome
  queries, replay retention, client action gates and restart-unknown behavior.
  Dependency lifetime is separate from automatic restart custody: an exited
  service is not a completed one-shot dependency.
- [DVM recovery contract](dvm-timeout-recovery.md): shared GPU operation/phase
  deadlines, independent watchdogs, completion publication and quarantine.
  Bound display readiness follows a real frame and belongs to the latest
  invitation generation/attempt; a failed attempt cannot be revived by a late
  ready record. The independent GPU probe has its own health lifecycle.

Physical hostd uses explicitly provisioned persistent authority. Every recovery
attempt establishes current exclusive ownership and the required fresh reset;
a persisted reset phase is progress, not a transferable quiescence capability.
Interrupted or unsupported partial handoff remains quarantined. Total loss of
all durable state and hardware reset effectiveness require independent
operator/platform evidence; they are not inferred from a timeout or boot ID.

## Executable validation

Native Rust tests cover deadline arithmetic, clock failures, exact reply/caller
custody, transaction replay/expiry, worker commit, client reconciliation,
startup admission and physical recovery state transitions. Native C witnesses
run the actual relay bodies via `formal/run-dvm-timeout-tests.sh`, including a
worker paused before completion publication and hard-watchdog retention.
These C tests are mandatory in the PR source-conformance lane.

`formal/verify-all.sh --profile pr` is the proof gate; a route printed by
`cargo xtask dev-plan` is not evidence. KVM traces are private per launch,
bind the launched image and are replayed before atomic publication. Their
advisory timing classification does not prove the timing SLO. KVM readiness
claims name the vCPU topology and host CPU-pressure profile. Physical VFIO
behavior is not established by native mocks or virtual KVM boots.

## External design references

These references inform the local contracts; RustOS does not copy their ABIs.

- [gRPC deadlines](https://grpc.io/docs/guides/deadlines/): propagate remaining
  time and handle clock-domain differences explicitly.
- [gRPC cancellation](https://grpc.io/docs/guides/cancellation/): application
  work must observe cancellation; stopping the caller alone is insufficient.
- [AWS timeouts, retries and jitter](https://aws.amazon.com/builders-library/timeouts-retries-and-backoff-with-jitter/):
  retry at an owned layer and do not duplicate side effects after timeout.
- [L4Re IPC](https://l4re.org/doc/l4re_concepts_ipc.html): phase-relative versus
  absolute timeouts and the lifetime of late reply authority.
- [seL4 MCS](https://docs.sel4.systems/Tutorials/mcs.html): explicit reply
  custody and CPU scheduling budgets, distinct from response deadlines.
- [Linux DMA-fence contract](https://docs.kernel.org/driver-api/dma-buf.html#dma-fence-cross-driver-contract):
  bounded fence completion, submission control and recovery.
- [systemd service supervision](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html):
  distinguish startup, ready, watchdog and stopping states.
