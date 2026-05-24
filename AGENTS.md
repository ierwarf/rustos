# RustOS Agent Instructions

Read this first, then keep context small.

## Context Budget

- Start with `docs/ai/token-policy.md`.
- Use `docs/ai/task-router.md` to choose the smallest context set.
- Use `docs/ai-map.md` or `docs/ai/repo-map.md` before broad source search.
- Do not preload all docs, all manifests, or whole subsystems.
- Prefer Serena MCP symbol tools, then ripgrep MCP, and exact focused reads over
  opening large files.

## Tool usage

Prefer symbol-aware search and scoped text search over opening large files.
When project MCP servers (e.g. Serena, ripgrep, GitHub) are available, use them
first; otherwise use the equivalent shell tooling. Keep matches scoped.

Let project hooks run. Do not bypass hooks with `--no-verify`, `--no-gpg-sign`,
or equivalent flags. If a hook blocks or reports a tool/config failure, treat
that output as primary evidence and repair the hook or command path before
continuing.

## Sub-Agent Use

A sub-agent is justified only when it reduces search/read churn that would
otherwise pollute the main context: parallel independent exploration, log
triage, or a disjoint write slice. Do not spawn one for a single focused read
or edit.

Constraints:

- Read-only by default. Workers may write only when given disjoint file scope.
- Pass narrow context: task, relevant paths, stop condition. No secrets,
  signing material, or unrelated repo state.
- Return evidence (files, line numbers, symbols, short summary, uncertainty),
  not final decisions. The main agent owns reasoning, integration, and
  validation.

## Do Not Inspect By Default

- `logs/`
- `target/`
- `build/`
- `vendor/`
- `perf.data`
- `Cargo.lock`

Allowed exceptions:

- For run/debug failures, inspect only the relevant `logs/` file and prefer the last 100-200 lines.
- For stage/registry bugs, inspect only specific files under `build/image/system/registry/`.
- For firmware/module packaging, inspect only the specific `vendor/` path involved.
- For dependency resolution changes, inspect focused `Cargo.lock` snippets with `rg` first.

## Common Commands

- Fast validation: `cargo xtask check`
- Full image build: `cargo xtask build`
- Kernel only: `cargo xtask build-kernel`
- Userspace only: `cargo xtask build-user`
- Driver modules only: `cargo xtask build-driver-modules`
- Stage existing artifacts: `cargo xtask stage`

Build/check commands are expected to be quiet on success. If they fail, use the
reported command output as the primary context instead of scanning logs.

## Hardening Direction

- Active refactor state: RustOS is in a service-first ring0 evacuation phase.
  The direct kernel-launched first user process is `rootd`, not the Linux
  `initd` runtime. `rootd` is the bootstrap authority modeled after an seL4
  initial task: it must stay independent of the Linux dynamic runtime and
  starts the foundational services (`syscalld`, `vfsd`, `loaderd`) before
  handing off to normal `initd`. Do not add generic Linux syscall fallbacks to
  make `initd` boot earlier; move that pressure into `rootd`, service
  manifests, or narrow bootstrap brokers.
  The old line-commented Linux compatibility reference files have been consumed
  into service-oriented syscall routing and removed from the kernel tree. A
  wider evacuation wave now keeps only unfinished Linux thread policy and
  Windows PE/Win32 policy as migration reference comments. Linux MM ABI policy
  now belongs to `syscalld`; PTE mutation and backing lifetime enforcement go
  through the gated `SYS_RUSTOS_MM_BROKER`. VFS, network, USB, input, provider,
  signal, and clock policy should be extended in `syscalld`, `vfsd`, `netd`,
  `loaderd`, `devmgrd`, `driverd`, `storaged`, or `inputd`.
  Do not restore deleted or commented ring0 policy modules for quick
  compatibility fixes; leave kernel code to narrow privileged primitives.
  During this phase, compile/QEMU validation may be intentionally deferred when
  the task is structural code removal.
- Product goal: RustOS must preserve native compatibility for both Linux ELF
  and Windows PE executables. Microkernel migration should move policy and
  namespace ownership to user services without casually breaking observable app
  ABI behavior; when ring0 code is removed, keep compatibility through narrow,
  explicit brokers or service-owned implementations.
- Prefer long-term hardening over symptom patches: make ownership, provider
  choice, timeouts, queue bounds, and ABI contracts explicit in source,
  manifests, registries, probes, or AI contracts.
- Avoid broad catch-alls and fabricated success paths. Fail closed with bounded
  waits and direct diagnostics when an implementation is incomplete.
- For display, input, driver loading, and compat work, keep fallback providers
  behind real hardware/virtio providers and add validation that catches black
  frames, stalls, stale surfaces, and provider-order regressions.

## Repo Entrypoints

- Workspace: `Cargo.toml`
- xtask CLI: `tools/xtask/src/cli.rs`
- Build orchestration: `tools/xtask/src/build/` (`mod.rs`, `cargo.rs`)
- Staging and registries: `tools/xtask/src/stage/mod.rs`
- QEMU runner: `tools/xtask/src/qemu/mod.rs`
- Host config: `tools/xtask/src/config/` (`mod.rs`, `project.rs`)
- Package schema: `tools/xtask/src/package_manifest.rs`
- Kernel boot entry: `kernel/src/main.rs`
- Kernel API boundaries: `kernel/*/src/api.rs`
- Runtime protocol: `libs/runtime-control/src/lib.rs`

## Token Discipline

- Ask or infer the narrow subsystem before searching.
- Summarize findings before opening more files.
- Avoid pasting long command output into responses.
- Keep chat output sparse during implementation. Use the chat for start,
  completion, blockers, or genuinely useful decisions; do not stream routine
  search/build noise.
- On completion, report a brief summary only: what changed, validation run, and
  any remaining blocker or risk.
- Keep stable instructions near the top of future prompts and task-specific
  details at the end. Prompt caching depends on exact reusable prefixes.
- Treat `AGENTS.md`, `docs/ai-map.md`, and the focused `docs/ai/*` files as the
  stable reusable prefix. Do not mix logs, generated output, or transient command
  output into that prefix.
- For providers with explicit context caching, this repo's best cache unit is:
  `AGENTS.md` + `docs/ai-map.md` + `docs/ai/token-policy.md` +
  `docs/ai/task-router.md`. Add one focused AI doc only when the task class is
  known.
