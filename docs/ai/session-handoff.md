# Session Handoff — 2026-11-05

Dated evidence, not instructions or proof of the current checkout. Resume with
`git status --short`, `git log -1`, and the live request. `AGENTS.md` is the only
stable repository prefix; use [task router](task-router.md) for owner contracts.

## Harness maintenance (integrated, uncommitted; base `b9a5b224`)

The user requested non-runtime harness cleanup, bug fixes and updates with
subagents. No commit or push is authorized. Three isolated workers covered
xtask developer checks, KVM harness and documentation; independent reviewers
approved the integrated source and corrected hook changes.

- Removed unused per-edit check stamps/fingerprinting and policy helpers,
  redundant verification stages, stale bootstrap triggers and duplicate prose.
- Fixed dev-plan rename/deletion/nested-module routing; its suggested KVM
  dry-run uses `--no-build`. Verify streams and retains every stage's logs and
  enables both required native service features in one workspace test sweep.
- Config parsing shares validation; fuzz seeds replay before deterministic
  mutation rounds. Fixed F5 environment diagnosis (16 vCPUs, not 8) and made
  the host-test alias's feature selection explicit.
- KVM readiness/recovery drains queued relay errors; snapshots honor their full
  deadline and report actual complete/partial status; missing option values
  cannot consume subsequent flags. Updated the owner docs and stale seal advice.
- Read guards distinguish semantic search scopes from file reads, resolve paths
  against event cwd (including traversal/symlinks), and reject unbounded zero
  limits. Edit advisories resolve the same paths and emit one JSON document for
  multi-file patches.
- Recursive force removal no longer bypasses the shell guard merely because
  `/tmp/` occurs in the command. The guard remains heuristic, not a sandbox.
- Failure diagnostics bound the first error line and avoid repeating it in the
  tail. Removed a retired Claude config trigger from Serena bootstrap CI.
- Verified: `cargo xtask check`; xtask tests (267); `cargo host-test` (192);
  shell syntax and hook assertions (46); Python agent tests (3); doc/context
  checks; `tools/check-dev-environment.sh --ai`; formal registry check; KVM
  `--dry-run --no-build --rustos-vcpus 1`. Worker fuzz evidence covers six
  targets x 256 iterations. Logs: `build/harness-maintenance/`, including
  the copied `worker-fuzz-host.log`.
- No full signed-image verify pipeline, PR/nightly formal run or live KVM boot
  was run for this harness batch. No kernel/service/driver source changed.
  Check's existing advisory is `kernel/compat/src/user/syscall/linux.rs`:
  625 lines > 600; no new compilation failures.
- The reported npm warning belongs to global `~/.pi/agent/npm`, not tracked
  RustOS settings: `@ast-grep/cli@0.43.0` postinstall is blocked. The current
  `ast_grep` tool successfully searched xtask. No install script was approved.

## Previous integrated runtime batch

`b9a5b224` integrates reply-bound socket publication, accept4 flag handling,
netd accounting workers, occlusion-aware atlas fragments, unified console /
Wayland stacking, truncation-safe shm snapshots and bootstrap font assets.
The previous session reported workspace/native tests, a PR formal seal,
1-vCPU KVM (6 sequential + 12 pinned parallel), and one 16-vCPU smoke pass.
These are historical results, not validation of this maintenance batch.

Full implementation history, old log paths and superseded observations remain
in `git show b9a5b224:docs/ai/session-handoff.md`; they are intentionally not
preloaded here. In particular, older entries describing missing stacking,
missing global AF_UNIX accounting, an unguarded shm copy or pure-spin allocator
must not override the newer integrated batch without checking source.

## Previously reported risks to recheck before related work

These are carry-forward findings, not newly verified defects. Runtime repairs
are outside this harness-only batch.

- No full independent audit of the latest Ring0 transfer registry / v2 ABI;
  no fresh DVM window capture. `display-provider` exceeded its advisory 2.5 s
  deadline (2.6–2.9 s); `linux.rs` exceeded its advisory 600-line budget.
- Wayland I/O remains on the UI loop; pointer hit tests ignore consoles.
  Cold DVM GPU budget behavior has limited parallel-load evidence. The DVM
  host ring/teardown audit and large `driver-domain-host` split are unfinished.
- VFS cursor transactions still lack a durable late-prepare cancellation fence
  and reserved reconciliation slot; restart/prepared-state and unpublished-open
  billing settlement remain concerns. Queue saturation can return ENOSPC.
- General INET edge-trigger wake correctness needs an audit of every stack
  advance site, not just ingress-worker error progress detection.
- Earlier 1-vCPU WayClick/startup failures and DNS EAI_SYSTEM/errno=16 were not
  fully isolated. Historical evidence: `build/ping-validation/`; do not call
  a DNS/ICMP success a complete smoke pass if first-frame acceptance fails.
- IPC receiver capacity / fast-frame pinning, rootd FAILED lease retirement,
  and sessiond rejected-launch retry accounting were reported unresolved.
- Compat limitations: poll >512 fds, oversized netd writev, forbidden sigreturn
  eflags, lossy C strings; MM heap provenance capacity, unreclaimed direct-map
  split budget, and existing-page alignment/width checks need review.
- Input-ring consumer-ready cursor ownership, FAT remove_dir/rename cyclic
  chains, refund queue saturation, Windows heap/TLS/critical-section/Unicode
  limitations, and runtimed's source-order-only refund test were also reported.

## Architecture and validation references

- [Ring3 status](../validation/ring3-8stage-status.md) and
  [provider custody](../validation/ring3-provider-custody-acceptance.md).
- [Job model](../architecture/job-model.md): total kmem charging and full
  legacy authority migration remain open.
- [TLB redesign](../architecture/tlb-shootdown-design.md): preserve exact ACK /
  reclaim safety; [commercial gates](commercial-quality-gates.md) and
  `formal/COVERAGE.md` track missing runtime/hardware evidence.
- Old seals do not validate changed source-binding inputs. Ordinary multi-vCPU
  smoke/interactive launches only warn on missing PR evidence; qualification /
  iteration modes enforce their matching seal. Run the PR gate explicitly for
  push/release evidence; see the updated build/KVM skills.
