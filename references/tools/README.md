# Tools

- `fetch_full_reference_pack.sh [dest]`: 공식 저장소에서 core/extended 소스 수집. `PROFILE=extended`로 확장.
- `fetch_linux_patchwork_cases.py`: Patchwork API에서 Linux patch 상태별 사례 수집.
- `fetch_github_pr_threads.py`: 사례 CSV의 PR 본문·issue comments·reviews·inline comments 수집.
- `fetch_linux_commit_context.py`: mainline commit 메시지와 Fixes/Link/Reviewed-by 등 provenance 태그 수집.
- `build_rag_jsonl.py`: 로컬 문서를 RAG JSONL로 청킹.
- `update_manifest.py`: 파일 hash·행 수·크기 manifest 생성.
- `verify_pack.py`: manifest와 필수 파일 검증.
- `snapshot_commits.py`: 수집 저장소의 정확한 commit 기록.

모든 스크립트는 표준 shell/Python 3만 사용하도록 작성했다.
