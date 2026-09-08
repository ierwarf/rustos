# AI Map

**Role:** on-demand repository index. It is not part of the stable prompt
prefix; use it only when exact search and, when needed, `docs/ai/task-router.md`
still do not identify the owner.

## Stable prefix

`AGENTS.md` only. Everything below is progressive disclosure.

## Focused AI docs

- `docs/ai/token-policy.md` — detailed context/tool/output budgets.
- `docs/ai/session-handoff.md` — volatile checkout state for continuation only.
- `docs/ai/repo-map.md` — deeper source ownership and entrypoints.
- `docs/ai/commands.md` — quiet build/check/debug commands.
- `docs/ai/contracts-infra.md` — manifests, stage/build/log/fault contracts.
- `docs/ai/contracts-abi.md` — IPC/service/broker ABI ownership.
- `docs/ai/kernel-api-map.md` — kernel public boundaries.
- `docs/ai/core-engineering-contract.md` — lifecycle/concurrency/ABI engineering rules.
- `docs/ai/commercial-quality-gates.md` — enabled-topology completion gates.
- `docs/ai/physical-gpu-status.md` — current physical GPU evidence/safety boundary.
- `docs/ai/performance-hardening.md` — measured performance runbook.
- `docs/ai/smp-contract.md` — x86_64 SMP/AP/IPI/per-CPU/TLB contract.
- `docs/ai/pager-protocol-contract.md` — paging/VMA/pager ownership.
- `docs/benchmarks/README.md` — IPC/syscall measured decomposition and closed hypotheses.

## Minimal source entrypoints

- Workspace: `Cargo.toml`
- Build/run CLI: `tools/xtask/src/cli.rs`
- Stage/registries: `tools/xtask/src/stage/mod.rs`
- KVM runner: `tools/xtask/src/kvm.rs`
- Kernel boot: `kernel/src/main.rs`
- Kernel APIs: `kernel/*/src/api.rs`
- Runtime protocol: `libs/runtime-control/src/lib.rs`
- Logging policy: `config/rustos.toml` `[logging]`

## Ownership

`kernel/` owns ring0 mechanisms; `services/` user policy; `apps/` user apps;
`driver-domains/linux/` isolated Linux DVMs; `drivers/libs/` driver ABI/runtime;
`libs/` shared crates; `compat/` Windows/Linux compatibility; `boot/` boot
protocol; `assets/image/` static image overlay.

Search before opening any deeper map or source file.
