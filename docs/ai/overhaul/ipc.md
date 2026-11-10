# IPC core: synchronous request/reply map, findings, design (area `ipc`)

Working notes of the `ipc` agent (see `README.md`). Status: done / disconnected /
unverified at the end.

## 1. Lifecycle of one synchronous call (caller C, server S, reply R, endpoint E)

Publication points (P#) are the stores/CASes another CPU can observe. "Leaf" =
an `IrqLeaf` lock; class numbers are the lockdep classes in module headers.

| # | Step | Code | What is published | Order / guard |
|---|------|------|-------------------|---------------|
| P1 | C reserves funding | `donation_publication::reserve_call[_with]`, `donation_ledger::reserve_own_lazy` | ledger reservation (donor -> R pending) | ledger leaf; before any wait arm |
| P2 | C enqueues the request on E (runtime) | `kernel-ipc-runtime` endpoint queue, reply object `AwaitingReceiver` | message + reply object | runtime locks; scheduler not involved |
| P3a | Fast path: C arms `EndpointReply(R)` **before** the trap | `current::arm_block_current_task_on_reply`, `CurrentWait::arm` | wait reason + epoch (Wait58) + owner `wait_armed` | arm precedes publication of the wake to S |
| P3b | Slow path: C polls, **then** arms `EndpointReply(R)` | `compat .../reply_wait.rs` | same as P3a, but *after* P2 | the post-arm re-poll is the race fix |
| P4 | Receiver wake (S parked on `EndpointReceive(E)`) | lane: `ipc_lane::call` -> `finish`; fused: `ipc_publication::call_switching` -> `publish_switching`; cold: `call_before_arm` -> `wake_publication::receive_call` | donation bind (ledger), typed pending phase (Wait58), owner CAS `Blocked -> Running` (lane) / `-> DirectHandoff` (local) / `-> RemoteQueued` + mailbox (remote), FIFO record (FIFO56) or `DirectSwitch` loan | see 2. Only ONE of the three paths runs per call |
| P5 | C commits its block | lane: `runqueue::commit_wait(c)` before `finish`; fused: `sender.commit()` after P4; slow: `commit_block_current_task_and_yield` | owner word Blocked (or WakeWon if a reverse reply already won) | the Current wait CAS arbitrates once; never a scalar `set_blocked` |
| P6 | C parks resident (lane only) | RSP ACK -> `resident::park` | resident cell (pin+permit) for C's owner generation | evicted by ANY other owner transition (`RunOwnerWord::compare_exchange`) |
| P7 | S completes R (runtime) | `complete_fast_reply_for_task` / `complete_endpoint_reply_*` | response in reply object, `ReplyCompletion` (caller + SC custody) | before any wake of C |
| P8 | Reply wake of C | lane: `ipc_lane::reply`; deferred: `pending_reply::stash` -> `take_for` (fused into S's receive) or `settle_deferred_fast_ipc_reply[_in]`; immediate: `ipc_publication::reply[_switching]` (fast) or `wake_publication::reply` (slow) | donation release (ledger), typed wake (Wait58), owner CAS, best-effort ReplyWake FIFO token or `DirectSwitch` | donation release precedes resolution, even on refusal |
| P9 | Refusal retry | `wake_retry::record_in` (per-CPU queue, serviced at every `DispatchEntry`) | epoch-exact retry token | only while that exact wait instance is still published |
| P10 | Deadline | `arch::rtc` sleep waiter armed by the caller; expedited only for a deferred refusal | timer wake -> re-poll | backstop; a response that is found after expiry is discarded (ETIMEDOUT) |
| P11 | C takes the response | `take_endpoint_response_for_wait` / register return | releases priority (`release_ipc_priority`) | after the wake or at the post-arm re-poll |

### 2. The three receiver-wake paths and their custody

- **Lane** (`ipc_lane`): both tasks resident on this CPU. Every check runs
  first, then the donation bind. `commit_wait(c)` is the commit point. The point
  of no return is `runqueue::lane_claim` (owner row `Blocked --LaneClaim-->
  Running`, duty `ResidentTake`, discharged by `LaneClaim::take_resident`)
  inside `lane_wake_clear` inside `with_placement_admission`. Every failure after the commit is `Generic`, and
  the receiver goes to the ordinary wake.
- **Fused DirectSwitch** (`publish_switching` with `switch`, local target,
  `direct_switch_admitted`): the receiver becomes `DirectHandoff` with NO FIFO
  record and NO fair membership. `DirectSwitch` holds its frame reader. Dropping
  an unconsumed `DirectSwitch` materializes the receiver
  (`materialize_direct_handoff_with_generation`), and so does `fast_switch` on
  any refusal. A failed `from_held` publishes the ordinary FIFO record
  instead.
- **FIFO** (`sync_handoff::publish_original`): the capacity check, the owner CAS
  and the record publication all happen under the target FIFO leaf, then
  `pending = true` is set (Release) under that same leaf. The consumer
  (`dispatcher::synchronous` via `take_next_ready_with`) takes one record per
  leaf hold and qualifies it outside the leaf. Every discard of a record that
  carries a claim generation (Direct / ReplyWake / bound Generic) materializes
  it, so a DirectHandoff owner is never left without a dispatcher. Pending is
  cleared only after the consumer has seen the queue empty under the leaf.

Reply direction: `wake_publication::reply_switching` first tries
`ipc_publication::reply_direct`, which runs only for a same-CPU recent sleeper
and goes through the same `publish_switching`. Otherwise it does the ordinary
typed wake plus a best-effort ReplyWake token. Both directions keep the typed
pending phase (`TypedWake`) from capture through every effect, so no new arm
can interleave.

### 3. Interleavings that are possible, and their outcome

| Interleaving | Outcome | Status |
|---|---|---|
| Reply (P8) before slow caller arm (P3b) | typed capture finds no `EndpointReply(R)` wait, so the wake refuses; the caller's post-arm re-poll takes the response | benign; now `NotWaiting` (was logged as `ipc-reply-wake-refused` and expedited the caller's deadline) |
| Reply after caller timeout / cancel | wait already completed: `NotWaiting`; response dropped by cancel | benign |
| Reply while caller is between Current and a published frame, frame reader busy, generation-publication gap | wake refuses with the wait still published: retry recorded (P9), deadline expedited | benign; recovered at the next `DispatchEntry` (`Deferred`) |
| Ordinary reply wake succeeded but the ReplyWake token could not be built (owner already RemoteQueued / claimed elsewhere) | was reported as a refusal | fixed by `owner` (`publish_reply_binding` best-effort) |
| Lane committed, claim lost (affinity/funding/raced CAS), ordinary fallback wake refuses | **receiver or caller stayed Blocked with its message/response published and NO retry**: a receiver strands (no deadline); a caller sleeps to its deadline and then discards the response as ETIMEDOUT | **defect, fixed** (F1) |
| Reverse reply wins before the caller's commit (P8 before P5) | the Current wait CAS returns WakeWon; the caller keeps running; the fused `DirectSwitch` is refused by `fast_switch` (`!blocked(current)`) and materialized | correct (witness `actual_call_reverse_wake_before_original_sender_commit_cannot_be_overwritten`) |
| A deferred fast reply (`pending_reply`) and a timer wake of the caller | settle finds `NotWaiting` | benign |
| FIFO budget exhausted while a fair competitor is ready | records stay queued, pending stays set; the next non-synchronous dispatch resets the streak | correct (bounded fairness) |
| Generic record dedup over a Direct/ReplyWake record | never downgrades (`refreshes`); a fresh Direct supersedes a stale ReplyWake | correct |

## 4. Root cause of the two log lines

Evidence: `rustos-evidence/4vcpu-100-round2-2026-11-09/lane2-*.log` (the only
debugcon excerpt with these lines) and the round-1 panic logs.

**`ipc-reply-wake-refused`** (slow reply, `current/ipc.rs`). There are 10 lines
in lane2: vfsd -> loaderd (task 10), rootd -> initd (task 22), netd -> task 55.
In every case the caller's next activity follows within about 10 ms (for
example, loaderd's `proc-map-file-batch` 10 ms after the refusal at 1.243 s). No
`kernel-scheduler-wake-retry-exhausted` appears in any evidence log, so no
refused wait stayed published for the 4 s retry window. The slow caller arms
*after* enqueueing (P3b), and the server replies from another CPU. The dominant
case is therefore "reply raced ahead of the arm", a legitimate protocol outcome.
The rest are "woken, but ordering token lost" (reported as a refusal, now fixed
by owner) and transient refusals recovered by the retry. Verdict: benign. It was
not silent, but it was misclassified, and it expedited the deadline of a task
that was not waiting, which cut an unrelated timed wait short. Not a lost wake.

**`ipc-service-reply-timeout`** (`compat ipc_ops.rs`, arg1 = `0xfa_0000006e` =
250 ms / ETIMEDOUT). There are two bursts:
1. ~0.64 s: the heartbeat threads (`rustos_svc_runtime::heartbeat`,
   `CALL_LIMIT_MS = 250`) of loaderd, procd and pagerd call rootd at ~0.39 s.
   rootd is inside `wait_for_core_readiness_with`
   (`services/rootd/src/main.rs`): 25 x 10 ms `SYS_RUSTOS_ROOTD_WAIT_BROKER`
   sleeps that end early only on readiness, not on pending requests. rootd
   therefore does not serve its endpoint for up to 250 ms, and every 250 ms
   heartbeat sent in that window expires. Userland policy, not a kernel defect.
   The heartbeat retries after 2 s with the same sequence ("first pulse" lines
   at ~1.9 s). Reported to the lead (rootd is outside the four areas).
2. ~34-48 s: after the `terminal_publication.rs:212` panic at about 18-34 s
   (lifecycle area). This is fallout of the panic.

The README's dominant stall family (`AwaitingPolicyConsumer`, a new task making
no progress) does not show up as IPC refusals. Nothing in these lines points
at a lost IPC token. F1 below is the one IPC path that could produce a stranded
receiver or a 250 ms-class reply timeout with a response already published.

## 5. Bugs found and fixed

- **F1, lane fallback dropped a refused wake** (`ipc_lane.rs`). `call` and
  `reply` ran the ordinary wake after the lane had committed (caller Blocked /
  response published) and ignored a refusal: no `wake_retry` record. Fix:
  `call_fallback` / `reply_fallback` -> `wake_or_retry` records an epoch-exact
  retry of the receiver's `EndpointReceive(E)` / the caller's `EndpointReply(R)`.
  Regression tests: `ipc_publication::tests::refused_lane_{reply,call}_fallback_keeps_an_exact_retry_*`.
  Both fail with the retry removed (checked by mutating the fix in a worktree).
  Mutation row `ipc-lane-fallback-refusal-dropped`.
- **F2, refusal classification + spurious expedite** (`current/ipc.rs`,
  `ipc_publication.rs`, `FastIpcReplyHandoffOutcome::NotWaiting` in
  `scheduler.rs`). A refused terminal wake is now `Deferred` (retry recorded;
  deadline expedited; milestones `ipc-reply-wake-deferred` /
  `ipc-fast-reply-wake-deferred` / `pager-fault-wake-deferred`) or
  `NotWaiting` (counted under `ipc-terminal-wake-not-waiting`, no expedite).
  Each class keeps its own counter; a milestone fires for the first 16 and then
  at powers of two, with arg0 = total << 32 | task and arg1 = reply/token. Only
  `LocalFallback` gives a dispatch hint. Tests:
  `refused_terminal_wake_is_deferred_only_when_a_retry_was_recorded`,
  `slow_reply_retry_records_only_the_exact_published_wait`, and the updated
  `actual_reply_foreign_context_and_wrong_wait_refuse_without_generic_fallback`
  / `actual_reverse_reply_uses_original_typed_wait_fair_owner_and_exact_fifo`.
  Mutation row `ipc-fast-reply-not-waiting-reported-deferred`; row
  `ipc-original-reply-refusal-scope` re-pointed.
- **F3, a successful reply wake reported as refused** (owner's
  `wake_publication::publish_reply_binding`): reported to owner, who fixed it.
- One shared scoped fast-reply producer (`publish_fast_reply_in`) for the
  immediate and the scheduler-entry (deferred) path. The deferred path used to
  `return` silently on missing SC custody. It now fails closed like the
  immediate path (a stash cannot exist without custody).

- **F4, rootd readiness wait never served its endpoint** (`services/rootd`,
  assigned by the lead). The bootstrap loop slept out each 250 ms readiness
  window. It now ends every turn with the same idle boundary as the post-init
  supervisor (`bootstrap_idle_boundary`): a bounded receive of
  `ROOTD_SUPERVISOR_IDLE_POLL_MS` that serves a control message at once, or a
  yield when the drain hit its budget. Readiness is re-checked every turn, and
  the readiness budget is still charged by monotonic time. Test
  `bootstrap_readiness_wait_serves_the_supervisor_endpoint_within_one_idle_poll`;
  mutation row `rootd-bootstrap-readiness-wait-skips-endpoint`. Contract:
  `docs/ai/abi/core.md` (supervisor loop) and `formal/models.md`
  (`rootd-restart-backoff` text). `SYS_RUSTOS_ROOTD_WAIT_BROKER` has no
  caller left (kernel broker + ABI constant: lead's decision).

## 6. Owner API migration (done)

- `ipc_publication::publish_switching`: `runqueue::claim_blocked_handoff_duty`.
  The `HandoffFifo` duty is discharged by `sync_handoff::publish_original`
  under the FIFO leaf after the record and pending store, or by the fused
  path once its `DirectSwitch` is built (whose Drop materializes) or after its
  fallback FIFO enqueue. The `Mailbox` duty is `duty.publish_mailbox(weight)`
  after every leaf ends.
- `sync_handoff::publish_original(.., commit: FnOnce() -> Option<(RunOwnerSnapshot,
  Option<EnqueueDuty>)>)`: the cold-call hint passes `None` because its typed
  wake already published fair custody.
- `ipc_lane::finish`: `lane_claim` + `take_resident`. Owner has deleted the
  wrappers (`claim_blocked_handoff`, `publish_handoff_mailbox`, `lane_cas`,
  `lane_running`).
- IPC still reads `runqueue::owner(slot)` snapshots for precondition checks
  and calls `commit_wait`, `publish_return` and
  `materialize_direct_handoff_with_generation`. Owner kept these signatures and
  routes them through the table.
- New `sync_handoff::holds_direct_record(cpu, slot, generation)` for owner's
  conservation scanner (periodic path only).

## 7. Deleted

- `donation_publication::bind_lane` (no caller; doc merged into `bind_lane_current`).
- `handoffs::published_handoff_lifecycle` and its test: a test-only mirror of
  the dispatcher's admission that no production path executed, together with its
  mutation row (`scheduler-handoff-lifecycle-missing-identity`) and
  `smp-source-contracts.toml` sequence. The production check stays pinned by the
  `dispatcher.rs fn synchronous(` sequence. Also deleted the duplicate test
  `exact_fifo_identity_only_falls_back_on_incomplete_publication`, whose body
  was identical to its neighbour's and which no registry referenced.

- `SyncHandoffState::{handoff_streak, last_handoff_tick, record_dispatch,
  handoff_streak(), set_handoff_streak(), take_next_ready_with_competitor}` and
  the competitor branch of `take_next_ready_record`: a test-only copy of the
  chain fairness budget. Production has always used `SYNC_HANDOFF_BUDGET`
  (the old code even asserted the mirror stayed 0). Its mutation row
  `handoffs-sync-budget-bound` mutated code that production never executed and
  is deleted. The fairness test now drives the production `record_dispatch` /
  `take_next_ready_with` on a private CPU index, and also asserts that an
  exhausted budget keeps the records and the pending flag. Row
  `handoffs-actual-sync-budget-bound` was re-pointed to occurrence 1.

## 8. Design going forward (one page)

1. **One outcome type per terminal wake.** Every IPC terminal wake (reply,
   pager reply, lane fallback, receiver wake) returns `Woken | Deferred |
   NotWaiting`, with no bare bool. `Deferred` exists only when `wake_retry`
   holds the exact wait instance. This is invariant I-IPC1: *a completion whose
   target still waits on its object is either woken or has a retry recorded
   before the producer's scope ends.* Today's sites: `current/ipc.rs` (slow,
   fast, deferred, pager), `ipc_publication::reply_published`, `ipc_lane::*_fallback`,
   `cpu_local::irq_entry` deferred wakes (already retried),
   `compat recover_unwoken_receiver` (a userland-side re-park; can be deleted
   once every receiver-wake refusal is retried in the kernel, which is now the
   case for the lane and cold paths. Left in place: compat is not this area's
   file).
2. **Publication points kept** (measured benefit, see memory notes): lane
   (41.9k -> 14.4k), fused DirectSwitch (-11%, about 4k at the time), FIFO
   (cross-CPU and fallback ordering). The ReplyWake token is urgency only: it
   never has to carry custody, because the ordinary wake already published fair
   membership. Direct records are the only FIFO records that ARE custody (a
   DirectHandoff owner has no fair membership). Their invariant is "consumed
   by `synchronous()` or materialized on every discard", which holds today
   (section 2).
3. **Owner API**: done (section 6). Every IPC claim into runnable custody
   carries an `EnqueueDuty` that panics if dropped (I1).

## 9. Hot path

The success paths of call and reply are unchanged. Every edit is on a refusal
path (`!woken`) or replaces one comparison with another (`!= Rejected` -> `==
LocalFallback`). Added on any success path: 0 atomic RMW, 0 locks, 0 cache
lines, 0 TSC reads. The new counters are `Relaxed fetch_add` on the refusal
path only, the same as the previous single counter. There is no new
measurement; see the final report.

## 10. Disconnected / unverified / deferred

- Not verified in KVM (not allowed for this agent). Host tests: full
  `cargo test -p kernel-ps --lib` (1096 passed) and `-p kernel-ipc-runtime`
  pass in the shared tree. Focused mutation rows were run in the worktree
  `/home/hongii2/wt-ipc`, because the shared registry preflight fails on the
  owner's pending anchors.
- `donation_publication/tests/legacy_witnesses.rs`: called from shared
  `scheduler/tests/part_{1,2}.rs` and pinned by `check-dispatch-source-contracts.py`
  and `smp-source-contracts.toml`. Not deleted; needs an audit of which newer
  witnesses subsume each of its seven helpers.
- `SYS_RUSTOS_ROOTD_WAIT_BROKER`: deleted end to end (compat broker handler
  and dispatch arm, `ROOTD_WAIT_MAX_MILLIS`, its bound test, the user-ABI
  constant, its linux.rs re-export and its pin in lib.rs). The number
  0x5255_003b is retired with a "do not reuse" comment, following the 0x5255_0037
  convention. A repo-wide rg, including generated and ignored files, found no
  other reference.
- `donation_publication/tests/legacy_witnesses.rs` audit: five helpers, not
  seven. `bounded_capacity` (ledger MAX_TASK bound), `self_and_terminal_refusal`
  (self-loop refusal at both layers + sealed-target bind/pager refusal),
  `bounded_chain` (depth 8 root compression, no truncation), `nested_interval`
  (root billing across reply withdrawal) and `transitive` (nested boosts
  revoked once) are each the only test of their invariant, so all are kept.
  The one redundancy, a second wrapper of `transitive`
  (`donation_ledger::tests::root_reply_revokes_every_nested_priority_boost_exactly_once`),
  is deleted, and row `scheduler-donation-ledger-release-counter` now points at
  `scheduler::tests::synchronous_ipc_donation_promotes_and_revokes_a_transitive_user_chain`.
