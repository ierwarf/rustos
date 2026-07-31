# 15만 LOC급 OS에서 이 팩을 쓰는 방법

## 항상 컨텍스트에 넣을 것

- `LLM_SYSTEM_PROMPT_KO.md`
- 현재 작업 subsystem의 설계·불변식 문서
- `OS_COMMON_SENSE_CHECKLIST_KO.md`에서 관련 section
- 실제 수정 파일과 호출자·피호출자
- 공개 ABI 또는 IPC schema

전체 팩을 한 번에 컨텍스트에 넣지 않는다. RAG로 8~20개 chunk를 가져오고, 공식 원문과 자체 설계 문서를 우선한다.

## 저장소에 추가할 최소 문서

각 subsystem 디렉터리에 다음 파일을 둔다.

- `ARCHITECTURE.md`: 책임, 경계, 데이터 흐름
- `INVARIANTS.md`: 반드시 유지할 조건
- `CONCURRENCY.md`: lock order, context, memory ordering
- `ABI.md`: 공개 구조체·syscall·IPC protocol
- `FAILURE_MODEL.md`: timeout, crash, restart, rollback
- `TEST_PLAN.md`: unit, stateful, race, fault injection, fuzz
- `OWNERS`: 리뷰 담당자와 security-critical quorum

## AI 작업 단위

좋은 작업 단위:
- 한 race의 재현 테스트와 fix
- 한 lifecycle transition의 명세·assert·test
- 한 syscall의 decoder/encoder와 differential tests
- 한 service protocol의 versioning

나쁜 작업 단위:
- “scheduler 전체 개선”
- “Linux 호환 완성”
- “모든 unsafe 제거”
- “driver framework 리팩터링과 새 GPU driver 동시 추가”

## RAG 운영

1. 질문을 subsystem, object, transition, context, ABI, hardware로 태깅한다.
2. 자체 저장소 문서를 가장 높은 가중치로 둔다.
3. 이 팩의 공식 원문을 두 번째로 둔다.
4. review cases는 코드 생성보다 리뷰 전략과 실패 패턴 검색에 사용한다.
5. 답변이 barrier, refcount, ABI, error code를 언급하면 대응 공식 문서를 반드시 재조회한다.

## 주간 품질 루프

- 새 bug를 invariant category로 분류
- 재현 테스트를 corpus에 추가
- 설계 문서와 runtime assert 갱신
- 비슷한 Linux/seL4/Qubes 사례 검색
- AI가 놓친 패턴을 system prompt 금지 규칙에 추가
- manifest와 RAG index 재생성

## 월간 릴리스 루프

- ABI diff 생성
- lock order와 unsafe inventory 비교
- fuzzer unique crash와 coverage 비교
- supported configuration matrix 고정
- third-party revision과 license/SBOM 고정
- update/rollback drill 수행
