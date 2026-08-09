# AI Contract — End-to-End System Flows

This is the cross-subsystem contract index. Detailed wire layouts and owner
rules stay in `contracts-abi.md`; build and proof infrastructure stays in
`contracts-infra.md`. The executable source of truth for the flows below is
`formal/system-flows.tsv`, checked by `formal/check-system-flows.sh`.

## Zero-trust service request composition

Every published endpoint follows one end-to-end chain: rootd admits endpoint
discovery, the kernel stamps the immediate sender, the receiving service
validates exact wire shape plus direct identity or a live service-owner
delegation, the subsystem owner rechecks object rights and generation, and the
caller admits only an exact request-bound response. A check performed by an
earlier hop is evidence, never authority for a later hop. The executable flow
is `zero-trust-e2e`; the per-subsystem inventory is
`formal/trust-boundaries.tsv`.
Identity-blind receive wrappers are intentionally absent from
`rustos-svc-runtime`; `formal/check-zero-trust-ingress.sh` rejects direct raw
receive use by any service and requires every published endpoint source to be
registered with explicit identity, delegation, object, and response policy.
`formal/zero-trust-subsystems.tsv` extends the same gate to boot/image parsers,
user memory, all DVM shared-memory consumers, local service sockets, and network
frames. The checker discovers new `dvm_*.rs` and service listener sources and
fails until shape, authority, revoke/generation, model, and source witnesses are
declared.

## Why this layer exists

Local contracts do not prove that their composition is safe. A correct
exception decoder can still enter an ordinary Rust call on a misaligned stack;
a correct endpoint registry can still be revoked at the wrong thread lifetime;
a correct readiness counter can still lose a wake between query and scheduler
arm; a durable mutation can still be exposed before recovery state is complete.
Every high-risk flow therefore names:

- a stable requirement and hazard ID;
- the exact owner of each transition;
- pre-state, event, and post-state;
- every success, error, timeout, cancel, revoke, and exit terminal that exists;
- the maximum blocking interval;
- one registered formal model, source anchor, and exact source witness.

The checker rejects duplicate IDs, missing graph exits, unbounded timeout
transitions, unregistered models, absent source/witness mappings, and any
reintroduced direct `.ko` route. `formal/contracts.toml` also names every
audited critical/high source surface, while `formal/model-bindings.tsv` binds
supporting refinement and evidence models to a production whole flow. An
orphan model or a risk surface that only names a model without an exact
transition/source mapping fails the registry. Linux driver modules remain
inside the DVM; they are not an exception to RustOS kernel/service lifecycle
contracts.

## Global composition invariants

1. **One owner and one linearization point.** Each object mutation has one
   service or kernel-substrate owner. A caller may propose an identity but may
   not publish, revoke, or reuse it outside the owner transition.
2. **Process objects outlive worker threads.** Endpoints, channels, open
   descriptions, and service epochs are process- or service-owned. Non-final
   thread retirement removes only task-local wait/reply authority. Final
   process retirement revokes process authority exactly once.
3. **Every control wait is bounded.** Raw and handle-carrying RustOS IPC calls
   share the finite 30-second service ceiling. Provider readiness calls are
   further capped at 16 ms and the remaining application deadline. Timeout
   cancels the exact reply identity; a late reply cannot revive it.
4. **Check, register, recheck, arm, presence-check.** A wait may commit sleep
   only while its exact waiter identity is still installed and every observed
   service generation/endpoint epoch remains current.
5. **Restart changes identity.** Endpoint epoch and provider generation advance
   across revoke/republication. Retained checkpoint state must be replayed and
   validated before the replacement endpoint becomes discoverable.
6. **Reference lifetime is descriptor-exact.** `dup` and `fork` retain the same
   open description; `exec` removes only close-on-exec descriptors; only the
   final close publishes provider teardown and wait-set purge.
7. **No Rust exception cleanup before ABI repair.** Every general exception
   crosses the explicit stack-alignment bridge before any nested ordinary Rust
   call. A recoverable user fault preserves authority; fatal non-final and
   final-thread paths have distinct cleanup.
8. **Failure is an observable terminal, not fabricated success.** Malformed
   envelopes, stale epochs, exhausted bounds, incomplete replay, and uncertain
   teardown return a direct error or withdraw authority.
9. **A scheduler block has one exact epoch.** Only a current, runnable,
   non-retired task may arm. Wake clears the arm before commit; commit refuses
   a raced wake; cancel requires a live arm; retirement removes all runnable,
   donation, and scheduler-owned wait authority. A retired user slot remains
   unreapable until executive housekeeping removes its exact task identity
   from futex, wait-set, endpoint-discovery, input, block, and exec-transition
   registries and acknowledges that cleanup back to the scheduler.
   Scheduler-aware locks use this same arm-before-publication transition and
   remove a waiter after any unrelated wake. Housekeeping completes no more
   than four retirement records and one deferred transfer release per turn;
   provider replay and acknowledgement are separate, 100 ms-bounded
   housekeeping turns rather than a synchronous foreground backlog drain.
   IPC object handles additionally bind a slot generation. Endpoint teardown
   removes the exact generation before scanning its message slots; message and
   reply transitions follow the runtime-enforced
   `endpoint -> message -> reply` lock-class order.
10. **All elapsed-time decisions share one validated monotonic domain.**
    Calendar RTC state never owns timeouts. A delayed clockevent catches every
    absolute deadline at or before the current source time, and cancel, wake,
    or owner exit removes the exact timer owner.
11. **Finite kernel objects are charged to the initiating owner.** Endpoint,
    shared-backing, and task admission reserve owner quota before publication.
    Endpoint/task exit returns quota at terminal removal; a dropped shared
    region remains charged while its physical backing is deferred and returns
    quota only after reclaim. Kernel/bootstrap reserves are not consumable by
    an ordinary process.
11. **Usercopy retains one process generation through validation and copy.**
    Copyin/copyout rejects kernel, noncanonical, wrapping, unmapped, and
    wrong-permission spans before dereference. Exec and exit serialize against
    the retained address-space state; a partial stale-generation copy is not a
    valid outcome.
12. **Physical frames and kernel mappings have disjoint authority.** Boot-owned
    and allocator-metadata frames never enter the free set. Allocation consumes
    one exact free frame/run, only an allocated frame may be released, kernel
    image mappings are W xor X, and MMIO/direct-map permission changes require
    one aligned nonwrapping range inside the admitted aperture.

## Registered whole flows

| Flow | Ingress to terminal | Principal owners | Required terminal coverage |
| --- | --- | --- | --- |
| `exception-retirement` | CPU exception → alignment bridge → ring/user classification → scheduler-owned stack-growth plan → nonblocking process mapping → exact commit, or resume/task retirement/process retirement/kernel panic | `kernel-hal`, `kernel-executive`, `kernel-ps` | success, error, grow-down commit, contention/stale-plan task retirement, non-final exit, final exit |
| `ipc-call` | enqueue → receive → reply, caller deadline, or server exit | `kernel-ipc-runtime`, `kernel-compat` | reply, timeout/cancel, peer revoke |
| `waitset` | provider query → generation registration → recheck → arm/presence check → wake/recheck | provider service, `kernel-compat`, `kernel-ps` | ready, timeout, signal cancel, provider revoke |
| `vfs-open-description` | staging checkpoint → path chunks → live open → dup/fork/exec reference transitions → cursor prepare/settle → final tombstone or restart replay | `vfsd`, `rootd`, compat fd table | live, commit, cancel, restart recovery, final close |
| `endpoint-lifecycle` | create → owner/epoch publication → same-process worker use → non-final or final exit | IPC runtime, compat registry, process table | continued process ownership, final revoke, stale-publication rejection |
| `service-restart` | observed exit → revoke → bounded backoff → checkpoint rebase → new epoch publication or terminal failure | `rootd`, retained checkpoint owner, compat registry | recovered service or failed supervisor |
| `service-bootstrap` | raw ELF entry → stack-aligned Rust supervisor → non-final helper handoff → initd activation → manifest-derived dependency lookup | `rootd`, `kernel-compat`, `kernel-ps`, `initd` | aligned admission, preserved process authority, declared lookup success, undeclared/contract-error rejection |
| `root-authority` | first rootd-owned endpoint → sealed boot owner → exact lease authorization → publisher endpoint-owner proof → same-rootd-epoch commit | `rootd`, compat registry, IPC runtime | published root/service authority, foreign root reclaim rejection, stale grant rejection |
| `service-call-authority` | authorized lookup → exact process/endpoint-epoch grant → call admission → response, revoke, or process-exit cleanup | compat registry, IPC runtime | owner/grantee success, guessed endpoint rejection, stale grant revoke |
| `runtime-control-ingress` | accept → `SO_PEERCRED` PID → live uiserver/logical-admin role check → dispatch or deny | `netd`, `runtimed`, signed launch registry | authorized command, foreign denial, service/process revoke |
| `boot-storage-handoff` | exclusive whole-device freeze → bounded host flush → L0-signed epoch → VFIO assignment → durable exact runtime record → generation-authenticated DVM readiness → exact-exit revoke → host restore or quarantine | `hostd`, storage DVM, `storaged` | active DVM volume, ordered recovery, stale-readiness rejection, quarantine |
| `dvm-block-startup` | exact prefetchable WB shared-RAM aperture admission → reserve-before-mutation cache-map transaction → signed fixed-aperture install → initial generation check → waiter registration → provider publication → atomic recheck → generation-bound FLUSH round trip → volume use; revoke may rebind only a newer L0-signed zero-cursor epoch | `kernel/io-manager`, `kernel-compat`, storage DVM, `storaged` | proven active volume, bounded timeout, forged-epoch rejection, transport revoke |
| `deferred-process-activation` | deferred spawn → kernel-owned exact requester binding → optional loader restart → one-shot activation or requester-exit revoke | `loaderd`, `kernel-compat`, `kernel-ps` | runnable target, denied foreign caller, revoked suspended target |
| `loader-request-authority` | identity-only initd publication → kernel-stamped loader sender → live role admission → image loading → terminal ring0 role revalidation | `rootd`, `initd`, `sessiond`, `procd`, `loaderd`, `kernel-compat` | committed spawn/exec, foreign denial, restart/revoke denial |
| `post-init-service-authority` | deferred child creation → exact `(target, supervisor)` ring0 proof → declared-path match → lease publication → endpoint-owner capability | `initd`, `sessiond`, `rootd`, `loaderd`, `kernel-compat` | live service lease, malformed/foreign denial, reporter-exit cascade |
| `durable-block-mutation` | live-generation admission → WRITE/FUA submission → accepted completion → FLUSH or stable completion → durable-through operation ID | `storaged`, compat block broker, `kernel/io-manager`, storage DVM | durable, timeout/cancel, generation revoke |
| `dvm-volume-io` | exact range validation → bounded broker chunks → generation-bound DVM dispatch | `vfsd`, `storaged`, compat block broker, `kernel/io-manager` | complete, invalid request, timeout, revoke, transport failure |
| `remote-file-map` | mapping admission → immutable early-system or DVM-volume ownership → one-time immutable digest proof or generation-bound source reads → exact-length copy → address-space commit | `loaderd`, `vfsd`, `kernel-compat`, `kernel/mm` | mapped, digest/range rejection, short-read abort, immutable-owner loss, transport failure |
| `memory-map` | canonical checked range → flags/backing plan → fixed-replace prevalidation → page install → complete-span protection preflight → W^X PTE commit → unmap | `syscalld`, compat MM broker, `kernel/mm` | mapped, non-destructive rejection, atomically admitted protection, unmapped |
| `syscall-simd-lifecycle` | trust-boundary user snapshot → kernel continuation → optional block/preemption → exact-task restore | `kernel-compat`, `kernel-ps` | returned, nested capture rejection, wrong-task restore rejection |
| `pci-resource-discovery` | disable command decode → probe/restore low BAR → probe/restore optional high BAR → lowest-mask-bit size decode → command restore/publication | `kernel-hal` | exact resource, invalid-mask rejection, fully restored hardware |
| `zero-trust-e2e` | kernel-stamped receive → exact wire/identity/delegation validation → object-generation admission → request-bound response | IPC substrate, receiving service, subsystem owner, caller | admitted response, malformed/foreign denial, timeout, revoke |
| `commercial-envelope` | exact request envelope → service dispatch → exact response envelope or peer/deadline failure | service owner, caller, IPC substrate | admitted response, malformed request/response, peer exit, timeout |
| `entropy-boundary` | boot RNG seed admission → private master derivation → capability-gated bounded copyout | boot protocol, `kernel-executive`, `kernel-compat` | entropy delivery, zero-seed rejection, authority/shape denial |
| `boot-info-admission` | untrusted boot record → version/size/canonical-field checks → admitted immutable boot state | boot protocol | admitted record, malformed record rejection, zero-RNG rejection |
| `executable-image-admission` | complete ELF/PE bytes → shared range/W^X/entry admission → accepted image or rejection | image admission, `loaderd` | ELF/PE admission, W+X rejection, overflow/alias rejection |
| `bootstrap-content-admission` | normalized early-system path → immutable record lookup → exact SHA-256 verification | boot protocol, `kernel/io-manager` | admitted bytes, path/digest/missing-record rejection |
| `dvm-control-ingress` | secret-derived channel → exact CID/frame → nonce/session proof | host control owner, DVM agent | authenticated session, foreign/duplicate/bad-proof revoke |
| `dvm-block-ingress` | exact prefetchable WB shared-RAM BAR admission → address-free exact-generation request → bounded ring dispatch → ticket/durability-bound completion | block transport substrate, storage DVM | completion, malformed request, stale completion revoke |
| `dvm-network-ingress` | bounded ring header → live control epoch → validated IPv4/ARP payload → exact revoke | network transport substrate, `inputd`, `netd` | frame delivery, malformed payload denial, stale/exact revoke |
| `dvm-input-ingress` | exact prefetchable WB shared-RAM BAR and host `MAP_SHARED` producer admission → bounded cursor-separated ring → policy-consumer admission → epoch/sequence/checksum validation | input transport substrate, `inputd` | record delivery, malformed record denial, transport revoke |
| `dvm-display-ingress` | authenticated display header → io-manager WC mapping transaction distinct from atomic shared-RAM admission → exact-predecessor damage → complete immutable snapshot publication | display transport substrate, `uiserver` | frame publication, shape/damage denial, provider/revoke failure |
| `dvm-read-cache` | generation-bound read miss → bounded window plan → exact-range cache publication or invalidation | `storaged` | cache hit/fill, range rejection, generation revoke |
| `wayland-client-ingress` | local client accept → bounded request/object validation → one-generation dispatch | `uiserver` | delivered request, malformed/buffer denial, client revoke |
| `msi-vector-ingress` | bounded vector lease → exact handler bind → masked table programming → commit or rollback | `kernel-hal` | active route, unauthorized bind denial, complete rollback |
| `process-address-space-lifecycle` | generation retain → exact exec reservation → sibling quiesce/authorization → scheduler root install → mandatory process-owner transfer, or serialized exit/thread attach → frozen exit epoch → final reclaim | `kernel-ps` | committed mutation/attach, pre-install exit-race rejection, post-install transfer despite concurrent exit, final reclaim |
| `post-init-bootstrap-barrier` | exact rootd lease + activation of independent services → overlapped initialization → exact PID-bound endpoint admission for every dependency → consumer start | `initd`, `rootd` | complete barrier success, foreign/stale endpoint denial, child-exit failure, early-consumer fail-closed |
| `bootstrap-activation-handoff` | supervisor-committed exact task → bounded deduplicated FIFO → optional stale removal or overdue fairness turn → oldest live child first dispatch | `kernel-ps` | FIFO first turn, duplicate suppression, stale revoke, preserved overdue turn, impossible-capacity panic |
| `atomic-process-activation-batch` | bounded unique cohort → exact requester binding → complete capability and scheduler preflight → lock-held one-shot authority consumption while every target remains suspended → atomic runnable publication → FIFO cohort first turns before loader reply | `initd`, `loaderd`, `kernel-compat`, `kernel-ps` | complete batch activation, shape/requester/capability/scheduler rejection without partial publication, requester-exit revoke, exact first-turn prefix |
| `ipc-handle-transfer` | rights-checked export → atomic message batch → invisible receive reservation → all-or-nothing install | IPC runtime, compat, fd/open-description substrate | installed batch, export/capacity denial, timeout/peer/exec revoke |
| `process-signal-lifecycle` | pending selection → mask/action/target recheck → handler/stop/kill or fault disposition | compat signal policy, `kernel-ps`, exception bridge | delivery, stale selection denial, recoverable fault, terminal exit |
| `futex-wait-lifecycle` | stable shared-backing key or exact private fallback → atomic word check and exact task/key registration → scheduler arm → wake/requeue, deadline, or task-exit shared-then-private candidate cleanup → bounded robust-list/pending owner-death transition | compat futex owner, `kernel-ps`, executive cleanup | wake, timeout, anonymous non-private fallback, owner-died/clear-child wake, exit cleanup |
| `kernel-resource-lifecycle` | owner quota reservation → allocation/publication → close/exit or deferred backing reclaim → exact quota return | IPC runtime, process table, display/DRM callers | admitted object/task, capacity rejection, immediate revoke, completed deferred reclaim |
| `netd-deferred-reply-lifecycle` | caller-owned absolute end → one admission-time class clamp → global pending-slot reserve → bounded detach batch without deadline refresh → exactly one terminal reply | compat IPC, `inputd`, `netd` | reply, pre-admission expiry, capacity/queue failure, provider timeout |
| `input-delivery-lifecycle` | authenticated DVM record → atomic ingestion-worker arm → bounded drain → unlocked session-authority sync with retained decoded batch and exact ACK suffix → readiness generation → authorized UI read | input transport, `inputd`, `netd`, wait-set, `uiserver` | delivered event, malformed record, bounded authority retry or fail-closed timeout, consumer-owner exit/rearm, provider timeout, transport revoke |
| `ui-main-loop-wakeup` | service-owned generation check → recheck → atomic block/reschedule or coalesced notification → bounded deadline return | `uiserver`, `kernel-hal`, `kernel-ps` | input/Wayland wake, deadline wake, stale generation retry, terminal provider revoke |
| `gpu-frame-lifecycle` | live primed provider → compiler/timeline checkpoint → bounded scene/capability and damage compile → address-free transport admission → commit and acquire/completion/page-flip fences, or exact rollback plus forced complete-atlas retry | `uiserver`, display substrate, Linux DVM | displayed frame, rejected submit without timeline advance, next-success full replay, provider/scene denial, stale completion revoke, hard timeout |
| `acpi-firmware-admission` | checksummed root SDT → atomic MCFG admission → exact HPET GAS admission or explicit legacy/no-HPET topology | `kernel-hal` | ECAM/HPET topology or explicit bounded fallback topology |
| `cpu-topology-admission` | checksummed MADT → complete variable-entry inspection → dense unique fixed-CPU topology publication or rejection | `kernel-hal` | complete topology success, malformed/unsupported/over-capacity error |
| `cpu-online-lifecycle` | admitted logical CPU generation → startup → require existing current-CPU PAT0=WB → program PAT2=UC/PAT4=WC and exact full-MSR readback → private architecture state → scheduler substrate → dispatch admission, quarantine, failure, or invariant panic | `kernel-hal`, `kernel-executive`, `kernel-mm` | online success, PAT/startup failure, skipped/stale transition panic |
| `cpu-affinity-observation` | exact Online snapshot → kernel-versioned topology/task stamp → service shape/owner admission → Linux effective mask or Windows processor observation | `kernel-hal`, `kernel-compat`, `syscalld` | exact observation success, stale/count/mask/owner/pointer rejection, no fabricated CPU zero |
| `task-affinity-lifecycle` | exact live task/process resolution → policy admission → atomic mask commit → forced migration when the running CPU is excluded → fork/clone inheritance or exec preservation | `kernel-ps`, `kernel-compat`, `syscalld` | Linux thread commit, Windows process/thread commit, invalid-mask denial without mutation, migration to an allowed Online CPU |
| `smp-reschedule-ipi-lifecycle` | unique per-CPU current task → durable 0→1 request → exact fixed IPI → locked deferral or safe dispatch → same-CPU consumption | `kernel-ps`, `kernel-hal`, `kernel-lowlevel` | coalesced notification, raw-lock deferral without lost work, safe dispatch, duplicate-owner panic |
| `scheduler-cpu-ownership` | admitted per-CPU current task → raw-guard CPU/APIC capture and nested preemption depth → IRQ deferral or unlocked incoming-task reservation with outgoing-stack retention → assembly `rsp` switch → outgoing-owner release → same-CPU exact guard release | `nucleus-core`, `kernel-lowlevel`, `kernel-hal`, `kernel-ps` | nested release, two-phase stack handoff, safe dispatch, locked-dispatch panic, cross-CPU release panic, underflow panic |
| `tlb-shootdown-lifecycle` | serialized CR3 activation/page-table mutation → active-root target snapshot → generation mailbox → local/remote flush → exact acknowledgements → frame reclaim | `kernel-hal`, `kernel-mm` | root-specific/global success, inactive CPU exclusion, stale/missing acknowledgement panic, no reclaim before all acknowledgements |
| `cross-cpu-task-retirement` | process generation seal → target no-dispatch quiesce + sibling retirement request → remote reschedule → no-running-owner detach → generation replacement → final shootdown → address-space reclaim | `kernel-ps`, `kernel-hal`, `kernel-mm` | thread-attach rejection, exact target binding preservation, bounded remote-owner timeout panic, no replace/reclaim before all old execution stops |
| `thread-exit-futex-cleanup` | retain exact process mapping → validate aligned writable u32 → acquire load → bounded AcqRel robust-owner CAS or release clear_child_tid store → wake | `kernel-mm`, `kernel-compat` | foreign-owner rejection, retry on observed race, WAITERS preservation, no wake before atomic publication, finite contention failure |
| `per-cpu-clockevent-lifecycle` | AP local APIC + invariant-TSC deadline admission → masked LVT programming → first deadline → CPU Online → private interrupt → strict-future rearm → CPU-local scheduler turn → local EOI | `kernel-hal`, `kernel-ps` | prerequisite rejection, vector isolation, no AP PIT writes, delayed-edge catch-up, no scheduling before rearm |
| `scheduler-lifecycle` | runnable current task → exact arm → raced wake/cancel or atomic committed block/reschedule → wake/deadline → exact dispatch and waiter cleanup before timer acknowledgement → retirement | `kernel-ps`, `kernel-hal`, `kernel-compat` | wake success, raced-wake cancel, bounded timeout, retained recovery authority through resume cleanup, terminal retirement |
| `scheduler-dispatch` | source-pinned class admission → optional reply-scoped donation/demotion → terminal reply's one-Scheduler release+wake+exact token capture → post-drop captured-CPU admission or stale-token urgency drop → selection-time exact owner recheck/in-place fresh-generation FIFO refresh → bounded System burst or overdue/latency User dispatch | `runtimed`, `kernel-ps` | User dispatch, donation completion/cancel/revoke, stale-token drop without fallback, base-class demotion |
| `smp-release-admission` | requested RustOS CPU count → prerequisite assessment → uniprocessor admission, complete SMP admission, or explicit rejection | `xtask` | 1-vCPU success, incomplete multi-vCPU error, all-prerequisite multi-vCPU success |
| `smp-ring3-qualification` | injector-owned private contract → suspended exact-executable child → runtimed bind-before-activate or failed-spawn retirement → kernel-bound SESSIOND owner/epoch plus owner/target process-MM identity → lock-stamped absolute deadline → kernel Ready/Start/Finish/Complete FSM with post-admission endpoint revalidation → post-join worker-0 Complete → exact 3N+1 host acceptance for 1/2/4/8 workers, or contract/bind/evidence/deadline/revoke rejection | `runtimed`, `kernel-compat`, `smpqual`, `xtask` | qualified success, unbound/bind identity/epoch rejection, malformed-evidence error, absolute-deadline timeout, owner/target cleanup |
| `monotonic-deadline-lifecycle` | validated invariant-TSC/HPET source → exact task/deadline arm → recheck/commit → clockevent/nondeadline wake, cancel, timeout, or retirement | `kernel-hal`, `kernel-ps` | source rejection, wake success, cancel, bounded timeout, owner exit |
| `user-memory-access` | retain exact process generation → canonical checked range → readable/writable live page spans → complete copy or rejection | `kernel-ps`, `kernel-mm` | complete copy, range/page-access rejection, exec/exit revoke |
| `kernel-memory-protection` | checked kernel ELF segments → W xor X direct-map protection → bounded MMIO map/unmap | `kernel-mm` | protected image, exact MMIO lifecycle, W+X/range rejection |
| `physical-frame-lifecycle` | trimmed boot memory map → kernel/early-system/bitmap reservation → exact free-run allocation → one-time release | `kernel-mm` | allocated/reusable frame, exhaustion, invalid/double/reserved free rejection |
| `service-heap-lifecycle` | one-time kernel bootstrap region → aligned live allocation → exact one-time release → address-ordered coalescing → unlock → bounded grow only after no-fit | `rustos-svc-runtime`, `syscalld`, KVM evidence | reusable span, no lock held across pager wait, duplicate-release rejection, bounded peak-resident growth, explicit allocation failure |
| `commercial-product-boot` | core readiness → initial exact acceptance read or one demoted bounded late-publication watcher → concurrent input/display/storage milestones → exact sealed executable snapshot → first presented WayClick frame; the storage-only branch ends at its proven data plane | `rootd`, input/display/storage owners, `vfsd`, `loaderd`, WayClick, KVM evidence | exactly-once acceptance announcement, malformed/partial rejection, ten-second interactive success, independent five-second storage-only success, stage timeout, generation revoke, no partial-image success |

## External design baselines

These are comparison inputs, not claims of certification equivalence:

- QNX Neutrino models IPC as explicit channel/connection objects and exposes
  send-, receive-, and reply-blocked states. Its timeout/unblock and connection
  death notifications make cancellation and peer loss part of the server
  contract, while resource managers preserve one open control block through
  `dup` and release it only on the last close:
  <https://qnx.com/developers/docs/7.1/com.qnx.doc.neutrino.sys_arch/topic/ipc_Channels.html>,
  <https://qnx.com/developers/docs/7.1/com.qnx.doc.neutrino.lib_ref/topic/c/channelcreate.html>,
  <https://qnx.com/developers/docs/7.1/com.qnx.doc.neutrino.resmgr/topic/messages_HANDLING_open.html>.
- QNX resource constraints reserve capacity for core services and require a
  proxy server to charge work to the constrained client rather than silently
  consuming the server reserve. RustOS applies the same separation at the
  service allocator boundary: transient client work must be reclaimable and
  an exhausted allocation is a visible health failure, never cumulative
  bump-heap loss:
  <https://www.qnx.com/developers/docs/8.0/com.qnx.doc.neutrino.prog/topic/process_resource_constraint.html>.
- Linux PSI distinguishes partial pressure from a full stall and supports
  threshold-triggered monitoring. RustOS uses that as the baseline for future
  CPU/memory/IO pressure epochs; raw log volume or the last printed line is not
  a progress oracle:
  <https://docs.kernel.org/accounting/psi.html>.
- seL4 capDL declares objects and capability distribution independently of the
  loader and translates the same description into initialization and formal
  reasoning inputs. RustOS uses this as the baseline for keeping owner,
  authority, model, source, and witness links machine-readable:
  <https://docs.sel4.systems/projects/capdl/index.html>.
- PikeOS and INTEGRITY certification material treats the evaluated
  configuration, interface documents, security target, partitioning, and
  verification evidence as a versioned bundle. RustOS does not inherit those
  assurance levels; the applicable lesson is that prose without traceable
  source/proof/evidence identity is not an acceptance artifact:
  <https://www.sysgo.com/common-criteria>,
  <https://ghs.com/products/safety_critical/integrity_178_safety_critical.html>.

## Change rule

Any change that adds a high-risk cross-owner transition must update
`system-flows.tsv` in the same change set. Add a new formal model only when no
existing model owns the transition; otherwise add the exact source witness to
the existing model. A supporting model must also appear in
`model-bindings.tsv`; adding a critical/high owner file requires an explicit
`risk_surfaces` entry. The fallback impact detector is intentionally limited to
stateful APIs, service entrypoints, broker operations, scheduler/process/IRQ
owners, and DVM transports. Low-risk local formatting, pure data conversion,
diagnostic rendering, and bounded leaf helpers do not need a flow row and no
longer inherit a blanket “all kernel/services Rust is high risk” classification.
