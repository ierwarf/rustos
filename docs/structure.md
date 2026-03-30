# 구조 정리 기준

## 디렉터리 역할

- `boot/`: bootloader, prekernel, boot protocol
- `core/`: kernel 및 핵심 런타임
- `system/`: native 서비스와 앱
- `compat/`: 호환 계층 소스
- `samples/`: demo, smoke, validation 프로그램
- `tests/`: 통합 테스트
- `tools/`: host-side build/run 도구
- `assets/image/`: 부팅 이미지 overlay
- `build/artifacts/`: 컴파일 결과 보관
- `build/image/`: staged 부팅 볼륨 루트
- `build/logs/`: QEMU 및 디버그 로그
- `drivers/`: first-party `.ko` 모듈과 driver ABI/runtime 공용 crate
- `vendor/`: 외부 바이너리, firmware, prebuilt `.ko`, OVMF

## 새 코드 배치 규칙

- 배포 단위 정책은 각 패키지 루트의 `RUSTOS.package.toml` 에 둡니다.
- boot/core/system/compat/samples/tests/tools 중 어떤 제품 계층이 소유하는 코드인지 먼저 결정합니다.
- boot chain 공용 로직은 `boot/` 아래 crate에 둡니다.
- driver ABI/runtime 또는 특정 버스/디바이스 공용 로직은 `drivers/` 아래 crate에 둡니다.
- 하드웨어나 ABI entrypoint가 필요한 코드는 `core/` 또는 `drivers/` 아래에 둡니다.
- 단순 staged asset 은 `assets/image/` 로 갑니다.
- 외부에서 가져온 바이너리 파일은 `vendor/` 로 갑니다.

## vendor / assets / build 구분

- `vendor`: 저장소가 보관하는 외부 원본
- `assets/image`: stage 시 복사되는 정적 overlay
- `build`: 언제든 재생성 가능한 산출물

## 새 모듈 추가 절차

1. source 기반 first-party 모듈이면 `drivers/...` 에 crate를 추가합니다.
2. 공용 driver API bind/log helper가 필요하면 `drivers/driver-module-runtime` 을 사용합니다.
3. deployable 단위라면 해당 패키지 루트에 `RUSTOS.package.toml` 을 추가합니다.
4. `cargo xtask build` 후 `build/image/system/registry/...` generated registry를 확인합니다.
5. README나 이 문서에 경로 규칙이 바뀌면 같이 갱신합니다.
