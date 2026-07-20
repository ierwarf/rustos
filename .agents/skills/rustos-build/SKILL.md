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

Before the first integration build of a commercial DVM hardware profile,
settle its kernel feature envelope in `board/linux.fragment` and its fail-closed
checks in `scripts/verify-kernel-config.sh`. Batch page-map, DMA-BUF/sync, KMS,
recovery, and observability requirements into that one structural change. Do
not pre-enable speculative guest VFIO/IOMMUFD, generic DMA heaps, or debug
providers merely to avoid a future rebuild. A kernel-envelope change rebuilds
Linux, both kernel-signed module packages, and the rootfs once; it must not
discard GCC, binutils, musl, Mesa, or LLVM.

Before invoking an integration build, run
`make -C driver-domains/linux build-plan`. Treat its output as routing only.
`mode=full-output` is valid for a changed Buildroot/toolchain identity or an
unsafe complete `BR2_*` transition. Kernel inputs, local relay inputs, and
rootfs/post-build inputs must appear as their narrower named lanes. Run
`make -C driver-domains/linux selftest-config-cache` after changing this policy.

The AMD `1002:1900` profile post-build seals the rootfs to the 13 names in
`board/amdgpu-firmware-1002-1900.txt`. Changing only that list or post-build
policy reinstalls the cached `linux-firmware` package into the already-pruned
target tree, then invalidates the rootfs image, not the host toolchain.
Verification must reject both missing and extra AMD firmware.

Release rootfs generation keeps the `.cpio.xz` ABI but uses the wrapper's
fixed-block parallel XZ contract. Do not invoke Buildroot directly or replace
it with the upstream reproducible-build default: that silently restores the
single-threaded `xz -9` bottleneck. On the current 454 MiB DVM rootfs the
measured compression step is about 14 seconds and 182 MiB, versus about
79 seconds and 144 MiB for the old default. The fixed block size, pinned host
XZ, normalized input timestamps, and manifest hash retain deterministic
release evidence; `verify-dvm` remains mandatory after integration.

The wrapper also preserves the host toolchain for a semantically verified
additive defconfig change only when every changed `BR2_*` value transitions
from disabled to `y` and appears in
`scripts/additive-package-cache-v1.txt`. That policy contains audited
target-only leaf packages that cannot alter already-built package features or
linkage. It then builds only the selected target package and rootfs. Any
unlisted addition, removal, or value change stays on the conservative
clean-output path. Linux source/Kconfig and host kernel-build headers have a
kernel-plus-signed-modules lane; AMD firmware and post-build policy have a
rootfs-only lane. Never widen these exceptions by path name alone.

The profile enables Buildroot ccache with relative output paths. Use the
wrapper's `ccache-stats` target to measure hits. A reusable external Buildroot
SDK is the next cold-build optimization, but adopting one requires a pinned
SDK hash, relocation check, toolchain ABI/config equivalence, and provenance;
do not switch the defconfig to an unverified host or distribution toolchain.

Keep `BR2_PER_PACKAGE_DIRECTORIES` disabled for this profile. Buildroot then
uses its own `.NOTPARALLEL` guard for the package graph while `BR2_JLEVEL=0`
still parallelizes compilation inside each package. Do not enable Buildroot's
experimental top-level parallel mode merely to shorten a cold build; it changes
host/target directory semantics used by the wrapper and is documented upstream
as known to fail in non-unusual cases.

## DVM Integration Decision

Before a DVM integration build:

1. Run `make -C driver-domains/linux selftest-config-cache` after any cache
   policy edit.
2. Run `make -C driver-domains/linux build-plan` and report its exact mode and
   lanes. A plan is not build or release evidence.
3. Reject an unexplained `full-output` result. Full output is justified only by
   the Buildroot/toolchain identity or an unsafe complete `BR2_*` transition.
4. For an interrupted or failed build, rerun the same target. Never use
   `clean`/`distclean` as recovery unless the classified inputs truly require a
   fresh tree or the cached tree is proven corrupt.

Linux source/config, the prepared kernel tree, and every enabled out-of-tree
module form one compatibility unit. Linux kbuild requires external modules to
use the matching kernel configuration and symbol data; module signatures cover
the final module bytes and must not be stripped afterward. Therefore a kernel
lane rebuilds and re-verifies all enabled signed modules even when their source
did not change.

Configuration stamps may be written after Kconfig reconciliation to support
resume. Mutable kernel/package/rootfs stamps are release-current only after the
wrapper completes config checks, module-signature verification, manifest
generation, and artifact verification.

## Cold Build Handoff

When the user schedules a cold build for a later session, stop before invoking
it and leave this exact handoff sequence:

```text
make -C driver-domains/linux selftest-config-cache
make -C driver-domains/linux build-plan
cargo xtask build-dvm
make -C driver-domains/linux ccache-stats
make -C driver-domains/linux profile-build
cargo xtask verify-dvm
```

Run `build-dvm` only once; rerunning after interruption resumes the same output.
`profile-build` is post-build attribution and may require matplotlib/numpy.
Do not install optional host packages without user authority. Keep KVM and
physical VFIO tests out of this handoff until the artifact gate succeeds.

Primary references: the
[Buildroot rebuild rules](https://buildroot.org/downloads/manual/manual.html#_understanding_when_a_full_rebuild_is_necessary),
[Buildroot ccache contract](https://buildroot.org/downloads/manual/manual.html#ccache),
[Linux external-module contract](https://docs.kernel.org/kbuild/modules.html),
and [Linux module-signing contract](https://docs.kernel.org/admin-guide/module-signing.html).

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
