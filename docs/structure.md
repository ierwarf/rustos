# 구조 정리 기준

## 디렉터리 역할

- `assets/image/`: 부팅 이미지 overlay
- `build/artifacts/`: 컴파일 결과 보관
- `build/image/`: staged 부팅 볼륨 루트
- `build/logs/`: QEMU 및 디버그 로그
- `crates/`: 여러 바이너리/드라이버가 공유하는 순수 로직
- `drivers/`: first-party `.ko` 모듈 소스
- `vendor/`: 외부 바이너리, firmware, prebuilt `.ko`, OVMF

## 새 코드 배치 규칙

- bootloader/prekernel/kernel/driver 사이에서 재사용되는 순수 로직은 먼저 `crates/` 를 검토합니다.
- 하드웨어나 ABI entrypoint가 필요한 코드는 `kernel/` 또는 `drivers/` 아래에 둡니다.
- 단순 staged asset 은 `assets/image/` 로 갑니다.
- 외부에서 가져온 바이너리 파일은 `vendor/` 로 갑니다.

## vendor / assets / build 구분

- `vendor`: 저장소가 보관하는 외부 원본
- `assets/image`: stage 시 복사되는 정적 overlay
- `build`: 언제든 재생성 가능한 산출물

## 새 모듈 추가 절차

1. source 기반 first-party 모듈이면 `drivers/...` 에 crate를 추가합니다.
2. 공용 driver API bind/log helper가 필요하면 `crates/driver-module-runtime` 을 사용합니다.
3. `xtask` build/stage 경로에 artifact 또는 vendor module 복사 규칙을 추가합니다.
4. 필요하면 `BOOTFILES.TXT` 반영 여부를 `cargo xtask stage`로 확인합니다.
5. README나 이 문서에 경로 규칙이 바뀌면 같이 갱신합니다.
