# VFS and storage contracts

[Contract index](../contracts-abi.md).

## VFS Surface (`vfsd`)

- Protocol: `VfsIpcRequest`/`VfsIpcResponse` (separate from `LinuxSyscallOffloadRequest`) for service-owned handles + chunked I/O.
- Vfsd remote and epoll object IDs are kernel-minted, boot-entropy-backed
  capabilities, not counters or caller-selected fd numbers. The exact token is
  preserved by dup/fork/IPC transfer, while rootd admits service lookup only on
  the declared caller-to-provider dependency edge. Vfsd receives the
  kernel-stamped sender identity and rejects a conflicting payload PID/TID.
  These checks are cumulative: sender identity never substitutes for object
  capability possession, and token obscurity never substitutes for the rootd
  dependency graph.
- Kernel fd tables mirror service-owned objects as `KernelHandle::RemoteVfs`.
- VFS device open responses use `VfsIpcResponse.aux` for service-owned route
  metadata such as `INPUTD_ACCESS_*` and `VFS_DEVICE_ACCESS_DRM_COMPAT`.
  Compat never classifies `/dev/*` path strings: vfsd resolves the namespace,
  then compat either transfers the devmgrd handle or retains the explicitly
  selected DRM compatibility description.
- Linux `openat` installs `KernelHandle::RemoteVfs` for regular files + directories after vfsd registration.
- Linux `dup`/`dup2`/`dup3` acquire the exact open-description reference before
  publishing the local duplicate. Socket and input descriptions cross their
  service boundary; remote VFS and epoll descriptor references are counted
  locally by the kernel fd substrate. Epoll sends only create and final-retire
  mutations to vfsd, so a routine `fcntl(F_DUPFD*)` cannot inherit vfsd
  scheduling latency. `close` removes the local descriptor first, then performs
  any token-addressed final provider release; a wedged vfsd/netd cannot retain
  a reusable numeric fd or block process retirement. Netd socket DUP, final
  CLOSE, and their exact replay ACK mutate the provider's open-description
  reference and therefore own one complete bounded 100 ms interactive-control
  attempt. They do not split the deadline or retry a real timeout; exact
  mutation identity transfers to housekeeping. Checkpointed epoll
  create/ADD/MOD/DEL/retire and matching purges follow the same control rail.
  Only non-consuming provider readiness queries use the 16 ms deadline. vfsd is
  the only intended caller of `SYS_RUSTOS_FD_*_BROKER`; gated by `VFS_POLICY`.
  Generic apps must not call those brokers directly.
  Fork clones the address space and fd table in one process-state snapshot;
  provider refs are acquired from that frozen child table before publication.
  Resnapshotting the live parent afterward is forbidden because a sibling
  close/reuse could bind the child fd to one object and the provider ref to
  another.
  Exec cleanup is likewise derived only from the exact `KernelHandle` values
  removed by the atomic process-table CLOEXEC commit; a pre-commit CLOEXEC
  snapshot is not cleanup authority.
- A devmgrd ioctl route is policy advice, not fd identity. Compat must resolve
  the live fd-table entry again before dispatch: sessiond TTY operations require
  an actual `KernelHandle::Console`, so a closed/reused ordinary file fd
  receives `ENOTTY` and cannot read or mutate the caller's console policy.
- `LinuxSyscallOffloadRequest.arg0..arg3` carry 64-bit fd-control args (target fd, cmd, arg, flags). **Do not pack pointer/flag values into the 32-bit `mask` field.**
- `mount`/`umount2` route to vfsd → gated `SYS_RUSTOS_VFS_*_BROKER` for kernel mount-table mutation. Do not reintroduce direct generic-app `linux_ops::mount`/`umount2` paths.
- `poll`/`ppoll`/`epoll_*` route generic fd readiness policy and epoll
  interest state through `VFS_IPC_OP_POLL_QUERY`; socket readiness is `netd`
  policy, input readiness is `inputd` policy plus the narrow native-input
  wait/completion substrate, and non-system console-input readiness is
  `sessiond` policy. sessiond advances its generation only on an empty-to-ready
  line-discipline transition; the readiness query is non-consuming and bounded
  by the application's remaining deadline. Live console output/error are
  immediately writable, but they observe the same session generation so close
  becomes `POLLHUP` even when the caller requested only hangup events.
  Closing a session advances that generation and a subsequent query returns non-live, which compat exposes as `POLLHUP` without
  recreating the removed session. Stdin/stdout/stderr are real fd-table entries,
  not numeric exceptions. Each console open description carries an unforgeable
  monotonic token shared by dup/fork and retired only after final close,
  CLOEXEC, or process exit. Persistent epoll binds every non-system console
  stream to that token plus the
  exact sessiond endpoint epoch; final retirement purges the matching vfsd
  interest, and a stale token fails closed even if purge delivery failed.
  The shared `WaitSetInterestWire` admission function is the single envelope
  validator on both sides of the boundary: ABI v1, a known provider, zero
  flags/reserved word, a target fd representable by the bounded fd table, and
  nonzero object/endpoint-epoch authority are mandatory before either ring0 or
  vfsd resolves an interest. Proof-indexed Kani harnesses exhaust the symbolic
  envelope partition and require both accepted and rejected witnesses; they do
  not prove provider lookup, fd lifetime, or concurrent wake behavior.
  Vfsd admits at most 256 live epoll objects and 512 interests per object, for
  a machine-readable global ceiling of 131,072 persistent interests. Live
  create and checkpoint restore fail closed at the object boundary; batch
  restore is built in a candidate registry and becomes visible only after the
  complete batch validates, so a duplicate or cap overflow preserves the exact
  prior registry. A bounded reverse index maps each provider-object identity to
  its exact `(epoll, interest-key)` memberships. Final-object purge removes
  only that set and is proportional to affected registrations rather than a
  clone or scan of every live epoll object.
  EPOLL_CTL_ADD/MOD pins the exact provider or console open description across
  the vfsd mutation. Releasing that transaction guard performs normal
  last-close purge, so a concurrent final close cannot purge first and then
  leave a newly inserted interest for a retired object.
  Linux defines `epoll_ctl` as the synchronous interest-list control interface,
  distinct from the timeout-bearing `epoll_wait`; the compatibility boundary
  therefore keeps mutation completion separate from the 16 ms provider-query
  cap (<https://man7.org/linux/man-pages/man2/epoll_ctl.2.html>). Its ADD record
  pins the target open file description, matching Linux's documented reference
  semantics; acquiring that netd reference is part of the same interactive
  mutation, not a readiness probe (<https://docs.kernel.org/filesystems/files.html>).
  `O_NONBLOCK` is read from the same fd-table snapshot as the console
  handle; an empty sessiond read returns `EAGAIN` instead of re-entering
  the blocking retry loop after a readiness race.
  System-console bootstrap input remains a deliberate non-persistent exception
  because it has no sessiond-owned readiness generation. Ring0 keeps fd-table validation, epoll token
  handles, user-copy, a bounded provider-wait registry, and deadline wakeup.
  Both persistent epoll sets and syscall-scoped multi-fd poll sets use the same
  generation-based path: check, register the exact observed generations,
  recheck through ordinary service IPC while the task remains runnable, arm
  the scheduler, then verify that every registered waiter still exists before
  commit. A provider signal removes its waiter before wake, so an event in the
  recheck-to-arm window is detected without nesting an IPC block inside an
  already armed scheduler block. The bounded waiter registry derives its
  capacity from the scheduler task ceiling times the provider ceiling; every
  schedulable task can therefore arm one observation for every provider
  without an artificial mid-capacity `ENOSPC` failure. Every readiness IPC is
  capped at the shorter
  of the remaining syscall deadline and 16 ms; an infinite application wait
  therefore cannot turn a wedged provider into an unbounded kernel-service
  call. `ppoll`/`epoll_pwait` atomically
  apply their temporary signal mask, reject a non-native sigset size, and
  return `EINTR` for a pending unmasked signal; SIGKILL/SIGSTOP can never be
  added to the temporary mask.
- User-task retirement is a two-phase runtime boundary. The scheduler first
  revokes runnable, deadline, and IPC authority, stamps the exact task/process
  identity plus whether this was the process's last live thread, and keeps the
  slot unreapable. Executive housekeeping then removes every task-scoped
  futex, wait-set, service-discovery, input, block, bootstrap-TTY, and proc
  broker record; terminal-process cleanup also closes provider references and
  revokes service publications. The directory publishes the immutable cleanup
  token only after all terminal metadata is complete. An independently locked
  Identity/lifecycle publication has an independent per-slot class-51 writer
  (IRQ-excluded leaf); metadata writers cannot supply lifecycle flags. Pending
  suspend/stop intent survives missing partial-initialization metadata; exact
  live commands reject a reused task ID. Four Scheduler gate arrays are
  test-only, not production authority. Cross-owner admission/exec/frame lifetime
  still belongs to the remaining directory/resource boundary. The
  per-slot `SchedulerRetirementCleanup` owner holds deferred producer custody
  and both cleanup/kernel Pending→Claimed→Complete transactions. Exec freezes
  process-detach metadata before either token becomes visible. Kernel claims
  are private, non-Copy and generation-bound; their final ACK follows all
  revocation/wake/detach work. Claim/ACK APIs do not acquire Scheduler and
  release the cell before external
  cleanup, user copy/faults or other subsystem locks. Only an exact claimed
  task and full-payload cleanup ACK plus kernel ACK permits reset/reuse;
  Pending/Claimed and deferred/in-progress producers remain unreapable. Reset
  precedes runtime/slot identity teardown; reaper notification is atomic
  consume-before-scan, not a lost-wakeup final false store. Producer and
  execution/process/TLB/stack quiescence still require their existing owners.
  A wait-lock unlock skips retired queue heads until it wakes one
  live task, so a stale identity cannot consume the only wakeup.
  A hardware-IRQ context validator may only quarantine the fixed task slot and
  record one allocation-free deferred reason. It must not scan the live
  current kernel frame as if it were a published runnable frame, nor enter
  IPC/process teardown from the IRQ. Housekeeping finalizes that quarantine
  through the same retirement boundary before reaping.
  Service-publication wait admission is task-scoped and its capacity derives
  from the scheduler task ceiling, not the smaller process-table ceiling. Its
  registry is a fixed allocation-free table: one task rearm replaces its exact
  slot, matching task IDs are extracted under the lock, and wake/hint calls run
  only after unlock. The native input wait substrate uses the same ceiling; the separate inputd
  ingestion waiter never competes with application slots.
- A vfsd epoll membership key is `target_fd + provider + open-description
  object_id`. The provider endpoint epoch is record state, never part of that
  identity: after restart, duplicate `ADD` remains `EEXIST`, `MOD`
  replaces the stale epoch, and `DEL` removes the exact registration.
  A stale epoch therefore fails closed but cannot create an undeletable or
  duplicate interest.
- Legacy `SYS_RUSTOS_{STATX,STAT,READLINK,ACCESS,GETCWD,CHDIR}_METADATA`: no generic-app VFS policy in ring0 after vfsd registers. Pre-vfsd bootstrap + registered policy-service callers retain direct kernel metadata access.
- `SYS_RUSTOS_BLOCK_BROKER` ABI v3 is DVM-only and gated by
  `STORAGE_POLICY`. `BOOT_INFO`, `BOOT_READ`, physical descriptors, and the
  `VFS_POLICY` lane do not exist. Operation-incompatible fields, the explicit
  timeout, and reserved words must be zero. Tickets bind generation, request
  ID, and transfer slot; a failed ticket copyout cancels the submitted request
  rather than publishing ownerless work.
- `SYS_RUSTOS_EARLY_SYSTEM_BROKER` ABI v1 is a separate vfsd-only immutable
  bootstrap-file lane. It accepts one inline exact path from the signed
  early-system table and provides only file length or at most 4 KiB of
  read-only bytes. It has no enumeration, directory, physical LBA, controller,
  mutation, or fallback operation. Vfsd owns path normalization, open
  descriptions, cursors, checkpointing, and the overlay decision; digest and
  exact-entry admission remain in the bounded ring0 reader.

## Storage Surface (`storaged`)

- Kernel storage inventory, `SYS_RUSTOS_STORAGE_LIST_BROKER`, boot extent
  leases, raw disk offsets, and AHCI/NVMe policy replies are retired. Storaged
  derives one DVM descriptor only from authenticated live DVM geometry.
- Boot protocol version 18 retains the former physical extent pointer/length
  only as two mandatory-zero reserved words. Multiboot accepts exactly one
  `rustos-early-system` module and has no `rustos-root-extents` selector.
- GRUB loads one signed, immutable, uncompressed early-system module containing
  the exact bounded bootstrap allowlist needed to start rootd, the core policy
  services and storaged. Hostd and the storage-DVM kernel/rootfs/control
  artifacts are separately signed host inputs. The early-system fixed table uses checked
  offsets, lengths, and per-file SHA-256 digests; pages stay reserved until no
  bootstrap executable or registry references them. Ring0 does not gain an
  archive decompressor, filesystem namespace, general path traversal, or
  mutable package policy.
- The exact allowlist includes the minimal dynamic ELF runtime closure needed
  before storage-DVM readiness (`/etc/ld.so.cache`,
  `/lib64/ld-linux-x86-64.so.2`, `libc.so.6`, and `libgcc_s.so.1`) in addition
  to bootstrap services and registries. The loader cache is part of this
  closure so glibc's cache lookup cannot turn the first UI process into a
  DVM-volume probe. Missing runtime members fail image staging; boot never
  falls through to a physical controller or an unverified host filesystem.
- The early-system module is a bootstrap capability, not a native-storage
  fallback. Once its manifest is admitted, rootd and loaderd may open only its
  exact entries until storaged publishes the DVM-backed root volume. Missing,
  duplicate, overlapping, out-of-range, digest-mismatched, or undeclared
  entries stop bootstrap. They must never trigger NVMe/AHCI probing.
- Vfsd resolves exact early-system files through the dedicated immutable
  broker before consulting the DVM FAT volume. This lets loaderd start initd
  and the fixed bootstrap closure without racing storage-DVM readiness; all
  non-entry paths continue to the service-owned DVM volume and an invalid
  early-system image fails closed instead of becoming a false `ENOENT`.
- On the read-only DVM root volume, vfsd may materialize a complete bounded
  file once per mount generation and serve later `pread`/mapping windows from
  that service-owned snapshot. Per-entry and aggregate byte ceilings are
  mandatory; oversized files fall back to bounded range reads, remount clears
  every cached byte, and enabling mutation requires explicit path/generation
  invalidation before this optimization remains legal.
- Per-file digest admission is bound to the immutable module identity, exact
  payload range, length, and expected digest. A successful proof may be reused
  for later bounded reads and process mappings during that boot; the cache is
  cleared if boot identity is initialized again and is strictly bounded.
  Reuse must never skip table/range/path validation, admit mutable DVM bytes,
  or turn a failed digest into a cached result.
- Early-system ownership is resolved before applying the broker's 4-KiB
  transfer bound. A non-owned path returns to the DVM volume even when the
  caller supplied a larger VFS buffer; an owned immutable entry is read in
  bounded 4-KiB chunks. An entry that disappears between INFO and READ is
  corruption and fails closed rather than falling through to mutable storage.
- The boot disk is a storage-DVM backing artifact, never a ring0 mount. Xtask
  reopens every staged FAT file after image construction and compares its
  exact bytes; early-system creation separately requires every allowlisted
  file, rejects duplicates/empty payloads, and signs the completed image.
- RustOS contains no AHCI/NVMe probe, queue, DMA allocator, partition registry,
  FAT opener, or physical-block fallback. Missing or malformed early-system
  state stops bootstrap; it cannot reactivate physical-storage code.
- Physical NVMe/AHCI ownership transfers exactly once on the host:
  `exclusive whole-device open -> reject holders/mounts/swaps -> bounded
  fsync+BLKFLSBUF -> validate exact controller and IOMMU group -> unbind the
  signed original driver -> reset -> bind vfio-pci -> launch one storage DVM
  with a newer generation -> authenticated check-arm-recheck readiness`.
  Host-native and DVM authority must never overlap. Failure after detach
  revokes the aperture before controller restoration; failed revocation
  quarantines the controller and retains recovery records.
- An active storage lease cannot use the direct `release --activate` path.
  `recover` must load the durable runtime record, bind the exact PID plus
  process start time, request bounded QMP/ACPI shutdown, observe that process
  exit, and only then revoke the generation-bound aperture. Controller reset
  and original-driver restoration follow aperture revocation; any failed step
  retains the lease and quarantine evidence instead of fabricating release.
- `driver-domain-protocol` owns the versioned, address-free DVM block wire
  contract. Version 2 fixes one 8-MiB power-of-two PCI BAR, two 64-entry rings,
  and 64 host-owned 64-KiB transfer slots. The unused tail after the fixed slot
  array is reserved and grants no addressable request/data authority. Requests
  name only a slot, launch generation, request ID, monotonic
  mutation operation ID, sector range, and Virtio-compatible
  READ/WRITE/FLUSH/DISCARD/WRITE_ZEROES operation. Unknown flags, reserved
  bytes, stale generations, unaligned/overflowing ranges, read-only mutations,
  and unsupported features fail closed. Its immutable geometry and generation
  carry an Ed25519 L0 signature. The verifying key is embedded in the
  GRUB-authenticated early-system header; the caller-owned signing key remains
  on L0 and is never mapped into the storage DVM. Signed schema-4 policy binds
  the SHA-256 of that verifying key, so hostd rejects an operator-supplied
  signer for a different RustOS image before freezing the whole device.
- Every shared block-header scalar is naturally aligned. The four 64-bit ring
  cursors are single-copy atomic little-endian values: each ring has exactly
  one producer and one consumer, the producer publishes its record/data before
  a Release cursor store, and the consumer performs an Acquire cursor load
  before reading that record/data. Header identity and geometry are immutable
  for one generation; only readiness and ring cursors may change. RustOS
  validates immutable fields separately from live cursors so normal peer
  progress cannot be mistaken for corruption, while any identity, geometry,
  feature, read-only-mode, or generation mutation revokes the aperture.
- L0 initializes signed immutable geometry with both readiness bits
  clear. RustOS alone sets `RUSTOS_READY`, only after the exact aperture and
  its MSI-X receiver are installed, then rings peer 1. Publication is one
  compare-exchange from the exact verified flags snapshot; if the peer changes
  any readiness state between verification and publication, the operation
  fails without setting `RUSTOS_READY` and the epoch remains revoked. The
  storage DVM may
  bind the initial aperture but must not publish `DVM_READY` until it observes
  that RustOS bit, validates the Linux block geometry, and owns the physical
  controller. Pre-setting either peer's bit, retaining readiness across an
  epoch, or publishing DVM readiness first fails admission.
- Storage-DVM relay admission reports the exact failing stage (`lock`,
  `transport`, `header`, `block-device`, `rustos-ready`, `publish-ready`, or
  `publish-evidence`) with the preserved errno. A generic readiness timeout is
  not acceptable evidence for this boundary. Authenticated block evidence must
  match the live signed aperture's capacity, logical/physical block sizes,
  features, and read-only mode exactly. Read-only is a supported least-
  authority transport mode, not an error or permission to report writable
  media.
- Both directions use ivshmem MSI-X vector 0 only after the corresponding
  Release cursor publication. RustOS writes BAR0 doorbell peer 1 for requests;
  the storage DVM writes peer 0 for completions and readiness withdrawal.
  Doorbells are coalescible wake hints, never queue state: each consumer checks
  its cursor before arming, arms the one event source, rechecks, and drains
  until producer equals consumer. Startup also drains pre-existing work, so a
  pre-boot or coalesced edge cannot require polling.
- A physical storage domain uses signed driver-domain policy schema 4 only:
  every non-block transport is disabled, `BLOCK_TRANSPORT=block-ring-msix`,
  the driver is exactly `nvme` or `ahci`, PCI vendor/device, class/prog-if,
  original host driver, and the one-function IOMMU group match exactly, FLUSH
  is mandatory, queue depth and slot size equal the compiled ABI, and
  handoff/reset deadlines are bounded.
  Schema 2/3 cannot opt into the block transport, so a generic or display
  domain cannot acquire storage authority by changing one transport string.
- The durable VFIO lease uses schema 5 and binds the SHA-256 of the exact
  immutable epoch signing bytes plus its signature before controller
  assignment. Generation and aperture path alone are not sufficient. Hostd
  rechecks that identity immediately before DVM launch and again when
  admitting readiness, so coordinated mutation of shared geometry and DVM
  evidence cannot manufacture supervisor success. Retired lease schemas fail
  closed instead of silently weakening this binding.
- AHCI admission includes the complete minimal Linux driver closure: `ahci`
  owns the PCI controller and signed `sd_mod` must publish its one whole-disk
  block namespace before the relay starts. NVMe uses its native namespace
  driver directly. `ahci`, `libahci`, `libata`, `sd_mod`, `nvme`, and
  `nvme-core` are signed modules, never built-in controller authority; the
  `S12` storage bootstrap loads only the exact assigned PCI modalias closure.
  Missing class-driver, signature, or namespace publication fails closed.
- Completion authority binds the exact generation, request ID, operation ID,
  and slot. Successful reads report no durability. Successful FUA writes and
  FLUSH report the exact durable-through operation ID; ordinary writeback
  completions may report only an earlier accepted mutation. DVM restart clears
  both rings and readiness, revokes the old generation, and requires a new
  authenticated readiness publication before requests resume. RustOS accepts
  a successor aperture on the existing mapping only after the prior epoch has
  revoked, all four cursors are zero, the generation strictly increases, and
  the successor signature verifies against the immutable early-system key.
  A stale, unsigned, or DVM-forged header remains revoked.
- Configured `block.read`, `block.write`, and `block.flush` fault points are
  admitted only after the current signed ready generation has been rechecked.
  The exact request is then published and must receive a generation-bound DVM
  completion before RustOS substitutes `DeviceFault` for the caller-visible
  result. This exercises the real transport and models the conservative
  outcome of an operation whose device result is uncertain; it never
  fabricates success. `block.write` covers WRITE, DISCARD, and WRITE_ZEROES
  mutations; `block.flush` remains separate so durability failure can be
  exercised without claiming a successful stable-write boundary.
- Aperture revocation clears only live readiness and ring cursors. It preserves
  the signed immutable `READ_ONLY` bit, so an interrupted read-only handoff can
  be revoked repeatedly by explicit recovery without invalidating its own L0
  epoch signature. Retry observes the same generation and static geometry; it
  never mints a successor epoch or restores controller authority.
- Ring0's DVM block endpoint is transport substrate only. It validates one
  exact aperture, arms exactly one MSI-X leaf that only wakes, copies bounded
  records/data, and exposes nonblocking submit/collect/cancel plus a
  check-arm-recheck wait. The wait accepts only a 1--30,000 ms deadline and
  shares the RTC wake path; it never spins or applies storage policy.
  Cancellation does not reuse a slot until the exact late completion is
  consumed or the generation is revoked.
- Aperture installation and provider readiness are separate composition
  points. Before RustOS has ever observed `DVM_READY`, its absence is
  `EAGAIN` and the wait predicate remains sleepable; it is not a fault event.
  A newly observed ready bit is itself one wait event, closing the race where
  the DVM publishes between storaged's failed INFO check and scheduler arm.
  Storaged applies one shared four-second product-boot deadline to the INFO
  wait and first generation-bound FLUSH proof, using one-second wait slices.
  Withdrawal after a successful observation is revoke, and timeout never
  selects early-system or a physical-controller fallback.
- Vfsd treats the storaged block-info response and on-disk FAT image as
  separate untrusted admissions. Response generation and capacity must bind
  the exact envelope; logical sectors are one of 512, 1024, 2048, or 4096
  bytes; physical alignment, flags, reserved fields, and total byte-length
  multiplication are checked before allocation. `storage-fat` snapshots that
  geometry and rejects a zero, unsupported, or overflowing device before
  parsing its boot sector. A malformed image never publishes a mounted volume.
- Common PCI resource discovery disables command decoding, probes and restores
  each standard BAR dword independently, and restores the low half of a 64-bit
  BAR before touching its high partner. Resource size is the least significant
  implemented address-mask bit, not a full-width two's-complement inversion,
  so zero upper mask bits cannot turn an 8-MiB ivshmem BAR into an enormous
  resource. This follows Linux `__pci_size_stdbars`/`__pci_read_base`; QEMU's
  ivshmem contract assigns the shared-memory object to BAR2.
- `storaged` binds every I/O to the current DVM generation, owns a 15-second
  operation deadline, and cancels timed-out tickets. Control operations and
  writes retain the generic 4-KiB commercial payload. Reads additionally use
  operation 12 and one dedicated exact 64-KiB response: an 80-byte fixed
  header plus at most 65,456 payload bytes. `vfsd` chooses only a
  block-aligned prefix of that payload (60 KiB for a 4-KiB logical block) and
  validates the echoed complete request header, generation, LBA, block count,
  reserved word, and exact payload length before advancing its cursor. The
  bulk operation reuses the ordinary read capability bit; it does not mint
  wider authority. `vfsd` still obtains geometry and performs FAT
  reads/writes/flushes only through storaged and never calls a physical
  boot-block broker.
- Storaged startup performs one generation-bound, non-mutating FLUSH and emits
  its storage E2E proof only after the request crosses the block broker and
  shared ring, the Linux DVM completes the read-only media barrier, and
  storaged consumes the exact completion. This proves transport liveness, not
  backing-image write durability; the host separately syncs the private image
  and its containing directory before signing the aperture. Storage-DVM KVM
  acceptance requires this proof in addition to both peer readiness flags and
  exact geometry.
  `cargo xtask kvm-smoke --timeout 30 --storage-dvm-only` is the independent
  acceptance gate; it cannot fail or pass because of a UI/GPU marker.
- Storage transport availability is a separate state from the `storaged`
  endpoint namespace publication. A registered endpoint proves only the exact
  service owner and capability lease; it must not make a caller wait behind a
  startup media barrier or a different caller's `DVM_READY` wait. The startup
  proof remains generation-bound and mandatory for its acceptance gate, while
  requests before that proof receive an explicit bounded not-ready result and
  retry through their owning policy path. The early-system runtime/compositor
  closure must not depend on that result: it is admitted from signed immutable
  entries, and mutable applications or DVM-volume assets wait on their own
  storage readiness. One response turn uses exactly the `INFO` generation that
  matched the proof: metadata is built from that snapshot, and a stale read,
  write, flush, or bulk-read argument returns `EAGAIN` before it can submit a
  DVM ticket. If the sole readiness worker cannot start, storaged exits for
  rootd revocation/restart rather than advertising an endpoint that can remain
  not-ready indefinitely.
- The Linux-DVM block relay starts immediately after the guest's module
  admission turn (`S12` after Buildroot `S11modules`), before unrelated guest
  network, display, and control-agent scripts. It still discovers exactly one
  assigned AHCI/NVMe controller and admits the complete AHCI `sd_mod` closure
  before opening the relay. The DVM build rejects a stale late-start script,
  so a rootfs overlay change cannot silently re-serialize storage behind DHCP
  or GPU startup.
- A DVM image carrying GPU vendor modules, GPU firmware, Mesa/LLVM, and a
  storage relay cannot satisfy the five-second mutable-volume readiness
  requirement merely by reordering init scripts: kernel initramfs unpack is
  itself before every service. The required bootstrap topology is therefore a
  separate signed **storage-bootstrap DVM** with only the fixed block relay,
  its exact AHCI/NVMe closure, the bounded block aperture, and the minimum
  init/runtime dependencies. The full display/network DVM remains a separate
  least-authority domain and cannot gate storage readiness. Each DVM has its
  own image digest, launch secret/epoch, readiness generation, restart budget,
  and revoke path; neither may borrow the other's device or controller
  authority. A storage-bootstrap `READY` proves only its exact block DVM
  identity, and RustOS still requires the generation-bound FLUSH completion
  before mounting the mutable volume. A staging/validation implementation must
  measure kernel-start-to-peer-ready independently, reject a five-second
  breach, and prove that a display/network DVM restart neither revokes nor
  stalls an already-live storage generation.
- Boot diagnosis records kernel-stamped `dvm-block-transport-installed`,
  `dvm-block-peer-ready`, `dvm-block-first-completion`, and the Required
  `dvm-block-transport-revoked` milestone. The revoke frame binds a closed
  nonzero reason in `arg0` and the installed generation in `arg1`, while the
  fixed diagnostic line records the pre-clear flags and ring cursors. Together
  with `ipc-service-capability-request`, `ipc-service-capability-reply`, and
  `ipc-service-register`, these delimit transport admission, the first real
  DVM completion, terminal transport evidence, rootd capability latency, and
  endpoint publication without treating debug print ordering as timing
  evidence. A UI-critical path above five seconds is a failed performance
  acceptance result; the three-second target is the optimization objective,
  not permission to bypass storage or rootd authorization.
- Physical display and storage device classes are signed loadable modules:
  `i915`, `xe`, `amdgpu`, `nvidia-drm`, AHCI, and NVMe may be selected only by
  a kernel-produced modalias for an assigned PCI function after the DVM is
  live. The fixed QEMU bootstrap keeps only virtio display, block, input, and
  network transports built in. Modularity is a lifecycle and authority
  boundary, not permission to omit a supported vendor profile or to probe
  unrelated devices; every packaged module remains signature-pinned and
  artifact-verified, and NVIDIA UVM/compute stays outside the display DVM.
- The schema-9 DVM boot artifact is one hash-bound `cpio.zst` initramfs. The
  build uses deterministic zstd level 3 with one compression worker and the
  kernel must advertise `CONFIG_RD_ZSTD`; an XZ artifact, schema-8 manifest,
  stale alternate rootfs, or decompressor fallback is not launchable. This is
  a boot-critical latency choice only: it does not weaken artifact hashing,
  module signatures, device ownership, or the Linux/Windows application ABI.
- The x86 arch default is only a configuration seed. Final DVM admission
  rejects unrelated built-in driver families including wireless, PCMCIA,
  software RAID/device-mapper, generic Ethernet vendors, sound, media, HID,
  desktop input classes, disk/network filesystems, SELinux policy, kexec,
  suspend, and hibernation. The fixed virtio bootstrap remains built in while
  physical GPU and storage classes remain signed modules. A source change to
  one external leaf module has its own Buildroot invalidation stamp and may
  rebuild that module plus the rootfs, but never the host toolchain, Mesa,
  LLVM, or unrelated services.
- Runtime diagnostics are bounded observation, not an event transport. Uiserver
  keeps its one-second aggregate heartbeat and bounded fault/cursor evidence,
  but does not print a per-input-pulse line while a pointer moves. Input state
  remains in the service-owned queue and its readiness generation; eliminating
  that debug-console work cannot alter delivery, wake, or compositor policy.
- Storaged may satisfy repeated FAT metadata and executable reads from at most
  eight validated 64-KiB read-ahead windows (512 KiB total), but it first
  revalidates live DVM geometry and the exact caller generation on every
  request. A hit must be wholly contained in one exact-generation window. A
  first or noncontiguous miss submits one forward window. Only a later miss at
  that exact window boundary may submit up to two contiguous forward windows,
  each no larger than one fixed DVM slot, before waiting for the first
  completion. The batch leaves headroom in the eight-window cache for
  interleaved FAT metadata and limits each completion batch to 128 KiB.
  This uses the transport's bounded queue for proven sequential demand without
  turning random FAT metadata probes into full-cache speculative reads. No
  window is published to the cache until the complete batch succeeds; submit
  failure cancels every issued ticket and completion failure cancels the
  remaining tickets and clears the cache epoch.
  overlapping replacements cannot coexist, a different generation atomically
  replaces the complete cache epoch, and every write clears all windows before
  submission. Transport-info failure also clears them. Cached bytes therefore
  cannot survive revoke, restart, or mutation, and the optimization adds no
  controller authority or hidden fallback to vfsd.
- Vfsd's separate immutable file-byte cache is an admission-controlled
  convenience cache, not a DVM-read substitute. It may materialize at most
  one 16-MiB file within a 32-MiB total budget only for a zero-offset request
  that covers that complete file. ELF headers, program-header probes, partial
  sequential reads, and nonzero-offset reads must use the bounded FAT
  range-read path instead. Thus a loader validation request for tens or
  hundreds of bytes cannot silently turn into a complete executable transfer
  across the block-DVM boundary; remount invalidation still clears every
  materialized byte before a new generation is served. The already-open
  read-only description's validated length bounds each range read and cache
  decision, so vfsd must not re-open FAT metadata merely to serve a small
  `pread`; reopen/metadata policy remains explicit at `openat` and remount.
- The FAT adapter retains one clean logical-block scratch line across adjacent
  small reads. Repeated directory-entry and FAT-chain reads within that block
  therefore cause one DVM block RPC, not one RPC per parser fragment. A write
  overlapping the line invalidates it before submission; a successful partial
  read-modify-write republishes the updated line, while an error leaves it
  invalid. This cache owns no dirty data and grants no authority beyond the
  mounted block device.
- A storaged write's declared logical-block count must checked-multiply to
  exactly the payload length and fit the admitted capacity before submission
  or read-cache invalidation. Payload length cannot silently redefine the
  caller's requested write extent.
- Storaged invokes RustOS-private syscalls only through the raw
  `rustos-svc-runtime` entrypoints. A libc wrapper must not collapse raw
  negative results into `-1` plus TLS `errno`; transient `EAGAIN`, absence,
  revocation, timeout, and protocol failure remain distinct across the full
  kernel-to-vfsd path.
- Storaged and vfsd each emit one diagnostic for a contiguous startup
  `EAGAIN`/`ENODEV`/`ENOSYS` interval and reset that diagnostic state only after
  one validated DVM or storaged reply. They never suppress envelope, binding,
  I/O, or authorization errors. Runtimed keeps a DVM-not-ready launch entry
  retryable and backs it off for 250 ms, so loader probes cannot flood the same
  bounded transport or turn expected readiness into debug-console work while
  the readiness owner is completing its proof.
- Rootd's least-authority service graph contains the explicit
  `vfsd -> storaged` edge. Before storaged publication, lookup preserves the
  transient registry errno; denial preserves `EACCES`. Kernel lookup validates
  canonical error responses before applying success-only value invariants, so
  neither state can be fabricated as `EINVAL` and cached as permanent.
- The signed early-system closure includes the `uiserver` executable and its
  already-required dynamic runtime, so the compositor can launch without a
  cold DVM-volume read. This grants neither storage-controller nor display
  authority to ring0: the DVM still owns the root volume and GPU/KMS provider,
  and uiserver waits for its authenticated display contract. General apps,
  WayClick, desktop assets, and mutable policy remain DVM-volume owned. A
  transient loader or DVM display-provider error schedules a bounded 500-ms
  supervisor retry; only the existing permanent-launch errno set disables the
  entry. Successful spawn ownership prevents duplicates while endpoint
  readiness is pending.
- Design sources: Linux VFIO defines the IOMMU group as the minimum viable
  ownership unit (`https://docs.kernel.org/driver-api/vfio.html`);
  RFC 8032 defines the Ed25519 signature encoding and verification algorithm
  used to authenticate immutable storage transport epochs
  (`https://www.rfc-editor.org/rfc/rfc8032.html`);
  Linux writeback-cache control defines PREFLUSH/FUA completion ordering and
  the rule that a successful flush makes earlier writes durable
  (`https://docs.kernel.org/block/writeback_cache_control.html`);
  Linux stable block queue ABI defines logical/physical sizes, FUA, discard,
  write cache, and write-zeroes
  (`https://docs.kernel.org/admin-guide/abi-stable-files.html`);
  QEMU ivshmem-doorbell defines the shared-memory server and interrupt-vector
  topology (`https://www.qemu.org/docs/master/system/devices/ivshmem.html`,
  `https://www.qemu.org/docs/master/specs/ivshmem-spec.html`); Linux sizes
  standard BAR dwords with decoding disabled before composing a 64-bit
  resource
  (`https://github.com/torvalds/linux/blob/master/drivers/pci/probe.c`).

