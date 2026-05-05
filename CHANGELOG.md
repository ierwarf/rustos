# Changelog

All notable project-level changes are tracked here. This file is intentionally
short; detailed design and API notes live in `docs/`.

## Unreleased

### Added

- mdBook documentation entrypoints with bilingual human docs.
- AI Agent Reference under `docs/ai/` for compact machine-oriented context.
- GitHub Actions checks for the OS build flow and host-side tests.
- Repository community files: license, contribution guide, security policy,
  code of conduct, issue templates, and pull request template.

### Changed

- README now acts as a product landing page and points to the full manual.
- CI uses the RustOS `cargo xtask check` path instead of a generic workspace
  build.

## 변경 기록

프로젝트 수준의 주요 변경 사항을 이 파일에 짧게 기록합니다. 자세한 설계와
API 설명은 `docs/`에 둡니다.

## 아직 릴리스되지 않음

### 추가

- bilingual human docs를 포함한 mdBook 문서 진입점
- 토큰 절약용 `docs/ai/` AI Agent Reference
- OS build flow와 host-side test를 검증하는 GitHub Actions
- license, contribution guide, security policy, code of conduct, issue
  template, pull request template

### 변경

- README를 전체 매뉴얼이 아닌 product landing page로 정리
- CI가 일반 workspace build 대신 RustOS 전용 `cargo xtask check` 경로 사용
