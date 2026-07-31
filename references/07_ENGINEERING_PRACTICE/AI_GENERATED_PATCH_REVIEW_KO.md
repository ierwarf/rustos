# AI 생성 커널 패치 리뷰 지침

## provenance

- 사용 모델·도구·프롬프트 범위와 사람이 검증한 영역을 기록한다.
- 생성 코드가 참조한 라이선스·코드 provenance를 확인한다.
- “AI가 만들었다”는 이유로 더 느슨하거나 더 엄격하게 판단하지 말고 증거로 판단한다.

## 위험 패턴

- 존재하지 않는 helper/API를 자연스럽게 호출
- 현재 branch가 아닌 오래된 API를 사용
- lock을 추가하지만 반대 경로·IRQ context를 놓침
- happy path만 수정하고 unwind 누락
- refcount 증가·감소 위치가 비대칭
- memcpy 크기를 구조체 하나로 가정하고 flexible array 누락
- barrier를 습관적으로 추가하거나 필요 barrier 누락
- errno·signal·ABI 세부를 host language 관습으로 대체
- 테스트가 구현을 그대로 복제해 oracle이 없음
- 큰 refactor로 실제 bug fix를 숨김

## 승인 조건

- 변경 이유를 사람이 독립적으로 설명 가능
- invariants가 문서·assert·test 중 최소 두 곳에 나타남
- 생성자가 아닌 사람이 critical path를 읽음
- fault injection과 race test가 있음
- API/ABI diff가 자동 생성됨
- 컴파일·정적 분석·sanitizer·fuzzer 결과가 보존됨
- patch를 작은 logical commits로 재구성함
