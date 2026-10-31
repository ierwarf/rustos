# RustOS Ring3 8-stage integration draft ledger — 2026-10-31

Status: UNCOMPILED / UNBOOTED / NOT ACCEPTED. The user requested source
drafting without compile, boot or formal gates; Codex owns hardening.
This ledger distinguishes implemented source, partial integration and
missing semantics. Do not call the eight-stage migration complete.

## Mandatory kernel/user-space boundary

Kernel retains: IPC transfer and reply, scheduling, futex, getpid/gettid,
clock, memory faults, page mapping and TLB commit, trusted Job identity,
resource counters and typed handle enforcement. Ring3 retains: service
routing, positive grants, manifest, image admission, supervisor policy,
pressure response and durable recovery orchestration. No new Ring3 IPC,
heap allocation or sleeping belongs inside any kernel hot path.

## 1. Hierarchical Job lifecycle

Existing generation-bound Job tree, process directory and retirement.
The current draft extends exact token retention and cannot let a recycled
Job slot silently inherit old session or memory policy. Remaining: prove
release, generation exhaustion and all kill/reap interleavings.

## 2. Memory and kernel resource limits

Already implemented: processes.max, threads.max and resident-user-page
ledger. Drafted: memory.high event counter, memory.min policy/ancestor
constraints, rights-scoped Job QoS syscall, loaderd application high
watermarks and rootd protected victim selection.

The newly added ProcessObject Box allocation dimension uses the actual
target-kernel size_of ProcessObject, counts live plus unreaped objects and
refuses process publication above the Job ancestor's configured byte cap.
JobKmemInfo and SYS_RUSTOS_JOB_KMEM expose inspect/set using Job descriptor
rights. Loaderd queries the actual object size before configuring an
application Job budget.

This is only ONE kernel allocation class, NOT full kmem.max_bytes. Exact
allocator custody is still missing for page tables, handles, IPC endpoints,
VMA objects, thread/scheduling structures and kernel-backed buffers.
memory.high is event-only; memory.min is OOM victim exclusion, not an
actual guarantee of physical resident pages.

## 3. Negative JobPolicy and positive service principal grants

Rootd service_grants has immutable per-service slow operation permissions.
Rootd now checks exact live sender lease and reporter graph, positive grant
and effective kernel JobPolicy before checkpoint, lease-reclaim, service
lookup, principal read, client billing and authenticated health control.
Numeric role masks still appear in some legacy broker ABI and descriptor
presentation; they are NOT a complete named capability transfer mechanism.
Migration of remaining raw role decisions and actual typed route descriptor
transfer is unfinished.

## 4. Generated IDL cutover

Generated schema 514 handles heartbeat pulse/ACK, 515 candidate update
approval, 519 client-funded session accounting. Live post-init lease
snapshot/reclaim use separate canonical schemas. Session billing and
health no longer accept old scalar payloads. Many other service protocols
still use repr(C) legacy structs and require explicit versioned migration
of every real producer/consumer before those structs can be deleted.
Verify the generator output matches schemas/ring3/control.json.

## 5. Manifest supervisor unification

Shared service_catalog contains 12 services, executable identities, parents,
and core/post-init roster; initd's former pathname matching was retired.
Rootd preflights manifest, role grants, routing and reporter ancestry.
Rootd tracks exact authenticated health pulses and acts on timeouts.
Runtimed sends periodic slow, canonical heartbeats. Initd polls rootd
update status round-robin and sticky-blocks automatic restart when rollback
is required but no authenticated old image has been restored.

Remaining: single atomic component activation owner across rootd/initd/
sessiond, real kernel named capability custody, all-provider heartbeat
coverage, and exact coherent graph transition under restart.

## 6. Client-funded sessions

Kernel validates client PID/Job membership and all ancestor session limits.
JobSession ABI v2 stores provider PID + token, atomic Job counts, high-water
cancellation markers and replay-safe refund. Records survive rootd PROCESS
restart; rootd startup and provider exit drain kernel-resident charges.
Runtimed charges suspended children before activation; loaderd sets app
session ceilings; rootd retains bounded receipts. Missing: netd/vfsd and
other provider session billing, exact SMP cancellation and PID-reuse proof,
and provider-lifecycle race acceptance.

## 7. Ring3 policy and personalities

Move only slow configuration, identities, filesystem/network policy,
lifecycle decisions, image parsing and personality controls into services.
Leave kernel-local IPC/futex/scheduler/clock/getpid/fault/TLB mechanisms in
ring0. Current draft expands slow operation grant boundaries and leaves
fastpaths unchanged. Remaining: full migration of old Linux/Windows broker
authorization and versioned compatibility across service calls.

## 8. Health, checkpoints, upgrades and rollback

Kernel rootd-only checkpoint journal stores validated operation descriptors,
survives rootd PROCESS death and refuses history exhaustion. Rootd preflights
checkpoint DAG changes, appends before ACK, and replays on rootd startup.
This journal is MEMORY-RESIDENT and does not survive power loss.

Rootd UpdateBook tracks staged, armed, trial, committed, rollback-required,
candidate PID, deadline and exact health. A candidate cannot satisfy trial
through claimed service name alone. Initd sees rollback-required and refuses
automatic launch of a potentially changed image until independent restoration.

NOT IMPLEMENTED: authoritative signed previous-image custody, real byte-
signature verification of new release in the update transaction, durable
filesystem journal and fsync, transactional old-image restoration, replay
after power loss, and end-to-end verified release acceptance. A rollback-
required state is NOT a completed rollback.

## Codex P0 repair and acceptance gates

- Compile new JobKmemInfo, JobKmem ABI and loaderd call chain; check exact
  loaderd handle rights and ProcessObject Box-size kernel accounting.
- Verify process publication rechecks the resource budget under the same
  ProcessTable lock. Reservation alone must not authorize publication.
- Prove kernel token ledger exactly-once CHARGE/RELEASE/DRain across copyout
  faults, PID reuse, Job slot recycle, simultaneous tokens sharing ancestors,
  late IPC, rootd restart and sessiond exit.
- Check new SlowBrokerOperation semantic IDs 36-39 and corresponding positive
  grants; negative JobPolicy must be checked AFTER authenticating sender.
- Validate generated IDL canonical framing: unknown/missing/duplicate fields,
  wrong phase, PID, epoch, service, challenge, tag ordering and length.
- Prove checkpoint kernel append before ACK, service/key/revision/op duplicate
  identities, log exhaustion and replay consistency after rootd crash.
- Never infer signature or prior-image custody from an untrusted digest or
  from a manifest pathname. Confirm update status cannot self-commit.
- Verify health timeouts cannot terminate a foreign or reused PID, and that
  rootd can restart healthy services without stale witness authority.
- Measure IPC/scheduling no-regression only after source hardening.

## Still-open major implementation

- Complete kernel-object byte charging in every real owner and release site.
- Actual transferable kernel capability endpoints and final positive-grant
  cutover; eliminate remaining numeric role-bit privilege checks.
- Full generated-IDL migration of remaining operational service protocols.
- Unified atomic component activation, consistent parent ownership and
  service crash rollback.
- Bill netd/vfsd clients, not service providers.
- Complete Ring3 slow personality/policy migration without hotpath changes.
- Persistent power-loss-safe storage, signed upgrades, exact old image
  rollback and accepted health recovery.

The writing assistant performed no compiler, boot, formal, dynamic test,
or performance verification for this source batch. This is an intentionally
aggressive Codex repair draft, not a product release.
