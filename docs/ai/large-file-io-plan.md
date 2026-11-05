# Large-file I/O and filesystem expansion plan

Status: proposed implementation plan, 2026-11-05. This planning task changes
no runtime code. Baseline: `c1f4badc` plus the uncommitted Chrome desktop work.
Owner boundaries remain [VFS/storage](abi/vfs-storage.md),
[loader/snapshot](abi/loader-scheduling.md), [pager](pager-protocol-contract.md)
and [Job accounting](../architecture/job-model.md).

## 1. Decision and scope

Build one bounded, filesystem-independent regular-file I/O path. Adapt FAT to
it first; add ext4 and NTFS as separate backends later. Do not make the common
interface a collection of FAT path helpers, or grow every buffer with file size.

Keep namespace, credentials, mount/launch policy and open-description state in
Ring3 vfsd; filesystem parsing/allocation in Ring3 filesystem workers; validated
block requests in storaged; hardware drivers in the Linux DVM. Ring0 retains
capability, frame, user-copy and mapping mechanisms. No filesystem parser,
cache policy, namespace shortcut or direct hardware fallback moves into Ring0.
The filesystem workers may initially be vfsd-owned workers; a new process per
filesystem is not a prerequisite.

Chrome is the first large executable witness, not the definition of this API.
Its `startup=none` desktop policy remains unchanged. Shared-library, sandbox,
Wayland and other browser blockers are separate work after storage admission.

## 2. Confirmed baseline and reuse

| Evidence | Current behavior / consequence |
| --- | --- |
| `build/chrome-first-blocker/user-launch-debugcon.log` | User launch fails at `vfsd: executable snapshot rejected stage=open errno=75`; loaderd propagates EOVERFLOW. It has not reached shared-library resolution. |
| Chrome ELF: 306,789,704 bytes (~292.6 MiB) | Exceeds both 134,217,728-byte executable snapshot bounds. |
| `services/loaderd/src/main.rs`, `services/vfsd/src/main.rs` | Both snapshot limits are 128 MiB; the loader request carries that maximum. |
| `services/vfsd/src/storage_owner.rs` | Allocates a whole-file Vec and performs one read under storage ownership. Simply splitting current range reads restarts FAT traversal. |
| `services/vfsd/src/main.rs` | Copies that Vec into terminally sealed memfd storage. Snapshot cache budget is separately 64 MiB. |
| `libs/rustos-user-abi/src/performance.rs` | Snapshot transaction deadline is 4 seconds, not a large-transfer progress protocol. |
| `libs/storage-fat/src/lib.rs` | Range reads reopen by path and seek from the start; `FatFile<'_, D>` borrows its volume. FAT write/truncate helpers already exist. |
| `services/vfsd/src/lib.rs`, `state_storage.rs` | Persistent WRITE/FTRUNCATE and ordinary O_CREAT/O_TRUNC are rejected with EROFS. Block/library writes are not a usable persistent VFS write path. |
| `libs/storage-core/src/lib.rs`, storaged/block adapters | Block read/write/flush and 64-bit block addressing already exist. Audit flush guarantees: the trait currently permits a default successful no-op. |
| `kernel/ps/src/user/memfd.rs` | memfd already uses page frames and has seal/mapping lifetime machinery; do not invent another byte-Vec kernel file implementation. |
| `services/pagerd/src/page_cache.rs` | Existing generational cache types are reusable foundations, not proof of a production file-backed paging/writeback path. |

Preserve existing generation validation, bounded transport slots, early-system
bootstrap, cursor prepare/copy/settle transactions and the pager ownership
protocol. Implement missing integration rather than replacing these with a
parallel incompatible stack.

### FAT backend reuse review

The existing FAT engine is already open source: `libs/storage-fat/Cargo.toml`
uses `rafalh/rust-fatfs` with `no_std`-compatible alloc/LFN features. Cargo.lock
currently resolves fatfs 0.4.0 at `c4b88477b22ca7e5131fbd8891f62a5deaa88e6e`.
It provides FAT12/16/32, file/directory mutation and seekable open files under
MIT. B/C are adapter and OS-integration work, not a new FAT implementation.

| Candidate | Fit and limits | Decision |
| --- | --- | --- |
| [rust-fatfs](https://github.com/rafalh/rust-fatfs) | Already integrated, MIT, Rust, Read/Write/Seek and directory operations. Current path helper discards open-file cursor state between ranges. It does not supply RustOS's snapshots, IPC, coherent mmap/writeback or persistence policy. | Preferred baseline: retain/extend the adapter and use a narrowly scoped upstream fix/fork only when needed. |
| [ChaN FatFs R0.16](https://elm-chan.org/fsw/ff/) | Portable C, author-described 1-clause BSD-style license; documented disk callbacks, flush, re-entrancy options and CLMT fast seek; optional exFAT. Requires Rust/C ABI and callback/lifetime/error integration. | Credible alternate backend, not a drop-in solution for POSIX open-file or paging semantics. Prototype only against a demonstrated requirement the current backend cannot meet reasonably. |
| [embedded-sdmmc](https://github.com/rust-embedded-community/embedded-sdmmc-rs) | MIT OR Apache-2.0, pure Rust/no_std/no_alloc, configurable bounded open-object tables; explicitly favors simplicity over performance. Its README still lists path-string handling and empty-directory deletion as TODOs. | Useful embedded design reference, not an obvious upgrade for this general-purpose OS. |

ChaN's [application note](https://elm-chan.org/fsw/ff/doc/appnote.html)
explicitly prohibits duplicated opens with a writer and rename/delete of an
open file under its sharing rules; re-entrancy serializes same-volume functions
rather than supplying OS concurrency semantics. Its
[fast seek](https://elm-chan.org/fsw/ff/doc/lseek.html) consumes a fragment map
and cannot expand a file while that mode is enabled. A port must handle these
constraints, not hide them behind successful VFS operations. Preserve source
license notices and review exact upstream revision/configuration before import.

All FAT32 implementations retain the format's 4-GiB-minus-one file-size ceiling.
Chrome's current 128-MiB failure is a RustOS snapshot policy, not that format
limit. Replacing the FAT engine cannot remove it or implement cache/mmap/flush
semantics above the engine. Optional exFAT is neither ext4 nor NTFS support.

## 3. Common contracts to settle before implementation

### File identity and backend boundary

- `FileId`: mount/provider epoch + opaque backend object identity + reuse
  generation. An open file is not a pathname; rename/unlink cannot retarget it.
  FAT needs an owned synthetic identity, not merely its mutable first cluster.
- `OpenDescriptionId`: shared cursor/flags/replay identity, distinct from the
  file object; dup/fork share the appropriate description, pread/pwrite do not
  advance it. Retain the existing settlement protocol across user-copy faults.
  Version open-description checkpoints to restore by file identity/version;
  reopening a mutable pathname after restart is not restoration of the old fd.
- Filesystem capabilities: maximum file size, block/alignment rules, read-only
  state, sparse/unwritten/inline/compressed data, snapshot support and actual
  durability guarantees. Unsupported operations fail explicitly.
- File size/offset/end arithmetic uses checked 64-bit values; only an admitted
  bounded transfer length becomes usize. Respect Linux signed off_t/ssize_t
  limits. FAT's 4-GiB-minus-one file limit remains a backend constraint, not a
  common VFS limit.
- Backend operations cover lookup/open/stat, positional reads/writes, owned
  sequential cursors or range plans, resize, allocation/metadata completion,
  flush and snapshot/version leases. Keep block extent details internal to
  filesystem-data workers; clients never receive raw device authority.
- Range plans distinguish mapped, hole, unwritten and inline data and carry a
  mapping version. NTFS also needs stream identity, allocation size, logical
  size and valid-data-length handling. Compression/encryption/reparse features
  must not be mistaken for ordinary extents or zero-filled holes.
- Do not store self-referential borrowed `FatFile` values, add unsafe lifetime
  escapes, or create a second writable FatVolume to avoid ownership. Design
  owned cursor/mapping tokens and one explicit mount-state owner.

### Bounded transfer and completion

- Separate logical operation length from transfer-window size. Split requests
  at admitted buffer, device, filesystem and transport boundaries; do not enlarge
  the DVM ring just because a file is large. Window sizes are negotiated/aligned
  within current bounded transport capacity, then measured.
- Each request owns caller identity, file/version lease, buffer grant, absolute
  deadline, sequence and confirmed-byte progress. Bound queued/in-flight bytes
  and concurrency globally, per mount and per funding Job. Reserve before work.
- Preserve EOF, short I/O, partial progress, EINTR/EFAULT, ENOSPC, EIO and
  cancellation semantics. A nonempty request cannot spin forever on zero progress.
- A timed-out write can have an unknown provider outcome. Reconcile that exact
  operation identity; never blindly replay append/allocation or report a failed
  chunk as definitely unwritten. Release buffers only after completion/revoke
  fencing makes reuse safe.
- Long operations use explicit asynchronous job/progress/cancel ownership with
  bounded leases; do not increase every IPC timeout or restart a fresh full
  deadline per chunk. Small operations may retain their synchronous fast path.
- Namespace locks cannot cover whole-file data I/O or user faults. Serialize
  metadata/reservations briefly, run admitted I/O outside that ownership, then
  revalidate epoch/version before publication. Use bounded scheduling turns so
  a Chrome read cannot starve UI/runtime metadata requests.

### Mutation, caching and durability

- Define per-open cursor serialization, overlapping-range ordering, append
  reservation/publication, truncate vs in-flight I/O, rename/unlink with open
  handles, and descriptor/provider restart before enabling writes.
- Separate mount/provider epochs, inode reuse, mapping versions and snapshot
  content versions. Existing mount-generation-only read-only caches are not
  sufficient once the same mounted file can change.
- Use one coherent file-data cache for buffered I/O and mapped file data,
  eventually owned by pagerd under its current contract. Backend metadata/block
  caches remain distinct and bounded. Dirty data cannot be discarded merely
  because a generation changed or an entry was evicted.
- Avoid cache RPC cycles: a vfsd buffered read that waits for pagerd must not
  occupy the filesystem-data worker that pagerd needs to fill a page. Separate
  queues/endpoints and asynchronous ownership; no all-workers-waiting-on-self
  topology or lock held over a cache-fill callback. Update rootd's service graph
  and endpoint/buffer-grant rights with these new paths, not just their queues.
- Distinguish accepted-to-cache, written-to-device and durable data/metadata.
  `fsync`, `fdatasync`, directory sync, journal ordering, provider flush and
  deferred errors need explicit contracts. A default no-op flush must not stand
  in for a persistent device guarantee; write-through/volatile memory are
  explicit cases. Audit the complete storaged -> DVM block -> QEMU/backing-file
  flush chain. File readback in the same boot is not durability evidence.
- Rootd owns funding/quotas; existing kernel mechanisms enforce admitted rights
  and frame lifetime. Account caches, dirty pages, staging snapshots and shared
  mappings without losing or inventing charges across close/crash/restart.

## 4. Delivery phases and exit criteria

| Phase | Implementation | Required exit evidence |
| --- | --- | --- |
| A — common model and witnesses | Pin the contracts above; introduce owned file/cursor/range identities, capability/error types and counting/faulting backends. Audit all executable admission caps and frame/mapping accounting. | u64 offset and boundary tests; stale/reused handle rejection; explicit memory/durability budgets and IPC dependency graph. |
| B — FAT large reads and executable admission | Replace path-reopen-per-window with an owned sequential cursor/range map. Stream bounded windows into the existing memfd, hash incrementally, seal once, then publish the exact owned fd. Remove the whole-file staging Vec and long global lock hold. | Chrome's exact 306,789,704-byte snapshot opens and reaches ELF admission; content hash/seals match; chain/I/O counters grow linearly rather than per-window rescans; concurrent small VFS work progresses. |
| C — persistent VFS writes | Connect bounded positional/stream writes, seek/append/truncate and filesystem mutation completion to a disposable writable FAT data volume. Add coherent invalidation, replay/cancel handling and real flush/error reporting. | Large sequential/random writes survive close/reopen and flush/reboot under the advertised FAT guarantees; partial failure/ENOSPC/restart cases pass. Existing signed boot/root volume remains read-only. |
| D — shared file cache and mmap | Wire the existing pager foundations to filesystem-data requests; unify buffered/mapped visibility, MAP_PRIVATE COW, MAP_SHARED dirtying/writeback, truncate invalidation, eviction and revoke. | Fault/cancel/unmap/close races, EOF/truncate SIGBUS behavior, bounded resident/dirty memory and persistence tests pass; pager/frame/TLB lifetime models match implementation. |
| E — new read-only backends | Port FAT to the stable interface completely; add ext4 RO and NTFS RO feature subsets without changing callers. Validate clean-volume admission and fail unsupported/corrupt states closed. | Cross-backend file-I/O tests plus independent Linux/Windows-generated image comparisons, malformed-image fuzzing and large/sparse file witnesses. |
| F — filesystem-specific writes | ext4 RW after recovery/journal/ordering validation; NTFS RW only after its distinct metadata/log/recovery and Windows interoperability gates. | Crash/replay, corruption refusal and cross-OS fsck/chkdsk evidence on disposable images; truthful documented feature matrices. |

B does not wait for ext4/NTFS. E's two RO adapters can proceed independently
once the common contract is stable; NTFS RW is not bundled into “NTFS support”.
These are dependency/acceptance phases, not calendar estimates or arbitrary
file-count milestones.

### Implementation status — 2026-11-05 (not an exit-evidence claim)

The first A/B implementation now connects common ReadAt/WriteAt and owned maps,
FAT extent planning and detached version-bound reads, ordinary read-map caching,
and bounded incremental memfd collection/hash/seal. The pinned FAT source is
locally managed with corruption-boundary checks and a directory-slot identity
accessor, not a replacement filesystem. Final-frame/scratch staging reservations
are Ring3 policy; fallible incremental frame-vector growth is the only memfd
mechanism change in Ring0. These reservations do not pre-fund rootd Job pages.

The initial actual Chrome probe passed the 128-MiB rejection and entered a
306,789,704-byte read, then hit the 4-second lease (errno 110). That failed boot
is retained in `build/large-io-validation/kvm-probe-timeout-debugcon.log`; GUI
readiness alone is not acceptance of the image. Snapshot ABI v3 now has bounded
OPEN/POLL/CANCEL sessions, exact owner/lease checks, confirmed progress and
orphan cleanup instead of holding a reply capability through bulk I/O. Control
waits and global IPC limits are not widened. The outer launch path now uses loader ABI v4 owner/deadline-bound work IDs;
rootd/initd/procd/runtimed share a short-RPC client. Primary collection is
asynchronous, then the existing serial loader consumes the same sealed image.
No image parser or filesystem policy moved into Ring0. Bootstrap/exec retain
5-second work budgets; catalog apps have an explicit 180-second maximum work
lease, primary snapshots at most 120 seconds, with 4-second control RPCs and
parent-clipped nested reads. An incoming attenuated Job loan is MOVED into
staging, not duplicated through an unsupported/ungranted fcntl path.

A 1-vCPU production-protocol VALIDATE_IMAGE probe passed with live GUI/input/
WayClick: 306,789,704 bytes, 50.206942 seconds of snapshot preparation, GUI trace
terminal 3652 ms with no advisory overshoot. The VFS stamped input digest and an
independent loaderd full sealed-FD readback both equal host SHA-256
`fa67e06a5bcd0525739996fb8e3ecff449039e6165c15ea7968d3935e59e829e`.
Evidence: `build/large-io-validation/kvm-staged-custody-probe-debugcon.log`.
VALIDATE_IMAGE performs no process construction or activation: browser execution,
dynamic-library/syscall/sandbox compatibility and a browser window remain
unverified. Source conformance passed 1471 checks / 155 model projections;
full PR/nightly formal and large-image SMP/fault campaigns were not run.
The probe-free signed image was restored and GUI smoke passed (4240 ms), with
one display-provider advisory overshoot (2837 >2500 ms). Chrome remains visible
manual-only, startup=none/autostart_enabled=0, with no boot execution.
Persistent writes, stable mutable/checkpoint object identities and paging are
still separate phases. Complete hostile-directory parsing and writable-volume
recovery validation are not implied by malformed file-chain tests.

## 5. Important limit of the first Chrome fix

Streaming removes the extra file-sized vfsd heap buffer; it does **not** make a
fully materialized sealed memfd consume constant total memory. Audit/admit the
final snapshot frames, frame metadata, sharing/mapping charges and worker
scratch together before accepting Chrome. Advance the current 128-MiB limits
consistently only after that admission works; size policy and cache budgets are
separate, not two independently edited magic numbers.

Keep all four terminal seals and the path/length/epoch/owned-fd binding. A
partial/cancelled/expired snapshot must never be returned as executable.
Incremental hashing and completion evidence bind the same bytes that are sealed.
A mount/version change during collection aborts publication, not silently
reopens the new pathname.

Larger-than-resident executable support belongs to D's immutable, pager-backed
snapshot work (or a separately budgeted stable backing object), not an unbounded
RAM promise. The process broker currently rejects live VFS/nonterminally sealed
objects: do not weaken that check to launch Chrome. Any new snapshot capability
must enforce immutable content identity, pin/COW or verified backing, page and
provider generations, and teardown/revoke. A read-only mount flag by itself is
not proof of immutable content against independent writers/provider changes.

Success for B is storage/snapshot admission, not a browser window. Preserve and
report the next actual loader/runtime error instead of suppressing it or adding
`--no-sandbox`, fake syscall success or automatic Chrome startup.

## 6. Filesystem-specific guardrails

### FAT

- Keep its actual maximum file size and lack of journaling visible. Do not
  promise crash-atomic multi-metadata transactions merely because flush exists.
- Sequential cursor/cluster-run reuse must handle fragmentation and malformed
  cyclic/out-of-range chains with bounded work. Random range access uses a
  bounded index, not permanent full-file data caching.
- Use a separate writable test/data volume; do not mutate the signed boot image
  or make the executable-cache assumptions false by remounting it writable.

### ext4

- RO minimum: checked superblock/group/inode/directory/extent traversal,
  supported 64-bit geometry, extent holes/unwritten ranges and applicable
  checksums. Declare support for legacy block maps, inline data and other
  optional features explicitly; reject unsupported incompatibility flags.
- An image requiring recovery is not safely mounted by merely skipping journal
  replay. Implement the supported recovery mode or reject it.
- RW requires allocation/freeing, concurrent metadata lifetime, orphan handling,
  ordered data/metadata persistence and validated JBD2 transactions/replay.
  Fast commits, bigalloc, encryption, casefold and similar features are separate
  capability gates, not silently ignored bits.

### NTFS

- RO minimum: validated MFT records/fixups, resident/nonresident attributes,
  run lists/attribute lists, directory indexes, stream identity, sparse ranges
  and initialized-data boundaries. File identity includes reuse protection;
  path spelling/case rules are not borrowed from FAT.
- Do not expose uninitialized allocation past valid data length. Named streams,
  reparse points, compression/EFS/WOF and security descriptors need explicit
  semantics/capabilities; unsupported data is never fabricated as zeros.
- Preserve Windows sharing/delete semantics through typed policy, not a false
  POSIX equivalence. Refuse unsafe writable admission of dirty/hibernated or
  independently mounted volumes.
- Metadata transaction/log/recovery support must be demonstrated separately.
  Journal replay is not the same as implementing safe journaled writes; the
  upstream NTFS3 documentation itself distinguishes these. USN change records
  are not a replacement for transactional recovery. NTFS RW is a later gate.

## 7. Validation plan

- Native: 0/short/unaligned ranges; 128-MiB boundary; exact Chrome length;
  1-GiB streaming checks; 4-GiB boundary and >4-GiB sparse offsets on a capable
  synthetic backend (not FAT); signed offset/end overflow; fragmented/cyclic
  FAT; object reuse, cursor sharing and partial progress. Count allocations,
  peak scratch, I/O requests and chain walks; do not use timing alone to claim
  bounded memory or linear traversal.
- Concurrency/fault injection: mixed small/large operations; same-file writers,
  append/truncate/read/mmap races; service death and reply loss at every stage;
  cancellation/timeout after possible write; remount/provider revoke; full
  queues/budgets; media errors, ENOSPC, checksum failure and failed flush.
- Recovery: deterministic power loss around data, allocation, metadata, journal
  commit, flush and publication; discard volatile state before verifying bytes
  and namespace. Use fresh service/device epochs, not a warm cache readback.
- Integration: signed RustOS build, explicit on-demand Chrome snapshot/ELF
  probe (never permanent autostart), 1-vCPU GUI responsiveness while bulk I/O
  runs, then sealed multi-vCPU stress/recovery. Record next Chrome blocker.
- Filesystem corpus: host tools create/reference disposable FAT/ext4/NTFS images;
  RustOS hashes/metadata are compared with independent readers, then filesystem
  checkers/recovery validate advertised RW behavior. Real user disks and force
  mounting dirty Windows volumes are excluded.
- Formal: update owner contracts, source anchors, ABI producers/consumers and
  witnesses together. Model request/grant lifetime, versioned snapshot publish,
  cursor/mutation settlement, writeback vs truncate and flush/crash ordering.
  Native passes or a registry check do not replace formal/runtime evidence.

## 8. Recommended first implementation batch

Implement A plus B end to end, not the ext4/NTFS parsers yet:

1. Owned FAT sequential/range cursor using the existing rust-fatfs engine,
   with checked 64-bit interface, bounded work and counting/fragmentation/error
   fixtures; do not start by rewriting FAT or replacing it with C.
2. Snapshot job admission, bounded streaming, incremental hash, terminal seal,
   exact-generation publication and cancellation cleanup using current memfd.
3. Audit/fund peak final-frame usage; unify and advance the snapshot size policy
   only for admitted resources. Introduce progress/deadline protocol if measured
   transfer time cannot fit the current transaction, rather than stretching
   unrelated IPC deadlines.
4. Prove Chrome passes the current EOVERFLOW gate, record the next blocker, and
   prove ordinary UI/metadata work remains live under the same large read.

C through F then extend that contract, with writable-cache assumptions audited
before persistent mutation is enabled.

## 9. External references consulted

These are filesystem/algorithm references, not authority to import Linux's
privilege model or claim feature parity:

- [Linux iomap design](https://docs.kernel.org/filesystems/iomap/design.html):
  range mappings, holes/unwritten data, mapping revalidation and completion.
- [ext4 on-disk documentation](https://docs.kernel.org/filesystems/ext4/index.html),
  [extent/inode mapping](https://docs.kernel.org/filesystems/ext4/ifork.html),
  [JBD2 journal](https://docs.kernel.org/filesystems/ext4/journal.html).
- [Linux NTFS3](https://docs.kernel.org/filesystems/ntfs3.html): current features,
  dirty-volume cautions, and journal-replay/full-journaling distinction.
- [Microsoft MFT overview](https://learn.microsoft.com/en-us/windows/win32/fileio/master-file-table),
  [file streams](https://learn.microsoft.com/en-us/windows/win32/fileio/file-streams):
  per-stream allocation, logical length, initialized length and sharing state.

Before selecting an external parser/engine, review its exact feature matrix,
license, hostile-image behavior, ownership model and fit with this backend API.
Do not assume that a crate name or an upstream “RW” label satisfies these gates.
