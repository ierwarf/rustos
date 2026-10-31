# Ring3 component model — implementation and cutover ledger

**Status (2026-10-31): partial integration. The eight-stage migration is NOT
complete.** This document distinguishes shipped source changes, pure policy
models, and missing kernel/service enforcement. Do not advertise the new
library as replacing the existing rootd/Job/IPC authorities.

## Applied integration

- `rustos-component-model` is a no_std, no-unsafe, userspace-only library.
  It has generation-bound `ComponentId`, bounded `Graph`, hierarchical
  `JobCeiling`, single-use charge tickets, scoped capability offers,
  canonical-encoding IDL/field checks, restart policy, memory-pressure and
  client-funded session *policy*, health probes and update/rollback models.
  Each capability offer now carries a monotonic issuance epoch: withdrawn and
  subsequently reissued offers do not revive an earlier session. The
  `Sessions` policy charges exactly one session unit to its client and
  scavenges revoked-route charges without waiting for their TTL.
- `rootd::component_policy::preflight` runs **once before rootd endpoint
  publication**, checks the immutable bootstrap/post-init manifests, rejects
  duplicate service identities, invalid paths, malformed scheduling contexts
  and non-forward bootstrap dependency edges. It performs no kernel syscall.
- `rootd::component_policy::route_allowed` consumes the immutable
  `SERVICE_ROUTING_MANIFEST` of 15 directed lookup offers alongside
  bootstrap-dependency grants. Bootstrap preflight rejects unknown service
  identities, self-edges, duplicate offers and redundant bootstrap grants.
  An exhaustive old-versus-new authority-matrix test covers the current
  service inventory, including unknown principals. Rootd still binds exact
  sender identity and lease generations; the table grants **no** kernel
  handles and inserts no additional synchronous IPC.
- `rootd` requires each running service to have one unambiguous PID
  and checks post-init reporter ancestry before admitting UI service identity.
  A conflicting live lease is denied, including when an old exit report is
  delayed. Existing kernel-stamped sender identity is still authoritative.
- `rootd::ServiceCheckpointStore` grows records on demand rather than
  reserving all 32,768 slots during bootstrap; failed checkpoint or scan
  allocations return ENOMEM instead of panicking. This is still an
  **in-memory** restart checkpoint, not durable crash storage.
- Ring3 IDL schemas are authored in `schemas/ring3/control.json` and
  converted deterministically to no_std Rust field definitions by
  `tools/ring3_idl_codegen.py`. CI rejects stale generated files.
  The five declarative control-plane schemas are staging contracts only:
  production IPC and syscall structs have **not** been switched over.
- `Graph::from_manifest` admits topologically sorted nested components
  one parent at a time before admitting each child. Its output is private
  until all exports and offers validate; malformed dependency indices
  fail closed. A dedicated nested-realm regression test covers activation,
  capability routing and descendant revocation.
- `rootd` service-role capabilities are now defined in one
  `SERVICE_ROLE_MANIFEST` with exactly 13 roles. Bootstrap rejects unknown,
  duplicate or overlapping service role definitions. Positive capability
  checks also bind the caller to an unambiguous live lease. Post-init reporter
  chains must end at the one live `initd` identity and reject duplicated
  ancestors. Rootd remains the authority: no kernel role or JobPolicy ABI
  was changed.
- `ServiceCheckpointStore` now supports nested parent-before-child
  checkpoint trees. Exact per-service keys cannot be silently reparented,
  ancestor tombstones mark their **entire descendant closure** atomically,
  and compaction removes only an exact, already-tombstoned subtree.
  Per-service storage has a 4096-record cap below the unchanged global
  32768-record ABI limit. A fallible, bounded bitmap keeps descendant
  selection independent of nesting depth; the control plane is still
  in-memory and is not persistent crash storage.
- The production `runtimed` console-session owner now limits live
  console sessions to 256, returns ENOSPC on resource saturation, refuses
  zero handles, and never wraps the externally visible u64 session identity.
  Both cooperative-worker and inline launch paths admit the console session
  before creating a child, and only advance the handle after success. This
  is a Ring3 *local quota*; it does not replace kernel Job charging or make
  the in-memory policy session ticket kernel-authoritative.
- On reporter exit **and** explicit initd lease reclamation, rootd prepares a
  bounded transitive post-init dependency closure before any kernel operation.
  Reclamation also traverses exited intermediate records. Every live descendant
  endpoint is revoked before initiating termination of any process; a failed
  production revoke/terminate request fail-closes rather than returning a
  partially successful lease cleanup. Regression tests cover nested trees,
  stale PIDs, cycles, unrelated leases, and revoked-before-terminate order.
- The no_std client-funded session policy now retires exhausted generation
  slots while refunding their exact charge. A formerly occupied slot can never
  wrap its generation and revive an old SessionId. The live runtimed console
  graph generation likewise refuses rollover instead of reusing an old token.
- `rootd` and the component model have focused host tests; this work
  leaves the live fixed service registry and exact kernel lease owner intact.

## Eight-stage status

| Stage | Implemented source/evidence | Remaining work |
| --- | --- | --- |
| 1. Job lifecycle | Existing ring0 generational Job tree and typed descriptors (unchanged) | Hardware qualification, not reimplementation |
| 2. Accounting | Existing counted process/thread/user-page limits; new userspace `pressure` model | Authoritative kernel-byte charge, memory.high/min admission/pressure wiring and acceptance |
| 3. JobPolicy | Existing 256-bit kernel policy ceilings; typed slow-operation registry and live 13-role rootd authority manifest | Assign positive subject grants and dual-run cutover of legacy service-capability checks |
| 4. IDL/runtime | Canonical `idl` codec, schema validator/code generator and five generated staging interfaces | Generated production bindings, ABI versioned adapters and live service migrations |
| 5. Components | New generation-bound Graph; live rootd manifest and 15-offer route validation, full legacy-route equivalence witness, and exact live service PID collision/reporter-chain denial | Declarative activation, named kernel capability transfer and consolidated production supervisor ownership |
| 6. Session budgets | New client-funded session policy with exact-generation tickets, overflow retirement, offer-issuance epochs, mandatory one-unit session charging and revoked-route scavenging | Binding client-funded service allocation to kernel Job budget and provider-failure accounting; live runtimed has an independent 256-console-session bound |
| 7. Personality split | Existing syscalld/procd/loaderd/vfsd policy split preserved | Audit remaining **slow** ring0 ABI policies and cut over with measured cost; no hotpath moves |
| 8. Operability | New health probe, restart, event and update/rollback policy models; live bounded nested checkpoint tree, transitive exit/reclaim revocation and exact compaction | Actual persistent crash store, authenticated health reporter, image admission, restart transaction and KVM rollback |
 
The model must never be mistaken for a security boundary. A policy decision
does **not** mint a capability or authorize a syscall. Rootd and ring0 must
still check the exact live principal, typed descriptor, version and endpoint
generation. A session's user-space ticket is not kernel memory charging.

## Explicit ring0 / TLB exclusions

No TLB code is changed. **Do not migrate** these fast or atomic mechanisms
out of ring0: futex wait/wake/requeue and robust-list cleanup, local
clock_gettime/timer wait paths, getpid/gettid, scheduler yield/block and
thread substrate, syscall entry, usercopy, page faults, page-table commit,
TLB invalidation and exact-ACK reclaim. Preserve the existing bootstrap-local
ELF, initial address-space and emergency console exceptions. For mmap/brk
keep the existing syscalld-policy versus kernel-mm commit boundary.

Changes in the service control plane must not add a synchronous IPC round
trip to any of those paths, even if a component router becomes available.

## Verification and acceptance boundary

Focused CI executes:

```sh
python3 tools/ring3_idl_codegen.py --check
cargo fmt -p rustos-component-model -- --check
rustfmt --edition 2021 --check services/rootd/src/component_policy.rs
cargo test -p rustos-component-model --locked
cargo test -p rootd --features host-test --locked
```

These are compile and state-machine witnesses, not KVM boot, SMP scheduling,
formal-model or hardware stability evidence. Before a production cutover,
validate `cargo xtask check`, the applicable formal profiles, 1/2/4/8-vCPU
KVM smoke, service termination/restart storms, Job/frame leakage, and IPC
performance relative to the prior release.

Related contracts: [Ring3 roadmap](ring3-process-model.md),
[Job model](job-model.md),
[commercial gates](../ai/commercial-quality-gates.md).
