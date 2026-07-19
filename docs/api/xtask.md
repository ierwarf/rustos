# xtask API

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

`cargo xtask` builds RustOS and runs KVM verification. The command surface
is defined in `tools/xtask/src/cli.rs`.

| Command | Purpose |
| --- | --- |
| `cargo xtask check` | Validate layering, manifests, targets, and workspace contracts. |
| `cargo xtask build` | Build and stage the signed RustOS boot disk. |
| `cargo xtask build-kernel` | Build the nucleus only. |
| `cargo xtask build-user` | Build every current Rust ELF, C ABI app, Windows PE, and Windows DLL userspace artifact. |
| `cargo xtask stage` | Stage already-built artifacts into the boot image. |
| `cargo xtask build-dvm` | Build and hash-verify the pinned Buildroot Linux DVM. |
| `cargo xtask verify-dvm` | Verify DVM artifact and pre-transport contract hashes. |
| `cargo xtask kvm-smoke` | Concurrently boot the Linux DVM and RustOS through QEMU/KVM. |
| `cargo xtask selftest` / `fuzz-host` | Run host contract tests and deterministic parser fuzzing, including hostd launch plans. |

`build-dvm` is a full Buildroot appliance build, not the default validation for
small RustOS changes. Reuse an already verified appliance. Before integration,
run `make -C driver-domains/linux build-plan`; it reports full-output or the
kernel/module, relay, and rootfs lanes without performing the build. When only
a DVM relay changed, run the matching cache-preserving target and re-verify:

```bash
make -C driver-domains/linux rebuild-agent   # control/input agent only
make -C driver-domains/linux rebuild-display # display relay only
make -C driver-domains/linux rebuild-net     # network relay only
cargo xtask verify-dvm
```

If `build-dvm` is interrupted or fails during compilation, rerun the same
command after correcting the error. Do not use `clean` or `distclean` unless
`build-plan` requires a full-output transition or the cache is proven corrupt.
After a completed build, `ccache-stats` reports cache use and `profile-build`
attributes elapsed time by package; neither is runtime evidence.

`kvm-smoke` is bounded to 30 seconds when waiting for readiness markers:

```bash
cargo xtask build
cargo xtask verify-dvm
cargo xtask kvm-smoke --expect 'runtimed: bootstrap ui done'
```

It requires a usable `qemu-system-x86_64` and `/dev/kvm` access, and writes
runtime inputs and captures to `build/kvm/`:

- a private writable RustOS disk copied from `build/rustos-boot.img`;
- RustOS debugcon and serial captures;
- Linux DVM serial and per-guest QEMU stderr captures.

The RustOS guest uses the repository-pinned OVMF through an explicit QEMU
firmware path. Its single bootstrap disk is the staged raw FAT image on an
emulated IDE controller.

`kvm-smoke` starts both guests and requires the selected RustOS milestones plus
the hash-bound `agent-v1-control` handshake. Optional exercise flags prove the
authenticated DVM input relay and bidirectional display/network shared-memory
rings; startup alone is not success.

`--gui-dvm-surfaces` now means the V3 control/pixel backing plus the private
three-slot GPU atlas transport. In QEMU it proves only
`source-path=staged-copy zero-copy=0` fixed-command GPU composition. It cannot
claim the physical AMD DMA-BUF source import, explicit-fence, or atomic-scanout
path, and no retired CPU-frame or zero-atlas relay is accepted as a fallback.

The explicitly non-commercial `--physical-amdgpu <BDF> --amd-vfct <TABLE>`
variant replaces the virtual GPU with one already-bound AMD `1002:1900` VFIO
function and disables QEMU networking. It never changes a driver binding or
resets the device. Readiness still requires a completed real `uiserver` scene
submission, DVM GPU rendering, DMA-BUF acquire, and physical atomic KMS
page-flip; DRM initialization or a test pattern is insufficient. The command
requires direct read/write IOMMUFD/VFIO access and at least 4 GiB inherited
memlock. Because timeout cleanup can leave the GPU dirty, this diagnostic path
is not commercial lifecycle, reset, revoke, or recovery evidence. QEMU 10.2.1
also attempts an unsupported IOMMUFD peer-to-peer map for an mmap-able PCI BAR
on this APU, so this lab-only launch disables VFIO BAR mmap and the PCI ROM BAR.
That permits functional diagnosis but slows MMIO; neither a successful boot nor
FPS measured with this workaround closes the commercial performance gate.

`--min-ui-fps <fps>` patches only the private KVM disk to enable uiserver and
WayClick profiles. It requires consecutive render/input windows, balanced
WayClick commit/frame-callback/buffer-release windows with bounded callback
gaps, and the requested DVM relay windows when GUI-DVM surfaces are enabled.
It does not lower or infer one result from another. A timeout reports the
observed WayClick window count, commit/callback rate range, largest callback
gap, and largest redraw time directly in the error so diagnosis does not
require dumping the whole debug log. The observation includes non-one-second
startup windows; use their elapsed time to distinguish provider admission delay
from steady-state throughput rather than silently discarding either.
When the private GPU proof is enabled, readiness also requires the exact
priority-8 scheduler admission, 50/100 ms continuous-CPU limits, hard-limit
termination action, and observed restoration to normal policy. Missing or
forged scheduler fields fail the gate.

<a id="korean"></a>

## 한국어

`cargo xtask`는 RustOS를 빌드하고 KVM 검증을 실행합니다. command
surface는 `tools/xtask/src/cli.rs`에 있습니다.

| Command | 용도 |
| --- | --- |
| `cargo xtask check` | layering, manifest, target, workspace contract를 검증합니다. |
| `cargo xtask build` | 서명된 RustOS boot disk를 빌드·stage합니다. |
| `cargo xtask build-kernel` | nucleus만 빌드합니다. |
| `cargo xtask build-user` | 현재 Rust ELF, C ABI app, Windows PE/DLL userspace artifact를 모두 빌드합니다. |
| `cargo xtask stage` | 기존 artifact를 boot image에 stage합니다. |
| `cargo xtask build-dvm` | 고정된 Buildroot Linux DVM을 빌드하고 hash를 검증합니다. |
| `cargo xtask verify-dvm` | DVM artifact와 host-control contract hash를 검증합니다. |
| `cargo xtask kvm-smoke` | QEMU/KVM에서 Linux DVM과 RustOS를 병렬 부팅합니다. |
| `cargo xtask selftest` / `fuzz-host` | host contract test와 hostd launch plan을 포함한 deterministic parser fuzz를 실행합니다. |

`build-dvm`은 Buildroot appliance 전체 빌드이므로 작은 RustOS 수정의 기본
검증 명령이 아닙니다. 검증된 appliance를 재사용하고, 통합 전에
`make -C driver-domains/linux build-plan`으로 전체 빌드 또는 kernel/module,
relay, rootfs 갱신 범위를 읽기 전용으로 확인합니다. DVM relay 자체가 바뀐
경우에만 `make -C driver-domains/linux rebuild-agent`,
`rebuild-display`, `rebuild-net` 중 해당 target을 실행한 뒤
`cargo xtask verify-dvm`으로 확인합니다.

`build-dvm`이 중단되거나 컴파일에 실패하면 원인을 고친 뒤 같은 명령을
재실행해 기존 output에서 이어갑니다. `build-plan`이 전체 전환을 요구하거나
cache 손상이 입증되지 않았다면 `clean`/`distclean`을 사용하지 않습니다.
성공 후 `ccache-stats`와 `profile-build`로 cache와 package별 시간을 확인할 수
있지만, 이 결과는 runtime 증거가 아닙니다.

`kvm-smoke`의 readiness marker 대기는 최대 30초입니다.

```bash
cargo xtask build
cargo xtask verify-dvm
cargo xtask kvm-smoke --expect 'runtimed: bootstrap ui done'
```

`qemu-system-x86_64`와 read/write `/dev/kvm`, `/dev/vhost-vsock` 접근이 필요합니다. 생성 입력과 log는
`build/kvm/`에 기록됩니다. RustOS disk는 `build/rustos-boot.img`를 복사한
private writable image이며, 저장소에 고정된 OVMF를 QEMU에 명시 경로로
연결합니다. bootstrap disk는 staged raw FAT image를 emulated IDE controller에
연결합니다.

`kvm-smoke`는 Linux DVM과 RustOS를 병렬 부팅하고 선택된 RustOS milestone과
hash-bound `agent-v1-control` handshake를 요구합니다. exercise option은 인증된
DVM input relay와 양방향 display/network shared-memory ring을 검증하며, 단순히
guest가 시작된 것만으로는 통과하지 않습니다.

`--gui-dvm-surfaces`는 V3 control/pixel backing과 private 3-slot GPU atlas
transport를 뜻합니다. QEMU에서는 `source-path=staged-copy zero-copy=0`인
fixed-command GPU composition만 증명합니다. 물리 DMA-BUF import나 direct
scanout 증거로 간주하지 않으며, 폐기한 CPU-frame/zero-atlas relay를 fallback으로
허용하지 않습니다.

명시적인 비상용 진단 옵션인 `--physical-amdgpu <BDF> --amd-vfct <TABLE>`은
가상 GPU 대신 이미 `vfio-pci`에 바인딩된 AMD `1002:1900` 장치를 연결하고
QEMU 네트워크를 끕니다. 실행기는 드라이버 바인딩과 reset을 변경하지 않습니다.
통과하려면 실제 `uiserver` 장면 제출 완료, DVM GPU 렌더링, DMA-BUF acquire,
물리 atomic KMS page-flip이 모두 관측되어야 하며 DRM 초기화나 테스트 패턴만으로는
통과하지 않습니다. 직접 읽기/쓰기가 가능한 IOMMUFD/VFIO와 상속된 4 GiB 이상의
memlock이 필요합니다. timeout 종료는 GPU를 dirty 상태로 남길 수 있으므로 이 경로는
상용 수명주기, reset, revoke, 복구 증거가 아닙니다. 이 APU에서 QEMU 10.2.1은
mmap 가능한 PCI BAR를 지원되지 않는 IOMMUFD P2P 영역으로 매핑하려 하므로, 이
실험 경로에서만 VFIO BAR mmap과 PCI ROM BAR를 끕니다. 이는 기능 진단을 위한
것이고 MMIO가 느려질 수 있으므로, 이 상태의 부팅이나 FPS는 상용 성능 게이트를
닫지 못합니다.

`--min-ui-fps <fps>`는 private KVM disk에서만 uiserver와 WayClick profile을
활성화합니다. 연속 render/input window, callback gap이 제한되고 commit/
frame-callback/buffer-release가 균형을 이룬 WayClick window, GUI-DVM surface를
사용할 때 요청된 DVM relay window를 각각 요구합니다. 한 경로의 수치로
다른 경로의 실패를 대신 통과시키지 않습니다. timeout 오류에는 관측된
WayClick window 수, commit/callback rate 범위, 최대 callback gap과 최대
redraw 시간이 함께 표시됩니다. 이 관측에는 1초가 아닌 startup window도
포함되므로 elapsed time으로 provider admission 지연과 steady-state 성능을
구분하며, 어느 쪽도 숨기지 않습니다.
private GPU proof가 켜진 경우 priority 8 admission, 50/100 ms 연속 CPU 제한,
hard-limit 프로세스 종료 동작, normal policy 복귀 확인도 정확히 일치해야
readiness가 통과합니다. scheduler 필드가 없거나 위조되면 실패합니다.
