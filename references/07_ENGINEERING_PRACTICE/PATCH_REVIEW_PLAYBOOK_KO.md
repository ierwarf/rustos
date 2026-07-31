# 커널 패치 리뷰 플레이북

## 제출자가 써야 할 것

- Problem: 실제 결함·제약·사용자 영향
- Reproduction: 최소 재현, 로그, 환경
- Root cause: 어느 invariant가 깨졌는가
- Change: 무엇을 어떻게 바꾸는가
- Alternatives: 왜 다른 접근을 택하지 않았는가
- Compatibility: ABI, persistent format, driver, toolchain 영향
- Risk: lock, lifetime, security, performance, rollback
- Tests: 정상·오류·동시성·실기기·회귀
- Dependencies: 선행 패치, userspace, proof, firmware

## 리뷰어 순서

1. 제목과 첫 문단만 보고 문제가 독립적으로 설명되는가.
2. patch size보다 logical scope가 하나인가.
3. 기존 behavior와 desired behavior가 testable한가.
4. publish/free/error unwind를 먼저 본다.
5. lock acquisition order와 sleep context를 본다.
6. 사용자 입력의 크기·범위·권한을 본다.
7. ABI·persistent state·wire protocol 변화를 본다.
8. negative test와 race test를 본다.
9. 성능 수치가 workload를 대표하는지 본다.
10. commit마다 bisect 가능한지 본다.

## 병합 보류 신호

- “나중에 테스트하겠다”, “아마 안전하다”가 핵심 경로에 남음
- 한 PR이 refactor, API, feature, driver, tests를 한꺼번에 바꿈
- architecture-specific behavior를 다른 플랫폼에 일반화
- proof/configuration impact가 분석되지 않음
- 사용자 ABI를 구현 편의상 바꿈
- security fix인데 threat model과 affected versions가 없음
- test-only/draft/RFC인데 merge 대상으로 표시
- 대체 PR과 dependency가 정리되지 않음

## 미병합 상태 해석

닫힘·미병합은 자동 품질 라벨이 아니다. 다음을 별도 필드로 조사한다.

- superseded_by
- duplicate_of
- author_withdrew
- test_only
- needs_rfc
- design_rejected
- implementation_bug
- stale_no_response
- repository_moved_or_archived
- dependency_unresolved
- release_timing
- licensing/provenance

`08_REVIEW_CASES`의 CSV는 상태만 확정하고, 사유를 임의로 채우지 않는다.
