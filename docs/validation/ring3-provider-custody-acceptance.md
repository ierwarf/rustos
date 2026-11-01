# Ring3 provider custody: Codex acceptance matrix

**Status (2026-11-01):** an acceptance *matrix*. The cases below are the
contract; the witness table at the end says which ones currently have an
executed unit test and which still need KVM fault injection. A case without a
named witness is not verified.

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

All accounting IPC happens at socket/open creation or final destruction, never
in send/recv/read/write, futex, scheduler, pager-fault or TLB paths. Nobody may
report success when a kernel debit or refund ACK is ambiguous: the exact token
is retained, an ambiguous CHARGE is fenced by CANCEL, and an ambiguous RELEASE
is retried with the same token. Restored vfsd checkpoint data is never fresh
billing authority; a restored description refuses data operations until it is
rebound through OPENAT admission.

Bootstrap: vfsd builds its provider ledger lazily after checkpoint replay, so
no rootd call is made while the endpoint is being registered. A 1-vCPU KVM smoke
boot reached init handoff and launched UI/app processes with the vfsd and netd
billing paths compiled in; the billing RPC volume was not instrumented.

## Receipt ACK and tombstone lifecycle

CANCEL and RELEASE leave a Settled tombstone in rootd. The provider confirms it
with op 28 from its bounded `ProviderClient` outbox (retried by netd's refund
worker and at every vfsd billing operation). Rootd removes the tombstone only on
an exact ACK. Under table pressure (4096 tickets) it reclaims the oldest Settled
tombstone; unsettled tickets are never reclaimed and a full table of them
refuses with ENOSPC. The per-owner nonce watermark is never reset by either
path, so a reclaimed token cannot be charged again, and a repeated RELEASE,
CANCEL or ACK of an absent token succeeds idempotently. A restarted rootd has an
empty book and a drained kernel ledger, so the same idempotence lets providers
finish closing without wedging.

## Witness status

| Case | Witness |
| --- | --- |
| Exact shape, scalar args, wrong op denied | `provider_control::tests::exact_shape_denies_scalar_and_wrong_operation` |
| Canonical IDL 520 round trip, trailing/corrupt/legacy frames | `provider_wire::tests` (3) |
| Duplicate charge never double-charges; settled token cannot recharge | `provider_billing::tests::billing_records_exact_client_not_provider` |
| Lost CHARGE ACK reuses the same kernel token | `uncertain_charge_reuses_same_kernel_token` |
| Lost RELEASE ACK, uncertain refund | `uncertain_refund_never_recharges_a_settled_ticket` |
| Wrong client/session/kind cannot refund | `wrong_client_job_or_session_cannot_claim_refund` |
| Provider exit drains before forgetting; failed drain keeps receipts | `a_provider_exit_drains_before_forgetting_receipts`, `failed_kernel_drain_keeps_provider_receipts` |
| CANCEL before CHARGE fences a late nonce | `cancel_undelivered_charge_fences_late_same_nonce` |
| ACK compaction keeps the spent-nonce fence | `ack_compaction_preserves_the_spent_nonce_fence` |
| Lost ACKs do not exhaust the table; unsettled exhaustion still refuses | `settled_tombstones_are_reclaimed_under_pressure_without_unspending_tokens`, `unsettled_table_exhaustion_still_refuses` |
| Rootd restart does not wedge RELEASE/ACK | `rootd_restart_does_not_wedge_provider_release_or_receipt_ack` |
| Provider-side ACK outbox retry, wrong-nonce reply, overflow eviction | `provider_client::tests::lost_receipt_ack_is_retained_and_retried_until_exact_commit`, `cancel_receipts_queue_acks_and_overflow_evicts_oldest` |
| Client and provider ledgers: replay, cancel, nonce mismatch | `provider_client::tests` (remaining) |
| netd UNIX lifecycle matrix (dup, close, accept requeue) | `netd` `unix_socket_lifecycle_tests` |
| Source ordering of charge/checkpoint/refund call sites | `tools/ring3_provider_source_audit.py` (string-order aid only) |
| Kernel charge commits then copyout faults | kernel `job_ops` source witnesses; no native copyout fault test |

### Not yet executed (needs KVM fault injection)

Rootd killed between kernel refund and ACK; provider PID reuse; concurrent late
CHARGE after CANCEL across two providers; deliberately dropped terminal ACKs
over more than 4096 close/reopen cycles; socketpair partial funding under
kernel capacity exhaustion; vfsd restart with live descriptions; IPC p50/p99
regression against the pre-change baseline.
