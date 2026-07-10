# xtask API

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

`cargo xtask` builds RustOS and prepares Xen-domain inputs. The command surface
is defined in `tools/xtask/src/cli.rs`.

| Command | Purpose |
| --- | --- |
| `cargo xtask check` | Validate layering, manifests, targets, and workspace contracts. |
| `cargo xtask build` | Build and stage the signed RustOS boot disk. |
| `cargo xtask build-dvm` | Build and hash-verify the pinned Buildroot Linux DVM. |
| `cargo xtask verify-dvm` | Verify DVM artifact and pre-transport contract hashes. |
| `cargo xtask xen-smoke` | Concurrently create the Linux DVM and RustOS HVM through the active Xen control domain. |
| `cargo xtask run` | Production Xen entry point. It fails closed until the authenticated RustOS↔DVM transport exists. |
| `cargo xtask selftest` / `fuzz-host` | Run host contract tests and deterministic parser fuzzing. |

`xen-smoke` is bounded to 30 seconds when waiting for markers:

```bash
cargo xtask build
cargo xtask build-dvm
cargo xtask xen-smoke --expect 'uiserver: wayland compositor ready'
```

It requires an already booted Xen control domain with `xl`, and writes generated
runtime inputs to `build/xen/`:

- a private writable RustOS HVM disk copied from `build/rustos-boot.img`;
- `linux-dvm.cfg` and `rustos-hvm.cfg`;
- HVM debugcon and serial captures.

The HVM config uses the repository-pinned OVMF through an explicit Xen
firmware path. It never silently falls back to the Dom0 distribution firmware.
Its single bootstrap disk is an emulated AHCI `hda` backed by the staged raw
FAT image; it never assumes an unimplemented Xen PV `xvda` frontend.

`xen-smoke` submits Linux DVM and RustOS HVM creation concurrently, then always
requires `rootd: core services ready, spawning initd via loaderd`. This rejects
a merely-created, paused, or early-panicked HVM; `--expect` adds stricter
milestones. The DVM manifest carries a hash-bound `agent-v1-pretransport`
contract for future L0-authenticated Xen vchan control, not a live transport or
driver data plane. Therefore `cargo xtask run` does not create a product session
or claim device availability.

<a id="korean"></a>

## 한국어

`cargo xtask`는 RustOS를 빌드하고 Xen domain 입력을 준비합니다. command
surface는 `tools/xtask/src/cli.rs`에 있습니다.

| Command | 용도 |
| --- | --- |
| `cargo xtask check` | layering, manifest, target, workspace contract를 검증합니다. |
| `cargo xtask build` | 서명된 RustOS boot disk를 빌드·stage합니다. |
| `cargo xtask build-dvm` | 고정된 Buildroot Linux DVM을 빌드하고 hash를 검증합니다. |
| `cargo xtask verify-dvm` | DVM artifact와 pre-transport contract hash를 검증합니다. |
| `cargo xtask xen-smoke` | 활성 Xen control domain에서 Linux DVM과 RustOS HVM을 병렬 생성합니다. |
| `cargo xtask run` | 상용 Xen 진입점입니다. 인증된 RustOS↔DVM transport가 구현될 때까지 fail-closed합니다. |
| `cargo xtask selftest` / `fuzz-host` | host contract test와 deterministic parser fuzz를 실행합니다. |

`xen-smoke`의 marker 대기는 최대 30초입니다.

```bash
cargo xtask build
cargo xtask build-dvm
cargo xtask xen-smoke --expect 'uiserver: wayland compositor ready'
```

이미 부팅된 Xen control domain과 `xl`이 필요합니다. 생성 입력과 로그는
`build/xen/`에 기록됩니다. RustOS HVM disk는 `build/rustos-boot.img`를 복사한
private writable image이며, Xen config는 저장소에 고정된 OVMF를 명시 경로로
사용하므로 Dom0 배포판 firmware로 조용히 fallback하지 않습니다.
bootstrap disk는 stage된 raw FAT image를 emulated AHCI `hda`로 연결하며,
아직 구현되지 않은 Xen PV `xvda` frontend를 가정하지 않습니다.

`xen-smoke`는 Linux DVM과 RustOS HVM 생성 요청을 병렬로 내고, 항상
`rootd: core services ready, spawning initd via loaderd`를 요구합니다. 생성만
됐거나 pause 상태 또는 초기 panic인 HVM은 통과하지 않으며, `--expect`로 더
엄격한 milestone을 추가합니다. DVM manifest의 hash-bound
`agent-v1-pretransport` contract는 이후 L0 인증 Xen vchan control을 위한
준비물일 뿐, 실제 transport나 driver data plane이 아닙니다. 따라서
`cargo xtask run`은 device가 준비된 것처럼 성공을 보고하지 않습니다.
