# Ring3 provider custody: Codex acceptance matrix

**Status:** 2026-11-01 draft. Nothing on this page proves the implementation
works. The author intentionally did not run compiler, tests, static audit,
QEMU/KVM, stress, sanitizer or formal proofs.

## Minimum production authority invariant

1. Rootd's exact authenticated sender matches an unexpired netd or vfsd
   service PID and live lease. No request-supplied service ID grants authority.
2. The positive per-service grant contains ChargeClientSession. Independently,
   the effective kernel JobPolicy permits the semantic operation.
3. The kernel, not the client, identifies the live generational JobId by PID,
   and refuses a stale/recycled PID or Job generation before charging.
4. A provider token has one epoch, monotonic nonce and rootd-owned kernel
   token for its entire lifecycle. Never create a second kernel token on a
   lost IPC ACK.
5. The kernel's JobSession ledger and watermark own the charge/refund.
   Provider-local memory may only mirror custody and must not create it.
6. A closed provider object is not a valid reason to retry its *close* after
   failure; refund is reconciled independently using the exact original token.
7. An authenticated kernel EXIT drains all remaining provider tokens before
   lease replacement; health timeout alone is not lifecycle proof.

## Exact rootd operation cases

| Case | Expected result |
| --- | --- |
| Charge with wrong protocol, op or body length | EINVAL, no charge |
| Charge with scalar args or nonzero untrusted Job | EINVAL |
| Charge from unrelated PID | EACCES |
| Charge from stale netd/vfsd PID | EACCES/ESTALE |
| Charge with disabled JobPolicy bit | EACCES |
| Charge a process not in claimed live Job | Rejected in kernel |
| Duplicate identical charge before ACK | Same token, same job |
| Duplicate charge with changed client PID | Rejected |
| Release with wrong client Job generation | Rejected |
| Release with wrong session or kind | Rejected |
| Duplicate identical release after lost ACK | Committed result, no double decrement |
| Charge after same token was released | ESTALE |
| Kernel session capacity exhausted | Error, no publication |
| Kernel charge commits and copyout faults | Next exact retry, no double charge |
| Rootd is killed after charge but before reply | Kernel charge reconciliation on restart |
| Provider process exits before first ACK | Kernel drain then tombstone retirement |
| Provider PID recycled after exit | Old epoch cannot grant new authority |

## netd acceptance cases

- AF_INET socket creation charges once before broker publication and sends
  only one public token after successful funding.
- AF_UNIX single socket and socketpair use one charge per open description.
  A failure funding the second side must refund the first and never publish
  one half of an allegedly successful pair.
- Netd duplicates a socket: only ref count increases; Job session does not.
- Netd closes one of multiple references: no refund until final close.
- Listen has queued accepted socket candidates: those remain internal and
  unbilled until the exact accept publication.
- Failed accept requeues an internal candidate: its tentative charge is
  refunded, including ambiguous copyout outcomes.
- Listener retirement recursively releases owned queued candidates; all
  already-public sockets remain separately charged until final close.
- A final close with lost refund ACK retires the socket and retains the
  original billing token for the independent refund worker.
- Kernel broker publication failure after funding causes exact-token
  rollback or retained ambiguous token; no silent discard.
- Simultaneous netd request workers cannot spend one token twice or release
  a freshly reminted socket with an old token.
- A provider exit while the refund worker is active must terminate the old
  authority before a new lease can be admitted.

## vfsd acceptance cases

- Boot checkpoint replay happens before endpoint publication and does not
  issue a provider billing RPC while holding any storage owner lock.
- OPENAT request uses a stable remote ID, verified client PID and actual
  successful metadata lookup; the client Job charge precedes publication.
- Existing open-description replay for the same remote ID and PID is
  idempotent. A different client PID must not adopt that charge.
- Checkpoint transaction failure after funding refunds the unexposed
  description; partial checkpoint writes remain an independently recoverable
  staging record.
- Final close records tombstone before refund. A lost refund ACK can be
  reconciled by replaying the same close operation.
- vfsd restart reconstructs structural descriptions from checkpoint but
  refuses data operations on them until reattached to a new client-funded
  provider epoch.
- Remote id reuse cannot cross provider epoch, PID generation or Job
  generation; old client ownership must be verified after restart.
- A duplicated fd shares the *same* open-description charge, rather than
  duplicating its quota.
- Rootd waiting for vfsd must not synchronously require the same receiver
  to await rootd: model the exact bootstrap dependency/call graph.
- Kernel Job charges for exited vfsd PID must be drained by rootd even if
  a local open descriptor outlived the old process.

## Failure and performance boundaries

The source draft currently leaves unresolved whether vfsd may safely make a
second rootd IPC from the exact bootstrap service callback. Require a
call-graph/deadlock audit before acceptance. Under the intended Ring3 model,
all accounting IPC is at socket/open creation or final destruction, not
send/recv/read/write, futex, scheduler, pager fault or TLB paths.

No one may report success when the kernel debit/refund ACK is ambiguous.
A provider restart may not automatically treat restored checkpoint data as
fresh billing authority. Bounded tombstone history can exhaust after many
completed sessions; Codex must add an authenticated completion-ACK/watermark
compaction contract before claiming long-running production stability.

### Required Codex runs (not executed in this draft session)

1. Rust formatting, Cargo.lock, targeted no_std crate compile and unit tests.
2. Generated-IDL check and schema 520 compatibility tests.
3. `python3 tools/ring3_provider_source_audit.py` followed by real code review.
4. KVM 1/8 vCPU open/close and socketpair/accept/final-ref stress.
5. Process-exit and rootd/netd/vfsd restart fault injection with PID reuse.
6. Kernel accounting counter deltas including exact negative-right tests.
7. Failure after kernel charge before user copyout and before IPC reply.
8. Control-plane deadlock and IPC p50/p99 latency regression review.
