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
| A DVM forges an ivshmem counter, malformed receive slot, or post-install header and makes RustOS advance a cursor, exceed a fixed ring bound, or deliver the frame to network policy | dvm-network-ring | libs/driver-domain-protocol/src/lib.rs, kernel/io-manager/src/io/dvm_network.rs, driver-domains/linux/package/rustos-dvm-net/src/rustos-dvm-net.c |
| A mapped DVM Ethernet aperture remains usable after its authenticated control session ends, a stale end tears down a newer session, or DVM-writable data-plane state creates network authority | dvm-network-control | libs/driver-domain-host/src/lib.rs, kernel/io-manager/src/input/dvm_frames.rs, kernel/io-manager/src/io/dvm_network.rs |
| A DVM reconnect or disconnect retains old Ctrl/Alt/key/button state, a reset waits behind stale queued input, or a retired epoch injects into the next session | dvm-input-revocation | kernel/io-manager/src/input/dvm_frames.rs, kernel/io-manager/src/input/event_queue.rs, services/inputd/src/main.rs, drivers/libs/keyboard-core/src/lib.rs |
| A DVM gains a write path to the host-owned ring, L0 produces after vector setup but before a live policy consumer, producer/consumer exceed the fixed aperture, normal traffic consumes cleanup reserve, IRQ decodes or moves cursors, revoke leaves decoder/input authority live, a stale/malformed record reaches inputd, recovery reallocates a permanent MSI-X vector or leaks an MMIO mapping, or finite committed work never drains | dvm-input-ring | libs/driver-domain-protocol/src/lib.rs, libs/driver-domain-host/src/{lib.rs,ivshmem.rs}, kernel/io-manager/src/input/{dvm_ring.rs,dvm_frames.rs}, kernel/compat/src/user/syscall/linux/{input_broker_ops.rs,service_ops/poll_epoll.rs} |
| A DVM-backed scanout/input path, a compromised DVM relay, or a lost presentation/input channel is mistaken for a trusted-attention path and permits a privileged prompt | trusted-ui-boundary | kernel/io-manager/src/io/dvm_display.rs, kernel/io-manager/src/io/gui.rs, libs/rustos-user-abi/src/{device,syscall}.rs, services/uiserver/src/sys.rs |
| A generic `poll`/`epoll` caller drains the DVM ring, the MSI-X worker transfer is absent from the ownership model, a finite `STATS` reply or authorized direct read loses/replays an event, uiserver waits on ring0 after inputd has moved the only record to service policy, or a missed reader cadence accumulates burst credit | input-readiness | kernel/io-manager/src/input/event_queue.rs, kernel/compat/src/user/syscall/linux/{ipc_ops.rs,service_ops/poll_epoll.rs,service_ops/ipc_helpers.rs}, services/inputd/src/main.rs, services/uiserver/src/input_loop.rs |
| A recovering console-policy service makes uiserver wait in the input/present loop, a keyboard burst grows an unbounded queue, a queue-full event disappears without telemetry, FIFO delivery is reordered, or a blocked console call prevents local input feedback | ui-frame-budget | services/uiserver/src/{input_loop.rs,main.rs}, services/uiserver/src/app/{input.rs,runtime.rs} |
| A DVM KVM selftest keeps sending accepted relative input after its pointer has clamped at a screen edge, producing a false low-FPS result instead of sustained visual work | ui-input-motion | driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c, services/uiserver/src/{input_loop.rs,main.rs} |
| A composite DVM selftest device is selected only as a keyboard, silently loses pointer events, emits before bounded guest scheduler admission, grants unbounded RT CPU authority, lets unrelated ready poll fds starve its monotonic cadence, accumulates catch-up bursts, or turns a long motion proof into repeated keyboard/console input | dvm-input-selftest | driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c and tools/xtask/src/kvm.rs |
| A recovering sessiond call holds devmgrd's only receiver, starving unrelated input/device work; or a sessiond ioctl burst grows without bound, silently drops, or reorders work | devmgrd-sessiond-isolation | services/devmgrd/src/main.rs |
| A topology-only VFIO preflight, unsigned/foreign/expired release authorization, retired durable-lease schema, partial IOMMU-group binding, or mismatched DVM artifact/device policy becomes an active device assignment | vfio-release-authorization | tools/hostd/src/main.rs and libs/driver-domain-host/src/lib.rs |
| An absent, unopenable, or ioctl-incompatible IOMMUFD or invalid runtime input is discovered only after VFIO binding; a plan omission detaches the L0 boot display or a connected DRM display; a mutable/symlinked launch artifact changes after authorization; a physical display DVM executes before its exact runtime identity is durable, launches without a complete-group reset or non-identity IOMMUFD, reports ready without authenticated control, treats a signaled/nonzero child exit as success, restores a dirty/live device, signals a PID-reused process without an exact pidfd, or enables excluded physical network/block assignment | dvm-commercial-lifecycle | tools/hostd/src/{main.rs,runtime.rs} and libs/driver-domain-host/src/lib.rs |
| A schema-8 DVM release omits or substitutes a companion config, source lock, certificate, or control contract; uses an unknown/duplicate manifest or control-contract key; is published through an unsafe or pre-existing path; changes after verification; or gains launch authority without hostd independently rechecking and snapshotting the co-located eight-file bundle | dvm-release-bundle | driver-domains/linux/scripts/{write-manifest,verify-release-artifacts,stage-release}.sh, tools/xtask/src/kvm.rs, and tools/hostd/src/runtime.rs |
| A physical display DVM omits the exact AMD `1002:1900` GC/PSP/SDMA/VCN firmware; or a Blackwell profile uses the proprietary kernel flavor, mismatches NVIDIA module and GSP releases, admits an unsigned module or unbound signing certificate, loads a host-selected module name instead of the assigned PCI modalias, admits UVM/CUDA authority, starts its relay after partial KMS initialization, or ships restricted firmware without redistribution authorization | dvm-amdgpu-supply and dvm-display-driver-supply | driver-domains/linux/{sources.lock,Config.in,configs/rustos_linux_dvm_x86_64_defconfig,board/linux.fragment,scripts/verify-module-signatures.sh}, package/rustos-dvm-nvidia-open, and board/overlay/etc/init.d/S48rustos-dvm-net |
| A physical display release binds a non-AMD or replaced PCI identity; the DVM reports a different DRM driver/device; a CPU-copy path, stale/replayed sample, sub-threshold page-flip rate, or excessive page-flip/atomic-commit latency is accepted as commercial readiness | dvm-amdgpu-evidence | libs/driver-domain-host/src/lib.rs; tools/hostd/src/runtime.rs; driver-domains/linux/package/rustos-dvm-{agent,display}/src |
| Another driver domain reuses a vsock CID, IOMMU group, or PCI function; a fleet policy changes after release binding; or a signed release names a different fleet | driver-domain-fleet | tools/hostd/src/main.rs and libs/driver-domain-host/src/lib.rs |
| GUI-DVM scheduling races RustOS for ivshmem peer 0, a GUI DVM connects without the pinned RustOS peer, or either peer disconnects and a replacement reuses the stale pair | ivshmem-pairing | libs/driver-domain-host/src/ivshmem.rs and tools/xtask/src/kvm.rs |
| A GUI-DVM overwrites a host-owned writing/ready surface; concurrent host writers advance the snapshot generation; accepts an odd, forged, stale, or unacknowledged release; loses a pre-module invitation or post-ready confirmation; retains readiness after offline; leaks stale startup slots; fabricates capacity under a saturated pool; reuses stale or different-source pixels for a damage-only snapshot; regresses the displayed generation; or treats an unavailable multi-domain focus authority as valid | gui-dvm-surface and gui-dvm-pixel-authority | tools/xtask/src/kvm.rs, kernel/io-manager/src/io/{dvm_display.rs,gui/backend.rs}, kernel/compat/src/user/{sysops/device.rs,syscall/linux/device_broker_ops.rs}, services/uiserver/src/main.rs, and driver-domains/linux/package/rustos-dvm-display/src/{rustos_dvm_ivshmem_uio.c,rustos-dvm-display.c} |
| A GUI-DVM grants device-write DMA authority, returns the current direct-scanout slot, releases the old front before its replacement page-flip fence, reuses a stale generation, or retains DMA authority after offline | dvm-atomic-scanout | driver-domains/linux/package/rustos-dvm-display/src/{rustos-dvm-display.c,rustos_dvm_ivshmem_uio.c} |
| A GPU compositor accepts an address, raw command buffer, application shader, unbounded work, fabricated/unmeasured pipeline prime, a prime or completion from a stale context epoch, more than three live submissions, execution before its acquire fence, device-write authority to a RustOS source, CPU fallback as GPU success, or source/output reuse before its release/present fence | dvm-gpu-compositor | libs/driver-domain-protocol/src/lib.rs, services/uiserver/src/{gpu_scene.rs,gpu_runtime.rs}, kernel/io-manager/src/io/dvm_display.rs, driver-domains/linux/package/rustos-dvm-display/src, and tools/xtask/src/{build/mod.rs,kvm.rs} |
| The display-DVM relay enters realtime scheduling before host authentication, outranks input, runs without a continuous-CPU ceiling, or retains realtime policy after stop/hard-limit | dvm-display-scheduler | driver-domains/linux/package/rustos-dvm-display/src/rustos-dvm-display.c and tools/xtask/src/kvm.rs |
| A late DVM GPU provider blocks the UI thread while allocating its atlas, promotes from a clear-only/unrepresentative or stale prime, promotes before the retained scene/first GPU frame, accepts a short or drifted provider pitch, hides a mandatory DVM path behind software success, or remains indefinitely armed after initialization/revoke | dvm-gpu-admission | services/uiserver/src/{gpu_runtime.rs,gpu_scene.rs,render.rs,sys.rs}, libs/rustos-user-abi/src/device.rs, kernel/{io-manager,ps} display-surface paths, and driver-domains/linux/package/rustos-dvm-display/src |
| A private UI frame publishes commands without its immutable atlas generation, initializes a new DVM texture from partial/no damage, overlaps damage records, executes texture updates out of submission order, reuses an atlas while the DVM still has read authority, executes a QEMU frame without its staged upload, reports staged copy as zero copy, presents before the GPU fence, reuses the old front before the KMS present fence, or retains source authority across revoke/reset | dvm-gpu-atlas-transport | libs/driver-domain-protocol/src/lib.rs, services/uiserver/src/{gpu_scene.rs,gpu_runtime.rs}, kernel/io-manager/src/io/dvm_display.rs, and driver-domains/linux/package/rustos-dvm-display/src |
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
the physical DMA-BUF/direct-scanout gate. A bounded QEMU capture reached the
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
ms inter-event gaps. The dedicated uiserver reader now bypasses the
ring0/service-queue lost-wake race with a non-accumulating 4 ms direct-read
cadence. The agent now grants only an authenticated live input stream SCHED_RR
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
The separate standard Wayland client proof remains failed: restoring WayClick's
normal blocking event dispatch and eliminating redundant/fixed-size netd IPC
raised the observed frame loop from about 1 FPS to 8.8--14.3 FPS in the final
30-second capture, while uiserver rendered at 53.7--60.8 FPS and the DVM relay
remained ready. WayClick's own maximum redraw work was 10--38 ms, but callback
gaps were 98--165 ms. The gate requires three
consecutive balanced WayClick commit/frame-callback/buffer-release windows at
55 FPS with at most a 50 ms callback gap; it does not infer client success from
compositor or relay throughput. Per-call synchronous AF_UNIX service transport
is still the measured bottleneck. A general shared userspace socket data plane
is intentionally outside the current private compositor ABI, so this is a
failed acceptance gate rather than a client-specific shortcut.
The scope remains private (`scope-public-abi=0`): an application 3D ABI,
physical read-only DMA-BUF import, zero-copy AMD scanout, and physical VFIO
fault/reset/revoke evidence remain failed gates.
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
complete IOMMU groups. Its requested AMD `1002:1900` function is the L0
`boot_vga` device with a connected eDP connector, so the live assignment gate
correctly rejects it before any driver mutation. No physical capture was
fabricated by detaching the active host display. NVIDIA GSP firmware also
remains non-redistributable until a product redistribution authorization is
recorded. The separate RustOS
native boot-device DMA backend remains identity-only. Therefore the DMA
hardware gate stays explicitly failed even though both finite abstractions pass
TLC.
