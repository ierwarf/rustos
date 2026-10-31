# Codex handoff: Ring3 recovery and service-role cutover

This is a **validation handoff**, not an assertion that the full eight-stage
Ring3 redesign is complete. The changes are in `rootd`, not in ring0.
Do not alter TLB, futex, scheduler or syscalld hotpaths to address this work.

## Scope of the new change

1. Consolidate 13 fixed service-role positive capability values in
   `services/rootd/src/service_manifest.rs` and validate them at bootstrap.
   Runtime role lookup uses that same table, no longer a second match.
2. Verify that a service-role request resolves to one exact, live service PID,
   for both bootstrap and post-init principals. A duplicate or dangling
   reporter cannot inherit authority. Every post-init reporter chain must
   terminate at the one live initd lease.
3. Make checkpoint key identity unique *within a service namespace*, even
   when its parent path differs. Admit nested checkpoint records only if
   their parent has been admitted and remains live.
4. Compute the entire descendant closure before a tombstone commit. On
   overflow or allocation failure, no checkpoint records may be modified.
   Commit atomically within one rootd policy turn (not across disk reboot).
5. Compact only a fully tombstoned exact subtree, preserving unrelated
   services and sibling roots. Proofs for recycled keys must fail stale.
6. Bound storage to 4,096 records per service and 32,768 globally. The
   per-service limit is stricter than the pre-existing ABI-wide maximum.
   No wire ABI version or guest address-space code was changed.

## Baseline quick checks

```sh
cargo fmt -p rustos-component-model -- --check
rustfmt --edition 2021 --check services/rootd/src/component_policy.rs
rustfmt --edition 2021 --check services/rootd/src/service_manifest.rs
rustfmt --edition 2021 --check services/rootd/src/service_checkpoint.rs
cargo test -p rustos-component-model --locked
cargo test -p rootd --features host-test --locked
cargo xtask check
```

Note: repository-wide `cargo fmt --all -- --check` had pre-existing
failures before this feature. Separate baseline formatting drift from
regressions in the files above.

## Required adversarial tests (not yet all executed)

- Exercise 1/2/4/8-vCPU boot, GUI readiness, service endpoint creation,
  initd bootstrap and post-init service lookup. Compare with baseline.
- Interleave lifecycle exit events with service-role and checkpoint queries.
  Inject core/core, core/post-init, post-init/post-init and reporter-only
  PID collisions, as well as PID recycling and delayed exit notifications.
  No positive role may escape after the owner or its initd root retires.
- Enumerate every service-role principal; check unknown IDs, duplicated role
  bits, incomplete tables and one-hot uniqueness.
- Exercise nested checkpoint trees at depths 1, 2, 32 and 128 with multiple
  siblings and separate service namespaces. Descendants must revoke
  together; one stale child revision must leave the entire tree unchanged.
- Inject failures into the bitmap `try_reserve_exact` and the record
  `try_reserve`; assert unchanged data and bytewise snapshots on ENOMEM.
- Check compaction idempotency, stale proof after reused key, alternate
  parent attempt, revision overflow, nonexistent parent, tombstoned parent,
  invalid payload/flags and per-service ENOSPC at 4,096 records.
- Benchmark mutation/compaction at 1, 128, 1,024 and 4,096 records, with
  the global checkpoint table near 32,768. The closure scan uses bounded
  `find_key` lookups and may still be quadratic in total record count.
  Confirm no unacceptable rootd IPC tail latency or priority inversion;
  optimize this *slow control-plane* path if the bound is too costly.
- Compare syscall/IPC round-trip p50/p99 and scheduler yield against the
  baseline. No new synchronous IPC should appear in syscall hot paths.
- Run applicable bounded formal profiles and fault injection:
  `bash formal/verify-all.sh --profile pr`.
- Validate persistent recovery separately: this checkpoint ledger is
  intentionally *not* durable across rootd process death or power loss.

## Live runtimed console-session admission

Production runtimed now caps the number of live consoles at 256 and refuses
session-handle wraparound. The handle is reserved only after its session
buffer has been admitted; a failed attempt leaves both generation and graph
unchanged, and ordinary child activation is not reached.

Codex should stress 256 live sessions with concurrent close/reopen, asynchronous
spawn cancellation, deferred-start failure, foreground-focus transitions,
graph-generation waits and app exit/reaping. Verify neither a leaked console
session nor a recycled u64 handle can grant stale authority. Confirm
non-console-hosted launch behavior remains unchanged. This quota is **not**
client-funded kernel Job memory accounting.

## Transitive post-init reclaim and client-session generation hardening

This follow-up changes production service behavior, not only the policy model:

- Explicit initd lease reclaim now discovers the full post-init descendant
  closure before changing any state. It must traverse an already-exited
  intermediate reporter, refuse PID cycles and reject a stale exact PID.
- Revoke *all* selected live service endpoints before terminating *any* of
  their processes, then reset only the selected lease records after success.
  A kernel revoke/terminate error in production must fail-closed; do not
  return a successful or partially committed reclaim receipt.
- Both exit-driven revocation and explicit reclaim share the fixed-size,
  bounded ancestry selection logic.
- Client-funded model sessions refund exact charges when generation reaches
  `u32::MAX`; the exhausted slot is permanently retired, never wrapped.
- Live runtimed's console graph generation now rejects `u64` rollover rather
  than reusing a compositor change token. Host tests exercise the guard.

Codex should add adversarial runtime tests that race initd reclaim, a
sessiond crash, uiserver exit, late PID reports, and independent netd/inputd
leases. Verify the absence of active descendant endpoints after reclaim,
and bound the number of termination syscalls to the selected subtree.
Check the above pure tests separately from 1/2/4/8-vCPU KVM experiments.
The prior KVM boot smoke missed its 25-second readiness markers, so do not
treat a model/host pass as release acceptance.

## Declarative post-init supervisor parent cutover

The exact reporter parent for each post-init service is now specified in
`POST_INIT_MANIFEST`. Validation enforces a previously defined parent and
rejects cyclic or unknown dependencies. Rootd's live parent admission and
transitive reporter-chain checks use this manifest, instead of selecting
an arbitrary live service with a matching PID. Verify the negative cases
where a real netd principal attempts to report a uiserver child, when
sessiond's own parent is stale, and when duplicate live service PIDs exist.

`initd` remains authorized to query and reclaim **only** services it
actually supervises. Do not delete the legacy fixed numeric service-ID ABI
until independent named capability handles are provisioned through kernel
and userspace. The authorization predicate itself is no longer duplicated.

## Live slow-control JobPolicy gate

Rootd now obtains a fresh kernel Job identity and inspect-only typed handle
for the authenticated service caller of checkpoint read/write/compact and
lease reclaim. It checks effective JobPolicy before modifying control-plane
state. The IDL and positive role ACL remain separate. Run tests with a
reduced effective ceiling, stale caller PID, recycled JobId and revoked parent
policy; fail closed on inspection errors. This intentionally adds slow
syscalls only to service-control operations, not the syscall/IPC hot path.
Full positive role-capability cutover is still incomplete.

## Known remaining integration gates

- Kernel-authoritative per-Job kernel-byte and service-session charges.
- Generated IDL adapters on live services; no production wire cutover yet.
- JobPolicy capability role dual-run and slow broker policy migration.
- Persistent crash storage, authenticated health witnesses and executable
  rollback transactions.
- KVM + physical device acceptance. Build/unit tests alone are insufficient.

The preceding items must remain labelled **incomplete** until implemented and
measured by an independent validation runner.

## Production IDL: post-init lease snapshot validation

The initd/rootd post-init query now uses generated schema opcode 517 inside
supervisor operation 15; old scalar operation 8 is removed. Test strict TLV
field ordering, malformed length, replayed challenge, wrong service identity,
unknown state values, exited service PIDs and recovery across rootd restart.
This is one live service operation cutover, not an entire ABI rewrite.

## Production generated-IDL reclaim transaction

Scalar lease-reclaim op 9 is retired. Initd uses generated schema 518 and
supervisor op 16; the exact service/PID/nonce and ACK phase must match.
Rootd preallocates the ACK before irrevocable kernel actions and retains
its fail-closed endpoint/terminate contract. Fault-inject OOM before mutation,
verify stale PID and ancestor cycles refuse, and stress lost ACK responses
without ever adopting a recycled PID. Full durable rollback is not claimed.

## Production client-Job console session charging (kernel/rootd/runtimed)

Rootd has a serialized 256-token accounting book. A live sessiond owner
requests charge for an exact suspended client PID; rootd resolves JobId,
then calls rights-scoped `SYS_RUSTOS_JOB_SESSION`. The kernel rechecks PID
membership while holding the job lock, checks hierarchical quotas and retains
that Job's subtree usage until the exact refund. Runtimed requests refunds
on failed activation or process reaping and never in the console I/O path.
Rootd's token book prevents duplicate charge and replay; a new live sessiond
PID causes the predecessor's charges to drain and resets its token namespace.
Test: PID recycling between lookup/admission, rootd restart, sessiond crash,
refund IPC timeout, wrong reporter, duplicate token, multiple descendant Jobs,
limit tightening, session flood, KVM 1/8 vCPU and launch-worker ACK timeout.
The generic netd/vfsd session path and durable restart journal remain TODO.

## Lifecycle settlement and worker fallback retirement

Check exact sessiond PID match during rootd lifecycle drain; refund all outstanding
client sessions on exit, check that a second exit is idempotent, and that a new
instance may restart its token namespace at one. Loaderd sets per-application
session ceilings on the same child handle as its other Job limits. The old
runtimed launch-worker-absent inline fallback is intentionally retired.
