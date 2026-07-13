# Repository Structure

<a id="english"></a>

## English

RustOS is a hybrid microkernel-oriented system. Kernel code keeps privileged
bootstrap, scheduling, memory, user-copy, and bounded DVM transports. Services
own app-visible policy. Linux drivers run only in the isolated DVM.

| Path | Role |
| --- | --- |
| `boot/` | Shared boot protocol and loader inputs. |
| `kernel/` | Privileged substrate, ABI entry, storage boot transport, DVM transport. |
| `services/` | Policy services such as rootd, syscalld, vfsd, inputd, netd, loaderd. |
| `apps/` | User/demo applications. |
| `driver-domains/linux/` | Pinned Linux DVM image, relays, and build scripts. |
| `drivers/libs/` | Shared driver-domain ABI and protocol crates. |
| `libs/` | Shared non-kernel libraries. |
| `compat/` | Windows/Linux application compatibility support. |
| `assets/image/` | Static image overlay. |
| `tools/xtask/` | Build, staging, DVM, and KVM commands. |

Rules:

- Put new device support in `driver-domains/linux/` plus a fixed RustOS DVM
  transport contract. Do not add direct kernel hardware or module paths.
- A missing DVM transport means that device is unavailable. Do not add a
  firmware, USB, PS/2, or direct virtio fallback.
- Keep policy in the owning service. Kernel changes must be a narrow,
  capability-gated or bounds-validated substrate.
- `cargo xtask check` validates workspace layering. Use
  `cargo xtask build-dvm`, `verify-dvm`, and focused KVM smoke for DVM work.

<a id="korean"></a>

## 한국어

RustOS는 hybrid microkernel 지향 구조입니다. kernel은 privileged bootstrap,
scheduler, memory, user-copy, 제한된 DVM transport만 담당합니다. 앱에 보이는
정책은 service가 담당하고 Linux driver는 격리된 DVM에서만 실행합니다.

| 경로 | 역할 |
| --- | --- |
| `boot/` | 공용 boot protocol과 loader input |
| `kernel/` | privileged substrate, ABI entry, storage boot transport, DVM transport |
| `services/` | rootd, syscalld, vfsd, inputd, netd, loaderd 같은 정책 service |
| `apps/` | user/demo app |
| `driver-domains/linux/` | 고정 Linux DVM image, relay, build script |
| `drivers/libs/` | driver-domain ABI/protocol crate |
| `libs/` | 공용 non-kernel library |
| `compat/` | Windows/Linux application compatibility support |
| `assets/image/` | static image overlay |
| `tools/xtask/` | build, stage, DVM, KVM command |

규칙:

- 새 device support는 `driver-domains/linux/`와 고정 RustOS DVM transport
  contract로 만듭니다. kernel에 direct hardware 또는 module path를 넣지 않습니다.
- DVM transport가 없으면 그 device는 사용할 수 없습니다. firmware, USB, PS/2,
  direct virtio fallback을 추가하지 않습니다.
- 정책은 소유 service에 둡니다. kernel 변경은 capability gate 또는 bounds
  validation이 있는 좁은 substrate여야 합니다.
- layering은 `cargo xtask check`, DVM은 `build-dvm`, `verify-dvm`, focused KVM
  smoke로 검증합니다.
