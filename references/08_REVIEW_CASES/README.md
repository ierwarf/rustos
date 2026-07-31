# 공개 커밋·PR 수락/미병합 사례

- 총 PR 사례: 138개
- 구성: seL4, HelenOS, Qubes core-admin, Genode, MINIX
- 상태: 공개 GitHub 검색에서 `merged` 또는 `closed_unmerged`로 분류된 표본

## 가장 중요한 경고

`closed_unmerged`는 “거절”이라는 단일 의미가 아니다. 테스트용, draft, superseded, duplicate, dependency, RFC 필요, 방향 불일치, 장기 미응답, 저장소 이전 등이 모두 포함될 수 있다. CSV의 `reason_confirmed`는 기본적으로 `no`다. 실제 사유를 사용하려면 PR timeline과 maintainer comment를 직접 읽고 출처를 남겨라.

## 추천 학습법

1. 같은 주제의 merged와 closed_unmerged를 짝지어 비교한다.
2. patch size, scope, test evidence, dependency, ABI impact를 표로 만든다.
3. 결과를 보고 사후 합리화하지 말고, 첫 버전만 보고 예측한 뒤 실제 상태와 비교한다.
4. 제목이 `[do not merge]`, `testing only`, RFC라고 명시된 사례는 기술 품질 거절 데이터로 사용하지 않는다.
5. 오래된 PR은 당시 branch/API 상황을 확인한다.

## Linux

Linux는 GitHub PR보다 mailing list, subsystem tree, pull request, signed tag 흐름이 중심이다. `linux/accepted_commit_examples.csv`와 `linux/LINUX_PUSH_CASES_KO.md`는 실제 mainline 반영 사례를 담고, `tools/fetch_linux_patchwork_cases.py`는 Patchwork API에서 상태별 표본을 추가 수집한다.
