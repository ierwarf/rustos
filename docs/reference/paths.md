# Paths Reference

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

| Path | Meaning |
| --- | --- |
| `Cargo.toml` | Workspace manifest. |
| `config/rustos.toml` | Shared RustOS operational config, including logging and kernel build knobs. |
| `boot/` | Boot protocol crate shared by the GRUB-loaded nucleus. |
| `kernel/` | Kernel entry and subsystem crates. |
| `services/` | Userspace system services. |
| `apps/` | User/demo apps. |
| `driver-domains/linux/` | Isolated Linux DVM image and relays. |
| `drivers/libs/` | Driver ABI/runtime/helper crates. |
| `libs/` | General shared crates. |
| `compat/` | Compatibility layer sources. |
| `assets/image/` | Static staged image overlay. |
| `vendor/` | External firmware/prebuilt/module inputs. |
| `build/artifacts/` | Build artifacts copied by stage. |
| `build/image/` | Staged boot volume root. |
| `build/rustos-boot.img` | Immutable source disk for a KVM RustOS guest. |
| `build/kvm/` | Private KVM disk plus RustOS and Linux DVM logs. |
| `build/image/EFI/BOOT/BOOTX64.EFI` | GRUB-generated default UEFI entry. |
| `build/image/nucleus.elf.sig` | Detached GPG signature for `nucleus.elf`. |
| `build/image/system/registry/` | Generated runtime registries. |

### Generated Registries

| Registry | Purpose |
| --- | --- |
| `system/registry/system/desktop-programs.tsv` | Desktop app/service metadata. |
| `system/registry/system/runtime-launch-programs.tsv` | Runtime launch policy. |
| `system/registry/system/startup-programs.tsv` | Startup ordering/policy. |
| `system/registry/compat/windows-system-dlls.txt` | Windows DLL inventory. |

<a id="korean"></a>

## 한국어

| Path | Meaning |
| --- | --- |
| `Cargo.toml` | workspace manifest |
| `config/rustos.toml` | logging과 kernel build knob을 포함한 RustOS operational config |
| `boot/` | GRUB이 로드하는 nucleus와 공유하는 boot protocol crate |
| `kernel/` | kernel entry와 subsystem crate |
| `services/` | userspace system service |
| `apps/` | user/demo app |
| `driver-domains/linux/` | isolated Linux DVM image와 relay |
| `drivers/libs/` | driver ABI/runtime/helper crate |
| `libs/` | general shared crate |
| `compat/` | compatibility layer source |
| `assets/image/` | static staged image overlay |
| `vendor/` | external firmware/prebuilt/module input |
| `build/artifacts/` | stage가 복사하는 build artifact |
| `build/image/` | staged boot volume root |
| `build/rustos-boot.img` | KVM RustOS guest 실행의 immutable source disk |
| `build/kvm/` | private KVM disk, RustOS/Linux DVM log |
| `build/image/EFI/BOOT/BOOTX64.EFI` | GRUB이 생성한 기본 UEFI entry |
| `build/image/nucleus.elf.sig` | `nucleus.elf` detached GPG signature |
| `build/image/system/registry/` | generated runtime registry |

### Generated Registries

| Registry | Purpose |
| --- | --- |
| `system/registry/system/desktop-programs.tsv` | desktop app/service metadata |
| `system/registry/system/runtime-launch-programs.tsv` | runtime launch policy |
| `system/registry/system/startup-programs.tsv` | startup ordering/policy |
| `system/registry/compat/windows-system-dlls.txt` | Windows DLL inventory |
