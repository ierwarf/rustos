# RAG 검색 가이드

## Metadata

`rag_corpus.jsonl` 각 행:

- `id`: path와 chunk index의 stable hash
- `path`: 원문 상대 경로
- `topic`: linux, dual_abi, microkernel, qubes_xen, verification, specification, engineering, review_cases
- `chunk_index`: 원문 내 순서
- `text`: 최대 약 1400자, 180자 overlap

## 추천 retrieval

1. 질문에서 subsystem·context·ABI·failure mode를 추출한다.
2. `topic` filter로 1차 축소한다.
3. BM25와 embedding을 결합한다.
4. 같은 path의 앞뒤 chunk를 함께 가져온다.
5. 공식 원문 snapshot을 curated note보다 우선하되, 원문의 적용 범위를 확인한다.
6. review case는 outcome만으로 정답 label을 만들지 않는다.

## 예시 query expansion

- “SMP에서 UI 서비스만 멈춤” → memory barrier, wakeup, lock ordering, IPC queue, scheduler, RCU, per-CPU
- “Linux exe 호환” → ELF, auxv, signal frame, futex, procfs, ioctl, epoll, vDSO, time64
- “드라이버 호스트 재시작” → IOMMU revoke, IRQ mask, DMA quiesce, generation handle, shared ring reset
- “PR이 안 받아들여짐” → logical scope, test evidence, RFC, ABI impact, superseded, dependency
