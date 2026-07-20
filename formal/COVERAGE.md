# Formal coverage gate

Every new TLA+ model must state its owner, linearization point, explicit safety
invariants, bounded configuration, and a concrete source mapping. A model
cannot claim to prove an implementation merely because its state machine
passes TLC.

## Gate for a protocol change

1. Name the service or kernel owner in the model header.
2. Model successful, rejected, timeout, revoke, and exit outcomes where they
   exist in the real protocol.
3. State invariants for authority, identity, lifecycle cleanup, and bounded
   resources. Use exact PID, capability, handle, or ticket identities rather
   than a path or service-name approximation.
4. Add the small exhaustive configuration to run-all-tlc.sh.
5. Keep one source-level validation: a focused Rust test, cargo xtask check,
   or a bounded KVM smoke expectation.

## Current high-risk coverage

| Risk | Model | Source anchor |
| --- | --- | --- |
| A malformed ELF64 or PE64 plan maps outside the process window, overlaps another region, creates a writable executable image, or starts outside executable memory | dual-abi-image-admission | libs/rustos-image-admission/src/lib.rs and services/loaderd/src/main.rs |
| A malformed ELF64/PE64 byte table, relocation, import, or changed post-parse snapshot reaches a process mapping | dual-abi-byte-parser | libs/rustos-image-admission/src/lib.rs, services/loaderd/src/main.rs, and kernel/compat/src/user/syscall/linux/proc_broker_ops.rs |
| A user page aliases a kernel/dead frame, remains W+X, or retains access authority after unmap | page-table-lifecycle | kernel/mm/src/memory/address_space.rs |
| A device maps outside its assigned DMA aperture or keeps DMA authority after domain revoke | dma-iommu-isolation | tools/hostd/src/main.rs, libs/driver-domain-host/src/lib.rs, and kernel/io-manager/src/driver/iommu.rs |
| Boot extents return content different from the authenticated staged file | filesystem-content-integrity | tools/xtask/src/stage/mod.rs and kernel/io-manager/src/storage/boot_volume.rs |
| A malformed checksum, fragment, unsupported EtherType, or stale session payload reaches netd | network-payload-session | libs/driver-domain-protocol/src/lib.rs and kernel/io-manager/src/io/dvm_network.rs |
| Continuously runnable System work consumes every dispatch while User work remains runnable; one busy User hides another past its ready-age bound; or a latency handoff FIFO overwrites an owner, admits duplicates/System tasks, retains stale owners, grows without bound, or consumes an unbounded dispatch burst | scheduler-cpu-distribution | kernel/ps/src/multitask/scheduler.rs and kernel/compat/src/user/syscall/linux/ipc_ops.rs |
| Stale service endpoint or capability after revoke/exit | endpoint-registry | kernel/compat/src/user/syscall/linux/ipc_ops.rs |
| Concurrent registration wins after another registrar or exit cleanup has observed an empty endpoint | endpoint-publication | kernel/compat/src/user/syscall/linux/ipc_ops.rs and kernel/ps/src/multitask/process_table.rs |
| Child runs before exact supervisor lease admission | deferred-start | services/rootd/src/main.rs and services/loaderd/src/main.rs |
| Wrong supervisor/PID becomes a post-init policy service, or another sender rebinds a running exact-PID lease | post-init-leases | services/rootd/src/main.rs |
| A crashed core service restarts in the same scheduler turn, exhausts its retry budget without elapsed backoff, or retains old service authority during pending/failed recovery | rootd-restart-backoff | services/rootd/src/main.rs and kernel/compat/src/user/syscall/linux/lifecycle_broker_ops.rs |
| A restarted initd duplicates a surviving post-init service, reclaims a ready exact-PID service, leaves an endpoint-less stale child authoritative past its deadline, or permits uiserver authority after its sessiond reporter exits | post-init-supervisor-recovery | services/rootd/src/main.rs, services/initd/src/main.rs, and kernel/compat/src/user/syscall/linux/lifecycle_broker_ops.rs |
| Core dependency or restart sequence starts initd incorrectly | rootd-bootstrap | services/rootd/src/main.rs |
| A same-CID process lacks the per-launch challenge proof yet gains control authority; a foreign DVM, mismatched reply, stale input epoch, or out-of-order relay frame gains authority | dvm-control-relay | libs/driver-domain-host/src/lib.rs, driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c, kernel/io-manager/src/input/dvm_frames.rs |
| A same-CID unprivileged process discovers the static control listener and holds its setup slot, delaying the launch agent before HMAC validation | dvm-control-endpoint | libs/driver-domain-host/src/lib.rs, tools/{hostd,xtask}/src, driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c |
| A dead DVM control agent, stale or malformed ready file, unsafe state directory, partially written candidate, or one-shot announcement is accepted as live local readiness; repeated pre-rename crashes accumulate unbounded candidates | dvm-agent-readiness | driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c and driver-domains/linux/board/overlay/etc/init.d/S50rustos-dvm |
| A DVM forges an ivshmem counter, malformed receive slot, or post-install header and makes RustOS advance a cursor, exceed a fixed ring bound, or deliver the frame to network policy | dvm-network-ring | libs/driver-domain-protocol/src/lib.rs, kernel/io-manager/src/io/dvm_network.rs, driver-domains/linux/package/rustos-dvm-net/src/rustos-dvm-net.c |
| A mapped DVM Ethernet aperture remains usable after its authenticated control session ends, a stale end tears down a newer session, or DVM-writable data-plane state creates network authority | dvm-network-control | libs/driver-domain-host/src/lib.rs, kernel/io-manager/src/input/dvm_frames.rs, kernel/io-manager/src/io/dvm_network.rs |
| A DVM reconnect or disconnect retains old Ctrl/Alt/key/button state, a reset waits behind stale queued input, or a retired epoch injects into the next session | dvm-input-revocation | kernel/io-manager/src/input/dvm_frames.rs, kernel/io-manager/src/input/event_queue.rs, services/inputd/src/main.rs, drivers/libs/keyboard-core/src/lib.rs |
| A DVM gains a write path to the host-owned ring, L0 produces after vector setup but before a live policy consumer, producer/consumer exceed the fixed aperture, normal traffic consumes cleanup reserve, IRQ decodes or moves cursors, revoke leaves decoder/input authority live, a stale/malformed record reaches inputd, recovery reallocates a permanent MSI-X vector or leaks an MMIO mapping, or finite committed work never drains | dvm-input-ring | libs/driver-domain-protocol/src/lib.rs, libs/driver-domain-host/src/{lib.rs,ivshmem.rs}, kernel/io-manager/src/input/{dvm_ring.rs,dvm_frames.rs}, kernel/compat/src/user/syscall/linux/{input_broker_ops.rs,service_ops/poll_epoll.rs} |
| A DVM-backed scanout/input path, a compromised DVM relay, or a lost presentation/input channel is mistaken for a trusted-attention path and permits a privileged prompt | trusted-ui-boundary | kernel/io-manager/src/io/dvm_display.rs, kernel/io-manager/src/io/gui.rs, libs/rustos-user-abi/src/{device,syscall}.rs, services/uiserver/src/sys.rs |
| A generic `poll`/`epoll` caller drains the DVM ring, the MSI-X worker transfer is absent from the ownership model, a finite `STATS` reply or readiness-gated read loses/replays an event, uiserver starts the stateful inputd READ merely to discover an empty queue, waits on ring0 after inputd has moved the only record to service policy, or accumulates burst credit after a missed reader cadence | input-readiness | kernel/io-manager/src/input/event_queue.rs, kernel/compat/src/user/syscall/linux/{ipc_ops.rs,service_ops/poll_epoll.rs,service_ops/ipc_helpers.rs}, services/inputd/src/main.rs, services/uiserver/src/{input_loop.rs,sys.rs} |
| A recovering console-policy service makes uiserver wait in the input/present loop, a keyboard burst grows an unbounded queue, a queue-full event disappears without telemetry, FIFO delivery is reordered, or a blocked console call prevents local input feedback | ui-frame-budget | services/uiserver/src/{input_loop.rs,main.rs}, services/uiserver/src/app/{input.rs,runtime.rs} |
| Input and Wayland damage are split across redundant early presentations; a Wayland frame callback runs without a previous real presentation or damage-free cadence permit; missed timer pulses accumulate callback credit; or pending damage/callback work can remain live forever under the declared scheduler/timer fairness assumptions | wayland-frame-pacing | services/uiserver/src/{main.rs,wayland.rs} |
| A DVM KVM selftest keeps sending accepted relative input after its pointer has clamped at a screen edge, producing a false low-FPS result instead of sustained visual work | ui-input-motion | driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c, services/uiserver/src/{input_loop.rs,main.rs} |
| A composite DVM selftest device is selected only as a keyboard, silently loses pointer events, emits during partial scheduler admission, grants unbounded or unverified RT CPU authority, reconnects after an uncertain scheduler/RT-limit restore, lets unrelated ready poll fds starve its monotonic cadence, accumulates catch-up bursts, or turns a long motion proof into repeated keyboard/console input | dvm-input-selftest | driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c and tools/xtask/src/{build/mod.rs,kvm.rs} |
| A recovering sessiond call holds devmgrd's only receiver, starving unrelated input/device work; or a sessiond ioctl burst grows without bound, silently drops, or reorders work | devmgrd-sessiond-isolation | services/devmgrd/src/main.rs |
| A topology-only VFIO preflight, unsigned/foreign/expired release authorization, retired durable-lease schema, partial IOMMU-group binding, or mismatched DVM artifact/device policy becomes an active device assignment | vfio-release-authorization | tools/hostd/src/main.rs and libs/driver-domain-host/src/lib.rs |
| An absent, unopenable, or ioctl-incompatible IOMMUFD, insufficient QEMU memlock/pinning budget, or invalid runtime input is discovered only after VFIO binding; a plan omission detaches the L0 boot display or a connected DRM display; a reset fallback affects a PCI function outside the admitted IOMMU group; VFIO idle-D3 restores bus mastering while the device still has an identity mapping; an AMD launch lacks an exact checksummed VFCT/ATOM VBIOS snapshot; a mutable/symlinked launch artifact changes after authorization; a physical display DVM executes before its exact runtime identity is durable, launches without a complete-group reset or non-identity IOMMUFD, reports ready without authenticated control, treats a signaled/nonzero child exit as success, restores a dirty/live device, signals a PID-reused process without an exact pidfd, or enables excluded physical network/block assignment | dvm-commercial-lifecycle | tools/hostd/src/{main.rs,runtime.rs} and libs/driver-domain-host/src/lib.rs |
| A schema-8 DVM release omits or substitutes a companion config, source lock, certificate, or control contract; uses an unknown/duplicate manifest or control-contract key; is published through an unsafe or pre-existing path; changes after verification; or gains launch authority without hostd independently rechecking and snapshotting the co-located eight-file bundle | dvm-release-bundle | driver-domains/linux/scripts/{write-manifest,verify-release-artifacts,stage-release}.sh, tools/xtask/src/kvm.rs, and tools/hostd/src/runtime.rs |
| A physical AMD display DVM omits an exact host-PCI-matched checksummed VFCT image, accepts a partial/mismatched subsystem pair or malformed non-ATOM VBIOS, changes payload bytes while relocating to the fixed guest BDF, supplies an invalid/mutable/non-private QEMU ACPI table, or omits the exact AMD `1002:1900` DCN/GC/PSP/SDMA/VCN firmware; or a Blackwell profile uses the proprietary kernel flavor, mismatches NVIDIA module and GSP releases, admits an unsigned module or unbound signing certificate, loads a host-selected module name instead of the assigned PCI modalias, admits UVM/CUDA authority, starts its relay after partial KMS initialization, or ships restricted firmware without redistribution authorization | dvm-amdgpu-supply and dvm-display-driver-supply | tools/hostd/src/runtime.rs; driver-domains/linux/{sources.lock,Config.in,configs/rustos_linux_dvm_x86_64_defconfig,board/linux.fragment,scripts/verify-module-signatures.sh}; package/rustos-dvm-nvidia-open; and board/overlay/etc/init.d/S48rustos-dvm-net |
| A physical display release binds a non-AMD or replaced PCI identity; the DVM reports a different DRM driver/device; a CPU-copy path, stale/replayed sample, sub-threshold page-flip rate, or excessive page-flip/atomic-commit latency is accepted as commercial readiness | dvm-amdgpu-evidence | libs/driver-domain-host/src/lib.rs; tools/hostd/src/runtime.rs; driver-domains/linux/package/rustos-dvm-{agent,display}/src |
| Another driver domain reuses a vsock CID, IOMMU group, or PCI function; a fleet policy changes after release binding; or a signed release names a different fleet | driver-domain-fleet | tools/hostd/src/main.rs and libs/driver-domain-host/src/lib.rs |
| GUI-DVM scheduling races RustOS for ivshmem peer 0, a GUI DVM connects without the pinned RustOS peer, or either peer disconnects and a replacement reuses the stale pair | ivshmem-pairing | libs/driver-domain-host/src/ivshmem.rs and tools/xtask/src/kvm.rs |
| A GUI-DVM overwrites a host-owned writing/ready surface; concurrent host writers advance the snapshot generation; accepts an odd, forged, stale, or unacknowledged release; loses a pre-module invitation or post-ready confirmation; retains readiness after offline; leaks stale startup slots; fabricates capacity under a saturated pool; reuses stale or different-source pixels for a damage-only snapshot; regresses the displayed generation; or treats an unavailable multi-domain focus authority as valid | gui-dvm-surface and gui-dvm-pixel-authority | tools/xtask/src/kvm.rs, kernel/io-manager/src/io/{dvm_display.rs,gui/backend.rs}, kernel/compat/src/user/{sysops/device.rs,syscall/linux/device_broker_ops.rs}, services/uiserver/src/{main.rs,gpu_runtime.rs}, and driver-domains/linux/package/rustos-dvm-display/src/{rustos_dvm_ivshmem_uio.c,rustos-dvm-display.c} |
| A physical GUI-DVM grants device-write DMA authority to a source, samples before the exact RustOS producer release is materialized and server-waited, replays an acquire fd, imports an implicit/unknown layout, returns a source before GPU completion, releases an old GBM output before its replacement page-flip fence, displays a newer ready source ahead of an older generation, reuses a stale source/output generation, publishes evidence without the complete DMA-BUF/GPU/fence/atomic-KMS chain, or retains DMA authority after offline | dvm-atomic-scanout (source/model matched; physical gate failed) | `rustos_dvm_ivshmem_uio.c` validates the exact live source and exports read-only DMA-BUFs plus one-use acquire `sync_file`s; the driver-neutral runtime requires explicit one-plane linear ARGB8888 import and server-waits the acquire fence before GLES composition into a separate three-buffer GBM pool; the sealed registry currently certifies only AMD direct-DMA-BUF and virtio staged-copy. The latest physical run proved sustained real-frame DMA-BUF/GPU/fence/atomic-KMS operation, but the newly found RustOS backing-slot visual corruption and the offline revoke/recovery capture remain failed gates. |
| A GPU compositor accepts an address, raw command buffer, application shader, unbounded work, fabricated/unmeasured pipeline prime, a prime or completion from a stale context epoch, more than three live submissions, execution before its acquire fence, device-write authority to a RustOS source, CPU fallback as GPU success, or source/output reuse before its release/present fence | dvm-gpu-compositor | libs/driver-domain-protocol/src/lib.rs, services/uiserver/src/{gpu_scene.rs,gpu_runtime.rs}, kernel/io-manager/src/io/dvm_display.rs, driver-domains/linux/package/rustos-dvm-display/src, and tools/xtask/src/{build/mod.rs,kvm.rs} |
| The private AMD/virtio GPU proof measures boot-time scheduler starvation as GPU latency; gains priority equal to or above the display/input relays; runs before exact 50/100 ms bound readback; publishes evidence before exact policy/limit restoration; survives a hard-limit or restore failure; or retains realtime authority in its long-lived health loop | dvm-gpu-proof-scheduler | driver-domains/linux/package/rustos-dvm-display/src/rustos-dvm-gpu-probe.c and tools/xtask/src/{build/mod.rs,kvm.rs} |
| The display-DVM relay installs or enters realtime scheduling before host authentication, starts while admission is partial, outranks input, runs without exact continuous-CPU-bound readback, retries after uncertain policy/limit restoration, or survives a Linux hard-limit/restore failure with relay authority | dvm-display-scheduler | driver-domains/linux/package/rustos-dvm-display/src/rustos-dvm-display.c and tools/xtask/src/{build/mod.rs,kvm.rs} |
| A duplicate display relay publishes readiness; a partial, stale, or cross-mode ready file is accepted; amdgpu local health admits the staged virtio payload instead of the exact DMA-BUF/GPU/fence/atomic-KMS schema; a relay fault retains local health during scheduler restoration; a hard-limit/process exit retains readiness authority; or repeated pre-rename crashes accumulate candidate files | dvm-display-readiness | driver-domains/linux/package/rustos-dvm-display/src/rustos-dvm-display.c, driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c, and tools/xtask/src/{build/mod.rs,kvm.rs} |
| A late DVM GPU provider blocks the UI thread while allocating its atlas, promotes from a clear-only/unrepresentative or stale prime, promotes before the retained scene/first GPU frame, accepts a short or drifted provider pitch, hides a mandatory DVM path behind software success, or remains indefinitely armed after initialization/revoke | dvm-gpu-admission | services/uiserver/src/{gpu_runtime.rs,gpu_scene.rs,render.rs,sys.rs}, libs/rustos-user-abi/src/device.rs, kernel/{io-manager,ps} display-surface paths, and driver-domains/linux/package/rustos-dvm-display/src |
| A private UI frame publishes commands without its immutable atlas generation, admits an unregistered or backend/mode-mismatched GPU profile, uses an old or ambiguous prime source-mode value, submits a mode different from the authenticated prime, initializes a new DVM texture from partial/no damage, applies partial damage to a backing slot that is not the exact preceding snapshot, overlaps damage records, executes texture updates out of submission order, reuses an atlas while the DVM still has read authority, executes a QEMU frame without its staged upload, reports staged copy as zero copy, presents before the GPU fence, reuses the old front before the KMS present fence, or retains source authority across revoke/reset | dvm-gpu-atlas-transport | libs/driver-domain-protocol/src/lib.rs, services/uiserver/src/{gpu_scene.rs,gpu_runtime.rs}, kernel/io-manager/src/io/dvm_display.rs, and driver-domains/linux/package/rustos-dvm-display/src |
| Concurrent GUI-DVM install calls allocate duplicate MSI-X vectors; malformed/absent BARs retain either mapping; an MSI/provider-registration failure retains mappings; or a revoked GUI transport reopens through a fallback path | gui-dvm-install | kernel/io-manager/src/io/dvm_display.rs |
| A deadline-bounded IPC caller remains blocked after a reply, endpoint owner exit, or timeout; a late reply revives a cancelled call | ipc-reply-deadline | kernel/ipc-runtime/src/ipc/mod.rs and kernel/compat/src/user/syscall/linux/ipc_ops.rs |
| A wake between arm and commit is lost, a timer-expired task remains blocked, or a retired task is selected/woken through stale scheduler state | scheduler-wakeup | kernel/ps/src/multitask/scheduler.rs, kernel/ps/src/multitask/current.rs, and kernel/ps/src/multitask/irq.rs |
| Monotonic time is inferred from lossy RTC interrupt count, a delayed virtual clockevent extends every deadline, an unvalidated TSC becomes authoritative, or sleep reacquires the process-table lock already held by its syscall | clocksource-deadline | kernel/hal/src/arch/{acpi.rs,clock.rs,rtc.rs}, kernel/hal/src/hooks.rs, kernel/ps/src/multitask/{current.rs,scheduler.rs,irq.rs} |
| A mutable or malformed runtime launch record requests strict System weight for an ordinary app, or UI weight is granted to a path that merely resembles the trusted UI executable | scheduler-admission | services/runtimed/src/{main.rs,spawn.rs} |
| A catalog child becomes runnable before runtimed records its PID, or an activated child never receives its one-shot first turn while UI/input IPC handoffs remain busy | deferred-start, scheduler-cpu-distribution | services/runtimed/src/spawn.rs and kernel/ps/src/multitask/scheduler.rs |
| A System caller waits on a User broker or nested User policy server without reply-scoped donation; a critical DVM/UI flood exceeds its two-dispatch System bound while User work is ready; a completed/cancelled/exited reply leaks an inherited System class; or a foreign/malformed netd response creates latency authority | ipc-priority-inheritance, scheduler-cpu-distribution | kernel/ps/src/multitask/{scheduler.rs,current.rs}, kernel/compat/src/user/syscall/linux/ipc_ops.rs |
| Opaque IPC descriptors remain in the pending registry after queue cancellation, peer-close, invalid receiver output, or caller exit; one batch is partially installed | ipc-handle-transfer | kernel/ps/src/user/handles.rs, kernel/ipc-runtime/src/ipc/mod.rs, kernel/compat/src/user/syscall/linux/ipc_ops.rs, and kernel/ps/src/multitask/current.rs |
| A foreign process receives a process-owned endpoint, completes a guessed reply capability, installs attached handles, prevents worker-thread service, leaves authority after owner-process exit, or makes `dup2`/`F_DUPFD` sparsely expand a ring-0 descriptor table | ipc-endpoint-ownership | kernel/ipc-runtime/src/ipc/mod.rs, kernel/compat/src/user/syscall/linux/ipc_ops.rs, kernel/ps/src/multitask/current.rs, kernel/ps/src/user/handles/table.rs, and kernel/compat/src/user/syscall/linux/service_ops/vfs_socket.rs |
| A stale or foreign loader process maps/commits a prepare handle, a rejected commit retains mappings, or loader exit leaks uncommitted broker state | proc-broker-session | kernel/compat/src/user/syscall/linux/proc_broker_ops.rs and services/loaderd/src/main.rs |
| Wrong PID/TID cancellation or exec consumes another target's ticket; target-thread exit or exec sibling retirement retains ticket/register handoff state; an image becomes schedulable before its register handoff exists | exec-ticket | services/procd/src/main.rs, services/loaderd/src/main.rs, kernel/compat/src/user/syscall/linux/proc_broker_ops.rs, kernel/compat/src/user/syscall/linux.rs, and kernel/compat/src/user/syscall/linux/support.rs |
| Rootd or storaged accepts a retired private request envelope, interprets a truncated request as a valid operation, silently ignores fields not consumed by the selected storage operation, or leaves a synchronous caller blocked by dropping a malformed-size request without a reply | commercial-service-envelope | services/rootd/src/main.rs, services/storaged/src/main.rs, and libs/rustos-user-abi/src/syscall.rs |

## Release-blocking proof gaps

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
health context. uiserver now grants one non-accumulating 16 ms cadence permit,
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
bounded 100 Hz/40-second producer, and steady-state uiserver submission never
waits for a GPU slot on the UI thread. The next exact 30-second packaged
capture passed independent GPU proof, RustOS rendering, DVM relay
performance/fences, and display lifecycle with no compositor revoke or offline
marker. Its sole failed predicate was input: 92--102 accepted events/s, zero
drop/slow/error/backlog, and at most 35 ms reader age still contained 61--153
ms inter-event gaps. The dedicated uiserver reader used a non-accumulating
4 ms direct-read cadence to bypass the ring0/service-queue lost-wake race; the
later physical stall described below replaces that unsafe empty-queue probe
with the bounded STATS readiness gate. The agent now grants only an authenticated live input stream SCHED_RR
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
The latest compositor change removed the retired cursor-only early-present
lane, coalesces input and Wayland damage into one output turn, and consumes one
permit from the previous real presentation before the next present. This
matches the Wayland requirement to give a callback-driven client time to draw
for the next refresh without creating an unpaced callback loop. Three bounded
captures reached 35--43 FPS with balanced commit/callback/release counts;
uiserver callback wait fell from 25--30 ms to normally 1--4 ms. The rebuilt
schema-8 artifact's final signed 30-second capture separated a 4.961-second
startup window at 0.403 commit FPS and 0.201 callback FPS from 20 settled
one-second windows at 33.348--45.705 FPS. The largest settled callback gap was
83 ms and the largest redraw was 24 ms. Compositor callback wait was normally
1--5 ms and instrumented full-frame `wl_shm` copies averaged 0.488--2.074 ms,
so application drawing, ordinary compositor copying, and the GPU are not the
remaining steady-state limiter. The private DVM GPU proof completed 120 frames
at 128.705 FPS with 7.769 ms average and 11.075 ms maximum GPU time, while the
relay retained GPU composition, explicit fences, three scanout buffers, and no
provider revoke, context loss, compositor-offline, or `display not available`
marker. The exact 55-FPS command still failed. The gate requires three
consecutive balanced WayClick windows at 55 FPS with at most a 50 ms callback
gap and does not combine disjoint compositor, client, or relay windows.

Source inspection isolates the remaining OS userspace event/readiness gap.
uiserver dispatches Wayland clients nonblocking, but its idle wait listens only
for input and a deadline capped by the 16 ms runtime cadence; it does not include
the Wayland backend aggregate poll fd. The Linux compatibility `epoll_wait`
entry point currently ignores its timeout argument and performs one vfsd
readiness query. The existing event-driven netd wait covers only one indefinite
socket poll, while a Wayland display requires an aggregate wait over multiple
client fds. Closing that gap correctly requires a general capability-bound
cross-provider wait-set/readiness ABI with timeout and lost-wake semantics, fd
lifetime across dup/fork/exec, peer close, descriptor and credential transfer,
bounded data rings, and revoke recovery. That scope is the failed next-ABI gate,
not permission for a WayClick-specific shortcut.

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
in those windows and its Wayland callback waits were normally 2--4 ms. The
remaining deterministic frame loss is the userspace readiness boundary: after
a callback, the client commit cannot wake uiserver and waits for its next 16 ms
poll. Closing it requires the separately scoped cross-provider wait-set ABI;
the result is not accepted as 55/60 FPS success.
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
`uiserver` now records each slot's retained content epoch and permits partial
damage only for the exact predecessor; an uninitialized or older slot receives
a complete atlas snapshot. This source fix has unit, workspace-check, RustOS
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
