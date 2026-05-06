# AI Commands

Use from repo root.

| Command | Use | Writes | Common failure meaning |
| --- | --- | --- | --- |
| `cargo xtask check` | validate layering/manifests/workspace | `target/` | dependency layer violation, bad manifest, missing target |
| `cargo xtask build` | full OS build + stage | `target/`, `build/` | compile error, missing firmware/artifact, manifest staging error |
| `cargo xtask stage` | restage built artifacts | `build/image` | missing required artifact, bad install path |
| `cargo xtask run` | boot current image in QEMU | `logs/`, temp dirs | missing `build/image`, missing OVMF, QEMU failure |
| `cargo xtask debug` | QEMU with GDB stub | `logs/rustos-debug.gdb` | same as run plus debug setup |
| `cargo xtask probe-display` | headless display probe | `logs/` | display/surface/present regression |
| `cargo xtask build-user` | userspace packages only | `target/`, `build/artifacts` | service/app compile error |
| `cargo xtask build-driver-modules` | bridge modules only | `target/`, `build/artifacts` | driver/module build error |
| `cargo test -p module-tests` | module tests | `target/` | unit/module regression |
| `git diff --check` | whitespace sanity | none | trailing whitespace/conflict marker |

QEMU args:

- xtask args before `--`.
- raw QEMU args after `--`.
- Example: `cargo xtask run --profile nvme -- --no-reboot`.
- Short KVM no-opt debug runs should use the built-in timeout and summary:
  `cargo xtask run --profile nvme --accel-profile kvm --usb-input --debugcon file --timeout 35 --summarize-log -- --no-reboot`.
- Use repeated `--expect <marker>` when a run should stop as soon as specific
  debugcon markers appear. Without `--expect`, `--timeout` is a controlled stop.
- Prefer `--summarize-log` and focused `rg` over opening whole log files.
- Do not add ad hoc QEMU or kernel debug branches for one driver. Route durable
  debug state through logging, milestones, registries, and common subsystem APIs.

Do not run:

- destructive git commands unless explicitly requested.
- formatters that rewrite files unless the task is implementation, not planning/review.

Docs verification:

- `mdbook build` if `mdbook` exists.
- `rg -n "\[[^]]+\]\(([^)#]+\.md)" docs README.md` to inspect md links.
- top-level human docs should include `[English](#english) | [한국어](#korean)`.

Fast context commands:

- `rg -n "symbol_or_path" kernel services tools libs drivers apps`
- `find kernel -maxdepth 4 -name api.rs | sort`
- `find . -name RUSTOS.package.toml | sort`
- `rg -n "enum XtaskCommand|struct Config|enum PackageKind" tools/xtask/src`
- `sed -n 'START,ENDp' path/to/file` after `rg` finds the relevant line range.

GRUB Secure Boot debug environment:

- `export RUSTOS_GPG_HOME=$PWD/build/dev-grub-gpg`
- `export RUSTOS_GRUB_PUBKEY=$PWD/build/dev-grub.pub`
- `export RUSTOS_GRUB_SIGNING_KEY=20E25476F6977B91B007E98F0A12944CD8F6DA70`
- `grub-file --is-x86-multiboot2 build/image/nucleus.elf`
- `gpg --homedir build/dev-grub-gpg --verify build/image/nucleus.elf.sig build/image/nucleus.elf`

KVM display boot loop:

- `cargo xtask build`
- `cargo xtask run --profile nvme --accel-profile kvm --usb-input --debugcon file`
- `rg -n "error: no suitable video mode|boot framebuffer|bootfb|virtio-gpu|virtio register|DisplayUnavailable|uiserver|panic|scheduler invalid" logs/debugcon.log logs/qemu*.log`

Use `rg --files` instead of recursive `ls` or broad `find` when searching many files.

Generated path exceptions:

- Inspect `logs/` only for run/debug failures.
- Inspect `build/image/system/registry/` only for stage/registry verification.
- Inspect `vendor/` only for firmware/module packaging tasks.
