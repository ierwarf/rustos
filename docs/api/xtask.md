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
small RustOS changes. Reuse an already verified appliance. When only a DVM
relay changed, run the matching cache-preserving target and re-verify:

```bash
make -C driver-domains/linux rebuild-agent   # control/input agent only
make -C driver-domains/linux rebuild-display # display relay only
make -C driver-domains/linux rebuild-net     # network relay only
cargo xtask verify-dvm
```

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

`kvm-smoke` starts both guests and requires the selected RustOS milestones plus
the hash-bound `agent-v1-control` handshake. Optional exercise flags prove the
authenticated DVM input relay and bidirectional display/network shared-memory
rings; startup alone is not success.

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
검증 명령이 아닙니다. 검증된 appliance를 재사용하고, DVM relay 자체가
바뀐 경우에만 `make -C driver-domains/linux rebuild-agent`,
`rebuild-display`, `rebuild-net` 중 해당 target을 실행한 뒤
`cargo xtask verify-dvm`으로 확인합니다.

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

`kvm-smoke`는 Linux DVM과 RustOS를 병렬 부팅하고 선택된 RustOS milestone과
hash-bound `agent-v1-control` handshake를 요구합니다. exercise option은 인증된
DVM input relay와 양방향 display/network shared-memory ring을 검증하며, 단순히
guest가 시작된 것만으로는 통과하지 않습니다.
