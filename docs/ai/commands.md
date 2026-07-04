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
| `cargo xtask run` | boot current image in QEMU | `logs/`, temp dirs | missing `build/image`, missing OVMF, QEMU failure |
| `cargo xtask debug` | QEMU with GDB stub | `logs/rustos-debug.gdb` | same as run plus debug setup |
| `cargo xtask probe-display` | headless display probe with screendump geometry and non-black-frame validation | `logs/` | display/surface/present regression |
| `cargo xtask qemu-scenarios --list` | list predefined QEMU regression scenarios | none | unknown local xtask binary |
| `cargo xtask qemu-scenarios --scenario display-probe` | run one QEMU regression scenario | `logs/` | boot/display/input regression |

## Tests and inventory

| Command | Use | Writes | Common failure meaning |
| --- | --- | --- | --- |
| `cargo xtask selftest` | host selftests for fault parsing, ABI/layout, runtime contracts, module tests | `target/` | contract/layout regression |
| `cargo xtask fuzz-host --target all` | deterministic host fuzz smoke for fault rules, project config, package manifest parsing | `logs/` on crash | parser panic or invariant bug |
| `cargo xtask ring3-inventory` | classify remaining `RING3-MIGRATION-REFERENCE` and `RING3-MIGRATION-COMMENTED-OUT` LOC by owner/lane; read `migration_candidate_loc` as real remaining ring3 work, `ko_slowpath_ring3_loc` as Linux `.ko` slow-path brokerization reference LOC, and `cleanup_debt_loc` as delete/retire work | none | stale marker classification or unexpected active LOC growth |
| `cargo test -p module-tests` | module tests | `target/` | unit/module regression |
| `git diff --check` | whitespace sanity | none | trailing whitespace/conflict marker |

## QEMU args

- xtask args go before `--`; raw QEMU args after `--`.
- Example: `cargo xtask run --profile nvme`.
- Short KVM no-opt debug runs use the built-in timeout and summary:
  ```
  cargo xtask run --profile nvme --accel-profile kvm --usb-input \
    --debugcon file --timeout 35 --summarize-log
  ```
- Commercial-max closure runs use the readiness signature bundle:
  ```
  cargo xtask run --profile nvme --accel-profile kvm --usb-input \
    --debugcon file --commercial-max-ready
  ```
- Use repeated `--expect <marker>` to stop as soon as specific debugcon
  markers appear. Without `--expect`, `--timeout` is a controlled stop.
- For high-density USB pointer validation, use `cargo xtask probe-display
  --accel-profile kvm --usb-input --usb-input-device tablet` or
  `--usb-input-device mouse`. The probe attaches only the selected USB pointer
  device, sends tablet absolute events directly, routes mouse relative events
  through the default display device id, waits for the input surface/storage
  post-boot markers before stressing input, and fails if HID reports, inputd
  reads, or uiserver input ticks stop advancing.
- Tune pointer stress with `RUSTOS_PROBE_STRESS_MS`, `RUSTOS_PROBE_STEP_MS`,
  `RUSTOS_PROBE_INPUT_START_MS` (post-boot quiet time before sending input),
  and `RUSTOS_PROBE_INPUT_STALL_MS` when reproducing short input stalls.
- Use repeated `--fault <location=action>` to pass a validated fault-injection
  rule to the guest via QEMU fw_cfg (`opt/rustos/fault-injection`). Examples:
  `display.present=drop-every:10`, `block.read=fail-after:50`,
  `socket.send=rate:5`.
- Prefer `--summarize-log` and focused `rg` over opening whole log files.
- Do not add ad hoc QEMU or kernel debug branches for one driver. Route
  durable debug state through logging, milestones, registries, and common
  subsystem APIs.

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
