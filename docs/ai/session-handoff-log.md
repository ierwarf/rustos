# Session Handoff Log

Chronology and evidence moved out of [session-handoff.md](session-handoff.md) so the
resume file stays short. Entries are dated snapshots written by different agents;
verify any claim against the live tree before reuse. Append new evidence here, not
to the handoff head.

---

## Fresh execution-stability work (2026-10-30)

The operator asked to finish the current work and stabilize execution before
continuing the ring3 goal. No commit or dedicated performance benchmark ran.
Mixed existing changes were preserved.

Fresh now955/998 and required native/services, signed build/check/contracts and
PR reuse passed. Full1772-file zero-exclusion seal was
`55ee10fed64c080a622dc75d748904e59b44eedce06d2a887a747300462b7b7c`.
Default16-vCPU exact40:38/40, exit1 (runs1/12 UI/ipcbench missing; run1 uiserver
main-present watchdog). Opt-in strict quota single passed; new quota60:57/60,
exit1 (9/28 lacked accounting/drain,59 DVM GPU not ready despite accounting).
All100 cohort raw logs have CPU0–15 first-reschedule markers. No failed log has
either TLB/IRQ diagnostic or kernel panic marker; this is not an absence proof
for arbitrary failures or a matched-control regression claim. Quota observations
32768–32770 pages. Restored all three probe options to unset, signed normal
build/check/contracts passed, and real job OOM passed (pid73/job42949672961,
resident372271,free3947→374914; real input relay). Evidence is retained under
`build/ring3-evidence/resume-20261030`, including commands and raw attempts.

Control preparation found an execution-tooling defect, not a kernel diagnosis:
`formal/check-dead-tests.py` checked absolute path components, so a clean HEAD
worktree under `build/ab/control` skipped every Cargo manifest and falsely
reported both registered debt rows stale. The actual unknown-test rejection was
also bypassed by that ancestor path. Four new tests reproduced three failures
(including two ancestor subcases) before the fix. The main checker now excludes
only checkout-relative build/target caches, retaining stale-debt rejection;
`formal/selftest.sh` runs the new selftest. All four tests then passed, as did
refreshed now955/998, AI context contract, preflight20/20 and full PR (45 artifacts,
131 seconds). The clean HEAD control was moved to
`/home/hongii2/rustos-head-control`, where the unchanged checker passes. Its signed
build passed; the cold TLC lane exhausted its total180-second budget and is
resumed from exact caches, without changing timeouts or skipping models. The
control then passed full PR45 artifacts (2115 seconds, mutation lane2058s). No
flags/gates were bypassed and no control source was edited. Next is full
resealing and40 interleaved pairs
with archived raw logs before any runtime repair/regression claim.

Read-only step3 investigation confirmed a shared job bitmap alone cannot equal
legacy exact-PID/live-endpoint authority for both owner and same-job fork child
or across endpoint revoke. The exact native job-inheritance witness passed;
`/tmp/ring3-policy-parity-counterexample.py` is only a logical counterexample,
not production-code verification. An independently authorized non-inherited
subject grant could preserve semantics; `policy && legacy` is circular and not
an independent parity proof. No step3 interface or implementation was added.

Commit-message draft for the tooling slice (not committed):
`fix(formal): scope dead-test cache exclusions to the checkout`.
Step0 draft remains `feat(loaderd,rootd): isolate application jobs and prove quota refusal`;
its functional evidence does not assert every smoke attempt passed or execution
stability is solved. Subsequent docs/tool edits invalidate the prior full seal.

### Matched boot comparison and causal-selection repair

Both full source supplements were stable throughout40 interleaved16-vCPU pairs:
treatment1773 files/tree9d7042a3df4c7a92703bcf705a8dd062c24cce98d845a85566d5501ae899a57e;
clean HEAD1726 files/tree0407d6314fb6de139decc1f94b8ec6ca7d5af0e7cd1f0f1573c5d3b6d691843a.
Treatment38/40 versus HEAD37/40, Fisher p=1.0. No equivalence, improvement or
complete-stability claim follows. All80 raw logs contain CPU0–15 first-reschedule
markers and no TLB/IRQ diagnostic/kernel-panic marker. Every attempt is archived
under `build/ring3-evidence/resume-20261030/ab40`.

Treatment pair2 failed host trace validation: earliest snapshot2948242us preceded
storage2958984us, although later verified snapshots (including3112304us) existed.
Pair3 missed ipcbench end. HEAD pair24 had the same early-snapshot issue, pair26
had storage before input policy, pair29 missed ipcbench end. The latter two
kinds are not repaired by accepting later snapshot repeats, and remain failures.

`tools/xtask/src/formal_contracts/runtime_trace.rs` now computes all-predecessor
maximum time and selects the first complete identity-verified repeat at/after
it. Framing and evidence validation still precede any skip; no missing/early-only
trace is accepted, no scenario dependency was removed. Six native tests passed;
the old selection semantics reproduced two functional failures first.
`formal/runtime-trace-causal-selection/RuntimeTraceCausalSelection.tla` projects
three immutable candidates and two parents, not live logging/clock correctness
or cross-step executable identity. No concurrency/ISA claim is manufactured for
this serial parser. Independent review accepted Rust and found shared Bound in
the formal expected outcome; expected candidates now compare each parent and
frame/identity independently. A first-parent-only mutant exercises that repair.
Initial four spec mutants were killed; all five need rerun against the final
model. Four source mutants were killed; a fifth was compile-invalid because its
bail removed type inference, then corrected and killed by the actual test rather
than counting compilation as kill. The final model checked21204 distinct states;
all five spec mutants were re-killed, all five focused source mutants passed,
and independent follow-up found no blocking model issue. Final now959/1002,
xtask233, required native batch (PS1035/compat233/MM103/object7/ABI70),
rootd70/loaderd25 host tests, normal signed build/check/contracts,
preflight20/20 and AI context contract passed. Models157/flows77/transitions966/
witnesses1432. Full PR passed and sealed45 artifacts (elapsed158s; source89s,
spec127s, implementation82s, concurrency109s). Fresh all-files supplemental seal
and exact40-boot validation followed.

The final exact default16-vCPU/20-second/40-boot command passed40/40, exit0.
All1775 files were sealed without exclusions, source tree
`8490b7f51593680b80118444cd1cab6507026720403fe45b7521f96d8714f2f5`, and
checks before the cohort and after each completed attempt matched. Every raw
log has CPU0–15
first-reschedule markers; none has TLB/IRQ diagnostic, kernel panic, uiserver
watchdog or enabled limit-probe result. This is functional acceptance, not a
failure-rate/performance improvement or proof of universal progress. Commands,
results, audit and raw logs are retained under
`build/ring3-evidence/resume-20261030/causal40`. Storage-before-input without a
later marker and ipcbench timeouts remain known failed-attempt categories,
not repaired by this selector. The user-prioritized stabilization slice now
has source/unit/model/mutation/doc/PR/boot evidence; result-document edits need
fresh PR/source supplements before continuing step3. No commit or benchmark ran.

Commit-message draft (not committed):
`fix(xtask): select causally eligible repeated runtime milestones`.

## Previous handoff body (as of 2026-10-29)

Volatile snapshot, not a stable instruction prefix. On resume inspect
`git status --short`, `git log -1` and the relevant owner sources before reusing
evidence. Root `AGENTS.md` owns repository policy.

## Active ring3 redesign follow-up (2026-10-28)

### Pi quota proof results (2026-10-30): strict SMP success and 60 attempts

After the non-emergency routing correction, full PR sealed 45 artifacts and
all 1772 nonignored files were additionally sealed with zero exclusions:
`61940ff3ed7e1f09f642db8c1659733c9863254e1251a0f4b333a53bfb346b21`.
The strict single SMP proof passed (pid73, job42949672961, charged32768 pages,
free342110, four fully framed/checksummed vector14 workers, exact zero-object/
zero-page drain and real input relay). Log `/tmp/ring3-quota-kvm-second.log`.

The test-option cohort was 59/60, aggregate exit1. Every attempt, including
failed run1, was archived in `/tmp/ring3-quota-60-framing`; full source/PR seal
was checked before and after every attempt and remained unchanged. Accepted
charges ranged 32768–32770. Run1 missed `ipcbench: end` within30s, so it is NOT
counted as passed despite observed app-limit accounting/drain. Its retained
full log has no TLB/IRQ diagnostic or panic marker. All60 raw logs contain
first-reschedule-IPI markers for CPU0–15. This simple presence check is not
complete-frame validation; no same-session control or regression/rate claim.

Next: restore normal signed image by unsetting RUSTOS_LOADERD_LIMIT_PROBE,
RUSTOS_ROOTD_APP_LIMIT_PROBE and RUSTOS_ROOTD_LIMIT_PROBE; check/contracts,
preflight/fullPR, fresh all-files supplement, then exact default40 smoke.
Normal-image restoration with all three variables unset, check/contracts and
now lane955/998 subsequently passed (`/tmp/ring3-default-` logs). Static
preflight/PR, full-files supplement and exact default40 smoke are next.
The result-document edits invalidate the previous exact-tree seal. Probes
remain off by default; no commit or dedicated benchmark has run. Proceed to
policy only after this step0 closure.

### Pi live quota follow-up (2026-10-29): raw exception breadcrumb interleaving

The fixture repair passed all 23 focused re-kills, now 951/993 associations,
required native/opt-in-build/check/contracts, then full PR sealed 45 artifacts
(`/tmp/ring3-fixture-pr.log`). The all-files supplemental seal included 1768
files with zero exclusions, hash
`08c41c639319c8df72339c8893a88bbc9b8de004c35375bb43ad600ab401406b`.

First sealed 16-vCPU `--oom-hog-job-limit` exited 1: strict transport-frame
validation rejected raw `!EX:0e` bytes interleaved into fault-record prefixes.
Observed accounting/drain (job 42949672961, pid 75, usage 32769/32768 pages,
threads 1, free/min 342128/3947) is NOT a passed quota proof. Source/tree hashes
were unchanged before and after the attempt. Logs:
`/tmp/ring3-quota-kvm-first.log`, `/tmp/ring3-quota-kvm-first-debugcon.log`.

Fix under validation: `kernel/hal/src/arch/idt/handlers.rs` routes ordinary
user-exception breadcrumbs through existing bounded serialized output, leaving
supervisor/NMI/double-fault and refused-retirement raw fallback intact. No
parser relaxation, new allocator, lock, assembly or atomic was introduced.
HAL callback/wiring tests and existing debug-core complete-renderer/escaping/
emergency separation witnesses bind the new `exception-diagnostic-custody`
model to existing `exception-retirement` flow. Final native HAL 83, debug-core
16, rootd 70/loaderd 25, required native batch, Loom/Shuttle projections and
opt-in signed build/check/contracts passed. The model checked 20 states; four
implementation and three spec mutants were killed. Independent read-only
review accepted the focused IRQ-safe/lock-bounded routing correction. Now lane
passed 955 selected tests / 998 associations; full PR and resealing must
precede another SMP attempt. Logs use
`/tmp/ring3-exception-log-` prefixes. Preserve all 60 cohort attempts with
`/tmp/ring3-quota-cohort.py` (refuses overwrite, checks full seal every run).
Reseal and retry the real proof, then preserve all 60 cohort logs. No commit
or dedicated ring3 benchmark has run.

### Pi validation follow-up (2026-10-29): exact-test fixture initialization

The full P1-fix PR run failed after 2501 seconds in implementation mutations:
23 of 1671 failed baseline establishment. Actual logs show the exact registered
test DID run but failed on `Stale`/`None`; the runner's generic baseline error
was not a missing-FQN diagnosis. Other PR lanes passed; no seal was emitted
and no 0b SMP boot ran. Log: `/tmp/ring3-limit-object-pr.log`.

Fresh-process reproductions confirmed three fixture families depended on
other tests allocating chunked side storage. Test-only fixes now explicitly
grow the first process chunk in `kernel/ps/src/multitask/pager_vma/tests.rs`,
the fake process placement cell in
`kernel/ps/src/multitask/process_placement/tests.rs`, and the fake user's cell in
`kernel/ps/src/multitask/current_identity/execution/tests.rs`. Production
fail-closed guards and mutation-runner strictness are unchanged. All three
isolated red cases now pass green; logs are
`/tmp/ring3-fixture-isolation-red.log` and
`/tmp/ring3-fixture-isolation-green.log`. Next: focused re-kills for the 23
failed mutations, refresh now/stable gates, then PR and the opt-in live proof.
Post-fixture now lane passed 951 native selections / 993 associations, and
the required combined native batch, rootd/loaderd host-test, opt-in signed
build, check and contracts passed. All 23 focused re-kills passed after their
pristine exact baselines succeeded; none was skipped or accepted on a failing
baseline. No fresh PR seal or live 0b proof yet. Logs use `/tmp/ring3-fixture-` prefixes.
No commit or benchmark has been made.

### Pi resumed (2026-10-29): 0b P1 fixed, fresh validation pending

Goal explicitly resumed; preserve all mixed WIP and continue in the original order.
`services/rootd/src/jobs.rs` now preserves `usage_processes` separately from
live `subtree_processes`. `services/rootd/src/application_limit_probe.rs` maps
both into its snapshot; observation needs one of each and drain needs both zero.
The real wire → decoder → snapshot → poll regression reproduced premature
`closed=[8]` / `Drained { job: 80, pid: 41 }`, then passed after the fix; all
70 rootd host-tests passed. Red/green logs are
`/tmp/ring3-limit-object-red.log` and `/tmp/ring3-limit-object-green.log`.
The TLA model now clears live membership at Kill but objects only at Reap,
and permits an extra unreaped object before observation. Loom/Shuttle projections
likewise start post-kill with zero live members but an unreaped object. New
source/spec mutants cover client/snapshot mapping, extra-object observation and
live-count substitution at drain. Fresh validation now passed: now lane 916 selected tests / 958 associations;
17 focused implementation mutants and seven spec mutants killed; model 431
states. Native PS 1035, compat 233, MM 103, object 7, ABI 70, rootd host-test 70,
loaderd 25, quota-validator 3 and focused Loom/Shuttle each 1 passed. Opt-in
signed build (`RUSTOS_LOADERD_LIMIT_PROBE=1 RUSTOS_ROOTD_APP_LIMIT_PROBE=1`),
xtask check and generated contracts/check passed. Independent follow-up
accepted the P1 fix read-only. Current validation logs use
`/tmp/ring3-limit-object-` prefixes. PR seal/live proof are next; 0b remains
unbooted, and no benchmark or commit has been made.

The newer repository source binding exempts the two handoff documents. The
operator's stricter all-nonignored-files requirement is still in force: use
`/tmp/ring3-all-files-seal.py` to create/check a supplemental complete-tree
seal (no exclusions, also bound to a fresh PR seal) before/after every boot
cohort, and freeze even these handoff files during boots. This does not alter
or revert the other agent's infrastructure changes.

### Pi checkpoint (2026-10-29): Goal disabled; 0b has an unresolved P1

The operator has now disabled Goal mode. Do not resume automatic implementation
without a new request. Preserve all mixed WIP; no commits or dedicated
benchmarks. Steps 3–8 remain open. Live job OOM passed in 0a; live limited-hog
refusal remains unverified.

**Unresolved P1 from independent review, confirmed against kernel source:**
`services/rootd/src/application_limit_probe.rs` incorrectly fills its
`Snapshot.processes` from live `subtree_processes`. In
`kernel/ps/src/multitask/process_table/jobs.rs`, `job_info` explicitly separates
live membership from `usage_processes` (unreaped process objects). Therefore
the current zero-accounting branch does NOT prove zero process objects.
`services/rootd/src/jobs.rs` still omits `usage_processes` from `Info`.
No fix was applied before stopping. On authorized resume: preserve both live
and object counts, require one object for observation and zero for drain,
and add the missing regression (zero live processes/threads/pages but one
unreaped object must retain custody). Update affected witnesses/mutants and
rerun gates. Probe is off by default, and 0b has not been booted or PR-sealed.

Latest pre-fix evidence: now lane passed 915 selected native tests / 957
associations; 14 implementation mutants and five spec mutants were killed,
and model checked 239 states. Those fake/model projections missed the
live-versus-object distinction; passing them does not resolve the P1.

Current 0b source: `services/rootd/src/application_limit_probe.rs` is an
opt-in census observer/cleanup state machine with positive accounting/last-thread/
headroom checks and retained safe-subtree cleanup until objects/pages drain.
`services/rootd/src/jobs.rs` now decodes existing ABI-v4 usage_threads via a
pure tested info conversion (no ABI extension). Shared quota constant is in
`libs/rustos-user-abi/src/syscall/loader_job_realm.rs` and reused by loaderd.
`tools/xtask/src/kvm/application_limit.rs` validates four unique complete
checksum-framed vector-14 worker events plus exact job/limit/accounting/drain,
refusing physical OOM and workload failures. New flag `--oom-hog-job-limit`
cannot combine with watchdog; both build flags stay off by default. For test
image: `RUSTOS_LOADERD_LIMIT_PROBE=1 RUSTOS_ROOTD_APP_LIMIT_PROBE=1 cargo xtask build`.
Reseal before SMP, run one proof then a 60-boot opt-in cohort, then restore a
normal signed image and reseal/normal smoke. No 0b boot has run yet.

Initial 0b native probe tests 5 and host validator tests 3 passed. New model
`application-limit-refusal` passed 239 states; all formal registries/docs and
Loom/Shuttle projections are added. Preflights passed (1668 implementation
mutants, 67 spec-required models / 335 spec mutants). Focused 14 implementation
mutants, five spec mutants, now lane and independent read-only review are
running. Logs `/tmp/ring3-app-limit-{now,mutants,spec-mutants,model,validator,host}.log`.
Do not treat the old 0a seal as valid after these source/doc edits. Next run
full required native/build/check/contracts, full PR, then live proof. Facts
(fault class/accounting/drain) and source-based quota-cause attribution are
explicitly distinguished; surviving-main cleanup is not SIGSEGV process exit.

New slice: `services/rootd/src/loader_realm.rs` retains a separate root-child
app realm per loader incarnation; `services/rootd/src/job_lease.rs` drains it
before service-job retirement. Exact 16-byte provisioning v1 lives in
`libs/rustos-user-abi/src/syscall/loader_job_realm.rs`. Loader retains an exact
attenuated descriptor, then gives each live SESSIOND/runtimed spawn a child
with all three limits before prepare; see
`services/loaderd/src/application_jobs.rs` and
`services/loaderd/src/application_launch.rs`. Exact membership precedes success;
failed terminal replies retire the whole child job. Collection is demand-driven
on the next request. Default budget: 8 processes, 64 threads, 2 GiB data pages.
The opt-in loader limit-proof ceiling is 128 MiB only for
`/apps/oomhog/oomhog.elf`; UI/service budgets stay unchanged.

Independent review found a blocking rootd↔loader authorization cycle in the
initial synchronous provisioning. Fixed with shared cooperative supervision in
`services/rootd/src/loader_transaction.rs` and the existing worker lifecycle.
An independent descriptor precedes worker startup; lease/incarnation checks
follow join. Callback/thread regression schedules an earlier spawn's VFSD
lookup ahead of configuration. Reviewer follow-up accepted this fix read-only.

Step-0a evidence: native PS 1035, compat 233, MM 103, object 7, ABI 70, rootd
host-test 63, loaderd 25 and OOM validator 14 passed; signed build, xtask check
and generated contracts/check passed. Missing-realm errors are transient EAGAIN
(runtimed treats EACCES as permanent), verified in the refreshed batch. New
`application-job-custody` TLC checked 1780 states; five spec mutants killed.
Nineteen focused implementation mutants (including two re-anchored old rootd
mutants) were killed. Final now lane passed 895 selected native tests / 932
associations, including the errno fix. Full PR sealed 45 artifacts.
Earlier
attempts stopped at missing flow registration and invariant-stanza parsing,
now fixed. Convenience logs: `/tmp/ring3-app-{now,mutants,spec-mutants,host,model,build-user}.log`.

KVM OOM validator now parses job kills and requires matching limited app
placement, refuses mixed collateral and job kills under watchdog-blind mode;
its 14 tests passed. `tools/xtask/src/kvm/smoke_args.rs` now waits for
the common process/job termination prefix before exact validation. Sealed
16-vCPU ordinary smoke passed; real hog job OOM passed (pid 75, job 42949672961,
372335 resident pages, free pages 3947 → 208939, actual DVM input relay). Exact
40-boot smoke command: 38/40, exit 1; run 2 missed wayclick frame, run 6 missed
ipcbench end. No matched control, no regression/rate claim. Failed logs were
not both archived by the runner; saved last failure has no TLB/IRQ diagnostic
in its causal tail, not a claim about unavailable full logs.
Evidence: `/tmp/ring3-app-kvm-{oom.log,oom-debugcon.log,40.log,40-failure.json}`;
seal copy `/tmp/ring3-app-0a-pr-seal.json`, tree
`4ad95cd5189ba9e9ee369e51b9d4a6eca20f46fcf90ddbc8771d36c112c34179`.
These new docs edits invalidate that seal; reseal before another SMP boot.
Next: close a separate opt-in limited-hog SIGSEGV proof slice. Reuse existing
job ABI v4 (GET_INFO already has usage_threads; expose it in rootd's client).
Probe only under a new explicit test build option, piggybacking on the existing
rootd census after UI startup: identify the 32768-page hog job, require its
usage near the limit, only its main thread remaining, and free memory above
reserve; retain an inspect/kill handle, retire and wait for zero processes/pages.
The host acceptance must independently match hog pid/job/limit, four unique
vector-14 faulted worker TIDs, no global OOM kill, and a drained-job marker.
No new kernel instrumentation or ABI expansion is needed; distinguish observed
fault/accounting facts from source-based cause attribution. Keep mode off by
default, validate 60 boots for the opt-in proof. Formal/witness/mutant/docs and
all gates still apply. Draft for closed 0a (not committed):
`feat(loaderd): isolate runtime launches in quota-limited application jobs`.
Only after complete step 0 advance to policy/IDL/component/session/personality/
operability in the requested order. Do not benchmark intermediate phases.

### Claude continuation (2026-10-29): job-level governor, control boot

Resumed on the uncommitted Codex work (kept intact, backed up outside the repo
before any control experiment). Native batch over it passed: PS 1012, compat 226,
MM 88, object 7, ABI 69, loaderd 16, rootd host-test 51.

**1-vCPU control.** Codex's failed UP readiness is baseline behavior, not a
placement regression: at `--timeout 60 --expect "ipcbench: end"` both the WIP and
the phase-1 commit (`edae8623`, stashed control, same command, 3 boots each)
reached DVM GPU and UI-FPS readiness and never printed `ipcbench: end`. The
earlier 20 s timeout was too short for UP.

**New in this slice.** `JOB_OP_OPEN_SUBTREE` (op 6; kernel `job_is_within`),
a job-aware `rootd` governor (`assess_with_jobs`, `Verdict::KillJob`,
`job_kill_is_safe`, `kill_job_by_id`), and job kills notifying the ring0
watchdog. Tests: rootd host-test 59, kernel-ps job tests 10, compat job_ops 9.
Eight implementation mutants killed (six new, two re-anchored recovery rows).
The job path has not run in a live OOM episode: applications still inherit
`initd`'s protected job. The ring0 watchdog is still process-granular.

Phase 2a (2026-10-29): process-table and side arrays are chunked directories
(`slot_dir.rs`), `grow_slots`, batched reaper, console registry grown on demand,
`process_limit` runtime variable (default 32, not yet job policy). See
`architecture/job-model.md#phase-2a-as-built`. Sealed PR pass, 1022 kernel-ps
tests, OOM hog e2e and 47/52 sealed 16-vCPU boots (no panics; no matched
control). Next: 2b (charging, `SET_LIMITS`, threads.max/kmem), which is what
connects `process_limit` to the root job. `--proc-stress` printed nothing in two
attempts; its end-to-end status is unverified.
Still open from phase 1: give applications their own job so the job-level
governor path runs in a live episode.

Phase 2b-ii (2026-10-29): `memory_pages` limit via mm charge groups bound to roots
(`charge.rs`, ledger hooks, ABI v4, model `memory-charge-conservation`); see
`architecture/job-model.md#phase-2b-ii-memory-limit-as-built`. Live: rootd boot
self-check `job memory charge ok` (every boot) and the test-mode probe `job memory
limit ok` (build with `RUSTOS_ROOTD_LIMIT_PROBE=1`; an unprivileged copy of rootd,
admitted for direct spawn only without logical admin, is refused pages at a 48 MiB job
limit; off by default because it perturbed SMP start-up). No application has hit a limit; application jobs do not exist.

Phase 2b-i (2026-10-29): counted `processes`/`threads` limits with hierarchical
effective limits, `SET_LIMITS` (ABI v3), admission at publish/attach, model
`job-limits`. **Memory and kmem charging are not implemented**; the design (charge
group on the ledger root record, hooks at each data-leaf count site in
`frame_descriptor_ledger.rs`) is in `architecture/job-model.md` Phase 2b and is the
next slice. See that section for what is deliberately not faked.

### Operator checkpoint: implementation stopped, phase 1 not closed

The operator disabled Goal mode and requested a commit of the current work.
Do not resume automatic implementation from this handoff without a new request.
All pre-existing changes were preserved and are part of this checkpoint. Phase
2a and every later phase remain unimplemented; no dedicated ring3 benchmark ran.

Boot-direct placement now passed the full now lane (768 selected native tests /
798 associations), signed build, ten focused implementation mutants and its
additional CREATE_PROCESS spec mutant. The preceding rootd-transfer slice
passed 762 selected native tests, eight mutants, and the 277-state expanded model
with seven spec mutants. Direct-slice native counts: compat 226, feature-unified
ABI 69 and rootd host-test 51; PS 1012/MM 88 passed before this final bootstrap-only
slice and are being refreshed in the final combined batch. Mutation registry
preflight passed 1611 rows. Final PR sealing follows this documentation freeze;
commit without source/doc edits after that seal. Logs are the
`/tmp/ring3-bootstrap-placement-{now,build,mutants,spec,native}.log` files.

The final 1-vCPU smoke reached five `rootd: direct job placement ok` markers,
`rootd: job tree ok` and `rootd: loader job placement ok`, but failed full 20s
readiness (missing ipcbench end and wayclick first frame). No TLB/IRQ diagnostic
milestone appeared. This is placement evidence only, not complete smoke
acceptance, an explanation of earlier UP failures, or a regression-rate claim.
Saved logs: `/tmp/ring3-bootstrap-placement-kvm-1-debugcon.log`,
`/tmp/ring3-bootstrap-placement-kvm-1-failure.json` and
`/tmp/ring3-bootstrap-placement-kvm-1.log`. No new SMP smoke was run.

Next source work, only if requested: finish job-level memory governor/watchdog
and their protection semantics, then close all phase-1 gates before 2a. Existing
Loom/Shuttle witnesses remain bounded protocol projections, not full production
slab/teardown executions. Keep baseline `edae8623` for the eventual final matched
benchmark session; do not benchmark intermediate phases.

### Earlier slice chronology (superseded by checkpoint above)

Keep all existing WIP. No commits or dedicated ring3 benchmarks. One new
unsealed 1-vCPU placement smoke ran; its placement milestone passed but overall
20s acceptance failed (details below). No new SMP boot or rate comparison.
New work adds authenticated byte+descriptor receive/fused receive syscalls
`0x5255_0058/59` (legacy layouts unchanged), shared retained-MM output before
exact fd publication, and svc-runtime wrappers with no handle-unaware fallback.
Two duplicate REQ/HAZ-SCD IDs were moved to unused 172/173. The zero-trust ingress
checker now recognizes both authenticated receive spellings (including its
identity-only endpoint refusal).

Process prepare ABI is now v5 (32 bytes), with an explicit JOB_HANDLE flag and
loader-local Job descriptor requiring typed CREATE_PROCESS. Exec cannot combine
that flag with its ticket. Prepare retains only JobId/generation; commit reserves
in that job, or the authenticated requester's job rather than loaderd's. Loaderd
now preserves sender identity and handle custody in both receive forms, refuses
placement on non-spawn operations, and closes received authority after handling.
Rootd's next WIP now transfers placement handles for initial initd and loader-
backed restarts. `Lease.job_handle` retains supervisor authority; the loader gets
only CREATE_PROCESS|TRANSFER. Rootd verifies the child's JobId before reporting
`rootd: loader job placement ok`, kills a failed/lost attempt before retry, and
keeps custody across pending bounded retirement. New `job_lease.rs` callback and
source tests passed with rootd host-test 49. All eight rootd implementation
mutants were killed. The expanded model checked 277 states and all seven spec
mutants were killed (the close mutant initially reported the earlier overlapping
NoAbandonedMembers invariant; its expected invariant was corrected). The new
rootd witness rows initially omitted `|host-test`, causing native SIGSEGV in the
now lane, not a guest panic. All four rows now specify it; generated contracts/check and the refreshed now
lane passed (`bg-25`, 762 selected native tests / 792 associations). Signed build
passed for the rootd transfer slice.

The 1-vCPU smoke reached `rootd: job tree ok` and `rootd: loader job placement ok`;
overall readiness failed within 20s, missing ipcbench end, compositor active and
wayclick first frame. Neither TLB/IRQ diagnostic milestone appeared, and no
actual kernel panic was found (the wayclick panic-hook line is initialization).
This is limited placement evidence, not full KVM acceptance or a regression
claim. Logs: `/tmp/ring3-rootd-placement-kvm-1-{debugcon.log,failure.json}` and
`/tmp/ring3-rootd-placement-kvm-1.log`. Other rootd slice logs are
`/tmp/ring3-rootd-placement-{now,mutants,tlc,build,tests}.log`. Boot-direct service launches still lack explicit placement;
job-level governor/watchdog remain open. Do not treat this as phase-1 completion.

Evidence from this continuation: authenticated receive native batch passed PS
1012, compat 221, MM 88, object 7, ABI 67, svc-runtime 15, rootd host-test 45;
9 new auth implementation mutants and its spec mutant were killed. Its initial
now lane completed (individual TLC and 722 selected native witnesses); subsequent
placement edits require refreshed now evidence. Placement native batch passed
PS 1012, compat 224, MM 88, ABI 67, rootd host-test 45, loaderd 16. The later
append-only prepare-layout test passed in the standalone ABI batch (63 tests;
feature-unified counts differ). `cargo xtask build-user` and signed `cargo xtask
build` passed for this placement slice. The final bounded `job-spawn-placement`
model checked 97 states; all five spec mutants and ten implementation mutants
were killed. Full registries are updated; no debt limits raised. The refreshed
now lane passed (`bg-14`, 729 selected native witnesses / 757 associations),
including xtask check, generated contracts/check and formal selftest (150 models).
Context-contract selftest and mutation preflight (1593 entries) passed. Log:
`/tmp/ring3-placement-now.log`. Earlier attempts failed on an uppercase event
name, old ingress spelling and a concurrently changed generated-contract file;
all were fixed/regenerated before the final successful now lane. Other logs:
`/tmp/ring3-placement-{native-all,mutants,tlc,build-user}.log`,
`/tmp/ring3-auth-{mutants,spec-mutant}.log`. Rerun dev-plan/generation after any
registry edits; these are not a PR seal or current KVM acceptance.

The final implementation slice adds boot-direct placement: rootd-only
SYS_RUSTOS_SPAWN_EXEC now takes `RustosBootstrapSpawnArgs` ABI v1 (40 bytes,
required typed Job descriptor), retiring ignored argv/env registers. Shared
CREATE_PROCESS admission precedes loading; ConsoleProgramSpec retains JobId,
without granting root authority, and forwards it to the existing exact spawn
reservation. Rootd allocates/retains/verifies these child jobs too, emits
`rootd: direct job placement ok`, and retires failed attempts. Normal process
prepare stays ABI v5 with absent-job requester inheritance. Removed the now-unused
no-argument compat reservation wrapper after reference/structural checks.
Native compat 226, feature-unified ABI 69 and rootd host-test 51 passed. Five
new witness associations, ten implementation mutants and one extra rights spec
mutant are registered and passed the validation recorded above. Logs:
`/tmp/ring3-bootstrap-placement-native.log` and
`/tmp/ring3-bootstrap-placement-dev-plan.txt`. Next: validate this bootstrap slice, then finish job-level
governor/watchdog protection before phase 2a. Refresh all gates and phase-1
runtime evidence before 2a. Preserve the original baseline `edae8623` and defer
matched-session benchmarks until every phase has finished. Suggested eventual
slice commit draft (do not commit): `feat(loader): preserve authenticated job
placement authority through process prepare`.

The current request supersedes the older IPC resume directions below. Start at
phase-1 closure, then follow `architecture/ring3-process-model.md` §6 strictly:
2a directory, 2b charging/limits, 3 policy, 4 IDL/runtime, 5 component manager,
6 sessions, 7 personalities, 8 operability. Do not run ring3 benchmarks until
all phases are finished; final matched-session baseline is `edae8623` (the
phase-1 commit, clean at task entry). No commits were made in this follow-up.

Current uncommitted slice is verification only: nine native job tests now cover
reserved-before-kill publication refusal, recycled-job generation rejection,
exact cancellation, and actual detach/reap before reuse. The job TLA model has
explicit reservation/cancellation, with 5,994 distinct states checked. New spec
mutant `job-lifecycle-publish-stale-reservation` and implementation mutants
`job-publication-drops-reserved-job` / `job-reservation-accepts-recycled-generation`
were killed. Both original job spec mutants were re-killed. Full Loom/Shuttle
lanes passed with two new bounded protocol witnesses each; these do not execute
production teardown/spinlocks or prove the entire slab. Native PS 1005, compat
215, MM 88, ABI 65 and rootd host-test 45 passed; formal selftest, generated
contracts/check and xtask check passed (pre-existing linux.rs size warning).
That witness-only snapshot passed signed build and the full PR seal (45
artifacts). Subsequent authority edits invalidate that seal; no KVM or benchmark
has been run in this follow-up.

Current WIP adds `JobHandleRights`, a separate generational Job token namespace,
Job descriptors (empty default rights, exact explicit dup/transfer/fork), an
explicit executive-to-rootd fd-3 bootstrap grant, and Job syscall ABI v2 with
per-handle operation rights, subset duplication and scope-masked PID inspection.
`PendingJobHandle` reserves before create, copies out before committing the fd,
and cancels/rolls back an unpublished child. New `job-handle-publication` models
this bounded reservation/copyout/reuse/rollback transaction. Formal registries
and new mutant rows are being added; generated contracts/complete now lane,
final phase validation still needs refreshing. All 23 newly added implementation
mutants and seven new spec mutants were killed. Job test registrations must use
`user::syscall::linux::broker_ops::job_ops::tests::...` (not the flat filesystem
path); corrected source conformance passed 1368 checks before the next fix. The bootstrap
native test initially SIGSEGV'd because its reused fake bootstrap had consumed
Linux metadata and tried to scan a fake page table; fresh bootstrap metadata
plus the canonical fixture fixes it (focused test passed). This was a native
fixture error, not a KVM/TLB/IRQ panic. Native PS 1011, compat 219, object 7,
MM 88, ABI 65, rootd host-test 45 and the executive bootstrap witness passed.

Independent review found an existing reserve helper defect newly exposed by
Job publication: `BTreeMap::insert` overwrote a live invisible ticket before
checking its old value. Fixed by skipping already-reserved fds before insert.
`two_live_job_publications_keep_independent_reservations` failed red (ticket 2
replaced ticket 1) and passed green; the updated existing reservation-conflict
mutant was killed. The publication model now witnesses contending reservations
and its new overwrite mutant was killed. The read-only reviewer follow-up
accepted the fix and found no new issue; it did not rerun tests.
After the fix, native PS 1012, compat 219, MM 88, object 7, ABI 65 and rootd
host-test 45 passed; source conformance passed 1369 checks, and generated
contracts/check, formal selftest (149 models), mutation preflights and xtask check
passed. The authority+reservation-fix snapshot then passed the full PR seal
(45 artifacts) and one 16-vCPU `--no-build --no-auto-verify` smoke with ordinary
readiness, authenticated DVM input relay, rootd Job self-check and ipcbench end.
Two runtime-trace advisory deadlines overshot but conformance passed. Later
handoff/source edits invalidate that exact-tree seal; reseal before more SMP.

The authority candidate's 1-vCPU KVM attempt reached `rootd: job tree ok` and
`ipcbench: end` but failed overall readiness: uiserver idle watchdog and missing
wayclick first frame. No `tlb-ack-liveness-timeout` or
`irq-architecture-capture-refused` milestone. No regression claim without matched
40+ boot cohorts. Saved logs: `/tmp/ring3-authority-kvm-1-debugcon.log` and
`/tmp/ring3-authority-kvm-1-failure.json`. Two post-fix 1-vCPU attempts also
failed the 20s readiness gate, both with the rootd Job self-check passing: attempt
2 missed ipcbench/compositor/wayclick; quiet attempt 3 reached ipcbench end but
missed wayclick first frame. Neither had the TLB/IRQ diagnostic milestones.
Logs are `/tmp/ring3-authority-kvm-{2,3}-{debugcon.log,failure.json}`. There is
one later successful sealed 16-vCPU smoke, but no explanation of the three
1-vCPU failures and no matched-control regression/failure-rate claim. Successful
snapshot logs: `/tmp/ring3-authority-sealed-16-{kvm.log,debugcon.log}`.
No dedicated ring3 benchmark has run.

Broker/service placement, job-level governor and watchdog protection remain
open: phase 1 is NOT closed and phase 2a has not started. Normal rootd spawn goes
directly to loaderd, not procd. Placement must transfer an attenuated Job handle
via existing IPC into loader prepare and keep exec/fork membership unchanged.
The existing handle-aware receive lacks authenticated sender outputs: do not
replace loaderd's sender-authenticated receive with it and lose authentication.
Keep all current changes and finish these seams before progressing to 2a.

 IPC redesign remains paused;
the active follow-up is queued 8-vCPU recovery and reference-proven dead-code
cleanup, not a new latency redesign.

## Slow paths folded into the rendezvous (2026-10-27)

Unfused server 40.1k -> 8.8k p50 (8 vCPU 65k -> 9.5k), 272-byte reply buffer
24.0k -> 8.2k (1 vCPU). Cause and design: [IPC lane](performance/ipc-lane.md#slow-paths-folded-into-the-rendezvous-2026-10-27-after-the-records-below).
Gates passed: kernel-ps 991, ipc-runtime 83, compat 198, ipcbench 21, `cargo xtask check`,
full PR formal (sealed), implementation mutations 1522/1522 (3 new rows), SMP iteration,
1 and 8 vCPU KVM smoke, default bench. Still queued: handle-bearing calls (26.1k),
unparked receivers. Not measured: cross-process large payloads, the 8-vCPU queued
handle probe.

## Current implementation

- Generic dispatch trims and deferred standalone reply wake are integrated with
  IPC queued/rendezvous convergence; policy remains in ring3 services.
- Bounded byte CALLs may attempt rendezvous. One absolute deadline caps fallback;
  expiry before or during response observation wins over publication.
- Register queued/rendezvous CALLs share collection and direct syscall-frame
  parking. Plain RECV and spurious re-parks also use direct continuation; plain
  versus reply-committed receive errors retain separate kinds.
- Queued and rendezvous byte-only REPLY_RECV share deferred completion/receive.
  Removed the separate eager-wake receive helper, constant-true donation
  branches and queued-inline double copy. The neutral direct schedule initializes
  its full 432-byte frame; independent review accepted the correction.
- Follow-up: fused completion refusals retain an exact retry observation;
  serial admission is atomic with typed-wake capture, and retry-pending state
  defeats idle periodic bypass. Missing-wake red test reproduced, four new
  regression cases passed; first recovered 8-vCPU queued run completed 20,000
  samples (p50 89,400 ticks). Full PR/repeated runtime validation still pending.
  Removed unused profiling wrapper and GDIAG counters; kept test-only witnesses.
- Queue/message/reply objects, asynchronous donation materialization, handle
  cleanup and remote wake/IPI remain live. Objectless virtual rendezvous and
  paired-custody switching are **not implemented**.

Current contract and chronology: [IPC ABI](abi/ipc.md),
[IPC lane](performance/ipc-lane.md). The [virtual rendezvous draft](performance/ipc-rendezvous.md)
is future design, not shipped behavior. Older phase measurements are historical,
not an optimization baseline for this checkpoint.

## Validation actually completed

- Native: compat 198, HAL 81, PS 986, IPC runtime 81, ipcbench 21 tests.
- `cargo xtask check`, all 60 selected now commands, source conformance 1349,
  four focused implementation mutants, full PR (45 sealed artifacts), and SMP
  iteration (26 sealed artifacts) passed. Docs-only cleanup uses fresh focused
  context/layout and source-contract checks, not new runtime evidence.
- Signed-image dual-guest register benchmarks passed at 1 and 8 RustOS vCPUs
  with authenticated DVM input relay. These do **not** establish queued-only
  8-vCPU success. No physical GPU or exhaustive cancellation/SIMD stress claim.

## Current queued-only measurement

Probe: `ipc_rt_intra_process_queued_reply_recv`; unchanged kernel, same-process
16-byte echo, fused server, real 272-byte reply capacity forces rejection of the
256-byte fast frame. 2,000 warmups, 20,000 samples, exact STOP/join and untouched
tail required. Shipping profiles all off; source/linked/staged ELF freshness passed.

- **1 vCPU:** p50 **24,080 ticks / 6.033 us**, p99 **36,280 / 9.090 us**;
  observed max **52,866,240 / 13.246 ms**. CPUID p50 4480; no outliers removed.
- Separate leg p50s: request-to-receive 18,760; reply-to-return 5,160 ticks.
- Diagnostic-only run: 22,001 admission attempts, fallbacks and shape refusals;
  no fast reservation publication. Instrumented timings are not shipping results.
- **Historical 8-vCPU failures:** both pre-fix runs reached anchors but lacked
  a queued result / `ipcbench: end` within 30s; repeat missed wayclick first-frame.
  First recovery pass now succeeds; preserve both failures as pre-fix evidence.
  Artifact: `build/bench-core/ipc_rt_intra_process_queued_reply_recv-vcpu-8-1793085089493832523/`.
- No matched queued-before cohort: do not claim improvement over the old ~40k.
  The earlier 7–10k discussion is an aspirational design budget, not evidence.

Durable result: `build/bench-core/ipc_rt_intra_process_queued_reply_recv-vcpu-1-1793082845527969983/`.
Diagnostic and failed-run paths, other probe results and full method are in
[the IPC lane measurement record](performance/ipc-lane.md#measuring-the-queued-slowpath-alone-historical-shape).
`/tmp/ipc-slow-only-*` and `/tmp/ipc-converge-final-*` are convenience logs only.

## Resume boundaries

Do not restart optimization without a new task. Complete recovery validation
and commit the current follow-up; retain normal readiness/end-marker acceptance. Cross-process IPC and loaded/handle-bearing latency were
not measured here. Prior lifecycle/state-rail and physical-GPU questions are
separate scope: consult [core-path gaps](performance/core-paths-gaps.md),
[benchmark progress](../benchmarks/core-paths-progress.md) and
[physical GPU status](physical-gpu-status.md), then obtain fresh evidence.

## Termination, OOM and hardening pass (2026-10-28)

Uncommitted working tree. Added: memory-pressure governor in rootd plus ring0
`OomWatchdog` (defers to rootd, spares service-endpoint owners), single-finalizer
termination (`terminate_user_process_once`), the terminal-withdraw versus
dispatch-claim fix, and new models `scheduler-terminal-claim` and
`oom-termination-owner` with Loom, Shuttle, Kani and mutation witnesses. Disposition and
remaining gaps: `formal/coverage/release-proof-gaps.md`. TLB redesign is a design only:
`docs/architecture/tlb-shootdown-design.md`.

KVM baseline: roughly 4% of 16-vCPU `--expect "ipcbench: end"` boots fail on unmodified
HEAD (UI-class); the OOM work did not change it (48/50 on all three arms). Two scheduler
or TLB panics in about 200 candidate boots remain unattributed; read the new milestones
`irq-architecture-capture-refused` and `tlb-ack-liveness-timeout` first if one recurs.
No performance optimization was made in this pass; none was measured.

## JobPolicy ceiling slice3a resume (2026-10-30; boot closure pending)

Preserved mixed uncommitted work. The new independent96-byte JobPolicy v1
syscall005a, generation-bound hierarchical four-word cache, immutable membership,
Current adapter, rootd typed client/default-off controller and strict host mode
are documented in `docs/architecture/job-model.md`. Legacy role checks are not
migrated, and no real overlap release is claimed. No commit/benchmark ran.

Fresh now lane passed1023 selected native tests/1070 associations. Required stable
native passed ps1041/compat235/mm103/object7/ABI73, services rootd74/loaderd25
host-test and xtask237 passed. Focused Loom4/Shuttle3 and herd passed; the Loom
weak-payload negative fixture finds its expected counterexample. Two exact-input
TLC model summaries have16050/55 distinct states. Nine targeted spec mutant
checks reused exact cached kills;16 --only implementation checks killed all
(15 fresh,1 exact cache). Source risk headers and0/0 debt entries were completed
without increasing any existing debt limit; identity pin/order rationales are
explicit. Preflight20/20 and opt-in signed build passed. Check/contracts/dry-run,
full PR and zero-exclusion source reseal/boot closure are still pending.
The subsequent check/contracts generate+check and KVM dry-run passed.

Read-only independent review found host acceptance counted initial allowance as
restoration. Deleting the post-denial allowed frame failed a new regression
before the fix; acceptance now requires a later allowed sequence, with a focused
restoration-order mutant killed. Host/native suites are green. These bounded
models/projections do not prove production teardown, Current ownership, atomic
256-bit/subtree snapshots, wall-clock-only freshness or arbitrary boot liveness.
Evidence and the next exact closure commands are retained under
`build/ring3-evidence/policy3a-20261030`.

Commit-message draft only:
`feat(ps,compat,rootd): add hierarchical job policy ceilings and typed control`.
Next after closure: independently authorized non-inherited subject grants and
broker-by-broker overlap; do not use circular `policy && legacy` parity or delete
legacy bits before the required real release interval.

### First opt-in boots and evidence-output correction

Full PR reused the exact source seal45 artifacts; zero-exclusion supplement
covered1792 files/treeef4e099d3a83ebdef159f6eac9c692ce6b6085b7985810e3b0b47f21dadfdf7a.
The first boot missed the controller because the earlier KVM dry-run had default
build enabled and restaged normal images. It is not a passed probe. After opt-in
signed rebuild and rootd artifact-string check, the second boot passed the rootd
controller but strict host acceptance refused absent kernel cache frames. Root
GET calls made four output attempts in the best-effort class while the log's
dropped counter rose by four during that interval. This is evidence consistent
with contention loss, not proof that no other execution failure exists.

A native test reproduced wrong BestEffort classification. The exact policy event
in `kernel/nucleus-core/src/debug/milestone_class.rs` now uses existing bounded
retry/deferred Required custody; a near-suffix stays best-effort. New source/flow
mapping and0/0 debt, native regression and two implementation mutants accompany
it. Source change invalidates both prior seals; now/stable/PR/reseal and live
retry remain pending. The host parser was not relaxed. Raw single-attempt logs
are retained in the policy evidence directory. Future dry-runs explicitly use
`--no-build --no-auto-verify`; no benchmark or commit has run.
Correction green and both source mutants killed fresh. Post-fix now1024/1071,
required native/services plus nucleus69/xtask237, preflight20/20, check/contracts,
non-mutating dry-run and opt-in signed build passed. New PR/zero-exclusion seal
and strict single/60/default40 live acceptance remain pending.
