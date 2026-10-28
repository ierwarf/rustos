# Release-blocking proof gaps

Owner page split from [the coverage gate](../COVERAGE.md). Entries record the
closed or remaining release-blocking gap, its model, and its source/runtime
evidence boundary.


Procd and syscalld now consume independent kernel fan-out queues and perform no
cross-service discovery while draining. A replacement procd can therefore
rebase queued signal-policy evidence before serving, while a replacement
syscalld rebases queued credential/MM evidence before its first request. The
former procd-to-syscalld process-exit request was retired, removing both the
bootstrap dependency cycle and its unauthenticated PID-shaped cleanup surface.
The restart topology now has a current signed-image parallel KVM witness in
addition to its source/model evidence. The runtime terminal was reached within
the five-second product bound without recreating the rootd/loaderd/procd cycle,
panic, or stalled lifecycle drain. The same run passed the three-window 55 FPS WayClick,
uiserver-input, and DVM page-flip gates. Physical-GPU admission remains a
separate hardware-specific evidence boundary.

Provider reference mutations now carry kernel-minted 128-bit operation IDs.
Netd reserves replay capacity before mutation, retains the exact result until a
kernel ACK, rejects aliases, and never evicts an unresolved result. Compat
retries the same identity and keeps unresolved mutation/ACK work in a bounded
reconciliation queue. Remote VFS dup/fork/transfer no longer mutates a remote
refcount at all: the kernel fd table owns descriptor references and vfsd sees
only initial open and idempotent final close. This closes the ambiguous remote
increment/decrement mechanism instead of adding compensating guesses.

The vfsd capability boundary now combines three independent checks. Rootd
admits lookup only on an explicit service dependency edge; object IDs are
kernel-minted boot-entropy capabilities preserved across dup/fork/transfer; and
vfsd/netd compare payload identity with the kernel-stamped IPC sender. A
service PID or a difficult-to-guess token alone is not accepted as authority.
The entropy source itself is a capability-gated boot-CSPRNG broker; the retired
PID/TID/counter generator is not a security fallback.

Vfsd commits live epoll identities and complete interest records to rootd's
authenticated, versioned opaque checkpoint before local publication. Ring0 is
the sole descriptor-reference authority, so dup/fork/transfer do not issue
provider mutations; vfsd sees only create and the idempotent final-retire
tombstone. A
replacement vfsd scans and validates the checkpoint, reconstructs its registry,
and only then creates/publishes the endpoint; malformed or partial replay fails
closed. The `service-mutation-recovery` model covers unique commit revisions,
crash, replay-before-publication, and reconciliation. Live crash-injection/QEMU
evidence for the replacement-service path remains an acceptance gate; passing
source/model checks alone does not claim that runtime evidence.

Ordinary vfsd open descriptions now retain their normalized path, kind,
cursor, length/content identity, and mutable status flags in rootd. OPEN uses a
staging parent plus exact path chunks and publishes only the final record.
Sequential read/getdents uses prepared cursor state so failed kernel copyout
cancels rather than skipping bytes. Final close and epoll deletion retain exact
tombstones across response loss; a separate visibility ACK authorizes rootd
compaction, so long churn does not consume the fixed checkpoint bound and an
old proof cannot erase a reused key. `vfs-open-description-recovery` covers
these ordering rules. Live crash-injection remains required before runtime
acceptance is claimed.

Immutable desktop and launch-policy files use bounded explicit-offset snapshot
reads. They therefore retain their open-description cursor and cannot add a
prepare/commit checkpoint transaction to vfsd's single request executor while
latency-sensitive readiness and Wayland control traffic is pending. The source
witness seeds a nonzero cursor and the implementation-mutation gate replaces
the positioned read with a sequential read; that mutant must fail.
The private KVM acceptance contract uses the same primitive with a separate
256-byte protocol ceiling in both runtimed's launch injection and uiserver's
bounded late watcher. Source conformance rejects either consumer if it falls
back to a sequential cursor-mutating read.

Dedicated finite abstractions now cover raw ELF/PE parser admission, page-table
lifecycle, DMA-domain isolation, authenticated boot-file contents, DVM packet
payload admission, and the bounded System-to-User CPU reservation. Pinned Kani
0.67.0 source proofs additionally cover exact little-endian field decoding,
arbitrary ELF load-segment and PE section admission, entry/W^X invariants,
missing relocation tables, one arbitrary relocation entry's bounded exact
effect, and one arbitrary import thunk's identity and bounds. Verus proves the
five unbounded runtime-response theorems. These proofs do not by themselves
close the release gates: arbitrary-length multi-block/multi-descriptor parser
equivalence and runtime fault evidence still require independent artifacts.
Commercial release remains blocked until the same properties have source
conformance plus runtime fault evidence.

IPC handle receive now reserves descriptor slots invisibly, copies the complete
numeric result, then publishes the whole batch at one descriptor-table commit
point. Reservation generations survive ordinary concurrency and prevent a
stale pre-exec transaction from cancelling or committing into numerically
reused slots; fork does not inherit reservations, while exec/exit revoke them.
Runtime output-remap fault injection remains required acceptance evidence, but
the former partially visible installation path is no longer present in source.

Provider-publication rollback reclaims vfsd objects and netd tokens when local
fd admission, pair installation, or pair copyout fails. Process close, CLOEXEC,
exit, fork rollback, and cancelled transfer use the same descriptor-exact
cleanup: remote VFS closes are final-reference-only and idempotently queued;
netd closes retain replay state through explicit ACK. Queue/capacity failure is
observable and fail-closed. Runtime provider-crash injection remains required
to accept the complete teardown topology.

The signal proof now includes SIGSTOP alongside the scheduler-level stop,
SIGCONT resume, and wait-status lifecycle that `process-signal-delivery` and
`sigchld-notification` audit as matched in `CONFORMANCE.md`.

Historical retirement gaps (superseded at source level): scheduler retirement
revoked generic IPC authority, but forced/fault/invalid-context paths lacked
exact target-thread `clear_child_tid`, robust-owner-death, waiter removal, and
deferred terminal-notification cleanup. The earlier current-thread-only futex
proof did not cover those paths, and long-deadline retirements could retain
bounded waiter-table slots.

The current `futex-waiter-lifecycle` contract in [CONFORMANCE.md](../CONFORMANCE.md)
records exact deferred cleanup identities and Linux-thread snapshots for every
retirement, with acknowledgement before slot reuse. The bounded consumer in
`kernel/compat/src/user/syscall/linux/lifecycle_broker_ops.rs` removes runtime
registrations and handles scheduler-stamped terminal fault notification outside
scheduler state; executive housekeeping services that consumer. The
`thread-exit-futex-cleanup` contract records translated bounded atomic
compare-exchange for robust owner death and release-store `clear_child_tid`
cleanup. These mechanisms supersede the missing-protocol and missing-atomic
implementation claims, not the independent runtime, SMP, or hardware evidence
gates. No such qualification is inferred from source or the finite model.

The virtual compositor topology now intentionally uses one staged upload into
virtio-GPU and labels it `source-path=staged-copy zero-copy=0`; it cannot close
the physical DMA-BUF-source/atomic-scanout gate. A bounded QEMU capture reached the
real virgl/radeonsi prime, DVM GLES/KMS activation, authenticated input-ring
drain, and the padded 1600x900 provider pitch of 7168 bytes. Moving atlas
initialization to a bounded worker fixed the observed UI-loop stall: the worker
started, demoted itself, completed atlas mapping, and the main loop remained
live. That capture then exposed the next real defect: the old clear-only prime
advertised readiness before a full atlas upload/textured draw, so the first real
batch exhausted its frame budget and revoked the DVM epoch. The source now
requires a representative full-atlas, textured-draw, EGL-fence, and
atomic-present prime under a 500 ms setup budget, with separate
upload/render/fence/pageflip failure stages. A subsequent 30-second run passed
all UI, input, relay-FPS, and fence-count subgates but revealed that unpaced
90-100 FPS submission eventually timed out one relay frame and the independent
health context. uiserver now grants one non-accumulating 15 ms cadence permit,
retains early work behind `EAGAIN`, and never falls through to CPU presentation.
The next paced 30-second capture exposed a second defect visible on the QEMU
display as periodic `display not available`: the 16.667 ms commercial target
was also the epoch-killing fence timeout, so ordinary scheduling jitter caused
repeated provider revoke/re-prime cycles and eventually stopped input delivery.
The contract now keeps the 16.667 ms performance gate but uses a distinct,
bounded 50 ms hard timeout; the previous front buffer and context remain live
inside that jitter window. The first post-separation packaged capture recorded
zero compositor-offline/context-loss events and kept fence counts matched, so
the visible provider-revoke defect is fixed. It still failed performance
because several one-second windows contained 17--19.8 ms GPU maxima, and its
16-second synthetic-input bound ended before the 30-second gate. Extending the
source then exposed fixed-ring saturation at 3,622 events. The source is now a
bounded 66.7 Hz/90-second producer, and steady-state uiserver submission never
waits for a GPU slot on the UI thread. The next exact 30-second packaged
capture passed independent GPU proof, RustOS rendering, DVM relay
performance/fences, and display lifecycle with no compositor revoke or offline
marker. Its sole failed predicate was input: 92--102 accepted events/s, zero
drop/slow/error/backlog, and at most 35 ms reader age still contained 61--153
ms inter-event gaps. The earlier dedicated uiserver reader used a
non-accumulating 4 ms direct-read cadence to bypass the
ring0/service-queue lost-wake race. That compatibility route is now retired:
the reader blocks on the generic provider-generation epoll contract, treats
revoke as ERR/HUP, and issues authorized READ only after positive readiness.
The agent now grants only an authenticated live input stream SCHED_RR
priority 10 under a 50/100 ms soft/hard `RLIMIT_RTTIME` guard, and both KVM and
physical launch descriptions select the verified dynamic kernel's full
preemption mode. Boundary profiling then proved DVM-to-L0 delivery was already
continuous at 99--101 frames/s with a 36.784 ms worst observed gap. The false
61--243 ms result came from recording input arrival only at the end of a UI
turn: a retained GPU-backpressure retry skipped that health snapshot after the
input was already consumed. Arrival is now recorded at queue consumption and
the last timestamp survives one-second sample rollover.

The final packaged, 30-second-bounded acceptance run passed every QEMU
predicate. The gate now requires one identical consecutive sample set to pass
both render-rate and input-integrity predicates; disjoint good FPS and input
windows cannot be combined into success. Its five accepted UI windows were
60.940, 60.821, 60.940, 63.000, and 62.000 FPS, with 88/92/90/89/96 input
events, 80/87/86/87/92 presented cursor moves, 34/27/35/28/24 ms maximum input
gaps, 27/26/23/18/19 ms maximum input age, exact logical/presented coordinates,
and zero drop/slow/error/backlog. No compositor-offline, submit-loss,
display-unavailable, input-watchdog, or input-reader-error marker occurred.

The DVM relay now samples inside the continuously drained submission loop, so
a permanently non-empty queue cannot suppress one-second evidence. Per-frame
serial logging is rate-limited, and each accepted window must contain matching
page-flip, GPU-fence, and present-fence completion counts, zero relay CPU copy,
at least 60 FPS, at most 12 ms average GPU/atomic work, and at most 16.667 ms
maximum GPU render time. The unpaced run captured nine consecutive passing
windows before one later timeout, while the first paced run visibly cycled
through provider unavailability. The post-timeout-separation capture proved no
revoke but not three consecutive maximum-latency windows. The independent probe
now publishes `performance-target=0` instead of suppressing the entire display
relay when only that performance target misses. A stricter ten-window request
then exposed one 17.986 ms GPU tail and two one-loop input backlogs, rather than
device loss. The authenticated GPU/KMS relay now admits only its live interval
to `SCHED_RR` priority 9, below input priority 10, with a verified 50/100 ms
`RLIMIT_RTTIME` guard and mandatory restore. An eight-window request produced
only five active post-boot windows within the 30-second global cap, so it was
correctly rejected rather than counted as an eight-window pass. The final
five-window gate passed at 64.545, 62.495, 61.757, 63.194, and 62.550 FPS with
equal page-flip/GPU-fence/present-fence counts of 65/63/62/64/63, zero relay CPU
copy, and 12.605/12.777/13.495/15.091/12.407 ms maximum GPU render time.
The separate standard Wayland client proof remains failed. Restoring
WayClick's normal blocking dispatch and compacting netd first raised the loop
from about 1 FPS to 8.8--14.3 FPS. Caching only successful, immutable
per-process syscalld admission for `CLOCK_MONOTONIC` then reached 18--23 FPS.
Coalesced presentation, exact damage, direct mapped atlas slots, bounded
readiness handoff, and ordinary User-class fair share subsequently removed the
steady bottlenecks without a client-specific route. The compositor now consumes
one permit from the previous real presentation before the next present and
phase-locks damage-free callback pulses with the 15 ms DVM presentation clock.
A pending callback also participates in the main wait deadline.

The current schema-9 bounded capture passed the exact 55-FPS command. The first
accepted three consecutive WayClick windows contained 57, 65, and 68 exactly
balanced commit/callback/release cycles over 3.060 seconds, for 62.092 FPS
aggregate, 56.394--67.342 FPS per window, and a 45 ms maximum callback gap.
The same capture passed the independently aligned uiserver FPS/input/cursor and
DVM relay/fence-count gates with no provider revoke, context loss,
compositor-offline, or `display not available` marker. The gate does not
combine disjoint compositor, client, or relay windows. Older schema-8
33--45 FPS measurements remain historical diagnostic evidence only.

The current schema-9 GUI/input F5 rerun verified display and storage in
4.214 seconds and then remained healthy for more than 60 seconds, passing the
independent five-second gate with the authenticated staged-copy compositor active on a 2048x2048
atlas and both DVM keyboard and pointer ingress observed. Uiserver overlaps
provider admission and atlas mapping with the CPU boot frame, then gives the
first local GPU frame a bounded 750 ms turn before unrelated Wayland/runtime
policy startup. In the GUI-only topology, block discovery now emits one fixed
topology-absence record and storaged emits one matching errno transition
instead of rescanning PCI and logging the same absence every 50 ms. The
separate steady-state WayClick proof now passes as described above; the
five-second boot gate remains independent and cannot substitute for it.

The generic userspace wait-set implementation now replaces the single-snapshot
`epoll_wait`: vfsd owns bounded interest sets, netd/inputd/sessiond publish monotonic
readiness generations, and compat uses atomic check-arm-recheck with monotonic
deadlines. The same bounded broker now covers transient multi-fd `poll` sets,
polling an epoll descriptor, unmasked-signal cancellation, and the temporary
signal masks of `ppoll`/`epoll_pwait`. Interests bind the target fd plus stable
provider object identity, so fd reuse cannot retarget an old open description;
the observed epoch is mutable registration state that only explicit MOD may
replace after restart. Open-description
references are acquired for dup/fork and released for close/CLOEXEC/process exit,
with last-close purge. Fork acquires from the frozen child fd-table clone, and
exec releases only the exact handles retired by its process-table commit, so a
sibling close/reuse cannot redirect either transaction. ADD/MOD pins the exact
target description through the vfsd mutation; its final guard release performs
the ordinary last-close purge, preventing purge-before-insert resurrection.
The ring0 waiter array is sized from the scheduler task ceiling times the
provider ceiling, so maximum legal task occupancy cannot exhaust the registry
before every task has one waiter for every provider.
Uiserver's demoted readiness worker waits on Wayland-
server's existing aggregate epoll fd, merges its one-slot notification with the
input wake, and requires main-loop dispatch before rearm. This closes the
source/model portion of the failed next-ABI gate without a WayClick-specific
route. The current parallel KVM witness exercises this live wait/event
workload: three consecutive WayClick callback and uiserver input/present
windows exceed 55 FPS with no loss/backlog, and the DVM page-flip relay
independently sustains the same bound.
The concrete refinement does not issue service IPC from an already scheduler-
armed task: it registers exact observations, rechecks while runnable, arms,
then verifies waiter presence. Every provider query is capped at 16 ms and at
the remaining finite wait deadline, preventing a 30-second generic service
timeout from overriding `poll`/`epoll_wait`.
Sessiond's provider covers non-system console input in transient `poll` sets:
the readiness query reports the line-discipline queue without consuming it and
the generation advances on empty-to-ready transitions and session removal;
removed sessions return `POLLHUP` and are not recreated by the query. Console
descriptors now use a unique open-description identity with exact
close/dup/fork/exec lifetime in persistent `epoll_ctl`; neither a reusable
session number nor the numeric fd is treated as that identity.

The relay cleanup also closed a source/model mismatch. The mandatory V3 atlas
header is nonzero, so the former `serve_display` direct-DMA-BUF branch was
unreachable, while a separate pre-command renderer could upload a CPU-composed
GUI snapshot and publish display readiness before any bounded command batch.
Both routes are removed. `cargo xtask check` now fails if their symbols return,
matching `NoRawCommandOrCpuSuccess` and the admission requirement that only a
validated command batch with GPU and present completion can activate the relay.
The relay readiness lock is mode-exact: schema 2 reports only the virtio
`gpu-compositor-staged-copy` mode, while schema 3 requires AMD DMA-BUF source
import, GPU composition, external acquire and render fences, a separate
three-buffer atomic-KMS pool, and no staged CPU copy; the agent and relay
payloads are source-cross-checked. The physical source path now validates the
exact live invitation and bounded batch in the kernel, returns a non-replayable
`sync_file` for the completed RustOS CPU release, and inserts an EGL server wait
before sampling. The current `dev_pagemap` ownership change now has source,
model, cold DVM compile, module-signature, and schema-8 package evidence. The
module-safe ownership check uses each valid PFN's `struct page::pgmap` identity
instead of the kernel-private `pgmap_pfn_valid` symbol; external-module modpost
and the final 39-module signature verification passed. Physical hardware
evidence remains failed.
The scope remains private (`scope-public-abi=0`): an application 3D ABI is not
claimed, and physical DMA-BUF import/page-flip/rate plus VFIO fault/reset/revoke
captures remain failed gates.

The signed pre-scheduler-hardening post-cleanup schema-8 artifact was rebuilt
and rerun for the full 30-second QEMU bound. GPU, display, input, runtime, and DVM-network
readiness all succeeded. The private proof completed 120 frames at 110.389 FPS
with 9.057 ms average and 14.392 ms maximum GPU time. The relay activated only
as `source-path=staged-copy zero-copy=0 gpu-composition=1 explicit-fence=1
scanout_buffers=3 cpu-final-compose=0`, recorded zero relay CPU copy, and emitted
no retired direct-import/legacy-renderer, provider-revoke, context-loss,
compositor-offline, or `display not available` marker. Performance remains a
failed gate: after the 29.862 FPS startup sample, relay windows were
50.730--59.105 FPS, while settled balanced WayClick commit/callback/release
windows were 35.894--41.776 FPS. uiserver itself rendered at 53.789--57.886 FPS
in those windows and its Wayland callback waits were normally 2--4 ms. That
artifact's remaining deterministic frame loss was the then-missing userspace
readiness boundary: after a callback, the client commit could not wake uiserver
and waited for its next 16 ms poll. The current cross-provider wait-set
source/model closes that mechanism, including external INET ingress, but the
result is still not accepted as 55/60 FPS success until the current bounded
QEMU/KVM rerun records event and timeout evidence.
The later scheduler/readiness-hardening artifact passed incremental C
compilation, schema-8 packaging, signature/artifact verification, and KVM input
preparation, but was deliberately not booted or rerun for WayClick performance.
Its fatal rollback/restore branches, process-owned atomic readiness, stale-file
rejection, and teardown ordering therefore have source, negative guard, and TLC
evidence only; no runtime fault-injection claim is made.
Release additionally remains blocked on:
ELF/PE multi-block corpus fuzz and native launch captures; page-table/TLB tests
on target hardware; target captures proving the supervised IOMMUFD VFIO
non-identity map, read-only display DMA fault, reset, fault injection, and revoke;
boot-media corruption/recovery; packet saturation/cancellation/backpressure and
physical-NIC behavior; and
multicore CPU-time distribution under interrupt and DVM load. The supervised
physical display-DVM source now requires QEMU IOMMUFD and rejects an absent
`/dev/iommu`; this source gate is not hardware evidence. The current host now
has root-only `/dev/iommu`, translated AMD-Vi domains, interrupt remapping, and
complete IOMMU groups. A root execution completed one real empty IOMMUFD IOAS
allocate/destroy round trip; that proves the userspace ABI but not a device
attachment or non-identity DMA map. A later bounded, unprivileged QEMU 10.2.1
lab capture did open the IOMMUFD and AMD `0000:65:00.0` VFIO cdev, bind it as
device ID 1, allocate IOAS 2, attach HWPT 3, and successfully map guest IOVA
`0x0` for `0x400000` bytes to a distinct host virtual address. That is real
physical device-attachment and non-identity-map evidence, but it used a paused
4 MiB guest outside the rejected commercial hostd lifecycle. Raising only the
waiting launch process's `RLIMIT_MEMLOCK` to 2 GiB then allowed a bounded 1 GiB
DVM run to bind the same cdev, attach IOAS 2/HWPT 3, and successfully map the
full guest RAM IOVA `0x0`--`0x3fffffff` to a distinct host virtual range. QEMU's
failed attempts to map assigned PCI BAR host addresses were the documented
IOMMUFD no-P2P-BAR caveat, while ordinary RAM mappings remained successful.
The guest booted and its signed `amdgpu` reached the exact `1002:1900` IP-block
probe at 4.899 seconds. It then failed closed with `Unable to locate a BIOS ROM`
and `Fatal error during GPU init`: this APU stores its 16,896-byte VBIOS in the
host ACPI VFCT table and the previous launch supplied neither that table nor a
ROM file. Hostd now parses the bounded checksummed VFCT, binds bus/device/
function/vendor/device identity, accepts only an exact populated or wholly
absent subsystem pair, and validates 0x55aa plus ATOM. A first rerun supplied
the original host VFCT through ACPI and booted, but Linux correctly rejected it
because the assigned function was at guest `0000:00:01.0` while the table still
named host `0000:65:00.0`. Hostd now pins the VFIO function at guest
`0000:00:08.0`, rewrites only that validated image's VFCT BDF, recomputes the
ACPI checksum, proves the VBIOS payload unchanged, fsyncs the complete private
table, and supplies it through QEMU `-acpitable`. Unit evidence covers malformed,
truncated, mismatched, partially absent, incorrectly relocated, and unsupplied
states. The 30-second relocated-table physical rerun then read the VFCT at guest
`0000:00:08.0`, reported ATOM BIOS `113-PHXGENERIC-001`, initialized PSP, SMU,
DMUB and DCN 3.1.4, passed GFX/compute/SDMA ring tests, registered amdgpu DRM
minor 0, and created `amdgpudrmfb`. QEMU then stopped at the bound timeout with
no remaining VFIO/IOMMUFD opener; the host function remained on `vfio-pci` in
group 18 with PCI command `0003`, so bus mastering was still off. This is real
manual VBIOS and GPU-initialization evidence, but it ran outside the rejected
commercial hostd reset lifecycle. Its 1 GiB diagnostic guest also failed while
unpacking the initramfs and used the stale pre-v2 artifact, so it is not relay,
DMA-BUF, page-flip, performance, DMA-fault, reset, or revoke evidence. The
later 2 GiB schema-8 run fully unpacked the initramfs and launched the DVM
agent, but a second amdgpu initialization after the prior hard QEMU timeout
failed at PSP ring creation. This is dirty-device evidence, not an artifact
failure, and it leaves repeatable physical launch failed. The bounded KVM
runner now has an explicitly non-commercial physical-GPU profile mode. Its
only certified profile accepts an already-bound AMD `1002:1900` singleton
IOMMU group and a private checksummed
guest-`00:08.0` VFCT, writable IOMMUFD/VFIO character devices, disabled reset
methods, disabled PCI bus mastering, `disable_idle_d3=Y`, and at least 4 GiB
inherited memlock. It supplies one QEMU IOMMUFD object, the VFIO function, no
virtual GPU, and explicitly no network device. Its readiness gate requires the
real `uiserver` GPU-compositor completion plus a DVM `source-path=dmabuf
zero-copy=1` frame and completed KMS page flips; driver initialization or a
test pattern cannot pass. A read-only dry-run passed these device, artifact,
and command-input gates on 2026-07-20, with the expected warning that the agent
process inherited only 8 MiB memlock. No QEMU guest was launched by that
dry-run, so physical RustOS output, throughput, graceful cleanup, and
repeatability remain failed hardware gates. The 2026-07-21 operator rerun again
proved successful IOMMUFD mapping and VFCT discovery, but failed at
`PSP create ring failed` before the DRM render node existed. That run therefore
did not exercise the corrected first-frame DMA-BUF mode contract; it is another
reset-dirty device capture. The runner now diagnoses this class before the
30-second readiness timeout and records an atomic boot-ID claim so the
reset-disabled lane cannot launch twice in one host boot. A subsequent clean
early-VFIO cold-boot run on 2026-07-21 successfully initialized PSP, SMU, DMUB,
DCN, GFX/compute/SDMA rings, registered amdgpu DRM, and mapped the VFIO BARs
and pixel pool through IOMMUFD. It then failed before publishing the GPU-ready
contract because the newly extended backend evidence exceeded its former
1024-byte serialization buffer (`errno=75`). The evidence writer now has a
checked 2048-byte bound and the KVM runner reports publication failure
immediately. At that stage the source fix had not yet received a new cold-boot
physical rerun, and the successful driver initialization was not frame,
scanout, FPS, reset, or revoke evidence. Later paragraphs record the subsequent
reruns.
An earlier operator launch inherited
the required memlock and started both guests, but QEMU aborted before DVM boot:
IOMMUFD rejected the repository-local ext4 `virtio-pmem` mapping at guest IOVA
`0x100000000` for 128 MiB with `EINVAL`. RustOS independently reached the
`uiserver` GPU-scene compiler, while the DVM serial remained empty; therefore
this is VFIO backing admission failure, not AMDGPU, DMA-BUF, or scanout
evidence. Source now places the diagnostic pixel file in private tmpfs,
preallocates it before DVM VFIO attachment, requires the commercial hostd file
to be exact-size tmpfs/hugetlbfs, and models VFIO mapping as a prerequisite to
read authority and evidence. A post-fix hardware rerun passed that first
admission point but aborted on a different mapping: QEMU 10.2.1 attempted to
map an mmap-able AMD PCI BAR into IOMMUFD for peer-to-peer DMA at guest IOVA
`0xf0000000` for 128 MiB, and IOMMUFD rejected it with `EINVAL` before the DVM
kernel produced serial output. The non-commercial runner now disables VFIO BAR
mmap and the ROM BAR and records focused mapping traces. This is a functional
diagnostic workaround with slower MMIO, not commercial performance evidence;
the QEMU 11.0 post-workaround rerun subsequently mapped the read-only pixel
pool, guest RAM, and AMD BAR regions through IOMMUFD, fetched the relocated
ATOM VBIOS, and entered AMDGPU IP discovery. It then failed closed before KMS
because the twelve-file sealed rootfs omitted the DCN 3.1.4 DMCUB firmware.
The corrected schema-8 artifact seals and digest-verifies all thirteen
DCN/GC/PSP/SDMA/VCN payloads, including `dcn_3_1_4_dmcub.bin`. Its physical
rerun loaded DMUB, initialized DCN 3.1.4, passed gfx/compute/SDMA IB tests,
registered DRM/fbcon, imported all three read-only DMA-BUF sources, and
completed the initial fenced atomic KMS prime. The first RustOS frame then
failed at the DMA-BUF acquire ioctl with `EPERM`: the host submit record carried
staged-copy while the direct importer required direct-DMA-BUF. Prime-completion
ABI v2 now authenticates the selected mode, RustOS caches that exact value, and
the DVM requires it on every submit; old, unknown, zero, or ambiguous modes fail
closed. The fixed direct path also requires explicit linear-modifier EGL import.
The same earlier capture exposed a RustOS cleanup defect: address-space teardown
treated borrowed memfd leaf mappings as owned frames and later double-freed one
when the backing object dropped. Teardown now frees only its explicit allocation
ledger, and the latest run did not reproduce that panic. The corrected source
was rebuilt and the next clean early-VFIO run passed the parallel KVM gate,
authenticated control, the private 120-frame AMDGPU proof, three-source
read-only DMA-BUF import, explicit acquire/render/present fences, and the first
real RustOS frame's atomic page flip. `uiserver` promoted the physical zero-copy
compositor twice, matching the two frames visible on the panel. Each relay
instance then stopped after exactly one real frame at `gpu-batch-validate`: the
consumer incorrectly required the fixed atlas mapping generation to increase
on every frame, while all three imported slots correctly share one
provider-epoch generation and advance only sequence/content epoch. The runtime
and TLA model now preserve that mapping generation within an epoch, reject an
actual in-epoch rebind, and keep sequence/content-epoch replay checks. The old
smoke predicate could return success after the first readiness
marker even when the next serial record took the compositor offline; the runner
now fails immediately on any offline record and requires four consecutive
fenced zero-copy frames, traversing the three-slot pool plus one reuse. The
fixed DVM artifact and next clean physical run passed the parallel KVM gate.
The panel continuously displayed the RustOS UI and accepted pointer motion;
22 consecutive relay samples advanced through content epoch 1200 at roughly
54--62 FPS, with matching page-flip/GPU-fence/present-fence counts, zero relay
CPU copy, about 5--6.4 ms average page-flip latency, and an observed 11.736 ms
maximum. The visual output nevertheless flickered rapidly. Source inspection
found that three RustOS atlas backing slots rotated while each received only
the latest global damage rectangle: the two older slots therefore alternated
stale or zero pixels even though the DVM GPU/KMS pipeline remained healthy.
`uiserver` now records each slot's retained content epoch and a bounded
contiguous damage history. An exact predecessor receives the current patch; an
older released slot replays the complete retained history, while missing
history or an uninitialized slot receives a complete atlas snapshot. This
source fix has unit, workspace-check, RustOS
image-build, and existing DVM artifact-verification evidence. The next
cold-boot physical visual rerun confirmed a coherent, non-flickering RustOS
screen and completed the 224-event absolute-pointer square with zero input
drops, errors, backlog, or cursor mismatch. It then exposed an independent
availability defect: after the final synthetic event, uiserver started one
more stateful inputd authorize/read transaction solely to probe an empty queue.
That IPC remained active for 3,156 ms (`read_attempts=1218`,
`completed_reads=1217`), and the input watchdog terminated uiserver, leaving
the last physical scanout frozen. The DVM remained healthy through five
matched page-flip/GPU-fence/present-fence samples with zero relay CPU copy;
there was no GPU/KMS offline record. Uiserver now performs the existing
16 ms-bounded, non-consuming STATS readiness recheck before entering READ, and
the input-readiness model no longer permits the direct empty-queue probe. Unit
tests, the RustOS workspace/image build, existing DVM artifact verification,
and the 563,876-state input-readiness model pass. In the next cold-boot rerun,
the operator reported that the physical RustOS screen remained visually
coherent and responsive; neither rapid flicker nor the post-input frozen frame
recurred. This closes those two visual regressions but is qualitative evidence,
not a throughput measurement. The earlier short capture contained only
31.785--55.081 FPS relay samples, and the user explicitly deferred further FPS
capture. The 60 FPS gate therefore remains unaccepted rather than silently
passing; supervised reset/recovery also remains a failed hardware gate. The
release image now
selects and verifies Buildroot `acpid` with the exact power-button
`/sbin/poweroff` action; hostd uses a private QMP handshake and accepts normal
shutdown only after actual QEMU exit, with bounded TERM/KILL as failed-run
fallback. Source tests and the 155-state lifecycle model pass, but no physical
graceful-shutdown capture exists yet, so that gate remains failed.
The
requested AMD `1002:1900` function remains the L0
`boot_vga` device. The operator manually unbound it outside hostd and bound it
to `vfio-pci`. The native
`reset_method` was `bus` only, while the same bus contains functions in IOMMU
groups 19--24 outside the GPU lease. For the explicitly non-commercial
dirty-GPU experiment, the operator rebound with `disable_idle_d3=Y`, disabled
all reset methods, and left PCI bus mastering off. Commercial hostd rejects
both the original escaping bus scope and the current empty reset state. The first manual
vfio-pci bind also changed PCI command `0003` to `0407` while the IOMMU group
remained `identity`; bus mastering was manually cleared to `0403`. Hostd now
requires `disable_idle_d3=Y` before binding and verifies the bit after bind and
around reset. This source hardening, lab quarantine, and physical mapping/probe
capture are not supervised reset/revoke or display evidence. NVIDIA GSP firmware also
remains non-redistributable until a product redistribution authorization is
recorded. The separate RustOS
native boot-device DMA backend remains identity-only. Therefore the DMA
hardware gate stays explicitly failed even though both finite abstractions pass
TLC.

## Static scheduler review disposition (2026-10-28)

Result of a read-only review of the scheduler's terminal path, taken after an
intermittent `irq_entry.rs` "IRQ architecture lacks the original successful
claim" panic that execution could not reproduce on demand.

Closed, with a witness in each tool that the property fits:

- **Remote terminal withdraw versus an in-flight dispatch claim**
  (`scheduler-terminal-claim`): `runqueue::retire` could retire the owner word
  of a task another CPU had claimed but not yet published as Current. Fixed by
  `retire_unless_claim_in_flight` plus a parked, unwithdrawn producer. TLA+
  model, Loom and Shuttle row `scheduler-terminal-claim-window`, Kani harness
  `terminal_withdraw_never_retires_an_in_flight_claim...`, unit regressions,
  and implementation mutants.
- **Two terminators of one process** (`oom-termination-owner`): the root
  supervisor and the memory-pressure watchdog could both run the
  once-per-process follow-up. `request_marked` now reports freshness and
  `terminate_user_process_once` returns `Started`, `AlreadyQueued` or `Absent`.
  TLA+, Loom, Shuttle, unit test, and mutant
  `termination-joined-request-claims-ownership`.

Not closed (boundary stated, not claimed away):

- The observed IRQ-entry panic is **not confirmed** to be the withdraw race.
  `capture_checked` now records which of its 17 checks refused
  (`irq-architecture-capture-refused`, `arg0` = check number) before stopping.
- The two observed `tlb_shootdown.rs` panics (2 s no-ack, then a nested
  protocol-lock timeout) have no code defect found. `tlb-ack-liveness-timeout`
  records the target CPU and APIC id before the panic. The redesign is a plan,
  [TLB Shootdown Design](../../docs/architecture/tlb-shootdown-design.md); no
  phase beyond the diagnostic is implemented.
- Scheduler modules that received **no new** model, Loom, Shuttle or Kani
  witness in this pass and rely on their existing unit tests: group-exit
  service, exit membership, execution interval, process placement.
