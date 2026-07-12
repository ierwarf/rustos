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
| `cargo xtask build-dvm` | Build and hash-verify the pinned Buildroot Linux DVM. |
| `cargo xtask verify-dvm` | Verify DVM artifact and pre-transport contract hashes. |
| `cargo xtask kvm-smoke` | Concurrently boot the Linux DVM and RustOS through QEMU/KVM. |
| `cargo xtask selftest` / `fuzz-host` | Run host contract tests and deterministic parser fuzzing, including hostd launch plans. |

`kvm-smoke` is bounded to 30 seconds when waiting for readiness markers:

```bash
cargo xtask build
cargo xtask build-dvm
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

`kvm-smoke` starts both guests, then requires RustOS's
`rootd: core services ready, spawning initd via loaderd` marker and an L0-style
host-to-DVM KVM-vsock health/PCI-inventory/keyboard probe. The keyboard proof
accepts only a QEMU-injected `A`: the DVM must return Linux evdev code `30`,
then RustOS must show a new `inputd` read batch after the same synthetic key is
injected through its default PS/2 path. This rejects an early panic or a guest
that merely started; `--expect` adds stricter RustOS milestones. The DVM
manifest carries a hash-bound `agent-v1-control` contract. RustOS has no vsock
endpoint yet, so this is neither a RustOS transport nor a production input or
driver data-plane proof.

<a id="korean"></a>

## 한국어

`cargo xtask`는 RustOS를 빌드하고 KVM 검증을 실행합니다. command
surface는 `tools/xtask/src/cli.rs`에 있습니다.

| Command | 용도 |
| --- | --- |
| `cargo xtask check` | layering, manifest, target, workspace contract를 검증합니다. |
| `cargo xtask build` | 서명된 RustOS boot disk를 빌드·stage합니다. |
| `cargo xtask build-dvm` | 고정된 Buildroot Linux DVM을 빌드하고 hash를 검증합니다. |
| `cargo xtask verify-dvm` | DVM artifact와 host-control contract hash를 검증합니다. |
| `cargo xtask kvm-smoke` | QEMU/KVM에서 Linux DVM과 RustOS를 병렬 부팅합니다. |
| `cargo xtask selftest` / `fuzz-host` | host contract test와 hostd launch plan을 포함한 deterministic parser fuzz를 실행합니다. |

`kvm-smoke`의 readiness marker 대기는 최대 30초입니다.

```bash
cargo xtask build
cargo xtask build-dvm
cargo xtask kvm-smoke --expect 'runtimed: bootstrap ui done'
```

`qemu-system-x86_64`와 read/write `/dev/kvm`, `/dev/vhost-vsock` 접근이 필요합니다. 생성 입력과 log는
`build/kvm/`에 기록됩니다. RustOS disk는 `build/rustos-boot.img`를 복사한
private writable image이며, 저장소에 고정된 OVMF를 QEMU에 명시 경로로
연결합니다. bootstrap disk는 staged raw FAT image를 emulated IDE controller에
연결합니다.

`kvm-smoke`는 Linux DVM과 RustOS를 병렬 부팅하고 RustOS의
`rootd: core services ready, spawning initd via loaderd` marker 및 L0-style
host-to-DVM KVM-vsock health/PCI inventory/keyboard probe를 모두 요구합니다.
keyboard proof는 QEMU가 주입한 `A`만 받습니다. DVM은 Linux evdev code `30`을
반환해야 하며, 이후 RustOS가 기본 PS/2 경로로 주입된 같은 합성 키를 `inputd`가
새 batch로 소비했음을 보여야 합니다. 초기 panic 또는 단순히 생성된 guest는
통과하지 않으며, `--expect`로 RustOS milestone을 더 추가할 수 있습니다. DVM
manifest의 hash-bound `agent-v1-control` contract는 host-to-DVM control만
검증합니다. RustOS endpoint, 물리 keyboard forwarding, production input 또는
driver data plane 검증은 아닙니다.
