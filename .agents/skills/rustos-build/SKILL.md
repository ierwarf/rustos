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

Start after edits with `cargo xtask dev-plan`. It classifies fast checks and
one-time stable change-set gates; the plan itself is routing, not evidence.

| Goal | Command |
|---|---|
| Fast type/borrow check | `cargo xtask check` |
| Full image (kernel + user + drivers + stage) | `cargo xtask build` |
| Kernel only | `cargo xtask build-kernel` |
| Userspace only | `cargo xtask build-user` |
| Driver modules only | `cargo xtask build-driver-modules` |
| Stage existing artifacts | `cargo xtask stage` |

For a source-only edit in one cached Linux DVM relay package, use exactly one
matching development compile after the local edit set is coherent:

| Relay | Fast compile only | One-time integration rebuild |
| --- | --- | --- |
| input/control | `make -C driver-domains/linux dev-agent` | `make -C driver-domains/linux rebuild-agent` |
| display | `make -C driver-domains/linux dev-display` | `make -C driver-domains/linux rebuild-display` |
| network | `make -C driver-domains/linux dev-net` | `make -C driver-domains/linux rebuild-net` |

`dev-*` is intentionally not a DVM image build: it refuses a cold or changed
Buildroot configuration, compiles against the cached sysroot, and leaves the
rootfs, manifest, and release artifacts stale. It also makes `verify-dvm` and
KVM fail closed until the matching `rebuild-*` completes. Do not use it as an
every-edit ritual; batch source edits, then use it for a quick compile signal.
Use `rebuild-*` once the change set is stable, before any DVM verification or
KVM run. Never run `clean`, `distclean`, or a toolchain rebuild for an ordinary
relay source edit. This is aligned with Buildroot's package-rebuild and
development guidance: <https://buildroot.org/downloads/manual/manual.html>.

Release rootfs generation keeps the `.cpio.xz` ABI but uses the wrapper's
fixed-block parallel XZ contract. Do not invoke Buildroot directly or replace
it with the upstream reproducible-build default: that silently restores the
single-threaded `xz -9` bottleneck. On the current 454 MiB DVM rootfs the
measured compression step is about 14 seconds and 182 MiB, versus about
79 seconds and 144 MiB for the old default. The fixed block size, pinned host
XZ, normalized input timestamps, and manifest hash retain deterministic
release evidence; `verify-dvm` remains mandatory after integration.

All commands are expected to be quiet on success. On failure, use the
command output as primary context — do **not** scan `logs/` for build
errors. Build logs live in the command, not the runtime log dir.

## Quick Validation Loop

For most code changes, run the focused test/check lane emitted by
`cargo xtask dev-plan`. Only run the full `build` when packaging or preparing a
KVM guest run. For RustOS-only changes, `cargo xtask verify-dvm` reuses the
existing artifact; it does not justify a DVM rebuild.

## Do Not

- Do not invoke `cargo build` directly at the workspace root. The xtask
  wrapper handles cross-target setup that plain cargo misses.
- Do not add `--release` to xtask invocations unless the user asks; debug
  builds catch more bugs.
- Do not touch `Cargo.lock` to "fix" a resolver error without checking
  `tools/xtask/src/build.rs` first.
