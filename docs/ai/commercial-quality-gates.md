# Commercial Quality Gates

This contract is the non-negotiable definition of done for an enabled RustOS
product topology. It applies to kernel, service, DVM, boot, and tool-owned
paths alike. "Early stage", prototype status, compatibility, or a fallback
cannot waive a gate.

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
