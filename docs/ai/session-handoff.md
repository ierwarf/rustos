# Session Handoff

**Dated snapshot (2026-10-21), not live working-tree state.** On resume, first
inspect `git status --short`, `git rev-parse HEAD`, the relevant source
owners, and fresh validation. Root `AGENTS.md` is the only stable prompt
prefix. This page does not authorize reusing stale evidence, speculative
changes, or old uncommitted-work claims.

## Latest established implementation and validation

The October 20–21 rounds covered per-CPU domain budgets, precise wake and
reply refusal recovery, x2APIC, SMP scheduler claims, IPC frame custody, and
DVM block read-ahead. These are historical source-work results, not an
assertion that later commits preserved them. The DVM read-ahead experiment
reduced speculative windows from eight to two; its launch and transport
benefits require matched original-tree controls.

### Owned frame-reader admission — 2026-10-21
- `PinnedFrameReader` joins exact backing pin and saved-frame reader admission
  under one Resource53 leaf. Selection, final capture, IPC receiver publication
  and copied observations use it. Borrowed readers share the same checks;
  epoch/UID/geometry/IRQ/bank/writer/claim invariants remain in force. Drop
  releases reader then backing; `into_pin` retains original final-claim backing.
- Counted host control: two resource leaves -> one per combined admission,
  zero on release/transfer. Four new regression tests; native ps 944/944,
  four new exact mutation witnesses killed 4/4, focused selftest/check/contracts
  passed. Independent read-only review found no introduced defect.
- First shipping 1-vCPU isolated 20,000-round-trip run passed; control/candidate
  CPUID minimum 4,400/4,400, IPC minimum 61,200/60,640 and p50 63,640/63,920.
  Candidate overlapped host validation and the change is below the 2% floor:
  this is NOT a measured IPC latency improvement. No 8-vCPU control completed.
- The complete dev-plan now lane passed all 943 selected commands; final PR
  stable batch sealed successfully (45 artifacts). KVM dry-run and bounded
  8-vCPU UI/input smoke exited 0 with authenticated L0 input relay. Its input
  and display advisory deadlines still overshoot (1,752/1,500 ms and
  2,821/2,500 ms); evidence remains under `smp-evidence/unqualified/`, not a
  release boot-budget or complete SMP qualification claim.
- Two shipping repeats after host validation passed with 20,000 round trips
  each: CPUID minima 4,400/4,440, IPC minima 60,600/60,520 and p50
  63,040/63,240. Versus the same-session control, both deltas remain below 2%:
  only the per-admission lock reduction is established, NOT IPC latency/tail
  improvement. Maxima were 42,592,560 and 62,668,680 ticks; none were trimmed.
- Follow-up feasibility answer: large cuts should carry the SAME selected-frame
  lease through validation/funding/final claim, consuming reader custody into
  the existing CPU banks instead of dropping and reacquiring it. Reuse immutable
  facts, retain final revocation/epoch/MM/placement/budget checks and outgoing
  RSP ACK. seL4 fastpath's checked commit boundary and Linux `context_switch`
  custody handoff are references, not drop-in proofs or permission to delete
  RustOS's guards. This larger redesign is only a proposal, NOT implemented;
  avoid a blind all-at-once atomic/RCU conversion.

## Open questions at the snapshot boundary

- An earlier 8-vCPU cold-boot series saw two
  `ipc-donation-handoff-refused` cancellations in 18 boots, including a
  `storaged` launch failure; the final six in that series did not reproduce
  it. Verify current state before declaring the failure fixed or still live.
- IPC long-tail attribution remains unresolved: separate guest scheduling,
  service contention, DVM bulk-read amplification, and host/KVM scheduling.
  Do not attribute p99 changes without correlated samples and an anchored,
  same-session baseline.
- Virtual DVM input/UI smoke, model-checker passes, and control handshake
  observations do not qualify physical GPU FPS, reset/revoke, boot-time
  budgets, full SMP, or commercial recovery.

## Canonical sources

- IPC data and measurement protocol: `docs/benchmarks/README.md`,
  `docs/benchmarks/evidence/results.md`.
- Scheduler/IPC ownership: `docs/ai/smp/scheduler.md`,
  `docs/ai/ownership/execution.md`, `docs/ai/ownership/claim-return.md`.
- Proof and release limits: `formal/CONFORMANCE.md`, `formal/COVERAGE.md`,
  `docs/ai/smp/validation.md`.
- DVM and physical GPU: `docs/ai/abi/driver-domain.md`,
  `docs/ai/physical-gpu-status.md`.

Earlier experiment chronology is available in Git history. Local `/tmp`
logs and personal cache paths are not durable sources of truth.
