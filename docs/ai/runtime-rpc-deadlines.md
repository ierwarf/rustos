# Runtime-control RPC deadlines and mutation outcomes

`runtime-control` and `runtimed` jointly own the local `/run/runtimed.sock`
protocol. Version 3 uses a kernel-wide `CLOCK_MONOTONIC` nanosecond deadline
for every attempt; it is not a wall-clock timestamp or a cross-VM deadline.
The client uses one local absolute deadline for connect, request, response
header and body, including retries, partial progress, and syscall time. The
server also bounds incomplete frames from acceptance, validates the wire
deadline before dispatch, and does not park a watch past it.

Launch and terminate carry a stable transaction identity, original transaction
deadline, and the complete operation. The server authenticates each connection
using `SO_PEERCRED` and its existing live role check. A bounded table binds
identity to the peer PID and exact request fingerprint; the attempt deadline
and query flag are not part of that fingerprint. At most 128 records exist.
Full means `EBUSY`, not premature eviction. A transaction's original deadline
is bounded by its immutable identity-creation timestamp plus 30 seconds, not
by each new admission attempt. All replay metadata, including unknown outcomes,
is retained until that creation timestamp plus 30 seconds plus the 60-second
replay window. At that immutable horizon it may be reclaimed; the old identity
is then rejected as stale even if a caller supplies a later deadline. This
reclaims bounded metadata only: it does not cancel device work, assert rollback,
or release any separate in-flight worker/resource custody. Repeated unknown
outcomes therefore cannot permanently fill the table.

`RuntimeClient::launch_transaction` and `terminate_*_transaction` create a
caller-owned identity. Keep that object until its outcome is reconciled.
`submit_transaction` is called once; `query_transaction` only queries and has a
new bounded **attempt** deadline, not a new transaction lifetime. Both return
`MutationOutcome`: `Pending` covers queued and admitted, `Committed` confirms
completion, `Rejected` is a validated terminal rejection, and `Unknown` means
the outcome cannot be established. `Transport` means the reply was lost or
malformed, not a terminal rejection. Query the same identity after `Pending` or
`Transport`, with backoff bounded by `query_remaining`; never create a fresh
transaction merely because the reply was lost. Query ingress errors including
`ENOENT` are unknown, not proof that no side effect happened. The previous
one-shot mutation methods were removed because they discarded the identity on
ambiguous failure. `notify_ui_ready` is separately idempotent.

Sessiond retains one outstanding identity while reconciling. Unknown outcomes
or a committed launch absent from the bounded running snapshot disable automatic
launch of that package rather than spawning again. Uiserver retains its action
gate while reconciling and logs rejection or an unknown outcome; a later
explicit user action is a new intent, not an automatic transport retry.

The launch worker checks the deadline before spawning, after spawn, while
waiting for record acknowledgement, and before activation. It gives loaderd
the same or an earlier absolute deadline. A child created after cancellation
is retired rather than activated. Once activation has started its result may
be ambiguous; any error returned by the spawn boundary is also conservative
`EOWNERDEAD`, because that boundary does not distinguish pre-dispatch errors
from a lost loader reply. Ambiguous downstream failure or worker disconnect
records `EOWNERDEAD`, not a fabricated failure or success. Only the worker's terminal
success publishes a successful launch. Termination can be partly performed;
uncertain or partial failures record `EOWNERDEAD`. A failed response write does
not erase the recorded outcome.

The replay table is in memory. The high half of each identity is its monotonic
creation timestamp. Admission requires it to be after this server incarnation
started and no later than admission; a restarted server therefore rejects an
old identity with `ESTALE` rather than executing it again. The previous outcome
is not recoverable after restart, so `ESTALE` is not proof of failure. This
also rejects an identity created before a newly started server became ready.
Durable across-restart outcome recovery would require a persistent transaction
authority and downstream integration. Query visibility is limited to the
replay window and depends on the authenticated peer PID remaining a non-reused
process identity over that window.
