# Shared library cache and boot prewarming

## Approved direction and current boundary

Library contents, prewarm selection, I/O, read-ahead and eviction are Ring3
`pagerd`/VFS policy. Ring0 provides generic source-frame capabilities, private
COW leaf installation, generation checks, charging and TLB-safe lifetime.
The same mechanisms are intended for Windows section/views; ELF, PE, Linux
paths, and library popularity are not kernel policy.

**Current integration is partial; see \"Live file mappings\".** The stage producer and parser exist;
the native source supply/withdrawal control and service-exit closure exist.
Fixed private-file/section replies now have a source-pin/COW adoption path,
but file-backed VMA admission, actual pagerd file I/O, cache reuse and the
boot-prewarm consumer still need connection. Native tests
and a serialized hint file are not runtime cache or performance evidence.

## Boot plan

`tools/xtask/src/stage/library_cache.rs` derives potential consumers from the
actual isolated final-loader selections, after linker-cache generation and
baseline libc/loader verification. It writes `/etc/pagerd/library-cache.conf`
before signing the early-system closure and before FAT serialization. This
file is in the early-system authenticated allowlist. The live consumer must
explicitly read that authenticated provider representation, not accept a
same-path product-volume shadow as trusted policy. This reader boundary remains
to be connected; allowlist membership alone does not prove runtime selection.

`libs/rustos-library-cache-policy` is a no_std host/Ring3 dependency, not a
kernel dependency. Its versioned, bounded parser is shared by the stage producer
and the planned pager consumer. Selection defaults are:

- At most **32 MiB** of page-rounded boot cache residency, **4 MiB** per file,
  **128** entries and at most **16 pages** of lazy read-ahead. The accepted
  configurable budget cannot exceed the mechanical source envelope of
  **16,384 pages / 64 MiB**. Default warming leaves at least half available
  for lazy sources; eviction remains Ring3 policy.
- The small ubiquitous ABI runtimes libc/libm/libdl/libpthread/librt/libgcc_s/
  libstdc++ are initial common candidates. Other system libraries require at
  least two distinct application installations among staged dependencies.
- All helpers/shared objects within `/apps/NAME` or `/opt/NAME` count as one
  potential application. Eleven Chrome ELF roots do not imply eleven users.
- Common candidates, then larger consumer counts, then smaller files, then
  lexical paths define deterministic selection. Rejection spends no budget;
  a later small candidate can still fit. App-private, rare and large libraries
  remain lazy, rather than copying them during boot.
- This is deliberately an **initial static policy**, not measured runtime heat.
  Runtime hit/miss/read/supply/eviction counters must inform future tuning;
  do not claim a dependency count measures frequency of execution.

The header is `rustos-library-cache-v1`, followed by tab-separated budget,
maximum file size, maximum entries, minimum application count and read-ahead.
Each subsequent line is byte length, lowercase SHA-256, and an absolute UTF-8
path. Newline termination is required; controls, CRLF, dot components, path
escapes, duplicate paths, unknown policy/version, overflow and page-rounded
budget violations fail parsing. A plan is **not mapping authority**: the pager
must independently obtain an immutable provider capability with exact backing
identity/generations and verify each warm payload before cache publication.

## Required live behavior

Pager registration cannot wait for VFS: pagerd starts first in the bootstrap
control graph. A bounded warm worker must wait for VFS readiness without blocking
control/fault service or creating a syscalld receive-owner re-entry dependency.
Prewarm loads each eligible file once into shared clean source pages. Read-only
views alias those pages; private writes allocate their own COW replacement.
Cold/large files load only touched pages, with bounded optional read-ahead.
No file is made immutable merely because its basename resembles a library.
Mutable providers need coherent invalidation/truncate handling or remain on an
explicitly named existing eager contract, never a silent security downgrade.

Cache identity includes provider, object identity/generation, content generation
and page offset, not FD number or path alone. Service publication epochs and
backing-generation changes must invalidate old source authority. Closing the
original application FD must not destroy a live view's backing authority.
Unmap/fork/exec/restart/deadline cancellation must use the existing generation
and installer-permit rules; no late mapping into a replacement MM is permitted.
Source eviction closes new source pins, not existing private views. The MM
ledger retains their physical frame until the last TLB-safe leaf release.
Source rights allow READ and optional EXECUTE, never WRITE. A writable private
view only needs read access to the immutable source; its eventual write targets
a separately owned COW frame. This allows one R/X source to serve both code
and private writable views without incompatible source permission variants.
Warm cache must stay budgeted and reclaimable, not an unbounded permanent pin.

**Registry heap accounting.** The registry's per-object page-token arrays are
ring0 heap. They are billed to the Job of the service that creates the object
(pagerd) as `kmem_charge::Class::LibraryObject` (ABI
`JOB_KMEM_CLASS_LIBRARY_OBJECT`, 8), 8 bytes per page, charged in
`Registry::create` before the allocation. A refusal returns `ENOMEM` with no
slot, reservation or charge; pagerd's warm path already maps `ENOMEM` from
create/supply to a budget refusal (skip with a report, stop warming), not a
failure. The charge lives in a `Meter` owned by the object, so retire and
lease-replacement drops refund exactly once under the registry lock (the meter
is lock-free); a repeated retire is `Stale` and refunds nothing. The fixed slot
array is static, not heap, and is not billed; supplying pages allocates no
registry heap (frame bytes are charged through the frame-source group).
loaderd sets budgets only for application Jobs, so pagerd's class limit is
unlimited by default; the existing `MAX_RESERVED_PAGES` / object-count bounds
remain the mechanical cap until an operator sets a limit.

## Live file mappings: what exists and what does not

Implemented design (differs from the earlier memfd-record plan; no memfd exists):

1. **Identity.** vfsd attests an immutable-volume file identity
   (`VfsFileIdentityWire`: volume, object, generation, storage epoch, mount
   generation, length, immutable flag) from the storage layer's read map, never
   from a path hash. `VFS_IPC_OP_FILE_IDENTITY` (26) answers by `remote_id`;
   `VFS_IPC_OP_LIBRARY_READ` (27) is pagerd-only (`validate_service_owner`) and
   returns identity plus a page-aligned chunk (at most 8 pages). Early-system
   image files (libc, libgcc_s) have no storage read map, so vfsd attests them
   as volume `VFS_EARLY_SYSTEM_VOLUME_ID`, object = SHA-256 prefix of the
   image-relative path, generation/epoch/mount_gen 1, immutable; the image
   length must equal the requested length. FILE_IDENTITY is bound to the
   client's pid: only syscalld may send it naming another pid (tid must match),
   because the mapping broker asks on behalf of the guest that owns the handle.
   rootd routes PAGERD to VFSD for the library read op only.
2. **Registry.** `kernel/compat/src/library_cache.rs` holds at most 128 sealed
   objects, 1024 pages each, 12288 reserved pages total, built through
   `SYS_RUSTOS_LIBRARY_OBJECT` (create / supply_page / seal / retire), which
   needs PagerPolicy and the live pagerd lease. Pages are frame-source tokens
   owned by the pager lease epoch; an object is invisible until sealed; a lease
   change drops the old owner's objects. The kernel zero-fills the EOF tail and
   does not hash: pagerd verifies sha256 against the manifest before sealing.
3. **Binding.** syscalld names only a candidate (`MM_BROKER_OP_BIND_LIBRARY`,
   advisory, errors ignored). The kernel asks vfsd for the identity of that
   handle's `remote_id`, matches it exactly against sealed objects, and stores
   the result in the `RemoteVfsHandle` (`LibraryBinding`). There is no
   revocation list: the binding dies with the handle (close/exec/exit/fork
   follow handle rules). The content hash is not part of the vfsd match; the
   immutable-volume identity is, and pagerd already bound hash to identity.
4. **Serving.** `broker_map_file_private` aliases at mmap time, not on fault:
   the fault path is IRQ-off and `map_cached_private_file_cow_at` needs normal
   context. Under the request lease it pins all pages of the range (all or
   nothing), publishes the `FILE_PRIVATE` VMA, then installs read-only COW
   leaves; any failure unmaps and falls back to the eager copy. Only
   page-aligned, read-capable mappings inside the file's pages qualify. A write
   COW-copies into a separately owned frame; fork COW-clones installed leaves
   and the child holds no pin.
5. **Policy.** pagerd reads `/etc/pagerd/library-cache.conf` on a separate warm
   thread once vfsd is up (`services/pagerd/src/library_warm.rs`): attested
   identity and manifest length must match, bytes stream through sha256, pages
   are supplied, then the object is sealed; any refusal retires it. A row read
   that answers EAGAIN/ENODEV/ENOSYS (boot volume's storage not serving yet)
   retries the whole run every 250 ms (<= 60 s); an already-published identical
   object counts as warm. The thread is created with `rustos_svc_runtime::worker`
   (raw `clone`; the entry pointer is passed in r12 because `syscall` clobbers
   rcx/r11). Every outcome logs once: `warmed`, `sealed nothing stage= code=
   refused=` (stage = `library_warm::Stage` ordinal), `manifest refused`, or
   `gave up`. 1-vCPU KVM boot: `pagerd: library cache warmed`.

Runtime-proven (KVM 1 vCPU, `cargo xtask kvm-smoke --library-map-probe`,
`apps/libmapprobe`, kernel diagnostic `SYS_RUSTOS_LIBRARY_MAP_PROBE` honoured
only for `/apps/libmapprobe/libmapprobe.elf`): libgcc_s (45 pages, 1296-byte
EOF tail) mapped through the bound-fd path is served by aliasing without a
copy, matches the manifest sha256 and an independent read() oracle; a second
independent mapping and a fork child hold the same physical frames read-only; a
parent write copies exactly that page and the child's frame is unchanged; the
EOF tail is zero; a fork child that unmaps two middle pages (splitting the
alias VMA) can fork a grandchild that reads every surviving page; a fork after
a private write works; munmap/exit return all leaves and frames. Marker:
`library-map-probe: shared frames=45 cow-isolated eof-zero exit-clean`.
The probe is off by default (needs the `libmapprobe-v1.env` contract written by
`--library-map-probe`).

Fork semantics (defined and tested): an alias VMA is fully populated at mmap,
fork clones every leaf, and no live path removes a leaf without removing its
VMA (madvise is a no-op in syscalld), so there is no demand-miss state to
inherit; the child needs no pin. A fault inside such a VMA would indicate a
broken invariant and goes to the pager rendezvous, where pagerd denies
(SIGSEGV), never to a fabricated page.

Retention policy (pagerd, `services/pagerd/src/library_custody.rs`): the kernel
exposes `LIBRARY_OBJECT_OP_PRESSURE` (free/min/denials/reserved) and does only
cold reclaim at retire. pagerd's custodian polls every 100 ms; when free pages
fall to 32x the minimum (~490 MiB; a hog drains the 8x margin in under one
second, measured) it retires the coldest objects (highest manifest rank,
larger first) until the shortfall is covered, logging
`pagerd: library cache reclaimed objects= pages= free= kept=`. Retirement is
permanent for the lease (no re-warm). Native tests only; not KVM-exercised.

Not done:
- Fault-driven serving of a post-mmap miss is not built (not needed under the
  invariant above).
- Re-warming a retired object after pressure subsides.
- Phase 2 (large/rare lazy files through pagerd miss I/O, read-ahead capped at
  16 pages) is not started, deliberately. Precise blockers found by reading the
  code (2026-11-13), each a prerequisite, none scaffolded:
  1. pagerd's fault resolver answers only anonymous first touch
     (`resolve_anonymous_first_touch` -> `PAGER_ACTION_MAP_ZEROED`); the
     `FILE_PRIVATE` fault path has no reply action that installs a pager-owned
     frame, and `main.rs` returns `NotManaged` for everything else.
  2. The kernel publishes a `FILE_PRIVATE` VMA only together with fully
     installed leaves (`map_library_alias`); a demand-populated VMA needs a new
     publish mode, per-page miss state, and fork/unmap/quota rules for
     unpopulated pages (today none exists, which is what makes fork safe).
  3. pagerd has no general immutable-volume range read: `VFS_IPC_OP_LIBRARY_READ`
     answers by a client `remote_id` that pagerd cannot name, so a new vfsd op
     keyed by an attested identity (with forged-identity and stale-generation
     checks) is required, plus quota charging to the faulting process.
  4. Timeout-late-apply for a miss reply needs the fixed-rendezvous adoption
     (`file_source.rs`) extended from anonymous to file pages.
  Doing this as one connected slice needs a KVM-verified kernel+pagerd+vfsd
  change; without KVM in this stage it would be unverifiable scaffolding.

Reclaim proof (runtime-proven, 1 vCPU; the probe maps before the hog's 3 s start delay):
`pagerd: library cache reclaimed objects=6 pages=1438 free=107552 kept=0`,
`library-map-probe: reclaim-survived sealed=6->0 checks=10 pages=45`, then
`rootd: oom killed` exactly the hog:
`cargo xtask kvm-smoke --oom-hog --library-reclaim-probe` writes the contract
with `mode=reclaim`; `libmapprobe` holds a live alias mapping, re-verifies its
sha256 and frames every 500 ms until the registry shrinks, then logs
`library-map-probe: reclaim-survived sealed=A->B checks=N pages=P`. The harness
also requires `pagerd: library cache reclaimed`. If the hog does not push free
pages under 32x the minimum, the probe fails after 90 s with
`no library object was reclaimed under pressure`.
- Mutable providers and non-volume files keep the eager copy.

## Acceptance

Host policy tests cover common/rare/large selection, full-page billing, bounded
entries, malformed input and deterministic selection. The ignored Chrome staging
test hashes every hint target and serializes/reopens every FAT file, including
the plan. This proves the producer representation, not the live consumer.

Required runtime evidence still includes warm-source hits without repeated VFS
reads, two independent views sharing actual frames, private-write isolation,
rare/large lazy reads, source/revoke/close/unmap lifetime, restart/cancel refusal,
and no retained-page leak after reclamation. Compare the same workload/profile
for timing claims; counters for aliased/copied bytes are not storage throughput.

Primary references: [Linux mmap](https://man7.org/linux/man-pages/man2/mmap.2.html),
[Windows section objects/views](https://learn.microsoft.com/en-us/windows-hardware/drivers/kernel/section-objects-and-views),
and [Zircon pager page supply](https://fuchsia.dev/reference/syscalls/pager_supply_pages).
The local sources remain authoritative for which parts are actually connected.
