# AI Contracts — Index

Split into two focused files. Load only what the task needs.

- `contracts-infra.md` — package manifest, stage outputs, runtime control,
  kernel API, kernel build, fault injection, logging, docs.
- `contracts-abi.md` — kernel/userspace ABI, IPC service IDs, broker syscalls,
  handle transfer, service routing, display/scheduler contracts.

When updating contracts: edit the file whose section owns the changed behavior.
