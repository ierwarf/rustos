# Changelog

All notable project-level changes are tracked here. This file is intentionally
short; detailed design and API notes live in `docs/`.

## Unreleased

### Added

- mdBook documentation entrypoints with bilingual human docs.
- AI Agent Reference under `docs/ai/` for compact machine-oriented context.
- GitHub Actions checks for the OS build flow and host-side tests.
- GRUB EFI boot manager generation with embedded public-key signature
  enforcement for `nucleus.elf`.
- Repository community files: license, contribution guide, security policy,
  code of conduct, issue templates, and pull request template.

### Changed

- README now acts as a product landing page and points to the full manual.
- CI uses the RustOS `cargo xtask check` path instead of a generic workspace
  build.
- GRUB Multiboot2 boot no longer requests a boot framebuffer, avoiding firmware
  video-mode failures before native display drivers are available.
- GRUB Multiboot2 boot now requests a 32-bit linear framebuffer for the KVM
  virtio display path.
- The Multiboot2 kernel link layout now includes ELF headers in the first
  loadable segment so kernel VM setup can derive executable protections.
- The kernel build script now tracks the Multiboot2 linker script for reliable
  relinks after boot-layout edits.
- Scheduler invalid-context panic paths now emit structured `sched` diagnostics
  before halting.
- Scheduler startup now enters the kernel continuation through a regular root
  kernel task owned by the scheduler.
- Kernel code that temporarily switches stacks can register alternate stack
  bounds with the scheduler for normal context validation.
- Logging docs now require using existing category/level controls first and
  turning new output into reusable structured diagnostics.
- The unfinished `virtio_net` bridge driver is no longer part of the default
  staged boot profile.

## 변경 기록

프로젝트 수준의 주요 변경 사항을 이 파일에 짧게 기록합니다. 자세한 설계와
API 설명은 `docs/`에 둡니다.

## 아직 릴리스되지 않음

### 추가

- bilingual human docs를 포함한 mdBook 문서 진입점
- 토큰 절약용 `docs/ai/` AI Agent Reference
- OS build flow와 host-side test를 검증하는 GitHub Actions
- embedded public key로 `nucleus.elf` signature를 강제 검증하는 GRUB EFI
  boot manager 생성
- license, contribution guide, security policy, code of conduct, issue
  template, pull request template

### 변경

- README를 전체 매뉴얼이 아닌 product landing page로 정리
- CI가 일반 workspace build 대신 RustOS 전용 `cargo xtask check` 경로 사용
- native display driver가 준비되기 전 firmware video mode 실패를 피하도록 GRUB
  Multiboot2 boot에서 boot framebuffer 요청을 제거
- GRUB Multiboot2 boot가 KVM virtio display path용 32-bit linear framebuffer를
  요청
- kernel VM setup이 executable protection을 계산할 수 있도록 Multiboot2
  kernel link layout의 첫 loadable segment에 ELF header 포함
- boot layout 수정 후 stale binary가 남지 않도록 kernel build script가
  Multiboot2 linker script를 추적
- scheduler invalid-context panic 경로가 중단 전에 structured `sched`
  diagnostics를 남김
- scheduler startup이 scheduler가 소유한 regular root kernel task로 kernel
  continuation에 진입
- 임시 stack으로 전환하는 kernel code가 scheduler에 alternate stack bounds를
  등록해 일반 context validation을 사용하도록 변경
- logging docs에 기존 category/level 조정을 먼저 사용하고 새 출력은 재사용
  가능한 structured diagnostics로 만들도록 명시
- 미완성 `virtio_net` bridge driver를 기본 staged boot profile에서 제외
