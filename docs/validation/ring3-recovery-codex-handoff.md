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

## Known remaining integration gates

- Kernel-authoritative per-Job kernel-byte and service-session charges.
- Generated IDL adapters on live services; no production wire cutover yet.
- JobPolicy capability role dual-run and slow broker policy migration.
- Persistent crash storage, authenticated health witnesses and executable
  rollback transactions.
- KVM + physical device acceptance. Build/unit tests alone are insufficient.

The preceding items must remain labelled **incomplete** until implemented and
measured by an independent validation runner.
