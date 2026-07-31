# 병합·미병합 패턴 분석

## 병합 사례에서 자주 보이는 증거

- 재현 가능한 구체적 결함과 사용자 영향
- 작은 논리 범위
- 기존 commit·issue·spec과의 연결
- 테스트 환경과 결과
- architecture/configuration별 영향 설명
- error path·rollback 포함
- 리뷰 피드백에 따른 후속 버전
- dependency와 merge order 명시

## 미병합 사례에서 추가 확인할 신호

- PR 제목이 test/RFC/draft임
- 범위가 여러 subsystem에 걸침
- 기존 API·proof·ABI를 크게 바꿈
- “아직 테스트하지 않음”, “추후 지원”이 핵심 기능에 남음
- 비슷한 대체 PR이 있음
- 저장소가 archived이거나 development channel이 이동함
- maintainer가 다른 설계 방향을 요구함
- CI/format/license/provenance가 해결되지 않음

## 이 데이터로 해서는 안 되는 것

- 제목만으로 기술적 정확성을 판정
- closed_unmerged 비율을 프로젝트 품질 점수로 사용
- 오래된 프로젝트의 현재 정책을 추론
- 한 프로젝트의 리뷰 문화를 다른 프로젝트에 그대로 적용
- AI 학습에서 outcome을 유일한 label로 사용
