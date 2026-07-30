# Commercial Quality Gates

This contract is the non-negotiable definition of done for an enabled RustOS
product topology. It applies to kernel, service, DVM, boot, and tool-owned
paths alike. "Early stage", prototype status, compatibility, or a fallback
cannot waive a gate.

The source-writing rules and product boundary are defined in
`core-engineering-contract.md`; an implementation that violates that contract
cannot satisfy this gate merely by passing a runtime smoke test.

## Required evidence

1. **Ownership and authority** — every state transition, device aperture,
   service endpoint, and recovery action has one named owner. Authority is
   least-privilege, capability-bound, authenticated across domains, and revoked
   atomically on exit or lease loss.
2. **Lifecycle and recovery** — startup, readiness, failure, restart, and
   teardown are explicit state machines with bounded waits, idempotent actions,
   stale-event rejection, restart budgets, and diagnostic milestones. A missing
   provider fails closed; it cannot silently select a weaker path.
3. **Isolation and input integrity** — a compromised DVM or service cannot
   forge another domain's memory mapping, control record, focus, input, or
   authority. DMA/IOMMU, capability, and mapping assumptions are stated and
   checked by the owning layer.
4. **Real-time behavior** — runnable work has a bounded scheduling and queueing
   contract. Priority inheritance, admission/budget rules, IRQ handoff, and
   back-pressure prevent a non-critical worker from starving boot, UI, or
   recovery work. Latency and frame-rate acceptance thresholds are measured,
   not inferred.
5. **Protocol and memory safety** — all external records are fixed or bounded,
   versioned, length-checked, replay/staleness-checked, and reject unknown
   authority. Shared-memory ownership, cache/order fences, and release rules are
   explicit; no reader relies on an unbounded retry or polling fallback.
   Long-lived services reclaim dropped allocations, coalesce adjacent spans,
   grow only after reusable capacity is exhausted, and expose allocation
   failure as a product-health fault. Cumulative traffic may not become
   cumulative resident memory.
6. **Formal and source conformance** — the state machine has safety invariants,
   progress properties, and adversarial transitions in `formal/`. Source tests
   prove encoding/bounds/authorization correspondence, and runtime tests cover
   normal, denial, crash, and restart paths. A model alone is not implementation
   evidence.
7. **Retirement discipline** — when a primary path replaces a legacy one, the
   old source, package selection, test expectations, formal model, and docs are
   removed in the same completion slice after a scoped reference search. Active
   code is never deleted before its replacement has passed the gates above.

An unmet mandatory item blocks release and remains an implementation task with
an owner and a verification command. It must not be relabeled as a known issue,
future enhancement, or acceptable transitional limitation.

## Risk-ordered acceptance scope

Run and report these lanes in order. A lower lane cannot compensate for an
open higher lane, and a model name is not a pass unless its mapped source and
runtime evidence also pass.

| Priority | Release surface | Required implementation property | Canonical evidence |
| --- | --- | --- | --- |
| P0.1 | Linux ELF / Windows PE64 launch | `loaderd` owns raw parsing; the shared admission gate rejects overflow, out-of-window and overlapping regions, W+X, and non-executable main entries before any broker map. ELF/PE parser bytes, relocations, imports, and file-mutation behavior need adversarial source tests, not only a plan model. | `dual-abi-image-admission`, pinned Kani `rustos-image-admission` compositional proofs, `fuzz-host --target image-admission`, Linux and PE launch smokes |
| P0.2 | Identity, capability, and namespace | Every endpoint, handle, ticket, broker call, and cross-domain request binds an L0/kernel-stamped subject plus exact destination and operation. A path, CID, DVM field, or service name cannot manufacture authority. | endpoint/publication/ownership, handle-transfer, proc/exec, DVM-control models plus denial/revoke/restart tests |
| P0.3 | User memory, service heaps, and page tables | Every user-copy and map operation proves canonical range, page rounding, access direction, non-overlap, backing lifetime, and teardown. A rejected `MAP_FIXED` request validates flags and backing before it may remove an existing VMA; multi-page protect/unmap validates the complete span and reserves ownership/region ledgers before changing the first PTE. Kernel mappings never follow guest pointers. Every long-lived service allocator returns dropped spans to reusable capacity, preserves alignment and exact ownership, installs bootstrap capacity once, rejects duplicate release without free-list mutation, and releases its spin lock before a blocking grow request; mmap hint exhaustion wraps to reusable holes. | `service-heap-lifecycle`, page-table and process-address-space models; allocator reuse/coalescing/alignment/growth-lock tests; mmap plan, protection, and unmap preflight tests; source tests for `kernel-mm`/user-copy/brokers |
| P0.4 | Bounded lifecycle and IPC | Startup, readiness, reply, timeout, cancellation, crash, restart, and teardown converge on one terminal owner state. No core service or policy call can wait indefinitely or retain a stale capability. A service holds no local policy/state lock across discovery or synchronous cross-service IPC; an empty maintenance drain performs no discovery, and every bootstrap dependency is in the declared readiness order or has a bounded explicit handshake. Cross-provider waits bind service-owned readiness generations, exact endpoint epochs, and open-description lifetime through check-register-recheck-arm-presence-check; provider IPC stays within both a 16 ms service cap and the remaining application deadline. | rootd/endpoint/IPC deadline/wakeup/userspace-wait-set models, fault injection, source conformance, 30-second KVM gates |
| P0.5 | DVM memory and device authority | Host-created apertures, IOMMU groups, MSI-X meanings, control secrets, epochs, reset order, runtime process identity, and revocation are exact and disjoint. A physical-device child requires a durable signed lease, non-identity IOMMUFD, authenticated readiness, and bounded post-stop reset. Direct display scanout grants device-read but never device-write DMA authority. | DVM fleet/control/ring/pixel/scanout/commercial-lifecycle models, `verify-dvm`, signed VFIO release tests, KVM transport exercises, target IOMMU fault/reset/revoke captures |
| P1.1 | Scheduler and queue overload | Critical work has explicit admission, priority inheritance, bounded turns and queues, measurable wait/frame thresholds, and a guaranteed recovery/User share under flood. IRQ leaves policy and unbounded work to schedulable context. | scheduler admission/demotion/wakeup/IPC PI models, `kernel-ps` tests, UI profile and stall markers |
| P1.2 | Storage and filesystem mutation | Boot substrate is descriptor/extent bounded; namespace, mount, metadata, and post-bootstrap storage policy stay in services. Power loss, partial write, media removal, and replay have explicit terminal results. | boot-volume model, manifest fuzz, storage fault tests; filesystem-content/crash-consistency model remains mandatory |
| P1.3 | Network and message payloads | DVM Ethernet is only a bounded authenticated transport; `netd` owns socket policy, queue limits, cancellation, and peer namespaces. Payload length/checksum/fragment adversaries cannot escape their session. | DVM network models and KVM exercise; packet-payload and socket-backpressure models remain mandatory |
| P1.4 | Display and input integrity | Only authenticated DVM ingress reaches inputd; UI policy never runs in IRQ context; scanout completion is a fence, not an ioctl return; queues coalesce only lossy motion and preserve edges. The MSI-X worker owns input transport progress, inputd publishes policy-queue readiness generations, and the common wait set performs an atomic provider recheck before sleep; uiserver's dedicated reader retains its bounded, non-consuming STATS bridge until equivalent runtime wait-set evidence exists. GPU composition admits only bounded OS-owned contexts, fixed commands, read-only source capabilities, explicit acquire/completion/release fences, and epoch-wide hard-timeout/revoke. The 16.667 ms frame target remains a strict performance gate, while a distinct 50 ms hard timeout prevents one scheduling-jitter miss from withdrawing the last valid front buffer or fabricating device loss. Built-in shader/pipeline priming is a separately fenced setup phase bounded to 500 ms, exercises a full provider-stride atlas upload plus the fixed textured draw and atomic present, and keeps frame admission closed until it succeeds; late atlas allocation runs off the UI thread while the current CPU-presented surface remains live, provider pitch is preserved exactly, and a separate first-frame deadline gates promotion. In the absence of an exported vblank clock, one non-accumulating 15 ms cadence permit phase-locks DVM presentation with Wayland callback work without CPU fallback or post-stall bursts. Raw commands, malformed layers, application shaders, software rendering, CPU fallback, and clear-only priming cannot count as GPU success. | input/readiness/wait-set/display/scanout/frame-budget/GPU-compositor/admission models, fixed-contract source tests, representative bounded-prime virgl execution proof, and 60 FPS DVM profile gate with matched GPU/present-fence counts |
| P2 | Capacity, update, and operability | Boot time, steady-state CPU/memory/IO pressure, storage growth, update rollback, telemetry loss, and recovery budgets have published thresholds with reproducible collection commands. A KVM run fails immediately on allocator failure or a fatal core-service readiness cascade; a last log line is not accepted as proof of cause. | bounded performance captures, pressure/latency counters, allocator-failure oracle, upgrade/rollback and long-duration soak gates |

Current known release blockers must stay visible in `formal/COVERAGE.md` and
`formal/CONFORMANCE.md`. Dedicated finite abstractions now exist for ELF/PE
byte admission, page-table lifecycle, DMA-domain isolation, boot-file content,
DVM Ethernet payloads, and bounded System-to-User dispatch. Pinned Kani now
proves the compositional byte decoder, segment/section, entry, single-relocation,
and single-import contracts, but not arbitrary-length table equivalence. These
do not replace runtime fault evidence. Multi-block native loader corpora, target page-table/TLB checks,
non-identity VT-d/IOMMU fault-and-revoke captures, corrupted-media recovery,
network saturation/cancellation/backpressure plus physical-NIC captures, and
multicore CPU-time measurements remain failed release gates until their
evidence artifacts pass. The composite 30-second KVM GUI/input/netprobe gate is
currently failed: the standard Linux 6.12 virtio-gpu cannot import the foreign
SG-table DMA-BUF used by the physical zero-copy source path, while QEMU's
legacy VMware SVGA device cannot bind current vmwgfx. The enabled AMD profile
can close that runtime gate only with the physical amdgpu assignment; a
CPU-copy validation fallback is forbidden. The current Blackwell target uses the exact
580.173.02 open-module/GSP pair and requires kernel-enforced signatures bound
with their certificate and enforcement configuration to artifact-manifest
schema 9. The manifest and seven named payload files must be admitted as one
self-contained, immutable, safely staged release directory. Its
non-redistributable firmware license, target
connector, DMA fault, reset, and 60 FPS page-flip captures are separate failed
release gates until evidence is filed.

The pre-public-ABI GPU-composition gate is narrower and independently
observable. RustOS already compiles bounded scene layers into the private fixed
contract, and the Linux DVM executes the identical command vocabulary through
virgl or AMDGPU GLES with explicit fences, pixel verification, a frame hash,
bounded one-time pipeline-prime latency, and measured steady-state completion
latency. The KVM proof must explicitly report that no
public ABI, live RustOS UI connection, or scanout handoff exists. Therefore it
cannot close the still-failed private submit transport, AppState layer adapter,
GPU-output KMS handoff, foreign DMA-BUF zero-copy, application 3D ABI, or
physical AMD capture gates.

For the enabled AMD `1002:1900` slice, source admission now additionally
requires the signed schema-3 `amdgpu` identity and five authenticated fresh
evidence-v2 samples proving read-only DMA-BUF source import, GPU composition,
explicit fence, a separate three-buffer atomic-KMS output pool, zero relay CPU
copy, no staged damage upload, at least the 59,000 mHz measurement floor around
a nominal 60 Hz mode, at most 25 ms commit-to-page-flip latency, and at most 2
ms nonblocking atomic-commit time.
This instrumentation and its finite model do not close the physical gate while
the only available AMD function remains the active L0 boot display.

The current implementation slice intentionally excludes trusted UI/multi-DVM
focus authority and physical network DVM assignment. Physical block-DVM
assignment is source-enabled behind signed schema-4 policy, an L0-signed
transport epoch, exact-process supervision, VFIO/IOMMU admission, and ordered
revoke/reset/restore. It remains a failed hardware acceptance gate until target
NVMe and AHCI fault/reset/revoke captures pass; virtual transport evidence does
not substitute for those captures.

## Architecture baselines

These are acceptance baselines, not branding claims or a substitute for direct
evidence.

- **Qubes-style domain containment:** a device/GUI DVM receives only its
  assigned aperture and explicit relay authority. Cross-domain requests are
  authenticated, policy-admitted, and bound to the source/target domain; GUI
  input and foreign memory mapping never derive authority from DVM-provided
  identifiers alone. See the [Qubes architecture](https://doc.qubes-os.org/en/latest/developer/system/architecture.html), [qrexec internals](https://doc.qubes-os.org/en/latest/developer/services/qrexec-internals.html), and [GUI virtualization boundary](https://doc.qubes-os.org/en/latest/developer/system/gui.html).
- **QNX-style overload containment:** realtime admission is an explicit
  authority decision with bounded queues and measurable service behavior. A
  mutable launch record, untrusted workload, or inherited placement cannot
  self-promote into a critical budget. See QNX's
  [adaptive-partitioning security guidance](https://china.qnx.com/developers/docs/7.1/com.qnx.doc.security.system/topic/manual/adaptive_partitioning.html).
- **seL4-style capability evidence:** object access must be reducible to a
  finite capability/owner graph, and the claim must name its configuration and
  proof scope. A model or a checked configuration never proves excluded boot,
  IOMMU, debug, or hardware assumptions. See seL4
  [capabilities](https://docs.sel4.systems/Tutorials/capabilities.html),
  [capDL](https://docs.sel4.systems/projects/capdl/index.html), and
  [verified-configuration scope](https://docs.sel4.systems/projects/sel4/verified-configurations.html).
