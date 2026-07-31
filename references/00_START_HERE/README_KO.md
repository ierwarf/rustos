# OS 개발 AI 참고 자료 팩

- 생성일: 2026-07-31
- 대상: 약 15만 LOC 규모의 상용·준상용 OS, 특히 Rust/C/C++ 기반 하이브리드·마이크로커널형 시스템
- 중심 주제: Linux, Qubes OS/Xen, seL4와 다른 마이크로커널, 듀얼 ABI·Linux 호환 계층, 동시성·SMP, 명세·검증, 협업·패치 리뷰

이 팩은 거대한 저장소를 무작정 통째로 복제한 덤프가 아니다. LLM이 실제 설계·코드 리뷰에서 자주 놓치는 문서와 사례를 우선순위화한 **오프라인 코어**, 그리고 필요할 때 공식 저장소에서 확장 데이터를 가져오는 **재현 가능한 수집 스크립트**로 구성된다.

## 가장 먼저 읽을 순서

1. `LLM_SYSTEM_PROMPT_KO.md` — AI에 붙일 운영 규칙
2. `TOPIC_MAP.md` — 질문별로 어느 디렉터리를 볼지
3. `../07_ENGINEERING_PRACTICE/OS_COMMON_SENSE_CHECKLIST_KO.md` — 구현 전·후 체크리스트
4. `../02_DUAL_ABI/DUAL_ABI_DESIGN_GUIDE_KO.md` — Linux/자체 ABI 공존 설계
5. `../05_VERIFICATION/VERIFICATION_LADDER_KO.md` — 테스트부터 형식 검증까지
6. `../08_REVIEW_CASES/README.md` — 병합·미병합 사례를 해석하는 법
7. `../09_MANIFEST/rag_corpus.jsonl` — RAG용 사전 청킹 데이터

## 포함 방식

- **offline-snapshot**: 라이선스가 명확한 공식 문서 원문 일부를 그대로 포함한다.
- **curated-note**: 여러 공식 자료를 교차해 만든 요약·체크리스트다.
- **case-index**: 공개 PR/커밋의 제목, 상태, URL과 검토 관점을 담는다. 리뷰 댓글 전체를 복제하지 않는다.
- **fetch-only**: 크거나 자주 변하는 저장소는 `tools/fetch_full_reference_pack.sh`가 공식 원본에서 받는다.
- **link-only**: QNX, Intel/AMD/Arm 일부 매뉴얼, UEFI/ACPI 일부 배포물처럼 재배포 조건을 별도 확인해야 하는 자료는 URL과 사용 목적만 기록한다.

## 중요한 해석 규칙

- `closed_unmerged`는 곧 “기술적으로 틀림”이 아니다. 중복, 대체 패치, 장기 미응답, 범위 과다, CI 실패, API 정책, 릴리스 타이밍 등 다양한 이유가 있다.
- Linux의 내부 커널 API는 대체로 안정 ABI가 아니다. 반면 userspace ABI, syscall, ioctl, ELF, signal frame, proc/sysfs의 실사용 인터페이스는 호환성 비용이 매우 크다.
- seL4의 “검증됨”은 특정 구성·아키텍처·옵션과 증명 대상에 묶인다. 지원 표와 CAVEATS를 확인하지 않고 SMP/MCS/하이퍼바이저까지 한꺼번에 증명되었다고 가정하지 않는다.
- Qubes의 보안은 Xen 하나가 아니라 dom0 최소화, qrexec 정책, GUI·스토리지·네트워크 분리, 템플릿·DispVM 수명주기까지 합친 시스템 속성이다.
- 듀얼 ABI는 syscall 번호 변환만으로 끝나지 않는다. ELF 로더, signal/ptrace, futex, fd·epoll, proc/sysfs, ioctl, 네임스페이스, seccomp, 시간·타이머, vDSO, 장치 노출 정책이 함께 맞아야 한다.

## 라이선스

이 팩이 새로 작성한 색인·요약·스크립트는 `SPDX-License-Identifier: CC0-1.0`로 제공한다. 포함된 업스트림 파일은 각 파일과 원 저장소의 라이선스를 그대로 따른다. 자세한 내용은 `../NOTICE_AND_LICENSE_POLICY.md`를 보라.
