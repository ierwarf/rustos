# Ring3 large-batch source draft: 2026-11-01

**Status: intentionally uncompiled, unbooted and not production-accepted.**
This change is a single Git tree/commit of newly authored Ring3 control-plane
code. Only rootd's bootstrap preflight is production-wired; the other
state-machine modules are *staged integration candidates*, not live systems.
Codex must repair and connect code before any of the semantics can be claimed
to work end-to-end. Do not interpret the presence of source as a completed
eight-stage migration. No performance measurement is implied.

## Ownership rules (non-negotiable)

- ring0 **must retain** syscall entry/return, usercopy, IPC transfer/reply,
  endpoint mechanics, scheduler, priority donation, futex, time, getpid/gettid,
  paging faults, prepared mappings, page accounting, kernel-only object
  ownership, TLB shootdowns and capability handle enforcement.
- Ring3 **may own** slow filesystem, namespace, identity, network, device,
  compatibility and release policy, manifests, restart scheduling,
  authenticated health, and durable recovery orchestration.
- Never add a policy RPC to a lock-held, IF-disabled, fault, IPC or scheduler
  path. Do not substitute Ring3's resource planning ledger for kernel charges.
- PID, JobId, service ID and pathname are names, not grants. Exact kernel
  sender, descriptor rights, Job generation and live lease are required.
- Kernel copyout failure after a committed mutation is **ambiguous**.
  Retain the same token and query kernel custody, rather than creating a new
  token on retry or accepting a fallback.

## Stage-by-stage draft source map

| Stage | New file | Actual role | Incomplete ownership |
| --- | --- | --- | --- |
| 2 | `resource_ledger.rs` | Bounded hierarchical resource/class accounting model, preflight and reconciliation | Kernel allocation-class admission/release at every real site |
| 3 | `capability_escrow.rs` | Provenance, attenuated rights, exact principal offer/ACK/revoke states | Kernel handle transfer authority and descriptor evidence |
| 4 | `canonical_rpc.rs` | Canonical tagged field codec and replay window | Generated-IDL cutover of each running producer/consumer |
| 5 | `component_activation.rs` | Parent/dependency ordered activation/revocation staging | Rootd/initd single transaction owner and kernel lease publication |
| 6 | `provider_sessions.rs` | Multi-provider client-funded token reconciliation | netd/vfsd RPC call sites and kernel token exact-once proof |
| 6 | `provider_wire.rs` | Strict typed provider accounting frame (not live ABI) | Generated schema, clients, rootd dispatch, refunds |
| 7 | `namespace_policy.rs` | Per-personality/namespace exact-principal slow policy | Live syscalld/procd/loaderd/vfsd/netd policy cutover |
| 7 | existing `personality.rs` | ABI negotiation and slow operation configuration | No kernel-path moves; still not live |
| 8 | `durable_recovery.rs` | CRC32C sequence/phase replay format | Authentic signed image, stable on-disk journal |
| 8 | `image_custody.rs` | Image verification/pinned previous-image lifecycle model | External verifier and independent immutable image storage |
| 8 | `reconcile.rs` | Ordered freeze/refund/revoke/terminate/restart queue | Live supervisor producer and authoritative kernel evidence |
| 8 | `rollout_plan.rs` | Parent-ready rollouts, restart budgets | Actual endpoint migration and service restart |
| 8 | `pressure_governor.rs` | Slow memory.high event policy and protected OOM floor | Polling consumer and bounded reclaim execution |
| 8 | `storage_replay.rs` | Append/sync/readback trait, exact sequence replay | storaged stable-media implementation and power-loss tests |
| 3/5 | `services/rootd/src/control_preflight.rs` | **Live pre-endpoint** cross-check of catalog, grants, exclusive slow operations | Full positive capability transfer migration |
| 6 | `services/rootd/src/provider_billing.rs` | Unconnected multi-provider rootd ledger draft | Authenticate provider, expose canonical RPC, wire kernel refunds |

The previous Ring3 modules remain published by `rustos-component-model`
and are not replaced by these drafts. The new policy models use bounded arrays,
revision/generation checks and explicit error handling, but do not own real
kernel resources. A recorded `bool` representing external verification is
*not* a cryptographic proof; it is a contract for a future authenticated
subsystem, not a security boundary.

## Rootd changes in this batch

1. `control_preflight` is imported by the production rootd supervisor and
   runs alongside component and grant bootstrap validation BEFORE endpoint
   publication. It checks the declared service inventory, rootd-only
   exception, positive authority for mandatory operations and unique slow
   operation IDs.
2. `component_policy::ManifestError` gains the already referenced
   `InvalidManifest` variant; missing variant was a visible static defect.
3. The new `provider_billing` module is imported into rootd but is NOT
   dispatched by its current supervisor protocol. It records charge intent
   before a possibly ambiguous kernel charge and keeps uncertain tokens.
4. Existing sessiond charging is unchanged. Do not advertise socket/open
   descriptor billing until netd/vfsd and rootd actually use the new wire.
5. Existing rootd update stages still track trial health in memory. Neither
   `durable_recovery` nor `storage_replay` are currently invoked on the
   update dispatch path. A trial is NOT a signed/durable release.

## Required atomic cutover sequence for Codex

### Producer/consumer ownership

- Inventory each service protocol, exact caller, user ABI and kernel broker.
- Introduce generated schema and canonical request/ACK, pin exact service
  incarnation and challenge, and migrate **both** producer and consumer.
- Reject unknown tags, overlong values, unordered fields, wrong request IDs,
  wrong operations, stale lease epochs and duplicate non-idempotent requests.
- Keep the old service wire until a live replacement handles all valid users.
- Delete old wire, scalar fallbacks and role-mask authorization only after a
  negative-right test matrix confirms no silent fallback remains.

### Client-funded providers

- Kernel accepts a charge only after exact client PID/Job-generation check.
- Rootd authenticates actual provider and verifies the positive operation
  grant and effective Job policy before spending the client Job budget.
- Reserve a token slot BEFORE invoking kernel charge. A lost ACK leaves the
  same token unresolved, not a free quota or a second charge.
- Use a kernel-owned provider PID + token tuple with generation discipline.
  Perform exact kernel reconciliation after provider process exit and rootd
  restart. No PID reuse may resurrect a retired session.
- For netd, charge the client socket session before publication, refund after
  final close/detach/peer cancellation, and cover DVM failure and blocked
  local/INET operations.
- For vfsd, choose a stable open-description charge owner, not individual
  borrowed fds. Handle dup/fork/close, checkpoint replay, forced unlink,
  failed open, in-flight replies and remote provider restart.

### Image admission and crash safety

- Loaderd verifies executable bytes, manifest, signature and trusted key,
  not an untrusted digest. Storaged pins the precise previous executable
  bytes before candidate publication.
- Journal writes must become durable with ordering: begin + old-image pin,
  candidate verified, quiesce, activate, exact health witness, commit or
  rollback-required, restore, verify, complete.
- The storage trait must connect to real flush/FUA/fsync semantics. CRC32C
  detects torn/accidental corruption but provides no adversarial integrity.
- An update ACK may be returned only AFTER the append sync/readback, and
  recovery must not truncate an acknowledged record on corruption.
- If ACK is lost, replay the exact transaction and sequence; do not append a
  different record with a new request ID to guess success.
- Restored state requires exact old bytes and durable activation. Simply
  sticky-blocking initd launches is not an image rollback.
- Failing to prove prior image custody, signature or durable sync must leave
  restart suppressed rather than silently re-enabling a candidate.

## Codex P0 repair checklist

- Statics: review no_std imports, trait signatures, Copy derives, pointer
  custody, const generics, enum indexes, overflow and allocation failures.
- ResourceLedger: check every ancestor/class overflow before any mutation;
  reconcile real kernel allocation events and slot retirement.
- Escrow: ensure lost-ACK transfer custody cannot double-deliver or leak a
  kernel handle. Confirm that revocation propagates to descendants.
- ActivationBook: reject stale plan across every parent/child edge; verify
  revocation and kernel endpoint/lease publication are truly atomic.
- ProviderBilling: unify `kernel_token` and provider token epochs across
  process death, reboot, and a recycled provider PID; retain uncertain
  charge/release receipts. Integrate with kernel v2 exact-token accounting.
- ReplayWindow: prevent replay after receipt eviction; use a kernel-authored
  epoch/watermark rather than assuming a bounded user-space ring is sufficient.
- DurableJournal: implement true persisted record readback and stable flush,
  storage fault injection and restart-owner handoff; resolve unacknowledged
  writes by reading the same journal index.
- UpdateBook: current health/promotion behavior is not the durable transition
  model. Replace with an image custody and journal-backed transaction.
- Ensure policy changes never introduce new synchronous IPC into existing
  syscall/IPC/futex/TLB/scheduler hot paths.
- Only after this source repair, run compile, unit, fault injection, formal,
  KVM 1/8-vCPU, live lifecycle storms and IPC p50/p99 tests.

## Verification declaration

This drafting session intentionally **did not execute** cargo, rustc,
rustfmt check, IDL generation/check, QEMU, KVM, CI, formal verification,
sanitizer, runtime stress or performance measurement. The content is meant
for Codex to repair, not an accepted release. All unresolved production
interfaces above remain unresolved.
