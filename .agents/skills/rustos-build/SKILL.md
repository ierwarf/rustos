---
name: rustos-build
description: Build, check, compile, or stage the RustOS workspace through its signed xtask and DVM wrappers. Use for RustOS build requests; skip unrelated Rust projects.
---

# RustOS Build

For source changes, load `rustos-code-editing` first: Serena, ast-grep MCP,
and CodeGraph must pass their preflight before source is edited, with Serena
as the primary editor.

## Routing

Run `cargo xtask dev-plan` after edits. It selects `now` checks and the
one-time `stable-batch` gates; its output is routing, not validation evidence.

| Need | Command |
| --- | --- |
| Fast workspace check | `cargo xtask check` |
| Full signed image | `cargo xtask build` |
| Kernel | `cargo xtask build-kernel` |
| Userspace | `cargo xtask build-user` |
| Restage existing artifacts | `cargo xtask stage` |
| Linux DVM appliance | `cargo xtask build-dvm` (see DVM changes below) |

Do not call root `cargo build` directly. The xtask wrapper owns cross-target
configuration and the repository-local development signing identity. Only set
`RUSTOS_GRUB_SIGNING_KEY` or `RUSTOS_GPG_HOME` when the user supplies the
intended signing material; never invent overrides.

## DVM changes

Before any Linux DVM integration build, run:

```sh
make -C driver-domains/linux build-plan
```

Run the plan's narrow lane. For a cached relay source edit, use its matching
`dev-*` compile while iterating, then one matching `rebuild-*` after the set is
stable and before `verify-dvm` or KVM. A cold/toolchain/config change may use
`cargo xtask build-dvm`. An interrupted build resumes the same target; do not
run `clean` or `distclean` as recovery.

## Evidence

Treat command output, hook output, artifact verification, and the selected
tests as evidence. Keep `Cargo.lock`, generated output, and runtime logs out
of routine inspection. Never bypass hooks with `--no-verify` or signing flags.
