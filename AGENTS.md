# RustOS Agent Instructions

Read this first, then keep context small.

## Context Budget

- Start with `docs/ai/token-policy.md`.
- Use `docs/ai/task-router.md` to choose the smallest context set.
- Use `docs/ai-map.md` or `docs/ai/repo-map.md` before broad source search.
- Do not preload all docs, all manifests, or whole subsystems.
- Prefer `rg` and exact `sed -n 'START,ENDp'` ranges over opening large files.

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
- Build orchestration: `tools/xtask/src/build.rs`
- Staging and registries: `tools/xtask/src/stage.rs`
- QEMU runner: `tools/xtask/src/qemu.rs`
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
  details at the end. OpenAI prompt caching depends on exact reusable prefixes.
- Treat `AGENTS.md`, `docs/ai-map.md`, and the focused `docs/ai/*` files as the
  stable reusable prefix. Do not mix logs, generated output, or transient command
  output into that prefix.
- For providers with explicit context caching, this repo's best cache unit is:
  `AGENTS.md` + `docs/ai-map.md` + `docs/ai/token-policy.md` +
  `docs/ai/task-router.md`. Add one focused AI doc only when the task class is
  known.
