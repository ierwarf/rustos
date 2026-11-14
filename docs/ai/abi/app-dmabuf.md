# Application DMA-BUF import (Wayland `zwp_linux_dmabuf_v1`)

Status: source-complete across Ring0/Ring3 and the DVM C exporter for linear
single-plane ARGB8888/XRGB8888. The DVM C sources were type-checked on the host
(see "Verification status") but **nothing here has been booted or run under KVM**.
See "Not done / unverified" before relying on any part of this.

## Architecture (approved; preserves single-atlas composition)

* Authority is a RustOS kernel capability (`KernelHandle::DeviceBuffer`,
  `kernel/ps/src/user/handles/device_buffer.rs`). Nothing a client, the DVM, or
  `libs/driver-domain-protocol/src/app_buffer.rs` (`DvmAppBuffer`, 64-byte
  record) states is authority: the record only describes a range the kernel
  already holds.
* Ring0 supplies mechanisms only: one persistent aperture registry (rights,
  generation, revoke, mapping holds, reclaim), the exact devmgrd publication
  lease, the BAR2 mailbox, user mapping with TLB-synchronous teardown, `pread`,
  `fstat` identity and scrub. Policy (who may allocate, size limits, timeout
  and retirement driving) is `services/devmgrd/src/app_buffer.rs`.
* Composition is unchanged: `uiserver` copies each committed buffer into
  compositor-owned pixels through the existing bounded snapshot workers.

## Aperture

512 MiB shared writable virtio-pmem at GPA `0x1_2000_0000`, adjacent to (and
disjoint from) the read-only atlas pixel pool. `verify_aperture()`
(`kernel/io-manager/src/io/dvm_display/app_buffer.rs`) requires the physical
allocator to own no frame of the range (`phys::range_overlaps_boot_usable`) and
a virtio-pmem PCI function (1af4:105b) to be present. It does not read the
pmem function's start/size registers. No user PTE can be built before it
succeeds (`aperture_phys`).

## Allocation (client -> devmgrd -> broker -> DVM)

1. Client calls `SYS_RUSTOS_APP_BUFFER_ALLOC` (`RustosAppBufferAllocArgs`:
   width, height, stride, format), normally through `libs/rustos-app-dmabuf`
   (see "Client library"). The kernel first checks the per-holder quota (below),
   stamps pid/tid and calls devmgrd (`DevmgrdAppBufferRequest`); devmgrd refuses
   a pid/tid that is not the IPC sender.
2. `devmgrd` validates (linear XRGB/ARGB, <=8192 px, stride >= 4*width, 4-byte
   aligned, <= 64 MiB) and drives the broker `SYS_RUSTOS_APP_BUFFER_BROKER`
   (devmgrd-only; the caller must own the live devmgrd endpoint). BEGIN
   allocates from the registry, reserves an invisible fd slot, and publishes
   the record to the DVM mailbox under `with_devmgrd_publication` (owner pid +
   endpoint epoch + `DevicePolicy`). Request and reply are bound by the mailbox
   sequence; only the matching acknowledged sequence is accepted.
3. TAKE commits the fd only on a timely reply and under the same live
   publication lease. Past the 500 ms deadline, owner/epoch change, provider
   error or lost lease the slot is cancelled and the range is revoked at the
   DVM; the range stays quarantined until the DVM acknowledges that its final
   dma-buf reference is gone. `ECANCELED` from TAKE means "retired, no fd".
4. The reply carries exactly one transferred handle; the kernel installs it
   only if it is a `DeviceBuffer`, and only if the caller's table is still under
   quota at that instant (the quota check and the install are one critical
   section on the caller's handle table; on refusal the transferred handle is
   dropped and the caller gets `ENOMEM`).

Concurrency: devmgrd fronts the broker with one worker and a bounded queue
(`QUEUE = 4`, overflow is refused with the reply capability released). The
broker intentionally holds a single pending allocation because the BAR2 mailbox
carries one outstanding record; a second `BEGIN` while one is pending or still
retiring is `EBUSY`; devmgrd drains retirement before each request (bounded
50 ms) so the client sees `EBUSY` only if a retirement outlasts that. A deeper broker queue would need a multi-slot mailbox in
the DVM exporter and is not implemented.

Per-client quota: a process may hold at most `APP_BUFFER_CLIENT_QUOTA_BYTES`
(128 MiB) of distinct app DMA-BUF allocations in its descriptor table
(`HandleTable::device_buffer_held_bytes`; duplicates of one allocation count
once, revoked capabilities still count until closed). This is a per-holder cap. Each
installed allocation is additionally billed to the caller's Job as
`kmem_charge::Class::DeviceBuffer` (ABI `JOB_KMEM_CLASS_DEVICE_BUFFER`, loaderd
budget 256 MiB), charged in the same critical section as the quota check and
install (refused as `ENOMEM`, refunded if install fails). The charge is stored
on the allocation, stays with the original Job across duplicates, forks and
transfers, and is returned only by `BufferPool::reclaim_after_scrub`, so a
revoked-but-unreclaimed range keeps counting. Passing descriptors to another
process therefore cannot cycle allocations past the Job budget; the 512 MiB
registry and devmgrd's 64 MiB per-buffer bound still hold.

## Client visibility

* `fstat`: `S_IFCHR`, `st_rdev = APP_BUFFER_STAT_RDEV`, `st_dev` = provider
  generation, `st_ino` = allocation id, `st_size` = capability length.
* `mmap(MAP_SHARED)`: syscalld plan `DeviceShared` -> `broker_map_device_buffer`.
  Offset/length/rights/revocation are checked against the capability
  (`admit_mapping`); the physical address derives only from the kernel record.
  The mapping record (`ProcessState::device_buffer_mappings`) holds the
  capability mapping count. Release happens only on `Drop` of the hold, which
  runs (a) in the munmap post-unmap callback, after the synchronous shootdown,
  and (b) with the retired execution payload after the root is reclaimed (exec
  and exit). A leaked hold quarantines the range forever (fail closed).
* `fork`: device-buffer mappings are not inherited (no child PTEs, no custody);
  the child must receive its own handle. Handles are `FD_CLOEXEC`.
* `pread`: bounded kernel copy under a whole-capability hold; `ESTALE` once
  revoked.
* Revocation does not force-unmap existing client mappings (a deliberate
  decision, not an omission): it closes new admission and keeps the range
  unreusable until every mapping, capability reference and the DVM export are
  gone, then the kernel scrubs (zeros) the range outside the registry lock and
  only then frees it for reuse. A revoked-while-mapped client therefore keeps
  access only to its own quarantined range, never to another client's data;
  the cost is availability (a client that never unmaps holds the range until it
  exits or execs). Synchronous revoke would reuse the munmap shootdown path
  across processes and is not implemented.

## Client library

`libs/rustos-app-dmabuf` is the single client-side owner of the allocation
syscall. `AppDmabuf::allocate(&KernelSource, w, h, Format)` validates geometry
(stride = 4*w, nonzero, <= 64 MiB), calls the syscall, then refuses anything
that does not `fstat` as the genuine handle (`S_IFCHR`, `APP_BUFFER_STAT_RDEV`,
`st_size` >= image), and maps it read/write. `apps/wayclick` uses it when
`RUSTOS_WAYCLICK_DMABUF=1`: it binds `zwp_linux_dmabuf_v1` v3, allocates two
linear ARGB8888 buffers, fills pixels through the mapping and imports each with
`create_immed` (fd over SCM_RIGHTS). If requested and allocation or import
fails, wayclick stops instead of presenting through `wl_shm`.

There is no libgbm/DRM shim: Chrome's GBM path is not connected to this
syscall. The library is the intended surface for such a shim.

## Wayland import (`services/uiserver/src/wayland/dmabuf.rs`)

Advertises `modifier(ARGB8888|XRGB8888, LINEAR)` (v3). `create`/`create_immed`
require: exactly plane 0, modifier LINEAR, flags 0, supported format, fd
`fstat` identity above, geometry fitting `st_size`, and a one-byte `pread`
probe (stale/revoked -> reject). `create` failures send `failed`;
`create_immed` raises `invalid_format`/`out_of_bounds`/`invalid_wl_buffer`
after consuming the new id. The imported fd is the pool of an ordinary
`BufferData`; `wl_buffer.release` follows the `wl_shm` snapshot-complete rule.
A second use of a params object is `already_used`.

## DVM exporter (C)

`rustos_dvm_ivshmem_uio.c` consumes the mailbox, validates the record, refuses
overlapping live exports, zeroes the range, exports a real dma-buf (coherent
attach/map only, `begin/end_cpu_access` barriers, `mmap`), and on revoke
acknowledges only from the dma-buf `release` callback. The relay
(`rustos-dvm-display.c`) holds the fd until the exact revoke.

## KVM probe (`kvm-smoke --app-dmabuf-probe`)

Opt-in; implies `--gui-dvm-surfaces` (the app aperture exists only in that
topology). xtask writes the private contract
`system/registry/system/appdmabuf-probe-v1.env` (`contract=rustos-appdmabuf-probe-v1`)
on the per-run boot disk; `apps/wayclick` reads it and, only then, presents
through app DMA-BUFs, sends a forged descriptor (a pipe) with the non-immediate
`create`, and exits after a frame is presented and a buffer released. The
production image has no such file. Required markers, in this order
(`tools/xtask/src/kvm/app_dmabuf_probe.rs` validates order, success status,
sequence equality and that the revoked/scrubbed range equals the exported one):

Kernel markers are debug milestones (the only kernel channel that reaches
debugcon): `name=... arg0= arg1=`; ranges are packed as
`(offset pages << 32) | length pages`.

1. `name=app-dmabuf-export-published arg0=seq arg1=packed range` (kernel mailbox)
2. `name=app-dmabuf-export-ack arg0=seq arg1=0` (DVM response read by the kernel)
3. `name=app-dmabuf-allocation-granted arg0=fd arg1=length` (kernel installs the fd)
4. `wayclick: app-dmabuf allocated index=` (client mapped it)
5. `uiserver: app-dmabuf import accepted width=`
6. `uiserver: app-dmabuf snapshot ok bytes=`
7. `uiserver: app-dmabuf buffer released`
8. negative: `uiserver: app-dmabuf import refused reason=NotAppBuffer` and
   `wayclick: app-dmabuf probe forged-fd refused`; `wayclick: app-dmabuf probe passed`
9. after the client exits: `name=app-dmabuf-revoke-published`,
   `name=app-dmabuf-revoke-ack arg1=0`,
   `name=app-dmabuf-range-reclaimed arg1=packed range` (emitted only after the scrub succeeded)

Any line containing `app-dmabuf-probe: FAIL` ends the boot at once. uiserver
logs at most the first 3 lines of each import/snapshot/release kind. A stale
(revoked) descriptor is not exercised end to end: a client cannot revoke its own
buffer, so that path stays covered by the host tests only.

## Not done / unverified

* Nothing here has been booted (the probe above is armed but has not run); the
  virtio-pmem start/size registers are not read.
* No real GPU producer imports these dma-bufs yet; sync is CPU-barrier only.
* No libgbm/DRM shim: Chrome does not reach `SYS_RUSTOS_APP_BUFFER_ALLOC`; only
  the wayclick opt-in path does, and it has not run under KVM.
* Quota is per descriptor-table holder, not a Job charge (see Allocation).
* `formal/app-buffer-custody` models allocation, export, timeout, provider
  restart and scrub with one range; it does not model mappings, per-page TLB
  acknowledgement, multiple ranges or the quota.

## Verification status

Host-run: `rustos-app-dmabuf` (geometry, forged fd, mapping), uiserver
object-level import over the real Wayland wire with the kernel observations
substituted (forged fd, revoked/stale probe, out-of-bounds geometry, double
use of a params object, params and imported-buffer exhaustion), kernel-ps
registry and handle-table tests (stale/reused reservation, revoke with
mappings, exhaustion, held-byte accounting), generation-bound process access
including a reused PID, devmgrd request validation, and the broker's quota and
error mapping. The DVM C was compiled, not run: `rustos_dvm_ivshmem_uio.c`
with the buildroot GCC 14.3 against the configured linux-6.12.94 tree
(`-Wall -Wextra`, no warnings; every undefined symbol resolves to an export and
`MODULE_IMPORT_NS(DMA_BUF)` covers the three namespaced ones), and the relay
plus GPU runtime/probe with `-std=c11 -Wall -Wextra -Werror` against the
buildroot sysroot. modpost/vermagic and loading were not exercised.
