# Build Artifacts and Disk

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

Build outputs, caches, and formal scratch directories are **not source files**.
Use the targeted cleanup modes only when disk pressure warrants it. Avoid
`clean`/`distclean` as routine recovery from an interrupted build: resume the
same command first.

| Path | Purpose |
| --- | --- |
| `target/` | Cargo compilation and incremental artifacts. |
| `build/artifacts/`, `build/image/` | Built artifacts and staged boot volume. |
| `build/kvm/` | Private guest disk, serial/debug logs, and smoke evidence. |
| `build/formal/` | Generated proof evidence and rebuildable scratch. |
| `driver-domains/linux/out/` | Buildroot outputs, download and compiler caches. |

### Targeted cleanup

```bash
cargo xtask clean --dry-run --stale 7 --scratch
cargo xtask clean --stale 7
cargo xtask clean --scratch
```

`--stale <DAYS>` targets aged compilation residue; `--scratch` removes
regenerable formal scratch. Check the dry-run output before deleting. A bare
`cargo xtask clean` performs a much wider wipe. Never remove current proof
evidence or a DVM bundle needed for validation just to reclaim space.

### Building the workspace

Use `cargo xtask check` to validate workspace contracts and
`cargo xtask build` for the staged RustOS image. Plain
`cargo build --workspace` is not the OS build contract: mixing freestanding
`no_std` binaries with host/`std` members in one Cargo feature-unified
invocation can produce duplicate panic-handler/linking failures.
Targeted host Cargo checks remain useful, but do not replace `xtask`.

See [xtask API](../api/xtask.md) for commands and
[Paths Reference](../reference/paths.md) for generated locations. Historical
per-machine disk sizes, build timings, and discarded optimization experiments
belong in benchmark evidence or Git history, not evergreen build guidance.

<a id="korean"></a>

## 한국어

빌드 결과물, cache, formal scratch는 **소스 파일이 아닙니다**. 디스크 공간이
부족할 때만 범위를 좁힌 정리 명령을 사용하세요. 중단된 빌드를 복구하려고
`clean`/`distclean`부터 실행하지 말고, 먼저 같은 명령을 재실행하세요.

| 경로 | 용도 |
| --- | --- |
| `target/` | Cargo 컴파일 및 incremental 산출물 |
| `build/artifacts/`, `build/image/` | 빌드 산출물과 stage된 boot volume |
| `build/kvm/` | 게스트 전용 디스크, serial/debug log, smoke 증거 |
| `build/formal/` | 생성된 검증 증거 및 재생성 가능한 scratch |
| `driver-domains/linux/out/` | Buildroot 결과물, download/compiler cache |

### 범위를 좁힌 정리

```bash
cargo xtask clean --dry-run --stale 7 --scratch
cargo xtask clean --stale 7
cargo xtask clean --scratch
```

`--stale <DAYS>`는 오래된 컴파일 잔여물, `--scratch`는 재생성 가능한 formal
scratch를 정리합니다. 삭제 전에 `--dry-run` 결과를 확인하세요. 인자 없는
`cargo xtask clean`은 훨씬 넓은 범위를 삭제합니다. 현재 검증에 필요한 증거나
DVM bundle을 공간 확보 목적으로 지우지 마세요.

### Workspace 빌드

workspace 계약은 `cargo xtask check`, stage된 RustOS image는
`cargo xtask build`로 검증·생성하세요. `cargo build --workspace`는 공식 OS
빌드 경로가 아닙니다. 하나의 Cargo feature-unified 호출에 freestanding
`no_std` binary와 host/`std` member를 섞으면 panic handler 중복이나
link 오류가 생길 수 있습니다. 개별 host Cargo 검사는 유용하지만
`xtask` 검증을 대신하지 않습니다.

자세한 명령은 [xtask API](../api/xtask.md), 생성 경로는
[Paths Reference](../reference/paths.md)를 참고하세요. 특정 개발 PC의
과거 용량·빌드 시간과 폐기된 최적화 실험 기록은 Git history나 벤치마크
증거에 두고, 일반 빌드 가이드에는 유지하지 않습니다.
