# Session Handoff — 2026-10-30

Volatile snapshot, not a stable instruction prefix. Inspect live `git status
--short`, `git log -1` and owner sources before reusing evidence. `AGENTS.md`
owns policy; chronology/evidence live in [session-handoff-log.md](session-handoff-log.md).

## Goal and worktree

- HEAD remains `edae8623`; all subsequent mixed-agent work is **uncommitted**.
  Preserve it. No commit or dedicated ring3 performance benchmark has run.
- Continue sequentially: application isolation/quota proof, then job policy3,
  IDL/runtime4, components5, client-funded sessions6, personalities7, operability8;
  `kmem.max_bytes`, `memory.high/min` also remain absent. Benchmark only after
  all stages, against HEAD in the same session. TLB redesign stays out of scope.
- The operator prioritized execution stability before continuing implementation.

## Stabilization slice closed; documentation reseal next

`formal/check-dead-tests.py` now excludes checkout-relative caches only, not
an ancestor named build/target. Four regression tests reproduced three failures
and then passed. The clean HEAD control was moved, without source changes, to
`/home/hongii2/rustos-head-control`; signed build and full PR45-artifact seal passed.

Same-session40 interleaved16-vCPU pairs: current38/40, HEAD37/40, Fisher p=1.0.
No equivalence/stability claim. All80 raw logs were preserved. This exposed
premature repeated-snapshot selection in the host trace reader in both arms.
`tools/xtask/src/formal_contracts/runtime_trace.rs` now selects the first complete
identity-verified repeat after every predecessor, without relaxing the DAG or
skipping malformed evidence. Independent Rust/formal review passed; model21204
states, five spec/five source mutants killed, six focused selector tests and
xtask233 passed. Final now959/1002, required native/services, normal signed
build/check/contracts, preflight20/20, AI context and full PR45 artifacts passed.

Post-fix exact default16-vCPU/20s/40 boots: **40/40, exit0**, all1775 files sealed,
tree `8490b7f51593680b80118444cd1cab6507026720403fe45b7521f96d8714f2f5`.
Seal checked before cohort/after each completed attempt, unchanged. All40 logs
contain CPU0–15 markers;
no TLB/IRQ/kernel-panic/UI-watchdog marker or enabled limit-probe result.
This is functional acceptance, not a proven failure-rate/performance improvement.
Evidence/commands/raw logs: `build/ring3-evidence/resume-20261030`, especially
`ab40` and `causal40`. Result-document edits require a final fresh seal.

## Application proof and remaining boundaries

Fresh strict quota single passed; opt-in60 was57/60, exit1 (failures9/28 lacked
accounting/drain,59 missed DVM GPU readiness). All60 CPU0–15 markers present;
no failed-log TLB/IRQ/kernel-panic marker. Observed32768–32770 pages. Four verified
vector14 worker faults are SIGSEGV-class evidence, not a final process wait-status
claim: rootd cleans up the surviving main thread. Normal image was restored;
real job OOM passed, pid73/job42949672961, resident372271, free3947→374914.
Probes remain off by default; older missing /tmp evidence was not treated as proof.

Known baseline boundaries: storage-before-input with no later marker and
ipcbench timeouts remain failures, not repaired by repeated-snapshot selection.
Read TLB/IRQ diagnostic source milestones first if either recurs. No new
concurrency/clock/cross-event identity proof is claimed for the serial parser.

## Current stage3a WIP (not sealed or boot-verified)

New96-byte JobPolicy ABI v1 uses syscall0x5255005a, preserving Job ABI v4.
Hierarchical256-bit ceilings, exact process/job cache publication, lock-free
Current adapter, typed INSPECT/SET_POLICY syscall and rootd client/probe are
implemented. Legacy broker checks remain unchanged: no positive subject grant
or role migration is claimed. Probe is default-off (`RUSTOS_ROOTD_POLICY_PROBE=1`).

Fresh required native batch passed: ps1041, compat235, mm103, object7, ABI73;
rootd74/loaderd25 host tests and xtask237 passed. Focused Loom4/Shuttle3, herd,
nine targeted spec mutant checks and preflight20/20 passed. Spec checks reused
exact cached kill evidence. Independent read-only review found host acceptance
could mistake initial allowance for restoration; a red regression reproduced
it. Acceptance now requires an allowed frame after denial; the green batch
passed. Source risk contracts are documented and new debt rows remain0/0;
no existing debt limit was raised. New model/source/flow/mutation mappings are
registered; earlier stale generated docs and numeric test ABI version were fixed.

Now lane1023 tests/1070 associations,16 focused source mutant kills (15 fresh,
1 exact cache), opt-in signed build, check, contracts generate/check and KVM
dry-run passed. PR45 exact-tree reuse and1792-file supplement passed, but neither
initial strict boot was accepted: dry-run's default build first restaged normal
(no probe); rebuilt opt-in then passed rootd controller but lacked kernel frames.
The new marker was best-effort; a native red test reproduced it. Exact-name
required-output classification is now added with0/0 debt, source/flow mapping,
regression and two source mutants. Both mutants killed fresh; refreshed now1024/
1071, required native/services, nucleus69/xtask237, preflight20/20, check/contracts,
non-mutating dry-run and opt-in signed build passed. Prior seals are invalidated;
new PR/all-file seal and strict single/opt-in60/default40 remain pending.
Use `--dry-run --no-build --no-auto-verify` to preserve selected image. Evidence is
retained under `build/ring3-evidence/policy3a-20261030`. No commit/benchmark ran.
Next: close this slice before implementing broker migration. A job bitmap alone
cannot match exact endpoint owner/fork/revoke authority; preserve least privilege
through an independently authorized non-inherited subject grant, not circular
`policy && legacy` parity. A real overlap release before bit deletion cannot be
established by a local test run. Full goal stages4–8 and missing memory controls
remain open; stabilization checkpoint seals are invalidated by current edits.
