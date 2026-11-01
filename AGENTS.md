# RustOS Agent Instructions

Keep model context small. This `AGENTS.md` is the only stable repository prompt
prefix; everything else is read on demand.

## Project

RustOS is a microkernel operating system that runs Linux ELF and Windows PE
applications, with Linux drivers isolated in a driver domain. Keep privileged
mechanisms in Ring0 and policy in Ring3 named services (`rootd`, `syscalld`,
`vfsd`, `loaderd`, `netd`, `inputd`, ...). Preserve the existing architecture
unless the user asks for an architectural change; do not move policy back into
ring0 as a shortcut.

## Execution

- Implement the requested goal, not merely a small patch. Scope follows the
  goal and its real dependencies.
- No arbitrary limits on changed files, lines, modules, subsystems or size.
  Cross-subsystem changes and large refactors are normal.
- Read and modify all relevant code when necessary. Update producers,
  consumers, interfaces, call sites, manifests and related implementations
  together.
- Prefer complete integration over isolated scaffolding, and remove superseded
  code once its replacement is complete.
- Do not stop at an arbitrary small milestone while the goal can be advanced.
- Make routine implementation decisions autonomously; ask only when a decision
  genuinely needs the user.
- Preserve unrelated user changes.

## Tools and context

- Use `rg`, Serena, shell commands and `apply_patch` as the task suggests.
  Serena is a preference, not a mandatory gate.
- Batch independent searches, reads and edits. Do not repeat exploration or
  reread unchanged files.
- Keep generated output and large logs (`logs/`, `target/`, `build/`,
  `vendor/`, `perf.data`, `Cargo.lock`) out of model context; capture them to a
  file and read a bounded part.
- Read focused reference docs on demand, not up front:
  resume `docs/ai/session-handoff.md` then `git status --short`; routing
  `docs/ai/task-router.md`; kernel APIs `docs/ai/kernel-api-map.md`;
  ABI/services `docs/ai/contracts-abi.md`; SMP `docs/ai/smp-contract.md`;
  build/KVM/performance skills under `.agents/skills/`;
  physical GPU `docs/ai/physical-gpu-status.md`.
- Use parallel agents or worktrees for genuinely independent work with
  explicit, disjoint write ownership.
- For long tasks keep a short progress note in `docs/ai/session-handoff.md`
  (what is done, what is disconnected, what is unverified) and resume from
  `git status --short` plus the changed files.

## Validation

- Validation timing belongs to the task. Batch related changes, then validate
  once per coherent batch instead of after every edit.
- `cargo xtask dev-plan` lists suggested checks for the changed files; it is
  routing, not evidence.
- Verification is tiered, so the expensive tier runs once per batch, not per
  edit: (1) inner loop - `cargo xtask check`, focused `cargo test -p`, and a
  1-vCPU `kvm-smoke` (needs no formal seal); (2) push gate -
  `formal/verify-all.sh --profile pr`, whose mutation lane reuses kills cached
  per witness and per package dependency closure (a `Cargo.lock` edit only
  re-runs mutants whose package depends on the changed crate), and which a
  multi-vCPU `kvm-smoke` seals automatically; (3) nightly -
  `--profile nightly` turns the mutation cache off and re-audits everything,
  which is what bounds the cache's documented approximation.
- If the user asks for unverified drafting, skip compilation, builds, tests and
  formal runs.
- Report failures and unverified assumptions accurately; never claim
  validation that was not performed.

## Engineering

- Respect privilege, capability, lifetime, concurrency, IPC and memory-safety
  invariants (lock order, IRQ/preemption, user copy, publication/teardown,
  capability generation, ABI/wire format).
- No fake success paths, silent security fallbacks, or placeholder-only
  implementations.
- Prefer clean ownership and typed interfaces, and delete obsolete code instead
  of accumulating compatibility layers.
- Update an owner contract in Markdown when behavior, interface, invariant,
  ownership or validation actually changes.
- External research is warranted for a new subsystem architecture or an
  unfamiliar external ABI/specification; otherwise prefer local evidence.

## Git and completion

- Prefer coherent large commits over micro-commits; commit and push when the
  user authorizes it.
- Never bypass hooks or signing (`--no-verify`, `--no-gpg-sign`), discard
  unrelated work, or force-push without explicit authorization.
- At the end state what was implemented, what remains disconnected, and what
  was not verified.
