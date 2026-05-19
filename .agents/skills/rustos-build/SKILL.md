---
name: rustos-build
description: Build the RustOS workspace correctly with required GPG signing environment. Use whenever the user asks to build, check, compile, or stage RustOS — including kernel-only, userspace-only, driver-modules, or full image builds. Skip for unrelated Rust projects.
---

# RustOS Build Skill

## Required Environment

Builds will fail or stage unsigned artifacts without these:

- `RUSTOS_GRUB_SIGNING_KEY` — GPG key ID used to sign the GRUB nucleus
- `RUSTOS_GPG_HOME` — GPG home directory containing that key
- `GPG` (optional) — path to gpg binary, defaults to `gpg` on PATH

If either of the first two is unset, `cargo xtask build-kernel` will warn
about a missing nucleus signature and `stage` will refuse. Confirm with
the user before exporting these — never assume values.

## Commands

| Goal | Command |
|---|---|
| Fast type/borrow check | `cargo xtask check` |
| Full image (kernel + user + drivers + stage) | `cargo xtask build` |
| Kernel only | `cargo xtask build-kernel` |
| Userspace only | `cargo xtask build-user` |
| Driver modules only | `cargo xtask build-driver-modules` |
| Stage existing artifacts | `cargo xtask stage` |

All commands are expected to be quiet on success. On failure, use the
command output as primary context — do **not** scan `logs/` for build
errors. Build logs live in the command, not the runtime log dir.

## Quick Validation Loop

For most code changes: `cargo xtask check` is enough. Only run the full
`build` when packaging or testing in QEMU.

## Do Not

- Do not invoke `cargo build` directly at the workspace root. The xtask
  wrapper handles cross-target setup that plain cargo misses.
- Do not add `--release` to xtask invocations unless the user asks; debug
  builds catch more bugs.
- Do not touch `Cargo.lock` to "fix" a resolver error without checking
  `tools/xtask/src/build.rs` first.
