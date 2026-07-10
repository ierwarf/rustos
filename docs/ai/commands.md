# AI Commands

Run from repo root. Commands are expected to be quiet on success; treat
failure output as the primary debugging context.

## Build, stage, check

| Command | Use | Writes | Common failure meaning |
| --- | --- | --- | --- |
| `cargo xtask check` | validate layering/manifests/workspace | `target/` | dependency layer violation, bad manifest, missing target |
| `cargo xtask build` | full OS build + stage | `target/`, `build/` | compile error, missing firmware/artifact, manifest staging error |
| `cargo xtask build-user` | userspace packages only | `target/`, `build/artifacts` | service/app compile error |
| `cargo xtask build-driver-modules` | bridge modules only | `target/`, `build/artifacts` | driver/module build error |
| `cargo xtask stage` | restage built artifacts | `build/image` | missing required artifact, bad install path |
| `cargo xtask clean` | remove generated host/build/runtime outputs | removes `target/`, `build/`, `logs/` | stale generated artifact cleanup |

## Run and debug

| Command | Use | Writes | Common failure meaning |
| --- | --- | --- | --- |
| `cargo xtask build-dvm` | build the pinned Linux DVM and verify its manifest | `driver-domains/linux/out/` | missing Buildroot prerequisite or source/artifact mismatch |
| `cargo xtask verify-dvm` | verify every DVM artifact and control-contract hash | none | altered/missing DVM artifact or contract |
| `cargo xtask xen-smoke` | concurrently create Linux DVM and RustOS HVM through Xen Dom0 | `build/xen/` | missing `xl`, failed domain lifecycle, missing HVM marker |
| `cargo xtask run` | production Xen entry | none until transport exists | deliberately fails while DVM is pre-transport |

## Tests and inventory

| Command | Use | Writes | Common failure meaning |
| --- | --- | --- | --- |
| `cargo xtask selftest` | host selftests for fault parsing, ABI/layout, runtime contracts, module tests | `target/` | contract/layout regression |
| `cargo xtask fuzz-host --target all` | deterministic host fuzz smoke for fault rules, project config, package manifest, and DVM manifest parsing | `logs/` on crash | parser panic or invariant bug |
| `cargo xtask ring3-inventory` | classify remaining `RING3-MIGRATION-REFERENCE` and `RING3-MIGRATION-COMMENTED-OUT` LOC by owner/lane; read `migration_candidate_loc` as real remaining ring3 work, `ko_slowpath_ring3_loc` as Linux `.ko` slow-path brokerization reference LOC, and `cleanup_debt_loc` as delete/retire work | none | stale marker classification or unexpected active LOC growth |
| `cargo test -p module-tests` | module tests | `target/` | unit/module regression |
| `git diff --check` | whitespace sanity | none | trailing whitespace/conflict marker |

## Xen smoke arguments

- `xen-smoke` requires an active Xen Dom0 and `xl`; it never starts Xen itself.
- `--timeout <seconds>` is bounded to `1..=30` and applies only while waiting
  for expected HVM debugcon markers.
- The default marker is `rootd: core services ready, spawning initd via loaderd`;
  repeat `--expect <marker>` for each additional RustOS milestone.
- `--dry-run` verifies DVM artifacts and writes `build/xen/*.cfg` without
  invoking `xl`.
- The DVM's `agent-v1-pretransport` contract is future L0-authenticated Xen
  vchan control only. The lifecycle smoke is not a NIC, storage, `.ko`, PCI
  passthrough, or transport test. Keep those claims behind explicit markers.

## Do not run

- destructive git commands unless explicitly requested.
- formatters that rewrite files unless the task is implementation, not
  planning/review.

## Docs verification

- `mdbook build` if `mdbook` exists.
- Inspect markdown links with pattern `\[[^]]+\]\(([^)#]+\.md)`.
- Top-level human docs should include `[English](#english) | [한국어](#korean)`.

## Fast context commands

- Prefer symbol-aware search (Serena MCP) for symbols and scoped text search
  (ripgrep MCP or `rg`) for raw `symbol_or_path` matches under `kernel`,
  `services`, `tools`, `libs`, `drivers`, and `apps`.
- `find kernel -maxdepth 4 -name api.rs | sort`
- `find . -name RUSTOS.package.toml | sort`
- Search for `enum XtaskCommand|struct Config|enum PackageKind` under
  `tools/xtask/src`.
- Read `START..END` only after search finds the relevant line range.
- Prefer scoped file-listing search (`rg --files`) over recursive `ls` or
  broad `find`.

## GRUB Secure Boot debug environment

- `cargo xtask build` creates a local dev GRUB signing key under
  `build/dev-grub-gpg` when `RUSTOS_GRUB_*` is unset.
- `grub-file --is-x86-multiboot2 build/image/nucleus.elf`
- `gpg --homedir build/dev-grub-gpg --verify build/image/nucleus.elf.sig build/image/nucleus.elf`

## KVM display boot loop

1. `cargo xtask build`
2. `cargo xtask run --profile nvme --accel-profile kvm --usb-input --debugcon file`
3. Search the relevant log for
   `error: no suitable video mode|boot framebuffer|bootfb|virtio-gpu|virtio register|DisplayUnavailable|uiserver|panic|scheduler invalid`.

## Generated path exceptions

See `token-policy.md` §10 for the canonical list. Summary: `logs/` only for
run/debug failures, `build/image/system/registry/` only for stage/registry
verification, `vendor/` only for firmware/module packaging.
