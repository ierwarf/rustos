# Session Handoff

**Role:** volatile routing note for resuming this checkout. It is not build,
runtime, formal, or hardware evidence. Source, the live goal tracker, and fresh
command output win when they disagree with this page.

## Current checkout snapshot

Recorded on 2026-08-01 when the user handed the active SMP/UI investigation to
another Pro session:

- The commercial x86_64 SMP goal is still active but this agent was explicitly
  told to stop here and prepare a handoff. Continue only from the user's new
  Pro session; do not mark the full goal complete from the bounded evidence
  below.
- The worktree is intentionally dirty across formal models, kernel and service
  hardening, AI infrastructure, and supporting tools. Preserve all existing
  tracked and untracked work. Never use `reset`, `clean`, or broad `restore` to
  make the checkout look tidy.
- Current-source validation passed after the latest input and epoll-control
  fixes: `cargo xtask check`, `cargo xtask build`, `cargo xtask verify-dvm`,
  `formal/selftest.sh`, and `formal/verify-all.sh --profile pr`. The PR profile
  reused all 22 exact TLC inputs in two seconds, killed 16/16 specification and
  40/40 implementation mutations, passed 423 source witnesses, Kani, Verus,
  Loom, Shuttle, and herd7, then sealed 34 current artifacts.
- Inputd no longer consumes the host's one-shot authenticated `SESSION_START`
  before netd publication. It retains the decoded batch, decoder epoch, and
  exact unacknowledged transition suffix, retries outside the policy queue,
  and exits fail-closed at five seconds. The latest KVM run consumed all
  1783/1783 input-ring records with zero uiserver input drops/errors and moved
  the cursor visibly; the operator confirmed that the mouse moved, although
  the UI was extremely choppy.
- The next proven failure was generic Wayland client admission. Wayland-rs
  `insert_client` performs `epoll_ctl(ADD)`, but RustOS incorrectly charged
  persistent epoll create/ADD/MOD/DEL/retire/purge to the 16 ms readiness-query
  deadline. Under SMP contention this returned `ETIMEDOUT` after netd had
  accepted the socket. The mutation class is now a focused module using the
  100 ms interactive-control deadline, with `waitset` and
  `service-mutation-recovery` flow/source witnesses.
- A fresh 2-vCPU 30-second KVM run after that fix proved the correction:
  `uiserver: wayland client accepted` and `wayclick: first frame presented`
  appeared, all general RustOS/Linux-DVM/display/input/network readiness was
  true, and real network counters reached tx 5/5 and rx 6/6. It still failed
  the 55 FPS gate. The operator observed roughly 7 FPS and severe cursor/UI
  stutter. Uiserver update windows were about 6-9 frames/s after WayClick
  admission, while the DVM relay reported roughly 3-14 frame/s.
- The private KVM UI profiler did not activate in that final run: neither
  uiserver nor WayClick emitted its `acceptance profile enabled` marker, so the
  runner reported `wayclick-observed=None` despite the real first-frame marker.
  `services/runtimed/src/spawn.rs::apply_kvm_acceptance_contract` reads
  `/system/registry/system/kvm-acceptance-v1.env`; determine why the private
  contract written by `tools/xtask/src/kvm/layout.rs` was unavailable or not
  applied to both bootstrap uiserver and later WayClick. Fixing evidence
  injection is necessary but is not the performance fix: the visibly measured
  low compositor/relay cadence remains real.
- Resume with a high-risk static review of the generic path
  `WayClick -> AF_UNIX/netd -> epoll/vfsd -> uiserver dispatch -> GPU submit ->
  DVM relay`, then add only evidence-driven probes or structural fixes. Do not
  restore a WayClick-specific fast path, CPU renderer fallback, unbounded
  polling, or the previously rejected experiment that retained runtimed's
  System class during catalog loading.
- The requested 1/2/4/8-vCPU 90-second WayClick 55-FPS matrix, recovery
  qualification, and bounded perf optimization remain incomplete.
- The independent guest boot-deadline termination is temporarily disabled for
  SMP diagnosis; the measured ten-second product target remains in ABI/formal
  contracts, while KVM currently terminates only at its outer `--timeout`.
- `formal/COVERAGE.md` is the acceptance ledger. Re-run the gate relevant to a
  new claim. Do not rerun an unchanged successful exact TLC input; the cache is
  authoritative for identical model/config/runner inputs. Any source edit
  still invalidates the current-source formal seal before KVM.
- Physical GPU state, evidence limits, and the generic userspace wait-set's
  remaining release gates live only in `physical-gpu-status.md`. Do not start,
  bind, reset, or retry
  hardware merely because a new session began.
- Documentation, skill, hook, Serena, formal-model, and RustOS-only changes do
  not require a Linux DVM rebuild. Route any real DVM change through the
  `rustos-build` and `rustos-kvm` skills and their cached-build rules.

## Resume sequence

1. Read the stable prefix: `AGENTS.md`, `docs/ai-map.md`, `token-policy.md`, and
   `task-router.md`.
2. Query the live goal state, then run `git status --short` and a focused
   `git diff --stat`. Treat both as inspection only; do not normalize the
   checkout.
3. Route the new user request through `task-router.md`. Read this page again
   only for continuation or handoff work, not as a universal fifth prefix.
4. Use Serena or ripgrep for scoped discovery. If either MCP server is absent
   or fails, continue with local `rg`; MCP availability is not a product gate.
5. After edits, run `cargo xtask dev-plan` and execute only the relevant lanes.
   For AI-infrastructure changes, also run `.codex/hooks/selftest.sh` and
   `tools/check-dev-environment.sh --ai`.

## Refresh rule

Update this page only when preparing another handoff or when the live goal,
major blocker, hardware safety boundary, or validation ownership changes.
Keep durable architecture in the focused AI contracts and detailed pass/fail
evidence in its owning ledger; do not duplicate either here.
